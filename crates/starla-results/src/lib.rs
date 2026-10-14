//! Results management and upload for RIPE Atlas probe
//!
//! This crate handles:
//! - Queueing measurement results for upload (in-memory queue)
//! - Batching multiple results for efficient upload
//! - Retry logic with exponential backoff
//! - RIPE Atlas result format wrapping

pub mod format;
pub mod host_telemetry;
mod post_schedule;
mod queue;
mod system_status;
mod time_sync;
mod uploader;

pub use format::{AtlasResult, ResultBundle};
pub use host_telemetry::{HostTelemetry, HostTelemetryKind};
pub use post_schedule::PostSchedule;
pub use queue::{QueueStats, QueuedResult, ResultQueue};
pub use time_sync::TimeSyncTracker;
pub use uploader::{
    PostTarget, RateLimited, ResultUploader, UploadStream, UploadTransport, UploaderConfig,
};

use post_schedule::Slots;
use starla_common::MeasurementResult;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Configuration for the result handler
#[derive(Debug, Clone)]
pub struct ResultHandlerConfig {
    /// Starla's own upload interval, until the controller's httppost line
    /// arrives
    pub upload_interval: Duration,
    /// Maximum result age before dropping (seconds)
    pub max_result_age_secs: i64,
    /// Maximum retry attempts per result
    pub max_attempts: u32,
    /// Cleanup interval for expired/failed results
    pub cleanup_interval: Duration,
    /// Maximum number of results in queue
    pub max_queue_size: usize,
    /// Send interface counters (9002) with the 180 s status lines, as the
    /// reference probe does only with RXTXRPT=yes.
    pub rxtxrpt: bool,
    /// Where to keep the last clock sync so lts survives a restart
    /// (atlas_timesync.c). None keeps it in memory only, for tests.
    pub timesync_path: Option<std::path::PathBuf>,
}

impl Default for ResultHandlerConfig {
    fn default() -> Self {
        Self {
            upload_interval: Duration::from_secs(60),
            max_result_age_secs: 3600, // 1 hour
            max_attempts: 5,
            cleanup_interval: Duration::from_secs(300), // 5 minutes
            max_queue_size: 10000,
            rxtxrpt: false,
            timesync_path: None,
        }
    }
}

/// An httppost CRONLINE as the controller sent it, e.g. session 2 of
/// enodia's 2026-10-13 journal: `CRONLINE 60 12 2000000000 UNIFORM 20
/// httppost -A 9015 --set-time 2 --delete-file --maxpostsize 9000000
/// --post-header … --post-dir /home/atlas/data/out --post-footer … -O …
/// http://127.0.0.1:8080/?PROBE_ID=1018212&SESSION_ID=… >> …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttppostLine {
    /// The CRONLINE's timing fields
    pub schedule: PostSchedule,
    /// The URL httppost posts to
    pub url: String,
    /// `--post-dir`: the results go in the body. Without it, the header and
    /// footer only: the `&SRC=timesync` line.
    pub post_dir: bool,
    /// `--set-time N`
    pub set_time: Option<i64>,
    /// `--local-time`
    pub local_time: bool,
}

/// One httppost line as Starla runs it: where, and perd's slot state.
struct PostLine {
    target: PostTarget,
    slots: Slots,
}

/// The controller's httppost lines, one of each kind; the last one sent of
/// a kind wins. Empty until the controller's crontab arrives, and emptied
/// by a new session (its URLs carry the old SESSION_ID).
#[derive(Default)]
struct PostLines {
    /// The line with `--post-dir`
    main: Option<PostLine>,
    /// A line without (`&SRC=timesync`)
    header_only: Option<PostLine>,
}

impl PostLines {
    fn line(&mut self, post_dir: bool) -> &mut Option<PostLine> {
        if post_dir {
            &mut self.main
        } else {
            &mut self.header_only
        }
    }
}

/// Unix time in seconds. The system clock, except in tests, which drive it
/// from tokio's paused clock.
type WallClock = Arc<dyn Fn() -> f64 + Send + Sync>;

fn system_wall_clock() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// How many one-off measurements may be awaiting their result at once. A
/// one-off that fails produces nothing; its entry is dropped once this many
/// newer ones are waiting.
const MAX_PENDING_ONEOFFS: usize = 256;

/// Main result handler that coordinates queuing and uploading
pub struct ResultHandler {
    queue: Arc<Mutex<ResultQueue>>,
    /// Results of ONEOFF measurements, eooqd's spool: posted on their own
    /// with `&SRC=oneoff`, never in the main upload.
    oneoff_queue: Mutex<ResultQueue>,
    /// msm ids of ONEOFF measurements whose result has not come in yet
    oneoff_pending: Mutex<VecDeque<u64>>,
    /// A one-off result is queued
    oneoff_ready: Notify,
    uploader: Arc<Mutex<ResultUploader>>,
    config: ResultHandlerConfig,
    time_sync: Arc<TimeSyncTracker>,
    session_id: Arc<Mutex<Option<String>>>,
    host_telemetry: Arc<HostTelemetry>,
    lines: Mutex<PostLines>,
    /// An httppost line was set, or the session's lines were dropped
    lines_changed: Notify,
    wall_clock: WallClock,
    metrics: starla_metrics::MetricsRegistry,
}

impl ResultHandler {
    /// Create a new result handler
    pub fn new(
        transport: Box<dyn UploadTransport>,
        uploader_config: UploaderConfig,
        config: ResultHandlerConfig,
        metrics: starla_metrics::MetricsRegistry,
    ) -> Self {
        let queue = ResultQueue::new(config.max_queue_size);
        let host_telemetry = HostTelemetry::new();
        // Keep the last sync on disk so lts does not reset to -1 on restart,
        // as the reference reads its timesync file back (atlas_timesync.c).
        // Tests leave `timesync_path` unset and stay in memory.
        let time_sync = Arc::new(match &config.timesync_path {
            Some(path) => TimeSyncTracker::open(path.clone()),
            None => TimeSyncTracker::new(),
        });
        let uploader = ResultUploader::new(
            transport,
            uploader_config,
            metrics.clone(),
            Arc::clone(&host_telemetry),
            Arc::clone(&time_sync),
        );

        Self {
            queue: Arc::new(Mutex::new(queue)),
            oneoff_queue: Mutex::new(ResultQueue::new(config.max_queue_size)),
            oneoff_pending: Mutex::new(VecDeque::new()),
            oneoff_ready: Notify::new(),
            uploader: Arc::new(Mutex::new(uploader)),
            config,
            time_sync,
            session_id: Arc::new(Mutex::new(None)),
            host_telemetry,
            lines: Mutex::new(PostLines::default()),
            lines_changed: Notify::new(),
            wall_clock: Arc::new(system_wall_clock),
            metrics,
        }
    }

