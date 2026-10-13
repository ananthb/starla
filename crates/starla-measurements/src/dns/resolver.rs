//! evtdig on the wire: the query it builds, how it asks, and how it reads the
//! reply, after probe-busybox/eperd/evtdig.c (firmware 5130).
//!
//! The query is built byte by byte as mk_dns_buff() does, not with a DNS
//! library: which header flags and which EDNS0 options go out is the
//! measurement, and RIPE compares it across probes. The reply is decoded the
//! way printReply() decodes it, which is less than a library would.

use super::DnsConfig;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout_at;

/// The resolvers get_local_resolvers() reads (eperd/readresolv.c:43).
pub const RESOLV_CONF: &str = "/etc/resolv.conf";

/// resolv.h's MAXNS: get_local_resolvers() stops at three nameservers.
pub const MAXNS: usize = 3;

/// sizeof(struct DNS_HEADER)
const DNS_HEADER: usize = 12;

/// The reply buffer, MAX_DNS_BUF_SIZE (evtdig.c:251): a longer UDP reply is
/// cut to this, and its `size` says so.
const MAX_DNS_BUF_SIZE: usize = 5120;

/// The TCP connect *and* answer budget, CONN_TO (evtdig.c:258): the timer
/// tu_connect_to_name() arms before connecting is only deleted by
/// tu_cleanup(), so it bounds the whole exchange. `--timeout` is not used.
const TCP_TIMEOUT: Duration = Duration::from_secs(5);

/// What evutil_gai_strerror() (libevent-2.1.11-stable/evutil.c:1640) says
/// for EVUTIL_EAI_NONAME: evdns's answer for a name that does not exist.
const EAI_NONAME: &str = "nodename nor servname provided, or not known";

/// What it says for EVUTIL_EAI_FAIL: evdns's answer for every other
/// failure, a name without an address of the family asked for included
/// (evdns_err_to_getaddrinfo_err(), evdns.c:4381).
const EAI_FAIL: &str = "non-recoverable failure in name resolution";

/// gai_strerror(EAI_NONAME) as glibc, musl and the BSDs word it: the
/// system resolver's "no such name".
const SYSTEM_EAI_NONAME: [&str; 3] = [
    "Name or service not known",
    "Name does not resolve",
    "nodename nor servname provided, or not known",
];

/// The probe's resolvers, as get_local_resolvers() (eperd/readresolv.c:35)
/// reads them: the token after `nameserver` on each line that starts with
/// it, at most [`MAXNS`]. A missing file is no resolvers.
pub fn read_resolv_conf(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read(path) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&text);
    let mut list = Vec::new();
    for line in text.split('\n') {
        if list.len() >= MAXNS {
            break;
        }
        // strtok_r(line, " \t"): the first token, then the next
        let mut tokens = line.split([' ', '\t']).filter(|t| !t.is_empty());
        if tokens.next() != Some("nameserver") {
            continue;
        }
        if let Some(ns) = tokens.next() {
            list.push(ns.to_string());
        }
    }
    list
}

/// atlas_name_macro() (libbb/atlas_name_macro.c): `$p` is the probe id
/// (`%d`, -1 while unknown), `$t` the time, `$r` 16 random hex digits. Any
/// other `$` makes the expansion fail, which evtdig reports as
/// "macro expansion failed". The result is cut at 255 bytes as its buffer
/// is.
pub fn expand_name_macros(name: &str, probe_id: i64) -> Option<String> {
    const BUF: usize = 256;
    if !name.contains('$') {
        return Some(name.to_string());
    }
    let mut out = String::with_capacity(BUF);
    let mut rest = name;
    // What snprintf()/strlcpy() would keep of `s` with `out` already used
    let room = |out: &String| BUF - 1 - out.len().min(BUF - 1);
    while !rest.is_empty() {
        let Some(at) = rest.find('$') else {
            let keep = rest.len().min(room(&out));
            out.push_str(&rest[..keep]);
            break;
        };
        if at > 0 {
            // `if (len+1 > buf+sizeof(buf)-out) return NULL;`
            if at + 1 > BUF - out.len() {
                return None;
            }
            out.push_str(&rest[..at]);
        }
        let piece = match rest.as_bytes().get(at + 1) {
            Some(b'p') => probe_id.to_string(),
            Some(b't') => unix_now().to_string(),
            Some(b'r') => {
                if 8 * 2 + 1 > BUF - out.len() {
                    return None;
                }
                let bytes: [u8; 8] = rand::random();
                bytes.iter().fold(String::new(), |mut s, b| {
                    let _ = write!(s, "{b:02x}");
                    s
                })
            }
            _ => return None,
        };
        let keep = piece.len().min(room(&out));
        out.push_str(&piece[..keep]);
        rest = &rest[at + 2..];
    }
    Some(out)
}

/// ChangetoDnsNameFormat() (evtdig.c:3382): the name must end with a dot,
/// and fit in 255 bytes with labels of 1 to 63 bytes; "." is the root.
pub fn encode_name(name: &str) -> Option<Vec<u8>> {
    const MAXLEN: usize = 255;
    if name.len() + 1 > MAXLEN {
        return None;
    }
    if name == "." {
        return Some(vec![0]);
    }
    let mut out = Vec::with_capacity(name.len() + 1);
    let mut rest = name.as_bytes();
    while !rest.is_empty() {
        let dot = rest.iter().position(|&b| b == b'.')?;
        if dot == 0 || dot > 63 {
            return None;
        }
        out.push(dot as u8);
        out.extend_from_slice(&rest[..dot]);
        rest = &rest[dot + 1..];
    }
    out.push(0);
    Some(out)
}

/// mk_dns_buff() (evtdig.c:955): the query evtdig sends.
///
/// - RD only with `--resolv` or `-R`; AD with `--ad`, CD with `--cd`.
/// - An OPT record only when something needs one: `-n`, `-c`, `--cookies`,
///   `-d`, `-e` above 512, or a non-zero `--edns-version`/`--edns-flags`/
///   `--edns-option`. Its UDP size is `-e` (512 by default), DO is `-d`.
///
/// `v4` is the family of the server (the client-subnet option's family).
/// None when the name can't be encoded ("unable to format DNS query").
pub fn build_query(cfg: &DnsConfig, name: &str, id: u16, v4: bool) -> Option<Vec<u8>> {
    let qname = encode_name(name)?;
    let mut p = Vec::with_capacity(DNS_HEADER + qname.len() + 4 + 11 + 24);
    p.extend_from_slice(&id.to_be_bytes());
    let mut flags: u16 = 0;
    if cfg.resolv || cfg.rd {
        flags |= 0x0100;
    }
    if cfg.ad {
        flags |= 0x0020;
    }
    if cfg.cd {
        flags |= 0x0010;
    }
    p.extend_from_slice(&flags.to_be_bytes());
    p.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // QD 1, AN 0, NS 0, AR 0
    p.extend_from_slice(&qname);
    p.extend_from_slice(&cfg.qtype.to_be_bytes());
    p.extend_from_slice(&cfg.qclass.to_be_bytes());

    let opt = cfg.nsid
        || cfg.client_subnet
        || cfg.cookies
        || cfg.dnssec
        || cfg.edns0 > 512
        || cfg.edns_version != 0
        || cfg.edns_flags != 0
        || cfg.edns_option != 0;
    if opt {
        p.push(0); // the root
        p.extend_from_slice(&41u16.to_be_bytes()); // OPT
        p.extend_from_slice(&(cfg.edns0 as u16).to_be_bytes());
        p.push(0); // extended RCODE
        p.push(cfg.edns_version);
        let z = cfg.edns_flags | if cfg.dnssec { 0x8000 } else { 0 };
        p.extend_from_slice(&z.to_be_bytes());
        let mut rdata = Vec::new();
        if cfg.nsid {
            rdata.extend_from_slice(&[0, 3, 0, 0]);
        }
        if cfg.client_subnet {
            // RFC 7871 with a /0 source prefix: family, 0, 0
            rdata.extend_from_slice(&[0, 8, 0, 4, 0, if v4 { 1 } else { 2 }, 0, 0]);
        }
        if cfg.cookies {
            // RFC 7873: an 8-byte client cookie. The reference takes the last
            // 8 bytes of a SHA-256 over the addresses and a secret, which on
            // the wire is as random as this; it would also send a server
            // cookie remembered from an earlier reply, which Starla doesn't.
            rdata.extend_from_slice(&[0, 10, 0, 8]);
            rdata.extend_from_slice(&rand::random::<[u8; 8]>());
        }
        if cfg.edns_option != 0 {
            rdata.extend_from_slice(&cfg.edns_option.to_be_bytes());
            rdata.extend_from_slice(&[0, 0]);
        }
        p.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        p.extend_from_slice(&rdata);
        p[11] = 1; // ARCOUNT
    }
    Some(p)
}

