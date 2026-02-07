//! Results management and upload for RIPE Atlas probe
//!
//! This crate handles:
//! - Queueing measurement results for upload (persistent RocksDB-backed queue)
//! - Batching multiple results for efficient upload
//! - Retry logic with exponential backoff
//! - Gzip compression with auto-negotiation
//! - RIPE Atlas result format wrapping

mod format;
mod persistent_queue;
mod time_sync;
mod uploader;

pub use format::{AtlasResult, ResultBundle};
pub use persistent_queue::{PersistentResultQueue, QueueStats, QueuedResult};
pub use time_sync::TimeSyncTracker;
pub use uploader::{CompressionMode, ResultUploader, UploaderConfig};

use starla_common::MeasurementResult;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Configuration for the result handler
#[derive(Debug, Clone)]
pub struct ResultHandlerConfig {
    /// Upload batch size
    pub batch_size: usize,
    /// Upload interval
    pub upload_interval: Duration,
    /// Maximum result age before dropping (seconds)
    pub max_result_age_secs: i64,
    /// Maximum retry attempts per result
    pub max_attempts: u32,
    /// Cleanup interval for expired/failed results
    pub cleanup_interval: Duration,
}

impl Default for ResultHandlerConfig {
    fn default() -> Self {
        Self {
            batch_size: 10,
            upload_interval: Duration::from_secs(10),
            max_result_age_secs: 3600, // 1 hour
            max_attempts: 5,
            cleanup_interval: Duration::from_secs(300), // 5 minutes
        }
    }
}

/// Main result handler that coordinates queuing and uploading
pub struct ResultHandler {
    /// Persistent queue (using Mutex for synchronous RocksDB access)
    queue: Arc<Mutex<PersistentResultQueue>>,
    /// Uploader
    uploader: Arc<Mutex<ResultUploader>>,
    /// Configuration
    config: ResultHandlerConfig,
    /// Time sync tracker for lts calculation
    time_sync: Arc<TimeSyncTracker>,
    /// Session ID for upload footer (per httppost --post-footer behavior)
    session_id: Arc<Mutex<Option<String>>>,
}

impl ResultHandler {
    /// Create a new result handler with persistent queue
    pub fn new(
        db_path: &Path,
        uploader_config: UploaderConfig,
        config: ResultHandlerConfig,
    ) -> anyhow::Result<Self> {
        let queue = PersistentResultQueue::new(db_path, 1000)?;
        let uploader = ResultUploader::new(uploader_config);

        Ok(Self {
            queue: Arc::new(Mutex::new(queue)),
            uploader: Arc::new(Mutex::new(uploader)),
            config,
            time_sync: Arc::new(TimeSyncTracker::new()),
            session_id: Arc::new(Mutex::new(None)),
        })
    }

    /// Create with default configuration
    pub fn with_defaults(db_path: &Path, uploader_config: UploaderConfig) -> anyhow::Result<Self> {
        Self::new(db_path, uploader_config, ResultHandlerConfig::default())
    }

    /// Set the upload endpoint (call after controller connection)
    pub async fn set_endpoint(&self, endpoint: String) {
        let mut uploader = self.uploader.lock().await;
        uploader.set_endpoint(endpoint);
        debug!("Result upload endpoint set");
    }

    /// Set the session ID (for upload body footer per httppost --post-footer
    /// behavior)
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

    /// Mark time as synchronized (call after NTP sync or similar)
    pub fn mark_time_synced(&self) {
        self.time_sync.mark_synced();
    }

    /// Get the time sync tracker (for external use)
    pub fn time_sync(&self) -> Arc<TimeSyncTracker> {
        Arc::clone(&self.time_sync)
    }

    /// Get current lts value
    pub fn current_lts(&self) -> i64 {
        self.time_sync.lts()
    }

    /// Submit a result for upload
    pub async fn submit(&self, result: MeasurementResult) -> anyhow::Result<()> {
        let mut queue = self.queue.lock().await;
        queue.enqueue(result)?;

        let stats = queue.stats()?;
        debug!(
            "Result enqueued, queue: {} memory / {} disk",
            stats.memory_count, stats.disk_count
        );

        Ok(())
    }

    /// Get current queue statistics
    pub async fn queue_stats(&self) -> anyhow::Result<QueueStats> {
        let queue = self.queue.lock().await;
        queue.stats()
    }