    /// Read unix time from `clock` instead of the system clock.
    #[cfg(test)]
    fn with_wall_clock(mut self, clock: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        self.wall_clock = Arc::new(clock);
        self
    }

    fn wall_now(&self) -> f64 {
        (self.wall_clock)()
    }

    /// Get a handle to the host-telemetry registry.
    /// Returned `Arc` is shared with the uploader; callers schedule reports
    /// via [`HostTelemetry::schedule_buddyinfo`] etc.
    pub fn host_telemetry(&self) -> Arc<HostTelemetry> {
        Arc::clone(&self.host_telemetry)
    }

    /// Set the upload endpoint path (call after controller connection). A
    /// new endpoint is a new session: the old session's httppost lines go
    /// (their URLs name its SESSION_ID), and Starla's own cadence holds until
    /// the new crontab brings new ones.
    pub async fn set_endpoint_path(&self, path: String) -> anyhow::Result<()> {
        let mut uploader = self.uploader.lock().await;
        let changed = uploader.default_target(None).path != path;
        uploader.set_endpoint_path(path)?;
        drop(uploader);
        if changed {
            *self.lines.lock().await = PostLines::default();
            self.lines_changed.notify_one();
        }
        debug!("Result upload endpoint path set");
        Ok(())
    }

    /// Run uploads on the controller's httppost CRONLINE: its URL, and
    /// perd's slots (`start_time + k * interval`, UNIFORM spread) in place
    /// of Starla's own minutely cadence. A line with `--post-dir` sets the
    /// main upload; one without sets the header-only (timesync) post.
    pub async fn set_httppost_line(&self, line: HttppostLine) -> anyhow::Result<()> {
        // perd refuses an interval outside 1..=2 years (perd.c:494-503), and
        // reads the start time unsigned.
        let s = &line.schedule;
        if !(1..=post_schedule::MAX_INTERVAL).contains(&s.interval) || s.start_time < 0 {
            anyhow::bail!(
                "httppost line with interval {} and start time {}",
                s.interval,
                s.start_time
            );
        }
        let target = PostTarget::from_url(&line.url, line.set_time, line.local_time)?;
        let now = self.wall_now() as i64;
        let mut lines = self.lines.lock().await;
        let slot = lines.line(line.post_dir);
        let slots = match slot {
            Some(old) if old.target == target && old.slots.same_cycles(&line.schedule) => {
                Slots::carried_over(&old.slots, line.schedule)
            }
            _ => Slots::new(line.schedule, now),
        };
        debug!(
            post_dir = line.post_dir,
            next = ?slots.next_run(now),
            "httppost line: {}",
            target.path
        );
        *slot = Some(PostLine { target, slots });
        drop(lines);
        self.lines_changed.notify_one();
        Ok(())
    }

    /// Set the session ID (for upload body footer)
    pub async fn set_session_id(&self, session_id: String) {
        let mut sid = self.session_id.lock().await;
        *sid = Some(session_id);
        debug!("Session ID set for upload footer");
    }

    /// Check if an endpoint is configured
    pub async fn has_endpoint(&self) -> bool {
        let uploader = self.uploader.lock().await;
        uploader.has_endpoint()
    }

    /// Mark time as synchronized
    pub fn mark_time_synced(&self) {
        self.time_sync.mark_synced();
    }

    /// Get the time sync tracker
    pub fn time_sync(&self) -> Arc<TimeSyncTracker> {
        Arc::clone(&self.time_sync)
    }

    /// Get current lts value
    pub fn current_lts(&self) -> i64 {
        self.time_sync.lts()
    }

    /// The next result of measurement `msm_id` is a ONEOFF's: it goes to
    /// eooqd's spool, posted at once with `&SRC=oneoff`, not into the next
    /// main upload. Call before the measurement is started.
    pub async fn expect_oneoff(&self, msm_id: u64) {
        let mut pending = self.oneoff_pending.lock().await;
        if pending.len() >= MAX_PENDING_ONEOFFS {
            pending.pop_front();
        }
        pending.push_back(msm_id);
    }

    /// Submit a result for upload
    pub async fn submit(&self, result: MeasurementResult) {
        let oneoff = {
            let mut pending = self.oneoff_pending.lock().await;
            let at = pending.iter().position(|&id| id == result.msm_id.0);
            at.and_then(|i| pending.remove(i)).is_some()
        };
        if oneoff {
            let mut queue = self.oneoff_queue.lock().await;
            queue.enqueue_measured(result, self.time_sync.lts());
            debug!("One-off result enqueued, queue: {}", queue.stats().count);
            drop(queue);
            self.oneoff_ready.notify_one();
            return;
        }

        let mut queue = self.queue.lock().await;
        let before = queue.stats().count;
        queue.enqueue_measured(result, self.time_sync.lts());
        let after = queue.stats().count;

        if after <= before && before > 0 {
            self.metrics.record_queue_drop();
        }

        self.metrics.update_queue_depth(after as i64);
        debug!("Result enqueued, queue: {}", after);
    }

    /// Get current queue statistics
    pub async fn queue_stats(&self) -> QueueStats {
        let queue = self.queue.lock().await;
        queue.stats()
    }

    /// The main upload's target: the controller's `--post-dir` line, or
    /// Starla's own until it arrives.
    async fn main_target(&self) -> PostTarget {
        if let Some(line) = &self.lines.lock().await.main {
            return line.target.clone();
        }
        self.uploader.lock().await.default_target(None)
    }

    /// Try to upload pending results
    pub async fn upload_pending(&self) -> anyhow::Result<usize> {
        let target = self.main_target().await;
        self.upload_pending_to(&target).await
    }

