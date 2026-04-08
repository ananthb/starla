//! Windows ICMP Helper API wrapper
//!
//! Uses IcmpSendEcho2/Icmp6SendEcho2 from iphlpapi.dll for ping and traceroute
//! without requiring administrator privileges.

use std::io;
use std::mem;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ptr;
use std::time::Instant;

// IP Status codes from ipexport.h
const IP_SUCCESS: u32 = 0;
const IP_TTL_EXPIRED_TRANSIT: u32 = 11013;
const IP_DEST_NET_UNREACHABLE: u32 = 11002;
const IP_DEST_HOST_UNREACHABLE: u32 = 11003;
const IP_DEST_PROT_UNREACHABLE: u32 = 11004;
const IP_DEST_PORT_UNREACHABLE: u32 = 11005;
const IP_REQ_TIMED_OUT: u32 = 11010;

const AF_INET6: u16 = 23;
const INVALID_HANDLE_VALUE: isize = -1;

#[repr(C)]
#[derive(Clone, Copy)]
struct IpOptionInformation {
    ttl: u8,
    tos: u8,
    flags: u8,
    options_size: u8,
    options_data: *mut u8,
}

// SAFETY: IpOptionInformation only contains a raw pointer that we always set to
// null. The struct is only used within a single blocking call scope.
unsafe impl Send for IpOptionInformation {}

#[repr(C)]
struct IcmpEchoReply {
    address: u32,
    status: u32,
    round_trip_time: u32,
    data_size: u16,
    reserved: u16,
    data: *mut std::ffi::c_void,
    options: IpOptionInformation,
}

#[repr(C)]
struct SockaddrIn6 {
    sin6_family: u16,
    sin6_port: u16,
    sin6_flowinfo: u32,
    sin6_addr: [u8; 16],
    sin6_scope_id: u32,
}

#[repr(C)]
struct Ipv6AddressEx {
    sin6_port: u16,
    sin6_flowinfo: u32,
    sin6_addr: [u16; 8],
    sin6_scope_id: u32,
}

#[repr(C)]
struct Icmpv6EchoReply {
    address: Ipv6AddressEx,
    status: u32,
    round_trip_time: u32,
}

#[link(name = "iphlpapi")]
extern "system" {
    fn IcmpCreateFile() -> isize;
    fn Icmp6CreateFile() -> isize;
    fn IcmpCloseHandle(handle: isize) -> i32;
    fn IcmpSendEcho2(
        icmp_handle: isize,
        event: isize,
        apc_routine: *const std::ffi::c_void,
        apc_context: *const std::ffi::c_void,
        destination_address: u32,
        request_data: *const std::ffi::c_void,
        request_size: u16,
        request_options: *const IpOptionInformation,
        reply_buffer: *mut std::ffi::c_void,
        reply_size: u32,
        timeout: u32,
    ) -> u32;
    fn Icmp6SendEcho2(
        icmp_handle: isize,
        event: isize,
        apc_routine: *const std::ffi::c_void,
        apc_context: *const std::ffi::c_void,
        source_address: *const SockaddrIn6,
        destination_address: *const SockaddrIn6,
        request_data: *const std::ffi::c_void,
        request_size: u16,
        request_options: *const IpOptionInformation,
        reply_buffer: *mut std::ffi::c_void,
        reply_size: u32,
        timeout: u32,
    ) -> u32;
}

/// ICMP reply status
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IcmpStatus {
    Success,
    TtlExpired,
    DestUnreachable,
    TimedOut,
    Other(u32),
}

impl From<u32> for IcmpStatus {
    fn from(status: u32) -> Self {
        match status {
            IP_SUCCESS => IcmpStatus::Success,
            IP_TTL_EXPIRED_TRANSIT => IcmpStatus::TtlExpired,
            IP_DEST_NET_UNREACHABLE
            | IP_DEST_HOST_UNREACHABLE
            | IP_DEST_PROT_UNREACHABLE
            | IP_DEST_PORT_UNREACHABLE => IcmpStatus::DestUnreachable,
            IP_REQ_TIMED_OUT => IcmpStatus::TimedOut,
            other => IcmpStatus::Other(other),
        }
    }
}

