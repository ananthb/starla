//! Prometheus metrics for Starla
//!
//! This crate provides metrics collection and export for the probe.
//! Metrics are only available when the `export` feature is enabled.

#[cfg(feature = "export")]
mod metrics_impl;

#[cfg(feature = "export")]
pub mod server;

#[cfg(feature = "export")]
pub use metrics_impl::MetricsRegistry;

#[cfg(feature = "export")]
pub use server::start_metrics_server;

// No-op implementation when metrics export is disabled
#[cfg(not(feature = "export"))]
#[derive(Clone, Default)]
pub struct MetricsRegistry;

#[cfg(not(feature = "export"))]
impl MetricsRegistry {
    pub fn new() -> Result<Self, anyhow::Error> {
        Ok(MetricsRegistry)
    }

    pub fn record_measurement_started(&self, _msm_type: &str) {}
    pub fn record_measurement_completed(&self, _msm_type: &str, _duration: f64) {}
    pub fn record_measurement_failed(&self, _msm_type: &str, _duration: f64) {}
    pub fn record_upload_attempt(&self) {}
    pub fn record_upload_success(&self) {}
    pub fn record_upload_failure(&self) {}
    pub fn record_upload_duration(&self, _duration: f64) {}
    pub fn update_queue_depth(&self, _depth: i64) {}
    pub fn update_queue_size(&self, _bytes: u64) {}
    pub fn update_pending_count(&self, _count: i64) {}
    pub fn update_database_size(&self, _bytes: u64) {}
    pub fn record_query_duration(&self, _operation: &str, _duration: f64) {}
    pub fn update_scheduler_tasks(&self, _count: i64) {}
    pub fn record_cleanup_run(
        &self,
        _deleted_by_time: u64,
        _deleted_by_size: u64,
        _freed_bytes: u64,
    ) {
    }
    pub fn record_cleanup_duration(&self, _duration: f64) {}
}
