//! DNS resolver logic using hickory-dns

use super::{DnsConfig, DnsProtocol};

use hickory_client::client::{AsyncClient, ClientHandle};
use hickory_client::tcp::TcpClientStream;
use hickory_client::udp::UdpClientStream;
use hickory_proto::iocompat::AsyncIoTokioAsStd;
use hickory_proto::rr::{DNSClass, Name, RecordType};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout;

/// DNS measurement result matching the official C probe format.
///
/// The C probe's result object contains only: rt, size, ID, ANCOUNT, QDCOUNT,
/// NSCOUNT, ARCOUNT. Flags (AA, TC, RD, RA, AD, CD), RCODE, qname, qtype are
/// decoded from `abuf` by the API layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsResult {
    /// Round-trip time in ms
    pub rt: f64,
    /// Response size in bytes
    pub size: usize,
    /// DNS transaction ID
    #[serde(rename = "ID")]
    pub id: u16,
    /// Number of answer records
    #[serde(rename = "ANCOUNT")]
    pub ancount: usize,
    /// Number of question records
    #[serde(rename = "QDCOUNT")]
    pub qdcount: usize,
    /// Number of authority records
    #[serde(rename = "NSCOUNT")]
    pub nscount: usize,
    /// Number of additional records
    #[serde(rename = "ARCOUNT")]
    pub arcount: usize,
    /// Error message if query failed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn execute_dns_query(config: &DnsConfig) -> anyhow::Result<DnsResult> {
    // Parse name and type
    let query_name = expand_random_label(&config.query_name);
    let name = Name::from_str(&query_name)?;
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

    // Estimate response size (wire format)
    let estimated_size = 12 // DNS header
        + response.answers().len() * 50
        + response.name_servers().len() * 50
        + response.additionals().len() * 50;

    Ok(DnsResult {
        rt: rtt,
        size: estimated_size,
        id: response.id(),
        ancount: response.answers().len(),
        qdcount: response.queries().len(),
        nscount: response.name_servers().len(),
        arcount: response.additionals().len(),
        error: None,
    })
}

fn expand_random_label(query_name: &str) -> String {
    if query_name == "." {
        return query_name.to_string();
    }

    query_name
        .split('.')
        .map(|label| {
            if label.contains("$r") {
                label.replace("$r", &random_label())
            } else {
                label.to_string()
            }
        })
        .collect::<Vec<String>>()
        .join(".")
}

fn random_label() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(8)
        .map(char::from)
        .collect::<String>()
        .to_lowercase()
}