/// Result of an ICMP ping via the Windows ICMP Helper API
#[derive(Debug, Clone)]
pub struct IcmpPingReply {
    pub from: IpAddr,
    pub rtt_ms: f64,
    pub status: IcmpStatus,
    pub reply_ttl: u8,
}

/// RAII wrapper for Windows ICMP handle
struct IcmpHandle(isize);

impl IcmpHandle {
    fn new_v4() -> io::Result<Self> {
        let handle = unsafe { IcmpCreateFile() };
        if handle == INVALID_HANDLE_VALUE || handle == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }

    fn new_v6() -> io::Result<Self> {
        let handle = unsafe { Icmp6CreateFile() };
        if handle == INVALID_HANDLE_VALUE || handle == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }
}

impl Drop for IcmpHandle {
    fn drop(&mut self) {
        unsafe {
            IcmpCloseHandle(self.0);
        }
    }
}

// SAFETY: The ICMP handle is a kernel object that can be used from any thread.
unsafe impl Send for IcmpHandle {}

fn icmp_ping_v4_sync(
    handle: &IcmpHandle,
    dest: Ipv4Addr,
    ttl: u8,
    timeout_ms: u32,
    payload: &[u8],
) -> io::Result<IcmpPingReply> {
    let dest_addr = u32::from_ne_bytes(dest.octets());

    let options = IpOptionInformation {
        ttl,
        tos: 0,
        flags: 0,
        options_size: 0,
        options_data: ptr::null_mut(),
    };

    // Reply buffer must hold at least one ICMP_ECHO_REPLY + payload + 8 bytes
    let reply_size = mem::size_of::<IcmpEchoReply>() + payload.len() + 8;
    let mut reply_buf = vec![0u8; reply_size];

    let start = Instant::now();

    let num_replies = unsafe {
        IcmpSendEcho2(
            handle.0,
            0,
            ptr::null(),
            ptr::null(),
            dest_addr,
            payload.as_ptr() as *const _,
            payload.len() as u16,
            &options,
            reply_buf.as_mut_ptr() as *mut _,
            reply_size as u32,
            timeout_ms,
        )
    };

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

    if num_replies == 0 {
        let err = io::Error::last_os_error();
        let code = err.raw_os_error().unwrap_or(0) as u32;
        let status = IcmpStatus::from(code);

        match status {
            IcmpStatus::TimedOut => {
                return Ok(IcmpPingReply {
                    from: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    rtt_ms: elapsed_ms,
                    status: IcmpStatus::TimedOut,
                    reply_ttl: 0,
                });
            }
            // TTL expired and dest unreachable still populate the reply buffer
            IcmpStatus::TtlExpired | IcmpStatus::DestUnreachable => {
                let reply = unsafe { &*(reply_buf.as_ptr() as *const IcmpEchoReply) };
                let from = Ipv4Addr::from(reply.address.to_ne_bytes());
                return Ok(IcmpPingReply {
                    from: IpAddr::V4(from),
                    rtt_ms: elapsed_ms,
                    status,
                    reply_ttl: reply.options.ttl,
                });
            }
            _ => return Err(err),
        }
    }

    let reply = unsafe { &*(reply_buf.as_ptr() as *const IcmpEchoReply) };
    let from = Ipv4Addr::from(reply.address.to_ne_bytes());
    let status = IcmpStatus::from(reply.status);

    Ok(IcmpPingReply {
        from: IpAddr::V4(from),
        rtt_ms: elapsed_ms,
        status,
        reply_ttl: reply.options.ttl,
    })
}

