//! Where sslgetcert connects: tcputil's name resolution
//! (probe-busybox/eperd/tcputil.c, tu_connect_to_name() and dns_cb()) and
//! atlas_check_addr() (probe-busybox/libbb/atlas_check_addr.c).
//!
//! The reference resolves through libevent's evdns_getaddrinfo() with the
//! family of `-4`/`-6`: one A or one AAAA query (both, A answers first,
//! with neither), no EDNS, the name in 0x20 mixed case, each query to one
//! nameserver. Its ttr is the lookup's time and a failure is reported as
//! "dnserr" with evutil_gai_strerror()'s text. Starla used to resolve in
//! the scheduler with getaddrinfo(AF_UNSPEC) — an A and an AAAA query for
//! `-6` — and a failure produced no result at all.

use hickory_resolver::config::{
    ConnectionConfig, NameServerConfig, ProtocolConfig, ResolveHosts, ResolverConfig, ResolverOpts,
    ServerOrderingStrategy,
};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::net::udp::UdpClientStream;
use hickory_resolver::net::xfer::DnsExchange;
use hickory_resolver::net::{DnsError, NetError};
use hickory_resolver::proto::op::ResponseCode;
use hickory_resolver::proto::rr::{Name, RData, RecordType};
use hickory_resolver::{ConnectionProvider, PoolContext, Resolver};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// evutil_gai_strerror(EVUTIL_EAI_NONAME) (libevent 2.1.11 evutil.c:1665):
/// the name does not exist (DNS_ERR_NOTEXIST, an NXDOMAIN).
pub const EAI_NONAME: &str = "nodename nor servname provided, or not known";

/// evutil_gai_strerror(EVUTIL_EAI_FAIL) (evutil.c:1657): every other
/// failure. evdns_err_to_getaddrinfo_err() (evdns.c:4381) maps all but
/// NOTEXIST here, including DNS_ERR_NODATA — a NOERROR answer without
/// records of the type (evdns.c:909) — and timeouts.
pub const EAI_FAIL: &str = "non-recoverable failure in name resolution";

/// What evdns_getaddrinfo() gives dns_cb(): the addresses to try in order,
/// or the error text.
pub type Resolved = Result<Vec<IpAddr>, &'static str>;

/// Whose turn it is among the nameservers. evdns's nameserver_pick()
/// (libevent 2.1.11 evdns.c:1348-1384) hands each new request (:2855) the
/// server at server_head and moves server_head on to the next — it "load
/// balance[s] across the rest by updating the server_head global each
/// time" — and eperd has one DnsBase for all its measurements (eperd.c:349),
/// so the reference's queries go round its resolv.conf servers one by one.
/// In the twin run they alternate strictly between 172.30.74.20 and
/// 2001:db8:7769:6e00::20 (probe-ref.pcap), and the 15001 AAAA query went
/// to the second. The turn here is sslgetcert's own; the other
/// measurements' queries do not move it.
static NEXT_SERVER: AtomicUsize = AtomicUsize::new(0);

/// Resolve `host` for family `af` (4, 6, or 0 for either) and time it in
/// ms, as dns_cb() does (tcputil.c:230-239). An address literal of the
/// family needs no query; one of the other family is looked up as a name,
/// as evutil_getaddrinfo_common_() does. `nameservers` replace
/// /etc/resolv.conf (and the hosts file) in tests; empty is the system's.
pub async fn resolve(host: &str, af: u8, nameservers: &[SocketAddr]) -> (Resolved, f64) {
    resolve_taking_turns(host, af, nameservers, &NEXT_SERVER, None).await
}

