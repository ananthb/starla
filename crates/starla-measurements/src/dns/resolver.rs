//! DNS resolver logic using hickory-dns

use super::{DnsConfig, DnsProtocol};

use hickory_client::client::{AsyncClient, ClientHandle};
use hickory_client::tcp::TcpClientStream;
use hickory_client::udp::UdpClientStream;
use hickory_proto::iocompat::AsyncIoTokioAsStd;
use hickory_proto::rr::{DNSClass, Name, RecordType};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout;

/// DNS measurement result in RIPE Atlas format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsResult {
    /// Round-trip time in ms
    pub rt: f64,
    /// Number of answers
    #[serde(rename = "ancount")]
    pub answer_count: usize,
    /// Number of authority records
    #[serde(rename = "nscount")]
    pub authority_count: usize,
    /// Number of additional records
    #[serde(rename = "arcount")]
    pub additional_count: usize,
    /// Response code (NOERROR=0, NXDOMAIN=3, etc.)
    #[serde(rename = "RCODE")]
    pub rcode: u16,
    /// Response size in bytes (estimated)
    pub size: usize,
    /// Query type
    #[serde(rename = "qtype")]
    pub query_type: String,
    /// Query name
    #[serde(rename = "qname")]
    pub query_name: String,
    /// Authoritative answer flag
    #[serde(rename = "AA")]
    pub authoritative: bool,
    /// Truncated flag
    #[serde(rename = "TC")]
    pub truncated: bool,
    /// Recursion desired flag
    #[serde(rename = "RD")]
    pub recursion_desired: bool,
    /// Recursion available flag
    #[serde(rename = "RA")]
    pub recursion_available: bool,
    /// Authenticated data flag
    #[serde(rename = "AD")]
    pub authenticated_data: bool,
    /// Checking disabled flag
    #[serde(rename = "CD")]
    pub checking_disabled: bool,
    /// Base64 encoded wire format response (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abuf: Option<String>,
    /// Answers in human-readable format
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<DnsAnswer>,
    /// Error message if query failed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Individual DNS answer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsAnswer {
    /// Record name
    pub name: String,
    /// Record type
    #[serde(rename = "type")]
    pub rtype: String,
    /// Record class
    pub class: String,
    /// TTL
    pub ttl: u32,
    /// Record data
    pub rdata: String,
}

pub async fn execute_dns_query(config: &DnsConfig) -> anyhow::Result<DnsResult> {
    // Parse name and type
    let name = Name::from_str(&config.query_name)?;
    let record_type = RecordType::from_str(&config.query_type).unwrap_or(RecordType::A);
    let dns_class = DNSClass::from_str(&config.query_class).unwrap_or(DNSClass::IN);

    let dest = SocketAddr::new(config.target, 53);

    let start = Instant::now();

    // Execute query based on protocol
    // Use configurable timeout for the DNS client (not just the query wrapper)
    let query_timeout = Duration::from_millis(config.timeout_ms);

    let response = match config.protocol {
        DnsProtocol::UDP => {
            // Use with_timeout to set the internal hickory-dns timeout
            // Default is 5 seconds which is often too short for DNS measurements
            let stream = UdpClientStream::<UdpSocket>::with_timeout(dest, query_timeout);
            let (mut client, bg) = AsyncClient::connect(stream).await?;
            tokio::spawn(bg);

            timeout(
                query_timeout,
                client.query(name.clone(), dns_class, record_type),
            )
            .await??
        }
        DnsProtocol::TCP => {
            // TCP also needs timeout configuration via TcpClientStream
            let (stream, sender) =
                TcpClientStream::<AsyncIoTokioAsStd<TcpStream>>::with_timeout(dest, query_timeout);
            let (mut client, bg) = AsyncClient::new(stream, sender, None).await?;
            tokio::spawn(bg);

            timeout(
                query_timeout,
                client.query(name.clone(), dns_class, record_type),
            )
            .await??
        }
    };

    let rtt = start.elapsed().as_secs_f64() * 1000.0;

    // Extract answers in readable format
    let answers: Vec<DnsAnswer> = response
        .answers()
        .iter()
        .map(|record| DnsAnswer {
            name: record.name().to_string(),
            rtype: record.record_type().to_string(),
            class: record.dns_class().to_string(),
            ttl: record.ttl(),
            rdata: record.data().map(|d| format!("{}", d)).unwrap_or_default(),
        })
        .collect();

    // Estimate response size based on record count
    // Real wire format size would need raw bytes access
    let estimated_size = 12 // DNS header
        + response.answers().len() * 50  // Average answer size
        + response.name_servers().len() * 50
        + response.additionals().len() * 50;

    Ok(DnsResult {
        rt: rtt,
        answer_count: response.answers().len(),
        authority_count: response.name_servers().len(),
        additional_count: response.additionals().len(),
        rcode: response.response_code().low() as u16,
        size: estimated_size,
        query_type: config.query_type.clone(),
        query_name: config.query_name.clone(),
        authoritative: response.authoritative(),
        truncated: response.truncated(),
        recursion_desired: response.recursion_desired(),
        recursion_available: response.recursion_available(),
        authenticated_data: response.authentic_data(),
        checking_disabled: response.checking_disabled(),
        abuf: None, // Would need raw wire bytes for base64 encoding
        answers,
        error: None,
    })
}
