//! Result uploader for sending measurements to the controller
//!
//! Supports gzip compression with auto-negotiation based on server
//! capabilities.

use super::format::AtlasResult;
use super::persistent_queue::QueuedResult;
use flate2::write::GzEncoder;
use flate2::Compression;
use reqwest::Client;
use starla_common::MeasurementResult;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::{debug, error, trace, warn};

/// Compression mode for uploads
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompressionMode {
    /// Always compress (if above threshold)
    Always,
    /// Never compress
    Never,
    /// Auto-detect based on server capabilities
    #[default]
    Auto,
}

/// Configuration for the result uploader
#[derive(Debug, Clone)]
pub struct UploaderConfig {
    /// Upload endpoint URL
    pub endpoint: String,
    /// Request timeout
    pub timeout: Duration,
    /// Maximum retry attempts per result
    pub max_retries: u32,
    /// Base delay between retries
    pub retry_delay: Duration,
    /// Maximum delay between retries
    pub max_retry_delay: Duration,
    /// Compression mode
    pub compression: CompressionMode,
    /// Minimum body size to compress (bytes)
    pub compression_threshold: usize,
}

impl Default for UploaderConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            timeout: Duration::from_secs(15),
            max_retries: 3,
            retry_delay: Duration::from_secs(5),
            max_retry_delay: Duration::from_secs(60),
            compression: CompressionMode::Auto,
            compression_threshold: 1024, // Only compress if > 1KB
        }
    }
}

/// Uploads measurement results to the controller
pub struct ResultUploader {
    client: Client,
    config: UploaderConfig,
    /// Whether server supports gzip (cached after first probe)
    server_supports_gzip: AtomicBool,
    /// Whether we've probed the server yet
    has_probed_server: AtomicBool,
}

