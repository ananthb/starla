//! ICMP Traceroute logic

use super::TracerouteConfig;
use rand::Rng;
use serde::{Deserialize, Serialize};
use starla_network::{
    build_icmpv4_echo_request, build_icmpv6_echo_request, new_icmpv4_socket, new_icmpv6_socket,
    parse_icmpv4_packet, parse_icmpv6_packet, IcmpResponse,
};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::time::timeout;

/// Individual probe result within a hop
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    /// Source address of response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<IpAddr>,
    /// Round-trip time in ms
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt: Option<f64>,
    /// TTL from response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u8>,
    /// Size of response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<usize>,
    /// ICMP type in response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icmptype: Option<u8>,
    /// Timeout indicator
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<String>,
    /// Error message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
}

/// Single hop in traceroute
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracerouteHop {
    /// Hop number (TTL used)
    pub hop: u8,
    /// Results for each probe at this hop
    pub result: Vec<ProbeResult>,
}

/// Full traceroute result in RIPE Atlas format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracerouteResult {
    /// Destination address
    pub dst_addr: IpAddr,
    /// Destination name (usually same as addr for IP targets)
    pub dst_name: String,
    /// Address family (4 or 6)
    pub af: u8,
    /// Protocol used ("ICMP")
    pub proto: String,
    /// Packet size
    pub size: u16,
    /// Paris ID (0 if not using Paris traceroute)
    pub paris_id: u16,
    /// All hops
    pub result: Vec<TracerouteHop>,
}

pub async fn execute_traceroute(config: &TracerouteConfig) -> anyhow::Result<TracerouteResult> {
    let socket = if config.target.is_ipv4() {
        new_icmpv4_socket()?
    } else {
        new_icmpv6_socket()?
    };

    let mut hops = Vec::new();
    let identifier = rand::thread_rng().gen::<u16>();
    let is_dgram = socket.is_dgram();
    let dest = SocketAddr::new(config.target, 0);

    // Default queries per hop is usually 3
    let queries_per_hop = 3;

    let mut _destination_reached = false;

    for ttl in config.first_hop..=config.max_hops {
        let mut hop_results = Vec::new();

        // Set TTL for this hop
        socket.set_ttl(ttl as u32)?;

        let mut got_reply_from_target = false;

        for seq in 0..queries_per_hop {
            let sequence = (ttl as u16)
                .wrapping_mul(queries_per_hop as u16)
                .wrapping_add(seq as u16);

            let total_size = if config.size < 8 { 64 } else { config.size };
            let mut buffer = vec![0u8; total_size as usize];
            let payload_len = total_size as usize - 8;
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
            let wait_result = timeout(Duration::from_millis(config.timeout_ms), async {
                loop {
                    let result = socket.recv_from(&mut recv_buf).await;
                    match result {
                        Ok((len, addr)) => {
                            // Check if this is a relevant packet
                            let response = if config.target.is_ipv4() {
                                parse_icmpv4_packet(&recv_buf[..len])
                            } else {
                                parse_icmpv6_packet(&recv_buf[..len])
                            };

                            match response {
                                Some(IcmpResponse::TimeExceeded(_)) => {
                                    // Intermediate hop - ICMP type 11
                                    return Ok((len, addr, 11u8, false));
                                }
                                Some(IcmpResponse::EchoReply {
                                    identifier: id,
                                    sequence: s,
                                }) => {
                                    // For DGRAM sockets, kernel handles identifier filtering
                                    // For RAW sockets, we need to verify both
                                    if (is_dgram || id == identifier) && s == sequence {
                                        // Destination reached - ICMP type 0
                                        return Ok((len, addr, 0u8, true));
                                    }
                                }
                                Some(IcmpResponse::DestinationUnreachable(_code)) => {
                                    // Destination unreachable - ICMP type 3
                                    return Ok((len, addr, 3u8, true));
                                }
                                _ => continue,
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
            })
            .await;

            match wait_result {
                Ok(Ok((len, addr, icmp_type, is_target))) => {
                    let rtt = start.elapsed().as_secs_f64() * 1000.0;
                    if is_target {
                        got_reply_from_target = true;
                    }

                    hop_results.push(ProbeResult {
                        from: Some(addr.ip()),
                        rtt: Some(rtt),
                        ttl: None, // Would need IP_RECVTTL socket option
                        size: Some(len),
                        icmptype: Some(icmp_type),
                        x: None,
                        err: None,
                    });
                }
                Ok(Err(e)) => {
                    hop_results.push(ProbeResult {
                        from: None,
                        rtt: None,
                        ttl: None,
                        size: None,
                        icmptype: None,
                        x: None,
                        err: Some(format!("Socket error: {}", e)),
                    });
                }
                Err(_) => {
                    // Timeout
                    hop_results.push(ProbeResult {
                        from: None,
                        rtt: None,
                        ttl: None,
                        size: None,
                        icmptype: None,
                        x: Some("*".to_string()),
                        err: None,
                    });
                }
            }
        }

        hops.push(TracerouteHop {
            hop: ttl,
            result: hop_results,
        });

        if got_reply_from_target {
            _destination_reached = true;
            break;
        }
    }

    Ok(TracerouteResult {
        dst_addr: config.target,
        dst_name: config.target.to_string(),
        af: if config.target.is_ipv4() { 4 } else { 6 },
        proto: "ICMP".to_string(),
        size: config.size,
        paris_id: config.paris,
        result: hops,
    })
}
