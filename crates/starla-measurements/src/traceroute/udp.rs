//! UDP Traceroute logic

use super::{
    icmp::{ProbeResult, TracerouteHop, TracerouteResult},
    TracerouteConfig,
};
use socket2::{Domain, Protocol, Socket, Type};
use starla_network::{
    new_icmpv4_socket, new_icmpv6_socket, parse_icmpv4_packet, parse_icmpv6_packet, IcmpResponse,
};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::time::timeout;

pub async fn execute_traceroute(config: &TracerouteConfig) -> anyhow::Result<TracerouteResult> {
    // We need two sockets:
    // 1. UDP socket for sending probes
    // 2. Raw ICMP socket for receiving errors

    let icmp_socket = if config.target.is_ipv4() {
        new_icmpv4_socket()?
    } else {
        new_icmpv6_socket()?
    };

    let mut hops = Vec::new();
    let queries_per_hop = 3;

    // Base port for traceroute (classic unix traceroute uses 33434)
    let base_port = 33434;

    for ttl in config.first_hop..=config.max_hops {
        let mut hop_results = Vec::new();
        let mut got_reply_from_target = false;

        for seq in 0..queries_per_hop {
            // Calculate destination port: base + (hop-1)*queries + seq
            // This ensures each probe has a unique destination port
            let dest_port =
                base_port + ((ttl - 1) as u16) * (queries_per_hop as u16) + (seq as u16);
            let dest = SocketAddr::new(config.target, dest_port);

            // Create UDP socket for this probe to set TTL easily
            let domain = if config.target.is_ipv4() {
                Domain::IPV4
            } else {
                Domain::IPV6
            };
            let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;

            if config.target.is_ipv4() {
                socket.set_ttl(ttl as u32)?;
            } else {
                socket.set_unicast_hops_v6(ttl as u32)?;
            }

            // Must set non-blocking before converting to tokio socket
            socket.set_nonblocking(true)?;

            let udp_socket = UdpSocket::from_std(socket.into())?;

            // Send empty payload (or minimal)
            let payload = [0u8; 0];

            let start = Instant::now();
            udp_socket.send_to(&payload, dest).await?;

            // Wait for reply on ICMP socket
            let mut recv_buf = [0u8; 1500];
            let wait_result = timeout(Duration::from_millis(config.timeout_ms), async {
                loop {
                    let result = icmp_socket.recv_from(&mut recv_buf).await;
                    match result {
                        Ok((len, addr)) => {
                            let response = if config.target.is_ipv4() {
                                parse_icmpv4_packet(&recv_buf[..len])
                            } else {
                                parse_icmpv6_packet(&recv_buf[..len])
                            };

                            match response {
                                Some(IcmpResponse::TimeExceeded(_payload)) => {
                                    // ICMP type 11
                                    return Ok((len, addr, 11u8, false));
                                }
                                Some(IcmpResponse::DestinationUnreachable(_code)) => {
                                    // ICMP type 3 - Port Unreachable means we hit the target
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
                        ttl: None,
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
            break;
        }
    }

    Ok(TracerouteResult {
        dst_addr: config.target,
        dst_name: config.target.to_string(),
        af: if config.target.is_ipv4() { 4 } else { 6 },
        proto: "UDP".to_string(),
        size: config.size,
        paris_id: config.paris,
        result: hops,
    })
}
