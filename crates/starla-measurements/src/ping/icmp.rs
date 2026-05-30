//! ICMP Ping execution logic

use super::PingConfig;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Individual ping reply result (per packet)
/// Official format: { "rtt":1.672125 } or { "x":"*" } for timeout
///
/// RTT uses 6 decimal places to match the C probe's %f format.
#[derive(Debug, Clone, Deserialize)]
pub enum PingReplyOrTimeout {
    Reply { rtt: f64 },
    Timeout,
}

impl Serialize for PingReplyOrTimeout {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            PingReplyOrTimeout::Reply { rtt } => {
                // Format RTT with 6 decimal places to match C probe's %f
                let mut map = serializer.serialize_map(Some(1))?;
                let rtt_str = format!("{:.6}", rtt);
                // Serialize as a raw number (not a string)
                map.serialize_entry(
                    "rtt",
                    &serde_json::value::RawValue::from_string(rtt_str).unwrap(),
                )?;
                map.end()
            }
            PingReplyOrTimeout::Timeout => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("x", "*")?;
                map.end()
            }
        }
    }
}

/// Ping measurement results - a vector of RTT measurements
pub type PingResults = Vec<PingReplyOrTimeout>;

/// Get statistics from ping results
pub fn ping_stats(results: &PingResults) -> (f64, f64, f64, u32, u32) {
    let rtts: Vec<f64> = results
        .iter()
        .filter_map(|r| match r {
            PingReplyOrTimeout::Reply { rtt } => Some(*rtt),
            PingReplyOrTimeout::Timeout => None,
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

#[cfg(unix)]
pub async fn execute_ping(config: &PingConfig) -> anyhow::Result<PingResults> {
    use starla_network::{
        build_icmpv4_echo_request, build_icmpv6_echo_request, new_icmpv4_socket, new_icmpv6_socket,
        parse_icmpv4_echo_reply, parse_icmpv6_echo_reply,
    };
    use std::net::SocketAddr;
    use std::time::Instant;
    use tokio::time::timeout;

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
    let identifier = rand::random::<u16>();
    let is_dgram = socket.is_dgram();

    // We send to the target address on port 0
    let dest = SocketAddr::new(config.target, 0);

    // Pre-allocate send buffer outside the loop
    let total_size = if config.size < 8 { 64 } else { config.size };
    let mut buffer = vec![0u8; total_size as usize];
    let payload_len = total_size as usize - 8; // Subtract ICMP header size
    let payload = vec![0u8; payload_len];

    for seq in 0..config.count {
        let sequence = seq as u16;
        buffer.fill(0);

        let packet_size = if config.target.is_ipv4() {
            build_icmpv4_echo_request(&mut buffer, identifier, sequence, &payload)?
        } else {
            build_icmpv6_echo_request(&mut buffer, identifier, sequence, &payload)?
        };

        let start = Instant::now();
        socket.send_to(&buffer[..packet_size], &dest).await?;

        // Wait for reply
        let mut recv_buf = [0u8; 1500];

        let wait_result = timeout(Duration::from_millis(config.timeout_ms), async {
            loop {
                let result = socket.recv_from(&mut recv_buf).await;
                match result {
                    Ok((len, addr)) => {
                        let is_reply = if config.target.is_ipv4() {
                            parse_icmpv4_echo_reply(&recv_buf[..len])
                                .map(|(id, s)| (is_dgram || id == identifier) && s == sequence)
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
                results.push(PingReplyOrTimeout::Timeout);
            }
            Err(_) => {
                results.push(PingReplyOrTimeout::Timeout);
            }
        }

        // Wait interval if needed (1 second between pings by default)
        if seq < config.count - 1 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    Ok(results)
}

#[cfg(windows)]
pub async fn execute_ping(config: &PingConfig) -> anyhow::Result<PingResults> {
    use starla_network::windows_icmp::{self, IcmpStatus};

    let mut results = Vec::new();
    let payload_len = if config.size < 8 {
        56
    } else {
        (config.size - 8) as usize
    };
    let payload = vec![0u8; payload_len];

    for seq in 0..config.count {
        let reply = windows_icmp::icmp_ping(
            config.target,
            config.ttl,
            config.timeout_ms as u32,
            payload.clone(),
        )
        .await?;

        match reply.status {
            IcmpStatus::Success => {
                results.push(PingReplyOrTimeout::Reply { rtt: reply.rtt_ms });
            }
            _ => {
                results.push(PingReplyOrTimeout::Timeout);
            }
        }

        if seq < config.count - 1 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    Ok(results)
}
