//! Target name lookups as eperd does them.
//!
//! Every eperd measurement looks its target up when it starts, through
//! libevent's evdns_getaddrinfo with the family its -4/-6 asked for
//! (ping.c:1793-1798, traceroute.c:4969). eperd sets evdns up from the
//! probe's resolv.conf and hosts file (eperd.c:349-371, DNS_OPTIONS_ALL), and
//! evdns randomises the letter case of every name it asks for (DNS 0x20:
//! `randomize-case`, on by default, evdns.c:4013, 2824-2836). Starla used
//! the libc resolver instead: for a -6 measurement it asked for A and AAAA,
//! in lower case, and reported the address where the reference reports the
//! name and the time the lookup took. On the twin of 2026-10-13 the
//! reference asked one `AAAA yOUTuBE.COM.` where Starla asked `A
//! youtube.com.` and `AAAA youtube.com.`, and reported
//! `"dst_name":"bits.wikimedia.org", "ttr":1.030459` where Starla wrote the
//! address twice.
//!
//! This is evdns's stub resolver, cut to what eperd's measurements use: the
//! hosts file first (evdns.c:4794), then one query per family asked for
//! (A for -4, AAAA for -6, both otherwise; evdns.c:4833-4854) to the
//! resolv.conf servers in turn, with resolv.conf's search list, ndots,
//! timeout and attempts. A reply is matched to its query by ID from any
//! server's socket, with the question's name compared regardless of case
//! (evdns.c:1098-1104); every A or AAAA record of the answer section counts
//! (evdns.c:1127-1183). As in evdns, each server has one socket for as long
//! as resolv.conf lists it (one source port per server), and a server that
//! fails is passed over until a probe query finds it up again
//! (nameserver_pick, nameserver_failed, nameserver_send_probe).
//!
//! Windows has no resolv.conf: there the servers and search list are the
//! OS's ([`ResolvConf::system`]).

use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::{DNSClass, Name, RData, RecordType};
use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Notify, OnceCell};
use tracing::debug;

/// eperd's resolv.conf (eperd.c:309-324, RESOLV_CONF).
#[cfg(not(windows))]
const RESOLV_CONF: &str = "/etc/resolv.conf";

/// evdns's hosts file (evdns.c:3685-3688); on Windows the system's.
fn hosts_path() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        std::path::PathBuf::from(root).join(r"System32\drivers\etc\hosts")
    }
    #[cfg(not(windows))]
    {
        "/etc/hosts".into()
    }
}

/// What a result says about its target: the name as the controller wrote
/// it and, when it had to be resolved, how long that took.
#[derive(Debug, Clone, PartialEq)]
pub struct DstName {
    pub name: String,
    /// Time to resolve in milliseconds; None for an address literal, for
    /// which the reference reports no "ttr".
    pub ttr: Option<f64>,
}

impl DstName {
    /// `"dst_name":"…"` and, for a name, `, "ttr":…` with %f's six
    /// decimals, as ping.c:253-269 and traceroute.c:381-397 write them: the
    /// reference prints "ttr" whenever getaddrinfo(AI_NUMERICHOST) does not
    /// take the target as an address.
    pub fn fields(&self) -> String {
        match self.ttr {
            Some(ttr) => format!("\"dst_name\":\"{}\", \"ttr\":{:.6}", self.name, ttr),
            None => format!("\"dst_name\":\"{}\"", self.name),
        }
    }
}

/// Why a lookup found no address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupError {
    /// The server said the name does not exist: EVUTIL_EAI_NONAME
    /// (evdns.c:4386-4387).
    NoName,
    /// Anything else (no records of the type, timeouts, SERVFAIL, REFUSED,
    /// truncation, a malformed reply, no server): EVUTIL_EAI_FAIL
    /// (evdns.c:4388-4389).
    Fail,
    /// The hosts file has the name, but no address of the family asked for:
    /// an empty answer (evdns.c:4729-4732) that ping.c reports nothing for
    /// (PING_ERR_DNS_NO_ADDR, which ping_cb ignores).
    NoAddress,
}

impl fmt::Display for LookupError {
    /// evutil_gai_strerror's text (evutil.c:1657-1666), which ping.c puts
    /// after "dns resolution failed: ".
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LookupError::NoName => "nodename nor servname provided, or not known",
            LookupError::Fail => "non-recoverable failure in name resolution",
            LookupError::NoAddress => "no address associated with nodename",
        })
    }
}

impl std::error::Error for LookupError {}

/// Whether eperd measures `addr`: atlas_check_addr
/// (libbb/atlas_check_addr.c:8-110), which ping, traceroute and the other
/// measurements apply to the address their lookup gave, a literal's too
/// (evdns_getaddrinfo hands it to dns_cb like any answer; ping.c:1706,
/// traceroute.c:4886). It refuses IPv4 127/8, 10/8, 172.16/12,
/// 192.168/16, 169.254/16 and 224/4, and IPv6 ::1, ::ffff:0:0/96 and
/// e000::/3 (ULA, link-local and multicast all start there), unless
/// ATLAS_DISABLE_CHECK_ADDR=yes is in the environment. Starla measured any
/// address, its own LAN's too.
pub fn address_allowed(addr: IpAddr) -> bool {
    std::env::var("ATLAS_DISABLE_CHECK_ADDR").is_ok_and(|v| v == "yes") || !refused(addr)
}

