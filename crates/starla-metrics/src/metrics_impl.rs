//! Prometheus metrics implementation
//!
//! Only compiled when the `export` feature is enabled.

use prometheus::{
    proto::MetricFamily, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec,
    IntGauge, Opts, Registry,
};
use std::sync::Arc;

/// Metrics registry for Starla probe operations.
#[derive(Clone)]
pub struct MetricsRegistry {
    registry: Arc<Registry>,

    // Measurements
    measurements_started: IntCounterVec,
    measurements_completed: IntCounterVec,
    measurements_failed: IntCounterVec,
    measurement_duration_seconds: HistogramVec,
    scheduler_tasks_active: IntGauge,

    // Uploads
    upload_attempts: IntCounter,
    upload_success: IntCounter,
    upload_failures: IntCounter,
    upload_duration_seconds: Histogram,
    upload_queue_depth: IntGauge,
    queue_dropped: IntCounter,

    // Connection
    controller_connected: IntGauge,
    connection_attempts: IntCounter,
}

impl MetricsRegistry {
    pub fn new() -> Result<Self, anyhow::Error> {
        let registry = Registry::new();

        // Measurements
        let measurements_started = IntCounterVec::new(
            Opts::new("starla_measurements_started_total", "Measurements started"),
            &["type"],
        )?;
        registry.register(Box::new(measurements_started.clone()))?;

        let measurements_completed = IntCounterVec::new(
            Opts::new(
                "starla_measurements_completed_total",
                "Measurements completed successfully",
            ),
            &["type"],
        )?;
        registry.register(Box::new(measurements_completed.clone()))?;

        let measurements_failed = IntCounterVec::new(
            Opts::new("starla_measurements_failed_total", "Measurements failed"),
            &["type"],
        )?;
        registry.register(Box::new(measurements_failed.clone()))?;

        let measurement_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "starla_measurement_duration_seconds",
                "Measurement execution duration",
            )
            .buckets(vec![0.01, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 120.0]),
            &["type"],
        )?;
        registry.register(Box::new(measurement_duration_seconds.clone()))?;

        let scheduler_tasks_active = IntGauge::new(
            "starla_scheduler_tasks_active",
            "Active scheduled measurement tasks",
        )?;
        registry.register(Box::new(scheduler_tasks_active.clone()))?;

        // Uploads
        let upload_attempts =
            IntCounter::new("starla_upload_attempts_total", "Result upload attempts")?;
        registry.register(Box::new(upload_attempts.clone()))?;

        let upload_success =
            IntCounter::new("starla_upload_success_total", "Successful result uploads")?;
        registry.register(Box::new(upload_success.clone()))?;

        let upload_failures =
            IntCounter::new("starla_upload_failures_total", "Failed result uploads")?;
        registry.register(Box::new(upload_failures.clone()))?;

        let upload_duration_seconds = Histogram::with_opts(
            HistogramOpts::new("starla_upload_duration_seconds", "Result upload duration")
                .buckets(vec![0.1, 0.5, 1.0, 5.0, 10.0, 30.0]),
        )?;
        registry.register(Box::new(upload_duration_seconds.clone()))?;

        let upload_queue_depth = IntGauge::new(
            "starla_upload_queue_depth",
            "Results waiting in the upload queue",
        )?;
        registry.register(Box::new(upload_queue_depth.clone()))?;

        let queue_dropped = IntCounter::new(
            "starla_queue_dropped_total",
            "Results dropped due to queue overflow",
        )?;
        registry.register(Box::new(queue_dropped.clone()))?;

        // Connection
        let controller_connected = IntGauge::new(
            "starla_controller_connected",
            "Whether the probe is connected to a controller (0/1)",
        )?;
        registry.register(Box::new(controller_connected.clone()))?;

        let connection_attempts = IntCounter::new(
            "starla_connection_attempts_total",
            "Controller connection attempts",
        )?;
        registry.register(Box::new(connection_attempts.clone()))?;

        Ok(Self {
            registry: Arc::new(registry),
            measurements_started,
            measurements_completed,
            measurements_failed,
            measurement_duration_seconds,
            scheduler_tasks_active,
            upload_attempts,
            upload_success,
            upload_failures,
            upload_duration_seconds,
            upload_queue_depth,
            queue_dropped,
            controller_connected,
            connection_attempts,
        })
    }

    pub fn registry(&self) -> Arc<Registry> {
        self.registry.clone()
    }

    pub fn gather(&self) -> Vec<MetricFamily> {
        self.registry.gather()
    }

    // Measurements

    pub fn record_measurement_started(&self, msm_type: &str) {
        self.measurements_started
            .with_label_values(&[msm_type])
            .inc();
    }

    pub fn record_measurement_completed(&self, msm_type: &str, duration: f64) {
        self.measurements_completed
            .with_label_values(&[msm_type])
            .inc();
        self.measurement_duration_seconds
            .with_label_values(&[msm_type])
            .observe(duration);
    }

    pub fn record_measurement_failed(&self, msm_type: &str, duration: f64) {
        self.measurements_failed
            .with_label_values(&[msm_type])
            .inc();
        self.measurement_duration_seconds
            .with_label_values(&[msm_type])
            .observe(duration);
    }

    pub fn update_scheduler_tasks(&self, count: i64) {
        self.scheduler_tasks_active.set(count);
    }

    // Uploads

    pub fn record_upload_attempt(&self) {
        self.upload_attempts.inc();
    }

    pub fn record_upload_success(&self) {
        self.upload_success.inc();
    }

    pub fn record_upload_failure(&self) {
        self.upload_failures.inc();
    }

    pub fn record_upload_duration(&self, duration: f64) {
        self.upload_duration_seconds.observe(duration);
    }

    pub fn update_queue_depth(&self, depth: i64) {
        self.upload_queue_depth.set(depth);
    }

    pub fn record_queue_drop(&self) {
        self.queue_dropped.inc();
    }

    // Connection

    pub fn set_connected(&self, connected: bool) {
        self.controller_connected.set(if connected { 1 } else { 0 });
    }

    pub fn record_connection_attempt(&self) {
        self.connection_attempts.inc();
    }
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new().expect("Failed to create MetricsRegistry")
    }
}
