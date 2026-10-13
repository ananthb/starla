//! Measurement Scheduler
//!
//! The scheduler is responsible for:
//! - Receiving measurement jobs from the command handler
//! - Executing measurements at the correct times
//! - Forwarding results to the result handler for upload

pub mod executor;
pub mod job;
mod lines;
pub mod task;

pub use job::{
    DnsJobSpec, HttpJobSpec, MeasurementJob, MeasurementSpec, NtpJobSpec, PingJobSpec, TlsJobSpec,
    TracerouteJobSpec,
};
pub use lines::{Crontab, CrontabLine};

use lines::Lines;
use starla_common::ProbeId;
use starla_results::ResultHandler;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Commands that can be sent to the scheduler
#[derive(Debug)]
pub enum SchedulerCommand {
    /// Schedule a measurement job on its own, outside any crontab (Starla's
    /// JSON commands): a recurring one replaces the one with its msm id
    Schedule(MeasurementJob),
    /// A crontab the controller finished: its file's recurring lines are
    /// now these, as eperd reads a crontab anew (eperd.c:513-645)
    Crontab(Crontab),
    /// Stop every line of a measurement
    Stop(u64),
    /// Execute a measurement immediately (one-shot)
    ExecuteNow(MeasurementJob),
}

pub struct Scheduler {
    /// The recurring lines, each crontab's and Starla's own
    lines: Arc<Mutex<Lines>>,
    /// Probe ID for measurement results
    probe_id: ProbeId,
    /// Result handler for uploading results
    result_handler: Option<Arc<ResultHandler>>,
    /// Metrics registry for recording operations
    metrics: starla_metrics::MetricsRegistry,
    /// Command receiver
    cmd_rx: Arc<Mutex<Option<mpsc::Receiver<SchedulerCommand>>>>,
    /// Command sender (for cloning)
    cmd_tx: mpsc::Sender<SchedulerCommand>,
    /// Cancellation token for graceful shutdown
    cancel_token: CancellationToken,
}

/// Read-only handle to the scheduler's task set. Exposed so the probe can
/// report active measurement counts to the tray without leaking the
/// internal task map type.
#[derive(Clone)]
pub struct SchedulerStatus {
    lines: Arc<Mutex<Lines>>,
}

impl SchedulerStatus {
    /// Count active recurring measurement lines grouped by type ("ping",
    /// "dns", ...).
    pub async fn measurement_counts(&self) -> HashMap<String, u64> {
        self.lines.lock().await.counts()
    }
}

