//! Prometheus metrics implementation
//!
//! Only compiled when the `export` feature is enabled.

use prometheus::{
    proto::MetricFamily, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec,
    IntGauge, Opts, Registry,
};
use std::sync::Arc;

/// Metrics registry for Starla
///
/// Collects and exposes Prometheus metrics for all probe operations.
#[derive(Clone)]
pub struct MetricsRegistry {
    registry: Arc<Registry>,

    // Counters - measurements (per type)
    measurements_total: IntCounterVec,
    measurements_completed: IntCounterVec,
    measurements_failed: IntCounterVec,

    // Counters - uploads
    upload_attempts_total: IntCounter,
    upload_success_total: IntCounter,
    upload_failures_total: IntCounter,

    // Counters - cleanup
    cleanup_runs_total: IntCounter,
    cleanup_measurements_deleted: IntCounterVec,
    cleanup_freed_bytes: IntCounter,

    // Gauges - current state
    measurements_pending: IntGauge,
    upload_queue_depth: IntGauge,
    upload_queue_size_bytes: IntGauge,
    database_size_bytes: IntGauge,
    scheduler_tasks_active: IntGauge,

    // Histograms - durations (per type)
    measurement_duration_seconds: HistogramVec,
    upload_duration_seconds: Histogram,
    database_query_duration_seconds: HistogramVec,
    cleanup_duration_seconds: Histogram,
}

