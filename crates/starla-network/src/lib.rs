//! Network primitives for Starla measurements

pub mod capabilities;

pub mod icmp;
#[cfg(unix)]
pub mod raw_socket;
#[cfg(windows)]
pub mod windows_icmp;
pub mod wsacmsg;

pub use capabilities::*;
pub use icmp::*;
#[cfg(unix)]
pub use raw_socket::{new_icmpv4_socket, new_icmpv6_socket, RawSocket};

/// What came with a packet: its TTL or hop limit, and the address it was
/// sent to. On unix from recvmsg's ancillary data
/// ([`RawSocket::recv_meta`]), on Windows from WSARecvMsg's
/// ([`wsacmsg::recv_meta`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecvMeta {
    pub ttl: Option<u8>,
    pub dst: Option<std::net::IpAddr>,
}

/// The index of the network interface called `name`, as if_nametoindex(3)
/// gives it: what an IPv6 address's `%zone` names (a link-local resolver
/// in resolv.conf, `nameserver fe80::1%eth0`). None when there is no such
/// interface.
#[cfg(unix)]
pub fn interface_index(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    // SAFETY: `name` is a NUL-terminated string that outlives the call.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    (index != 0).then_some(index)
}

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
    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    #[test]
    fn interface_indexes() {
        let lo = if cfg!(target_os = "linux") {
            "lo"
        } else {
            "lo0"
        };
        assert!(interface_index(lo).is_some_and(|i| i > 0));
        assert_eq!(interface_index("nosuchif0"), None);
        assert_eq!(interface_index("bad\0name"), None);
    }

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