/// atlas_check_addr's bad_ipv4 and bad_ipv6 prefixes.
fn refused(addr: IpAddr) -> bool {
    const BAD_IPV4: [(u32, u32); 6] = [
        (0x7F00_0000, 8),
        (0x0A00_0000, 8),
        (0xAC10_0000, 12),
        (0xC0A8_0000, 16),
        (0xA9FE_0000, 16),
        (0xE000_0000, 4),
    ];
    const BAD_IPV6: [(u128, u32); 3] = [(1, 128), (0xFFFF_0000_0000, 96), (0xE000 << 112, 3)];
    match addr {
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

/// A target looked up: what the result says about it, and the address to
/// measure.
#[derive(Debug, Clone)]
pub struct Lookup {
    pub dst_name: DstName,
    pub addr: Result<IpAddr, LookupError>,
}

/// Look `target` up as eperd does for a measurement with `af` (4 for -4, 6
/// for -6, anything else for neither), with the probe's resolv.conf and
/// hosts file as they are now.
pub async fn lookup(target: &str, af: u8) -> Lookup {
    Resolver::system().lookup(target, af).await
}

/// What evdns takes from resolv.conf (evdns.c:3590-3634).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvConf {
    pub nameservers: Vec<SocketAddr>,
    pub search: Vec<String>,
    pub ndots: usize,
    /// How long to wait for each transmission's reply
    pub timeout: Duration,
    /// Transmissions per query before it times out (evdns's
    /// global_max_retransmits, `options attempts:`)
    pub attempts: u32,
    pub randomize_case: bool,
    /// Retransmissions in a row to a server that may time out before it
    /// counts as down (global_max_nameserver_timeout, `max-timeouts:`)
    pub max_timeouts: u32,
    /// How long after a server goes down it is probed, the first time
    /// (global_nameserver_probe_initial_timeout, `initial-probe-timeout:`)
    pub probe_timeout: Duration,
}

impl Default for ResolvConf {
    /// evdns_base_new's defaults (evdns.c:4007-4017, 3118).
    fn default() -> Self {
        Self {
            nameservers: Vec::new(),
            search: Vec::new(),
            ndots: 1,
            timeout: Duration::from_secs(5),
            attempts: 3,
            randomize_case: true,
            max_timeouts: 3,
            probe_timeout: Duration::from_secs(10),
        }
    }
}

impl ResolvConf {
    /// The probe's: resolv.conf, read as eperd's evdns reads it on Linux.
    /// Windows has none (evdns's 127.0.0.1 when it is missing would leave
    /// every lookup there to time out): there the servers and search list
    /// are the ones the OS gives its adapters, which std's resolver used
    /// before, with evdns's defaults for the rest.
    pub fn system() -> Self {
        #[cfg(windows)]
        {
            Self::windows()
        }
        #[cfg(not(windows))]
        {
            let resolv = std::fs::read_to_string(RESOLV_CONF).unwrap_or_default();
            Self::parse(&resolv, hostname().as_deref())
        }
    }

    /// hickory-resolver's read of the adapters' DNS settings (the
    /// system-config feature, on by default).
    #[cfg(windows)]
    fn windows() -> Self {
        match hickory_resolver::system_conf::read_system_conf() {
            Ok((config, _)) => Self::from_os(
                config.name_servers().iter().map(|ns| ns.ip),
                config
                    .domain()
                    .filter(|d| !d.is_root())
                    .map(|d| d.to_ascii())
                    .as_deref(),
                config.search().iter().map(|d| d.to_ascii()),
            ),
            Err(e) => {
                tracing::warn!("no DNS servers from the system: {}", e);
                Self::from_os(std::iter::empty(), None, std::iter::empty())
            }
        }
    }

    /// evdns's defaults with servers (port 53) and a search list the OS
    /// gave: its search list, else its domain. No 127.0.0.1 when it gives
    /// no server: that is evdns reading a missing resolv.conf
    /// (evdns.c:3697-3725), and lookups then fail at once.
    pub fn from_os(
        servers: impl IntoIterator<Item = IpAddr>,
        domain: Option<&str>,
        search: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut conf = Self::default();
        for ip in servers {
            let addr = SocketAddr::new(ip, 53);
            if !conf.nameservers.contains(&addr) {
                conf.nameservers.push(addr);
            }
        }
        let domain_postfix = |d: &str| search_postfix(d.trim_end_matches('.'));
        conf.search = search
            .into_iter()
            .map(|d| domain_postfix(&d))
            .filter(|d| !d.is_empty())
            .collect();
        if conf.search.is_empty() {
            conf.search.extend(domain.map(domain_postfix));
        }
        conf
    }

    /// Parse resolv.conf as evdns_base_resolv_conf_parse does
    /// (evdns.c:3590-3634, 3672-3730): `nameserver` (127.0.0.1 when there
    /// is none), `domain` and `search` (the last one wins; without either,
    /// the domain part of the host name), and the `options` evdns knows.
    pub fn parse(text: &str, hostname: Option<&str>) -> Self {
        let mut conf = Self::default();
        for line in text.split('\n') {
            let mut tokens = line.split([' ', '\t']).filter(|t| !t.is_empty());
            match tokens.next() {
                Some("nameserver") => {
                    // a server already listed is not added again
                    // (evdns_nameserver_add_impl_, evdns.c:2513-2518)
                    if let Some(addr) = tokens.next().and_then(parse_nameserver) {
                        if !conf.nameservers.contains(&addr) {
                            conf.nameservers.push(addr);
                        }
                    }
                }
                Some("domain") => {
                    if let Some(domain) = tokens.next() {
                        conf.search = vec![search_postfix(domain)];
                    }
                }
                Some("search") => conf.search = tokens.map(search_postfix).collect(),
                Some("options") => {
                    for option in tokens {
                        let (name, val) = option.split_once(':').unwrap_or((option, ""));
                        conf.set_option(name, val);
                    }
                }
                _ => {}
            }
        }
        if conf.nameservers.is_empty() {
            conf.nameservers
                .push(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 53));
        }
        // search_set_from_hostname (evdns.c:3209-3218, 3725-3726)
        if conf.search.is_empty() {
            if let Some((_, domain)) = hostname.and_then(|h| h.split_once('.')) {
                conf.search.push(search_postfix(domain));
            }
        }
        conf
    }

    /// evdns_base_set_option_impl (evdns.c:3497-3579), for the options
    /// that change what goes on the wire.
    fn set_option(&mut self, name: &str, val: &str) {
        match name {
            "ndots" => {
                if let Some(n) = strtoint(val).and_then(|n| usize::try_from(n).ok()) {
                    self.ndots = n;
                }
            }
            "timeout" => {
                if let Some(t) = strtotimeval(val) {
                    self.timeout = t;
                }
            }
            "attempts" => {
                if let Some(n) = strtoint(val).and_then(|n| u32::try_from(n).ok()) {
                    self.attempts = n.min(255);
                }
            }
            "randomize-case" => self.randomize_case = strtoint(val) != Some(0),
            "max-timeouts" => {
                // strtoint_clipped(val, 1, 255)
                if let Some(n) = strtoint(val).filter(|n| *n >= 0) {
                    self.max_timeouts = n.clamp(1, 255) as u32;
                }
            }
            "initial-probe-timeout" => {
                if let Some(t) = strtotimeval(val) {
                    self.probe_timeout = t.min(Duration::from_secs(3600));
                }
            }
            _ => {}
        }
    }
}

/// evdns's strtoint: a whole decimal number, or nothing.
fn strtoint(val: &str) -> Option<i64> {
    if val.is_empty() {
        return Some(0);
    }
    val.parse().ok()
}

/// evdns_strtotimeval: seconds, at least a millisecond (evdns.c:3408-3420).
fn strtotimeval(val: &str) -> Option<Duration> {
    val.parse::<f64>()
        .ok()
        .filter(|s| *s >= 0.001)
        .map(Duration::from_secs_f64)
}

/// search_postfix_add drops leading dots (evdns.c:3148).
fn search_postfix(domain: &str) -> String {
    domain.trim_start_matches('.').to_string()
}

/// evutil_parse_sockaddr_port as evdns_base_nameserver_ip_add uses it
/// (evutil.c:2140-2230): `v6`, `[v6]`, `[v6]:port`, `v4:port` or `v4`,
/// port 53 when none is given (evdns.c:2668-2673), a port given 1-65535.
/// RIPE's libevent takes an IPv6 zone, `%ifname` or `%index`
/// (evutil_inet_pton_scope, evutil.c:1993-2026), as the scope of a
/// link-local resolver (`nameserver fe80::1%eth0`, as RDNSS sets it up).
fn parse_nameserver(s: &str) -> Option<SocketAddr> {
    let (addr, port, v6) = if let Some(rest) = s.strip_prefix('[') {
        let (addr, after) = rest.split_once(']')?;
        (addr, after.strip_prefix(':'), true)
    } else {
        match s.split_once(':') {
            Some((_, after)) if after.contains(':') => (s, None, true),
            Some((addr, port)) => (addr, Some(port), false),
            None => (s, None, false),
        }
    };
    let port = match port {
        None => 53,
        // atoi, then `port <= 0 || port > 65535` is refused
        Some(port) => {
            let digits = port.bytes().take_while(u8::is_ascii_digit).count();
            match port[..digits].parse::<u32>().ok()? {
                port @ 1..=65535 => port as u16,
                _ => return None,
            }
        }
    };
    if !v6 {
        return Some(SocketAddr::new(addr.parse::<Ipv4Addr>().ok()?.into(), port));
    }
    let (addr, scope) = match addr.split_once('%') {
        Some((addr, zone)) => (addr, Some(zone)),
        None => (addr, None),
    };
    let addr = addr.parse::<Ipv6Addr>().ok()?;
    let scope = match scope {
        None => 0,
        Some(zone) => zone_index(zone)?,
    };
    Some(SocketAddr::V6(SocketAddrV6::new(addr, port, 0, scope)))
}

/// An IPv6 zone: the interface of that name, else a number (strtoul up to
/// the end, so an empty zone is 0).
fn zone_index(zone: &str) -> Option<u32> {
    #[cfg(unix)]
    {
        if let Some(index) = starla_network::interface_index(zone) {
            return Some(index);
        }
    }
    if zone.is_empty() {
        return Some(0);
    }
    zone.parse().ok()
}

/// The hosts file as evdns_base_parse_hosts_line reads it
/// (evdns.c:4224-4275): an address, then names up to a `#`.
fn parse_hosts(text: &str) -> Vec<(String, IpAddr)> {
    let mut entries = Vec::new();
    for line in text.split(['\n', '\r']) {
        let mut tokens = line.split([' ', '\t']).filter(|t| !t.is_empty());
        let Some(addr) = tokens.next().filter(|a| !a.starts_with('#')) else {
            continue;
        };
        let Some(addr) = addr
            .strip_prefix('[')
            .and_then(|a| a.strip_suffix(']'))
            .unwrap_or(addr)
            .parse::<IpAddr>()
            .ok()
        else {
            continue;
        };
        for name in tokens {
            let (name, comment) = match name.split_once('#') {
                Some((name, _)) => (name, true),
                None => (name, false),
            };
            if !name.is_empty() {
                entries.push((name.to_string(), addr));
            }
            if comment {
                break;
            }
        }
    }
    entries
}

/// The host name, for the search list when resolv.conf has none.
#[cfg(not(windows))]
fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|h| h.trim().to_string())
}

