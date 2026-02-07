//! HTTP client logic

use super::HttpConfig;
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::time::{Duration, Instant};

/// HTTP measurement result in RIPE Atlas format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResult {
    /// Total round-trip time in ms
    pub rt: f64,
    /// HTTP status code
    #[serde(rename = "res")]
    pub status: u16,
    /// HTTP version (e.g., "1.1", "2")
    pub ver: String,
    /// Method used
    pub method: String,
    /// Destination name (URL host)
    pub dst_name: String,
    /// Response header size in bytes
    #[serde(rename = "hsize")]
    pub header_size: usize,
    /// Response body size in bytes
    #[serde(rename = "bsize")]
    pub body_size: usize,
    /// Time to first byte (TTFB) in ms
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttfb: Option<f64>,
    /// Time to connect in ms
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttc: Option<f64>,
    /// Request headers sent
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<Vec<String>>,
    /// Readbuf - partial body content for debugging
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readbuf: Option<String>,
    /// Error message if request failed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
}

pub async fn execute_http_request(config: &HttpConfig) -> anyhow::Result<HttpResult> {
    let mut builder = Client::builder()
        .timeout(Duration::from_millis(config.timeout_ms))
        .danger_accept_invalid_certs(!config.verify_ssl)
        .redirect(reqwest::redirect::Policy::none()); // Usually probes follow redirects manually or not at all

    if let Some(ref v) = config.version {
        if v == "2" {
            builder = builder.http2_prior_knowledge();
        } else if v == "1.1" {
            builder = builder.http1_only();
        }
    }

    let client = builder.build()?;

    let method = Method::from_str(&config.method.to_uppercase()).unwrap_or(Method::GET);

    // Extract host from URL
    let url = reqwest::Url::parse(&config.url)?;
    let dst_name = url.host_str().unwrap_or("unknown").to_string();

    let mut request = client.request(method.clone(), &config.url);

    // Track headers sent
    let mut headers_sent = Vec::new();
    if let Some(ref headers) = config.headers {
        for (k, v) in headers {
            request = request.header(k, v);
            headers_sent.push(format!("{}: {}", k, v));
        }
    }

    if let Some(ref body) = config.body {
        request = request.body(body.clone());
    }

    let start = Instant::now();
    let response = request.send().await?;
    let rtt = start.elapsed().as_secs_f64() * 1000.0;

    let status = response.status().as_u16();
    let version = match response.version() {
        reqwest::Version::HTTP_09 => "0.9",
        reqwest::Version::HTTP_10 => "1.0",
        reqwest::Version::HTTP_11 => "1.1",
        reqwest::Version::HTTP_2 => "2",
        reqwest::Version::HTTP_3 => "3",
        _ => "unknown",
    }
    .to_string();

    // Calculate sizes
    let headers = response.headers();
    let mut header_size = 0;
    for (k, v) in headers {
        header_size += k.as_str().len() + v.as_bytes().len() + 4; // key + ": "
                                                                  // + value + "
                                                                  // \r\n"
    }

    let body = response.bytes().await?;
    let body_size = body.len();

    // Get first 512 bytes of body for readbuf (for debugging)
    let readbuf = if body_size > 0 {
        let truncated = &body[..body_size.min(512)];
        String::from_utf8_lossy(truncated).to_string().into()
    } else {
        None
    };

    Ok(HttpResult {
        rt: rtt,
        status,
        ver: version,
        method: method.to_string(),
        dst_name,
        header_size,
        body_size,
        ttfb: None, // Would need streaming response to measure TTFB accurately
        ttc: None,  // Connection time not easily available in reqwest
        header: if headers_sent.is_empty() {
            None
        } else {
            Some(headers_sent)
        },
        readbuf,
        err: None,
    })
}