/// `timeout` replaces the timeout of each transmission (tests).
async fn resolve_taking_turns(
    host: &str,
    af: u8,
    nameservers: &[SocketAddr],
    next_server: &AtomicUsize,
    timeout: Option<Duration>,
) -> (Resolved, f64) {
    let start = Instant::now();
    let elapsed = |start: Instant| start.elapsed().as_secs_f64() * 1000.0;
    if let Ok(ip) = host.parse::<IpAddr>() {
        if af == 0 || (af == 4) == ip.is_ipv4() {
            return (Ok(vec![ip]), elapsed(start));
        }
    }
    let Ok((config, mut opts)) = config(nameservers) else {
        return (Err(EAI_FAIL), elapsed(start));
    };
    if let Some(timeout) = timeout {
        opts.timeout = timeout;
    }
    // Each query is a request of its own with a turn of its own; with
    // neither -4 nor -6 the A request is made first (evdns.c:4831-4852)
    let rtypes: &[RecordType] = match af {
        4 => &[RecordType::A],
        6 => &[RecordType::AAAA],
        _ => &[RecordType::A, RecordType::AAAA],
    };
    let first = next_server.fetch_add(rtypes.len(), Ordering::Relaxed);
    let mut resolvers = Vec::with_capacity(rtypes.len());
    for turn in first..first + rtypes.len() {
        match resolver(&config, &opts, turn) {
            Ok(r) => resolvers.push(r),
            Err(_) => return (Err(EAI_FAIL), elapsed(start)),
        }
    }
    let start = Instant::now();
    let res = match (&resolvers[..], rtypes) {
        ([r], [rtype]) => lookup(r, host, *rtype).await,
        ([a, aaaa], _) => lookup_both(a, aaaa, host).await,
        _ => Err(EAI_FAIL),
    };
    (res, elapsed(start))
}

/// evdns's 0x20 (request_new(), libevent 2.1.11 evdns.c:2824-2836): once
/// per request, each letter of the name is put in lower case if its random
/// bit is set and in upper case if not; everything else is left. The reply
/// is then taken whatever the case of its question (reply_parse(),
/// :1098-1103, evutil_ascii_strcasecmp()), which is what hickory does with
/// its own randomization off. With it on, hickory refused a reply in
/// another case as forged and asked again over TCP: behind a forwarder
/// that folds case, a lookup the reference makes failed with "dnserr".
fn randomize_case(host: &str) -> String {
    let bits: Vec<u8> = (0..host.len().div_ceil(8))
        .map(|_| rand::random::<u8>())
        .collect();
    host.bytes()
        .enumerate()
        .map(|(i, c)| {
            let lower = bits[i >> 3] & (1 << (i & 7)) != 0;
            match (c.is_ascii_alphabetic(), lower) {
                (false, _) => c as char,
                (true, true) => c.to_ascii_lowercase() as char,
                (true, false) => c.to_ascii_uppercase() as char,
            }
        })
        .collect()
}

/// The nameservers and options to ask with: the system's (resolv.conf and
/// the hosts file, as evdns_base_resolv_conf_parse() with DNS_OPTIONS_ALL
/// reads them, eperd.c:366), or the given ones in tests; the servers in
/// the order evdns goes round them.
fn config(nameservers: &[SocketAddr]) -> Result<(ResolverConfig, ResolverOpts), NetError> {
    let (mut config, opts) = if nameservers.is_empty() {
        hickory_resolver::system_conf::read_system_conf()?
    } else {
        let servers = nameservers
            .iter()
            .map(|ns| {
                let mut udp = ConnectionConfig::udp();
                udp.port = ns.port();
                NameServerConfig::new(ns.ip(), true, vec![udp])
            })
            .collect();
        let mut opts = ResolverOpts::default();
        opts.use_hosts_file = ResolveHosts::Never;
        (ResolverConfig::from_name_servers(servers), opts)
    };
    ring_order(&mut config.name_servers);
    Ok((config, opts))
}

/// evdns links each server it adds in right after server_head
/// (evdns.c:2586-2595), so it goes round resolv.conf's a, b, c, d as
/// a, d, c, b.
fn ring_order<T>(servers: &mut [T]) {
    if servers.len() > 2 {
        servers[1..].reverse();
    }
}

/// How many times evdns sends a request before it gives up:
/// global_max_retransmits (evdns.c:4010). resolv.conf's `attempts:` is not
/// read for it.
const TRANSMISSIONS: usize = 3;

