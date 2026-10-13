//! Results management and upload for RIPE Atlas probe
//!
//! This crate handles:
//! - Queueing measurement results for upload (in-memory queue)
//! - Batching multiple results for efficient upload
//! - Retry logic with exponential backoff
//! - RIPE Atlas result format wrapping

pub mod format;
pub mod host_telemetry;
mod queue;
mod system_status;
mod time_sync;
mod uploader;

pub use format::{AtlasResult, ResultBundle};
pub use host_telemetry::{HostTelemetry, HostTelemetryKind};
pub use queue::{QueueStats, QueuedResult, ResultQueue};
pub use time_sync::TimeSyncTracker;
pub use uploader::{ResultUploader, UploadStream, UploadTransport, UploaderConfig};

use starla_common::MeasurementResult;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Configuration for the result handler
#[derive(Debug, Clone)]
pub struct ResultHandlerConfig {
    /// Upload interval
    pub upload_interval: Duration,
    /// Maximum result age before dropping (seconds)
    pub max_result_age_secs: i64,
    /// Maximum retry attempts per result
    pub max_attempts: u32,
    /// Cleanup interval for expired/failed results
    pub cleanup_interval: Duration,
    /// Maximum number of results in queue
    pub max_queue_size: usize,
}

impl Default for ResultHandlerConfig {
    fn default() -> Self {
        Self {
            upload_interval: Duration::from_secs(60),
            max_result_age_secs: 3600, // 1 hour
            max_attempts: 5,
            cleanup_interval: Duration::from_secs(300), // 5 minutes
            max_queue_size: 10000,
        }
    }
}

/// Main result handler that coordinates queuing and uploading
pub struct ResultHandler {
    queue: Arc<Mutex<ResultQueue>>,
    uploader: Arc<Mutex<ResultUploader>>,
    config: ResultHandlerConfig,
    time_sync: Arc<TimeSyncTracker>,
    session_id: Arc<Mutex<Option<String>>>,
    host_telemetry: Arc<HostTelemetry>,
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
        let time_sync = Arc::new(TimeSyncTracker::new());
        let uploader = ResultUploader::new(
            transport,
            uploader_config,
            metrics.clone(),
            Arc::clone(&host_telemetry),
            Arc::clone(&time_sync),
        );

        Self {
            queue: Arc::new(Mutex::new(queue)),
            uploader: Arc::new(Mutex::new(uploader)),
            config,
            time_sync,
            session_id: Arc::new(Mutex::new(None)),
            host_telemetry,
            metrics,
        }
    }

    /// Get a handle to the host-telemetry registry.
    /// Returned `Arc` is shared with the uploader; callers schedule reports
    /// via [`HostTelemetry::schedule_buddyinfo`] etc.
    pub fn host_telemetry(&self) -> Arc<HostTelemetry> {
        Arc::clone(&self.host_telemetry)
    }

    /// Set the upload endpoint path (call after controller connection)
    pub async fn set_endpoint_path(&self, path: String) -> anyhow::Result<()> {
        let mut uploader = self.uploader.lock().await;
        uploader.set_endpoint_path(path)?;
        debug!("Result upload endpoint path set");
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

    /// Submit a result for upload
    pub async fn submit(&self, result: MeasurementResult) {
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

    /// Try to upload pending results
    pub async fn upload_pending(&self) -> anyhow::Result<usize> {
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

        let lts = self.time_sync.lts();
        let session_id = self.session_id.lock().await;
        let session_id_ref = session_id.as_deref();

        let uploader = self.uploader.lock().await;
        match uploader
            .upload_batch(&uploadable, lts, session_id_ref)
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

    /// The 900 s clock-check post (`httppost … &SRC=timesync`).
    pub async fn post_timesync(&self) -> anyhow::Result<()> {
        let session_id = self.session_id.lock().await.clone();
        let uploader = self.uploader.lock().await;
        uploader.post_timesync(session_id.as_deref()).await
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
    }

    /// Run the upload loop
    pub async fn run(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        info!("Result handler starting");

        let mut upload_interval = tokio::time::interval(self.config.upload_interval);
        let mut cleanup_interval = tokio::time::interval(self.config.cleanup_interval);
        let mut timesync_interval = tokio::time::interval_at(
            tokio::time::Instant::now() + TIMESYNC_INTERVAL,
            TIMESYNC_INTERVAL,
        );

        let mut consecutive_failures = 0u32;
        let mut rate_limited = 0u32;
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(300);
        // No upload starts on a tick before this. A hold skips ticks rather
        // than sleeping inside the select, so cleanup and cancellation still
        // run during a half-hour hold.
        let mut hold_until: Option<tokio::time::Instant> = None;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!("Result handler shutting down");
                    break;
                }

                tick = upload_interval.tick() => {
                    if hold_until.is_some_and(|t| tick < t) {
                        continue;
                    }
                    match self.upload_pending().await {
                        Ok(_) => {
                            consecutive_failures = 0;
                            rate_limited = 0;
                            backoff = Duration::from_secs(1);
                        }
                        Err(e) => {
                            consecutive_failures += 1;
                            let err_msg = e.to_string();
                            if is_rate_limited(&e) {
                                rate_limited += 1;
                                // Parse "rate limited N" for Retry-After seconds
                                let retry_after = err_msg
                                    .strip_prefix("rate limited ")
                                    .and_then(|s| s.parse::<u64>().ok());
                                let hold = rate_limit_hold(rate_limited, retry_after);
                                warn!(
                                    "Rate limited ({} in a row), next upload in {}s",
                                    rate_limited,
                                    hold.as_secs()
                                );
                                hold_until = Some(tick + hold);
                            } else {
                                warn!("Upload failed (attempt {}): {}", consecutive_failures, e);
                                if consecutive_failures > 3 {
                                    debug!("Applying backoff: {:?}", backoff);
                                    hold_until = Some(tick + backoff);
                                    backoff = std::cmp::min(backoff * 2, max_backoff);
                                }
                            }
                        }
                    }
                }

                _ = cleanup_interval.tick() => {
                    self.run_cleanup().await;
                }

                tick = timesync_interval.tick() => {
                    // Not while a 429 hold is on: it would be one more refused
                    // request.
                    if hold_until.is_some_and(|t| tick < t) {
                        continue;
                    }
                    if let Err(e) = self.post_timesync().await {
                        debug!("Timesync post failed: {}", e);
                    }
                }
            }
        }

        Ok(())
    }
}

/// The uploader's 429 ("rate limited" or "rate limited <Retry-After>").
fn is_rate_limited(e: &anyhow::Error) -> bool {
    e.to_string().starts_with("rate limited")
}

/// The controller's crontab posts `&SRC=timesync` every 900 s.
const TIMESYNC_INTERVAL: Duration = Duration::from_secs(900);

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

        // Mid-hold (the last 429 came at 9,060 s, the next upload is due at
        // 10,860 s), a stop returns at once: no virtual time passes.
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
}