    async fn upload_pending_to(&self, target: &PostTarget) -> anyhow::Result<usize> {
        if !self.has_endpoint().await {
            debug!("No upload endpoint configured, skipping upload");
            return Ok(0);
        }

        // Drain all available results: upload everything each cycle,
        // matching the official probe's httppost behavior
        let results = {
            let mut queue = self.queue.lock().await;
            let res = queue.drain_all();
            self.metrics.update_queue_depth(0);
            res
        };

        let (uploadable, failed): (Vec<_>, Vec<_>) = results
            .into_iter()
            .partition(|r| r.attempts < self.config.max_attempts);

        if !failed.is_empty() {
            warn!(
                "Dropping {} results that exceeded max attempts",
                failed.len()
            );
            for _ in 0..failed.len() {
                self.metrics.record_queue_drop();
            }
        }

        // Nothing queued is still an upload: the reference probe posts every
        // minute whatever data/out holds, and each accepted post is a chance
        // to confirm the clock.
        let count = uploadable.len();
        if count > 0 {
            info!("Uploading {} results", count);
        } else {
            debug!("Uploading no results (status and clock check)");
        }

        let session_id = self.session_id.lock().await;
        let session_id_ref = session_id.as_deref();

        let uploader = self.uploader.lock().await;
        match uploader
            .upload_batch_to(target, &uploadable, session_id_ref)
            .await
        {
            Ok(()) => {
                info!("Successfully uploaded {} results", count);
                self.metrics.record_upload_success();
                Ok(count)
            }
            Err(e) => {
                warn!("Upload failed: {}", e);
                self.metrics.record_upload_failure();
                let mut queue = self.queue.lock().await;
                if is_rate_limited(&e) {
                    // 2026-10-12: five 429s in ~5 min dropped every result
                    // probe 1018212 measured while RIPE refused it.
                    queue.requeue_refused(uploadable);
                } else {
                    queue.requeue_failed(uploadable);
                }
                self.metrics.update_queue_depth(queue.stats().count as i64);
                Err(e)
            }
        }
    }

    /// Post the queued one-off results, as eooqd does once a one-off is
    /// done (cmddone → post_results, eooqd.c:604-645). Nothing queued,
    /// nothing posted (`need_post`, eooqd.c:733-783). A failed post leaves
    /// them queued for the next one, as httppost's `--delete-file` only
    /// deletes on an OK.
    pub async fn upload_oneoffs(&self) -> anyhow::Result<usize> {
        if !self.has_endpoint().await {
            return Ok(0);
        }
        let results = self.oneoff_queue.lock().await.drain_all();
        let (uploadable, failed): (Vec<_>, Vec<_>) = results
            .into_iter()
            .partition(|r| r.attempts < self.config.max_attempts);
        if !failed.is_empty() {
            warn!(
                "Dropping {} one-off results that exceeded max attempts",
                failed.len()
            );
        }
        if uploadable.is_empty() {
            return Ok(0);
        }
        let count = uploadable.len();
        let session_id = self.session_id.lock().await.clone();
        let sent = self
            .uploader
            .lock()
            .await
            .post_oneoff(&uploadable, session_id.as_deref())
            .await;
        match sent {
            Ok(()) => {
                info!("Posted {} one-off results", count);
                Ok(count)
            }
            Err(e) => {
                warn!("One-off post failed: {}", e);
                let mut queue = self.oneoff_queue.lock().await;
                if is_rate_limited(&e) {
                    queue.requeue_refused(uploadable);
                } else {
                    queue.requeue_failed(uploadable);
                }
                Err(e)
            }
        }
    }

    /// The 900 s clock-check post (`httppost … &SRC=timesync`): the
    /// controller's header-only line, or Starla's own until it arrives.
    pub async fn post_timesync(&self) -> anyhow::Result<()> {
        let line = self
            .lines
            .lock()
            .await
            .header_only
            .as_ref()
            .map(|l| l.target.clone());
        let session_id = self.session_id.lock().await.clone();
        let uploader = self.uploader.lock().await;
        let target = line.unwrap_or_else(|| uploader.default_target(Some("timesync")));
        uploader
            .post_header_only(&target, session_id.as_deref())
            .await
    }

    /// Run cleanup to remove expired and failed results
    pub async fn run_cleanup(&self) {
        let mut queue = self.queue.lock().await;
        let removed_expired = queue.cleanup_expired(self.config.max_result_age_secs);
        let removed_failed = queue.cleanup_failed(self.config.max_attempts);

        let total_removed = removed_expired + removed_failed;
        if total_removed > 0 {
            for _ in 0..total_removed {
                self.metrics.record_queue_drop();
            }
            self.metrics.update_queue_depth(queue.stats().count as i64);
        }
        drop(queue);

        let mut oneoffs = self.oneoff_queue.lock().await;
        oneoffs.cleanup_expired(self.config.max_result_age_secs);
        oneoffs.cleanup_failed(self.config.max_attempts);
    }

    /// When the controller's line of this kind is next due, as a tokio
    /// deadline: None without a line (Starla's own cadence runs),
    /// `Some(None)` for a line that will not run again (past its end time).
    async fn line_deadline(&self, post_dir: bool, now: Instant) -> Option<Option<Instant>> {
        let wall = self.wall_now();
        let mut lines = self.lines.lock().await;
        let line = lines.line(post_dir).as_mut()?;
        line.slots.check_clock(wall as i64);
        Some(
            line.slots
                .next_run(wall as i64)
                .map(|at| now + Duration::from_secs_f64((at as f64 - wall).max(0.0))),
        )
    }

    /// The controller's line of this kind, if it is due now: its target, the
    /// line moved on to its next cycle. None if there is no line or it is
    /// not due yet (the clock moved since the deadline was set).
    async fn take_due_line(&self, post_dir: bool) -> Option<PostTarget> {
        let now = self.wall_now() as i64;
        let mut lines = self.lines.lock().await;
        let line = lines.line(post_dir).as_mut()?;
        if !line.slots.is_due(now) {
            return None;
        }
        line.slots.advance(now);
        Some(line.target.clone())
    }

