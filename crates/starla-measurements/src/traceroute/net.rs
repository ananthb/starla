//! evtraceroute's sockets: one raw ICMP socket that hears every reply (and
//! sends the ICMP probes), a fresh UDP or raw TCP socket per UDP/TCP probe,
//! and a raw TCP socket for the target's TCP answers — create_socket and
//! send_pkt (probe-busybox/eperd/traceroute.c:502-1575, 4572-4789).
//!
//! The receiving socket must be SOCK_RAW. An unprivileged ICMP "ping"
//! socket (SOCK_DGRAM, what starla_network's helpers prefer) only ever
//! sees echo replies to its own requests: it never gets the time-exceeded
//! and port-unreachable errors that UDP and TCP probes (and ICMP probes on
//! the way) draw. That is why, on 2026-10-13, every hop of Starla's IPv6
//! UDP traceroutes read { "x":"*" } while 1,900 port unreachables from the
//! target arrived. Starla's unit has CAP_NET_RAW.
//!
//! macOS is the exception: there an ICMP SOCK_DGRAM socket needs no root
//! and still hears every ICMP message, errors included (XNU puts it on the
//! raw-IP pcb list, keeps the echo id we send and returns the IPv4 header),
//! so an unprivileged probe falls back to one for ICMP and UDP traces; see
//! [`reply_socket`].

use super::engine::{Hop, Probe, Trace};
use super::wire::{self, TraceId, BASE_PORT, SRC_BASE_PORT};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use starla_common::{TracerouteOptions, TracerouteProtocol};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::AsRawFd;
use std::time::Duration;
use tokio::io::unix::AsyncFd;

/// base->packet: MAX_DATA_SIZE + 128 (traceroute.c:90); longer replies are
/// cut to it, and their "size" with them.
const RECV_BUF: usize = 4096 + 128;

/// gettime_mono: CLOCK_MONOTONIC, which the reference also writes into
/// every v6info.
pub fn mono() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime only writes the timespec it is given.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// strerror(errno), as the reference writes it into a result.
pub fn strerror(e: &io::Error) -> String {
    let s = e.to_string();
    match s.rfind(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

fn setsockopt_int(
    sock: &Socket,
    level: libc::c_int,
    name: libc::c_int,
    v: libc::c_int,
) -> io::Result<()> {
    // SAFETY: a valid fd and a pointer to an int of the size given.
    let r = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            level,
            name,
            &v as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if r == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// set_tos (traceroute.c:460-500): IPV6_TCLASS / IP_TOS when `-t` is given.
fn set_tos(sock: &Socket, v6: bool, tos: u32) -> Result<(), String> {
    if tos == 0 {
        return Ok(());
    }
    let r = if v6 {
        setsockopt_int(
            sock,
            libc::IPPROTO_IPV6,
            libc::IPV6_TCLASS,
            tos as libc::c_int,
        )
    } else {
        setsockopt_int(sock, libc::IPPROTO_IP, libc::IP_TOS, tos as libc::c_int)
    };
    r.map_err(|_| {
        format!(
            "setting {} failed",
            if v6 { "traffic class" } else { "ToS" }
        )
    })
}

/// IP(V6)_MTU_DISCOVER: don't fragment with `-F`, else let the kernel
/// fragment. Linux only, as in the reference.
fn set_df(sock: &Socket, v6: bool, df: bool) {
    #[cfg(target_os = "linux")]
    {
        let _ = if v6 {
            setsockopt_int(
                sock,
                libc::IPPROTO_IPV6,
                libc::IPV6_MTU_DISCOVER,
                if df {
                    libc::IPV6_PMTUDISC_DO
                } else {
                    libc::IPV6_PMTUDISC_DONT
                },
            )
        } else {
            setsockopt_int(
                sock,
                libc::IPPROTO_IP,
                libc::IP_MTU_DISCOVER,
                if df {
                    libc::IP_PMTUDISC_DO
                } else {
                    libc::IP_PMTUDISC_DONT
                },
            )
        };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (sock, v6, df);
}

/// do_ipv6_option (probe-busybox/libbb/atlas_ipv6_option.c): a sticky
/// hop-by-hop or destination options header of `size` bytes of padding
/// (none for 0). Errors are ignored, as there.
fn set_ipv6_option(sock: &Socket, dest: bool, size: u16) {
    #[cfg(target_os = "linux")]
    {
        let name = if dest {
            libc::IPV6_DSTOPTS
        } else {
            libc::IPV6_HOPOPTS
        };
        let mut buf = Vec::new();
        if size != 0 {
            let total = (2 + size as usize).next_multiple_of(8);
            let ehlen = total / 8 - 1;
            if ehlen > 255 {
                return;
            }
            buf = vec![0u8; total];
            buf[1] = ehlen as u8;
            let mut i = 2;
            while i < total {
                let pad = total - i;
                if pad == 1 {
                    // Pad1
                    buf[i] = 0;
                    i += 1;
                    continue;
                }
                let n = (pad - 2).min(255);
                buf[i] = 1; // PadN
                buf[i + 1] = n as u8;
                i += 2 + n;
            }
        }
        // SAFETY: a valid fd and a buffer of the length given (or none).
        unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::IPPROTO_IPV6,
                name,
                if buf.is_empty() {
                    std::ptr::null()
                } else {
                    buf.as_ptr() as *const libc::c_void
                },
                buf.len() as libc::socklen_t,
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (sock, dest, size);
}

/// `-i`: SO_BINDTODEVICE (Linux).
fn bind_interface(sock: &Socket, interface: Option<&str>) -> Result<(), String> {
    let Some(name) = interface else {
        return Ok(());
    };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        sock.bind_device(Some(name.as_bytes()))
            .map_err(|_| "bind_interface failed".to_string())
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = (sock, name);
        Err("bind_interface failed".to_string())
    }
}

fn sockaddr_ip(ss: &libc::sockaddr_storage) -> Option<IpAddr> {
    match ss.ss_family as libc::c_int {
        libc::AF_INET => {
            // SAFETY: the family says this storage holds a sockaddr_in.
            let sin = unsafe { &*(ss as *const _ as *const libc::sockaddr_in) };
            Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                sin.sin_addr.s_addr,
            ))))
        }
        libc::AF_INET6 => {
            // SAFETY: the family says this storage holds a sockaddr_in6.
            let sin6 = unsafe { &*(ss as *const _ as *const libc::sockaddr_in6) };
            Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
        }
        _ => None,
    }
}