fn icmp_ping_v6_sync(
    handle: &IcmpHandle,
    src: Ipv6Addr,
    dest: Ipv6Addr,
    ttl: u8,
    timeout_ms: u32,
    payload: &[u8],
) -> io::Result<IcmpPingReply> {
    let src_addr = SockaddrIn6 {
        sin6_family: AF_INET6,
        sin6_port: 0,
        sin6_flowinfo: 0,
        sin6_addr: src.octets(),
        sin6_scope_id: 0,
    };

    let dest_addr = SockaddrIn6 {
        sin6_family: AF_INET6,
        sin6_port: 0,
        sin6_flowinfo: 0,
        sin6_addr: dest.octets(),
        sin6_scope_id: 0,
    };

    let options = IpOptionInformation {
        ttl,
        tos: 0,
        flags: 0,
        options_size: 0,
        options_data: ptr::null_mut(),
    };

    let reply_size = mem::size_of::<Icmpv6EchoReply>() + payload.len() + 8;
    let mut reply_buf = vec![0u8; reply_size.max(256)];

    let start = Instant::now();

    let num_replies = unsafe {
        Icmp6SendEcho2(
            handle.0,
            0,
            ptr::null(),
            ptr::null(),
            &src_addr,
            &dest_addr,
            payload.as_ptr() as *const _,
            payload.len() as u16,
            &options,
            reply_buf.as_mut_ptr() as *mut _,
            reply_buf.len() as u32,
            timeout_ms,
        )
    };

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

    if num_replies == 0 {
        let err = io::Error::last_os_error();
        let code = err.raw_os_error().unwrap_or(0) as u32;
        let status = IcmpStatus::from(code);

        match status {
            IcmpStatus::TimedOut => {
                return Ok(IcmpPingReply {
                    from: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                    rtt_ms: elapsed_ms,
                    status: IcmpStatus::TimedOut,
                    reply_ttl: 0,
                });
            }
            IcmpStatus::TtlExpired | IcmpStatus::DestUnreachable => {
                let reply = unsafe { &*(reply_buf.as_ptr() as *const Icmpv6EchoReply) };
                let from = ipv6_from_address_ex(&reply.address);
                return Ok(IcmpPingReply {
                    from: IpAddr::V6(from),
                    rtt_ms: elapsed_ms,
                    status,
                    reply_ttl: 0,
                });
            }
            _ => return Err(err),
        }
    }

    let reply = unsafe { &*(reply_buf.as_ptr() as *const Icmpv6EchoReply) };
    let from = ipv6_from_address_ex(&reply.address);
    let status = IcmpStatus::from(reply.status);

    Ok(IcmpPingReply {
        from: IpAddr::V6(from),
        rtt_ms: elapsed_ms,
        status,
        reply_ttl: 0, // IPv6 echo reply doesn't expose TTL
    })
}

fn ipv6_from_address_ex(addr: &Ipv6AddressEx) -> Ipv6Addr {
    let mut bytes = [0u8; 16];
    for (i, word) in addr.sin6_addr.iter().enumerate() {
        let b = word.to_be_bytes();
        bytes[i * 2] = b[0];
        bytes[i * 2 + 1] = b[1];
    }
    Ipv6Addr::from(bytes)
}

/// Send an ICMP echo request and receive a reply.
///
/// Uses `tokio::task::spawn_blocking` since the Windows ICMP API is
/// synchronous. No administrator privileges are required.
pub async fn icmp_ping(
    dest: IpAddr,
    ttl: u8,
    timeout_ms: u32,
    payload: Vec<u8>,
) -> io::Result<IcmpPingReply> {
    tokio::task::spawn_blocking(move || match dest {
        IpAddr::V4(v4) => {
            let handle = IcmpHandle::new_v4()?;
            icmp_ping_v4_sync(&handle, v4, ttl, timeout_ms, &payload)
        }
        IpAddr::V6(v6) => {
            let handle = IcmpHandle::new_v6()?;
            let src = match crate::get_source_addr_for_dest(IpAddr::V6(v6)) {
                Ok(IpAddr::V6(s)) => s,
                _ => Ipv6Addr::UNSPECIFIED,
            };
            icmp_ping_v6_sync(&handle, src, v6, ttl, timeout_ms, &payload)
        }
    })
    .await
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
}
