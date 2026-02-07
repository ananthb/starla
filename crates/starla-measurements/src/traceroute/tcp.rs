//! TCP Traceroute logic

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
use tokio::net::TcpStream;
use tokio::time::timeout;

pub async fn execute_traceroute(config: &TracerouteConfig) -> anyhow::Result<TracerouteResult> {
    let icmp_socket = if config.target.is_ipv4() {
        new_icmpv4_socket()?
    } else {
        new_icmpv6_socket()?
    };

    let mut hops = Vec::new();
    let queries_per_hop = 3;

    // Default TCP port if not specified (traceroute usually uses 80)
    let dest_port = 80;
    let dest = SocketAddr::new(config.target, dest_port);

    for ttl in config.first_hop..=config.max_hops {
        let mut hop_results = Vec::new();
        let mut got_reply_from_target = false;

        for _seq in 0..queries_per_hop {
            // Create TCP socket
            let domain = if config.target.is_ipv4() {
                Domain::IPV4
            } else {
                Domain::IPV6
            };
            let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;

            // Set TTL
            if config.target.is_ipv4() {
                socket.set_ttl(ttl as u32)?;
            } else {
                socket.set_unicast_hops_v6(ttl as u32)?;
            }

            socket.set_nonblocking(true)?;

            // Start connection attempt
            let _connect_result = socket.connect(&dest.into());
            let start = Instant::now();

            // Convert to tokio TcpStream
            let tcp_socket = TcpStream::from_std(socket.into())?;

            let wait_result = timeout(Duration::from_millis(config.timeout_ms), async {
                tokio::select! {
                    // Case 1: TCP connection established or failed
                    res = tcp_socket.writable() => {
                        match res {
                            Ok(_) => {
                                // Check for actual error (SO_ERROR)
                                match tcp_socket.take_error() {
                                    Ok(Some(e)) => {
                                        // Connection refused (RST) usually means target reached but port closed
                                        if e.kind() == std::io::ErrorKind::ConnectionRefused {
                                            // TCP RST - no ICMP type, return 0
                                            Ok::<_, anyhow::Error>((0usize, dest.ip(), None, true))
                                        } else {
                                            // Other error
                                            Err(e.into())
                                        }
                                    },
                                    Ok(None) => {
                                        // Success (SYN-ACK received)
                                        Ok((0usize, dest.ip(), None, true))
                                    },
                                    Err(e) => Err(e.into()),
                                }
                            },
                            Err(e) => Err(e.into()),
                        }
                    },

                    // Case 2: ICMP error received
                    res = async {
                        let mut recv_buf = [0u8; 1500];
                        loop {
                            let (len, addr) = icmp_socket.recv_from(&mut recv_buf).await?;
                            let response = if config.target.is_ipv4() {
                                parse_icmpv4_packet(&recv_buf[..len])
                            } else {
                                parse_icmpv6_packet(&recv_buf[..len])
                            };

                            match response {
                                Some(IcmpResponse::TimeExceeded(_)) => {
                                    // Intermediate hop - ICMP type 11
                                    return Ok::<_, anyhow::Error>((len, addr.ip(), Some(11u8), false));
                                },
                                Some(IcmpResponse::DestinationUnreachable(_)) => {
                                    // Target reached (or blocked) - ICMP type 3
                                    return Ok((len, addr.ip(), Some(3u8), true));
                                },
                                _ => continue,
                            }
                        }
                    } => res
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
                        from: Some(addr),
                        rtt: Some(rtt),
                        ttl: None,
                        size: if len > 0 { Some(len) } else { None },
                        icmptype: icmp_type,
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
                        err: Some(format!("Error: {}", e)),
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
        proto: "TCP".to_string(),
        size: config.size,
        paris_id: config.paris,
        result: hops,
    })
}