/// A query id as mk_dns_buff() draws it: `random() % 65535`.
pub fn query_id() -> u16 {
    (rand::random::<u32>() % 65535) as u16
}

/// Parse DNS response header fields from raw bytes: ID, QDCOUNT, ANCOUNT,
/// NSCOUNT, ARCOUNT.
pub fn parse_response(raw: &[u8]) -> anyhow::Result<(u16, u16, u16, u16, u16)> {
    if raw.len() < DNS_HEADER {
        anyhow::bail!("DNS response too short: {} bytes", raw.len());
    }
    let word = |i: usize| u16::from_be_bytes([raw[i], raw[i + 1]]);
    Ok((word(0), word(4), word(6), word(8), word(10)))
}

/// What became of one query.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The reply: its bytes, and `rt` and (TCP) `qt` in milliseconds
    Reply {
        wire: Vec<u8>,
        rt: f64,
        qt: Option<f64>,
    },
    /// No reply in time: evtdig's noreply_callback(), which retries
    Timeout,
    /// An error, worded for the result's `"error"` object as the reference
    /// words it. `retry` is whether `--retry` repeats the query after it
    /// (the TCP errors tcp_reporterr() reports do; the others don't).
    Error { text: String, retry: bool },
}

/// Where a query went, as printReply() can tell.
#[derive(Debug, Clone, PartialEq)]
pub enum Dst {
    /// Nothing known (no resolvers at all)
    None,
    /// Only the server as given: it never got as far as an address
    Name(String),
    /// Sent over UDP (`ressent`)
    Udp { server: String, addr: SocketAddr },
    /// Connecting over TCP (`dst_ai_family`)
    Tcp { server: String, addr: SocketAddr },
}

/// One query, as far as it got.
#[derive(Debug, Clone)]
pub struct Attempt {
    /// `xmit_time`: when it was sent, or the connection started
    pub time: i64,
    pub dst: Dst,
    /// `loc_sin6`: the local address getsockname() gave, scope id and all
    pub src: Option<SocketAddr>,
    /// `rcvdttl`: with `--ttl`, the TTL or hop limit a UDP reply arrived
    /// with
    pub ttl: Option<i32>,
    /// The query, for `--qbuf`, once it has been sent
    pub qbuf: Option<Vec<u8>>,
    pub outcome: Outcome,
}

impl Attempt {
    fn failed(time: i64, dst: Dst, src: Option<SocketAddr>, text: String, retry: bool) -> Self {
        Self {
            time,
            dst,
            src,
            ttl: None,
            qbuf: None,
            outcome: Outcome::Error { text, retry },
        }
    }
}

/// atlas_check_addr() (libbb/atlas_check_addr.c): eperd does not measure
/// IPv4 127/8, 10/8, 172.16/12, 192.168/16, 169.254/16 or 224/4, nor IPv6
/// ::1, ::ffff:0:0/96 (IPv4-mapped) or e000::/3 (ULA, link-local and
/// multicast all start there), unless ATLAS_DISABLE_CHECK_ADDR=yes is in
/// its environment, or here [`DnsConfig::disable_check_addr`]. It keeps a
/// measurement (or a name that resolves to such an address) from probing
/// the probe's own host and LAN.
pub fn address_allowed(addr: &SocketAddr, cfg: &DnsConfig) -> bool {
    cfg.disable_check_addr
        || std::env::var("ATLAS_DISABLE_CHECK_ADDR").is_ok_and(|v| v == "yes")
        || !refused(addr.ip())
}

/// atlas_check_addr()'s bad_ipv4 and bad_ipv6 prefixes.
fn refused(ip: IpAddr) -> bool {
    const BAD_IPV4: [(u32, u32); 6] = [
        (0x7F00_0000, 8),
        (0x0A00_0000, 8),
        (0xAC10_0000, 12),
        (0xC0A8_0000, 16),
        (0xA9FE_0000, 16),
        (0xE000_0000, 4),
    ];
    const BAD_IPV6: [(u128, u32); 3] = [(1, 128), (0xFFFF_0000_0000, 96), (0xE000 << 112, 3)];
    match ip {
        IpAddr::V4(v4) => {
            let a = u32::from(v4);
            BAD_IPV4
                .iter()
                .any(|&(prefix, len)| a & (u32::MAX << (32 - len)) == prefix)
        }
        IpAddr::V6(v6) => {
            let a = u128::from(v6);
            BAD_IPV6
                .iter()
                .any(|&(prefix, len)| a & (u128::MAX << (128 - len)) == prefix)
        }
    }
}

/// INET6_ADDRSTRLEN, the buffer printReply() and tcp_beforeconnect() give
/// getnameinfo()
const INET6_ADDRSTRLEN: usize = 46;

/// An address as getnameinfo(NI_NUMERICHOST) writes it, which is how
/// printReply() writes `src_addr` (evtdig.c:3798) and tcp_beforeconnect()
/// TCP's `dst_addr` (evtdig.c:1653): an IPv6 address with a scope id gets
/// `%` and its interface's name if it is link-local (unicast or multicast),
/// the number otherwise or if the interface has no name. RIPE stored
/// `"src_addr":"fe80::b2db:876:1abb:ef37%ens18"` from firmware 5130 (msm
/// 30001, probe 1014068, 2026-10-13). As glibc, a zone that does not fit
/// the buffer is EAI_OVERFLOW, which leaves the address without one.
pub fn numeric_host(addr: &SocketAddr) -> String {
    let SocketAddr::V6(v6) = addr else {
        return addr.ip().to_string();
    };
    let host = v6.ip().to_string();
    let scope = v6.scope_id();
    if scope == 0 {
        return host;
    }
    let first = v6.ip().segments()[0];
    // IN6_IS_ADDR_LINKLOCAL, IN6_IS_ADDR_MC_LINKLOCAL
    let named = first & 0xffc0 == 0xfe80 || first & 0xff0f == 0xff02;
    let zone = named
        .then(|| interface_name(scope))
        .flatten()
        .unwrap_or_else(|| scope.to_string());
    if host.len() + 1 + zone.len() + 1 > INET6_ADDRSTRLEN {
        return host;
    }
    format!("{host}%{zone}")
}