/// evdns's transport, for hickory: one UDP datagram per transmission and
/// `timeout` for its answer. evdns sends a request once and arms
/// global_timeout (evdns_request_transmit(), evdns.c:2257); when that runs
/// out it sends it again to the next nameserver
/// (evdns_request_timeout_callback(), :2179-2210, request_swap_ns(req,
/// nameserver_pick(base))), three times in all, and only then fails
/// (DNS_ERR_TIMEOUT, EAI_FAIL). hickory's UDP stream sends the same query
/// to the same server again after 333 ms and 666 ms
/// (DEFAULT_RETRY_FLOOR, max_retries 3) and its pool spends the whole
/// timeout on the first server, so behind a nameserver that does not
/// answer it never asked the next one: the reference's lookup took 5 s
/// and a bit (RIPE stored "ttr":5174.370657 for its 15001 on 2026-10-13),
/// Starla's failed after 15 s with nine datagrams to the silent server.
/// evdns asks over UDP alone, so this has no TCP.
#[derive(Clone)]
struct EvdnsUdp {
    runtime: TokioRuntimeProvider,
    /// global_timeout: resolv.conf's `timeout:`, 5 s without it (:4007)
    timeout: Duration,
}

type FutureExchange =
    Pin<Box<dyn Future<Output = Result<DnsExchange<TokioRuntimeProvider>, NetError>> + Send>>;

impl ConnectionProvider for EvdnsUdp {
    type Conn = DnsExchange<TokioRuntimeProvider>;
    type FutureConn = FutureExchange;
    type RuntimeProvider = TokioRuntimeProvider;

    fn new_connection(
        &self,
        ip: IpAddr,
        config: &ConnectionConfig,
        cx: &PoolContext,
    ) -> Result<Self::FutureConn, NetError> {
        if !matches!(config.protocol, ProtocolConfig::Udp) {
            return Err(NetError::from("evdns asks over UDP only"));
        }
        let stream =
            UdpClientStream::builder(SocketAddr::new(ip, config.port), self.runtime.clone())
                .with_timeout(Some(self.timeout))
                .with_max_retries(1)
                .with_os_port_selection(cx.options.os_port_selection)
                .avoid_local_ports(cx.options.avoid_local_udp_ports.clone())
                .with_bind_addr(config.bind_addr);
        Ok(Box::pin(async move { Ok(stream.exchange()) }))
    }

    fn runtime_provider(&self) -> &Self::RuntimeProvider {
        &self.runtime
    }
}

type EvdnsResolver = Resolver<EvdnsUdp>;

/// A resolver for one request that asks as evdns asks: no EDNS OPT record,
/// no case check of its own (the name comes cased by [`randomize_case`] —
/// evdns's randomize-case defaults to on, evdns.c:4013 — and the reply is
/// taken in any case), nothing cached between measurements, and its three
/// transmissions ([`EvdnsUdp`]) to the nameserver whose `turn` it is and
/// the next two round the ring, as each retransmit picks the next one
/// (nameserver_pick(), evdns.c:2203). hickory's defaults would send each
/// query to two servers at once (num_concurrent_reqs = 2), picked by a
/// statistic that starts out random.
fn resolver(
    config: &ResolverConfig,
    opts: &ResolverOpts,
    turn: usize,
) -> Result<EvdnsResolver, NetError> {
    let mut config = config.clone();
    let ring = std::mem::take(&mut config.name_servers);
    if !ring.is_empty() {
        config.name_servers = (turn..turn + TRANSMISSIONS)
            .map(|i| ring[i % ring.len()].clone())
            .collect();
    }
    let provider = EvdnsUdp {
        runtime: TokioRuntimeProvider::default(),
        timeout: opts.timeout,
    };
    let mut builder = Resolver::builder_with_config(config, provider);
    let o = builder.options_mut();
    *o = opts.clone();
    o.edns0 = false;
    o.case_randomization = false;
    o.num_concurrent_reqs = 1;
    o.server_ordering_strategy = ServerOrderingStrategy::UserProvidedOrder;
    // The transmissions are the server list, taken in order within one
    // pass of the pool, whose deadline is this; nothing is sent again
    // after that
    o.timeout = opts.timeout * TRANSMISSIONS as u32 + Duration::from_secs(1);
    o.attempts = 0;
    builder.build()
}

