//! Measurement Scheduler
//!
//! The scheduler is responsible for:
//! - Receiving measurement jobs from the command handler
//! - Executing measurements at the correct times
//! - Forwarding results to the result handler for upload

pub mod executor;
pub mod job;
pub mod task;

pub use job::{
    DnsJobSpec, HttpJobSpec, MeasurementJob, MeasurementSpec, NtpJobSpec, PingJobSpec, TlsJobSpec,
    TracerouteJobSpec,
};

use starla_common::ProbeId;
use starla_results::ResultHandler;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use task::ScheduledTask;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Commands that can be sent to the scheduler
#[derive(Debug)]
pub enum SchedulerCommand {
    /// Schedule a new measurement job
    Schedule(MeasurementJob),
    /// Stop a measurement
    Stop(u64),
    /// Execute a measurement immediately (one-shot)
    ExecuteNow(MeasurementJob),
}

pub struct Scheduler {
    /// Scheduled tasks (recurring measurements)
    tasks: Arc<Mutex<HashMap<u64, ScheduledTask>>>,
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
    tasks: Arc<Mutex<HashMap<u64, ScheduledTask>>>,
}

impl SchedulerStatus {
    /// Count active recurring measurements grouped by type ("ping", "dns",
    /// ...).
    pub async fn measurement_counts(&self) -> HashMap<String, u64> {
        let tasks = self.tasks.lock().await;
        let mut counts: HashMap<String, u64> = HashMap::new();
        for task in tasks.values() {
            *counts
                .entry(task.job.spec.type_name().to_string())
                .or_insert(0) += 1;
        }
        counts
    }
}

impl Scheduler {
    /// Create a new scheduler
    pub fn new(probe_id: ProbeId, metrics: starla_metrics::MetricsRegistry) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(100);
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
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

    /// Cheap clone of the tasks map for read-only status snapshots.
    /// Hands out a SchedulerStatus rather than the raw map so ScheduledTask
    /// can stay private to this crate.
    pub fn status_handle(&self) -> SchedulerStatus {
        SchedulerStatus {
            tasks: self.tasks.clone(),
        }
    }

    /// Schedule a measurement job
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
            let task = ScheduledTask::new(
                job.msm_id,
                job.interval,
                job.start_time,
                job.end_time,
                job.spread,
                job.clone(),
            );

            let mut tasks = self.tasks.lock().await;
            debug!(
                msm_id = job.msm_id,
                interval = job.interval,
                "Scheduling recurring measurement"
            );
            tasks.insert(job.msm_id, task);
        }

        Ok(())
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

    /// Stop a measurement
    pub async fn stop(&self, msm_id: u64) {
        let mut tasks = self.tasks.lock().await;
        if tasks.remove(&msm_id).is_some() {
            info!(msm_id, "Stopped measurement");
        } else {
            warn!(msm_id, "Measurement not found");
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
                    let count = self.tasks.lock().await.len() as i64;
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

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Collect due jobs first (to avoid holding lock during execution)
        let (due_jobs, _expired): (Vec<_>, Vec<_>) = {
            let mut tasks = self.tasks.lock().await;
            let mut due = Vec::new();
            let mut exp = Vec::new();

            for (msm_id, task) in tasks.iter_mut() {
                if task.is_due(now) {
                    due.push(task.job.clone());
                    task.schedule_next();

                    if task.is_expired(now) {
                        exp.push(*msm_id);
                    }
                }
            }

            // Remove expired tasks while we have the lock
            for msm_id in &exp {
                info!(msm_id, "Measurement expired, removing from schedule");
                tasks.remove(msm_id);
            }

            (due, exp)
        };

        // Execute due jobs concurrently
        for job in due_jobs {
            debug!(msm_id = job.msm_id, "Executing scheduled measurement");
            let result_handler = self.result_handler.clone();
            let metrics = self.metrics.clone();
            let probe_id = self.probe_id;
            tokio::spawn(async move {
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
            });
        }
    }
}
