//! Simple ICMP ping example demonstrating the network primitives
//!
//! This example sends an ICMPv4 echo request to a target host and waits for a
//! reply.
//!
//! Usage: cargo run --example simple_ping
//!
//! Note: This uses DGRAM ICMP sockets when available (Linux with
//! ping_group_range), which don't require CAP_NET_RAW. Falls back to RAW
//! sockets if needed.

use starla_network::{
    build_icmpv4_echo_request, new_icmpv4_socket, parse_icmpv4_packet, IcmpResponse,
};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Target host (8.8.8.8 - Google DNS)
    let target = IpAddr::V4("8.8.8.8".parse()?);
    let target_addr = SocketAddr::new(target, 0);

    println!("Sending ICMP echo request to {}", target);

    // Create ICMP socket (tries DGRAM first, falls back to RAW)
    let socket = new_icmpv4_socket()?;

    if socket.is_dgram() {
        println!("✓ Using unprivileged DGRAM ICMP socket");
    } else {
        println!("✓ Using privileged RAW ICMP socket");
    }

    // Build ICMP echo request packet
    let mut buffer = [0u8; 64];
    let identifier = std::process::id() as u16;
    let sequence = 1;
    let payload = b"Starla Probe";

    let packet_size = build_icmpv4_echo_request(&mut buffer, identifier, sequence, payload)?;

    println!(
        "Sending {} bytes (ID: {}, Seq: {})",
        packet_size, identifier, sequence
    );

    // Send the packet
    let start = Instant::now();
    socket.send_to(&buffer[..packet_size], &target_addr).await?;

    // Wait for reply (with timeout)
    let mut recv_buffer = [0u8; 1024];
    let timeout = Duration::from_secs(5);

    loop {
        if start.elapsed() > timeout {
            println!("✗ Timeout waiting for reply");
            return Ok(());
        }

        // Try to receive a packet
        match tokio::time::timeout(
            Duration::from_millis(100),
            socket.recv_from(&mut recv_buffer),
        )
        .await
        {
            Ok(Ok((n, from))) => {
                let elapsed = start.elapsed();

                // For DGRAM sockets, data starts directly with ICMP packet
                // For RAW sockets, we need to skip the IP header
                let icmp_offset = if socket.is_dgram() {
                    0
                } else if recv_buffer[0] >> 4 == 4 {
                    // IPv4: IHL field indicates header length
                    ((recv_buffer[0] & 0x0F) * 4) as usize
                } else {
                    0
                };

                if let Some(response) = parse_icmpv4_packet(&recv_buffer[icmp_offset..n]) {
                    match response {
                        IcmpResponse::EchoReply {
                            identifier: id,
                            sequence: seq,
                        } => {
                            // For DGRAM sockets, kernel handles identifier filtering
                            // For RAW sockets, we need to verify both
                            if (socket.is_dgram() || id == identifier) && seq == sequence {
                                println!(
                                    "✓ Reply from {}: {} bytes, time={:.2}ms",
                                    from.ip(),
                                    n,
                                    elapsed.as_secs_f64() * 1000.0
                                );
                                return Ok(());
                            }
                        }
                        IcmpResponse::TimeExceeded(_) => {
                            println!("  Time exceeded from {}", from.ip());
                        }
                        IcmpResponse::DestinationUnreachable(_) => {
                            println!("  Destination unreachable from {}", from.ip());
                        }
                        IcmpResponse::Other => {
                            // Ignore other ICMP messages
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                eprintln!("✗ Receive error: {}", e);
                return Err(e.into());
            }
            Err(_) => {
                // Timeout on this receive, try again
                continue;
            }
        }
    }
}