/// One request: the name in evdns's 0x20 case. hickory's `IntoName for
/// &str` goes through IDNA, which puts it all in lower case; an ASCII name
/// keeps its case through `Name::from_ascii()` (one that is not ASCII goes
/// the IDNA way, uncased).
async fn lookup(resolver: &EvdnsResolver, host: &str, rtype: RecordType) -> Resolved {
    let name = match Name::from_ascii(randomize_case(host)) {
        Ok(name) => Ok(name),
        Err(_) => Name::from_utf8(host),
    };
    let Ok(name) = name else {
        return Err(EAI_FAIL);
    };
    match resolver.lookup(name, rtype).await {
        Ok(lookup) => {
            let addrs: Vec<IpAddr> = lookup
                .answers()
                .iter()
                .filter_map(|r| match &r.data {
                    RData::A(a) => Some(IpAddr::V4(a.0)),
                    RData::AAAA(a) => Some(IpAddr::V6(a.0)),
                    _ => None,
                })
                .collect();
            if addrs.is_empty() {
                Err(EAI_FAIL)
            } else {
                Ok(addrs)
            }
        }
        Err(NetError::Dns(DnsError::NoRecordsFound(nr)))
            if nr.response_code == ResponseCode::NXDomain =>
        {
            Err(EAI_NONAME)
        }
        Err(_) => Err(EAI_FAIL),
    }
}

/// Neither -4 nor -6: evdns_getaddrinfo() sends both queries and puts the
/// IPv4 answers first (evdns.c:4652-4660). One failing is ignored; both
/// failing is the error of the one answered last (getaddrinfo_merge_err(),
/// evdns.c:4393).
async fn lookup_both(a: &EvdnsResolver, aaaa: &EvdnsResolver, host: &str) -> Resolved {
    let a = lookup(a, host, RecordType::A);
    let aaaa = lookup(aaaa, host, RecordType::AAAA);
    tokio::pin!(a, aaaa);
    let (mut ra, mut raaaa): (Option<Resolved>, Option<Resolved>) = (None, None);
    let mut last_err = EAI_FAIL;
    while ra.is_none() || raaaa.is_none() {
        tokio::select! {
            // the A query goes out first
            biased;
            r = &mut a, if ra.is_none() => {
                if let Err(e) = r {
                    last_err = e;
                }
                ra = Some(r);
            }
            r = &mut aaaa, if raaaa.is_none() => {
                if let Err(e) = r {
                    last_err = e;
                }
                raaaa = Some(r);
            }
        }
    }
    let mut addrs = ra.and_then(Result::ok).unwrap_or_default();
    addrs.extend(raaaa.and_then(Result::ok).unwrap_or_default());
    if addrs.is_empty() {
        Err(last_err)
    } else {
        Ok(addrs)
    }
}

/// `ATLAS_DISABLE_CHECK_ADDR=yes` lets every address through, as in
/// atlas_check_addr.c:58-63.
pub fn check_addr_disabled() -> bool {
    std::env::var("ATLAS_DISABLE_CHECK_ADDR").is_ok_and(|v| v == "yes")
}