/// evdns's 0x20: each letter of the name goes out lower case when bit
/// `i & 7` of random byte `i >> 3` is set, upper case otherwise
/// (evdns.c:2824-2836); everything else is left alone.
pub fn randomize_case(name: &str, bits: &[u8]) -> String {
    let out: Vec<u8> = name
        .bytes()
        .enumerate()
        .map(|(i, c)| {
            if !c.is_ascii_alphabetic() {
                c
            } else if bits.get(i >> 3).is_some_and(|b| b & (1 << (i & 7)) != 0) {
                c | 0x20
            } else {
                c & !0x20
            }
        })
        .collect();
    // Only ASCII letters change, so the bytes stay valid UTF-8.
    String::from_utf8(out).unwrap_or_else(|_| name.to_string())
}

/// How one query ended (reply_handle, evdns.c:887-976).
#[derive(Debug, Clone, PartialEq)]
enum Answer {
    Addrs(Vec<IpAddr>),
    /// NXDOMAIN
    NotExist,
    /// No reply after the last transmission (or SERVFAIL then): the
    /// lookup ends here, without trying the rest of the search list
    /// (evdns_request_timeout_callback, evdns.c:2189-2197).
    TimedOut,
    /// Any other error: the search list goes on.
    Failed,
}

/// A reply to the query, as reply_parse sorts it (evdns.c:1036-1228).
#[derive(Debug, Clone, PartialEq)]
enum Reply {
    Answer(Answer),
    /// SERVFAIL counts as a timeout (evdns.c:927-940).
    ServFail,
    /// NOTIMPL and REFUSED: tried once more at another server
    /// (evdns.c:916-926).
    Refused,
}

/// request_reissue's limit (global_max_reissues, evdns.c:4009).
const MAX_REISSUES: u32 = 1;

/// nameserver_probe_failed's back-off (evdns.c:519-520).
const MAX_PROBE_TIMEOUT: Duration = Duration::from_secs(3600);
const TIMEOUT_BACKOFF_FACTOR: u32 = 3;

/// The probe evdns sends a server it thinks is down (nameserver_send_probe,
/// evdns.c:2348): an A query for this name, with no search list.
const PROBE_NAME: &str = "google.com";

/// The query evdns_request_data_build writes (evdns.c:1697-1716): the ID,
/// RD and nothing else in the header, one question, no EDNS.
fn query_packet(id: u16, name: &Name, qtype: RecordType) -> Option<Vec<u8>> {
    let mut msg = Message::new(id, MessageType::Query, OpCode::Query);
    msg.metadata.recursion_desired = true;
    msg.add_query(Query::query(name.clone(), qtype));
    msg.to_vec().ok()
}

/// Sort a datagram: None when it is no reply to this query (too short,
/// another ID, not a response), else what reply_parse makes of it. The
/// question must be the query's, regardless of case with randomize-case
/// on, exactly without (evdns.c:1098-1112). An NXDOMAIN is NXDOMAIN
/// whatever follows the header: a question that is not the query's, or
/// none, or a body that does not parse, goes to reply_parse's `err`, which
/// still hands the header's flags to reply_handle (evdns.c:1225-1227), and
/// rcode 3 there is DNS_ERR_NOTEXIST (evdns.c:898-907): EAI_NONAME,
/// "nodename nor servname provided, or not known". Only rcode 0 fails
/// over such a question, as DNS_ERR_UNKNOWN: EAI_FAIL.
fn parse_reply(
    bytes: &[u8],
    id: u16,
    name: &Name,
    qtype: RecordType,
    randomize_case: bool,
) -> Option<Reply> {
    if bytes.len() < 12 || u16::from_be_bytes([bytes[0], bytes[1]]) != id || bytes[2] & 0x80 == 0 {
        return None;
    }
    let truncated = bytes[2] & 0x02 != 0;
    let rcode = bytes[3] & 0x0f;
    if truncated {
        return Some(Reply::Answer(Answer::Failed));
    }
    match rcode {
        0 => {}
        3 => return Some(Reply::Answer(Answer::NotExist)),
        2 => return Some(Reply::ServFail),
        4 | 5 => return Some(Reply::Refused),
        _ => return Some(Reply::Answer(Answer::Failed)),
    }
    let Ok(msg) = Message::from_vec(bytes) else {
        return Some(Reply::Answer(Answer::Failed));
    };
    let ours = |q: &Query| {
        if randomize_case {
            q.name().eq_ignore_root(name)
        } else {
            q.name().to_ascii().trim_end_matches('.') == name.to_ascii().trim_end_matches('.')
        }
    };
    if !msg.queries.iter().any(ours) {
        return Some(Reply::Answer(Answer::Failed));
    }
    let addrs: Vec<IpAddr> = msg
        .answers
        .iter()
        .filter(|rr| rr.dns_class == DNSClass::IN)
        .filter_map(|rr| match (&rr.data, qtype) {
            (RData::A(a), RecordType::A) => Some(IpAddr::V4(a.0)),
            (RData::AAAA(a), RecordType::AAAA) => Some(IpAddr::V6(a.0)),
            _ => None,
        })
        .collect();
    Some(Reply::Answer(if addrs.is_empty() {
        Answer::Failed
    } else {
        Answer::Addrs(addrs)
    }))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One resolver, as evdns's struct nameserver (evdns.c:213-245) has it.
#[derive(Debug)]
struct NameServer {
    addr: SocketAddr,
    /// evdns opens the socket when the server is added and keeps it
    /// (evdns.c:2532-2577), so every query to a server leaves from one
    /// source port: on the twin, 79 of the reference's A/AAAA queries left
    /// from two ports, one per resolv.conf server. Made at the first query
    /// here.
    socket: OnceCell<Arc<UdpSocket>>,
    state: Mutex<ServerState>,
    /// Ends the socket's reader when the resolver goes.
    closed: Notify,
}

#[derive(Debug, Default)]
struct ServerState {
    /// ns->state == 0
    down: bool,
    /// ns->failed_times: the probe back-off's exponent
    failed_times: u32,
    /// ns->timedout: retransmissions to it in a row with nothing read
    timedout: u32,
    /// Bumped when it goes down: a probe loop from before stops.
    generation: u64,
}

impl NameServer {
    fn is_down(&self) -> bool {
        lock(&self.state).down
    }
}

/// What one evdns_base keeps between lookups: its servers, whose turn is
/// next, and the queries in flight.
#[derive(Debug)]
struct Base {
    /// In evdns's ring order: evdns_nameserver_add_impl_ links each server
    /// in right after the first (evdns.c:2586-2594), so resolv.conf's A, B,
    /// C take turns as A, C, B.
    servers: Vec<Arc<NameServer>>,
    /// server_head
    head: Mutex<usize>,
    /// Queries in flight by ID (req_heads); a reply on any server's socket
    /// goes to the query with its ID (reply_parse,
    /// request_find_from_trans_id).
    inflight: Mutex<HashMap<u16, mpsc::UnboundedSender<Vec<u8>>>>,
    /// The options probes run with.
    conf: ResolvConf,
}

impl Drop for Base {
    fn drop(&mut self) {
        for server in &self.servers {
            server.closed.notify_one();
        }
    }
}

/// A query's place in [`Base::inflight`], given up when the query ends.
struct InFlight {
    base: Arc<Base>,
    id: u16,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        lock(&self.base.inflight).remove(&self.id);
    }
}

impl Base {
    fn new(conf: &ResolvConf) -> Arc<Self> {
        let ring = conf
            .nameservers
            .first()
            .into_iter()
            .chain(conf.nameservers.iter().skip(1).rev());
        Arc::new(Self {
            servers: ring
                .map(|&addr| {
                    Arc::new(NameServer {
                        addr,
                        socket: OnceCell::new(),
                        state: Mutex::default(),
                        closed: Notify::new(),
                    })
                })
                .collect(),
            head: Mutex::new(0),
            inflight: Mutex::default(),
            conf: conf.clone(),
        })
    }

    /// nameserver_pick (evdns.c:1347-1384): the next server thought up,
    /// moving the head past it; with none up, simply the next one.
    fn pick(&self) -> Option<Arc<NameServer>> {
        let n = self.servers.len();
        if n == 0 {
            return None;
        }
        let mut head = lock(&self.head);
        if self.servers.iter().all(|s| s.is_down()) {
            *head = (*head + 1) % n;
            return Some(self.servers[*head].clone());
        }
        loop {
            let server = &self.servers[*head];
            *head = (*head + 1) % n;
            if !server.is_down() {
                return Some(server.clone());
            }
        }
    }