/// A packet off a raw socket.
struct Received {
    len: usize,
    from: IpAddr,
    /// IPV6_HOPLIMIT, -42 without one (rcvdttl's "spot problems" value)
    hoplimit: i32,
    /// IPV6_TCLASS, -42 without one
    tclass: i32,
    at: Duration,
}

/// A raw socket that reports the IPv6 hop limit and traffic class of what
/// it receives.
struct RawSock {
    fd: AsyncFd<Socket>,
}

impl RawSock {
    fn new(v6: bool, protocol: Protocol) -> io::Result<Self> {
        Self::with_type(v6, Type::RAW, protocol)
    }

    fn with_type(v6: bool, ty: Type, protocol: Protocol) -> io::Result<Self> {
        let domain = if v6 { Domain::IPV6 } else { Domain::IPV4 };
        let sock = Socket::new(domain, ty, Some(protocol))?;
        sock.set_nonblocking(true)?;
        if v6 {
            setsockopt_int(&sock, libc::IPPROTO_IPV6, libc::IPV6_RECVHOPLIMIT, 1)?;
            setsockopt_int(&sock, libc::IPPROTO_IPV6, libc::IPV6_RECVTCLASS, 1)?;
        }
        Ok(Self {
            fd: AsyncFd::new(sock)?,
        })
    }

    fn sock(&self) -> &Socket {
        self.fd.get_ref()
    }