    /// Run the upload loop
    pub async fn run(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        info!("Result handler starting");

        let started = Instant::now();
        let mut cleanup_interval = tokio::time::interval(self.config.cleanup_interval);
        // The reference main loop's status lines: first pass at start.
        let mut status_interval =
            tokio::time::interval(Duration::from_secs(system_status::STATUS_INTERVAL_SECS));
        let data_dir = starla_common::state_dir();
        // eooqd's re_post: every 60 s, post one-offs a failed post left
        // behind (eooqd.c:265-272, 712-719).
        let mut oneoff_repost = tokio::time::interval_at(started + ONEOFF_REPOST, ONEOFF_REPOST);

        // Starla's own cadence, until the controller's httppost lines arrive:
        // an upload a minute and a timesync post every 900 s. The first
        // upload waits a minute, not KEEP: a RIPE controller sends its crontab
        // within a second of KEEP (0.7 s in the twin), so in practice its
        // slots are what run; this keeps a probe whose controller sends no
        // httppost line reporting.
        let mut own_main_at = started + self.config.upload_interval;
        let mut own_timesync_at = started + TIMESYNC_INTERVAL;
        // Whether each kind ran on a controller line on the last pass: a
        // line that goes (a new session) hands back to Starla's own cadence,
        // counted afresh.
        let mut had_lines = (false, false);

        let mut gate = UploadGate::default();

        loop {
            let now = Instant::now();
            let main_line = self.line_deadline(true, now).await;
            let timesync_line = self.line_deadline(false, now).await;
            if had_lines.0 && main_line.is_none() {
                own_main_at = now + self.config.upload_interval;
            }
            if had_lines.1 && timesync_line.is_none() {
                own_timesync_at = now + TIMESYNC_INTERVAL;
            }
            had_lines = (main_line.is_some(), timesync_line.is_some());
            let main_at = main_line.unwrap_or(Some(own_main_at));
            let timesync_at = timesync_line.unwrap_or(Some(own_timesync_at));

            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!("Result handler shutting down");
                    break;
                }

                _ = self.lines_changed.notified() => {}

                _ = sleep_until(main_at) => {
                    let Some(tick) = main_at else { continue };
                    let target = if had_lines.0 {
                        match self.take_due_line(true).await {
                            Some(target) => target,
                            None => continue,
                        }
                    } else {
                        own_main_at = tick + self.config.upload_interval;
                        self.uploader.lock().await.default_target(None)
                    };
                    // A hold skips slots rather than sleeping inside the
                    // select, so cleanup and cancellation still run during a
                    // half-hour hold.
                    if gate.holds(tick) {
                        continue;
                    }
                    let sent = self.upload_pending_to(&target).await;
                    gate.after_post(tick, sent.map(|_| ()));
                }

                _ = sleep_until(timesync_at) => {
                    let Some(tick) = timesync_at else { continue };
                    let line = if had_lines.1 {
                        match self.take_due_line(false).await {
                            Some(target) => Some(target),
                            None => continue,
                        }
                    } else {
                        own_timesync_at = tick + TIMESYNC_INTERVAL;
                        None
                    };
                    // Not while a 429 hold is on: it would be one more refused
                    // request.
                    if gate.holds(tick) {
                        continue;
                    }
                    let session_id = self.session_id.lock().await.clone();
                    let uploader = self.uploader.lock().await;
                    let target =
                        line.unwrap_or_else(|| uploader.default_target(Some("timesync")));
                    let sent = uploader.post_header_only(&target, session_id.as_deref()).await;
                    if let Err(e) = sent {
                        debug!("Timesync post failed: {}", e);
                    }
                }

                _ = self.oneoff_ready.notified() => {
                    // Held one-offs wait for the re-post after the hold.
                    let tick = Instant::now();
                    if !gate.holds(tick) {
                        gate.after_oneoffs(tick, self.upload_oneoffs().await);
                    }
                }

                tick = oneoff_repost.tick() => {
                    if !gate.holds(tick) {
                        gate.after_oneoffs(tick, self.upload_oneoffs().await);
                    }
                }

                _ = cleanup_interval.tick() => {
                    self.run_cleanup().await;
                }

                _ = status_interval.tick() => {
                    let lts = self.time_sync.lts();
                    for line in system_status::status_lines(lts, &data_dir, self.config.rxtxrpt) {
                        self.host_telemetry.push(line).await;
                    }
                }
            }
        }

        Ok(())
    }
}

/// Sleep until `at`, or forever without one.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// What failed posts hold back: after a 429 no post starts before the hold
/// ends (0007, 0009), and after more than three failures in a row of any
/// other kind, an exponential backoff.
struct UploadGate {
    consecutive_failures: u32,
    rate_limited: u32,
    backoff: Duration,
    hold_until: Option<Instant>,
}

impl Default for UploadGate {
    fn default() -> Self {
        Self {
            consecutive_failures: 0,
            rate_limited: 0,
            backoff: Duration::from_secs(1),
            hold_until: None,
        }
    }
}

impl UploadGate {
    const MAX_BACKOFF: Duration = Duration::from_secs(300);

    /// Whether a post due at `tick` is held back.
    fn holds(&self, tick: Instant) -> bool {
        self.hold_until.is_some_and(|t| tick < t)
    }

    /// Account for a one-off post started at `tick`; with nothing queued
    /// nothing was posted, and nothing changes. The controller's answer to a
    /// one-off post counts as its answer to any other.
    fn after_oneoffs(&mut self, tick: Instant, sent: anyhow::Result<usize>) {
        if !matches!(sent, Ok(0)) {
            self.after_post(tick, sent.map(|_| ()));
        }
    }

    /// Account for a post started at `tick`.
    fn after_post(&mut self, tick: Instant, sent: anyhow::Result<()>) {
        let Err(e) = sent else {
            self.consecutive_failures = 0;
            self.rate_limited = 0;
            self.backoff = Duration::from_secs(1);
            return;
        };
        self.consecutive_failures += 1;
        if is_rate_limited(&e) {
            // A 429 is a refusal, not a plain failure: it steps its own
            // backoff and does not inflate `consecutive_failures`, which
            // would otherwise trip the generic `> 3` backoff early.
            self.rate_limited += 1;
            let retry_after = e.downcast_ref::<RateLimited>().and_then(|r| r.retry_after);
            let hold = rate_limit_hold(self.rate_limited, retry_after);
            warn!(
                "Rate limited ({} in a row), next upload in {}s",
                self.rate_limited,
                hold.as_secs()
            );
            self.hold_until = Some(tick + hold);
        } else {
            warn!(
                "Upload failed (attempt {}): {}",
                self.consecutive_failures, e
            );
            if self.consecutive_failures > 3 {
                debug!("Applying backoff: {:?}", self.backoff);
                self.hold_until = Some(tick + self.backoff);
                self.backoff = std::cmp::min(self.backoff * 2, Self::MAX_BACKOFF);
            }
        }
    }
}

/// The uploader's 429, told apart from any other failure by type.
fn is_rate_limited(e: &anyhow::Error) -> bool {
    e.downcast_ref::<RateLimited>().is_some()
}

/// The controller's crontab posts `&SRC=timesync` every 900 s.
const TIMESYNC_INTERVAL: Duration = Duration::from_secs(900);

