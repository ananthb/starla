//! ICMP Ping execution logic

use super::PingConfig;
use rand::Rng;
use serde::{Deserialize, Serialize};
use starla_network::{
    build_icmpv4_echo_request, build_icmpv6_echo_request, new_icmpv4_socket, new_icmpv6_socket,
    parse_icmpv4_echo_reply, parse_icmpv6_echo_reply,
};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::time::timeout;

/// Individual ping reply result (per packet)
/// Official format: { "rtt": 1.672125 } or { "x": "*" } for timeout
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PingReplyOrTimeout {
    /// Successful reply with RTT
    Reply { rtt: f64 },
    /// Timeout indicator
    Timeout { x: String },
}

/// Ping measurement results - a vector of RTT measurements
///
/// Official format example:
/// ```json
/// { "id":"2004", "fw":5120, "mver": "2.6.4", "lts":35, "time":1768243499,
///   "dst_name":"2001:500:2f::f", "af":6, "dst_addr":"2001:500:2f::f",
///   "src_addr":"...", "proto":"ICMP", "ttl":58, "size":32,
///   "result": [ { "rtt":1.672125 }, { "rtt":1.460567 }, { "rtt":1.419836 } ] }
/// ```
///
/// The result field is just an array of RTT measurements.
/// Statistics (min, max, avg) are NOT sent - they can be calculated from the
/// array.
///
/// This type serializes directly as the array (not wrapped in an object).
pub type PingResults = Vec<PingReplyOrTimeout>;

/// Get statistics from ping results
pub fn ping_stats(results: &PingResults) -> (f64, f64, f64, u32, u32) {
    let rtts: Vec<f64> = results
        .iter()
        .filter_map(|r| match r {
            PingReplyOrTimeout::Reply { rtt } => Some(*rtt),
            PingReplyOrTimeout::Timeout { .. } => None,
        })
        .collect();

    let sent = results.len() as u32;
    let rcvd = rtts.len() as u32;

    if rtts.is_empty() {
        (-1.0, -1.0, -1.0, sent, rcvd)
    } else {
        let min = rtts.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = rtts.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let avg = rtts.iter().sum::<f64>() / rtts.len() as f64;
        (min, max, avg, sent, rcvd)
    }
}

pub async fn execute_ping(config: &PingConfig) -> anyhow::Result<PingResults> {
    let socket = if config.target.is_ipv4() {
        new_icmpv4_socket()?
    } else {
        new_icmpv6_socket()?
    };

    // Set TTL if specified
    if config.ttl > 0 {
        socket.set_ttl(config.ttl as u32)?;
    }

    let mut results = Vec::new();
    let identifier = rand::thread_rng().gen::<u16>();
    let is_dgram = socket.is_dgram();

    // We send to the target address on port 0
    let dest = SocketAddr::new(config.target, 0);

    for seq in 0..config.count {
        let sequence = seq as u16;
        // Default size if too small
        let total_size = if config.size < 8 { 64 } else { config.size };
        let mut buffer = vec![0u8; total_size as usize];

        // Fill payload
        let payload_len = total_size as usize - 8; // Subtract ICMP header size
        let payload = vec![0u8; payload_len];

        let packet_size = if config.target.is_ipv4() {
            build_icmpv4_echo_request(&mut buffer, identifier, sequence, &payload)?
        } else {
            build_icmpv6_echo_request(&mut buffer, identifier, sequence, &payload)?
        };

        let start = Instant::now();
        socket.send_to(&buffer[..packet_size], &dest).await?;

        // Wait for reply
        let mut recv_buf = [0u8; 1500];

        // Using a loop with timeout to filter packets
        // Note: For DGRAM sockets, the kernel filters by identifier for us,
        // so we only need to verify the sequence number.
        let wait_result = timeout(Duration::from_millis(config.timeout_ms), async {
            loop {
                // socket.recv_from returns (size, addr)
                let result = socket.recv_from(&mut recv_buf).await;
                match result {
                    Ok((len, addr)) => {
                        // Verify it's a reply to our packet
                        let is_reply = if config.target.is_ipv4() {
                            parse_icmpv4_echo_reply(&recv_buf[..len])
                                .map(|(id, s)| {
                                    // For DGRAM sockets, kernel handles identifier filtering
                                    // For RAW sockets, we need to verify both
                                    (is_dgram || id == identifier) && s == sequence
                                })
                                .unwrap_or(false)
                        } else {
                            parse_icmpv6_echo_reply(&recv_buf[..len])
                                .map(|(id, s)| (is_dgram || id == identifier) && s == sequence)
                                .unwrap_or(false)
                        };

                        if is_reply {
                            return Ok((len, addr));
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await;

        match wait_result {
            Ok(Ok((_len, _addr))) => {
                let rtt = start.elapsed().as_secs_f64() * 1000.0;
                results.push(PingReplyOrTimeout::Reply { rtt });
            }
            Ok(Err(_e)) => {
                // Socket error - treat as timeout
                results.push(PingReplyOrTimeout::Timeout { x: "*".to_string() });
            }
            Err(_) => {
                // Timeout
                results.push(PingReplyOrTimeout::Timeout { x: "*".to_string() });
            }
        }

        // Wait interval if needed (1 second between pings by default)
        if seq < config.count - 1 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    Ok(results)
}