impl Scheduler {
    /// Create a new scheduler
    pub fn new(probe_id: ProbeId, metrics: starla_metrics::MetricsRegistry) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(100);
        Self {
            lines: Arc::new(Mutex::new(Lines::default())),
            probe_id,
            result_handler: None,
            metrics,
            cmd_rx: Arc::new(Mutex::new(Some(cmd_rx))),
            cmd_tx,
            cancel_token: CancellationToken::new(),
        }
    }

    /// Get a cancellation token for stopping the scheduler
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Set the result handler for uploading measurement results
    pub fn set_result_handler(&mut self, handler: Arc<ResultHandler>) {
        self.result_handler = Some(handler);
    }

    /// Get a command sender for scheduling measurements
    pub fn command_sender(&self) -> mpsc::Sender<SchedulerCommand> {
        self.cmd_tx.clone()
    }

    /// Cheap clone of the line set for read-only status snapshots.
    /// Hands out a SchedulerStatus rather than the line set itself so it
    /// can stay private to this crate.
    pub fn status_handle(&self) -> SchedulerStatus {
        SchedulerStatus {
            lines: self.lines.clone(),
        }
    }

    /// Schedule a measurement job on its own
    ///
    /// DNS resolution is deferred to execution time to handle transient
    /// failures and ensure fresh resolution for each measurement execution.
    pub async fn schedule(&self, job: MeasurementJob) -> anyhow::Result<()> {
        if job.is_one_shot() {
            // Execute immediately - resolve DNS now
            debug!(msm_id = job.msm_id, "Executing one-shot measurement");
            self.execute_job(&job).await;
        } else {
            // Add to scheduled tasks - DNS resolution deferred to execution
            debug!(
                msm_id = job.msm_id,
                interval = job.interval,
                "Scheduling recurring measurement"
            );
            self.lines.lock().await.schedule(job, unix_now());
        }

        Ok(())
    }

    /// A crontab the controller finished: its file's lines become the
    /// crontab's. Lines it repeats keep their slots, its new lines wait for
    /// their first, and lines it no longer has stop; other files' lines are
    /// untouched.
    pub async fn synchronize(&self, crontab: Crontab) {
        self.synchronize_at(crontab, unix_now()).await;
    }

    async fn synchronize_at(&self, crontab: Crontab, now: i64) {
        let file = crontab.file.clone();
        let count = crontab.lines.len();
        let synced = self.lines.lock().await.synchronize(crontab, now);
        if synced.added + synced.dropped > 0 {
            info!(
                "Crontab {}: {} lines, {} new, {} kept, {} dropped",
                file, count, synced.added, synced.kept, synced.dropped
            );
        } else {
            debug!("Crontab {}: {} lines, all kept", file, count);
        }
    }

    /// Execute a job (resolve DNS and run measurement)
    async fn execute_job(&self, job: &MeasurementJob) {
        match job.to_measurement(self.probe_id) {
            Ok(measurement) => {
                self.execute_measurement(measurement.as_ref()).await;
            }
            Err(e) => {
                error!(msm_id = job.msm_id, "Failed to create measurement: {}", e);
            }
        }
    }

    /// Stop a measurement: every line with its id, in every crontab
    pub async fn stop(&self, msm_id: u64) {
        match self.lines.lock().await.stop(msm_id) {
            0 => warn!(msm_id, "Measurement not found"),
            lines => info!(msm_id, lines, "Stopped measurement"),
        }
    }

    /// Execute a measurement and handle the result
    async fn execute_measurement(&self, measurement: &dyn starla_measurements::Measurement) {
        let start_time = std::time::Instant::now();
        let msm_type = measurement.measurement_type().to_string();

        match measurement.execute().await {
            Ok(result) => {
                let duration = start_time.elapsed().as_secs_f64();
                debug!(msm_id = result.msm_id.0, "Measurement completed");

                self.metrics
                    .record_measurement_completed(&msm_type, duration);

                if let Some(ref handler) = self.result_handler {
                    handler.submit(result).await;
                } else {
                    warn!("No result handler configured!");
                }
            }
            Err(e) => {
                let duration = start_time.elapsed().as_secs_f64();
                error!("Measurement execution failed: {}", e);
                self.metrics.record_measurement_failed(&msm_type, duration);
            }
        }
    }

    /// Run the scheduler loop
    pub async fn run(&self) {
        info!("Scheduler started");

        // Take the receiver out of the Option
        let mut cmd_rx = {
            let mut guard = self.cmd_rx.lock().await;
            guard.take()
        };

        // Track if the command channel is closed
        let mut channel_closed = false;

        let mut tick_interval = tokio::time::interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                // Check for cancellation
                _ = self.cancel_token.cancelled() => {
                    info!("Scheduler received cancellation signal, stopping");
                    break;
                }

                // Process incoming commands (only if channel is open)
                cmd = async {
                    if channel_closed {
                        // Channel is closed, just wait forever (let other select branches run)
                        std::future::pending().await
                    } else {
                        match cmd_rx.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    }
                } => {
                    match cmd {
                        Some(SchedulerCommand::Schedule(job)) => {
                            if let Err(e) = self.schedule(job).await {
                                error!("Failed to schedule job: {}", e);
                            }
                        }
                        Some(SchedulerCommand::Crontab(crontab)) => {
                            self.synchronize(crontab).await;
                        }
                        Some(SchedulerCommand::Stop(msm_id)) => {
                            self.stop(msm_id).await;
                        }
                        Some(SchedulerCommand::ExecuteNow(job)) => {
                            match job.to_measurement(self.probe_id) {
                                Ok(m) => self.execute_measurement(m.as_ref()).await,
                                Err(e) => error!("Failed to create measurement: {}", e),
                            }
                        }
                        None => {
                            // Channel closed - this can happen during shutdown
                            // Continue running scheduled tasks, but don't accept new commands
                            warn!("Command channel closed, scheduler will continue running scheduled tasks");
                            channel_closed = true;
                        }
                    }
                }

                // Check for due tasks
                _ = tick_interval.tick() => {
                    self.check_due_tasks().await;

                    // Update metrics
                    let count = self.lines.lock().await.len() as i64;
                    self.metrics.update_scheduler_tasks(count);
                }
            }
        }

        info!("Scheduler stopped");
    }

    /// Check and execute due tasks
    async fn check_due_tasks(&self) {
        if let Some(state) = starla_common::read_pause_state() {
            if state.is_active(chrono::Utc::now()) {
                return;
            }
            // Stale pause file (timestamp elapsed) — clear it so the
            // tray's status panel stops claiming we're still paused.
            let _ = starla_common::write_pause_state(None);
        }

        self.run_due(unix_now()).await;
    }

    /// Start the runs due at `now`, each on a task of its own. A run holds
    /// its own copy of the job, so it finishes and reports whatever becomes
    /// of its line meanwhile, as a busy eperd line is kept until its test is
    /// done (DeleteFile, eperd.c:875-883).
    async fn run_due(&self, now: i64) -> Vec<JoinHandle<()>> {
        // Collect due jobs first (to avoid holding lock during execution)
        let due_jobs = self.lines.lock().await.take_due(now);

        // Execute due jobs concurrently
        let mut runs = Vec::with_capacity(due_jobs.len());
        for job in due_jobs {
            debug!(msm_id = job.msm_id, "Executing scheduled measurement");
            let result_handler = self.result_handler.clone();
            let metrics = self.metrics.clone();
            let probe_id = self.probe_id;
            runs.push(tokio::spawn(async move {
                match job.to_measurement(probe_id) {
                    Ok(measurement) => {
                        let start_time = std::time::Instant::now();
                        let msm_type = measurement.measurement_type().to_string();
                        match measurement.execute().await {
                            Ok(result) => {
                                let duration = start_time.elapsed().as_secs_f64();
                                debug!(msm_id = result.msm_id.0, "Measurement completed");
                                metrics.record_measurement_completed(&msm_type, duration);
                                if let Some(ref handler) = result_handler {
                                    handler.submit(result).await;
                                }
                            }
                            Err(e) => {
                                let duration = start_time.elapsed().as_secs_f64();
                                error!(msm_id = job.msm_id, "Measurement failed: {}", e);
                                metrics.record_measurement_failed(&msm_type, duration);
                            }
                        }
                    }
                    Err(e) => {
                        error!(msm_id = job.msm_id, "Failed to create measurement: {}", e);
                    }
                }
            }));
        }
        runs
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use starla_results::{ResultHandlerConfig, UploadStream, UploadTransport, UploaderConfig};

    struct NoTransport;
    impl UploadTransport for NoTransport {
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

    /// An evtdig line to a server that never answers: its run is under way
    /// for its whole timeout, then reports the timeout.
    async fn slow_dns_line(server: &tokio::net::UdpSocket) -> CrontabLine {
        CrontabLine {
            command: "evtdig -4 --timeout 3000 -A \"5009\" 127.0.0.1".into(),
            job: MeasurementJob {
                msm_id: 5009,
                interval: 60,
                start_time: 7,
                end_time: 2_000_000_000,
                spread: 0,
                spec: MeasurementSpec::Dns(DnsJobSpec {
                    server: "127.0.0.1".into(),
                    port: server.local_addr().unwrap().port(),
                    query: "example.com.".into(),
                    timeout_ms: 3000,
                    disable_check_addr: true,
                    ..Default::default()
                }),
            },
        }
    }

    /// A line the next crontab drops while its run is under way stops, and
    /// that run still reports: DeleteFile keeps a busy line until its test
    /// is done, and RunJob starts it no more (eperd.c:875-883, 1258-1262).
    #[tokio::test]
    async fn a_dropped_lines_run_under_way_still_reports() {
        let metrics = starla_metrics::MetricsRegistry::new().unwrap();
        let handler = Arc::new(ResultHandler::new(
            Box::new(NoTransport),
            UploaderConfig::default(),
            ResultHandlerConfig::default(),
            metrics.clone(),
        ));
        let mut scheduler = Scheduler::new(ProbeId(1018212), metrics);
        scheduler.set_result_handler(handler.clone());

        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let line = slow_dns_line(&server).await;
        let file = "/var/spool/ripe-atlas/crons/7".to_string();
        let slot = 1_791_836_407; // 7 + k * 60
        scheduler
            .synchronize_at(
                Crontab {
                    file: file.clone(),
                    lines: vec![line],
                },
                slot - 30,
            )
            .await;
        assert!(scheduler.run_due(slot - 1).await.is_empty());
        let runs = scheduler.run_due(slot).await;
        assert_eq!(runs.len(), 1);

        // The query is out, the run waiting for its answer: the crontab
        // comes again without the line
        let mut query = [0u8; 512];
        server.recv(&mut query).await.unwrap();
        scheduler
            .synchronize_at(
                Crontab {
                    file,
                    lines: Vec::new(),
                },
                slot + 1,
            )
            .await;
        assert!(scheduler
            .status_handle()
            .measurement_counts()
            .await
            .is_empty());
        assert_eq!(handler.queue_stats().await.count, 0);

        for run in runs {
            run.await.unwrap();
        }
        assert_eq!(handler.queue_stats().await.count, 1, "the run reported");
        // and the line runs no more
        for now in slot + 1..slot + 600 {
            assert!(scheduler.run_due(now).await.is_empty(), "{now}");
        }
    }
}