    /// The server's socket, opened (and its reader started) the first time.
    async fn socket(self: &Arc<Self>, server: &Arc<NameServer>) -> std::io::Result<Arc<UdpSocket>> {
        server
            .socket
            .get_or_try_init(|| async {
                let any: IpAddr = match server.addr {
                    SocketAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
                    SocketAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
                };
                let socket = Arc::new(UdpSocket::bind(SocketAddr::new(any, 0)).await?);
                tokio::spawn(read_replies(
                    Arc::downgrade(self),
                    server.clone(),
                    socket.clone(),
                ));
                Ok::<_, std::io::Error>(socket)
            })
            .await
            .cloned()
    }

    /// evdns_request_transmit_to (evdns.c:2219-2245): a send that fails
    /// marks the server down, and the query still waits out its timeout.
    async fn transmit(self: &Arc<Self>, server: &Arc<NameServer>, packet: &[u8]) {
        let sent = match self.socket(server).await {
            Ok(socket) => socket.send_to(packet, server.addr).await.map(|_| ()),
            Err(e) => Err(e),
        };
        if let Err(e) = sent {
            debug!("DNS query to {} not sent: {}", server.addr, e);
            self.server_failed(server, &e.to_string());
        }
    }

    /// nameserver_failed (evdns.c:563-621): passed over by pick until a
    /// probe, `probe_timeout` later, finds it up.
    fn server_failed(self: &Arc<Self>, server: &Arc<NameServer>, why: &str) {
        let generation = {
            let mut state = lock(&server.state);
            if state.down {
                return;
            }
            state.down = true;
            state.failed_times = 1;
            state.generation += 1;
            state.generation
        };
        debug!("nameserver {} has failed: {}", server.addr, why);
        tokio::spawn(probe(Arc::downgrade(self), server.clone(), generation));
    }

    /// One query, as evdns sends it (request_new, evdns_request_transmit,
    /// evdns_request_timeout_callback, reply_handle): the name with its case
    /// randomised once (retransmissions repeat the same bytes) to `first`
    /// or the next server; after each `timeout` without a reply it goes
    /// again to the next server, up to `attempts` transmissions
    /// (evdns.c:2179-2214, 2256-2300).
    async fn ask(
        self: &Arc<Self>,
        conf: &ResolvConf,
        name: &str,
        qtype: RecordType,
        first: Option<Arc<NameServer>>,
    ) -> Answer {
        let Some(mut server) = first.or_else(|| self.pick()) else {
            return Answer::Failed;
        };
        let wire_name = if conf.randomize_case {
            let bits: Vec<u8> = (0..name.len().div_ceil(8))
                .map(|_| rand::random())
                .collect();
            randomize_case(name, &bits)
        } else {
            name.to_string()
        };
        let Ok(qname) = Name::from_ascii(&wire_name) else {
            return Answer::Failed;
        };
        // transaction_id_pick (evdns.c:1331-1345): unused, never 0xffff
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = {
            let mut inflight = lock(&self.inflight);
            let id = loop {
                let id = rand::random::<u16>();
                if id != 0xffff && !inflight.contains_key(&id) {
                    break id;
                }
            };
            inflight.insert(id, tx);
            id
        };
        let _in_flight = InFlight {
            base: self.clone(),
            id,
        };
        let Some(packet) = query_packet(id, &qname, qtype) else {
            return Answer::Failed;
        };

        self.transmit(&server, &packet).await;
        let mut tx_count = 1;
        let mut reissues = 0;
        let mut deadline = tokio::time::Instant::now() + conf.timeout;
        loop {
            let reply = loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Err(_) | Ok(None) => break None,
                    Ok(Some(bytes)) => {
                        if let Some(reply) =
                            parse_reply(&bytes, id, &qname, qtype, conf.randomize_case)
                        {
                            break Some(reply);
                        }
                    }
                }
            };
            match reply {
                Some(Reply::Answer(answer)) => {
                    // a good reply: the server is up (evdns.c:942-949, 971-973)
                    server_up(&server);
                    return answer;
                }
                Some(Reply::Refused) => {
                    // The server is marked down and the query handed to the
                    // next one with its count reset (request_reissue,
                    // evdns.c:715-741, 916-926), once. Nothing sends it
                    // then: it goes when its timer runs out, as a
                    // retransmission, to the server picked after that one.
                    if reissues >= MAX_REISSUES {
                        return Answer::Failed;
                    }
                    self.server_failed(&server, "bad response");
                    match self.pick() {
                        Some(next) if !Arc::ptr_eq(&next, &server) => {
                            reissues += 1;
                            tx_count = 0;
                            server = next;
                        }
                        _ => return Answer::Failed,
                    }
                }
                // evdns_request_timeout_callback (evdns.c:2179-2214), which
                // SERVFAIL calls at once
                None | Some(Reply::ServFail) => {
                    if tx_count >= conf.attempts {
                        self.server_failed(&server, "request timed out.");
                        return Answer::TimedOut;
                    }
                    let Some(next) = self.pick() else {
                        return Answer::TimedOut;
                    };
                    server = next;
                    self.transmit(&server, &packet).await;
                    tx_count += 1;
                    deadline = tokio::time::Instant::now() + conf.timeout;
                    // The retransmission counts against the server it goes
                    // to; any datagram read from a server resets its count
                    // (evdns.c:2203-2210, 1417).
                    let too_many = {
                        let mut state = lock(&server.state);
                        state.timedout += 1;
                        let too_many = state.timedout > conf.max_timeouts;
                        if too_many {
                            state.timedout = 0;
                        }
                        too_many
                    };
                    if too_many {
                        self.server_failed(&server, "request timed out.");
                    }
                }
            }
        }
    }
}

/// nameserver_up (evdns.c:623-642): only a server that was down changes.
fn server_up(server: &NameServer) {
    let mut state = lock(&server.state);
    if !state.down {
        return;
    }
    debug!("nameserver {} is back up", server.addr);
    state.down = false;
    state.failed_times = 0;
    state.timedout = 0;
}

/// nameserver_read (evdns.c:1387-1420): every datagram on the server's
/// socket that comes from the server's address goes to the query with its
/// ID; a read error marks the server down.
async fn read_replies(base: Weak<Base>, server: Arc<NameServer>, socket: Arc<UdpSocket>) {
    let mut buf = vec![0u8; 1500];
    loop {
        let received = tokio::select! {
            _ = server.closed.notified() => return,
            received = socket.recv_from(&mut buf) => received,
        };
        let Some(base) = base.upgrade() else {
            return;
        };
        match received {
            Err(e) => base.server_failed(&server, &e.to_string()),
            Ok((n, from)) => {
                if from.ip() != server.addr.ip() {
                    debug!("DNS reply from {} on {}'s socket", from, server.addr);
                    continue;
                }
                lock(&server.state).timedout = 0;
                if n >= 2 {
                    let id = u16::from_be_bytes([buf[0], buf[1]]);
                    if let Some(query) = lock(&base.inflight).get(&id) {
                        let _ = query.send(buf[..n].to_vec());
                    }
                }
            }
        }
    }
}

/// The wait before the probe after one that failed: probe_timeout times
/// 3 per failure so far, at most an hour (nameserver_probe_failed,
/// evdns.c:507-545).
fn probe_backoff(probe_timeout: Duration, failed_times: u32) -> Duration {
    let mut timeout = probe_timeout;
    for _ in 0..failed_times {
        if timeout >= MAX_PROBE_TIMEOUT {
            break;
        }
        timeout *= TIMEOUT_BACKOFF_FACTOR;
    }
    timeout.min(MAX_PROBE_TIMEOUT)
}

/// A server that went down is probed with an A query for google.com,
/// `probe_timeout` later and then backing off, until a reply (an answer or
/// NXDOMAIN) brings it up (nameserver_prod_callback, nameserver_send_probe,
/// nameserver_probe_callback, evdns.c:494-501, 2306-2360). The probe goes
/// to the server, and its retransmissions to whichever is next, as
/// evdns's do.
async fn probe(base: Weak<Base>, server: Arc<NameServer>, generation: u64) {
    let still_down = || {
        let state = lock(&server.state);
        state.down && state.generation == generation
    };
    let Some(mut delay) = base.upgrade().map(|b| b.conf.probe_timeout) else {
        return;
    };
    loop {
        tokio::time::sleep(delay).await;
        let Some(base) = base.upgrade() else {
            return;
        };
        if !still_down() {
            return;
        }
        let conf = base.conf.clone();
        let answer = base
            .ask(&conf, PROBE_NAME, RecordType::A, Some(server.clone()))
            .await;
        if matches!(answer, Answer::Addrs(_) | Answer::NotExist) {
            server_up(&server);
            return;
        }
        let mut state = lock(&server.state);
        if !(state.down && state.generation == generation) {
            return;
        }
        delay = probe_backoff(conf.probe_timeout, state.failed_times);
        state.failed_times += 1;
    }
}