/// atlas_check_addr(): the reference refuses to measure loopback, RFC 1918,
/// link-local and multicast IPv4, and ::1, IPv4-mapped and e000::/3 (ULA,
/// link-local, multicast) IPv6; dns_cb() then reports "address not allowed"
/// instead of connecting.
pub fn addr_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            const BAD: [(u32, u32); 6] = [
                (0x7F00_0000, 8),
                (0x0A00_0000, 8),
                (0xAC10_0000, 12),
                (0xC0A8_0000, 16),
                (0xA9FE_0000, 16),
                (0xE000_0000, 4),
            ];
            let a = u32::from(a);
            !BAD.iter()
                .any(|&(prefix, len)| a & (u32::MAX << (32 - len)) == prefix)
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            !(s == [0, 0, 0, 0, 0, 0, 0, 1]
                || s[..6] == [0, 0, 0, 0, 0, 0xffff]
                || s[0] & 0xe000 == 0xe000)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use hickory_resolver::proto::op::{Message, MessageType, OpCode};
    use hickory_resolver::proto::rr::rdata::{A, AAAA};
    use hickory_resolver::proto::rr::Record;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::sync::{Arc, Mutex};
    use tokio::net::UdpSocket;

    /// What a fake nameserver answers for a name.
    #[derive(Clone, Copy)]
    pub(crate) enum Zone {
        /// A 127.0.0.1 and AAAA ::1
        Loopback,
        /// As Loopback, with the question and the answer's owner in lower
        /// case, as a forwarder that folds case answers
        LowerCase,
        /// NXDOMAIN
        Missing,
        /// NOERROR without records
        NoData,
        /// No answer at all
        Silent,
        /// As Loopback, after this long
        Slow(std::time::Duration),
    }

    /// The queries a fake nameserver saw, as (type, name, had an OPT record).
    pub(crate) type Seen = Arc<Mutex<Vec<(RecordType, String, bool)>>>;

    /// A UDP nameserver on 127.0.0.1; returns its address and the queries it
    /// saw.
    pub(crate) async fn fake_nameserver(zone: Zone) -> (SocketAddr, Seen) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                    return;
                };
                let Ok(q) = Message::from_vec(&buf[..n]) else {
                    continue;
                };
                let query = q.queries[0].clone();
                log.lock().unwrap().push((
                    query.query_type(),
                    query.name().to_string(),
                    q.edns.is_some(),
                ));
                let mut resp = Message::new(q.metadata.id, MessageType::Response, OpCode::Query);
                resp.metadata.recursion_desired = q.metadata.recursion_desired;
                resp.metadata.recursion_available = true;
                let mut question = query.clone();
                match zone {
                    Zone::LowerCase => {
                        question.set_name(query.name().to_lowercase());
                    }
                    Zone::Slow(delay) => tokio::time::sleep(delay).await,
                    _ => {}
                }
                resp.add_query(question.clone());
                let name = question.name().clone();
                match zone {
                    Zone::Loopback | Zone::LowerCase | Zone::Slow(_) => {
                        let rdata = match query.query_type() {
                            RecordType::A => RData::A(A(Ipv4Addr::LOCALHOST)),
                            _ => RData::AAAA(AAAA(Ipv6Addr::LOCALHOST)),
                        };
                        resp.add_answer(Record::from_rdata(name, 60, rdata));
                    }
                    Zone::Missing => resp.metadata.response_code = ResponseCode::NXDomain,
                    Zone::NoData => {}
                    Zone::Silent => continue,
                }
                let _ = sock.send_to(&resp.to_vec().unwrap(), from).await;
            }
        });
        (addr, seen)
    }

    /// With -6 the reference sends one AAAA query (WwW.RipE.net. in the twin
    /// run); Starla sent an A and an AAAA.
    #[tokio::test]
    async fn minus_6_asks_for_aaaa_only_without_edns() {
        let (ns, seen) = fake_nameserver(Zone::Loopback).await;
        let (res, ttr) = resolve("www.ripe.net", 6, &[ns]).await;
        assert_eq!(res, Ok(vec![IpAddr::V6(Ipv6Addr::LOCALHOST)]));
        assert!(ttr > 0.0);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].0, RecordType::AAAA);
        assert!(seen[0].1.eq_ignore_ascii_case("www.ripe.net."), "{seen:?}");
        assert!(!seen[0].2, "evdns sends no OPT record");
    }

    /// evdns sends the name in 0x20 case and takes the reply whatever the
    /// case of its question: with randomize-case on, reply_parse() compares
    /// the names with evutil_ascii_strcasecmp() (libevent 2.1.11
    /// evdns.c:1098-1103). hickory's own case randomization refuses such a
    /// reply as forged (QueryCaseMismatch) and asks again over TCP — which a
    /// forwarder that folds case may not even listen on, as this one does
    /// not.
    #[tokio::test]
    async fn a_reply_in_another_case_is_taken_as_evdns_takes_it() {
        let (ns, seen) = fake_nameserver(Zone::LowerCase).await;
        for _ in 0..3 {
            let (res, _) = resolve("www.ripe.net", 6, &[ns]).await;
            assert_eq!(res, Ok(vec![IpAddr::V6(Ipv6Addr::LOCALHOST)]));
        }
        let seen = seen.lock().unwrap();
        // one query per lookup, all over UDP
        assert_eq!(seen.len(), 3, "{seen:?}");
        // and the name went out in mixed case (all three lower: 2^-30)
        assert!(
            seen.iter().any(|q| q.1 != q.1.to_ascii_lowercase()),
            "{seen:?}"
        );
        assert!(seen
            .iter()
            .all(|q| q.1.eq_ignore_ascii_case("www.ripe.net.")));
    }

    /// Letters take a case from their bit; digits, dots and hyphens stay.
    #[test]
    fn only_letters_change_case() {
        let mut upper = 0;
        for _ in 0..64 {
            let n = randomize_case("ns-1.ripe.net");
            assert!(n.eq_ignore_ascii_case("ns-1.ripe.net"), "{n}");
            assert_eq!(&n[2..5], "-1.");
            upper += n.bytes().filter(u8::is_ascii_uppercase).count();
        }
        // 9 letters a time: about half of 576 in upper case
        assert!((150..430).contains(&upper), "{upper}");
    }

    #[tokio::test]
    async fn minus_4_asks_for_a_only() {
        let (ns, seen) = fake_nameserver(Zone::Loopback).await;
        let (res, _) = resolve("www.ripe.net", 4, &[ns]).await;
        assert_eq!(res, Ok(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]));
        assert_eq!(seen.lock().unwrap()[0].0, RecordType::A);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn neither_puts_the_ipv4_answers_first() {
        let (ns, seen) = fake_nameserver(Zone::Loopback).await;
        let (res, _) = resolve("www.ripe.net", 0, &[ns]).await;
        assert_eq!(
            res,
            Ok(vec![
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST)
            ])
        );
        let types: Vec<RecordType> = seen.lock().unwrap().iter().map(|q| q.0).collect();
        assert_eq!(types, [RecordType::A, RecordType::AAAA]);
    }

    /// The query types each nameserver saw, once any query sent alongside
    /// has had time to land.
    async fn seen_by(servers: &[&Seen]) -> Vec<Vec<RecordType>> {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        servers
            .iter()
            .map(|s| s.lock().unwrap().iter().map(|q| q.0).collect())
            .collect()
    }

    /// The twin's resolv.conf has two nameservers, and hickory's defaults
    /// asked both at once: two AAAA queries for -6 where the reference sent
    /// one, four for neither -4 nor -6 where it sent two. evdns sends each
    /// request to one server, and the next request to the next one.
    #[tokio::test]
    async fn each_query_goes_to_one_nameserver_and_the_next_to_the_next() {
        let (ns1, seen1) = fake_nameserver(Zone::Loopback).await;
        let (ns2, seen2) = fake_nameserver(Zone::Loopback).await;
        let turns = AtomicUsize::new(0);
        let both = [ns1, ns2];
        use RecordType::{A, AAAA};

        let (res, _) = resolve_taking_turns("www.ripe.net", 6, &both, &turns, None).await;
        assert_eq!(res, Ok(vec![IpAddr::V6(Ipv6Addr::LOCALHOST)]));
        assert_eq!(seen_by(&[&seen1, &seen2]).await, [vec![AAAA], vec![]]);

        let (res, _) = resolve_taking_turns("www.ripe.net", 6, &both, &turns, None).await;
        assert!(res.is_ok());
        assert_eq!(seen_by(&[&seen1, &seen2]).await, [vec![AAAA], vec![AAAA]]);

        // neither: two requests, so one query to each
        let (res, _) = resolve_taking_turns("www.ripe.net", 0, &both, &turns, None).await;
        assert_eq!(res.map(|a| a.len()), Ok(2));
        assert_eq!(
            seen_by(&[&seen1, &seen2]).await,
            [vec![AAAA, A], vec![AAAA, AAAA]]
        );
    }

    /// A nameserver that does not answer gets the request once; the timeout
    /// later evdns sends it to the next one (evdns_request_timeout_callback(),
    /// evdns.c:2179-2210), and ttr is the wait and that answer. hickory sent
    /// the silent one the query again after 333 and 666 ms, spent its whole
    /// timeout there and never asked the next: the lookup failed.
    #[tokio::test]
    async fn a_silent_nameserver_is_left_for_the_next_after_the_timeout() {
        let (silent, seen_silent) = fake_nameserver(Zone::Silent).await;
        let (good, seen_good) = fake_nameserver(Zone::Loopback).await;
        let turns = AtomicUsize::new(0);
        let timeout = std::time::Duration::from_millis(1000);
        let (res, ttr) =
            resolve_taking_turns("www.ripe.net", 6, &[silent, good], &turns, Some(timeout)).await;
        assert_eq!(res, Ok(vec![IpAddr::V6(Ipv6Addr::LOCALHOST)]));
        assert!((1000.0..1900.0).contains(&ttr), "{ttr}");
        assert_eq!(
            seen_by(&[&seen_silent, &seen_good]).await,
            [vec![RecordType::AAAA], vec![RecordType::AAAA]]
        );
    }

    /// Three transmissions, each to the next nameserver round the ring, and
    /// then the lookup fails (DNS_ERR_TIMEOUT, EAI_FAIL).
    #[tokio::test]
    async fn no_answer_in_three_transmissions_is_a_failure() {
        let (a, seen_a) = fake_nameserver(Zone::Silent).await;
        let (b, seen_b) = fake_nameserver(Zone::Silent).await;
        let turns = AtomicUsize::new(0);
        let timeout = std::time::Duration::from_millis(200);
        let (res, ttr) =
            resolve_taking_turns("www.ripe.net", 4, &[a, b], &turns, Some(timeout)).await;
        assert_eq!(res, Err(EAI_FAIL));
        assert!((600.0..1500.0).contains(&ttr), "{ttr}");
        let counts: Vec<usize> = seen_by(&[&seen_a, &seen_b])
            .await
            .iter()
            .map(Vec::len)
            .collect();
        assert_eq!(counts, [2, 1]);
    }

    /// evdns goes round resolv.conf's a, b, c as a, c, b.
    #[tokio::test]
    async fn three_nameservers_are_taken_in_evdns_ring_order() {
        let (ns1, seen1) = fake_nameserver(Zone::Loopback).await;
        let (ns2, seen2) = fake_nameserver(Zone::Loopback).await;
        let (ns3, seen3) = fake_nameserver(Zone::Loopback).await;
        let turns = AtomicUsize::new(0);
        let all = [ns1, ns2, ns3];
        for expected in [[1, 0, 0], [1, 0, 1], [1, 1, 1], [2, 1, 1]] {
            let (res, _) = resolve_taking_turns("www.ripe.net", 4, &all, &turns, None).await;
            assert_eq!(res, Ok(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]));
            let seen = seen_by(&[&seen1, &seen2, &seen3]).await;
            assert_eq!(seen.iter().map(Vec::len).collect::<Vec<_>>(), expected);
        }
    }

    #[tokio::test]
    async fn nxdomain_and_nodata_are_evdns_errors() {
        let (ns, _) = fake_nameserver(Zone::Missing).await;
        assert_eq!(resolve("nx.example", 6, &[ns]).await.0, Err(EAI_NONAME));
        let (ns, _) = fake_nameserver(Zone::NoData).await;
        assert_eq!(resolve("v4only.example", 6, &[ns]).await.0, Err(EAI_FAIL));
    }

    #[tokio::test]
    async fn a_literal_of_the_family_is_not_looked_up() {
        let (ns, seen) = fake_nameserver(Zone::Missing).await;
        let (res, _) = resolve("2001:db8::1", 6, &[ns]).await;
        assert_eq!(res, Ok(vec!["2001:db8::1".parse().unwrap()]));
        // -6 with an IPv4 literal: evdns asks for its AAAA
        let (res, _) = resolve("192.0.2.1", 6, &[ns]).await;
        assert_eq!(res, Err(EAI_NONAME));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    /// atlas_check_addr.c's tables, both sides of each edge.
    #[test]
    fn the_reference_refuses_these_addresses() {
        let refused = [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.0.1",
            "224.0.0.1",
            "239.255.255.255",
            "::1",
            "::ffff:193.0.6.139",
            "fc00::1",
            "fd57:4700::1",
            "fe80::1",
            "ff02::1",
            "e000::1",
        ];
        let allowed = [
            "193.0.6.139",
            "172.15.255.255",
            "172.32.0.1",
            "11.0.0.1",
            "223.255.255.255",
            // 224.0.0.0/4 ends at 239: class E is let through
            "240.0.0.1",
            "255.255.255.255",
            "2001:67c:2e8:22::c100:68b",
            "2001:db8:7769:6e00::20",
            "::",
            "::2",
            "dfff::1",
        ];
        for a in refused {
            assert!(!addr_allowed(a.parse().unwrap()), "{a}");
        }
        for a in allowed {
            assert!(addr_allowed(a.parse().unwrap()), "{a}");
        }
    }
}
