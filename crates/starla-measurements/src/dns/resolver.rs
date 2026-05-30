//! DNS resolver using raw sockets for wire-format capture
//!
//! Sends DNS queries via raw UDP/TCP sockets and captures the exact
//! response bytes for the `abuf` field, matching the official C probe.

use super::{DnsConfig, DnsProtocol};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::{DNSClass, Name, RecordType};
use hickory_proto::serialize::binary::BinDecodable;
use rand::{distr::Alphanumeric, RngExt};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout;

/// DNS measurement result matching the official C probe format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsResult {
    /// Round-trip time in ms
    pub rt: f64,
    /// Response size in bytes (actual wire size)
    pub size: usize,
    /// Base64-encoded raw DNS response (wire format)
    pub abuf: String,
    /// DNS transaction ID
    #[serde(rename = "ID")]
    pub id: u16,
    /// Number of answer records
    #[serde(rename = "ANCOUNT")]
    pub ancount: u16,
    /// Number of question records
    #[serde(rename = "QDCOUNT")]
    pub qdcount: u16,
    /// Number of authority records
    #[serde(rename = "NSCOUNT")]
    pub nscount: u16,
    /// Number of additional records
    #[serde(rename = "ARCOUNT")]
    pub arcount: u16,
}

/// Build a DNS query message
fn build_query(config: &DnsConfig, probe_id: u32) -> anyhow::Result<(Message, Vec<u8>)> {
    let query_name = expand_query_templates(&config.query_name, probe_id);
    let name = Name::from_str(&query_name)?;
    let record_type = RecordType::from_str(&config.query_type).unwrap_or(RecordType::A);
    let dns_class = DNSClass::from_str(&config.query_class).unwrap_or(DNSClass::IN);

    let mut msg = Message::new();
    msg.set_id(rand::random());
    msg.set_message_type(MessageType::Query);
    msg.set_op_code(OpCode::Query);
    msg.set_recursion_desired(config.recursion_desired);
    msg.add_query(
        Query::query(name, record_type)
            .set_query_class(dns_class)
            .clone(),
    );

    if config.dnssec || config.edns_buf_size.is_some() {
        let buf_size = config.edns_buf_size.unwrap_or(4096);
        let edns = msg.extensions_mut().get_or_insert_with(Default::default);
        edns.set_max_payload(buf_size);
        if config.dnssec {
            edns.set_dnssec_ok(true);
        }
    }

    let wire = msg.to_vec()?;
    Ok((msg, wire))
}

/// Parse DNS response header fields from raw bytes
pub fn parse_response(raw: &[u8]) -> anyhow::Result<(u16, u16, u16, u16, u16)> {
    // DNS header is 12 bytes: ID(2) + flags(2) + QDCOUNT(2) + ANCOUNT(2) +
    // NSCOUNT(2) + ARCOUNT(2)
    if raw.len() < 12 {
        anyhow::bail!("DNS response too short: {} bytes", raw.len());
    }
    let id = u16::from_be_bytes([raw[0], raw[1]]);
    let qdcount = u16::from_be_bytes([raw[4], raw[5]]);
    let ancount = u16::from_be_bytes([raw[6], raw[7]]);
    let nscount = u16::from_be_bytes([raw[8], raw[9]]);
    let arcount = u16::from_be_bytes([raw[10], raw[11]]);
    Ok((id, qdcount, ancount, nscount, arcount))
}

/// Parse DNS answer records from raw response for the `answers` array
fn parse_answers(raw: &[u8]) -> Option<Vec<AnswerRecord>> {
    let msg = Message::from_bytes(raw).ok()?;
    let answers: Vec<AnswerRecord> = msg
        .answers()
        .iter()
        .map(|rr| AnswerRecord {
            record_type: format!("{}", rr.record_type()),
            name: rr.name().to_string(),
            rdata: format_rdata(rr),
        })
        .collect();
    if answers.is_empty() {
        None
    } else {
        Some(answers)
    }
}

#[derive(Debug, Clone)]
pub struct AnswerRecord {
    pub record_type: String,
    pub name: String,
    pub rdata: Vec<String>,
}

