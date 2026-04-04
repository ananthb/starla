//! TLS certificate checking logic

use super::TlsConfig;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

/// TLS measurement result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsResult {
    /// Total round-trip time (TCP connect + TLS handshake) in ms
    pub rt: f64,
    /// Time to establish TCP connection in ms
    pub tcp_connect_time: f64,
    /// TLS protocol version (e.g., "1.2", "1.3")
    pub ver: String,
    /// Cipher suite as hex (e.g., "0xc030")
    pub cipher: String,
    /// PEM-encoded certificate chain
    pub pem_certs: Vec<String>,
}

/// Convert TLS version to C probe format
fn format_tls_version(version: tokio_rustls::rustls::ProtocolVersion) -> String {
    use tokio_rustls::rustls::ProtocolVersion;
    match version {
        ProtocolVersion::TLSv1_0 => "1.0".to_string(),
        ProtocolVersion::TLSv1_1 => "1.1".to_string(),
        ProtocolVersion::TLSv1_2 => "1.2".to_string(),
        ProtocolVersion::TLSv1_3 => "1.3".to_string(),
        other => format!("{:?}", other),
    }
}

/// Convert cipher suite to hex format matching C probe
fn format_cipher_suite(suite: tokio_rustls::rustls::CipherSuite) -> String {
    // CipherSuite has get_u16() method
    format!("{:#06x}", u16::from(suite))
}

/// Convert DER certificate to PEM format
fn der_to_pem(der: &[u8]) -> String {
    let b64 = BASE64.encode(der);
    // Split into 64-character lines
    let lines: Vec<&str> = b64
        .as_bytes()
        .chunks(64)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or(""))
        .collect();
    format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----",
        lines.join("\n")
    )
}

pub async fn execute_tls_check(config: &TlsConfig) -> anyhow::Result<TlsResult> {
    let total_timeout = Duration::from_millis(config.timeout_ms);

    let root_store = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };

    let client_config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let connector = TlsConnector::from(Arc::new(client_config));

    let dest = SocketAddr::new(config.target, config.port);
    let server_name = ServerName::try_from(config.hostname.as_str())
        .map_err(|_| anyhow::anyhow!("Invalid hostname"))?
        .to_owned();

    // TCP connection with timeout
    let start = Instant::now();
    let stream = timeout(total_timeout, TcpStream::connect(dest))
        .await
        .map_err(|_| anyhow::anyhow!("TCP connection timeout"))??;
    let tcp_connect_time = start.elapsed().as_secs_f64() * 1000.0;

    // Remaining timeout for TLS handshake
    let remaining = total_timeout.saturating_sub(start.elapsed());

    // TLS handshake with remaining timeout
    let stream = timeout(remaining, connector.connect(server_name, stream))
        .await
        .map_err(|_| anyhow::anyhow!("TLS handshake timeout"))??;

    let rt = start.elapsed().as_secs_f64() * 1000.0;

    let (_, connection) = stream.get_ref();

    let version = connection
        .protocol_version()
        .map(format_tls_version)
        .unwrap_or_else(|| "unknown".to_string());

    let cipher = connection
        .negotiated_cipher_suite()
        .map(|c| format_cipher_suite(c.suite()))
        .unwrap_or_else(|| "unknown".to_string());

    // Extract certificate chain as PEM
    let certs = connection
        .peer_certificates()
        .ok_or_else(|| anyhow::anyhow!("No certificates found"))?;

    let pem_certs: Vec<String> = certs.iter().map(|c| der_to_pem(c.as_ref())).collect();

    Ok(TlsResult {
        rt,
        tcp_connect_time,
        ver: version,
        cipher,
        pem_certs,
    })
}
