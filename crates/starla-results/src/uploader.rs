//! Result uploader for sending measurements to the controller
//!
//! Sends result data over a transport that implements `UploadTransport`.
//! In production this is an SSH channel; for testing it can be anything.

use super::format::AtlasResult;
use super::queue::QueuedResult;
use super::system_status;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, trace, warn};

/// An async bidirectional stream for upload transport.
pub trait UploadStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> UploadStream for T {}

/// A transport that opens streams to the controller's result endpoint
/// (127.0.0.1:8080 on the remote side of the SSH tunnel).
pub trait UploadTransport: Send + Sync {
    /// Open a new bidirectional stream to the controller.
    fn open(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Box<dyn UploadStream>>> + Send + '_>>;
}

/// Configuration for the result uploader
#[derive(Debug, Clone)]
pub struct UploaderConfig {
    /// URL path and query for the HTTP POST (e.g.,
    /// "/?PROBE_ID=123&SESSION_ID=abc")
    pub endpoint_path: String,
    /// Request timeout
    pub timeout: Duration,
}

impl Default for UploaderConfig {
    fn default() -> Self {
        Self {
            endpoint_path: String::new(),
            timeout: Duration::from_secs(15),
        }
    }
}

/// Uploads measurement results to the controller
pub struct ResultUploader {
    transport: Box<dyn UploadTransport>,
    config: UploaderConfig,
    /// Process start time (unix timestamp) for uptime reporting
    start_time: u64,
}

impl ResultUploader {
    /// Create a new uploader with the given transport
    pub fn new(transport: Box<dyn UploadTransport>, config: UploaderConfig) -> Self {
        let start_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            transport,
            config,
            start_time,
        }
    }

    /// Set the endpoint path (call after controller connection).
    /// Path must start with '/' (e.g., "/?PROBE_ID=123&SESSION_ID=abc").
    pub fn set_endpoint_path(&mut self, path: String) -> anyhow::Result<()> {
        if !path.starts_with('/') {
            anyhow::bail!("endpoint path must start with '/', got: {}", path);
        }
        if path.contains('\n') || path.contains('\r') {
            anyhow::bail!("endpoint path contains invalid characters");
        }
        self.config.endpoint_path = path;
        Ok(())
    }

    /// Check if an endpoint path is configured
    pub fn has_endpoint(&self) -> bool {
        !self.config.endpoint_path.is_empty()
    }

    /// Upload a batch of results
    ///
    /// Builds the POST body (P_TO_C_REPORT header, RESULT lines, SESSION_ID
    /// footer), writes a raw HTTP/1.1 POST over the transport stream, and
    /// reads the response.
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

        // Build POST body
        let mut body = Vec::new();
        body.extend_from_slice(b"P_TO_C_REPORT\n");

        // System status results (disk, uptime, interfaces, ongoing status)
        // The controller expects these alongside measurement results
        let status = system_status::system_status_lines(lts, self.start_time);
        body.extend_from_slice(status.as_bytes());

        for queued in results {
            let atlas_result =
                AtlasResult::from_measurement(queued.result.clone(), None).with_lts(lts);
            body.extend_from_slice(atlas_result.to_result_line().as_bytes());
        }

        if let Some(sid) = session_id {
            body.push(b'\n');
            body.extend_from_slice(b"SESSION_ID ");
            body.extend_from_slice(sid.as_bytes());
            body.push(b'\n');
        }

        debug!(
            "Uploading batch of {} results ({} bytes)",
            results.len(),
            body.len(),
        );
        trace!("Upload body:\n{}", String::from_utf8_lossy(&body));

        // Open transport stream and send raw HTTP/1.1 POST
        let mut stream = tokio::time::timeout(self.config.timeout, self.transport.open())
            .await
            .map_err(|_| {
                anyhow::anyhow!("Transport open timed out after {:?}", self.config.timeout)
            })??;

        // Write HTTP request
        let request = format!(
            "POST {} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: \
             application/x-www-form-urlencoded\r\nUser-Agent: httppost for \
             atlas.ripe.net\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            self.config.endpoint_path,
            body.len(),
        );

        stream.write_all(request.as_bytes()).await?;
        stream.write_all(&body).await?;
        stream.flush().await?;

        // Read HTTP response
        let mut response = Vec::with_capacity(256);
        let mut buf = [0u8; 1024];
        loop {
            match tokio::time::timeout(self.config.timeout, stream.read(&mut buf)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => response.extend_from_slice(&buf[..n]),
                Ok(Err(e)) => {
                    // Read error after we got some data is OK (connection close)
                    if response.is_empty() {
                        return Err(e.into());
                    }
                    break;
                }
                Err(_) => {
                    if response.is_empty() {
                        anyhow::bail!("Response timed out after {:?}", self.config.timeout);
                    }
                    break;
                }
            }
            // Don't read forever — response should be short
            if response.len() > 4096 {
                break;
            }
        }

        let response_str = String::from_utf8_lossy(&response);
        trace!("Upload response: {:?}", response_str);

        // Parse HTTP status
        if let Some(status_line) = response_str.lines().next() {
            if status_line.contains("429") {
                // Parse Retry-After header if present
                let retry_after = response_str
                    .lines()
                    .find(|l| l.to_lowercase().starts_with("retry-after:"))
                    .and_then(|l| l.split(':').nth(1))
                    .and_then(|v| v.trim().parse::<u64>().ok());
                if let Some(secs) = retry_after {
                    warn!("Rate limited (429), Retry-After: {}s", secs);
                    anyhow::bail!("rate limited {}", secs);
                } else {
                    warn!("Rate limited (429)");
                    anyhow::bail!("rate limited");
                }
            } else if status_line.contains("200") {
                // Check body for OK/ERR
                if let Some(body_start) = response_str.find("\r\n\r\n") {
                    let resp_body = response_str[body_start + 4..].trim();
                    if resp_body == "OK" {
                        debug!("Batch upload successful: {} results", results.len());
                        return Ok(());
                    } else {
                        debug!("Upload rejected: {:?}", resp_body);
                        anyhow::bail!("Upload rejected: {}", resp_body);
                    }
                }
                // No body separator found but status was 200 — treat as success
                debug!(
                    "Batch upload successful (no body): {} results",
                    results.len()
                );
                return Ok(());
            } else {
                anyhow::bail!("Upload failed: {}", status_line);
            }
        }

        anyhow::bail!("Empty response from controller")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = UploaderConfig::default();
        assert!(config.endpoint_path.is_empty());
    }
}