    /// Try to upload pending results
    pub async fn upload_pending(&self) -> anyhow::Result<usize> {
        // Check if we have an endpoint
        if !self.has_endpoint().await {
            debug!("No upload endpoint configured, skipping upload");
            return Ok(0);
        }

        // Get batch from queue
        let results = {
            let mut queue = self.queue.lock().await;
            queue.drain_batch(self.config.batch_size)
        };

        if results.is_empty() {
            return Ok(0);
        }

        // Filter out expired results
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

        let (valid, expired): (Vec<_>, Vec<_>) = results
            .into_iter()
            .partition(|r| now - r.queued_at < self.config.max_result_age_secs);

        if !expired.is_empty() {
            warn!("Dropping {} expired results", expired.len());
        }

        if valid.is_empty() {
            return Ok(0);
        }

        // Filter out results that have exceeded max attempts
        let (uploadable, failed): (Vec<_>, Vec<_>) = valid
            .into_iter()
            .partition(|r| r.attempts < self.config.max_attempts);

        if !failed.is_empty() {
            warn!(
                "Dropping {} results that exceeded max attempts",
                failed.len()
            );
        }

        if uploadable.is_empty() {
            return Ok(0);
        }

        let count = uploadable.len();
        info!("Uploading {} results", count);

        // Get current lts value
        let lts = self.time_sync.lts();

        // Get session ID for footer
        let session_id = self.session_id.lock().await;
        let session_id_ref = session_id.as_deref();

        // Try to upload
        let uploader = self.uploader.lock().await;
        match uploader
            .upload_batch(&uploadable, lts, session_id_ref)
            .await
        {
            Ok(()) => {
                // Mark as uploaded (delete from disk)
                let mut queue = self.queue.lock().await;
                queue.mark_uploaded(&uploadable)?;
                info!("Successfully uploaded {} results", count);
                Ok(count)
            }
            Err(e) => {
                // Re-queue failed results with incremented attempt counter
                warn!("Upload failed: {}", e);
                let mut queue = self.queue.lock().await;
                queue.requeue_failed(uploadable)?;
                Err(e)
            }
        }
    }

    /// Run cleanup to remove expired and failed results
    pub async fn run_cleanup(&self) -> anyhow::Result<()> {
        let mut queue = self.queue.lock().await;

        // Clean expired
        queue.cleanup_expired(self.config.max_result_age_secs)?;

        // Clean failed
        queue.cleanup_failed(self.config.max_attempts)?;

        Ok(())
    }

    /// Flush queue to disk (for graceful shutdown)
    pub async fn flush(&self) -> anyhow::Result<()> {
        let mut queue = self.queue.lock().await;
        queue.flush()
    }

    /// Run the upload loop
    ///
    /// This is the main entry point for running the result handler.
    /// It will periodically upload pending results and clean up expired ones.
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
                        Ok(_) => {
                            // No results to upload, that's fine
                        }
                        Err(e) => {
                            consecutive_failures += 1;
                            warn!(
                                "Upload failed (attempt {}): {}",
                                consecutive_failures, e
                            );

                            // Apply backoff
                            if consecutive_failures > 3 {
                                debug!("Applying backoff: {:?}", backoff);
                                tokio::time::sleep(backoff).await;
                                backoff = std::cmp::min(backoff * 2, max_backoff);
                            }
                        }
                    }
                }

                _ = cleanup_interval.tick() => {
                    if let Err(e) = self.run_cleanup().await {
                        error!("Cleanup failed: {}", e);
                    }
                }
            }
        }

        // Final flush on shutdown
        if let Err(e) = self.flush().await {
            error!("Failed to flush queue on shutdown: {}", e);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starla_common::{MeasurementData, MeasurementId, MeasurementType, ProbeId, Timestamp};
    use std::net::IpAddr;
    use tempfile::tempdir;

    fn make_result(msm_id: u64) -> MeasurementResult {
        MeasurementResult {
            fw: 6000,
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

    #[tokio::test]
    async fn test_result_handler_creation() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("results.db");

        let handler = ResultHandler::new(
            &db_path,
            UploaderConfig::default(),
            ResultHandlerConfig::default(),
        )
        .unwrap();

        // Should start empty
        let stats = handler.queue_stats().await.unwrap();
        assert_eq!(stats.memory_count, 0);
        assert_eq!(stats.disk_count, 0);
    }

    #[tokio::test]
    async fn test_submit_and_stats() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("results.db");

        let handler = ResultHandler::new(
            &db_path,
            UploaderConfig::default(),
            ResultHandlerConfig::default(),
        )
        .unwrap();

        // Submit a result
        handler.submit(make_result(1001)).await.unwrap();

        // Check stats
        let stats = handler.queue_stats().await.unwrap();
        assert_eq!(stats.memory_count, 1);
        assert_eq!(stats.disk_count, 1);
    }
}