/// evdns, set up as eperd sets it up.
#[derive(Debug, Clone)]
pub struct Resolver {
    pub conf: ResolvConf,
    pub hosts: Vec<(String, IpAddr)>,
    /// The servers' sockets and state, made from `conf.nameservers` when
    /// the resolver is.
    base: Arc<Base>,
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new(ResolvConf::default(), Vec::new())
    }
}

impl Resolver {
    pub fn new(conf: ResolvConf, hosts: Vec<(String, IpAddr)>) -> Self {
        Self {
            base: Base::new(&conf),
            conf,
            hosts,
        }
    }

    /// From the probe's resolv.conf (or Windows's servers) and hosts file
    /// as they are now. The servers, their sockets and their up/down state
    /// stay the same for as long as the configuration does, as eperd keeps
    /// its evdns base until resolv.conf changes (check_resolv_conf,
    /// eperd.c:647-689).
    pub fn system() -> Self {
        static SYSTEM: Mutex<Option<Arc<Base>>> = Mutex::new(None);
        let conf = ResolvConf::system();
        let hosts = parse_hosts(&std::fs::read_to_string(hosts_path()).unwrap_or_default());
        let mut system = lock(&SYSTEM);
        let base = match system.as_ref() {
            Some(base) if base.conf == conf => base.clone(),
            _ => {
                let base = Base::new(&conf);
                *system = Some(base.clone());
                base
            }
        };
        Self { conf, hosts, base }
    }

    pub async fn lookup(&self, target: &str, af: u8) -> Lookup {
        // An address is used as it is, and reported without a ttr. The
        // reference only takes a literal of the family asked for; RIPE
        // never pairs -4 with an IPv6 literal or -6 with an IPv4 one, and
        // Starla keeps measuring the literal it was given.
        if let Ok(addr) = target.parse::<IpAddr>() {
            return Lookup {
                dst_name: DstName {
                    name: target.to_string(),
                    ttr: None,
                },
                addr: Ok(addr),
            };
        }
        // ttr runs from the lookup's start to its callback (ping.c:1787,
        // 1636-1645).
        let start = Instant::now();
        let addr = self.resolve(target, af).await;
        Lookup {
            dst_name: DstName {
                name: target.to_string(),
                ttr: Some(start.elapsed().as_secs_f64() * 1e3),
            },
            addr,
        }
    }

    /// evdns_getaddrinfo (evdns.c:4741-4870), first address.
    async fn resolve(&self, name: &str, af: u8) -> Result<IpAddr, LookupError> {
        if let Some(found) = self.hosts_entry(name, af) {
            return found.first().copied().ok_or(LookupError::NoAddress);
        }
        let addrs = match af {
            4 => self.resolve_type(name, RecordType::A).await,
            6 => self.resolve_type(name, RecordType::AAAA).await,
            _ => {
                // Both at once; the IPv4 answers first (evdns.c:4663-4669).
                let (a, aaaa) = tokio::join!(
                    self.resolve_type(name, RecordType::A),
                    self.resolve_type(name, RecordType::AAAA)
                );
                a.or(aaaa)
            }
        }?;
        addrs.first().copied().ok_or(LookupError::Fail)
    }

    /// evdns_getaddrinfo_fromhosts (evdns.c:4701-4738): None when the hosts
    /// file does not have the name, else its addresses of the family.
    fn hosts_entry(&self, name: &str, af: u8) -> Option<Vec<IpAddr>> {
        let mut found = false;
        let mut addrs = Vec::new();
        for (host, addr) in &self.hosts {
            if host.eq_ignore_ascii_case(name) {
                found = true;
                if !matches!((af, addr), (4, IpAddr::V6(_)) | (6, IpAddr::V4(_))) {
                    addrs.push(*addr);
                }
            }
        }
        found.then_some(addrs)
    }

    /// The names to ask for, in order (search_request_new and
    /// search_try_next, evdns.c:3252-3342): with ndots dots or more, the
    /// name itself first, then with each search domain; with fewer, the
    /// search domains first and the name itself last.
    fn candidates(&self, name: &str) -> Vec<String> {
        let search = &self.conf.search;
        if search.is_empty() {
            return vec![name.to_string()];
        }
        let dot = if name.ends_with('.') { "" } else { "." };
        let with = search.iter().map(|d| format!("{name}{dot}{d}"));
        if name.matches('.').count() >= self.conf.ndots {
            std::iter::once(name.to_string()).chain(with).collect()
        } else {
            with.chain(std::iter::once(name.to_string())).collect()
        }
    }