fn format_rdata(rr: &hickory_proto::rr::Record) -> Vec<String> {
    let Some(data) = rr.data() else {
        return vec![];
    };
    match data {
        hickory_proto::rr::RData::TXT(txt) => txt
            .iter()
            .map(|s| String::from_utf8_lossy(s).to_string())
            .collect(),
        hickory_proto::rr::RData::A(a) => vec![a.to_string()],
        hickory_proto::rr::RData::AAAA(aaaa) => vec![aaaa.to_string()],
        hickory_proto::rr::RData::CNAME(name) => vec![name.to_string()],
        hickory_proto::rr::RData::MX(mx) => {
            vec![format!("{} {}", mx.preference(), mx.exchange())]
        }
        hickory_proto::rr::RData::NS(name) => vec![name.to_string()],
        hickory_proto::rr::RData::SOA(soa) => vec![format!(
            "{} {} {} {} {} {} {}",
            soa.mname(),
            soa.rname(),
            soa.serial(),
            soa.refresh(),
            soa.retry(),
            soa.expire(),
            soa.minimum()
        )],
        other => vec![format!("{:?}", other)],
    }
}

/// Execute a DNS query via UDP, capturing raw wire bytes
async fn query_udp(
    dest: SocketAddr,
    wire_query: &[u8],
    timeout_dur: Duration,
) -> anyhow::Result<(Vec<u8>, Duration)> {
    let socket = UdpSocket::bind(if dest.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .await?;

    let start = Instant::now();
    socket.send_to(wire_query, dest).await?;

    let mut buf = vec![0u8; 65535];
    let n = timeout(timeout_dur, socket.recv(&mut buf)).await??;
    let rtt = start.elapsed();

    buf.truncate(n);
    Ok((buf, rtt))
}

/// Execute a DNS query via TCP, capturing raw wire bytes
async fn query_tcp(
    dest: SocketAddr,
    wire_query: &[u8],
    timeout_dur: Duration,
) -> anyhow::Result<(Vec<u8>, Duration)> {
    let start = Instant::now();
    let mut stream = timeout(timeout_dur, TcpStream::connect(dest)).await??;

    // TCP DNS: 2-byte length prefix
    let len = (wire_query.len() as u16).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(wire_query).await?;

    // Read response: 2-byte length prefix then data
    let mut len_buf = [0u8; 2];
    timeout(timeout_dur, stream.read_exact(&mut len_buf)).await??;
    let resp_len = u16::from_be_bytes(len_buf) as usize;

    let mut buf = vec![0u8; resp_len];
    timeout(timeout_dur, stream.read_exact(&mut buf)).await??;
    let rtt = start.elapsed();

    Ok((buf, rtt))
}

pub async fn execute_dns_query(config: &DnsConfig, probe_id: u32) -> anyhow::Result<DnsResult> {
    let (_msg, wire_query) = build_query(config, probe_id)?;
    let dest = SocketAddr::new(config.target, 53);
    let timeout_dur = Duration::from_millis(config.timeout_ms);

    let (raw_response, rtt) = match config.protocol {
        DnsProtocol::UDP => query_udp(dest, &wire_query, timeout_dur).await?,
        DnsProtocol::TCP => query_tcp(dest, &wire_query, timeout_dur).await?,
    };

    let rtt_ms = rtt.as_secs_f64() * 1000.0;
    let size = raw_response.len();
    let abuf = BASE64.encode(&raw_response);
    let (id, qdcount, ancount, nscount, arcount) = parse_response(&raw_response)?;

    Ok(DnsResult {
        rt: rtt_ms,
        size,
        abuf,
        id,
        ancount,
        qdcount,
        nscount,
        arcount,
    })
}

/// Get parsed answer records from a DnsResult's abuf
pub fn decode_answers(abuf: &str) -> Option<Vec<AnswerRecord>> {
    let raw = BASE64.decode(abuf).ok()?;
    parse_answers(&raw)
}

/// Expand RIPE Atlas DNS query name templates:
/// - `$r`: random 8-char alphanumeric (prevents DNS caching)
/// - `$p`: probe ID
/// - `$t`: current Unix timestamp
fn expand_query_templates(query_name: &str, probe_id: u32) -> String {
    if query_name == "." {
        return query_name.to_string();
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    let probe_str = probe_id.to_string();

    query_name
        .split('.')
        .map(|label| {
            let mut s = label.to_string();
            if s.contains("$r") {
                s = s.replace("$r", &random_label());
            }
            if s.contains("$p") {
                s = s.replace("$p", &probe_str);
            }
            if s.contains("$t") {
                s = s.replace("$t", &timestamp);
            }
            s
        })
        .collect::<Vec<String>>()
        .join(".")
}

fn random_label() -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(8)
        .map(char::from)
        .collect::<String>()
        .to_lowercase()
}