impl ResultUploader {
    /// Create a new uploader
    pub fn new(config: UploaderConfig) -> Self {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            config,
            server_supports_gzip: AtomicBool::new(false),
            has_probed_server: AtomicBool::new(false),
        }
    }

    /// Set the upload endpoint
    pub fn set_endpoint(&mut self, endpoint: String) {
        self.config.endpoint = endpoint;
        // Reset probe state when endpoint changes
        self.has_probed_server.store(false, Ordering::SeqCst);
        self.server_supports_gzip.store(false, Ordering::SeqCst);
    }

    /// Get the current endpoint
    pub fn endpoint(&self) -> &str {
        &self.config.endpoint
    }

    /// Check if an endpoint is configured
    pub fn has_endpoint(&self) -> bool {
        !self.config.endpoint.is_empty()
    }

    /// Probe server for compression support
    ///
    /// Note: The official RIPE Atlas httppost tool does not use compression,
    /// so we disable compression probing to avoid sending unexpected requests
    /// to the controller. We return false to indicate compression is not
    /// supported.
    async fn probe_compression_support(&self) -> bool {
        // The official httppost tool does not use compression
        // Disable probing to match reference behavior
        debug!("Compression disabled to match official httppost behavior");
        false
    }

    /// Ensure we've probed the server for compression support
    async fn ensure_probed(&self) {
        if self.config.compression == CompressionMode::Auto
            && !self.has_probed_server.load(Ordering::SeqCst)
        {
            let supports = self.probe_compression_support().await;
            self.server_supports_gzip.store(supports, Ordering::SeqCst);
            self.has_probed_server.store(true, Ordering::SeqCst);
        }
    }

    /// Check if we should compress based on mode and server support
    fn should_compress(&self, size: usize) -> bool {
        if size < self.config.compression_threshold {
            return false;
        }

        match self.config.compression {
            CompressionMode::Never => false,
            CompressionMode::Always => true,
            CompressionMode::Auto => self.server_supports_gzip.load(Ordering::SeqCst),
        }
    }

    /// Compress data using gzip
    fn compress(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data)?;
        let compressed = encoder.finish()?;

        debug!(
            "Compressed {} bytes to {} bytes ({:.1}% reduction)",
            data.len(),
            compressed.len(),
            (1.0 - compressed.len() as f64 / data.len() as f64) * 100.0
        );

        Ok(compressed)
    }

    /// Upload a single result
    ///
    /// The `lts` parameter is the current "local time sync" value - seconds
    /// since last time sync. The `session_id` is appended to the body as a
    /// footer (per official httppost behavior).
    pub async fn upload(
        &self,
        result: &MeasurementResult,
        lts: i64,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        if !self.has_endpoint() {
            anyhow::bail!("No upload endpoint configured");
        }

        self.ensure_probed().await;

        // Convert to AtlasResult format matching official httppost format:
        // 1. Header: "P_TO_C_REPORT\n" (probe-to-controller report header)
        // 2. Stream marker: "RESULT 9907 ongoing\n"
        // 3. Result lines: "RESULT <json>\n"
        // 4. Footer: "SESSION_ID <session_id>\n" (note: SESSION_ID prefix required)
        let atlas_result = AtlasResult::from_measurement(result.clone(), None).with_lts(lts);
        let json = serde_json::to_vec(&atlas_result)?;

        let header = b"P_TO_C_REPORT\n";
        let stream_marker = b"RESULT 9907 ongoing\n";
        // Footer format: "SESSION_ID <session_id>\n" (matches con_session_id.txt
        // format)
        let footer_len = session_id
            .map(|s| "SESSION_ID ".len() + s.len() + 1)
            .unwrap_or(0);
        let mut body = Vec::with_capacity(
            header.len() + stream_marker.len() + b"RESULT ".len() + json.len() + 1 + footer_len,
        );
        body.extend_from_slice(header);
        body.extend_from_slice(stream_marker);
        body.extend_from_slice(b"RESULT ");
        body.extend_from_slice(&json);
        body.push(b'\n');

        // Append session ID as footer (per official httppost --post-footer behavior)
        // Format must be "SESSION_ID <session_id>\n" to match con_session_id.txt
        if let Some(sid) = session_id {
            body.extend_from_slice(b"SESSION_ID ");
            body.extend_from_slice(sid.as_bytes());
            body.push(b'\n');
        }

        debug!(
            "Uploading result: msm_id={}, type={:?}",
            result.msm_id, result.measurement_type
        );

        let (body, is_compressed) = if self.should_compress(body.len()) {
            (Self::compress(&body)?, true)
        } else {
            (body, false)
        };

        // RIPE Atlas controller expects application/x-www-form-urlencoded format
        // with User-Agent matching the official httppost tool
        // Responds with "OK\n" for success or "ERR\n" for failure
        let mut request = self
            .client
            .post(&self.config.endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", "httppost for atlas.ripe.net")
            .header("Connection", "close");

        if is_compressed {
            request = request.header("Content-Encoding", "gzip");
        }

        let response = request.body(body).send().await?;

        if response.status().is_success() {
            // Check response body for OK/ERR
            let response_body = response.text().await.unwrap_or_default();
            if response_body.trim() == "OK" {
                debug!("Upload successful");
                Ok(())
            } else {
                debug!("Upload rejected by controller: {:?}", response_body);
                anyhow::bail!("Upload rejected: {}", response_body.trim())
            }
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            debug!(
                "Upload rejected: status={}, response_body={:?}",
                status, body
            );
            anyhow::bail!("Upload failed: {} - {}", status, body)
        }
    }

    /// Upload a batch of results
    ///
    /// The `lts` parameter is the current "local time sync" value - seconds
    /// since last time sync. The `session_id` is appended to the body as a
    /// footer (per official httppost behavior).
    pub async fn upload_batch(
        &self,
        results: &[QueuedResult],
        lts: i64,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        if results.is_empty() {
            return Ok(());
        }

        if !self.has_endpoint() {
            anyhow::bail!("No upload endpoint configured");
        }

        self.ensure_probed().await;

        // RIPE Atlas expects results in a specific format matching official httppost:
        // 1. Header: "P_TO_C_REPORT\n" (probe-to-controller report header)
        // 2. Stream marker: "RESULT 9907 ongoing\n"
        // 3. Each result line prefixed with "RESULT " followed by JSON
        // 4. Footer: "SESSION_ID <session_id>\n" (note: SESSION_ID prefix required)
        let mut body = Vec::new();

        // Add probe-to-controller report header (from p_to_c_report_header file)
        body.extend_from_slice(b"P_TO_C_REPORT\n");

        // Add stream marker header (Stream 9907 = measurement results)
        body.extend_from_slice(b"RESULT 9907 ongoing\n");

        // Add each result as "RESULT <json>\n"
        for queued in results {
            // Convert to AtlasResult format and set lts
            let atlas_result =
                AtlasResult::from_measurement(queued.result.clone(), None).with_lts(lts);
            body.extend_from_slice(b"RESULT ");
            let json = serde_json::to_vec(&atlas_result)?;
            body.extend_from_slice(&json);
            body.push(b'\n');
        }

        // Append session ID as footer (per official httppost --post-footer behavior)
        // Format must be "SESSION_ID <session_id>\n" to match con_session_id.txt
        if let Some(sid) = session_id {
            body.extend_from_slice(b"SESSION_ID ");
            body.extend_from_slice(sid.as_bytes());
            body.push(b'\n');
        }

        debug!(
            "Uploading batch of {} results ({} bytes) to {}, session_id present: {}",
            results.len(),
            body.len(),
            self.config.endpoint,
            session_id.is_some()
        );
        // Log first 2KB and last 200 bytes of request body at trace level for debugging
        let body_str = String::from_utf8_lossy(&body);
        if body.len() > 2200 {
            trace!("Upload request body (first 2KB):\n{}", &body_str[..2048]);
            trace!(
                "Upload request body (last 200 bytes):\n...{}",
                &body_str[body.len().saturating_sub(200)..]
            );
        } else {
            trace!("Upload request body:\n{}", body_str);
        }

        let (body, is_compressed) = if self.should_compress(body.len()) {
            (Self::compress(&body)?, true)
        } else {
            (body, false)
        };

        // RIPE Atlas controller expects application/x-www-form-urlencoded format
        // with User-Agent matching the official httppost tool
        // Responds with "OK\n" for success or "ERR\n" for failure
        let mut request = self
            .client
            .post(&self.config.endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", "httppost for atlas.ripe.net")
            .header("Connection", "close");

        if is_compressed {
            request = request.header("Content-Encoding", "gzip");
        }

        let response = request.body(body).send().await?;

        if response.status().is_success() {
            // Check response body for OK/ERR
            let response_body = response.text().await.unwrap_or_default();
            if response_body.trim() == "OK" {
                debug!("Batch upload successful: {} results", results.len());
                Ok(())
            } else {
                debug!("Batch upload rejected by controller: {:?}", response_body);
                anyhow::bail!("Batch upload rejected: {}", response_body.trim())
            }
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            debug!(
                "Upload rejected: status={}, response_body={:?}",
                status, body
            );
            anyhow::bail!("Batch upload failed: {} - {}", status, body)
        }
    }

    /// Upload with retry logic
    ///
    /// The `lts` parameter is the current "local time sync" value - seconds
    /// since last time sync. The `session_id` is appended to the body as a
    /// footer (per official httppost behavior).
    pub async fn upload_with_retry(
        &self,
        result: &MeasurementResult,
        lts: i64,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut attempts = 0;
        let mut delay = self.config.retry_delay;

        loop {
            attempts += 1;

            match self.upload(result, lts, session_id).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if attempts >= self.config.max_retries {
                        error!("Upload failed after {} attempts: {}", attempts, e);
                        return Err(e);
                    }

                    warn!(
                        "Upload attempt {} failed: {}. Retrying in {:?}...",
                        attempts, e, delay
                    );

                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, self.config.max_retry_delay);
                }
            }
        }
    }

    /// Upload batch with retry logic
    ///
    /// The `lts` parameter is the current "local time sync" value - seconds
    /// since last time sync. The `session_id` is appended to the body as a
    /// footer (per official httppost behavior).
    pub async fn upload_batch_with_retry(
        &self,
        results: &[QueuedResult],
        lts: i64,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut attempts = 0;
        let mut delay = self.config.retry_delay;

        loop {
            attempts += 1;

            match self.upload_batch(results, lts, session_id).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if attempts >= self.config.max_retries {
                        error!("Batch upload failed after {} attempts: {}", attempts, e);
                        return Err(e);
                    }

                    warn!(
                        "Batch upload attempt {} failed: {}. Retrying in {:?}...",
                        attempts, e, delay
                    );

                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, self.config.max_retry_delay);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = UploaderConfig::default();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.compression, CompressionMode::Auto);
        assert_eq!(config.compression_threshold, 1024);
    }

    #[test]
    fn test_compression() {
        let data = b"hello world hello world hello world hello world";
        let compressed = ResultUploader::compress(data).unwrap();
        // Compressed should be smaller (for repetitive data)
        assert!(compressed.len() < data.len());
    }

    #[test]
    fn test_uploader_creation() {
        let config = UploaderConfig {
            endpoint: "http://localhost:8080/results".to_string(),
            ..Default::default()
        };
        let uploader = ResultUploader::new(config);
        assert_eq!(uploader.endpoint(), "http://localhost:8080/results");
        assert!(uploader.has_endpoint());
    }

    #[test]
    fn test_no_endpoint() {
        let config = UploaderConfig::default();
        let uploader = ResultUploader::new(config);
        assert!(!uploader.has_endpoint());
    }
}
