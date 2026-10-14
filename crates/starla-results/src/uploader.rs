//! Result uploader for sending measurements to the controller
//!
//! Sends result data over a transport that implements `UploadTransport`.
//! In production this is an SSH channel; for testing it can be anything.

use super::format::AtlasResult;
use super::host_telemetry::HostTelemetry;
use super::queue::QueuedResult;
use super::system_status;
use super::time_sync::TimeSyncTracker;
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
    /// Host telemetry registry; pending lines are drained per upload batch.
    host_telemetry: Arc<HostTelemetry>,
    /// Marked synced when an accepted upload's Date agrees with our clock.
    time_sync: Arc<TimeSyncTracker>,
    metrics: starla_metrics::MetricsRegistry,
}

impl ResultUploader {
    /// Create a new uploader with the given transport
    pub fn new(
        transport: Box<dyn UploadTransport>,
        config: UploaderConfig,
        metrics: starla_metrics::MetricsRegistry,
        host_telemetry: Arc<HostTelemetry>,
        time_sync: Arc<TimeSyncTracker>,
    ) -> Self {
        Self {
            transport,
            config,
            host_telemetry,
            time_sync,
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
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        // An empty batch is still posted (header, telemetry, footer), as the
        // reference probe's minutely httppost is with an empty data/out.
        if !self.has_endpoint() {
            anyhow::bail!("No upload endpoint configured");
        }

        self.metrics.record_upload_attempt();
        let start_time = std::time::Instant::now();

        // Build POST body
        let mut body = Vec::new();
        body.extend_from_slice(b"P_TO_C_REPORT\n");

        // Host telemetry RESULT lines (buddyinfo, rptaddrs) accumulated since
        // the last upload. Pushed by HostTelemetry's periodic reporters, and
        // put back if this upload fails (below).
        let telemetry = self.host_telemetry.drain().await;
        for line in &telemetry {
            body.extend_from_slice(line.as_bytes());
        }

        for queued in results {
            let atlas_result =
                AtlasResult::from_measurement(queued.result.clone(), None).with_lts(queued.lts);
            body.extend_from_slice(atlas_result.to_result_line().as_bytes());
        }
        if !results.is_empty() {
            body.extend_from_slice(system_status::results_marker().as_bytes());
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

        let path = self.config.endpoint_path.clone();
        let sent = self.post(&path, &body, results.len(), start_time).await;
        if sent.is_err() {
            // The caller requeues this batch's results (`requeue_failed`); its
            // telemetry lines go back too, or a failed upload loses them.
            self.host_telemetry.restore(telemetry).await;
        }
        sent
    }

    /// The controller's 900 s `httppost … &SRC=timesync` line: header and
    /// footer only, so the clock is judged even when nothing else is posted.
    pub async fn post_timesync(&self, session_id: Option<&str>) -> anyhow::Result<()> {
        if !self.has_endpoint() {
            anyhow::bail!("No upload endpoint configured");
        }
        let mut body = b"P_TO_C_REPORT\n".to_vec();
        if let Some(sid) = session_id {
            body.extend_from_slice(format!("\nSESSION_ID {sid}\n").as_bytes());
        }
        let path = format!("{}&SRC=timesync", self.config.endpoint_path);
        self.post(&path, &body, 0, std::time::Instant::now()).await
    }

    /// httppost --set-time 2: compare the answer's Date with our clock. In
    /// step and answered within a second: a time sync (lts restarts from 0).
    /// Otherwise a RESULT 9015 line, uploaded with the next batch, as
    /// httppost's stdout goes to data/new/main. Starla never sets the clock.
    async fn judge_clock(&self, response: &str, rtt: Duration) {
        let Some(server) = response
            .lines()
            .take_while(|l| !l.is_empty())
            .find_map(|l| l.strip_prefix("Date:").or_else(|| l.strip_prefix("date:")))
            .and_then(parse_http_date)
        else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        match clock_verdict(now, server, rtt.as_secs_f64(), SET_TIME_TOLERANCE_SECS) {
            ClockVerdict::InSync => self.time_sync.mark_synced(),
            ClockVerdict::NotInSync => {
                warn!("Clock not in sync with the controller: local {now}, remote {server}");
                self.host_telemetry
                    .push(format!(
                        "RESULT 9015 ongoing {now} httppost not in sync, local {now}, remote \
                         {server}\n{}",
                        system_status::main_marker()
                    ))
                    .await;
            }
            ClockVerdict::SlowAnswer => {
                self.host_telemetry
                    .push(format!(
                        "RESULT 9015 ongoing {now} httppost rtt {} ms\n{}",
                        format_g(rtt.as_secs_f64() * 1000.0),
                        system_status::main_marker()
                    ))
                    .await;
            }
        }
    }

    /// POST one built body over a fresh transport stream and judge the
    /// controller's answer. Any error is the whole batch's failure.
    async fn post(
        &self,
        path: &str,
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
            path,
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
                        self.judge_clock(&response_str, start_time.elapsed()).await;
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
                self.judge_clock(&response_str, start_time.elapsed()).await;
                return Ok(());
            } else {
                anyhow::bail!("Upload failed: {}", status_line);
            }
        }

        anyhow::bail!("Empty response from controller")
    }
}

