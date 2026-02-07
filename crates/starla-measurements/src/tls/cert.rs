//! TLS certificate checking logic

use super::TlsConfig;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::rustls::{pki_types::ServerName, ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use x509_parser::prelude::*;

/// TLS measurement result in RIPE Atlas format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsResult {
    /// Round-trip time (TCP connect + TLS handshake) in ms
    pub rt: f64,
    /// Time to establish TCP connection in ms
    #[serde(rename = "ttc")]
    pub tcp_connect_time: f64,
    /// TLS handshake time in ms
    #[serde(rename = "tth")]
    pub tls_handshake_time: f64,
    /// TLS protocol version (e.g., "TLS1.3")
    pub ver: String,
    /// Cipher suite
    pub cipher: String,
    /// Certificate chain
    pub cert: Vec<CertInfo>,
    /// Alert info (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert: Option<String>,
}

/// Certificate information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertInfo {
    /// Certificate subject
    pub subject: String,
    /// Certificate issuer
    pub issuer: String,
    /// Not valid before (Unix timestamp)
    #[serde(rename = "notBefore")]
    pub not_before: i64,
    /// Not valid after (Unix timestamp)
    #[serde(rename = "notAfter")]
    pub not_after: i64,
    /// Serial number (hex encoded)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    /// SHA256 fingerprint
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256fp: Option<String>,
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
    let start_handshake = Instant::now();
    let stream = timeout(remaining, connector.connect(server_name, stream))
        .await
        .map_err(|_| anyhow::anyhow!("TLS handshake timeout"))??;
    let tls_handshake_time = start_handshake.elapsed().as_secs_f64() * 1000.0;

    let (_, connection) = stream.get_ref();

    let version = connection
        .protocol_version()
        .map(|v| format!("{:?}", v))
        .unwrap_or_else(|| "unknown".to_string());
    let cipher = connection
        .negotiated_cipher_suite()
        .map(|c| format!("{:?}", c.suite()))
        .unwrap_or_else(|| "unknown".to_string());

    // Extract certificate chain
    let certs = connection
        .peer_certificates()
        .ok_or_else(|| anyhow::anyhow!("No certificates found"))?;

    let mut cert_chain = Vec::new();
    for cert_der in certs {
        if let Ok((_, cert)) = X509Certificate::from_der(cert_der.as_ref()) {
            // Calculate SHA256 fingerprint
            let sha256fp = {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(cert_der.as_ref());
                let result = hasher.finalize();
                Some(hex::encode(result))
            };

            cert_chain.push(CertInfo {
                subject: cert.subject().to_string(),
                issuer: cert.issuer().to_string(),
                not_before: cert.validity().not_before.timestamp(),
                not_after: cert.validity().not_after.timestamp(),
                serial: Some(cert.serial.to_string()),
                sha256fp,
            });
        }
    }

    Ok(TlsResult {
        rt: tcp_connect_time + tls_handshake_time,
        tcp_connect_time,
        tls_handshake_time,
        ver: version,
        cipher,
        cert: cert_chain,
        alert: None,
    })
}