/// eooqd's re-post timer (eooqd.c:271).
const ONEOFF_REPOST: Duration = Duration::from_secs(60);

const RATE_LIMIT_FIRST_HOLD: Duration = Duration::from_secs(60);
const RATE_LIMIT_MAX_HOLD: Duration = Duration::from_secs(30 * 60);
const RETRY_AFTER_MAX_HOLD: Duration = Duration::from_secs(60 * 60);

/// How long to hold uploads after the `in_a_row`th consecutive 429: the first
/// retry waits a minute, each further 429 doubles the wait, up to half an
/// hour. A longer Retry-After wins, up to an hour.
///
/// 2026-10-12: RIPE answered every upload from probe 1018212 with 429 (no
/// Retry-After) for hours, and Starla asked again every 60 s, re-sending the
/// whole batch each time.
fn rate_limit_hold(in_a_row: u32, retry_after: Option<u64>) -> Duration {
    let doublings = in_a_row.saturating_sub(1).min(16);
    let backoff = RATE_LIMIT_FIRST_HOLD
        .saturating_mul(1 << doublings)
        .min(RATE_LIMIT_MAX_HOLD);
    let asked = Duration::from_secs(retry_after.unwrap_or(0)).min(RETRY_AFTER_MAX_HOLD);
    backoff.max(asked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use starla_common::{MeasurementData, MeasurementId, MeasurementType, ProbeId, Timestamp};
    use std::net::IpAddr;

    fn make_result(msm_id: u64) -> MeasurementResult {
        MeasurementResult {
            fw: 5120,
            measurement_type: MeasurementType::Ping,
            prb_id: ProbeId(12345),
            msm_id: MeasurementId(msm_id),
            timestamp: Timestamp::now(),
            af: 4,
            dst_addr: "8.8.8.8".parse::<IpAddr>().unwrap(),
            dst_name: None,
            src_addr: None,
            proto: Some("ICMP".to_string()),
            ttl: Some(64),
            size: Some(32),
            data: MeasurementData::Generic(serde_json::json!([{"rtt": 12.5}])),
        }
    }

    struct NoopTransport;
    impl UploadTransport for NoopTransport {
        fn open(
            &self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = anyhow::Result<Box<dyn UploadStream>>> + Send + '_,
            >,
        > {
            Box::pin(async { anyhow::bail!("no transport") })
        }
    }

    #[tokio::test]
    async fn test_result_handler_creation() {
        let handler = ResultHandler::new(
            Box::new(NoopTransport),
            UploaderConfig::default(),
            ResultHandlerConfig::default(),
            starla_metrics::MetricsRegistry::new().unwrap(),
        );

        let stats = handler.queue_stats().await;
        assert_eq!(stats.count, 0);
    }

    #[tokio::test]
    async fn test_submit_and_stats() {
        let handler = ResultHandler::new(
            Box::new(NoopTransport),
            UploaderConfig::default(),
            ResultHandlerConfig::default(),
            starla_metrics::MetricsRegistry::new().unwrap(),
        );

        handler.submit(make_result(1001)).await;

        let stats = handler.queue_stats().await;
        assert_eq!(stats.count, 1);
    }

    #[test]
    fn rate_limit_hold_doubles_to_half_an_hour() {
        let secs = |n, ra| rate_limit_hold(n, ra).as_secs();
        assert_eq!(
            (1..=7).map(|n| secs(n, None)).collect::<Vec<_>>(),
            [60, 120, 240, 480, 960, 1800, 1800]
        );
        assert_eq!(secs(u32::MAX, None), 1800);
        // A longer Retry-After wins, a shorter one does not, and an absurd
        // one waits an hour.
        assert_eq!(secs(1, Some(300)), 300);
        assert_eq!(secs(4, Some(30)), 480);
        assert_eq!(secs(1, Some(86_400)), 3600);
    }

    /// A controller that answers every POST with 429, no Retry-After, as
    /// RIPE's did on 2026-10-12, and notes when each request arrived and
    /// what it said.
    struct RateLimitingController(
        tokio::sync::mpsc::UnboundedSender<(tokio::time::Instant, String)>,
    );

    impl UploadTransport for RateLimitingController {
        fn open(
            &self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = anyhow::Result<Box<dyn UploadStream>>> + Send + '_,
            >,
        > {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (probe, mut ctrl) = tokio::io::duplex(1 << 20);
            let seen = self.0.clone();
            tokio::spawn(async move {
                let mut req = Vec::new();
                let mut buf = [0u8; 8192];
                // Read until the request's Content-Length is satisfied.
                loop {
                    let n = ctrl.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&req);
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len: usize = text
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        if req.len() >= h + 4 + len {
                            break;
                        }
                    }
                }
                let _ = seen.send((
                    tokio::time::Instant::now(),
                    String::from_utf8_lossy(&req).into_owned(),
                ));
                let _ = ctrl
                    .write_all(
                        b"HTTP/1.1 429 \r\nContent-Length: 3\r\nConnection: close\r\n\r\nOK\n",
                    )
                    .await;
                let _ = ctrl.shutdown().await;
            });
            Box::pin(async move { Ok(Box::new(probe) as Box<dyn UploadStream>) })
        }
    }

    /// 2026-10-12: five 429s dropped every result probe 1018212 measured while
    /// RIPE refused it. A refusal is not a failed attempt.
    #[tokio::test]
    async fn a_refused_upload_drops_nothing() {
        let (tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let handler = ResultHandler::new(
            Box::new(RateLimitingController(tx)),
            UploaderConfig::default(),
            ResultHandlerConfig::default(),
            starla_metrics::MetricsRegistry::new().unwrap(),
        );
        handler
            .set_endpoint_path("/?PROBE_ID=1".into())
            .await
            .unwrap();
        handler.submit(make_result(1001)).await;

        let tries = ResultHandlerConfig::default().max_attempts + 3;
        for _ in 0..tries {
            let err = handler.upload_pending().await.unwrap_err();
            assert!(is_rate_limited(&err), "{err}");
        }
        assert_eq!(handler.queue_stats().await.count, 1);
        let mut posts = 0;
        while seen.try_recv().is_ok() {
            posts += 1;
        }
        assert_eq!(posts, tries, "every try still posted the result");
    }

    /// 2026-10-12: RIPE refused every upload from probe 1018212 for hours and
    /// Starla asked again every 60 s. Back off instead, and stay stoppable.
    #[tokio::test(start_paused = true)]
    async fn rate_limited_uploads_back_off_and_stay_stoppable() {
        let (tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let handler = Arc::new(ResultHandler::new(
            Box::new(RateLimitingController(tx)),
            UploaderConfig::default(),
            ResultHandlerConfig {
                max_attempts: u32::MAX,
                ..Default::default()
            },
            starla_metrics::MetricsRegistry::new().unwrap(),
        ));
        handler
            .set_endpoint_path("/?PROBE_ID=1".into())
            .await
            .unwrap();
        handler.submit(make_result(1001)).await;

        let cancel = CancellationToken::new();
        let run = tokio::spawn({
            let handler = Arc::clone(&handler);
            let cancel = cancel.clone();
            async move { handler.run(cancel).await }
        });
        tokio::time::sleep(Duration::from_secs(3 * 3600)).await;

        let mut at = Vec::new();
        while let Ok((t, req)) = seen.try_recv() {
            if !req.contains("SRC=timesync") {
                at.push(t);
            }
        }
        let gaps: Vec<u64> = at.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
        assert_eq!(gaps, [60, 120, 240, 480, 960, 1800, 1800, 1800, 1800]);

        // Mid-hold (the last 429 came at 9,120 s, the next upload is due at
        // 10,920 s), a stop returns at once: no virtual time passes.
        let stopped_at = tokio::time::Instant::now();
        cancel.cancel();
        run.await.unwrap().unwrap();
        assert_eq!(tokio::time::Instant::now(), stopped_at);
    }

    fn handler_against_429() -> (
        ResultHandler,
        tokio::sync::mpsc::UnboundedReceiver<(tokio::time::Instant, String)>,
    ) {
        let (tx, seen) = tokio::sync::mpsc::unbounded_channel();
        let handler = ResultHandler::new(
            Box::new(RateLimitingController(tx)),
            UploaderConfig::default(),
            ResultHandlerConfig::default(),
            starla_metrics::MetricsRegistry::new().unwrap(),
        );
        (handler, seen)
    }

    /// The reference probe posts every minute whatever data/out holds.
    #[tokio::test]
    async fn an_empty_queue_still_posts() {
        let (handler, mut seen) = handler_against_429();
        handler
            .set_endpoint_path("/?PROBE_ID=1&SESSION_ID=s".into())
            .await
            .unwrap();
        handler.set_session_id("s".into()).await;
        assert!(handler.upload_pending().await.is_err(), "posted, got 429");
        let (_, req) = seen.try_recv().expect("a post went out");
        let body = &req[req.find("\r\n\r\n").unwrap() + 4..];
        assert!(body.starts_with("P_TO_C_REPORT\n"), "{body}");
        assert!(body.ends_with("\nSESSION_ID s\n"), "{body}");
        assert!(!body.contains("\"af\""), "no measurement results: {body}");
    }

    /// The reference writes get_timesync() into a result when it is
    /// produced; a later sync must not rewrite it.
    #[tokio::test]
    async fn a_result_keeps_the_lts_it_was_measured_at() {
        let (handler, mut seen) = handler_against_429();
        handler
            .set_endpoint_path("/?PROBE_ID=1".into())
            .await
            .unwrap();
        handler.submit(make_result(1001)).await;
        handler.mark_time_synced();
        let _ = handler.upload_pending().await;
        let (_, req) = seen.try_recv().unwrap();
        let line = req
            .lines()
            .find(|l| l.contains("\"1001\""))
            .expect("the result line");
        assert!(line.contains("\"lts\":-1"), "{line}");
    }

    #[tokio::test]
    async fn the_timesync_post_carries_only_header_and_footer() {
        let (handler, mut seen) = handler_against_429();
        handler
            .set_endpoint_path("/?PROBE_ID=1&SESSION_ID=s".into())
            .await
            .unwrap();
        handler.set_session_id("s".into()).await;
        handler.submit(make_result(1001)).await;
        let _ = handler.post_timesync().await;
        let (_, req) = seen.try_recv().unwrap();
        assert!(
            req.starts_with("POST /?PROBE_ID=1&SESSION_ID=s&SRC=timesync HTTP/1.1\r\n"),
            "{req}"
        );
        let body = &req[req.find("\r\n\r\n").unwrap() + 4..];
        assert_eq!(body, "P_TO_C_REPORT\n\nSESSION_ID s\n");
        assert_eq!(handler.queue_stats().await.count, 1, "results stay queued");
    }

    /// The twin's KEEP, 2026-10-13 (wire-ctrl.jsonl: the reference probe's
    /// KEEP exec at 1791871112.115), and its session.
    const KEEP: f64 = 1_791_871_112.115;
    const SID: &str = "b6c053a85ee60b777fa64503f459764141780eea4c80d096c75cb2300531a15d";

    /// A controller that answers each POST with the next of `answers` (the
    /// last one for good), and notes when each request arrived and what it
    /// said.
    struct AnsweringController {
        answers: std::sync::Mutex<Vec<&'static str>>,
        seen: tokio::sync::mpsc::UnboundedSender<(tokio::time::Instant, String)>,
    }

    impl UploadTransport for AnsweringController {
        fn open(
            &self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = anyhow::Result<Box<dyn UploadStream>>> + Send + '_,
            >,
        > {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (probe, mut ctrl) = tokio::io::duplex(1 << 20);
            let reply = {
                let mut answers = self.answers.lock().unwrap();
                if answers.len() > 1 {
                    answers.remove(0)
                } else {
                    answers[0]
                }
            };
            let seen = self.seen.clone();
            tokio::spawn(async move {
                let mut req = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    let n = ctrl.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&req);
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len: usize = text
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        if req.len() >= h + 4 + len {
                            break;
                        }
                    }
                }
                let _ = seen.send((
                    tokio::time::Instant::now(),
                    String::from_utf8_lossy(&req).into_owned(),
                ));
                let _ = ctrl.write_all(reply.as_bytes()).await;
                let _ = ctrl.shutdown().await;
            });
            Box::pin(async move { Ok(Box::new(probe) as Box<dyn UploadStream>) })
        }
    }

    const OK: &str = "HTTP/1.1 200 \r\nContent-Length: 3\r\nConnection: close\r\n\r\nOK\n";

    /// A handler whose clock reads the twin's KEEP at the start of the test
    /// and then runs with tokio's (paused) clock, on the twin's session.
    async fn twin_handler(
        transport: Box<dyn UploadTransport>,
    ) -> (Arc<ResultHandler>, tokio::time::Instant) {
        let t0 = tokio::time::Instant::now();
        let handler = ResultHandler::new(
            transport,
            UploaderConfig::default(),
            ResultHandlerConfig::default(),
            starla_metrics::MetricsRegistry::new().unwrap(),
        )
        .with_wall_clock(move || KEEP + t0.elapsed().as_secs_f64());
        handler
            .set_endpoint_path(format!("/?PROBE_ID=1018212&SESSION_ID={SID}"))
            .await
            .unwrap();
        handler.set_session_id(SID.into()).await;
        (Arc::new(handler), t0)
    }

    fn answering(
        answers: &[&'static str],
    ) -> (
        Box<dyn UploadTransport>,
        tokio::sync::mpsc::UnboundedReceiver<(tokio::time::Instant, String)>,
    ) {
        let (tx, seen) = tokio::sync::mpsc::unbounded_channel();
        let ctrl = AnsweringController {
            answers: std::sync::Mutex::new(answers.to_vec()),
            seen: tx,
        };
        (Box::new(ctrl), seen)
    }

    fn start(handler: &Arc<ResultHandler>) -> CancellationToken {
        let cancel = CancellationToken::new();
        tokio::spawn({
            let handler = Arc::clone(handler);
            let cancel = cancel.clone();
            async move { handler.run(cancel).await }
        });
        cancel
    }

    /// The session's httppost CRONLINE, as enodia's controller sent it on
    /// 2026-10-13, with the twin's session: the main line (`src` None, with
    /// --post-dir) or the header-only `&SRC=timesync` one.
    fn twin_line(interval: u64, start_time: i64, spread: u64, src: Option<&str>) -> HttppostLine {
        let mut url = format!("http://127.0.0.1:8080/?PROBE_ID=1018212&SESSION_ID={SID}");
        if let Some(src) = src {
            url.push_str(&format!("&SRC={src}"));
        }
        HttppostLine {
            schedule: PostSchedule {
                interval,
                start_time,
                end_time: 2_000_000_000,
                spread,
            },
            url,
            post_dir: src.is_none(),
            set_time: Some(2),
            local_time: false,
        }
    }

    /// Each post so far: unix time on the test clock, request target, body.
    fn posts(
        seen: &mut tokio::sync::mpsc::UnboundedReceiver<(tokio::time::Instant, String)>,
        t0: tokio::time::Instant,
    ) -> Vec<(f64, String, String)> {
        let mut out = Vec::new();
        while let Ok((t, req)) = seen.try_recv() {
            let (head, body) = req.split_once("\r\n\r\n").unwrap();
            let target = head.split(' ').nth(1).unwrap().to_string();
            out.push((KEEP + (t - t0).as_secs_f64(), target, body.to_string()));
        }
        out
    }

    fn main_path() -> String {
        format!("/?PROBE_ID=1018212&SESSION_ID={SID}")
    }

    /// The twin's CRONLINEs (`60 12 … httppost … --post-dir`, `900 312 …
    /// &SRC=timesync`), spread 0 to pin the slots: the main upload at :12
    /// of every minute from the first one after KEEP (40 s after it, not
    /// at KEEP as Starla did), the timesync post at 312 + k·900 (400 s
    /// after KEEP, not 900 s after start).
    #[tokio::test(start_paused = true)]
    async fn uploads_run_on_the_controllers_slots() {
        let (transport, mut seen) = answering(&[OK]);
        let (handler, t0) = twin_handler(transport).await;
        handler
            .set_httppost_line(twin_line(60, 12, 0, None))
            .await
            .unwrap();
        handler
            .set_httppost_line(twin_line(900, 312, 0, Some("timesync")))
            .await
            .unwrap();
        let cancel = start(&handler);
        tokio::time::sleep(Duration::from_secs(600)).await;
        cancel.cancel();

        let posts = posts(&mut seen, t0);
        // The posts to `main_path() + src`.
        let at = |src: &str| -> Vec<i64> {
            posts
                .iter()
                .filter(|(_, target, _)| *target == format!("{}{src}", main_path()))
                .map(|(t, _, _)| {
                    assert!((t - t.round()).abs() < 0.01, "on a whole second: {t}");
                    t.round() as i64
                })
                .collect()
        };
        assert_eq!(
            at(""),
            (0..10).map(|k| 1_791_871_152 + 60 * k).collect::<Vec<_>>()
        );
        assert_eq!(at("&SRC=timesync"), [1_791_871_512]);
        assert_eq!(posts.len(), 11, "and nothing else: {posts:?}");
    }

    /// `UNIFORM 20`: each cycle's post lands within ±10 s of its slot, one
    /// per slot, the offset drawn afresh each time.
    #[tokio::test(start_paused = true)]
    async fn the_uniform_spread_moves_each_post_around_its_slot() {
        let (transport, mut seen) = answering(&[OK]);
        let (handler, t0) = twin_handler(transport).await;
        handler
            .set_httppost_line(twin_line(60, 12, 20, None))
            .await
            .unwrap();
        handler
            .set_httppost_line(twin_line(900, 312, 20, Some("timesync")))
            .await
            .unwrap();
        let cancel = start(&handler);
        tokio::time::sleep(Duration::from_secs(3600)).await;
        cancel.cancel();

        let posts = posts(&mut seen, t0);
        let offsets = |src: &str, interval: i64, phase: i64, first: i64| {
            let times: Vec<i64> = posts
                .iter()
                .filter(|(_, target, _)| target.ends_with(src))
                .map(|(t, _, _)| t.round() as i64)
                .collect();
            times
                .iter()
                .enumerate()
                .map(|(k, &t)| {
                    let slot = first + interval * k as i64;
                    assert_eq!((slot - phase) % interval, 0);
                    t - slot
                })
                .collect::<Vec<i64>>()
        };
        let main = offsets(&main_path(), 60, 12, 1_791_871_152);
        assert_eq!(main.len(), 60, "one post per minute's slot: {main:?}");
        assert!(main.iter().all(|o| (-10..=10).contains(o)), "{main:?}");
        let distinct: std::collections::BTreeSet<_> = main.iter().collect();
        assert!(distinct.len() > 5, "a fresh offset each cycle: {main:?}");
        let ts = offsets("&SRC=timesync", 900, 312, 1_791_871_512);
        assert_eq!(ts.len(), 4, "{ts:?}");
        assert!(ts.iter().all(|o| (-10..=10).contains(o)), "{ts:?}");
    }

    /// Before the controller's httppost line, Starla's own minute (the
    /// first a minute in, not at KEEP); the line takes over when it comes,
    /// and a new session drops it until its own crontab arrives.
    #[tokio::test(start_paused = true)]
    async fn starlas_own_cadence_until_the_line_arrives() {
        let (transport, mut seen) = answering(&[OK]);
        let (handler, t0) = twin_handler(transport).await;
        let cancel = start(&handler);
        tokio::time::sleep(Duration::from_secs(150)).await;
        handler
            .set_httppost_line(twin_line(60, 12, 0, None))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(150)).await;
        // A new session (a new INIT): its endpoint, Starla's minute again.
        handler
            .set_endpoint_path("/?PROBE_ID=1018212&SESSION_ID=next".into())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(70)).await;
        cancel.cancel();

        let posts: Vec<(i64, String)> = posts(&mut seen, t0)
            .into_iter()
            .map(|(t, target, _)| ((t - KEEP).round() as i64, target))
            .collect();
        let main = main_path();
        assert_eq!(
            posts,
            [
                (60, main.clone()),
                (120, main.clone()),
                // :12 slots: 1791871272 is KEEP + 159.9
                (160, main.clone()),
                (220, main.clone()),
                (280, main),
                (360, "/?PROBE_ID=1018212&SESSION_ID=next".into()),
            ]
        );
    }

    /// eooqd posts a one-off's result on its own as soon as it is done, to
    /// `&SRC=oneoff` on the session's endpoint (eooqd.c:796), framed as
    /// the main post but with nothing else in it; the minutely upload
    /// carries everything else and not the one-off.
    #[tokio::test(start_paused = true)]
    async fn a_one_off_result_is_posted_at_once_on_its_own() {
        let (transport, mut seen) = answering(&[OK]);
        let (handler, t0) = twin_handler(transport).await;
        let cancel = start(&handler);
        // The twin's first ONEOFF result came 19.8 s after KEEP.
        tokio::time::sleep(Duration::from_millis(19_800)).await;
        handler.expect_oneoff(222_024_177).await;
        let oneoff = make_result(222_024_177);
        handler.submit(oneoff.clone()).await;
        handler.submit(make_result(1001)).await;
        tokio::time::sleep(Duration::from_secs(50)).await;
        cancel.cancel();

        let posts = posts(&mut seen, t0);
        assert_eq!(posts.len(), 2, "{posts:?}");
        let (t, target, body) = &posts[0];
        assert!((t - (KEEP + 19.8)).abs() < 0.01, "at once: {t}");
        assert_eq!(*target, format!("{}&SRC=oneoff", main_path()));
        let line = AtlasResult::from_measurement(oneoff, None)
            .with_lts(-1)
            .to_result_line();
        assert_eq!(*body, format!("P_TO_C_REPORT\n{line}\nSESSION_ID {SID}\n"));

        let (t, target, body) = &posts[1];
        assert_eq!((t - KEEP).round(), 60.0);
        assert_eq!(*target, main_path());
        assert!(body.contains("\"1001\""), "{body}");
        assert!(!body.contains("222024177"), "{body}");
    }

    /// A failed one-off post leaves the result for eooqd's minutely re-post.
    #[tokio::test(start_paused = true)]
    async fn a_failed_one_off_post_is_retried_a_minute_later() {
        let (transport, mut seen) = answering(&["HTTP/1.1 500 \r\n\r\n", OK]);
        let (handler, t0) = twin_handler(transport).await;
        let cancel = start(&handler);
        tokio::time::sleep(Duration::from_secs(5)).await;
        handler.expect_oneoff(222_046_998).await;
        handler.submit(make_result(222_046_998)).await;
        tokio::time::sleep(Duration::from_secs(175)).await;
        cancel.cancel();

        let oneoffs: Vec<i64> = posts(&mut seen, t0)
            .into_iter()
            .filter(|(_, target, body)| {
                target.ends_with("&SRC=oneoff") && body.contains("222046998")
            })
            .map(|(t, _, _)| (t - KEEP).round() as i64)
            .collect();
        assert_eq!(oneoffs, [5, 60]);
    }

    /// 0007's holds stand on the controller's slots too: refused uploads go
    /// out on the slots a minute, two, four … half an hour apart.
    #[tokio::test(start_paused = true)]
    async fn rate_limited_uploads_back_off_on_the_controllers_slots() {
        let (tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let (handler, t0) = twin_handler(Box::new(RateLimitingController(tx))).await;
        handler
            .set_httppost_line(twin_line(60, 12, 0, None))
            .await
            .unwrap();
        let cancel = start(&handler);
        tokio::time::sleep(Duration::from_secs(3 * 3600)).await;
        cancel.cancel();

        let at: Vec<i64> = posts(&mut seen, t0)
            .into_iter()
            .filter(|(_, target, _)| !target.contains("SRC="))
            .map(|(t, _, _)| t.round() as i64)
            .collect();
        assert_eq!(at[0], 1_791_871_152);
        assert!(at.iter().all(|t| (t - 12) % 60 == 0), "{at:?}");
        let gaps: Vec<i64> = at.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(gaps, [60, 120, 240, 480, 960, 1800, 1800, 1800, 1800]);
    }

    /// A line the controller's crontab repeats keeps its slot (perd's
    /// CopyFromOld), and the URL it names is the one posted to.
    #[tokio::test(start_paused = true)]
    async fn the_controllers_url_is_posted_to() {
        let (transport, mut seen) = answering(&[OK]);
        let (handler, t0) = twin_handler(transport).await;
        let mut line = twin_line(60, 12, 20, None);
        line.url = "http://localhost:8080/?PROBE_ID=7".into();
        handler.set_httppost_line(line.clone()).await.unwrap();
        let next = handler.line_deadline(true, t0).await;
        for _ in 0..20 {
            handler.set_httppost_line(line.clone()).await.unwrap();
            assert_eq!(handler.line_deadline(true, t0).await, next, "offset kept");
        }
        handler.upload_pending().await.unwrap();
        let (_, req) = seen.try_recv().unwrap();
        assert!(
            req.starts_with("POST /?PROBE_ID=7 HTTP/1.1\r\nHost: localhost\r\n"),
            "{req}"
        );
        for (interval, start_time) in [(0, 12), (2 * 366 * 24 * 3600 + 1, 12), (60, -1)] {
            let bad = twin_line(interval, start_time, 0, None);
            assert!(handler.set_httppost_line(bad).await.is_err());
        }
    }
}
