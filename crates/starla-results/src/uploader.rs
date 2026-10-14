//! Result uploader for sending measurements to the controller
//!
//! Sends result data over a transport that implements `UploadTransport`.
//! In production this is an SSH channel; for testing it can be anything.

use super::format::AtlasResult;
use super::host_telemetry::HostTelemetry;
use super::queue::QueuedResult;
use super::system_status;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
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
    /// Host telemetry registry; pending lines are drained per upload batch.
    host_telemetry: Arc<HostTelemetry>,
    metrics: starla_metrics::MetricsRegistry,
}

impl ResultUploader {
    /// Create a new uploader with the given transport
    pub fn new(
        transport: Box<dyn UploadTransport>,
        config: UploaderConfig,
        metrics: starla_metrics::MetricsRegistry,
        host_telemetry: Arc<HostTelemetry>,
    ) -> Self {
        let start_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            transport,
            config,
            start_time,
            host_telemetry,
            metrics,
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

        self.metrics.record_upload_attempt();
        let start_time = std::time::Instant::now();

        // Build POST body
        let mut body = Vec::new();
        body.extend_from_slice(b"P_TO_C_REPORT\n");

        // System status results (disk, uptime, interfaces, ongoing status)
        // The controller expects these alongside measurement results
        let status = system_status::system_status_lines(lts, self.start_time);
        body.extend_from_slice(status.as_bytes());

        // Host telemetry RESULT lines (buddyinfo, rptaddrs) accumulated since
        // the last upload. Pushed by HostTelemetry's periodic reporters, and
        // put back if this upload fails (below).
        let telemetry = self.host_telemetry.drain().await;
        for line in &telemetry {
            body.extend_from_slice(line.as_bytes());
        }

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

        let sent = self.post(&body, results.len(), start_time).await;
        if sent.is_err() {
            // The caller requeues this batch's results (`requeue_failed`); its
            // telemetry lines go back too, or a failed upload loses them.
            self.host_telemetry.restore(telemetry).await;
        }
        sent
    }

    /// POST one built body over a fresh transport stream and judge the
    /// controller's answer. Any error is the whole batch's failure.
    async fn post(
        &self,
        body: &[u8],
        count: usize,
        start_time: std::time::Instant,
    ) -> anyhow::Result<()> {
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
        stream.write_all(body).await?;
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
            // Don't read forever: response should be short
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
                        debug!("Batch upload successful: {} results", count);
                        self.metrics
                            .record_upload_duration(start_time.elapsed().as_secs_f64());
                        return Ok(());
                    } else {
                        debug!("Upload rejected: {:?}", resp_body);
                        anyhow::bail!("Upload rejected: {}", resp_body);
                    }
                }
                // No body separator found but status was 200: treat as success
                debug!("Batch upload successful (no body): {} results", count);
                self.metrics
                    .record_upload_duration(start_time.elapsed().as_secs_f64());
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
    use starla_common::{
        MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId, Timestamp,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn test_default_config() {
        let config = UploaderConfig::default();
        assert!(config.endpoint_path.is_empty());
    }

    /// A controller that answers every POST with `reply`, and hands back each
    /// request it read.
    struct FakeController {
        reply: &'static str,
        seen: tokio::sync::mpsc::UnboundedSender<String>,
    }

    impl UploadTransport for FakeController {
        fn open(
            &self,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<Box<dyn UploadStream>>> + Send + '_>>
        {
            let (probe, mut ctrl) = tokio::io::duplex(1 << 20);
            let reply = self.reply;
            let seen = self.seen.clone();
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
                let _ = seen.send(String::from_utf8_lossy(&req).into_owned());
                let _ = ctrl.write_all(reply.as_bytes()).await;
                let _ = ctrl.shutdown().await;
            });
            Box::pin(async move { Ok(Box::new(probe) as Box<dyn UploadStream>) })
        }
    }

    fn uploader(
        reply: &'static str,
    ) -> (
        ResultUploader,
        Arc<HostTelemetry>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let telemetry = HostTelemetry::new();
        let mut u = ResultUploader::new(
            Box::new(FakeController { reply, seen: tx }),
            UploaderConfig::default(),
            starla_metrics::MetricsRegistry::new().unwrap(),
            Arc::clone(&telemetry),
        );
        u.set_endpoint_path("/?PROBE_ID=1".into()).unwrap();
        (u, telemetry, rx)
    }

    fn one_result() -> Vec<QueuedResult> {
        vec![QueuedResult {
            result: MeasurementResult {
                fw: 5120,
                measurement_type: MeasurementType::Ping,
                prb_id: ProbeId(1),
                msm_id: MeasurementId(1001),
                timestamp: Timestamp::now(),
                af: 4,
                dst_addr: "193.0.14.129".parse().unwrap(),
                dst_name: None,
                src_addr: None,
                proto: Some("ICMP".to_string()),
                ttl: Some(64),
                size: Some(32),
                data: MeasurementData::Generic(serde_json::json!([{"rtt": 4.2}])),
            },
            queued_at: 0,
            attempts: 0,
            last_attempt_at: None,
        }]
    }

    const REPORT: &str = "RESULT { \"id\": \"9104\", \"time\": 1 }\n";

    /// 2026-10-12: RIPE answered a probe's first-ever address report with 429,
    /// and the line was gone. A failed upload puts its telemetry back.
    #[tokio::test]
    async fn a_rate_limited_upload_keeps_its_telemetry() {
        let (u, telemetry, mut seen) = uploader("HTTP/1.1 429 Too Many Requests\r\n\r\n");
        telemetry.push(REPORT.into()).await;
        let err = u.upload_batch(&one_result(), 0, None).await.unwrap_err();
        assert!(err.to_string().contains("rate limited"), "{err}");
        assert!(
            seen.recv().await.unwrap().contains(REPORT),
            "the report was in the request"
        );
        assert_eq!(telemetry.drain().await, [REPORT], "and is pending again");
    }

    #[tokio::test]
    async fn an_accepted_upload_consumes_its_telemetry() {
        let (u, telemetry, mut seen) = uploader("HTTP/1.1 200 OK\r\n\r\nOK");
        telemetry.push(REPORT.into()).await;
        u.upload_batch(&one_result(), 0, None).await.unwrap();
        assert!(seen.recv().await.unwrap().contains(REPORT));
        assert!(telemetry.drain().await.is_empty());
    }
}