/// if_indextoname()
#[cfg(unix)]
fn interface_name(index: u32) -> Option<String> {
    let mut buf = [0 as libc::c_char; libc::IF_NAMESIZE];
    // SAFETY: the buffer is IF_NAMESIZE long, as if_indextoname() wants
    let name = unsafe { libc::if_indextoname(index, buf.as_mut_ptr()) };
    if name.is_null() {
        return None;
    }
    // SAFETY: on success it wrote a NUL-terminated name into buf
    let name = unsafe { std::ffi::CStr::from_ptr(name) };
    Some(name.to_string_lossy().into_owned())
}

#[cfg(not(unix))]
fn interface_name(_index: u32) -> Option<String> {
    None
}

/// The server's addresses as evdns_getaddrinfo() finds them, with evtdig's
/// hints: `-6` asks for IPv6, `-4` (the default) for IPv4, and `-4 -6`,
/// `-a` and `--resolv` for either. Never empty.
///
/// A failure is worded as evtdig words it, with evutil_gai_strerror()'s
/// text for the only two answers evdns gives: EAI_NONAME for a name that
/// does not exist, EAI_FAIL for anything else. RIPE stored both from
/// firmware 5130 on 2026-10-13: `"rcz.ns.ua nodename nor servname provided,
/// or not known"` (msm 218956432, probe 1017360) and `"ns1.criteo.com.
/// non-recoverable failure in name resolution"` (msm 211294130, probe
/// 1008241). The system resolver's own words ("Name or service not known")
/// are not what the reference reports.
async fn resolve(server: &str, port: u16, af: u8) -> Result<Vec<SocketAddr>, &'static str> {
    let family = |a: &SocketAddr| match af {
        4 => a.is_ipv4(),
        6 => a.is_ipv6(),
        _ => true,
    };
    match tokio::net::lookup_host((server, port)).await {
        Ok(addrs) => {
            let addrs: Vec<SocketAddr> = addrs.filter(family).collect();
            if !addrs.is_empty() {
                Ok(addrs)
            } else if server.parse::<IpAddr>().is_ok() {
                // A literal of the other family is no numeric host to evdns:
                // it asks the DNS for it as a name, which has no such TLD
                Err(EAI_NONAME)
            } else {
                // The name exists without an address of this family: the
                // A/AAAA query comes back empty, DNS_ERR_NODATA, EAI_FAIL
                Err(EAI_FAIL)
            }
        }
        Err(e) => Err(evdns_error(&e)),
    }
}

/// udp_dns_cb()'s error (evtdig.c:3025): the server as given, and why.
pub(crate) fn getaddrinfo_failed(server: &str, why: &str) -> String {
    format!("\"evdns_getaddrinfo\": \"{server} {why}\"")
}

/// What evdns would have answered where the system resolver failed with `e`.
pub(crate) fn evdns_error(e: &std::io::Error) -> &'static str {
    let text = e.to_string();
    if SYSTEM_EAI_NONAME.iter().any(|t| text.contains(t)) {
        EAI_NONAME
    } else {
        EAI_FAIL
    }
}

/// The family evtdig asks for: `--resolv` sets both -4 and -6, so it asks
/// each resolver whatever its family.
fn family(cfg: &DnsConfig) -> u8 {
    if cfg.resolv {
        0
    } else {
        cfg.af
    }
}

/// strerror(): an io::Error's text without Rust's " (os error N)".
fn strerror(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// The name to ask for: `--p_probe_id` puts `<probe id>.<time>.` in front
/// (mk_dns_buff(), evtdig.c:1033).
fn query_name(cfg: &DnsConfig, name: &str, probe_id: i64, time: i64) -> String {
    if cfg.prepend_probe_id {
        format!("{}.{}.{}", probe_id.max(0), time, name)
    } else {
        name.to_string()
    }
}

/// `--ttl`: have the kernel hand over each reply's TTL (IPv4) or hop limit
/// (IPv6), as tdig_send_query_callback() asks for it with IP_RECVTTL or
/// IPV6_RECVHOPLIMIT (evtdig.c:1320).
#[cfg(unix)]
fn ask_for_ttl(socket: &UdpSocket, v6: bool) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let (level, name) = if v6 {
        (libc::IPPROTO_IPV6, libc::IPV6_RECVHOPLIMIT)
    } else {
        (libc::IPPROTO_IP, libc::IP_RECVTTL)
    };
    let on: libc::c_int = 1;
    // SAFETY: an open descriptor, and an int option passed with its size
    let r = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            level,
            name,
            (&on as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if r == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn ask_for_ttl(_socket: &UdpSocket, _v6: bool) -> std::io::Result<()> {
    Ok(())
}

/// recv() with the TTL or hop limit [`ask_for_ttl`] asked for, read from
/// the control messages as process_reply() reads them (evtdig.c:1986):
/// IP_TTL / IPV6_HOPLIMIT, an int (Linux), or IP_RECVTTL, one byte (the
/// BSDs and macOS). None when there is none.
async fn recv_with_ttl(
    socket: &UdpSocket,
    buf: &mut [u8],
) -> std::io::Result<(usize, Option<i32>)> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = socket.as_raw_fd();
        socket
            .async_io(tokio::io::Interest::READABLE, || recvmsg_ttl(fd, &mut *buf))
            .await
    }
    #[cfg(not(unix))]
    {
        socket.recv(buf).await.map(|n| (n, None))
    }
}