    fn try_recv(&self, buf: &mut [u8]) -> io::Result<Received> {
        // SAFETY: plain C structs, zero is a valid value for all of them.
        let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        // u64s: cmsg buffers must be aligned for cmsghdr.
        let mut control = [0u64; 32];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = &mut ss as *mut _ as *mut libc::c_void;
        msg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = std::mem::size_of_val(&control) as _;
        // SAFETY: msg points at live buffers of the sizes it states.
        let n = unsafe { libc::recvmsg(self.sock().as_raw_fd(), &mut msg, libc::MSG_DONTWAIT) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let at = mono();
        let (mut hoplimit, mut tclass) = (-42, -42);
        // SAFETY: the CMSG_* walk stays inside msg_control/msg_controllen,
        // which the kernel just filled; the ints are read unaligned.
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_len == 0 {
                    break;
                }
                if (*c).cmsg_level == libc::IPPROTO_IPV6 {
                    let v = std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const libc::c_int);
                    if (*c).cmsg_type == libc::IPV6_HOPLIMIT {
                        hoplimit = v;
                    } else if (*c).cmsg_type == libc::IPV6_TCLASS {
                        tclass = v;
                    }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
        let from = sockaddr_ip(&ss).ok_or_else(|| io::Error::other("no source address"))?;
        Ok(Received {
            len: n as usize,
            from,
            hoplimit,
            tclass,
            at,
        })
    }

    async fn recv(&self, buf: &mut [u8]) -> io::Result<Received> {
        loop {
            let mut guard = self.fd.readable().await?;
            match guard.try_io(|_| self.try_recv(buf)) {
                Ok(r) => return r,
                Err(_would_block) => continue,
            }
        }
    }
}

/// The socket that hears every reply (create_socket's socket_icmp): raw,
/// as in the reference. On macOS without root, where SOCK_RAW fails with
/// EPERM, an ICMP SOCK_DGRAM socket instead: XNU attaches it to the same
/// raw-IP pcb list (icmp_dgram_attach), so it receives every ICMP message
/// a raw socket would — time exceededs and port unreachables quoting our
/// UDP probes too — with the IPv4 header in front, and it sends our echo
/// requests unchanged, id included. Before evtraceroute's sockets, Starla
/// traced through such sockets on macOS, and its install guide promises
/// that ping and traceroute work there without root. A TCP trace still
/// needs root there: its SYNs and the target's answers go through raw TCP
/// sockets, and the reference has no other way either.
fn reply_socket(v6: bool, protocol: TracerouteProtocol) -> io::Result<RawSock> {
    let icmp = if v6 {
        Protocol::ICMPV6
    } else {
        Protocol::ICMPV4
    };
    let raw = RawSock::new(v6, icmp);
    #[cfg(target_os = "macos")]
    if let Err(e) = &raw {
        let unprivileged = matches!(e.raw_os_error(), Some(libc::EPERM | libc::EACCES));
        if unprivileged && protocol != TracerouteProtocol::TCP {
            if let Ok(dgram) = RawSock::with_type(v6, Type::DGRAM, icmp) {
                return Ok(dgram);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = protocol;
    raw
}

async fn recv_opt(sock: Option<&RawSock>, buf: &mut [u8]) -> io::Result<Received> {
    match sock {
        Some(s) => s.recv(buf).await,
        None => std::future::pending().await,
    }
}

/// What one run needs to know.
pub struct RunSpec<'a> {
    pub target: IpAddr,
    pub opts: &'a TracerouteOptions,
    pub paris: Option<u16>,
    /// the measurement's slot (trtstate.index)
    pub index: u16,
}

/// Everything send_pkt needs for a probe.
struct Sender<'a> {
    spec: &'a RunSpec<'a>,
    id: TraceId,
    src: IpAddr,
    icmp: &'a RawSock,
}

/// send_pkt's error handling: these errno values leave the probe to time
/// out; anything else ends the trace with "sendto failed". Every branch
/// lets EMSGSIZE pass; only the IPv6 TCP and UDP ones also EACCES and
/// ECONNREFUSED (traceroute.c:742-744 and 1042-1044). IPv4 TCP and UDP
/// (1237, 1539) and both ICMPs (884, 1361) take nothing else.
fn ignorable(errno: i32, v6: bool, protocol: TracerouteProtocol) -> bool {
    if errno == libc::EMSGSIZE {
        return true;
    }
    let tcp_or_udp6 = v6 && matches!(protocol, TracerouteProtocol::TCP | TracerouteProtocol::UDP);
    tcp_or_udp6 && (errno == libc::EACCES || errno == libc::ECONNREFUSED)
}

impl Sender<'_> {
    fn v6(&self) -> bool {
        self.spec.target.is_ipv6()
    }

    fn local(&self) -> SockAddr {
        SockAddr::from(SocketAddr::new(
            self.src,
            SRC_BASE_PORT.wrapping_add(self.spec.index),
        ))
    }

    fn set_hops(&self, sock: &Socket, hop: u8) {
        let _ = if self.v6() {
            sock.set_unicast_hops_v6(hop as u32)
        } else {
            sock.set_ttl_v4(hop as u32)
        };
    }

    /// One probe; Err is the text of the error that ends the trace.
    fn send(&self, probe: &Probe, tv: Duration) -> Result<(), String> {
        let opts = self.spec.opts;
        let v6 = self.v6();
        let target = self.spec.target;
        let r = match opts.protocol {
            TracerouteProtocol::ICMP => {
                let sock = self.icmp.sock();
                self.set_hops(sock, probe.hop);
                set_df(sock, v6, opts.dont_fragment);
                let packet = match (self.src, target) {
                    (IpAddr::V6(src), IpAddr::V6(dst)) => {
                        set_ipv6_option(sock, false, opts.hbh_opt_size);
                        set_ipv6_option(sock, true, opts.dest_opt_size);
                        wire::icmp6_echo(
                            src,
                            dst,
                            self.id.pid,
                            self.spec.index,
                            probe.seq,
                            tv,
                            probe.size,
                            self.spec.paris,
                        )
                    }
                    _ => wire::icmp4_echo(self.spec.index, probe.seq, probe.size, self.spec.paris),
                };
                sock.send_to(&packet, &SocketAddr::new(target, 0).into())
            }
            TracerouteProtocol::UDP => {
                let domain = if v6 { Domain::IPV6 } else { Domain::IPV4 };
                let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))
                    .map_err(|e| format!("socket failed: {}", strerror(&e)))?;
                let _ = sock.set_reuse_address(true);
                if v6 && opts.hbh_opt_size != 0 {
                    set_ipv6_option(&sock, false, opts.hbh_opt_size);
                }
                if v6 && opts.dest_opt_size != 0 {
                    set_ipv6_option(&sock, true, opts.dest_opt_size);
                }
                set_tos(&sock, v6, opts.tos)?;
                bind_interface(&sock, opts.interface.as_deref())?;
                sock.bind(&self.local())
                    .map_err(|e| format!("bind failed: {}", strerror(&e)))?;
                // Paris: one flow for the whole trace; classic: a port per probe.
                let dport = match self.spec.paris {
                    Some(paris) => BASE_PORT.wrapping_add(paris),
                    None => BASE_PORT.wrapping_add(probe.seq),
                };
                self.set_hops(&sock, probe.hop);
                set_df(&sock, v6, opts.dont_fragment);
                let payload = match (self.src, target) {
                    (_, IpAddr::V6(_)) => {
                        wire::udp6_payload(self.id.pid, self.spec.index, probe.seq, tv, probe.size)
                    }
                    (IpAddr::V4(src), IpAddr::V4(dst)) => {
                        let val = if self.spec.paris.is_some() {
                            probe.seq
                        } else {
                            self.spec.index.wrapping_add(1)
                        };
                        let sport = SRC_BASE_PORT.wrapping_add(self.spec.index);
                        wire::udp4_payload(src, dst, sport, dport, probe.size, val)
                    }
                    _ => return Err("bind failed: address family mismatch".into()),
                };
                sock.send_to(&payload, &SocketAddr::new(target, dport).into())
            }
            TracerouteProtocol::TCP => {
                let domain = if v6 { Domain::IPV6 } else { Domain::IPV4 };
                let sock = Socket::new(domain, Type::RAW, Some(Protocol::TCP))
                    .map_err(|e| format!("socket failed: {}", strerror(&e)))?;
                let _ = sock.set_reuse_address(true);
                if v6 && opts.hbh_opt_size != 0 {
                    set_ipv6_option(&sock, false, opts.hbh_opt_size);
                }
                if v6 && opts.dest_opt_size != 0 {
                    set_ipv6_option(&sock, true, opts.dest_opt_size);
                }
                set_tos(&sock, v6, opts.tos)?;
                bind_interface(&sock, opts.interface.as_deref())?;
                sock.bind(&self.local())
                    .map_err(|e| format!("bind failed: {}", strerror(&e)))?;
                let packet = wire::tcp_syn(
                    self.src,
                    target,
                    SRC_BASE_PORT.wrapping_add(self.spec.index),
                    opts.dest_port,
                    self.spec.index,
                    probe.seq,
                    probe.size,
                );
                self.set_hops(&sock, probe.hop);
                set_df(&sock, v6, opts.dont_fragment);
                sock.send_to(&packet, &SocketAddr::new(target, 0).into())
            }
        };
        match r {
            Ok(_) => Ok(()),
            Err(e) if ignorable(e.raw_os_error().unwrap_or(0), v6, opts.protocol) => Ok(()),
            Err(e) => Err(format!("sendto failed: {}", strerror(&e))),
        }
    }
}

