//! Raw socket abstraction for sending/receiving packets
//!
//! This module provides a safe wrapper around raw sockets, handling:
//! - IPv4 and IPv6 support
//! - Socket creation and configuration
//! - Sending and receiving raw packets
//! - Integration with Tokio for async I/O
//!
//! Supports both privileged (RAW) and unprivileged (DGRAM) ICMP sockets.
//! On Linux with ping_group_range configured, DGRAM sockets allow unprivileged
//! users to send ICMP echo requests without CAP_NET_RAW.

use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::SocketAddr;
use tokio::io::unix::AsyncFd;
use tracing::trace;

/// A raw socket capable of sending and receiving packets
pub struct RawSocket {
    /// The underlying async file descriptor
    inner: AsyncFd<Socket>,
    /// Protocol (e.g., ICMPv4, ICMPv6)
    protocol: Protocol,
    /// Whether this is a DGRAM socket (unprivileged ICMP)
    is_dgram: bool,
}

impl RawSocket {
    /// Create a new raw socket for the specified protocol and address family
    ///
    /// Tries DGRAM first (unprivileged), falls back to RAW if needed.
    pub fn new(protocol: Protocol, domain: Domain) -> io::Result<Self> {
        // Try unprivileged DGRAM socket first for ICMP
        if protocol == Protocol::ICMPV4 || protocol == Protocol::ICMPV6 {
            if let Ok(socket) = Socket::new(domain, Type::DGRAM, Some(protocol)) {
                socket.set_nonblocking(true)?;
                trace!("Created unprivileged DGRAM ICMP socket");
                return Ok(Self {
                    inner: AsyncFd::new(socket)?,
                    protocol,
                    is_dgram: true,
                });
            }
        }

        // Fall back to RAW socket (requires CAP_NET_RAW or root)
        let socket = Socket::new(domain, Type::RAW, Some(protocol))?;
        socket.set_nonblocking(true)?;
        trace!("Created privileged RAW socket");

        Ok(Self {
            inner: AsyncFd::new(socket)?,
            protocol,
            is_dgram: false,
        })
    }

    /// Create a raw socket with explicit type (RAW or DGRAM)
    pub fn new_with_type(
        protocol: Protocol,
        domain: Domain,
        socket_type: Type,
    ) -> io::Result<Self> {
        let socket = Socket::new(domain, socket_type, Some(protocol))?;
        socket.set_nonblocking(true)?;

        Ok(Self {
            inner: AsyncFd::new(socket)?,
            protocol,
            is_dgram: socket_type == Type::DGRAM,
        })
    }

    /// Create a raw socket bound to a specific interface or address (optional)
    pub fn bind(protocol: Protocol, domain: Domain, addr: &SocketAddr) -> io::Result<Self> {
        // Try unprivileged DGRAM socket first for ICMP
        if protocol == Protocol::ICMPV4 || protocol == Protocol::ICMPV6 {
            if let Ok(socket) = Socket::new(domain, Type::DGRAM, Some(protocol)) {
                socket.set_nonblocking(true)?;
                socket.bind(&(*addr).into())?;
                trace!("Created bound unprivileged DGRAM ICMP socket");
                return Ok(Self {
                    inner: AsyncFd::new(socket)?,
                    protocol,
                    is_dgram: true,
                });
            }
        }

        let socket = Socket::new(domain, Type::RAW, Some(protocol))?;
        socket.set_nonblocking(true)?;
        socket.bind(&(*addr).into())?;

        Ok(Self {
            inner: AsyncFd::new(socket)?,
            protocol,
            is_dgram: false,
        })
    }

    /// Send a packet to a destination
    pub async fn send_to(&self, buf: &[u8], target: &SocketAddr) -> io::Result<usize> {
        loop {
            let mut guard = self.inner.writable().await?;

            match guard.try_io(|inner| inner.get_ref().send_to(buf, &(*target).into())) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }

    /// Receive a packet
    ///
    /// Returns the number of bytes read and the source address.
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        loop {
            let mut guard = self.inner.readable().await?;

            match guard.try_io(|inner| {
                // socket2's recv_from requires uninit buffer
                let maybe_uninit_buf = unsafe {
                    std::mem::transmute::<&mut [u8], &mut [std::mem::MaybeUninit<u8>]>(buf)
                };
                inner.get_ref().recv_from(maybe_uninit_buf)
            }) {
                Ok(Ok((n, addr))) => {
                    let addr = addr
                        .as_socket()
                        .ok_or_else(|| io::Error::other("Invalid socket address"))?;
                    return Ok((n, addr));
                }
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }

    /// Get the protocol this socket is configured for
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// Check if this is a DGRAM (unprivileged) socket
    pub fn is_dgram(&self) -> bool {
        self.is_dgram
    }

    /// Set the time-to-live (TTL) for outgoing packets
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        if self.protocol == Protocol::ICMPV6 {
            self.inner.get_ref().set_unicast_hops_v6(ttl)
        } else {
            self.inner.get_ref().set_ttl(ttl)
        }
    }
}

// Re-export Type for consumers that need explicit socket type control
pub use socket2::Type as SocketType;

/// Helper to create an ICMPv4 socket
pub fn new_icmpv4_socket() -> io::Result<RawSocket> {
    RawSocket::new(Protocol::ICMPV4, Domain::IPV4)
}

/// Helper to create an ICMPv6 socket
pub fn new_icmpv6_socket() -> io::Result<RawSocket> {
    RawSocket::new(Protocol::ICMPV6, Domain::IPV6)
}

/// Get the source IP address that would be used to reach a destination.
///
/// This works by creating a UDP socket and connecting it to the destination
/// (without actually sending data). The kernel then assigns the appropriate
/// local address based on routing tables.
///
/// This is a synchronous operation as it doesn't involve actual network I/O.
pub fn get_source_addr_for_dest(dest: std::net::IpAddr) -> io::Result<std::net::IpAddr> {
    use std::net::{SocketAddr, UdpSocket};

    // Use an arbitrary port (the connection won't actually send data)
    let dest_addr = SocketAddr::new(dest, 9);

    // Create a socket with the appropriate address family
    let socket = if dest.is_ipv4() {
        UdpSocket::bind("0.0.0.0:0")?
    } else {
        UdpSocket::bind("[::]:0")?
    };

    // Connect to the destination (assigns local address without sending data)
    socket.connect(dest_addr)?;

    // Get the local address the OS chose
    let local_addr = socket.local_addr()?;
    Ok(local_addr.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_socket_creation() {
        // This test requires CAP_NET_RAW or root.
        // We'll catch PermissionDenied and pass in that case to allow running on
        // non-privileged envs.
        match new_icmpv4_socket() {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                println!("Skipping test: Permission denied (CAP_NET_RAW required)");
            }
            Err(e) => panic!("Failed to create socket: {}", e),
        }
    }
}
