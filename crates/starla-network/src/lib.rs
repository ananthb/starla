//! Network primitives for Starla measurements

pub mod capabilities;

#[cfg(unix)]
pub mod icmp;
#[cfg(unix)]
pub mod raw_socket;
#[cfg(windows)]
pub mod windows_icmp;

pub use capabilities::*;
#[cfg(unix)]
pub use icmp::*;
#[cfg(unix)]
pub use raw_socket::{new_icmpv4_socket, new_icmpv6_socket, RawSocket};

/// Get the source IP address that would be used to reach a destination.
///
/// Creates a UDP socket and connects it to the destination (without sending
/// data). The kernel assigns the appropriate local address based on routing
/// tables.
pub fn get_source_addr_for_dest(dest: std::net::IpAddr) -> std::io::Result<std::net::IpAddr> {
    use std::net::{SocketAddr, UdpSocket};

    let dest_addr = SocketAddr::new(dest, 9);

    let socket = if dest.is_ipv4() {
        UdpSocket::bind("0.0.0.0:0")?
    } else {
        UdpSocket::bind("[::]:0")?
    };

    socket.connect(dest_addr)?;
    let local_addr = socket.local_addr()?;
    Ok(local_addr.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_socket_creation() {
        #[cfg(unix)]
        {
            use std::io;
            match new_icmpv4_socket() {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    println!("Skipping test: Permission denied (CAP_NET_RAW required)");
                }
                Err(e) => panic!("Failed to create socket: {}", e),
            }
        }
    }
}