/// The controller's crontab runs httppost with `--set-time 2`.
const SET_TIME_TOLERANCE_SECS: i64 = 2;

#[derive(Debug, PartialEq)]
enum ClockVerdict {
    InSync,
    NotInSync,
    SlowAnswer,
}

/// httppost's test: out of step if `now` is outside
/// `server ± (tolerance + rtt)`; in step and `rtt <= 1 s` is a sync.
fn clock_verdict(now: i64, server: i64, rtt: f64, tolerance: i64) -> ClockVerdict {
    let (now, server, tol) = (now as f64, server as f64, tolerance as f64);
    if now < server - tol - rtt || now > server + tol + rtt {
        ClockVerdict::NotInSync
    } else if rtt <= 1.0 {
        ClockVerdict::InSync
    } else {
        ClockVerdict::SlowAnswer
    }
}

/// An HTTP Date (`Mon, 12 Oct 2026 20:20:51 GMT`) as unix seconds.
fn parse_http_date(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc2822(value.trim())
        .ok()
        .map(|t| t.timestamp())
}

/// C's `%g` for the positive values httppost prints: six significant
/// digits, trailing zeros dropped.
fn format_g(x: f64) -> String {
    let int_digits = if x >= 1.0 {
        x.log10().floor() as i32 + 1
    } else {
        1
    };
    let decimals = (6 - int_digits).max(0) as usize;
    let s = format!("{x:.decimals$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
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
            Arc::new(TimeSyncTracker::new()),
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
            lts: -1,
        }]
    }

    const REPORT: &str = "RESULT { \"id\": \"9104\", \"time\": 1 }\n";

    /// 2026-10-12: RIPE answered a probe's first-ever address report with 429,
    /// and the line was gone. A failed upload puts its telemetry back.
    #[tokio::test]
    async fn a_rate_limited_upload_keeps_its_telemetry() {
        let (u, telemetry, mut seen) = uploader("HTTP/1.1 429 Too Many Requests\r\n\r\n");
        telemetry.push(REPORT.into()).await;
        let err = u.upload_batch(&one_result(), None).await.unwrap_err();
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
        u.upload_batch(&one_result(), None).await.unwrap();
        assert!(seen.recv().await.unwrap().contains(REPORT));
        assert!(telemetry.drain().await.is_empty());
    }

    #[test]
    fn the_clock_is_judged_as_httppost_judges_it() {
        let t = 1_791_836_451;
        assert_eq!(clock_verdict(t, t + 1, 0.2, 2), ClockVerdict::InSync);
        // Tolerance widens by the round trip.
        assert_eq!(clock_verdict(t, t + 2, 0.6, 2), ClockVerdict::InSync);
        assert_eq!(clock_verdict(t, t + 5, 0.2, 2), ClockVerdict::NotInSync);
        assert_eq!(clock_verdict(t, t - 5, 0.2, 2), ClockVerdict::NotInSync);
        assert_eq!(clock_verdict(t, t, 1.5, 2), ClockVerdict::SlowAnswer);
    }

    #[test]
    fn http_dates_and_g_format() {
        // RIPE's own header, 2026-10-12.
        assert_eq!(
            parse_http_date(" Mon, 12 Oct 2026 20:20:51 GMT"),
            Some(1_791_836_451)
        );
        assert_eq!(parse_http_date("yesterday"), None);
        assert_eq!(format_g(1523.456789), "1523.46");
        assert_eq!(format_g(60000.0), "60000");
        assert_eq!(format_g(2000.5), "2000.5");
    }

    fn reply_dated(offset_secs: i64) -> &'static str {
        let date = (chrono::Utc::now() + chrono::Duration::seconds(offset_secs))
            .format("%a, %d %b %Y %H:%M:%S GMT");
        Box::leak(
            format!(
                "HTTP/1.1 200 \r\nContent-Length: 3\r\nDate: {date}\r\nConnection: \
                 close\r\n\r\nOK\n"
            )
            .into_boxed_str(),
        )
    }

    fn with_time_sync(
        reply: &'static str,
    ) -> (ResultUploader, Arc<HostTelemetry>, Arc<TimeSyncTracker>) {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let telemetry = HostTelemetry::new();
        let sync = Arc::new(TimeSyncTracker::new());
        let mut u = ResultUploader::new(
            Box::new(FakeController { reply, seen: tx }),
            UploaderConfig::default(),
            starla_metrics::MetricsRegistry::new().unwrap(),
            Arc::clone(&telemetry),
            Arc::clone(&sync),
        );
        u.set_endpoint_path("/?PROBE_ID=1".into()).unwrap();
        (u, telemetry, sync)
    }

    #[tokio::test]
    async fn an_accepted_upload_in_step_is_a_time_sync() {
        let (u, telemetry, sync) = with_time_sync(reply_dated(0));
        assert_eq!(sync.lts(), -1);
        u.upload_batch(&[], Some("s")).await.unwrap();
        assert!((0..2).contains(&sync.lts()), "{}", sync.lts());
        assert!(telemetry.drain().await.is_empty());
    }

    #[tokio::test]
    async fn an_answer_an_hour_off_is_reported_not_trusted() {
        let (u, telemetry, sync) = with_time_sync(reply_dated(3600));
        u.upload_batch(&[], Some("s")).await.unwrap();
        assert_eq!(sync.lts(), -1);
        let lines = telemetry.drain().await;
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("RESULT 9015 ongoing ")
                && lines[0].contains(" httppost not in sync, local "),
            "{}",
            lines[0]
        );
        // condmv's marker for data/new/main follows httppost's own line.
        assert!(
            lines[0].contains("\nRESULT 9901 ongoing ")
                && lines[0].ends_with(" /home/atlas/data/new/main\n"),
            "{}",
            lines[0]
        );
    }

    /// The reference sends status lines from its 180 s loop, not with every
    /// upload, and marks eperd's results with condmv's 9907 line.
    #[tokio::test]
    async fn an_upload_carries_results_and_their_marker_only() {
        let (u, _telemetry, mut seen) = uploader("HTTP/1.1 200 \r\n\r\nOK\n");
        u.upload_batch(&one_result(), Some("s")).await.unwrap();
        let req = seen.recv().await.unwrap();
        let body = &req[req.find("\r\n\r\n").unwrap() + 4..];
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines[0], "P_TO_C_REPORT");
        assert!(lines[1].contains("\"1001\""), "{body}");
        assert!(
            lines[2].starts_with("RESULT 9907 ongoing ")
                && lines[2].ends_with(" /home/atlas/data/new/7"),
            "{body}"
        );
        for gone in ["\"7001\"", "\"9018\"", "\"9002\"", "starla"] {
            assert!(!body.contains(gone), "{gone} in {body}");
        }

        u.upload_batch(&[], Some("s")).await.unwrap();
        let req = seen.recv().await.unwrap();
        assert!(!req.contains("9907"), "no results, no marker: {req}");
    }
}