    /// One family's addresses: the candidates in turn until one answers.
    /// The last one's error is the lookup's (reply_handle, evdns.c:953-967);
    /// a timeout ends it at once.
    async fn resolve_type(
        &self,
        name: &str,
        qtype: RecordType,
    ) -> Result<Vec<IpAddr>, LookupError> {
        let mut error = LookupError::Fail;
        for candidate in self.candidates(name) {
            match self.base.ask(&self.conf, &candidate, qtype, None).await {
                Answer::Addrs(addrs) => return Ok(addrs),
                Answer::TimedOut => return Err(LookupError::Fail),
                Answer::NotExist => error = LookupError::NoName,
                Answer::Failed => error = LookupError::Fail,
            }
        }
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::rr::rdata::{A, AAAA};
    use hickory_proto::rr::Record;

    /// atlas_check_addr's prefixes, edges included.
    #[test]
    fn addresses_eperd_refuses() {
        for addr in [
            "127.0.0.1",
            "10.255.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.0.1",
            "224.0.0.1",
            "239.255.255.255",
            "172.30.74.20",
            "::1",
            "::ffff:192.0.2.1",
            "fe80::1",
            "fd00::1",
            "fc00::1",
            "ff02::1",
            "e000::1",
        ] {
            assert!(refused(addr.parse().unwrap()), "{addr}");
        }
        for addr in [
            "193.0.14.129",
            "172.15.255.255",
            "172.32.0.1",
            "192.0.2.1",
            "11.0.0.1",
            "0.0.0.1",
            // 224/4 ends at 239: 240/4 is not on the list
            "255.255.255.255",
            "2001:7fd::1",
            "2001:db8:7769:6e00::20",
            "::",
            "::2",
            "::ffff:0:1:0:0",
            "dfff::1",
            "64:ff9b::c000:201",
        ] {
            assert!(!refused(addr.parse().unwrap()), "{addr}");
        }
    }

    /// The queries a fake server got, as they came, with where from.
    type Seen = Arc<Mutex<Vec<(Vec<u8>, SocketAddr)>>>;

    fn queries(seen: &Seen) -> Vec<Message> {
        let seen = seen.lock().unwrap();
        seen.iter()
            .map(|(q, _)| Message::from_vec(q).unwrap())
            .collect()
    }

    fn sources(seen: &Seen) -> Vec<SocketAddr> {
        seen.lock().unwrap().iter().map(|(_, from)| *from).collect()
    }

    /// A resolver on a local socket that records every query and answers
    /// as `answer` says (None: no reply).
    async fn fake_server(
        answer: impl Fn(&Message) -> Option<Message> + Send + 'static,
    ) -> (SocketAddr, Seen) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let seen = Seen::default();
        let log = seen.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 1500];
            while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                log.lock().unwrap().push((buf[..n].to_vec(), from));
                let query = Message::from_vec(&buf[..n]).unwrap();
                if let Some(reply) = answer(&query) {
                    socket
                        .send_to(&reply.to_vec().unwrap(), from)
                        .await
                        .unwrap();
                }
            }
        });
        (addr, seen)
    }

    /// The reply a resolver gives: the question as asked, case and all.
    fn reply(query: &Message, rcode: u16) -> Message {
        let mut reply = Message::new(query.metadata.id, MessageType::Response, OpCode::Query);
        reply.metadata.recursion_desired = query.metadata.recursion_desired;
        reply.metadata.recursion_available = true;
        reply.metadata.response_code = rcode.into();
        for q in &query.queries {
            reply.add_query(q.clone());
        }
        reply
    }

    /// Answers both families for any name.
    fn answer_both(query: &Message) -> Option<Message> {
        let mut r = reply(query, 0);
        let q = &query.queries[0];
        let rdata = match q.query_type() {
            RecordType::A => RData::A(A(Ipv4Addr::new(192, 0, 2, 7))),
            _ => RData::AAAA(AAAA("2001:db8::7".parse().unwrap())),
        };
        r.add_answer(Record::from_rdata(q.name().clone(), 60, rdata));
        Some(r)
    }

    fn resolver(server: SocketAddr) -> Resolver {
        resolver_of(&[server])
    }

    fn resolver_of(servers: &[SocketAddr]) -> Resolver {
        Resolver::new(
            ResolvConf {
                nameservers: servers.to_vec(),
                ..Default::default()
            },
            Vec::new(),
        )
    }

    /// For a -6 measurement the reference asks one AAAA question and
    /// nothing else (ping.c:1793-1798 with hints.ai_family AF_INET6; the
    /// twin's `AAAA bitS.wIkImeDIa.Org.`); Starla asked A and AAAA.
    #[tokio::test]
    async fn a_minus_6_lookup_asks_only_for_aaaa() {
        let (server, seen) = fake_server(answer_both).await;
        let found = resolver(server).lookup("bits.wikimedia.org", 6).await;
        assert_eq!(found.addr, Ok("2001:db8::7".parse().unwrap()));
        let seen = queries(&seen);
        assert_eq!(seen.len(), 1, "{seen:?}");
        let q = &seen[0];
        assert_eq!(q.queries.len(), 1);
        assert_eq!(q.queries[0].query_type(), RecordType::AAAA);
        assert_eq!(q.queries[0].query_class(), DNSClass::IN);
        assert_eq!(
            q.queries[0].name().to_ascii().to_lowercase(),
            "bits.wikimedia.org."
        );
        // evdns_request_data_build: RD and nothing else, no EDNS
        assert!(q.metadata.recursion_desired);
        assert!(q.additionals.is_empty() && q.edns.is_none());
    }

    #[tokio::test]
    async fn a_minus_4_lookup_asks_only_for_a() {
        let (server, seen) = fake_server(answer_both).await;
        let found = resolver(server).lookup("www.ripe.net", 4).await;
        assert_eq!(found.addr, Ok(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7))));
        let types: Vec<_> = queries(&seen)
            .iter()
            .map(|q| q.queries[0].query_type())
            .collect();
        assert_eq!(types, [RecordType::A]);
    }

    /// The query's name goes out in randomised case, and the answer, which
    /// repeats the question as asked, is taken.
    #[tokio::test]
    async fn names_go_out_in_random_case() {
        let (server, seen) = fake_server(answer_both).await;
        let resolver = resolver(server);
        for _ in 0..20 {
            let found = resolver.lookup("topology6.dyndns.atlas.ripe.net", 6).await;
            assert!(found.addr.is_ok());
        }
        let names: Vec<String> = queries(&seen)
            .iter()
            .map(|q| q.queries[0].name().to_ascii())
            .collect();
        assert_eq!(names.len(), 20);
        for name in &names {
            assert!(name.eq_ignore_ascii_case("topology6.dyndns.atlas.ripe.net."));
        }
        // 27 letters each: all-lower-case 20 times is a 2^-540 chance
        assert!(
            names
                .iter()
                .any(|n| n != "topology6.dyndns.atlas.ripe.net."),
            "{names:?}"
        );
        assert!(names
            .iter()
            .any(|n| n.bytes().any(|b| b.is_ascii_uppercase())));
        assert!(names
            .iter()
            .any(|n| n.bytes().any(|b| b.is_ascii_lowercase())));
    }

    /// evdns.c:2827-2835, bit by bit.
    #[test]
    fn case_follows_evdns_bits() {
        assert_eq!(randomize_case("youtube.com", &[0x00, 0x00]), "YOUTUBE.COM");
        assert_eq!(randomize_case("YouTube.Com", &[0xff, 0xff]), "youtube.com");
        // bit 0, 2, 4, 6 of each byte set: letters 0, 2, 4, 6, 8, 10 lower
        assert_eq!(randomize_case("youtube.com", &[0x55, 0x55]), "yOuTuBe.cOm");
        assert_eq!(randomize_case("a-1.b_2", &[0x00]), "A-1.B_2");
    }

    #[tokio::test]
    async fn randomize_case_0_sends_the_name_as_written() {
        let (server, seen) = fake_server(answer_both).await;
        let mut resolver = resolver(server);
        resolver.conf.randomize_case = false;
        resolver.lookup("YouTube.com", 6).await.addr.unwrap();
        assert_eq!(
            queries(&seen)[0].queries[0].name().to_ascii(),
            "YouTube.com."
        );
    }

    /// dns_cb reports evutil_gai_strerror(result): NXDOMAIN is EAI_NONAME,
    /// anything else EAI_FAIL.
    #[tokio::test]
    async fn nxdomain_and_no_data_fail_as_evdns_says() {
        let (server, _) = fake_server(|q| Some(reply(q, 3))).await;
        let found = resolver(server).lookup("nonexistent.example", 6).await;
        assert_eq!(found.addr, Err(LookupError::NoName));
        assert_eq!(
            found.addr.unwrap_err().to_string(),
            "nodename nor servname provided, or not known"
        );
        assert!(found.dst_name.ttr.is_some());

        let (server, _) = fake_server(|q| Some(reply(q, 0))).await;
        let found = resolver(server).lookup("v4only.example", 6).await;
        assert_eq!(found.addr, Err(LookupError::Fail));
        assert_eq!(
            found.addr.unwrap_err().to_string(),
            "non-recoverable failure in name resolution"
        );
    }

    /// No reply: `attempts` transmissions `timeout` apart, then EAI_FAIL;
    /// SERVFAIL counts as a timeout.
    #[tokio::test]
    async fn silence_and_servfail_retransmit_then_fail() {
        for answer in [None, Some(2u16)] {
            let (server, seen) = fake_server(move |q| answer.map(|rc| reply(q, rc))).await;
            let mut resolver = resolver(server);
            resolver.conf.timeout = Duration::from_millis(50);
            resolver.conf.attempts = 3;
            let found = resolver.lookup("www.ripe.net", 6).await;
            assert_eq!(found.addr, Err(LookupError::Fail));
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 3, "{answer:?}");
            // retransmissions repeat the same bytes, case included
            assert!(seen.windows(2).all(|w| w[0].0 == w[1].0));
        }
    }

    /// A reply to another question fails the query (evdns.c:1110-1111).
    #[tokio::test]
    async fn a_reply_for_another_name_fails() {
        let (server, _) = fake_server(|q| {
            let mut r = answer_both(q)?;
            r.queries[0] =
                Query::query(Name::from_ascii("evil.example.").unwrap(), RecordType::AAAA);
            Some(r)
        })
        .await;
        let found = resolver(server).lookup("www.ripe.net", 6).await;
        assert_eq!(found.addr, Err(LookupError::Fail));
    }

    /// An NXDOMAIN with another question, or none, or a body that does
    /// not parse, is still NXDOMAIN: reply_parse's `err` passes the
    /// header's rcode on (evdns.c:1225-1227, 898-907). Starla failed it
    /// with EAI_FAIL, "non-recoverable failure in name resolution", where
    /// ping.c writes "dns resolution failed: nodename nor servname
    /// provided, or not known".
    #[test]
    fn nxdomain_is_nxdomain_whatever_its_question() {
        let name = Name::from_ascii("www.ripe.net.").unwrap();
        let query = Message::from_vec(&query_packet(7, &name, RecordType::AAAA).unwrap()).unwrap();
        let sort =
            |r: &Message| parse_reply(&r.to_vec().unwrap(), 7, &name, RecordType::AAAA, true);
        let other = Query::query(Name::from_ascii("evil.example.").unwrap(), RecordType::AAAA);

        let mut nx = reply(&query, 3);
        nx.queries[0] = other.clone();
        assert!(matches!(sort(&nx), Some(Reply::Answer(Answer::NotExist))));
        nx.queries.clear();
        assert!(matches!(sort(&nx), Some(Reply::Answer(Answer::NotExist))));
        let mut bytes = reply(&query, 3).to_vec().unwrap();
        bytes.truncate(14); // a question cut short
        assert!(matches!(
            parse_reply(&bytes, 7, &name, RecordType::AAAA, true),
            Some(Reply::Answer(Answer::NotExist))
        ));

        // rcode 0 with another question: DNS_ERR_UNKNOWN, EAI_FAIL
        let mut r = reply(&query, 0);
        r.queries[0] = other;
        assert!(matches!(sort(&r), Some(Reply::Answer(Answer::Failed))));
        // truncated NXDOMAIN: DNS_ERR_TRUNCATED, EAI_FAIL
        let mut tc = reply(&query, 3);
        tc.metadata.truncation = true;
        assert!(matches!(sort(&tc), Some(Reply::Answer(Answer::Failed))));
    }

    #[tokio::test]
    async fn the_hosts_file_answers_first() {
        let (server, seen) = fake_server(answer_both).await;
        let mut resolver = resolver(server);
        resolver.hosts = parse_hosts(
            "# comment\n127.0.0.1\tlocalhost\n::1 localhost ip6-localhost # x\n192.0.2.1 \
             v4only.lan\n",
        );
        let found = resolver.lookup("LocalHost", 6).await;
        assert_eq!(found.addr, Ok("::1".parse().unwrap()));
        assert!(found.dst_name.ttr.is_some());
        let found = resolver.lookup("localhost", 4).await;
        assert_eq!(found.addr, Ok("127.0.0.1".parse().unwrap()));
        // the name is there, but not in the family: an empty answer
        let found = resolver.lookup("v4only.lan", 6).await;
        assert_eq!(found.addr, Err(LookupError::NoAddress));
        assert!(seen.lock().unwrap().is_empty());
    }

    /// An address literal is measured without a lookup and reported with
    /// no ttr.
    #[tokio::test]
    async fn a_literal_needs_no_lookup() {
        let resolver = Resolver::default();
        let found = resolver.lookup("2001:7fd::1", 6).await;
        assert_eq!(found.addr, Ok("2001:7fd::1".parse().unwrap()));
        assert_eq!(
            found.dst_name,
            DstName {
                name: "2001:7fd::1".into(),
                ttr: None
            }
        );
    }

    /// ping.c:253-269 for a name and for a literal, as the twin's reference
    /// lines for 1040114 and 2001 have them.
    #[test]
    fn dst_name_fields_are_written_as_the_reference_writes_them() {
        let name = DstName {
            name: "bits.wikimedia.org".into(),
            ttr: Some(1.030459),
        };
        assert_eq!(
            name.fields(),
            r#""dst_name":"bits.wikimedia.org", "ttr":1.030459"#
        );
        let literal = DstName {
            name: "2001:db8:7769:6e00::20".into(),
            ttr: None,
        };
        assert_eq!(literal.fields(), r#""dst_name":"2001:db8:7769:6e00::20""#);
    }

    #[test]
    fn resolv_conf_is_read_as_evdns_reads_it() {
        let conf = ResolvConf::parse(
            "# atlas-twin\nnameserver \
             172.30.74.20\nnameserver\t2001:db8:7769:6e00::20\nnameserver \
             [2001:db8::53]:5353\nnameserver 192.0.2.53:54\nnameserver bogus\nnameserver \
             172.30.74.20:53\ndomain example.net\nsearch .a.example b.example\noptions ndots:2 \
             timeout:1.5 attempts:2 rotate max-timeouts:300 initial-probe-timeout:4000\n",
            Some("probe.lan.example"),
        );
        assert_eq!(
            conf.nameservers,
            [
                "172.30.74.20:53".parse().unwrap(),
                "[2001:db8:7769:6e00::20]:53".parse().unwrap(),
                "[2001:db8::53]:5353".parse().unwrap(),
                "192.0.2.53:54".parse().unwrap(),
            ]
        );
        assert_eq!(conf.search, ["a.example", "b.example"]);
        assert_eq!(conf.ndots, 2);
        assert_eq!(conf.timeout, Duration::from_millis(1500));
        assert_eq!(conf.attempts, 2);
        assert!(conf.randomize_case);
        // strtoint_clipped(1, 255); at most an hour
        assert_eq!(conf.max_timeouts, 255);
        assert_eq!(conf.probe_timeout, Duration::from_secs(3600));

        // nothing: 127.0.0.1, evdns's defaults, the host name's domain
        let conf = ResolvConf::parse("", Some("probe.lan.example"));
        assert_eq!(conf.nameservers, ["127.0.0.1:53".parse().unwrap()]);
        assert_eq!(conf.search, ["lan.example"]);
        assert_eq!((conf.ndots, conf.attempts, conf.max_timeouts), (1, 3, 3));
        assert_eq!(conf.timeout, Duration::from_secs(5));
        assert_eq!(conf.probe_timeout, Duration::from_secs(10));
        assert!(ResolvConf::parse("", Some("aletheia")).search.is_empty());
        assert!(!ResolvConf::parse("options randomize-case:0", None).randomize_case);
    }

    #[test]
    fn search_candidates_follow_ndots() {
        let mut resolver = Resolver::default();
        assert_eq!(resolver.candidates("www.ripe.net"), ["www.ripe.net"]);
        resolver.conf.search = vec!["a.example".into(), "b.example".into()];
        assert_eq!(
            resolver.candidates("www.ripe.net"),
            [
                "www.ripe.net",
                "www.ripe.net.a.example",
                "www.ripe.net.b.example"
            ]
        );
        assert_eq!(
            resolver.candidates("printer"),
            ["printer.a.example", "printer.b.example", "printer"]
        );
    }

    /// With a search list, NXDOMAIN moves on to the next name.
    #[tokio::test]
    async fn nxdomain_tries_the_search_list() {
        let (server, seen) = fake_server(|q| {
            if q.queries[0]
                .name()
                .to_ascii()
                .eq_ignore_ascii_case("printer.lan.")
            {
                answer_both(q)
            } else {
                Some(reply(q, 3))
            }
        })
        .await;
        let mut resolver = resolver(server);
        resolver.conf.search = vec!["corp".into(), "lan".into()];
        let found = resolver.lookup("printer", 4).await;
        assert_eq!(found.addr, Ok(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7))));
        let asked: Vec<String> = queries(&seen)
            .iter()
            .map(|q| q.queries[0].name().to_ascii().to_lowercase())
            .collect();
        assert_eq!(asked, ["printer.corp.", "printer.lan."]);
    }

    /// RIPE's libevent takes an IPv6 zone on a nameserver
    /// (evutil_inet_pton_scope): an interface name or an index. A zone
    /// that is neither drops the line, as does a port of 0.
    #[test]
    fn scoped_nameservers_keep_their_zone() {
        let lo = if cfg!(target_os = "linux") {
            "lo"
        } else {
            "lo0"
        };
        let lo_index = zone_index(lo).unwrap();
        assert!(lo_index > 0);
        let conf = ResolvConf::parse(
            &format!(
                "nameserver fe80::1%{lo}\nnameserver fe80::2%7\nnameserver \
                 [fe80::3%{lo}]:5353\nnameserver fe80::4%nosuchif0\nnameserver \
                 192.0.2.1:0\nnameserver fe80::5%\n"
            ),
            None,
        );
        let v6 = |ip: &str, port, scope| {
            SocketAddr::V6(SocketAddrV6::new(ip.parse().unwrap(), port, 0, scope))
        };
        assert_eq!(
            conf.nameservers,
            [
                v6("fe80::1", 53, lo_index),
                v6("fe80::2", 53, 7),
                v6("fe80::3", 5353, lo_index),
                v6("fe80::5", 53, 0),
            ]
        );
        // the only nameserver, dropped: evdns's 127.0.0.1
        assert_eq!(
            ResolvConf::parse("nameserver fe80::4%nosuchif0\n", None).nameservers,
            ["127.0.0.1:53".parse().unwrap()]
        );
    }

    /// Windows has no resolv.conf: the OS's servers and search list, and
    /// no 127.0.0.1 when it has none (that left every lookup to time out
    /// three times over, where std's resolver had used the OS's servers).
    #[tokio::test]
    async fn the_os_servers_stand_in_for_resolv_conf() {
        let conf = ResolvConf::from_os(
            [
                "192.0.2.53".parse().unwrap(),
                "2001:db8::53".parse().unwrap(),
            ],
            Some("corp.example."),
            Vec::new(),
        );
        assert_eq!(
            conf.nameservers,
            [
                "192.0.2.53:53".parse().unwrap(),
                "[2001:db8::53]:53".parse().unwrap()
            ]
        );
        assert_eq!(conf.search, ["corp.example"]);
        assert_eq!((conf.timeout, conf.attempts), (Duration::from_secs(5), 3));
        let conf = ResolvConf::from_os(
            std::iter::empty(),
            Some("corp.example"),
            ["a.example.".to_string(), "b.example".to_string()],
        );
        assert_eq!(conf.search, ["a.example", "b.example"]);

        let none = ResolvConf::from_os(std::iter::empty(), None, Vec::new());
        assert!(none.nameservers.is_empty());
        let start = Instant::now();
        let found = Resolver::new(none, Vec::new())
            .lookup("www.ripe.net", 6)
            .await;
        assert_eq!(found.addr, Err(LookupError::Fail));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    /// On Windows the servers are the adapters' (hickory-resolver's
    /// read_system_conf), not resolv.conf's 127.0.0.1.
    #[cfg(windows)]
    #[test]
    fn windows_takes_the_adapters_servers() {
        let (config, _) = hickory_resolver::system_conf::read_system_conf().unwrap();
        let conf = ResolvConf::system();
        assert!(!conf.nameservers.is_empty());
        for server in &conf.nameservers {
            assert_eq!(server.port(), 53);
            assert!(config.name_servers().iter().any(|ns| ns.ip == server.ip()));
        }
    }

    /// evdns keeps one socket per server: every query to a server leaves
    /// from the same port (the twin's reference: 79 queries from two ports,
    /// one per server; Starla's from a new port each). The servers take turns
    /// in evdns's ring order, resolv.conf's A, B, C as A, C, B.
    #[tokio::test]
    async fn one_socket_per_server_taking_turns_in_ring_order() {
        let (a, seen_a) = fake_server(answer_both).await;
        let (b, seen_b) = fake_server(answer_both).await;
        let (c, seen_c) = fake_server(answer_both).await;
        let resolver = resolver_of(&[a, b, c]);
        let order = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..6 {
            resolver.lookup("www.ripe.net", 6).await.addr.unwrap();
            let counts = [&seen_a, &seen_b, &seen_c].map(|s| s.lock().unwrap().len());
            order.lock().unwrap().push(counts);
        }
        let order = order.lock().unwrap();
        // which server each lookup went to
        let went: Vec<usize> = order
            .iter()
            .scan([0usize; 3], |before, now| {
                let i = (0..3).find(|&i| now[i] > before[i]).unwrap();
                *before = *now;
                Some(i)
            })
            .collect();
        assert_eq!(went, [0, 2, 1, 0, 2, 1]);
        for seen in [&seen_a, &seen_b, &seen_c] {
            let ports = sources(seen);
            assert_eq!(ports.len(), 2);
            assert_eq!(ports[0], ports[1], "one source port per server");
        }
        assert_ne!(sources(&seen_a)[0], sources(&seen_b)[0]);
    }

    /// A server that refuses is marked down and the query reissued to the
    /// next (request_reissue), which sends it when its timer runs out;
    /// later lookups pass it over until its probe, an A query for
    /// google.com `initial-probe-timeout` later, gets a reply. evdns
    /// retransmits the probe like any query, so the refusal sends it on to
    /// the good server, whose answer brings the refusing one back up.
    #[tokio::test]
    async fn a_server_that_fails_is_passed_over_until_a_probe() {
        let (refusing, seen_r) = fake_server(|q| Some(reply(q, 5))).await;
        let (good, seen_g) = fake_server(answer_both).await;
        let mut resolver = resolver_of(&[refusing, good]);
        resolver.conf.timeout = Duration::from_millis(100);
        resolver.conf.probe_timeout = Duration::from_millis(300);
        resolver.base = Base::new(&resolver.conf);

        // the reissue waits for the query's timer
        let found = resolver.lookup("www.ripe.net", 6).await;
        assert!(found.addr.is_ok());
        assert!(found.dst_name.ttr.unwrap() >= 100.0, "{found:?}");
        assert_eq!(
            (seen_r.lock().unwrap().len(), seen_g.lock().unwrap().len()),
            (1, 1)
        );
        for _ in 0..3 {
            resolver.lookup("www.ripe.net", 6).await.addr.unwrap();
        }
        assert_eq!(seen_r.lock().unwrap().len(), 1, "passed over while down");
        assert_eq!(seen_g.lock().unwrap().len(), 4);

        tokio::time::sleep(Duration::from_millis(600)).await;
        let probes: Vec<Message> = queries(&seen_r).into_iter().skip(1).collect();
        assert_eq!(probes.len(), 1, "{probes:?}");
        let q = &probes[0].queries[0];
        assert_eq!(q.query_type(), RecordType::A);
        assert!(q.name().to_ascii().eq_ignore_ascii_case("google.com."));
        assert!(queries(&seen_g).iter().any(|m| m.queries[0]
            .name()
            .to_ascii()
            .eq_ignore_ascii_case("google.com.")));
        // up again: the next lookup tries it first
        let before = seen_r.lock().unwrap().len();
        resolver.lookup("www.ripe.net", 6).await.addr.unwrap();
        assert_eq!(seen_r.lock().unwrap().len(), before + 1);
    }

    /// A server the query cannot even be sent to (an IPv4 resolver on an
    /// IPv6-only probe, say) is marked down at once (evdns.c:2236-2241):
    /// the lookup that found out waits out its timeout once, the next ones
    /// go straight to the server that works.
    #[tokio::test]
    async fn a_server_that_cannot_be_sent_to_goes_down() {
        let (good, seen_g) = fake_server(answer_both).await;
        // no SO_BROADCAST: sendto fails with EACCES
        let unsendable: SocketAddr = "255.255.255.255:53".parse().unwrap();
        let mut resolver = resolver_of(&[unsendable, good]);
        resolver.conf.timeout = Duration::from_millis(300);
        let first = resolver.lookup("www.ripe.net", 6).await;
        assert!(first.addr.is_ok());
        assert!(first.dst_name.ttr.unwrap() >= 300.0, "{first:?}");
        let second = resolver.lookup("www.ripe.net", 6).await;
        assert!(second.addr.is_ok());
        assert!(second.dst_name.ttr.unwrap() < 300.0, "{second:?}");
        assert_eq!(seen_g.lock().unwrap().len(), 2);
    }

    /// With randomize-case off evdns compares the question exactly.
    #[tokio::test]
    async fn without_randomize_case_the_question_must_match_exactly() {
        let (server, _) = fake_server(|q| {
            let mut r = answer_both(q)?;
            let name = r.queries[0].name().to_lowercase();
            r.queries[0] = Query::query(name, RecordType::AAAA);
            Some(r)
        })
        .await;
        let mut resolver = resolver(server);
        assert!(resolver.lookup("WWW.ripe.net", 6).await.addr.is_ok());
        resolver.conf.randomize_case = false;
        assert_eq!(
            resolver.lookup("WWW.ripe.net", 6).await.addr,
            Err(LookupError::Fail)
        );
        assert!(resolver.lookup("www.ripe.net", 6).await.addr.is_ok());
    }

    #[test]
    fn probes_back_off_threefold_to_an_hour() {
        let ten = Duration::from_secs(10);
        assert_eq!(probe_backoff(ten, 1), Duration::from_secs(30));
        assert_eq!(probe_backoff(ten, 2), Duration::from_secs(90));
        assert_eq!(probe_backoff(ten, 6), Duration::from_secs(3600));
        assert_eq!(probe_backoff(ten, 60), Duration::from_secs(3600));
    }

    /// Through the machine's own resolv.conf: `cargo test -p
    /// starla-measurements --lib target -- --ignored`.
    #[tokio::test]
    #[ignore = "asks the machine's resolvers"]
    async fn live_lookup_through_resolv_conf() {
        let found = lookup("www.ripe.net", 6).await;
        assert!(matches!(found.addr, Ok(IpAddr::V6(_))), "{found:?}");
        assert!(found.dst_name.ttr.is_some_and(|t| t > 0.0));
        let found = lookup("www.ripe.net", 4).await;
        assert!(matches!(found.addr, Ok(IpAddr::V4(_))), "{found:?}");
        let found = lookup("nonexistent.invalid", 6).await;
        assert_eq!(found.addr, Err(LookupError::NoName), "{found:?}");
    }
}