/// One traceroute run, from the first probe to the result's hops.
pub async fn run(spec: &RunSpec<'_>) -> Vec<Hop> {
    let opts = spec.opts;
    let v6 = spec.target.is_ipv6();
    let mut trace = Trace::new(v6, opts, spec.paris);

    // create_socket: the address our probes leave from, then the raw ICMP
    // socket that hears the replies.
    let src = match starla_network::get_source_addr_for_dest(spec.target) {
        Ok(a) => a,
        Err(e) => {
            trace.start_failed(&format!("connect failed: {}", strerror(&e)));
            return trace.into_hops();
        }
    };
    let icmp = match reply_socket(v6, opts.protocol) {
        Ok(s) => s,
        Err(e) => {
            trace.start_failed(&format!("socket failed: {}", strerror(&e)));
            return trace.into_hops();
        }
    };
    let setup = bind_interface(icmp.sock(), opts.interface.as_deref())
        .and_then(|_| set_tos(icmp.sock(), v6, opts.tos));
    if let Err(e) = setup {
        trace.start_failed(&e);
        return trace.into_hops();
    }
    let tcp = if opts.protocol == TracerouteProtocol::TCP {
        let s = match RawSock::new(v6, Protocol::TCP) {
            Ok(s) => s,
            Err(e) => {
                trace.start_failed(&format!("socket failed: {}", strerror(&e)));
                return trace.into_hops();
            }
        };
        if let Err(e) = bind_interface(s.sock(), opts.interface.as_deref()) {
            trace.start_failed(&e);
            return trace.into_hops();
        }
        if let Err(e) = s.sock().connect(&SocketAddr::new(spec.target, 0).into()) {
            trace.start_failed(&format!("connect failed: {}", strerror(&e)));
            return trace.into_hops();
        }
        Some(s)
    } else {
        None
    };

    let id = TraceId {
        v6,
        protocol: opts.protocol,
        pid: std::process::id(),
        index: spec.index,
        target: spec.target,
        paris: spec.paris,
    };
    let sender = Sender {
        spec,
        id: id.clone(),
        src,
        icmp: &icmp,
    };
    let timeout = Duration::from_millis(opts.timeout_ms as u64);
    let mut buf = vec![0u8; RECV_BUF];
    let mut tcp_buf = vec![0u8; RECV_BUF];

    loop {
        // send_pkt's xmit_time: the rtt's start, and the v6info's tv.
        let xmit = mono();
        let Some(probe) = trace.next_probe(xmit) else {
            break;
        };
        if let Err(e) = sender.send(&probe, xmit) {
            trace.send_failed(&e);
            break;
        }
        trace.sent();
        let mut deadline = mono() + timeout;
        loop {
            let now = mono();
            if now >= deadline {
                trace.timeout();
                break;
            }
            let got = tokio::select! {
                r = icmp.recv(&mut buf) => r.map(|p| (false, p)),
                r = recv_opt(tcp.as_ref(), &mut tcp_buf) => r.map(|p| (true, p)),
                _ = tokio::time::sleep(deadline - now) => {
                    trace.timeout();
                    break;
                }
            };
            let Ok((is_tcp, pkt)) = got else {
                // ready_callback*: "read error", and on with the wait.
                continue;
            };
            let data = if is_tcp {
                &tcp_buf[..pkt.len]
            } else {
                &buf[..pkt.len]
            };
            let answer = wire::parse_reply(
                &id,
                is_tcp,
                data,
                pkt.from,
                pkt.hoplimit,
                pkt.tclass,
                trace.seq(),
            );
            if let Some(a) = answer {
                if let Some(until) = trace.reply(&a, pkt.at) {
                    deadline = until;
                }
            }
        }
    }
    trace.into_hops()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traceroute::engine::render_hops;

    /// The live traces below need sockets that hear ICMP errors: SOCK_RAW
    /// (CAP_NET_RAW or root) on Linux, where they are #[ignore]d so that a
    /// run without the capability doesn't pass them silently; the Linux
    /// check runs them as root with --include-ignored. On macOS they run
    /// everywhere: unprivileged, through the SOCK_DGRAM fallback.
    async fn trace_localhost(target: IpAddr, protocol: TracerouteProtocol) -> String {
        trace_localhost_paris(target, protocol, Some(1)).await
    }

    async fn trace_localhost_paris(
        target: IpAddr,
        protocol: TracerouteProtocol,
        paris: Option<u16>,
    ) -> String {
        let opts = TracerouteOptions {
            protocol,
            timeout_ms: 1000,
            ..Default::default()
        };
        let spec = RunSpec {
            target,
            opts: &opts,
            paris,
            index: 4000 + protocol as u16 + if paris.is_some() { 0 } else { 8 },
        };
        let r = render_hops(&run(&spec).await);
        eprintln!("{} {:?}: {}", target, protocol, r);
        r
    }

    /// UDP traces to this host use Paris id 1 on Linux, as the twin's
    /// measurements did. On macOS this host as the target gets in the way
    /// of Paris, twice: XNU sends loopback UDP with a zero checksum, so an
    /// IPv4 error's quoted checksum (the Paris sequence number) is 0 and
    /// every reply reads as "late"; and it answers back-to-back datagrams
    /// of one flow with one port unreachable (a second within 10 ms gets
    /// none). Both are the target's doing, not the probe's; classic
    /// traceroute (a port per probe) avoids both.
    const LOCAL_UDP_PARIS: Option<u16> = if cfg!(target_os = "macos") {
        None
    } else {
        Some(1)
    };

    #[cfg(target_os = "macos")]
    fn root() -> bool {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    /// W2: the target's port unreachables end an IPv6 UDP trace at hop 1,
    /// with from/ttl/rtt/size as the reference writes them. ::1 answers a
    /// closed UDP port with an ICMPv6 port unreachable, as the twin's
    /// target did. A receiving socket that doesn't hear ICMP errors (a
    /// Linux ping socket, W2's cause) makes this a 32-hop trace of stars.
    #[tokio::test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "needs CAP_NET_RAW: the Linux check runs it with --include-ignored"
    )]
    async fn udp6_to_localhost_ends_at_hop_1() {
        let r = trace_localhost_paris(
            Ipv6Addr::LOCALHOST.into(),
            TracerouteProtocol::UDP,
            LOCAL_UDP_PARIS,
        )
        .await;
        let re = "{ \"from\":\"::1\", \"ttl\":64, \"rtt\":";
        assert!(
            r.starts_with("[ { \"hop\":1, \"result\": [ { \"from\":\"::1\""),
            "{r}"
        );
        assert_eq!(r.matches(re).count(), 3, "{r}");
        // -S 40: an ICMPv6 error quoting 40 + 8 + 40 bytes.
        assert_eq!(r.matches(", \"size\":88 }").count(), 3, "{r}");
        assert!(r.ends_with(" } ] } ]"), "{r}");
        assert!(!r.contains("\"hop\":2"), "{r}");
    }

    #[tokio::test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "needs CAP_NET_RAW: the Linux check runs it with --include-ignored"
    )]
    async fn udp4_to_localhost_ends_at_hop_1() {
        let r = trace_localhost_paris(
            Ipv4Addr::LOCALHOST.into(),
            TracerouteProtocol::UDP,
            LOCAL_UDP_PARIS,
        )
        .await;
        // -S 40: Linux quotes 20 + 8 + 40 bytes, size = that (nrecv - 28);
        // XNU only the IP header and 8 bytes (RFC 792).
        let size = if cfg!(target_os = "macos") { 28 } else { 68 };
        assert!(
            r.starts_with(&format!(
                "[ {{ \"hop\":1, \"result\": [ {{ \"from\":\"127.0.0.1\", \"ttl\":64, \
                 \"size\":{size}, \"rtt\":"
            )),
            "{r}"
        );
        assert_eq!(r.matches("\"from\":\"127.0.0.1\"").count(), 3, "{r}");
        assert!(!r.contains("\"hop\":2"), "{r}");
    }

    #[tokio::test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "needs CAP_NET_RAW: the Linux check runs it with --include-ignored"
    )]
    async fn icmp_to_localhost_ends_at_hop_1() {
        let r = trace_localhost(Ipv6Addr::LOCALHOST.into(), TracerouteProtocol::ICMP).await;
        assert!(
            r.starts_with("[ { \"hop\":1, \"result\": [ { \"from\":\"::1\", \"ttl\":64, \"rtt\":"),
            "{r}"
        );
        assert_eq!(r.matches(", \"size\":40 }").count(), 3, "{r}");
        let r = trace_localhost(Ipv4Addr::LOCALHOST.into(), TracerouteProtocol::ICMP).await;
        assert!(
            r.starts_with(
                "[ { \"hop\":1, \"result\": [ { \"from\":\"127.0.0.1\", \"ttl\":64, \"size\":40, \
                 \"rtt\":"
            ),
            "{r}"
        );
        assert_eq!(r.matches("\"from\":\"127.0.0.1\"").count(), 3, "{r}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "needs CAP_NET_RAW: the Linux check runs it with --include-ignored"]
    async fn tcp_to_localhost_gets_the_rst() {
        let r = trace_localhost(Ipv4Addr::LOCALHOST.into(), TracerouteProtocol::TCP).await;
        assert!(
            r.starts_with(
                "[ { \"hop\":1, \"result\": [ { \"from\":\"127.0.0.1\", \"ttl\":64, \"size\":0, \
                 \"flags\":\"RA\", \"rtt\":"
            ),
            "{r}"
        );
        assert!(!r.contains("\"hop\":2"), "{r}");
    }

    /// macOS without root (the Homebrew LaunchAgent): ICMP and UDP traces
    /// hear their replies through the SOCK_DGRAM fallback; a TCP trace
    /// can't, and says so the way create_socket does.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn unprivileged_macos_falls_back_for_icmp_and_udp_only() {
        if root() {
            eprintln!("root: SOCK_RAW works, no fallback to test");
            return;
        }
        assert!(Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::ICMPV4)).is_err());
        for v6 in [false, true] {
            assert!(reply_socket(v6, TracerouteProtocol::ICMP).is_ok());
            assert!(reply_socket(v6, TracerouteProtocol::UDP).is_ok());
            let e = reply_socket(v6, TracerouteProtocol::TCP).err().unwrap();
            assert_eq!(e.raw_os_error(), Some(libc::EPERM));
        }
        let r = trace_localhost(Ipv4Addr::LOCALHOST.into(), TracerouteProtocol::TCP).await;
        assert_eq!(
            r,
            "[ { \"hop\":1, \"error\":\"socket failed: Operation not permitted\" } ]"
        );
    }

    /// Which sendto errors leave the probe to time out: EMSGSIZE always;
    /// EACCES and ECONNREFUSED only for IPv6 TCP and UDP.
    #[test]
    fn sendto_errors_the_reference_ignores() {
        use TracerouteProtocol::*;
        for v6 in [false, true] {
            for p in [ICMP, UDP, TCP] {
                assert!(ignorable(libc::EMSGSIZE, v6, p), "{v6} {p:?}");
                let l4_6 = v6 && p != ICMP;
                assert_eq!(ignorable(libc::EACCES, v6, p), l4_6, "{v6} {p:?}");
                assert_eq!(ignorable(libc::ECONNREFUSED, v6, p), l4_6, "{v6} {p:?}");
                for errno in [libc::ENETUNREACH, libc::EHOSTUNREACH, libc::ENOBUFS, 0] {
                    assert!(!ignorable(errno, v6, p), "{v6} {p:?} {errno}");
                }
            }
        }
    }
}
