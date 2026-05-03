//! Results management and upload for RIPE Atlas probe
//!
//! This crate handles:
//! - Queueing measurement results for upload (in-memory queue)
//! - Batching multiple results for efficient upload
//! - Retry logic with exponential backoff
//! - RIPE Atlas result format wrapping

pub mod format;
mod queue;
mod system_status;
mod time_sync;
mod uploader;

pub use format::{AtlasResult, ResultBundle};
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
        let uploader = ResultUploader::new(transport, uploader_config, metrics.clone());

        Self {
            queue: Arc::new(Mutex::new(queue)),
            uploader: Arc::new(Mutex::new(uploader)),
            config,
            time_sync: Arc::new(TimeSyncTracker::new()),
            session_id: Arc::new(Mutex::new(None)),
            metrics,
        }
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
        queue.enqueue(result);
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

        if results.is_empty() {
            return Ok(0);
        }

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

        if uploadable.is_empty() {
            return Ok(0);
        }

        let count = uploadable.len();
        info!("Uploading {} results", count);

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
                queue.requeue_failed(uploadable);
                self.metrics.update_queue_depth(queue.stats().count as i64);
                Err(e)
            }
        }
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

        let mut consecutive_failures = 0u32;
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(300);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!("Result handler shutting down");
                    break;
                }

                _ = upload_interval.tick() => {
                    match self.upload_pending().await {
                        Ok(count) if count > 0 => {
                            consecutive_failures = 0;
                            backoff = Duration::from_secs(1);
                        }
                        Ok(_) => {}
                        Err(e) => {
                            consecutive_failures += 1;
                            let err_msg = e.to_string();
                            if err_msg.contains("rate limited") {
                                // Parse "rate limited N" for Retry-After seconds
                                let retry_secs = err_msg
                                    .strip_prefix("rate limited ")
                                    .and_then(|s| s.parse::<u64>().ok())
                                    .unwrap_or(60);
                                backoff = Duration::from_secs(retry_secs).max(Duration::from_secs(60));
                                warn!("Rate limited, retrying in {}s", backoff.as_secs());
                                tokio::time::sleep(backoff).await;
                            } else {
                                warn!("Upload failed (attempt {}): {}", consecutive_failures, e);
                                if consecutive_failures > 3 {
                                    debug!("Applying backoff: {:?}", backoff);
                                    tokio::time::sleep(backoff).await;
                                    backoff = std::cmp::min(backoff * 2, max_backoff);
                                }
                            }
                        }
                    }
                }

                _ = cleanup_interval.tick() => {
                    self.run_cleanup().await;
                }
            }
        }

        Ok(())
    }
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
}