impl MetricsRegistry {
    /// Create a new metrics registry
    pub fn new() -> Result<Self, anyhow::Error> {
        let registry = Registry::new();

        // Counters - measurements
        let measurements_total = IntCounterVec::new(
            Opts::new(
                "starla_measurements_total",
                "Total number of measurements by type and status",
            ),
            &["type", "status"],
        )?;
        registry.register(Box::new(measurements_total.clone()))?;

        let measurements_completed = IntCounterVec::new(
            Opts::new(
                "starla_measurements_completed_total",
                "Total number of completed measurements by type",
            ),
            &["type"],
        )?;
        registry.register(Box::new(measurements_completed.clone()))?;

        let measurements_failed = IntCounterVec::new(
            Opts::new(
                "starla_measurements_failed_total",
                "Total number of failed measurements by type",
            ),
            &["type"],
        )?;
        registry.register(Box::new(measurements_failed.clone()))?;

        // Counters - uploads
        let upload_attempts_total = IntCounter::new(
            "atlas_upload_attempts_total",
            "Total number of upload attempts",
        )?;
        registry.register(Box::new(upload_attempts_total.clone()))?;

        let upload_success_total = IntCounter::new(
            "atlas_upload_success_total",
            "Total number of successful uploads",
        )?;
        registry.register(Box::new(upload_success_total.clone()))?;

        let upload_failures_total = IntCounter::new(
            "atlas_upload_failures_total",
            "Total number of failed uploads",
        )?;
        registry.register(Box::new(upload_failures_total.clone()))?;

        // Counters - cleanup
        let cleanup_runs_total = IntCounter::new(
            "atlas_cleanup_runs_total",
            "Total number of cleanup cycles run",
        )?;
        registry.register(Box::new(cleanup_runs_total.clone()))?;

        let cleanup_measurements_deleted = IntCounterVec::new(
            Opts::new(
                "atlas_cleanup_measurements_deleted_total",
                "Total measurements deleted by cleanup reason",
            ),
            &["reason"],
        )?;
        registry.register(Box::new(cleanup_measurements_deleted.clone()))?;

        let cleanup_freed_bytes = IntCounter::new(
            "atlas_cleanup_freed_bytes_total",
            "Total bytes freed by cleanup operations",
        )?;
        registry.register(Box::new(cleanup_freed_bytes.clone()))?;

        // Gauges
        let measurements_pending = IntGauge::new(
            "starla_measurements_pending",
            "Number of pending measurements",
        )?;
        registry.register(Box::new(measurements_pending.clone()))?;

        let upload_queue_depth = IntGauge::new(
            "atlas_upload_queue_depth",
            "Number of items in upload queue",
        )?;
        registry.register(Box::new(upload_queue_depth.clone()))?;

        let upload_queue_size_bytes = IntGauge::new(
            "atlas_upload_queue_size_bytes",
            "Size of upload queue in bytes",
        )?;
        registry.register(Box::new(upload_queue_size_bytes.clone()))?;

        let database_size_bytes = IntGauge::new(
            "starla_database_size_bytes",
            "Total database size in bytes (including WAL and SHM)",
        )?;
        registry.register(Box::new(database_size_bytes.clone()))?;

        let scheduler_tasks_active = IntGauge::new(
            "starla_scheduler_tasks_active",
            "Number of active scheduled tasks",
        )?;
        registry.register(Box::new(scheduler_tasks_active.clone()))?;

        // Histograms - measurement durations by type
        let measurement_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "atlas_measurement_duration_seconds",
                "Measurement execution duration by type",
            )
            .buckets(vec![0.001, 0.01, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0]),
            &["type"],
        )?;
        registry.register(Box::new(measurement_duration_seconds.clone()))?;

        // Histograms - upload durations
        let upload_duration_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "atlas_upload_duration_seconds",
                "Upload duration in seconds",
            )
            .buckets(vec![0.1, 0.5, 1.0, 5.0, 10.0, 30.0]),
        )?;
        registry.register(Box::new(upload_duration_seconds.clone()))?;

        // Histograms - database query durations
        let database_query_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "starla_database_query_duration_seconds",
                "Database query duration by operation",
            )
            .buckets(vec![0.0001, 0.001, 0.01, 0.1, 1.0]),
            &["operation"],
        )?;
        registry.register(Box::new(database_query_duration_seconds.clone()))?;

        // Histograms - cleanup durations
        let cleanup_duration_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "atlas_cleanup_duration_seconds",
                "Cleanup cycle duration in seconds",
            )
            .buckets(vec![0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0]),
        )?;
        registry.register(Box::new(cleanup_duration_seconds.clone()))?;

        Ok(Self {
            registry: Arc::new(registry),
            measurements_total,
            measurements_completed,
            measurements_failed,
            upload_attempts_total,
            upload_success_total,
            upload_failures_total,
            cleanup_runs_total,
            cleanup_measurements_deleted,
            cleanup_freed_bytes,
            measurements_pending,
            upload_queue_depth,
            upload_queue_size_bytes,
            database_size_bytes,
            scheduler_tasks_active,
            measurement_duration_seconds,
            upload_duration_seconds,
            database_query_duration_seconds,
            cleanup_duration_seconds,
        })
    }

    /// Get the Prometheus registry for exporting metrics
    pub fn registry(&self) -> Arc<Registry> {
        self.registry.clone()
    }

    /// Gather all metrics from the registry
    pub fn gather(&self) -> Vec<MetricFamily> {
        self.registry.gather()
    }

    // Measurement metrics

    /// Record a measurement start
    pub fn record_measurement_started(&self, msm_type: &str) {
        self.measurements_total
            .with_label_values(&[msm_type, "started"])
            .inc();
    }

    /// Record a completed measurement
    pub fn record_measurement_completed(&self, msm_type: &str, duration: f64) {
        self.measurements_total
            .with_label_values(&[msm_type, "completed"])
            .inc();
        self.measurements_completed
            .with_label_values(&[msm_type])
            .inc();
        self.measurement_duration_seconds
            .with_label_values(&[msm_type])
            .observe(duration);
    }

    /// Record a failed measurement
    pub fn record_measurement_failed(&self, msm_type: &str, duration: f64) {
        self.measurements_total
            .with_label_values(&[msm_type, "failed"])
            .inc();
        self.measurements_failed
            .with_label_values(&[msm_type])
            .inc();
        self.measurement_duration_seconds
            .with_label_values(&[msm_type])
            .observe(duration);
    }

    // Upload metrics

    /// Record an upload attempt
    pub fn record_upload_attempt(&self) {
        self.upload_attempts_total.inc();
    }

    /// Record a successful upload
    pub fn record_upload_success(&self) {
        self.upload_success_total.inc();
    }

    /// Record a failed upload
    pub fn record_upload_failure(&self) {
        self.upload_failures_total.inc();
    }

    /// Record upload duration
    pub fn record_upload_duration(&self, duration: f64) {
        self.upload_duration_seconds.observe(duration);
    }

    // Queue metrics

    /// Update the upload queue depth
    pub fn update_queue_depth(&self, depth: i64) {
        self.upload_queue_depth.set(depth);
    }

    /// Update the upload queue size in bytes
    pub fn update_queue_size(&self, bytes: u64) {
        self.upload_queue_size_bytes.set(bytes as i64);
    }

    /// Update pending measurements count
    pub fn update_pending_count(&self, count: i64) {
        self.measurements_pending.set(count);
    }

    // Database metrics

    /// Update the database size in bytes
    pub fn update_database_size(&self, bytes: u64) {
        self.database_size_bytes.set(bytes as i64);
    }

    /// Record a database query duration
    pub fn record_query_duration(&self, operation: &str, duration: f64) {
        self.database_query_duration_seconds
            .with_label_values(&[operation])
            .observe(duration);
    }

    // Scheduler metrics

    /// Update the number of active scheduled tasks
    pub fn update_scheduler_tasks(&self, count: i64) {
        self.scheduler_tasks_active.set(count);
    }

    // Cleanup metrics

    /// Record a cleanup run
    pub fn record_cleanup_run(&self, deleted_by_time: u64, deleted_by_size: u64, freed_bytes: u64) {
        self.cleanup_runs_total.inc();
        self.cleanup_measurements_deleted
            .with_label_values(&["time"])
            .inc_by(deleted_by_time);
        self.cleanup_measurements_deleted
            .with_label_values(&["size"])
            .inc_by(deleted_by_size);
        self.cleanup_freed_bytes.inc_by(freed_bytes);
    }

    /// Record cleanup duration
    pub fn record_cleanup_duration(&self, duration: f64) {
        self.cleanup_duration_seconds.observe(duration);
    }
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new().expect("Failed to create MetricsRegistry")
    }
}