#[cfg(unix)]
fn recvmsg_ttl(fd: std::os::fd::RawFd, buf: &mut [u8]) -> std::io::Result<(usize, Option<i32>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // Room for a few control messages, aligned as a cmsghdr must be
    let mut control = [0u64; 16];
    // SAFETY: msghdr is plain data; all-zero is a valid empty one
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    // SAFETY: msg points at iov and control, both alive and sized above
    let n = unsafe { libc::recvmsg(fd, &mut msg, 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut ttl = None;
    // SAFETY: the CMSG_* walk stays within msg_controllen, which recvmsg()
    // set; each header is read only after CMSG_FIRSTHDR/NXTHDR vouched for it
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            let hdr = &*cmsg;
            if hdr.cmsg_len == 0 {
                break;
            }
            let ours = (hdr.cmsg_level == libc::IPPROTO_IP
                && (hdr.cmsg_type == libc::IP_TTL || hdr.cmsg_type == libc::IP_RECVTTL))
                || (hdr.cmsg_level == libc::IPPROTO_IPV6 && hdr.cmsg_type == libc::IPV6_HOPLIMIT);
            if ours {
                let data = libc::CMSG_DATA(cmsg);
                let len = (hdr.cmsg_len as usize).saturating_sub(data as usize - cmsg as usize);
                ttl = if len >= std::mem::size_of::<libc::c_int>() {
                    Some(std::ptr::read_unaligned(data.cast::<libc::c_int>()))
                } else if len >= 1 {
                    Some(i32::from(*data))
                } else {
                    ttl
                };
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((n as usize, ttl))
}

/// One UDP query, as tdig_send_query_callback() and ready_callback() make
/// it: a connected socket, one datagram, and the first reply that is at
/// least a header long and carries our id; anything else is ignored until
/// `--timeout` runs out.
///
/// A server name with several addresses of the family gets the query at
/// each of them, from a socket of its own that the next one closes, and
/// the reply is awaited from the last alone, as the loop of
/// tdig_send_query_callback() does (evtdig.c:1255-1495). A failure on the
/// way stops there, reported against the address a query last went to
/// (`ressent`). Only the last address's send error is reported: the
/// reference would also list an earlier one beside the result.
///
/// Except with `--resolv`, an address atlas_check_addr() refuses gets no
/// query: `"reason" : "address not allowed"` with that address
/// (evtdig.c:1358-1375).
pub async fn udp_attempt(cfg: &DnsConfig, server: &str, name: &str, probe_id: i64) -> Attempt {
    let time = unix_now();
    match resolve(server, cfg.port, family(cfg)).await {
        Ok(addrs) => udp_query(cfg, server, &addrs, name, probe_id).await,
        Err(msg) => {
            let text = getaddrinfo_failed(server, msg);
            Attempt::failed(time, Dst::Name(server.into()), None, text, false)
        }
    }
}

/// [`udp_attempt`] once `server` has resolved to `addrs` (at least one).
pub(crate) async fn udp_query(
    cfg: &DnsConfig,
    server: &str,
    addrs: &[SocketAddr],
    name: &str,
    probe_id: i64,
) -> Attempt {
    let time = unix_now();
    // What printReply() would find if it stopped here
    let mut dst = Dst::Name(server.into());
    let mut src = None;
    let mut qbuf = None;
    let fail = |dst: &Dst, src, qbuf: &Option<Vec<u8>>, text: &str| Attempt {
        time,
        dst: dst.clone(),
        src,
        ttl: None,
        qbuf: qbuf.clone(),
        outcome: Outcome::Error {
            text: text.to_string(),
            retry: false,
        },
    };
    // The last address's socket, query id, send time and send error
    let mut last = None;
    for &addr in addrs {
        let any: SocketAddr = if addr.is_ipv4() {
            (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let socket = match UdpSocket::bind(any).await {
            Ok(s) => s,
            Err(e) => {
                let text = format!(" \"socket\" : \"socket failed {}\"", strerror(&e));
                return fail(&dst, src, &qbuf, &text);
            }
        };
        if cfg.ttl {
            // As the reference, whatever setsockopt() says (evtdig.c:1320)
            let _ = ask_for_ttl(&socket, addr.is_ipv6());
        }
        if !cfg.resolv && !address_allowed(&addr, cfg) {
            // "Trigger printing of dst_addr" (evtdig.c:1371)
            let dst = Dst::Udp {
                server: server.into(),
                addr,
            };
            return fail(&dst, src, &qbuf, " \"reason\" : \"address not allowed\"");
        }
        if let Err(e) = socket.connect(addr).await {
            let text = format!(" \"socket\" : \"connect failed {}\"", strerror(&e));
            return fail(&dst, src, &qbuf, &text);
        }
        if let Ok(local) = socket.local_addr() {
            src = Some(local);
        }
        let id = query_id();
        let qname = query_name(cfg, name, probe_id, time);
        let Some(query) = build_query(cfg, &qname, id, addr.is_ipv4()) else {
            return fail(
                &dst,
                src,
                &qbuf,
                " \"err\" : \"unable to format DNS query\"",
            );
        };
        let start = Instant::now();
        let sent = match socket.send(&query).await {
            Ok(n) if n == query.len() => Ok(()),
            Ok(_) => Err(std::io::Error::last_os_error()),
            Err(e) => Err(e),
        };
        dst = Dst::Udp {
            server: server.into(),
            addr,
        };
        let error = match sent {
            Ok(()) => {
                if cfg.qbuf {
                    qbuf = Some(query);
                }
                None
            }
            Err(e) => Some(format!(
                " \"senderror\" : \"AF {}, {}\"",
                strerror(&e),
                if addr.is_ipv4() {
                    "AF_INET"
                } else {
                    "NOT AF_INET"
                }
            )),
        };
        // Replacing it closes the previous address's socket, as the
        // loop's next round does
        last = Some((socket, id, start, error));
    }
    let Some((socket, id, start, error)) = last else {
        // resolve() gives none of these
        let text = getaddrinfo_failed(server, EAI_FAIL);
        return Attempt::failed(time, dst, None, text, false);
    };
    if let Some(text) = error {
        return fail(&dst, src, &qbuf, &text);
    }
    let deadline =
        tokio::time::Instant::from_std(start) + Duration::from_millis(u64::from(cfg.timeout_ms));
    let mut buf = vec![0u8; MAX_DNS_BUF_SIZE];
    let (outcome, ttl) = loop {
        let recv = async {
            if cfg.ttl {
                recv_with_ttl(&socket, &mut buf).await
            } else {
                socket.recv(&mut buf).await.map(|n| (n, None))
            }
        };
        match timeout_at(deadline, recv).await {
            Err(_) => break (Outcome::Timeout, None),
            // recvfail: an ICMP error on the connected socket, say
            Ok(Err(_)) => continue,
            // shortpkt, and martian (someone else's id)
            Ok(Ok((n, _))) if n < DNS_HEADER || buf[..2] != id.to_be_bytes() => continue,
            Ok(Ok((n, ttl))) => {
                let reply = Outcome::Reply {
                    wire: buf[..n].to_vec(),
                    rt: start.elapsed().as_secs_f64() * 1000.0,
                    qt: None,
                };
                break (reply, ttl);
            }
        }
    };
    Attempt {
        time,
        dst,
        src,
        ttl,
        qbuf,
        outcome,
    }
}

/// One TCP query, as tu_connect_to_name() and the tcp_* callbacks make it:
/// `rt` runs from before the connect, `qt` from when the query was written,
/// and the whole exchange has [`TCP_TIMEOUT`].
///
/// It connects to the first address of the family. One atlas_check_addr()
/// refuses is not connected to, `--resolv` or not (tcputil.c:262): it is
/// reported as `"TU_BAD_ADDR" : true` with that address, and tcp_reporterr()
/// asks again while `--retry` allows (evtdig.c:1612).
pub async fn tcp_attempt(cfg: &DnsConfig, server: &str, name: &str, probe_id: i64) -> Attempt {
    let time = unix_now();
    let addr = match resolve(server, cfg.port, family(cfg)).await {
        Ok(a) => a[0],
        Err(msg) => {
            let text = format!(" \"TUDNS\" : \"{msg}\"");
            return Attempt::failed(time, Dst::Name(server.into()), None, text, true);
        }
    };
    let dst = Dst::Tcp {
        server: server.into(),
        addr,
    };
    if !address_allowed(&addr, cfg) {
        // tcp_beforeconnect() never runs: xmit_time is tdig_start()'s
        let text = " \"TU_BAD_ADDR\" : true".to_string();
        return Attempt::failed(time, dst, None, text, true);
    }
    // tcp_beforeconnect()
    let time = unix_now();
    let start = Instant::now();
    let deadline = tokio::time::Instant::from_std(start) + TCP_TIMEOUT;
    let timed_out = |src| Attempt {
        time,
        dst: dst.clone(),
        src,
        ttl: None,
        qbuf: None,
        outcome: Outcome::Timeout,
    };
    let mut stream = match timeout_at(deadline, TcpStream::connect(addr)).await {
        Err(_) => return timed_out(None),
        Ok(Err(e)) => {
            let text = format!(" \"TUCONNECT\" : \"{}\"", strerror(&e));
            return Attempt::failed(time, dst, None, text, true);
        }
        Ok(Ok(s)) => s,
    };
    // tcp_connected()
    let src = stream.local_addr().ok();
    let id = query_id();
    let qname = query_name(cfg, name, probe_id, time);
    let Some(query) = build_query(cfg, &qname, id, cfg.af != 6) else {
        let text = " \"err\" : \"unable to format DNS query\"".to_string();
        return Attempt::failed(time, dst, src, text, false);
    };
    let mut wire = Vec::with_capacity(query.len() + 2);
    wire.extend_from_slice(&(query.len() as u16).to_be_bytes());
    wire.extend_from_slice(&query);
    let qbuf = cfg.qbuf.then(|| query.clone());
    if stream.write_all(&wire).await.is_err() {
        // A write error is no event evtdig reports: it waits for the timer
        tokio::time::sleep_until(deadline).await;
        return Attempt {
            qbuf,
            ..timed_out(src)
        };
    }
    let qstart = Instant::now();
    let read_err = |qbuf| Attempt {
        time,
        dst: dst.clone(),
        src,
        ttl: None,
        qbuf,
        outcome: Outcome::Error {
            text: " \"TU_READ_ERR\" : \"\"".into(),
            retry: true,
        },
    };
    let mut len = [0u8; 2];
    match timeout_at(deadline, stream.read_exact(&mut len)).await {
        Err(_) => {
            return Attempt {
                qbuf,
                ..timed_out(src)
            }
        }
        Ok(Err(_)) => return read_err(qbuf),
        Ok(Ok(_)) => {}
    }
    let size = u16::from_be_bytes(len) as usize;
    if size < DNS_HEADER {
        return Attempt {
            time,
            dst,
            src,
            ttl: None,
            qbuf,
            outcome: Outcome::Error {
                text: format!(" \"TCPREADSIZE\" :  \"reply too small, got {size}\""),
                retry: false,
            },
        };
    }
    let mut reply = vec![0u8; size];
    match timeout_at(deadline, stream.read_exact(&mut reply)).await {
        Err(_) => {
            return Attempt {
                qbuf,
                ..timed_out(src)
            }
        }
        Ok(Err(_)) => return read_err(qbuf),
        Ok(Ok(_)) => {}
    }
    let outcome = if reply[..2] == id.to_be_bytes() {
        Outcome::Reply {
            wire: reply,
            rt: start.elapsed().as_secs_f64() * 1000.0,
            qt: Some(qstart.elapsed().as_secs_f64() * 1000.0),
        }
    } else {
        // `n` there is the last evbuffer_remove() of at most 256 bytes
        let n = match size % 256 {
            0 => 256,
            r => r,
        };
        Outcome::Error {
            text: format!("  \"idmismatch\" : \"mismatch id from tcp fd {n}\""),
            retry: false,
        }
    };
    Attempt {
        time,
        dst,
        src,
        // process_reply() is UDP's: a TCP reply sets no "ttl"
        ttl: None,
        qbuf,
        outcome,
    }
}

/// The `"result"` object printReply() writes for a reply (evtdig.c:3823),
/// from `"rt"` to its closing brace.
///
/// It decodes only the first two answers, and of those only TXT and SOA:
/// any other answer (an A, say) is skipped and the reader has `abuf`. A
/// reply cut short ends the object where the cut is found, without closing
/// an open `answers` array, as the reference's `goto truncated` does.
pub fn write_result(s: &mut String, wire: &[u8], rt: f64, qt: Option<f64>, abuf: bool) {
    let _ = write!(s, ",\"result\" : {{ \"rt\" : {rt:.3},");
    if let Some(qt) = qt {
        let _ = write!(s, "\"qt\" : {qt:.3},");
    }
    let _ = write!(s, "\"size\" : {} ", wire.len());
    if abuf {
        let _ = write!(s, ", \"abuf\" : \"{}\"", BASE64.encode(wire));
    }
    write_header_and_answers(s, wire);
    s.push_str(" }");
}

fn write_header_and_answers(s: &mut String, wire: &[u8]) {
    let Ok((id, qd, an, ns, ar)) = parse_response(wire) else {
        return;
    };
    let _ = write!(
        s,
        ",\"ID\" : {id} , \"ANCOUNT\" : {an} , \"QDCOUNT\" : {qd} , \"NSCOUNT\" : {ns} , \
         \"ARCOUNT\" : {ar}"
    );
    let size = wire.len();
    let Some(len) = dns_namelen(wire, DNS_HEADER) else {
        return;
    };
    let mut offset = DNS_HEADER + len + 4;
    if offset > size || an == 0 {
        return;
    }
    let mut answers = 0;
    for _ in 0..an.min(2) {
        let Some((name, used)) = read_name(wire, offset) else {
            return;
        };
        offset += used;
        if offset + 10 > size {
            return;
        }
        let rr = &wire[offset..offset + 10];
        let rtype = u16::from_be_bytes([rr[0], rr[1]]);
        let ttl = u32::from_be_bytes([rr[4], rr[5], rr[6], rr[7]]);
        let rdlen = u16::from_be_bytes([rr[8], rr[9]]) as usize;
        offset += 10;
        match rtype {
            16 => {
                if offset + rdlen > size {
                    return;
                }
                s.push_str(if answers == 0 {
                    ",\"answers\" : [ {"
                } else {
                    ", {"
                });
                answers += 1;
                let _ = write!(s, "\"TYPE\" : \"TXT\" , \"NAME\" : \"{name}\" , ");
                write_txt(s, &wire[offset..offset + rdlen]);
                offset += rdlen;
                s.push('}');
            }
            6 => {
                // The names are read from the RDATA's start, not bounded by
                // its length
                let Some((mname, used)) = read_name(wire, offset) else {
                    return;
                };
                offset += used;
                let Some((rname, used)) = read_name(wire, offset) else {
                    return;
                };
                offset += used;
                if offset + 5 * 4 > size {
                    return;
                }
                s.push_str(if answers == 0 {
                    ",\"answers\" : [ { "
                } else {
                    ",{ "
                });
                answers += 1;
                let serial = u32::from_be_bytes([
                    wire[offset],
                    wire[offset + 1],
                    wire[offset + 2],
                    wire[offset + 3],
                ]);
                let _ = write!(
                    s,
                    "\"TYPE\" : \"SOA\" , \"NAME\" : \"{name}.\" , \"TTL\" : {ttl} , \
                     \"MNAME\" : \"{mname}.\" , \"RNAME\" : \"{rname}.\" , \"SERIAL\" : {serial}"
                );
                offset += 5 * 4;
                s.push_str(" } ");
            }
            _ => {
                if offset + rdlen > size {
                    return;
                }
                offset += rdlen;
            }
        }
    }
    if answers > 0 {
        s.push_str(" ]");
    }
}

/// print_txt_json() (evtdig.c:841): the TXT strings, `"` and `\` and
/// anything outside ' '..'~' written as `\\` and three decimal digits.
fn write_txt(s: &mut String, rdata: &[u8]) {
    s.push_str("\"RDATA\" : [ \"");
    let mut left: i32 = -1;
    for &b in rdata {
        if left == 0 {
            s.push_str("\", \"");
            left = -1;
        }
        if left == -1 {
            left = i32::from(b);
            continue;
        }
        push_byte(s, b);
        left -= 1;
    }
    s.push_str("\" ] ");
}

fn push_byte(s: &mut String, b: u8) {
    if b == b'"' || b == b'\\' || !(b' '..=b'~').contains(&b) {
        let _ = write!(s, "\\\\{b:03}");
    } else {
        s.push(b as char);
    }
}

/// ReadName() (evtdig.c:4057): the name at `offset` without its final dot
/// ("" for the root), and the bytes it takes there. None where the
/// reference reports an error and the result is cut short.
///
/// The reference copies label bytes verbatim; here a `"`, a `\` or a byte
/// outside ' '..'~' is written as TXT strings are, so a hostile name can't
/// break the line's JSON (or its UTF-8).
pub fn read_name(base: &[u8], mut offset: usize) -> Option<(String, usize)> {
    let size = base.len();
    let mut name = String::new();
    let (mut count, mut p, mut jumped, mut jumps) = (0usize, 0usize, false, 0u32);
    while offset < size && base[offset] != 0 {
        let len = base[offset] as usize;
        if len & 0xc0 != 0 {
            if len & 0xc0 != 0xc0 {
                return None;
            }
            // The reply buffer is zeroed past the reply
            let next = ((len & 0x3f) << 8) | *base.get(offset + 1).unwrap_or(&0) as usize;
            if next >= size || jumps > 256 {
                return None;
            }
            offset = next;
            if !jumped {
                count += 2;
            }
            jumped = true;
            jumps += 1;
            continue;
        }
        if offset + len + 1 > size || p + len + 1 > 255 {
            return None;
        }
        if p > 0 {
            name.push('.');
        }
        for &b in &base[offset + 1..offset + 1 + len] {
            push_byte(&mut name, b);
        }
        p += len + 1;
        offset += len + 1;
        if !jumped {
            count += len + 1;
        }
    }
    if !jumped {
        count += 1;
    }
    Some((name, count))
}

/// dns_namelen() (evtdig.c:4162): the length of the question's name.
fn dns_namelen(base: &[u8], start: usize) -> Option<usize> {
    let mut offset = start;
    while offset < base.len() {
        let len = base[offset] as usize;
        if len & 0xc0 != 0 {
            if len & 0xc0 != 0xc0 {
                return None;
            }
            offset += 1;
            break;
        }
        if offset + len + 1 > base.len() {
            return None;
        }
        offset += len + 1;
        if len == 0 {
            break;
        }
    }
    Some(offset - start)
}

/// The result's `"result"` object for a reply, for fuzzing.
pub fn format_result(wire: &[u8]) -> String {
    let mut s = String::new();
    write_result(&mut s, wire, 0.0, None, true);
    s
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> Vec<u8> {
        BASE64.decode(s).unwrap()
    }

    fn cfg() -> DnsConfig {
        DnsConfig::default()
    }

    /// Header flags (byte 2, byte 3), ARCOUNT, and the OPT record if any.
    fn flags(q: &[u8]) -> (u8, u8, u16) {
        (q[2], q[3], u16::from_be_bytes([q[10], q[11]]))
    }

    #[test]
    fn names_encode_as_change_to_dns_name_format_does() {
        assert_eq!(encode_name(".").unwrap(), vec![0]);
        assert_eq!(
            encode_name("version.bind.").unwrap(),
            b"\x07version\x04bind\x00".to_vec()
        );
        // No trailing dot, an empty label, a label over 63: no query
        assert!(encode_name("version.bind").is_none());
        assert!(encode_name("a..b.").is_none());
        assert!(encode_name(&format!("{}.", "x".repeat(64))).is_none());
        assert!(encode_name(&format!("{}.", "x".repeat(63))).is_some());
    }

    /// `$p` is the probe id, `$t` the time, `$r` 16 hex digits
    /// (libbb/atlas_name_macro.c); twin 2026-10-13, 70502079 asked for
    /// 1e56f96a6f8a6d9d-1791871387-1018212.rpkitest6.nlnetlabs.nl.
    #[test]
    fn macros_expand_as_atlas_name_macro_does() {
        let before = unix_now();
        let name = expand_name_macros("$r-$t-$p.rpkitest6.nlnetlabs.nl.", 1018212).unwrap();
        let (r, rest) = name.split_once('-').unwrap();
        let (t, rest) = rest.split_once('-').unwrap();
        assert_eq!(r.len(), 16, "{name}");
        assert!(r
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        let t: i64 = t.parse().unwrap();
        assert!(t >= before && t <= unix_now(), "{name}");
        assert_eq!(rest, "1018212.rpkitest6.nlnetlabs.nl.");

        assert_eq!(
            expand_name_macros("$p.atlas.ripe.net.", 1018212).unwrap(),
            "1018212.atlas.ripe.net."
        );
        // Unknown: get_probe_id() is -1
        assert_eq!(expand_name_macros("$p.x.", -1).unwrap(), "-1.x.");
        // Each $r is drawn afresh
        assert_ne!(
            expand_name_macros("$r.", 1).unwrap(),
            expand_name_macros("$r.", 1).unwrap()
        );
        // Anything else after a $ fails
        assert!(expand_name_macros("$x.example.", 1).is_none());
        assert!(expand_name_macros("example.$", 1).is_none());
        assert_eq!(expand_name_macros("plain.", 1).unwrap(), "plain.");
    }

    /// `--soa .` (11001): no RD, no OPT. The reference's query bytes from
    /// the twin capture, id aside.
    #[test]
    fn a_soa_query_has_no_rd_and_no_edns() {
        let c = DnsConfig {
            qtype: 6,
            qclass: 1,
            ..cfg()
        };
        let q = build_query(&c, ".", 0x5e8d, false).unwrap();
        assert_eq!(
            q,
            b"\x5e\x8d\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x06\x00\x01"
        );
    }

    /// `-r` is version.server CH TXT, `-b` version.bind, `-h` hostname.bind,
    /// `-i` id.server: no RD, no OPT.
    #[test]
    fn a_chaos_query_asks_txt_ch_without_rd() {
        let q = build_query(&cfg(), "version.server.", 1, false).unwrap();
        assert_eq!(flags(&q), (0, 0, 0));
        assert_eq!(&q[12..], b"\x07version\x06server\x00\x00\x10\x00\x03");
    }

    /// `-e 512 -n --qbuf --a paypal.com.` (51457586): EDNS 512 with an empty
    /// NSID option. The reference's qbuf from the twin, byte for byte.
    #[test]
    fn edns_512_with_nsid_is_the_references_query() {
        let c = DnsConfig {
            qtype: 1,
            qclass: 1,
            edns0: 512,
            nsid: true,
            qbuf: true,
            ..cfg()
        };
        let reference = b64("F7kAAAABAAAAAAABBnBheXBhbANjb20AAAEAAQAAKQIAAAAAAAAEAAMAAA==");
        assert_eq!(
            build_query(&c, "paypal.com.", 0x17b9, false).unwrap(),
            reference
        );
    }

    /// `-e 512` alone is no reason for an OPT (only above 512 is); `-e 4096`
    /// is, and `-d` sets DO.
    #[test]
    fn edns_goes_out_only_when_something_needs_it() {
        let c = DnsConfig {
            qtype: 28,
            qclass: 1,
            edns0: 512,
            ..cfg()
        };
        assert_eq!(flags(&build_query(&c, "x.", 1, true).unwrap()).2, 0);

        let c = DnsConfig { edns0: 4096, ..c };
        let q = build_query(&c, "x.", 1, true).unwrap();
        assert_eq!(flags(&q).2, 1);
        assert_eq!(
            &q[q.len() - 11..],
            b"\x00\x00\x29\x10\x00\x00\x00\x00\x00\x00\x00"
        );

        let c = DnsConfig {
            edns0: 512,
            dnssec: true,
            ..c
        };
        let q = build_query(&c, "x.", 1, true).unwrap();
        assert_eq!(
            &q[q.len() - 11..],
            b"\x00\x00\x29\x02\x00\x00\x00\x80\x00\x00\x00"
        );
    }

    /// `--resolv` and `-R` set RD; `--ad` and `--cd` their bits.
    #[test]
    fn rd_comes_from_resolv_or_dash_r() {
        assert_eq!(flags(&build_query(&cfg(), "x.", 1, true).unwrap()).0, 0);
        let rd = DnsConfig { rd: true, ..cfg() };
        assert_eq!(flags(&build_query(&rd, "x.", 1, true).unwrap()).0, 1);
        let resolv = DnsConfig {
            resolv: true,
            ..cfg()
        };
        assert_eq!(flags(&build_query(&resolv, "x.", 1, true).unwrap()).0, 1);
        let adcd = DnsConfig {
            ad: true,
            cd: true,
            ..cfg()
        };
        assert_eq!(flags(&build_query(&adcd, "x.", 1, true).unwrap()).1, 0x30);
    }

    /// 30001 (`-4 --resolv --qbuf --type 1 --class 1 --query
    /// $p.atlas.ripe.net.$r.`) and 70502079 (`-6 --resolv -e 512 --qbuf
    /// --aaaa $r-$t-$p.rpkitest6.nlnetlabs.nl. -R`): RD, no OPT. The
    /// reference's qbufs from the twin.
    #[test]
    fn resolv_queries_are_the_references() {
        let c = DnsConfig {
            resolv: true,
            qtype: 1,
            qclass: 1,
            qbuf: true,
            ..cfg()
        };
        let reference =
            b64("i0kBAAABAAAAAAAABzEwMTgyMTIFYXRsYXMEcmlwZQNuZXQQYTdhMjM3OWViZTM2MmFiMwAAAQAB");
        let q = build_query(&c, "1018212.atlas.ripe.net.a7a2379ebe362ab3.", 0x8b49, true);
        assert_eq!(q.unwrap(), reference);

        let c = DnsConfig {
            resolv: true,
            rd: true,
            qtype: 28,
            qclass: 1,
            edns0: 512,
            qbuf: true,
            ..cfg()
        };
        let reference = b64("XJEBAAABAAAAAAAAIzFlNTZmOTZhNmY4YTZkOWQtMTc5MTg3MTM4Ny0xMDE4MjEyCXJwa2l0ZXN0NglubG5ldGxhYnMCbmwAABwAAQ==");
        let name = "1e56f96a6f8a6d9d-1791871387-1018212.rpkitest6.nlnetlabs.nl.";
        assert_eq!(build_query(&c, name, 0x5c91, true).unwrap(), reference);
    }

    /// Every query shape the reference sent in the twin run, as the target
    /// saw it (targets.jsonl, ids zeroed), rebuilt from that line's options.
    #[test]
    fn every_scenario_query_is_the_references() {
        let soa = DnsConfig {
            qtype: 6,
            qclass: 1,
            ..cfg()
        };
        let a = DnsConfig {
            qtype: 1,
            qclass: 1,
            ..cfg()
        };
        let aaaa = DnsConfig {
            qtype: 28,
            ..a.clone()
        };
        // -e 512 -n --qbuf
        let nsid = |c: &DnsConfig| DnsConfig {
            nsid: true,
            qbuf: true,
            ..c.clone()
        };
        let resolv = |c: &DnsConfig| DnsConfig {
            resolv: true,
            af: 0,
            qbuf: true,
            ..c.clone()
        };
        let cases: [(&str, DnsConfig, &str, &str); 12] = [
            ("11001 --soa .", soa.clone(), ".", "0000000000010000000000000000060001"),
            ("11101 -t --soa .", DnsConfig { tcp: true, ..soa }, ".", "0000000000010000000000000000060001"),
            ("11201 -b", cfg(), "version.bind.", "0000000000010000000000000776657273696f6e0462696e640000100003"),
            ("11301 -h", cfg(), "hostname.bind.", "00000000000100000000000008686f73746e616d650462696e640000100003"),
            ("11401 -i", cfg(), "id.server.", "000000000001000000000000026964067365727665720000100003"),
            ("11501 -r", cfg(), "version.server.", "0000000000010000000000000776657273696f6e067365727665720000100003"),
            ("156155996 -e 512 -i", DnsConfig { edns0: 512, ..cfg() }, "id.server.", "000000000001000000000000026964067365727665720000100003"),
            ("51457586 -t -e 512 -n --qbuf --a", DnsConfig { tcp: true, ..nsid(&a) }, "paypal.com.", "0000000000010000000000010670617970616c03636f6d0000010001000029020000000000000400030000"),
            ("51457594 -e 512 -n --qbuf --aaaa", nsid(&aaaa), "nytimes.com.", "000000000001000000000001076e7974696d657303636f6d00001c0001000029020000000000000400030000"),
            ("158899651 -e 512 -n --qbuf --aaaa $p.…$r.", nsid(&aaaa), "1018212.ripe-tld-experiment.ad58e4a320fa21b4.", "000000000001000000000001073130313832313213726970652d746c642d6578706572696d656e74106164353865346133323066613231623400001c0001000029020000000000000400030000"),
            ("30001 --resolv --qbuf --type 1 --class 1", resolv(&a), "1018212.atlas.ripe.net.a7a2379ebe362ab3.", "00000100000100000000000007313031383231320561746c61730472697065036e657410613761323337396562653336326162330000010001"),
            ("70502079 --resolv -e 512 --qbuf --aaaa -R", DnsConfig { rd: true, ..resolv(&aaaa) }, "1e56f96a6f8a6d9d-1791871387-1018212.rpkitest6.nlnetlabs.nl.", "00000100000100000000000023316535366639366136663861366439642d313739313837313338372d313031383231320972706b697465737436096e6c6e65746c616273026e6c00001c0001"),
        ];
        for (line, c, name, hex) in cases {
            let q = build_query(&c, name, 0, false).unwrap();
            let got: String = q.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(got, hex, "{line}");
        }
    }

    #[test]
    fn other_edns_options_go_in_order() {
        let c = DnsConfig {
            nsid: true,
            client_subnet: true,
            edns_option: 65001,
            edns_version: 1,
            edns_flags: 0x0001,
            ..cfg()
        };
        let q = build_query(&c, ".", 1, false).unwrap();
        let opt = &q[q.len() - (11 + 4 + 8 + 4)..];
        assert_eq!(&opt[..11], b"\x00\x00\x29\x02\x00\x00\x01\x00\x01\x00\x10");
        assert_eq!(
            &opt[11..],
            b"\x00\x03\x00\x00\x00\x08\x00\x04\x00\x02\x00\x00\xfd\xe9\x00\x00"
        );
    }

    #[test]
    fn resolvers_are_read_as_get_local_resolvers_reads_them() {
        let dir = std::env::temp_dir().join(format!("starla-resolv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("resolv.conf");
        std::fs::write(
            &path,
            "# atlas-twin\nsearch example\nnameserver 172.30.74.20\n\tnameserver\t2001:db8:7769:6e00::20 # x\n#nameserver 9.9.9.9\nnameserver\nnameserver 10.0.0.3\nnameserver 10.0.0.4\n",
        )
        .unwrap();
        assert_eq!(
            read_resolv_conf(&path),
            vec!["172.30.74.20", "2001:db8:7769:6e00::20", "10.0.0.3"]
        );
        assert!(read_resolv_conf(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The SOA answer the reference wrote for 11001, from its abuf.
    #[test]
    fn a_soa_answer_is_written_field_by_field() {
        let abuf = b64("Xo2EAAABAAEAAAAAAAAGAAEAAAYAAQAAASwAQAFhDHJvb3Qtc2VydmVycwNuZXQABW5zdGxkDHZlcmlzaWduLWdycwNjb20AeMPaNAAABwgAAAOEAAk6gAABUYA=");
        let mut s = String::new();
        write_header_and_answers(&mut s, &abuf);
        assert_eq!(
            s,
            ",\"ID\" : 24205 , \"ANCOUNT\" : 1 , \"QDCOUNT\" : 1 , \"NSCOUNT\" : 0 , \
             \"ARCOUNT\" : 0,\"answers\" : [ { \"TYPE\" : \"SOA\" , \"NAME\" : \".\" , \
             \"TTL\" : 300 , \"MNAME\" : \"a.root-servers.net.\" , \
             \"RNAME\" : \"nstld.verisign-grs.com.\" , \"SERIAL\" : 2026101300 }  ]"
        );
    }

    /// TXT names lose their final dot (11201: "version.bind").
    #[test]
    fn a_txt_answer_names_without_the_final_dot() {
        let abuf = b64("sPqEAAABAAEAAAAAB3ZlcnNpb24EYmluZAAAEAADwAwAEAADAAAAAAALCmF0bGFzLXR3aW4=");
        let mut s = String::new();
        write_header_and_answers(&mut s, &abuf);
        assert!(
            s.ends_with(
                "\"ARCOUNT\" : 0,\"answers\" : [ {\"TYPE\" : \"TXT\" , \
                 \"NAME\" : \"version.bind\" , \"RDATA\" : [ \"atlas-twin\" ] } ]"
            ),
            "{s}"
        );
    }

    /// An A answer is not decoded (51457586, 30001): the reader has abuf.
    #[test]
    fn an_a_answer_is_not_decoded() {
        let abuf = b64("F7mEAAABAAEAAAABBnBheXBhbANjb20AAAEAAcAMAAEAAQAAASwABKweShQAACkE0AAAAAAADgADAAphdGxhcy10d2lu");
        let mut s = String::new();
        write_header_and_answers(&mut s, &abuf);
        assert_eq!(
            s,
            ",\"ID\" : 6073 , \"ANCOUNT\" : 1 , \"QDCOUNT\" : 1 , \"NSCOUNT\" : 0 , \
             \"ARCOUNT\" : 1"
        );
    }

    #[test]
    fn txt_strings_are_escaped_as_print_txt_json_escapes_them() {
        let mut s = String::new();
        write_txt(&mut s, b"\x03a\"b\x00\x02\\\xff");
        assert_eq!(
            s,
            "\"RDATA\" : [ \"a\\\\034b\", \"\", \"\\\\092\\\\255\" ] "
        );
    }

    /// Only the first two answers are looked at.
    #[test]
    fn at_most_two_answers_are_decoded() {
        let mut m =
            b"\x00\x01\x84\x00\x00\x01\x00\x03\x00\x00\x00\x00\x01x\x00\x00\x10\x00\x03".to_vec();
        for t in [b"one", b"two", b"thr"] {
            m.extend_from_slice(b"\xc0\x0c\x00\x10\x00\x03\x00\x00\x00\x00\x00\x04\x03");
            m.extend_from_slice(t);
        }
        let mut s = String::new();
        write_header_and_answers(&mut s, &m);
        assert!(s.contains("\"one\" ] }, {\"TYPE\""), "{s}");
        assert!(s.contains("\"two\" ] } ]") && !s.contains("thr"), "{s}");
    }

    /// A reply cut inside an answer ends there, as `goto truncated` does.
    #[test]
    fn a_truncated_reply_stops_where_it_is_cut() {
        let abuf = b64("sPqEAAABAAEAAAAAB3ZlcnNpb24EYmluZAAAEAADwAwAEAADAAAAAAALCmF0bGFzLXR3aW4=");
        let mut s = String::new();
        write_header_and_answers(&mut s, &abuf[..abuf.len() - 3]);
        assert!(s.ends_with("\"ARCOUNT\" : 0"), "{s}");
        let mut s = String::new();
        write_header_and_answers(&mut s, &abuf[..5]);
        assert!(s.is_empty());
    }

    /// atlas_check_addr()'s prefixes, both sides of each edge.
    #[test]
    fn addresses_are_refused_as_atlas_check_addr_refuses_them() {
        let cases = [
            ("127.0.0.1", true),
            ("126.255.255.255", false),
            ("10.0.0.1", true),
            ("11.0.0.1", false),
            ("172.16.0.1", true),
            ("172.31.255.255", true),
            ("172.32.0.0", false),
            ("172.15.255.255", false),
            ("192.168.1.1", true),
            ("192.169.0.0", false),
            ("169.254.1.1", true),
            ("224.0.0.1", true),
            ("239.255.255.255", true),
            ("240.0.0.1", false),
            ("100.64.0.1", false),
            ("193.0.14.129", false),
            ("::1", true),
            ("::", false),
            ("::2", false),
            ("::ffff:193.0.14.129", true),
            ("::fffe:0:0", false),
            ("fd7a:115c:a1e0::53", true),
            ("fc00::1", true),
            ("fe80::1", true),
            ("ff02::1", true),
            ("e000::", true),
            ("dfff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", false),
            ("2001:7fd::1", false),
            ("64:ff9b::c000:e81", false),
        ];
        let checked = DnsConfig::default();
        let open = DnsConfig {
            disable_check_addr: true,
            ..cfg()
        };
        for (ip, bad) in cases {
            let addr = SocketAddr::new(ip.parse().unwrap(), 53);
            assert_eq!(refused(addr.ip()), bad, "{ip}");
            if std::env::var("ATLAS_DISABLE_CHECK_ADDR").is_err() {
                assert_eq!(address_allowed(&addr, &checked), !bad, "{ip}");
            }
            assert!(address_allowed(&addr, &open), "{ip}");
        }
    }

    /// The loopback interface: its name and index
    fn lo() -> (&'static str, u32) {
        let name = if cfg!(target_os = "linux") {
            "lo"
        } else {
            "lo0"
        };
        let c = std::ffi::CString::new(name).unwrap();
        // SAFETY: a NUL-terminated name
        let index = unsafe { libc::if_nametoindex(c.as_ptr()) };
        assert_ne!(index, 0, "{name}");
        (name, index)
    }

    /// getnameinfo(NI_NUMERICHOST) as glibc writes it: a link-local
    /// (unicast or multicast) address with a scope gets its interface's
    /// name, anything else with a scope the number.
    #[test]
    fn addresses_are_written_as_getnameinfo_writes_them() {
        let (lo, index) = lo();
        let v6 = |ip: &str, scope: u32| {
            SocketAddr::V6(std::net::SocketAddrV6::new(
                ip.parse().unwrap(),
                53,
                0,
                scope,
            ))
        };
        assert_eq!(
            numeric_host(&"193.0.14.129:53".parse().unwrap()),
            "193.0.14.129"
        );
        assert_eq!(numeric_host(&v6("2001:7fd::1", 0)), "2001:7fd::1");
        assert_eq!(numeric_host(&v6("fe80::1", 0)), "fe80::1");
        assert_eq!(numeric_host(&v6("fe80::1", index)), format!("fe80::1%{lo}"));
        assert_eq!(
            numeric_host(&v6("fe80::b2db:876:1abb:ef37", index)),
            format!("fe80::b2db:876:1abb:ef37%{lo}")
        );
        assert_eq!(numeric_host(&v6("ff02::1", index)), format!("ff02::1%{lo}"));
        // No such interface, or not link-local: the number
        assert_eq!(numeric_host(&v6("fe80::1", 4242424)), "fe80::1%4242424");
        assert_eq!(
            numeric_host(&v6("2001:7fd::1", index)),
            format!("2001:7fd::1%{index}")
        );
        assert_eq!(
            numeric_host(&v6("ff05::2", index)),
            format!("ff05::2%{index}")
        );
        // Past INET6_ADDRSTRLEN: EAI_OVERFLOW, the address alone
        let long = "2001:1234:5678:9abc:def0:1234:5678:9abc";
        assert_eq!(numeric_host(&v6(long, 4242424)), long);
        assert_eq!(numeric_host(&v6(long, 42)), format!("{long}%42"));
    }

    #[test]
    fn names_with_odd_bytes_stay_valid_json() {
        let (name, used) = read_name(b"\x03a\"\xff\x00", 0).unwrap();
        assert_eq!((name.as_str(), used), ("a\\\\034\\\\255", 5));
        // A pointer loop is an error, as is a pointer past the end
        assert!(read_name(b"\xc0\x00", 0).is_none());
        assert!(read_name(b"\xc0\x09", 0).is_none());
        assert!(read_name(b"\x40", 0).is_none());
    }
}
