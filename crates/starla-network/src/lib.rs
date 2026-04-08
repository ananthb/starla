//! Network primitives for Starla measurements

pub mod capabilities;

#[cfg(unix)]
pub mod icmp;
#[cfg(unix)]
pub mod raw_socket;

pub use capabilities::*;
#[cfg(unix)]
pub use icmp::*;
#[cfg(unix)]
pub use raw_socket::{get_source_addr_for_dest, new_icmpv4_socket, new_icmpv6_socket, RawSocket};

/// Get the source address for a destination (stub for non-Unix)
#[cfg(not(unix))]
pub fn get_source_addr_for_dest(_dest: std::net::IpAddr) -> std::io::Result<std::net::IpAddr> {
    // On Windows, we can't easily determine the source address
    // Return loopback as a fallback
    Ok(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}
