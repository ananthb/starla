//! Windows ICMP Helper API wrapper
//!
//! Uses IcmpSendEcho2/Icmp6SendEcho2 from iphlpapi.dll for ping and traceroute
//! without requiring administrator privileges. Icmp6SendEcho2's reply has
//! no hop limit, so an elevated ping over IPv6 uses [`RawIcmp6Socket`].

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
    .map_err(io::Error::other)?
}

/// GUID, as WSAIoctl's SIO_GET_EXTENSION_FUNCTION_POINTER takes one
#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// WSAID_WSARECVMSG (mswsock.h)
const WSAID_WSARECVMSG: Guid = Guid {
    data1: 0xf689_d7c8,
    data2: 0x6f1f,
    data3: 0x436b,
    data4: [0x8a, 0x53, 0xe5, 0x4f, 0xe3, 0x51, 0xc3, 0x22],
};

/// SIO_GET_EXTENSION_FUNCTION_POINTER: _WSAIORW(IOC_WS2, 6) (ws2def.h)
const SIO_GET_EXTENSION_FUNCTION_POINTER: u32 = 0xc800_0006;
const SOCKET_ERROR: i32 = -1;
/// POLLRDNORM (winsock2.h)
const POLLRDNORM: i16 = 0x0100;

#[repr(C)]
struct WsaBuf {
    len: u32,
    buf: *mut u8,
}

#[repr(C)]
struct WsaMsg {
    name: *mut std::ffi::c_void,
    namelen: i32,
    buffers: *mut WsaBuf,
    buffer_count: u32,
    control: WsaBuf,
    flags: u32,
}

#[repr(C)]
struct WsaPollFd {
    fd: usize,
    events: i16,
    revents: i16,
}

/// LPFN_WSARECVMSG (mswsock.h)
type WsaRecvMsg = unsafe extern "system" fn(
    s: usize,
    msg: *mut WsaMsg,
    received: *mut u32,
    overlapped: *mut std::ffi::c_void,
    completion: *const std::ffi::c_void,
) -> i32;

#[link(name = "ws2_32")]
extern "system" {
    fn WSAIoctl(
        s: usize,
        code: u32,
        in_buf: *const std::ffi::c_void,
        in_len: u32,
        out_buf: *mut std::ffi::c_void,
        out_len: u32,
        returned: *mut u32,
        overlapped: *mut std::ffi::c_void,
        completion: *const std::ffi::c_void,
    ) -> i32;
    fn WSAPoll(fds: *mut WsaPollFd, nfds: u32, timeout: i32) -> i32;
    fn setsockopt(s: usize, level: i32, name: i32, value: *const u8, len: i32) -> i32;
    fn WSAGetLastError() -> i32;
}

fn wsa_error() -> io::Error {
    // SAFETY: no arguments; reads the calling thread's last Winsock error.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// A raw ICMPv6 socket, as ping.c opens one (ping.c:1514-1537), with the
/// replies' hop limit and destination from WSARecvMsg's control data, as
/// ping.c has them from IPV6_HOPLIMIT and IPV6_PKTINFO (ping.c:1531-1537,
/// 1150-1162): the doc's "ttl" for an IPv6 ping is the replies' hop
/// limit, which Icmp6SendEcho2 does not tell. Windows gives raw sockets
/// to elevated processes only; [`RawIcmp6Socket::open`] fails with
/// WSAEACCES otherwise. Blocking: run it off the async runtime.
pub struct RawIcmp6Socket {
    socket: socket2::Socket,
    recv_msg: WsaRecvMsg,
}

impl RawIcmp6Socket {
    /// The socket, asking for each packet's hop limit and destination.
    pub fn open() -> io::Result<Self> {
        use socket2::{Domain, Protocol, Socket, Type};
        use std::os::windows::io::AsRawSocket;

        let socket = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::ICMPV6))?;
        let s = socket.as_raw_socket() as usize;
        let on: i32 = 1;
        for name in [crate::wsacmsg::IPV6_HOPLIMIT, crate::wsacmsg::IPV6_PKTINFO] {
            // SAFETY: `on` is an INT that outlives the call, as long as
            // the length given.
            let r = unsafe {
                setsockopt(
                    s,
                    crate::wsacmsg::IPPROTO_IPV6,
                    name,
                    (&on as *const i32).cast(),
                    mem::size_of::<i32>() as i32,
                )
            };
            if r == SOCKET_ERROR {
                return Err(wsa_error());
            }
        }
        // WSARecvMsg is an extension function: its address comes from the
        // provider (mswsock.h).
        let mut recv_msg: Option<WsaRecvMsg> = None;
        let mut returned = 0u32;
        // SAFETY: the GUID and the function pointer slot are live and as
        // long as the lengths given; no overlapped I/O.
        let r = unsafe {
            WSAIoctl(
                s,
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                (&WSAID_WSARECVMSG as *const Guid).cast(),
                mem::size_of::<Guid>() as u32,
                (&mut recv_msg as *mut Option<WsaRecvMsg>).cast(),
                mem::size_of::<Option<WsaRecvMsg>>() as u32,
                &mut returned,
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if r == SOCKET_ERROR {
            return Err(wsa_error());
        }
        let recv_msg = recv_msg.ok_or_else(|| io::Error::other("no WSARecvMsg"))?;
        Ok(Self { socket, recv_msg })
    }

    /// The hop limit the requests leave with.
    pub fn set_hop_limit(&self, hops: u32) -> io::Result<()> {
        self.socket.set_unicast_hops_v6(hops)
    }

    /// Bind to `local` and connect to `target`: the requests leave from
    /// `local`, which their checksum's pseudo-header names, and only
    /// `target`'s packets come up.
    pub fn bind_connect(&self, local: Ipv6Addr, target: Ipv6Addr) -> io::Result<()> {
        use std::net::SocketAddr;
        self.socket
            .bind(&SocketAddr::new(IpAddr::V6(local), 0).into())?;
        self.socket
            .connect(&SocketAddr::new(IpAddr::V6(target), 0).into())
    }

    /// Send an ICMPv6 message, its checksum filled in, to the target.
    pub fn send(&self, packet: &[u8]) -> io::Result<usize> {
        self.socket.send(packet)
    }

    /// Wait up to `timeout` for a packet: the ICMPv6 message (Windows
    /// gives no IPv6 header on a raw socket) and its hop limit and
    /// destination; None when none came in that time.
    pub fn recv_meta(
        &self,
        buf: &mut [u8],
        timeout: std::time::Duration,
    ) -> io::Result<Option<(usize, crate::RecvMeta)>> {
        use std::os::windows::io::AsRawSocket;

        let s = self.socket.as_raw_socket() as usize;
        let mut fd = WsaPollFd {
            fd: s,
            events: POLLRDNORM,
            revents: 0,
        };
        // whole milliseconds, rounded up so the wait is never cut short
        let ms = i32::try_from(timeout.as_micros().div_ceil(1000)).unwrap_or(i32::MAX);
        // SAFETY: one live WSAPOLLFD.
        let ready = unsafe { WSAPoll(&mut fd, 1, ms) };
        if ready == SOCKET_ERROR {
            return Err(wsa_error());
        }
        if ready == 0 {
            return Ok(None);
        }
        // u64s, so the buffers are aligned for the SOCKADDR and WSACMSGHDR
        let mut name = [0u64; 16];
        let mut control = [0u64; 32];
        let mut data = WsaBuf {
            len: u32::try_from(buf.len()).unwrap_or(u32::MAX),
            buf: buf.as_mut_ptr(),
        };
        let mut msg = WsaMsg {
            name: name.as_mut_ptr().cast(),
            namelen: mem::size_of_val(&name) as i32,
            buffers: &mut data,
            buffer_count: 1,
            control: WsaBuf {
                len: mem::size_of_val(&control) as u32,
                buf: control.as_mut_ptr().cast(),
            },
            flags: 0,
        };
        let mut received = 0u32;
        // SAFETY: msg points at `name`, `data` (over `buf`) and `control`,
        // all live and as long as their lengths say; no overlapped I/O.
        let r =
            unsafe { (self.recv_msg)(s, &mut msg, &mut received, ptr::null_mut(), ptr::null()) };
        if r == SOCKET_ERROR {
            return Err(wsa_error());
        }
        let len = (msg.control.len as usize).min(mem::size_of_val(&control));
        // SAFETY: `control` is live, and `len` is within it.
        let control = unsafe { std::slice::from_raw_parts(control.as_ptr().cast::<u8>(), len) };
        Ok(Some((
            received as usize,
            crate::wsacmsg::recv_meta(control),
        )))
    }
}
