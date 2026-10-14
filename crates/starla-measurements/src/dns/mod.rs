//! DNS measurement: evtdig, as probe-busybox/eperd/evtdig.c (firmware 5130)
//! asks and reports.
//!
//! One run is one query, or with `--resolv` one query to each of the probe's
//! resolvers (one `resultset` entry each), and its RESULT line is written
//! the way printReply() writes it, spacing and all: `"id" : "11001" , ` and
//! not `"id":"11001"`.

pub mod resolver;

use crate::traits::Measurement;
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use resolver::{Attempt, Dst, Outcome};
use serde::{Deserialize, Serialize};
use starla_common::{
    MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId, Timestamp,
};
use std::fmt::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// What stands for each `"lts"` value in the line until it is uploaded: the
/// seconds between that query's report and the end of the run, between two
/// of these. printReply() writes get_timesync() as it is when each query
/// ends (evtdig.c:3723), and Starla's lts is taken when the result is
/// queued, so starla-results' format.rs (`DNS_LTS_MARK`) puts the lts back
/// that many seconds. A control character: no part of the line can contain
/// one (names and TXT strings are escaped).
pub const LTS_MARK: char = '\u{1}';

/// evtdig's options, with tdig_init()'s defaults (evtdig.c:2397).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsConfig {
    /// The server, evtdig's one argument (an address or a name); unused with
    /// `resolv`
    pub server: String,
    /// 4 (`-4`, and the default) or 6 (`-6`), or 0 for either (`-4 -6`, `-a`)
    pub af: u8,
    /// `-t`: TCP
    pub tcp: bool,
    /// `--port` (53)
    pub port: u16,
    /// The query type and class: `--type`/`--class`, `--a NAME`, `--soa NAME`
    /// …; TXT CH (with `-b`, `-h`, `-i`, `-r`) unless told otherwise
    pub qtype: u16,
    pub qclass: u16,
    /// The name, ending with a dot, its `$p`/`$r`/`$t` not yet expanded
    pub query: String,
    /// `--resolv`: ask each of the probe's resolvers (/etc/resolv.conf), with
    /// RD, and report a `resultset`
    pub resolv: bool,
    /// `-I <ifname>`: with `--resolv`, read `/etc/resolv.conf.<ifname>` when
    /// it exists and fall back to `/etc/resolv.conf`, as
    /// get_local_resolvers() does (readresolv.c:54-67).
    #[serde(default)]
    pub interface: Option<String>,
    /// `-R`: set RD (evtdig never does otherwise)
    pub rd: bool,
    /// `--ad`, `--cd`: set those header bits
    pub ad: bool,
    pub cd: bool,
    /// `-d`: the OPT record's DO bit
    pub dnssec: bool,
    /// `-n`: an NSID option
    pub nsid: bool,
    /// `-c`: a client-subnet option, with a /0 prefix
    pub client_subnet: bool,
    /// `--cookies`: a client cookie
    pub cookies: bool,
    /// `-e`: the OPT record's UDP payload size (512). Above 512 it asks for
    /// an OPT record by itself.
    pub edns0: u32,
    /// `--edns-version`, `--edns-flags`, `--edns-option`
    pub edns_version: u8,
    pub edns_flags: u16,
    pub edns_option: u16,
    /// `--qbuf`: report the query sent
    pub qbuf: bool,
    /// Report the reply (true unless `--noabuf`)
    pub abuf: bool,
    /// `--p_probe_id`: ask for `<probe id>.<time>.<name>`
    pub prepend_probe_id: bool,
    /// `--timeout`: ms to wait for a UDP reply (5000)
    pub timeout_ms: u32,
    /// `--retry`: queries repeated after a timeout (0)
    pub retry: u32,
    /// `-B`: the bundle, reported as `"bundle"`
    pub bundle: Option<u64>,
    /// `--ttl`: report the TTL (hop limit) a UDP reply arrived with, as
    /// `"ttl"`
    #[serde(default)]
    pub ttl: bool,
    /// ATLAS_DISABLE_CHECK_ADDR=yes for this measurement alone: ask
    /// servers atlas_check_addr() refuses ([`resolver::address_allowed`]),
    /// as a test or a lab on a private network must
    #[serde(default)]
    pub disable_check_addr: bool,
    /// What earlier queries of this measurement left (see [`Carry`])
    #[serde(skip)]
    pub carry: Carry,
}

/// What the reference's query_state keeps from one query to the next for
/// as long as the measurement is scheduled. tdig_init() clears these once
/// (evtdig.c:2461-2466) and nothing clears them again, so printReply()
/// (evtdig.c:3785-3811) reports what the last query that set one left:
///
/// - `loc_sin6`, the source address, set by getsockname() after a UDP connect
///   or a TCP connection. A query that fails before that (a socket or connect
///   error, a refused address, a TCP connect that fails or times out) reports
///   the one before it: within a `--resolv` run the previous resolver's, for
///   the first resolver the previous run's. RIPE stored one in all 58 resultset
///   entries with a "socket" error from firmware 5130 on 2026-10-13 (msm 30001,
///   probe 1010265: `"dst_name":"fd7a:115c:a1e0::53",
///   "src_addr":"100.83.59.57"`, subid 1's source).
/// - `rcvdttl`, set by every UDP reply (process_reply(), evtdig.c:2032): with
///   `--ttl` a timeout or error after a reply still reports its `"ttl"`.
/// - `dst_ai_family`/`dst_addr_str`, set by tcp_beforeconnect() and
///   TU_BAD_ADDR: a later TCP query whose server does not resolve reports the
///   last address connected to beside its `"dst_name"`.
///
/// Clones share it, as the runs of one scheduled measurement do (each run
/// gets a clone of its config); a new CRONLINE starts afresh.
#[derive(Debug, Clone, Default)]
pub struct Carry(Arc<Mutex<Carried>>);

#[derive(Debug, Default)]
struct Carried {
    src: Option<SocketAddr>,
    ttl: Option<i32>,
    tcp_dst: Option<SocketAddr>,
}

impl Carry {
    /// Keep what `a` sets, and fill in what it lacks from earlier queries.
    fn apply(&self, a: &mut Attempt, tcp: bool) {
        let mut c = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match a.src {
            Some(src) => c.src = Some(src),
            None => a.src = c.src,
        }
        if !tcp && matches!(a.outcome, Outcome::Reply { .. }) {
            c.ttl = a.ttl;
        } else {
            a.ttl = c.ttl;
        }
        if tcp {
            match &a.dst {
                Dst::Tcp { addr, .. } => c.tcp_dst = Some(*addr),
                Dst::Name(server) => {
                    if let Some(addr) = c.tcp_dst {
                        a.dst = Dst::Tcp {
                            server: server.clone(),
                            addr,
                        };
                    }
                }
                _ => {}
            }
        }
    }
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            server: String::new(),
            af: 4,
            tcp: false,
            port: 53,
            qtype: 16, // T_TXT
            qclass: 3, // C_CHAOS
            query: String::new(),
            resolv: false,
            interface: None,
            rd: false,
            ad: false,
            cd: false,
            dnssec: false,
            nsid: false,
            client_subnet: false,
            cookies: false,
            edns0: 512,
            edns_version: 0,
            edns_flags: 0,
            edns_option: 0,
            qbuf: false,
            abuf: true,
            prepend_probe_id: false,
            timeout_ms: 5000,
            retry: 0,
            bundle: None,
            ttl: false,
            disable_check_addr: false,
            carry: Carry::default(),
        }
    }
}

pub struct Dns {
    pub config: DnsConfig,
    /// For `$p` and `--p_probe_id`; 0 while the probe has no id
    pub probe_id: ProbeId,
    pub msm_id: MeasurementId,
}

#[async_trait]
impl Measurement for Dns {
    fn measurement_type(&self) -> starla_common::MeasurementType {
        starla_common::MeasurementType::Dns
    }

    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        // Read at every run, as tdig_start() does. With -I, the interface's
        // resolv.conf.<ifname> when it exists, else /etc/resolv.conf.
        let resolvers = self.config.resolv.then(|| {
            resolver::read_resolv_conf(&resolver::resolv_conf_for(self.config.interface.as_deref()))
        });
        // get_probe_id() is -1 until the probe knows its id
        let probe_id = match self.probe_id.0 {
            0 => -1,
            id => i64::from(id),
        };
        let entries = run(&self.config, probe_id, resolvers).await;
        Ok(self.result(&entries, resolver::unix_now()))
    }
}

impl Dns {
    fn result(&self, entries: &[Entry], end: i64) -> MeasurementResult {
        let line = format_line(self.msm_id.0, &self.config, entries, end);
        let first = entries.iter().find_map(|e| match &e.attempt.dst {
            Dst::Udp { addr, .. } | Dst::Tcp { addr, .. } => Some(*addr),
            _ => None,
        });
        MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Dns,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: Timestamp(end),
            af: match first {
                Some(a) if a.is_ipv6() => 6,
                Some(_) => 4,
                None if self.config.af == 6 => 6,
                None => 4,
            },
            dst_addr: first.map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |a| a.ip()),
            dst_name: None,
            src_addr: entries.iter().find_map(|e| e.attempt.src.map(|a| a.ip())),
            proto: Some(proto(&self.config).to_string()),
            ttl: None,
            size: None,
            data: MeasurementData::FullLine(line),
        }
    }
}

fn proto(cfg: &DnsConfig) -> &'static str {
    if cfg.tcp {
        "TCP"
    } else {
        "UDP"
    }
}

/// One printReply(): a query as it ended.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub attempt: Attempt,
    /// `subid`, `submax` with `--resolv`
    pub sub: Option<(usize, usize)>,
    /// `--retry` repeats it took
    pub retry: u32,
    /// When it was reported (its lts is the lts then)
    pub printed: i64,
}

impl Entry {
    /// A report with nothing sent
    fn error(cfg: &DnsConfig, dst: Dst, sub: Option<(usize, usize)>, text: &str) -> Self {
        let now = resolver::unix_now();
        let mut attempt = Attempt {
            time: now,
            dst,
            src: None,
            ttl: None,
            qbuf: None,
            outcome: Outcome::Error {
                text: text.to_string(),
                retry: false,
            },
        };
        cfg.carry.apply(&mut attempt, cfg.tcp);
        Self {
            attempt,
            sub,
            retry: 0,
            printed: now,
        }
    }
}

/// One run: tdig_start() and what follows from it.
pub(crate) async fn run(
    cfg: &DnsConfig,
    probe_id: i64,
    resolvers: Option<Vec<String>>,
) -> Vec<Entry> {
    // The macros are expanded before anything else (evtdig.c:3068)
    let Some(name) = resolver::expand_name_macros(&cfg.query, probe_id) else {
        let (dst, sub) = match cfg.resolv {
            true => (Dst::None, Some((1, 0))),
            false => (Dst::Name(cfg.server.clone()), None),
        };
        return vec![Entry::error(
            cfg,
            dst,
            sub,
            "\"err\": \"macro expansion failed\"",
        )];
    };
    let Some(list) = resolvers else {
        return vec![query(cfg, &cfg.server, Some(&name), None, probe_id).await];
    };
    if list.is_empty() {
        let text = "\"nameserver\": \"no local resolvers found\"";
        return vec![Entry::error(cfg, Dst::None, Some((1, 0)), text)];
    }
    let mut entries = Vec::with_capacity(list.len());
    for (i, ns) in list.iter().enumerate() {
        if i > 0 {
            // free_qry_inst() starts the next resolver a second later
            // (evtdig.c:3444)
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        entries.push(query(cfg, ns, None, Some((i + 1, list.len())), probe_id).await);
    }
    entries
}

/// One server's query, with its `--retry` repeats: a timeout (and a TCP
/// error tcp_reporterr() reports) is asked again a second later
/// (noreply_callback(), evtdig.c:1522), until the retries run out.
async fn query(
    cfg: &DnsConfig,
    server: &str,
    name: Option<&str>,
    sub: Option<(usize, usize)>,
    probe_id: i64,
) -> Entry {
    let mut retry = 0;
    loop {
        // With --resolv each query expands the name afresh, so each resolver
        // (and each retry) gets its own $r (evtdig.c:1379)
        let fresh;
        let name = match name {
            Some(n) => n,
            None => {
                fresh = resolver::expand_name_macros(&cfg.query, probe_id)
                    .unwrap_or_else(|| cfg.query.clone());
                &fresh
            }
        };
        let mut attempt = if cfg.tcp {
            resolver::tcp_attempt(cfg, server, name, probe_id).await
        } else {
            resolver::udp_attempt(cfg, server, name, probe_id).await
        };
        // A retried query leaves its addresses too
        cfg.carry.apply(&mut attempt, cfg.tcp);
        let again = match &attempt.outcome {
            Outcome::Timeout => true,
            Outcome::Error { retry, .. } => *retry,
            Outcome::Reply { .. } => false,
        };
        if again && retry < cfg.retry {
            retry += 1;
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        return Entry {
            attempt,
            sub,
            retry,
            printed: resolver::unix_now(),
        };
    }
}

/// The RESULT line's body (between `RESULT { ` and ` }`) as printReply()
/// writes it (evtdig.c:3707), once per entry. `end` is when the run ended,
/// which the [`LTS_MARK`]s count back from.
pub(crate) fn format_line(msm_id: u64, cfg: &DnsConfig, entries: &[Entry], end: i64) -> String {
    let mut s = String::with_capacity(1024);
    let rset = cfg.resolv;
    for (i, e) in entries.iter().enumerate() {
        let a = &e.attempt;
        let lts = format!(
            "\"lts\" : {LTS_MARK}{}{LTS_MARK} , ",
            (end - e.printed).max(0)
        );
        if i == 0 {
            let _ = write!(s, "\"id\" : \"{msm_id}\" , ");
            if let Some(bundle) = cfg.bundle {
                let _ = write!(s, "\"bundle\" : {bundle} , ");
            }
            let _ = write!(
                s,
                "\"fw\":{}, \"mver\": \"2.6.4\", ",
                starla_common::FIRMWARE_VERSION
            );
            if rset {
                let _ = write!(s, "\"time\" : {} , {lts}\"resultset\" : [ {{", a.time);
            }
        } else if rset {
            s.push_str(",{");
        }
        let _ = write!(s, "\"time\" : {} , {lts}", a.time);
        if let Some((subid, submax)) = e.sub {
            let _ = write!(s, "\"subid\" : {subid} , \"submax\" : {submax} , ");
        }
        // UDP names the server "name" when it isn't the address, which is
        // inet_ntop()'s (no zone); TCP "dst_name", and its address is
        // getnameinfo()'s (with one)
        let sent = match &a.dst {
            Dst::Udp { server, addr } => Some(("name", server, addr.ip().to_string(), addr)),
            Dst::Tcp { server, addr } => {
                Some(("dst_name", server, resolver::numeric_host(addr), addr))
            }
            Dst::Name(server) => {
                let _ = write!(s, "\"dst_name\" : \"{server}\" , ");
                None
            }
            Dst::None => None,
        };
        if let Some((key, server, ip, addr)) = sent {
            if ip != *server {
                let _ = write!(s, "\"{key}\" : \"{server}\" , ");
            }
            let _ = write!(
                s,
                "\"dst_addr\" : \"{ip}\" , \"dst_port\" : \"{}\" , \"af\" : {} , ",
                cfg.port,
                if addr.is_ipv6() { 6 } else { 4 }
            );
        }
        if let Some(src) = &a.src {
            let src = resolver::numeric_host(src);
            let _ = write!(s, "\"src_addr\" : \"{src}\" , ");
        }
        if e.retry > 0 {
            let _ = write!(s, "\"retry\" : {} , ", e.retry);
        }
        // `--ttl`: here, beside the addresses, and not in "result" where
        // the documentation (version 5000, "DNS Lookup", from 5010) puts it
        if let Some(ttl) = a.ttl {
            let _ = write!(s, "\"ttl\" : {ttl} , ");
        }
        let _ = write!(s, "\"proto\" : \"{}\" ", proto(cfg));
        if let Some(q) = &a.qbuf {
            let _ = write!(s, ",\"qbuf\" : \"{}\" ", BASE64.encode(q));
        }
        let err = match &a.outcome {
            Outcome::Reply { wire, rt, qt, .. } => {
                resolver::write_result(&mut s, wire, *rt, *qt, cfg.abuf);
                None
            }
            Outcome::Timeout => Some(format!(" \"timeout\" : {}", cfg.timeout_ms)),
            Outcome::Error { text, .. } => Some(text.clone()),
        };
        if let Some(err) = err {
            let _ = write!(s, ", \"error\" : {{{err}}}");
        }
        if rset {
            s.push('}');
        }
    }
    if rset {
        s.push(']');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::net::{TcpListener, UdpSocket};

    fn b64(s: &str) -> Vec<u8> {
        BASE64.decode(s).unwrap()
    }

    /// The line as uploaded: what starla-results' format.rs does to the
    /// marks, given the lts the result was queued with.
    fn uploaded(body: &str, lts: i64) -> String {
        let mut out = String::new();
        for (i, part) in body.split(LTS_MARK).enumerate() {
            if i % 2 == 0 {
                out.push_str(part);
            } else {
                let back: i64 = part.parse().unwrap();
                let v = if lts < 0 { lts } else { (lts - back).max(0) };
                out.push_str(&v.to_string());
            }
        }
        format!("RESULT {{ {out} }}")
    }

    fn entry(
        time: i64,
        dst: Dst,
        src: &str,
        qbuf: Option<&str>,
        outcome: Outcome,
        sub: Option<(usize, usize)>,
    ) -> Entry {
        Entry {
            attempt: Attempt {
                time,
                dst,
                src: Some(local(src)),
                ttl: None,
                qbuf: qbuf.map(b64),
                outcome,
            },
            sub,
            retry: 0,
            printed: time,
        }
    }

    /// A source address as getsockname() gives it: an ephemeral port, and
    /// no scope
    fn local(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 49152)
    }

    fn udp(addr: &str) -> Dst {
        let addr: SocketAddr = addr.parse().unwrap();
        Dst::Udp {
            server: addr.ip().to_string(),
            addr,
        }
    }

    fn reply(abuf: &str, rt: f64, qt: Option<f64>) -> Outcome {
        Outcome::Reply {
            wire: b64(abuf),
            rt,
            qt,
        }
    }

    /// msm 11001 (`evtdig -6 --soa .`) as the reference reported it in the
    /// twin run of 2026-10-13, byte for byte. Starla wrote
    /// `"answers":[ {"TYPE":"SOA", "NAME":".", "RDATA":[ "a.root-servers.net.
    /// …" ]} ]`.
    #[test]
    fn a_soa_result_is_the_references_line() {
        let cfg = DnsConfig {
            af: 6,
            qtype: 6,
            qclass: 1,
            query: ".".into(),
            ..Default::default()
        };
        let abuf = "Xo2EAAABAAEAAAAAAAAGAAEAAAYAAQAAASwAQAFhDHJvb3Qtc2VydmVycwNuZXQABW5zdGxkDHZlcmlzaWduLWdycwNjb20AeMPaNAAABwgAAAOEAAk6gAABUYA=";
        let e = entry(
            1791871296,
            udp("[2001:db8:7769:6e00::20]:53"),
            "2001:db8:7769:6e00::31",
            None,
            reply(abuf, 0.765, None),
            None,
        );
        let line = format_line(11001, &cfg, &[e], 1791871296);
        assert_eq!(
            uploaded(&line, 13),
            r#"RESULT { "id" : "11001" , "fw":5130, "mver": "2.6.4", "time" : 1791871296 , "lts" : 13 , "dst_addr" : "2001:db8:7769:6e00::20" , "dst_port" : "53" , "af" : 6 , "src_addr" : "2001:db8:7769:6e00::31" , "proto" : "UDP" ,"result" : { "rt" : 0.765,"size" : 92 , "abuf" : "Xo2EAAABAAEAAAAAAAAGAAEAAAYAAQAAASwAQAFhDHJvb3Qtc2VydmVycwNuZXQABW5zdGxkDHZlcmlzaWduLWdycwNjb20AeMPaNAAABwgAAAOEAAk6gAABUYA=","ID" : 24205 , "ANCOUNT" : 1 , "QDCOUNT" : 1 , "NSCOUNT" : 0 , "ARCOUNT" : 0,"answers" : [ { "TYPE" : "SOA" , "NAME" : "." , "TTL" : 300 , "MNAME" : "a.root-servers.net." , "RNAME" : "nstld.verisign-grs.com." , "SERIAL" : 2026101300 }  ] } }"#
        );
    }

    /// msm 11101 (`-6 -t --soa .`): TCP adds `qt`, the time from the query
    /// being written (Starla had none).
    #[test]
    fn a_tcp_result_has_qt() {
        let cfg = DnsConfig {
            af: 6,
            tcp: true,
            qtype: 6,
            qclass: 1,
            query: ".".into(),
            ..Default::default()
        };
        let abuf = "e4GEAAABAAEAAAAAAAAGAAEAAAYAAQAAASwAQAFhDHJvb3Qtc2VydmVycwNuZXQABW5zdGxkDHZlcmlzaWduLWdycwNjb20AeMPaNAAABwgAAAOEAAk6gAABUYA=";
        let addr: SocketAddr = "[2001:db8:7769:6e00::20]:53".parse().unwrap();
        let dst = Dst::Tcp {
            server: "2001:db8:7769:6e00::20".into(),
            addr,
        };
        let e = entry(
            1791871327,
            dst,
            "2001:db8:7769:6e00::31",
            None,
            reply(abuf, 1.213, Some(0.821)),
            None,
        );
        let line = format_line(11101, &cfg, &[e], 1791871327);
        assert_eq!(
            uploaded(&line, 44),
            r#"RESULT { "id" : "11101" , "fw":5130, "mver": "2.6.4", "time" : 1791871327 , "lts" : 44 , "dst_addr" : "2001:db8:7769:6e00::20" , "dst_port" : "53" , "af" : 6 , "src_addr" : "2001:db8:7769:6e00::31" , "proto" : "TCP" ,"result" : { "rt" : 1.213,"qt" : 0.821,"size" : 92 , "abuf" : "e4GEAAABAAEAAAAAAAAGAAEAAAYAAQAAASwAQAFhDHJvb3Qtc2VydmVycwNuZXQABW5zdGxkDHZlcmlzaWduLWdycwNjb20AeMPaNAAABwgAAAOEAAk6gAABUYA=","ID" : 31617 , "ANCOUNT" : 1 , "QDCOUNT" : 1 , "NSCOUNT" : 0 , "ARCOUNT" : 0,"answers" : [ { "TYPE" : "SOA" , "NAME" : "." , "TTL" : 300 , "MNAME" : "a.root-servers.net." , "RNAME" : "nstld.verisign-grs.com." , "SERIAL" : 2026101300 }  ] } }"#
        );
    }

    /// msm 11501 (`-6 -r`): version.server, its name without the final dot.
    #[test]
    fn a_txt_result_is_the_references_line() {
        let cfg = DnsConfig {
            af: 6,
            query: "version.server.".into(),
            ..Default::default()
        };
        let abuf = "wHaEAAABAAEAAAAAB3ZlcnNpb24Gc2VydmVyAAAQAAPADAAQAAMAAAAAAAsKYXRsYXMtdHdpbg==";
        let e = entry(
            1791871259,
            udp("[2001:db8:7769:6e00::20]:53"),
            "2001:db8:7769:6e00::31",
            None,
            reply(abuf, 0.563, None),
            None,
        );
        let line = format_line(11501, &cfg, &[e], 1791871259);
        assert_eq!(
            uploaded(&line, 48),
            r#"RESULT { "id" : "11501" , "fw":5130, "mver": "2.6.4", "time" : 1791871259 , "lts" : 48 , "dst_addr" : "2001:db8:7769:6e00::20" , "dst_port" : "53" , "af" : 6 , "src_addr" : "2001:db8:7769:6e00::31" , "proto" : "UDP" ,"result" : { "rt" : 0.563,"size" : 55 , "abuf" : "wHaEAAABAAEAAAAAB3ZlcnNpb24Gc2VydmVyAAAQAAPADAAQAAMAAAAAAAsKYXRsYXMtdHdpbg==","ID" : 49270 , "ANCOUNT" : 1 , "QDCOUNT" : 1 , "NSCOUNT" : 0 , "ARCOUNT" : 0,"answers" : [ {"TYPE" : "TXT" , "NAME" : "version.server" , "RDATA" : [ "atlas-twin" ] } ] } }"#
        );
    }

    /// msm 51457586 (`-6 -t -e 512 -n --qbuf --a paypal.com.`): the query in
    /// `qbuf`, and no `answers` for an A record.
    #[test]
    fn a_qbuf_result_with_an_a_answer_is_the_references_line() {
        let cfg = DnsConfig {
            af: 6,
            tcp: true,
            qtype: 1,
            qclass: 1,
            nsid: true,
            qbuf: true,
            query: "paypal.com.".into(),
            ..Default::default()
        };
        let addr: SocketAddr = "[2001:db8:7769:6e00::20]:53".parse().unwrap();
        let dst = Dst::Tcp {
            server: "2001:db8:7769:6e00::20".into(),
            addr,
        };
        let e = entry(
            1791871350,
            dst,
            "2001:db8:7769:6e00::31",
            Some("F7kAAAABAAAAAAABBnBheXBhbANjb20AAAEAAQAAKQIAAAAAAAAEAAMAAA=="),
            reply("F7mEAAABAAEAAAABBnBheXBhbANjb20AAAEAAcAMAAEAAQAAASwABKweShQAACkE0AAAAAAADgADAAphdGxhcy10d2lu", 1.014, Some(0.604)),
            None,
        );
        let line = format_line(51457586, &cfg, &[e], 1791871350);
        assert_eq!(
            uploaded(&line, 10),
            r#"RESULT { "id" : "51457586" , "fw":5130, "mver": "2.6.4", "time" : 1791871350 , "lts" : 10 , "dst_addr" : "2001:db8:7769:6e00::20" , "dst_port" : "53" , "af" : 6 , "src_addr" : "2001:db8:7769:6e00::31" , "proto" : "TCP" ,"qbuf" : "F7kAAAABAAAAAAABBnBheXBhbANjb20AAAEAAQAAKQIAAAAAAAAEAAMAAA==" ,"result" : { "rt" : 1.014,"qt" : 0.604,"size" : 69 , "abuf" : "F7mEAAABAAEAAAABBnBheXBhbANjb20AAAEAAcAMAAEAAQAAASwABKweShQAACkE0AAAAAAADgADAAphdGxhcy10d2lu","ID" : 6073 , "ANCOUNT" : 1 , "QDCOUNT" : 1 , "NSCOUNT" : 0 , "ARCOUNT" : 1 } }"#
        );
    }

    /// msm 30001 (`-4 --resolv --qbuf --type 1 --query $p.atlas.ripe.net.$r.
    /// --class 1`): one entry per resolver in `resultset`, a second apart,
    /// each with its own lts. Starla asked 8.8.8.8 instead.
    #[test]
    fn a_resolv_result_is_the_references_resultset() {
        let cfg = DnsConfig {
            resolv: true,
            af: 0,
            qtype: 1,
            qclass: 1,
            qbuf: true,
            query: "$p.atlas.ripe.net.$r.".into(),
            ..Default::default()
        };
        let first = entry(
            1791871302,
            udp("172.30.74.20:53"),
            "172.30.74.31",
            Some("i0kBAAABAAAAAAAABzEwMTgyMTIFYXRsYXMEcmlwZQNuZXQQYTdhMjM3OWViZTM2MmFiMwAAAQAB"),
            reply("i0mFAAABAAEAAAAABzEwMTgyMTIFYXRsYXMEcmlwZQNuZXQQYTdhMjM3OWViZTM2MmFiMwAAAQABwAwAAQABAAABLAAErB5KFA==", 0.498, None),
            Some((1, 2)),
        );
        let second = entry(
            1791871303,
            udp("[2001:db8:7769:6e00::20]:53"),
            "2001:db8:7769:6e00::31",
            Some("MLEBAAABAAAAAAAABzEwMTgyMTIFYXRsYXMEcmlwZQNuZXQQZjQxY2Q5YzRlODlhZDVjMgAAAQAB"),
            reply("MLGFAAABAAEAAAAABzEwMTgyMTIFYXRsYXMEcmlwZQNuZXQQZjQxY2Q5YzRlODlhZDVjMgAAAQABwAwAAQABAAABLAAErB5KFA==", 0.540, None),
            Some((2, 2)),
        );
        let line = format_line(30001, &cfg, &[first, second], 1791871303);
        assert_eq!(
            uploaded(&line, 20),
            r#"RESULT { "id" : "30001" , "fw":5130, "mver": "2.6.4", "time" : 1791871302 , "lts" : 19 , "resultset" : [ {"time" : 1791871302 , "lts" : 19 , "subid" : 1 , "submax" : 2 , "dst_addr" : "172.30.74.20" , "dst_port" : "53" , "af" : 4 , "src_addr" : "172.30.74.31" , "proto" : "UDP" ,"qbuf" : "i0kBAAABAAAAAAAABzEwMTgyMTIFYXRsYXMEcmlwZQNuZXQQYTdhMjM3OWViZTM2MmFiMwAAAQAB" ,"result" : { "rt" : 0.498,"size" : 73 , "abuf" : "i0mFAAABAAEAAAAABzEwMTgyMTIFYXRsYXMEcmlwZQNuZXQQYTdhMjM3OWViZTM2MmFiMwAAAQABwAwAAQABAAABLAAErB5KFA==","ID" : 35657 , "ANCOUNT" : 1 , "QDCOUNT" : 1 , "NSCOUNT" : 0 , "ARCOUNT" : 0 }},{"time" : 1791871303 , "lts" : 20 , "subid" : 2 , "submax" : 2 , "dst_addr" : "2001:db8:7769:6e00::20" , "dst_port" : "53" , "af" : 6 , "src_addr" : "2001:db8:7769:6e00::31" , "proto" : "UDP" ,"qbuf" : "MLEBAAABAAAAAAAABzEwMTgyMTIFYXRsYXMEcmlwZQNuZXQQZjQxY2Q5YzRlODlhZDVjMgAAAQAB" ,"result" : { "rt" : 0.540,"size" : 73 , "abuf" : "MLGFAAABAAEAAAAABzEwMTgyMTIFYXRsYXMEcmlwZQNuZXQQZjQxY2Q5YzRlODlhZDVjMgAAAQABwAwAAQABAAABLAAErB5KFA==","ID" : 12465 , "ANCOUNT" : 1 , "QDCOUNT" : 1 , "NSCOUNT" : 0 , "ARCOUNT" : 0 }}] }"#
        );
        // Before the first clock check every lts is -1
        assert!(!uploaded(&line, -1).contains("\"lts\" : 1"));
        assert_eq!(uploaded(&line, -1).matches("\"lts\" : -1 ,").count(), 3);
    }

    /// No resolvers at all: tdig_start()'s error, in a one-entry resultset.
    #[tokio::test]
    async fn no_resolvers_is_reported_not_sent_elsewhere() {
        let cfg = DnsConfig {
            resolv: true,
            af: 0,
            qtype: 1,
            qclass: 1,
            query: "example.".into(),
            ..Default::default()
        };
        let entries = run(&cfg, 1018212, Some(vec![])).await;
        let line = uploaded(&format_line(30001, &cfg, &entries, entries[0].printed), 5);
        let t = entries[0].attempt.time;
        assert_eq!(
            line,
            format!(
                r#"RESULT {{ "id" : "30001" , "fw":5130, "mver": "2.6.4", "time" : {t} , "lts" : 5 , "resultset" : [ {{"time" : {t} , "lts" : 5 , "subid" : 1 , "submax" : 0 , "proto" : "UDP" , "error" : {{"nameserver": "no local resolvers found"}}}}] }}"#
            )
        );
    }

    /// A DNS server on a local socket: it answers each query with its own
    /// question (QR set), and hands back what it was asked.
    async fn udp_server(
        addr: impl tokio::net::ToSocketAddrs,
    ) -> (SocketAddr, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
        let sock = UdpSocket::bind(addr).await.unwrap();
        let local = sock.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
                let q = buf[..n].to_vec();
                let mut r = q.clone();
                r[2] |= 0x80;
                let _ = sock.send_to(&r, peer).await;
                let _ = tx.send(q);
            }
        });
        (local, rx)
    }

    fn name_of(query: &[u8]) -> String {
        resolver::read_name(query, 12).unwrap().0
    }

    /// `--resolv` asks each resolver in turn, never anyone else: two local
    /// resolvers, two entries, each name with its own `$r`, RD set.
    #[tokio::test]
    async fn resolv_asks_each_resolver_and_reports_a_resultset() {
        // One port for both, as /etc/resolv.conf gives no ports
        let (v4, mut rx4, v6, mut rx6) = loop {
            let (v4, rx4) = udp_server("127.0.0.1:0").await;
            if let Ok(s) = UdpSocket::bind(("::1", v4.port())).await {
                drop(s);
                let (v6, rx6) = udp_server(&format!("[::1]:{}", v4.port())).await;
                break (v4, rx4, v6, rx6);
            }
        };
        let cfg = DnsConfig {
            resolv: true,
            af: 0,
            port: v4.port(),
            qtype: 1,
            qclass: 1,
            qbuf: true,
            query: "$p.atlas.ripe.net.$r.".into(),
            timeout_ms: 2000,
            ..Default::default()
        };
        let list = vec!["127.0.0.1".to_string(), "::1".to_string()];
        let entries = run(&cfg, 1018212, Some(list)).await;
        assert_eq!(entries.len(), 2);
        let (q4, q6) = (rx4.recv().await.unwrap(), rx6.recv().await.unwrap());
        for q in [&q4, &q6] {
            assert_eq!(q[2] & 0x01, 1, "RD");
            assert_eq!(&q[10..12], &[0, 0], "no OPT");
            let name = name_of(q);
            assert!(name.starts_with("1018212.atlas.ripe.net."), "{name}");
            assert_eq!(name.len(), "1018212.atlas.ripe.net.".len() + 16);
        }
        assert_ne!(name_of(&q4), name_of(&q6), "each resolver its own $r");
        assert_eq!(
            entries[0].attempt.dst,
            Dst::Udp {
                server: "127.0.0.1".into(),
                addr: v4
            }
        );
        assert_eq!(
            entries[1].attempt.dst,
            Dst::Udp {
                server: "::1".into(),
                addr: v6
            }
        );
        assert!(entries[1].attempt.time > entries[0].attempt.time - 1);

        let line = uploaded(&format_line(30001, &cfg, &entries, entries[1].printed), 7);
        let port = v4.port();
        assert!(line.contains(&format!(r#""subid" : 1 , "submax" : 2 , "dst_addr" : "127.0.0.1" , "dst_port" : "{port}" , "af" : 4 , "src_addr" : "127.0.0.1" , "proto" : "UDP" ,"qbuf" : ""#)), "{line}");
        assert!(line.contains(&format!(r#""subid" : 2 , "submax" : 2 , "dst_addr" : "::1" , "dst_port" : "{port}" , "af" : 6 , "src_addr" : "::1" , "proto" : "UDP" ,"qbuf" : ""#)), "{line}");
        assert!(
            line.contains(&format!(
                r#""qbuf" : "{}" ,"result" : {{ "rt" : "#,
                BASE64.encode(&q4)
            )),
            "{line}"
        );
        assert!(line.ends_with(r#""ARCOUNT" : 0 }}] }"#), "{line}");
    }

    /// The query a UDP measurement sends is evtdig's: `-6 -e 512 -n --qbuf
    /// --aaaa $p.ripe-tld-experiment.$r.` (158899651) asks without RD, with
    /// EDNS 512 and NSID, for the probe's own id.
    #[tokio::test]
    async fn a_udp_query_goes_out_as_evtdigs() {
        let (server, mut rx) = udp_server("127.0.0.1:0").await;
        let cfg = DnsConfig {
            server: "127.0.0.1".into(),
            af: 4,
            port: server.port(),
            qtype: 28,
            qclass: 1,
            nsid: true,
            qbuf: true,
            query: "$p.ripe-tld-experiment.$r.".into(),
            disable_check_addr: true,
            ..Default::default()
        };
        let entries = run(&cfg, 1018212, None).await;
        let q = rx.recv().await.unwrap();
        assert_eq!(&q[2..4], &[0, 0], "no RD");
        assert_eq!(
            &q[q.len() - 15..],
            b"\x00\x00\x29\x02\x00\x00\x00\x00\x00\x00\x04\x00\x03\x00\x00"
        );
        assert!(name_of(&q).starts_with("1018212.ripe-tld-experiment."));
        let a = &entries[0].attempt;
        assert_eq!(a.qbuf.as_deref(), Some(&q[..]));
        let Outcome::Reply { wire, qt, .. } = &a.outcome else {
            panic!("{:?}", a.outcome)
        };
        assert_eq!(wire[2] & 0x80, 0x80);
        assert!(qt.is_none());
        let line = format_line(158899651, &cfg, &entries, entries[0].printed);
        // the server is the address: no "name"
        assert!(!line.contains("\"name\""), "{line}");
    }

    /// A timeout is retried `--retry` times, a second apart, and reported
    /// as `"error" : { "timeout" : N}` with the count in `"retry"`.
    #[tokio::test]
    async fn a_timeout_is_retried_and_reported() {
        let mute = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cfg = DnsConfig {
            server: "127.0.0.1".into(),
            port: mute.local_addr().unwrap().port(),
            query: "hostname.bind.".into(),
            timeout_ms: 100,
            retry: 1,
            disable_check_addr: true,
            ..Default::default()
        };
        let entries = run(&cfg, 1, None).await;
        let mut buf = [0u8; 512];
        let (n1, _) = mute.recv_from(&mut buf).await.unwrap();
        let first = buf[..n1].to_vec();
        let (n2, _) = mute.recv_from(&mut buf).await.unwrap();
        assert_ne!(first[..2], buf[..2], "a new id for the retry");
        let line = uploaded(&format_line(11301, &cfg, &entries, entries[0].printed), 3);
        let port = cfg.port;
        assert!(line.ends_with(&format!(r#""dst_addr" : "127.0.0.1" , "dst_port" : "{port}" , "af" : 4 , "src_addr" : "127.0.0.1" , "retry" : 1 , "proto" : "UDP" , "error" : {{ "timeout" : 100}} }}"#)), "{line}");
        assert_eq!(n1, n2);
    }

    /// A DNS server on a TCP listener: it answers one query with its own
    /// question (QR set), hands back what it was asked, and closes.
    fn answer_tcp_once(listener: TcpListener) -> tokio::task::JoinHandle<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut len = [0u8; 2];
            s.read_exact(&mut len).await.unwrap();
            let mut q = vec![0u8; u16::from_be_bytes(len) as usize];
            s.read_exact(&mut q).await.unwrap();
            let mut r = q.clone();
            r[2] |= 0x80;
            s.write_all(&(r.len() as u16).to_be_bytes()).await.unwrap();
            s.write_all(&r).await.unwrap();
            q
        })
    }

    /// TCP: the length-prefixed query, and `rt` and `qt` in the result; no
    /// `"ttl"` even with `--ttl` (only process_reply(), UDP's, reads one).
    #[tokio::test]
    async fn a_tcp_query_reports_rt_and_qt() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = answer_tcp_once(listener);
        let cfg = DnsConfig {
            server: "127.0.0.1".into(),
            tcp: true,
            port,
            qtype: 6,
            qclass: 1,
            query: ".".into(),
            ttl: true,
            disable_check_addr: true,
            ..Default::default()
        };
        let entries = run(&cfg, 1, None).await;
        let q = server.await.unwrap();
        assert_eq!(q.len(), 17, "--soa . with no OPT");
        let line = uploaded(&format_line(11101, &cfg, &entries, entries[0].printed), 3);
        assert!(
            line.contains(r#""proto" : "TCP" ,"result" : { "rt" : "#),
            "{line}"
        );
        assert!(
            line.contains(r#","qt" : "#) && line.contains(r#","size" : 17 , "abuf" : ""#),
            "{line}"
        );
        assert!(!line.contains("\"ttl\""), "{line}");
    }

    /// The result handed to the queue is the full line, with marks for lts.
    #[tokio::test]
    async fn the_measurement_hands_over_a_full_line() {
        let (server, _rx) = udp_server("127.0.0.1:0").await;
        let dns = Dns {
            config: DnsConfig {
                server: "127.0.0.1".into(),
                port: server.port(),
                query: "id.server.".into(),
                disable_check_addr: true,
                ..Default::default()
            },
            probe_id: ProbeId(1018212),
            msm_id: MeasurementId(11401),
        };
        let r = dns.execute().await.unwrap();
        let MeasurementData::FullLine(line) = &r.data else {
            panic!("{:?}", r.data)
        };
        assert!(
            line.starts_with("\"id\" : \"11401\" , \"fw\":5130, \"mver\": \"2.6.4\", \"time\" : ")
        );
        assert!(line.contains(&format!("\"lts\" : {LTS_MARK}0{LTS_MARK} , ")));
        assert_eq!(r.dst_addr, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(r.af, 4);
    }

    /// A name that can't be a query is reported, not sent.
    #[tokio::test]
    async fn bad_macros_and_names_are_reported() {
        let cfg = DnsConfig {
            server: "127.0.0.1".into(),
            query: "$q.example.".into(),
            ..Default::default()
        };
        let entries = run(&cfg, 1, None).await;
        let line = uploaded(&format_line(1, &cfg, &entries, entries[0].printed), 0);
        assert!(line.ends_with(r#""dst_name" : "127.0.0.1" , "proto" : "UDP" , "error" : {"err": "macro expansion failed"} }"#), "{line}");
    }

    /// The object RIPE stores for a RESULT line: "id" becomes "msm_id", a
    /// number, and "time" "timestamp". The backend adds prb_id, from,
    /// msm_name, type, group_id, step and stored_timestamp, which the probe
    /// must not send.
    fn stored(line: &str) -> serde_json::Value {
        let body = line.strip_prefix("RESULT ").unwrap();
        let mut v: serde_json::Value = serde_json::from_str(body).unwrap();
        let o = v.as_object_mut().unwrap();
        let id = o.remove("id").unwrap();
        let id: u64 = id.as_str().unwrap().parse().unwrap();
        o.insert("msm_id".into(), id.into());
        let time = o.remove("time").unwrap();
        o.insert("timestamp".into(), time);
        v
    }

    /// A result as RIPE's archive has it, less what its backend adds.
    fn archived(json: &str) -> serde_json::Value {
        let mut v: serde_json::Value = serde_json::from_str(json).unwrap();
        let o = v.as_object_mut().unwrap();
        for backend in [
            "prb_id",
            "from",
            "msm_name",
            "type",
            "group_id",
            "step",
            "stored_timestamp",
        ] {
            o.remove(backend);
        }
        v
    }

    /// One query as it ended, `printed` seconds after it was sent.
    fn ended(
        time: i64,
        printed: i64,
        dst: Dst,
        src: Option<&str>,
        qbuf: Option<&str>,
        outcome: Outcome,
        sub: Option<(usize, usize)>,
    ) -> Entry {
        Entry {
            attempt: Attempt {
                time,
                dst,
                src: src.map(local),
                ttl: None,
                qbuf: qbuf.map(b64),
                outcome,
            },
            sub,
            retry: 0,
            printed: time + printed,
        }
    }

    fn tcp(addr: &str) -> Dst {
        let addr: SocketAddr = addr.parse().unwrap();
        Dst::Tcp {
            server: addr.ip().to_string(),
            addr,
        }
    }

    fn error(text: &str) -> Outcome {
        Outcome::Error {
            text: text.into(),
            retry: false,
        }
    }

    /// The reference probe's (1000004, firmware 5130) latest results as
    /// RIPE stored them on 2026-10-13 (archive ref-<msm>.json), rebuilt from
    /// their own abuf, rt, addresses and times: one of each shape the
    /// scenario's single queries take. SOA (11001) and TXT answers (11201
    /// by -b, 156155996 by `-e 512 -i`), a REFUSED reply without answers
    /// (11213), and TCP with --qbuf, qt and an A question whose answer is
    /// only in abuf (51457586). Starla 5120 stored SOA as one RDATA string
    /// and TXT names with their final dot (starla-11001, starla-11213).
    #[test]
    fn single_results_are_what_ripe_stored_from_the_reference() {
        let soa = DnsConfig {
            af: 6,
            qtype: 6,
            qclass: 1,
            query: ".".into(),
            ..Default::default()
        };
        let chaos = DnsConfig {
            af: 6,
            ..Default::default()
        };
        let tcp_a = DnsConfig {
            af: 6,
            tcp: true,
            qtype: 1,
            qclass: 1,
            nsid: true,
            qbuf: true,
            query: "paypal.com.".into(),
            ..Default::default()
        };
        let src = Some("2001:418:1::33");
        let cases = [
            (
                11001,
                &soa,
                ended(1791885024, 0, udp("[2001:7fd::1]:53"), src, None, reply("ZUqEAAABAAEAAAAAAAAGAAEAAAYAAQABUYAAQAFhDHJvb3Qtc2VydmVycwNuZXQABW5zdGxkDHZlcmlzaWduLWdycwNjb20AeMPaNAAABwgAAAOEAAk6gAABUYA=", 3.698, None), None),
                36,
                r#"{"fw":5130,"mver":"2.6.4","lts":36,"dst_addr":"2001:7fd::1","dst_port":"53","af":6,"src_addr":"2001:418:1::33","proto":"UDP","result":{"rt":3.698,"size":92,"abuf":"ZUqEAAABAAEAAAAAAAAGAAEAAAYAAQABUYAAQAFhDHJvb3Qtc2VydmVycwNuZXQABW5zdGxkDHZlcmlzaWduLWdycwNjb20AeMPaNAAABwgAAAOEAAk6gAABUYA=","ID":25930,"ANCOUNT":1,"QDCOUNT":1,"NSCOUNT":0,"ARCOUNT":0,"answers":[{"TYPE":"SOA","NAME":".","TTL":86400,"MNAME":"a.root-servers.net.","RNAME":"nstld.verisign-grs.com.","SERIAL":2026101300}]},"msm_id":11001,"prb_id":1000004,"timestamp":1791885024,"msm_name":"Tdig","from":"2001:418:1::33","type":"dns","stored_timestamp":1791885104}"#,
            ),
            (
                11201,
                &chaos,
                ended(1791849459, 0, udp("[2001:7fd::1]:53"), src, None, reply("CHiEAAABAAEAAAAAB3ZlcnNpb24EYmluZAAAEAADwAwAEAADAAAAAAAFBEJJTkQ=", 3.822, None), None),
                4,
                r#"{"fw":5130,"mver":"2.6.4","lts":4,"dst_addr":"2001:7fd::1","dst_port":"53","af":6,"src_addr":"2001:418:1::33","proto":"UDP","result":{"rt":3.822,"size":47,"abuf":"CHiEAAABAAEAAAAAB3ZlcnNpb24EYmluZAAAEAADwAwAEAADAAAAAAAFBEJJTkQ=","ID":2168,"ANCOUNT":1,"QDCOUNT":1,"NSCOUNT":0,"ARCOUNT":0,"answers":[{"TYPE":"TXT","NAME":"version.bind","RDATA":["BIND"]}]},"msm_id":11201,"prb_id":1000004,"timestamp":1791849459,"msm_name":"Tdig","from":"2001:418:1::33","type":"dns","stored_timestamp":1791849528}"#,
            ),
            (
                11213,
                &chaos,
                ended(1791849586, 0, udp("[2001:500:a8::e]:53"), src, None, reply("MruABQABAAAAAAAAB3ZlcnNpb24EYmluZAAAEAAD", 0.371, None), None),
                1,
                r#"{"fw":5130,"mver":"2.6.4","lts":1,"dst_addr":"2001:500:a8::e","dst_port":"53","af":6,"src_addr":"2001:418:1::33","proto":"UDP","result":{"rt":0.371,"size":30,"abuf":"MruABQABAAAAAAAAB3ZlcnNpb24EYmluZAAAEAAD","ID":12987,"ANCOUNT":0,"QDCOUNT":1,"NSCOUNT":0,"ARCOUNT":0},"msm_id":11213,"prb_id":1000004,"timestamp":1791849586,"msm_name":"Tdig","from":"2001:418:1::33","type":"dns","stored_timestamp":1791849653}"#,
            ),
            (
                156155996,
                &DnsConfig { edns0: 512, ..chaos.clone() },
                ended(1791830638, 0, udp("[2606:4700:4700::1111]:53"), src, None, reply("IROAgAABAAEAAAAAAmlkBnNlcnZlcgAAEAADwAwAEAADAAAAAAAGBXNlYTA3", 3.067, None), None),
                5,
                r#"{"fw":5130,"mver":"2.6.4","lts":5,"dst_addr":"2606:4700:4700::1111","dst_port":"53","af":6,"src_addr":"2001:418:1::33","proto":"UDP","result":{"rt":3.067,"size":45,"abuf":"IROAgAABAAEAAAAAAmlkBnNlcnZlcgAAEAADwAwAEAADAAAAAAAGBXNlYTA3","ID":8467,"ANCOUNT":1,"QDCOUNT":1,"NSCOUNT":0,"ARCOUNT":0,"answers":[{"TYPE":"TXT","NAME":"id.server","RDATA":["sea07"]}]},"msm_id":156155996,"prb_id":1000004,"timestamp":1791830638,"msm_name":"Tdig","from":"2001:418:1::33","type":"dns","group_id":156155996,"stored_timestamp":1791830680}"#,
            ),
            (
                51457586,
                &tcp_a,
                ended(1791866657, 0, tcp("[2001:7fd::1]:53"), src, Some("w5YAAAABAAAAAAABBnBheXBhbANjb20AAAEAAQAAKQIAAAAAAAAEAAMAAA=="), reply(PAYPAL_860, 7.501, Some(3.801)), None),
                39,
                &format!(r#"{{"fw":5130,"mver":"2.6.4","lts":39,"dst_addr":"2001:7fd::1","dst_port":"53","af":6,"src_addr":"2001:418:1::33","proto":"TCP","qbuf":"w5YAAAABAAAAAAABBnBheXBhbANjb20AAAEAAQAAKQIAAAAAAAAEAAMAAA==","result":{{"rt":7.501,"qt":3.801,"size":860,"abuf":"{PAYPAL_860}","ID":50070,"ANCOUNT":0,"QDCOUNT":1,"NSCOUNT":13,"ARCOUNT":27}},"msm_id":51457586,"prb_id":1000004,"timestamp":1791866657,"msm_name":"Tdig","from":"2001:418:1::33","type":"dns","group_id":51457586,"stored_timestamp":1791866738}}"#),
            ),
        ];
        for (msm, cfg, e, lts, want) in cases {
            let line = uploaded(&format_line(msm, cfg, &[e], 0), lts);
            assert_eq!(stored(&line), archived(want), "{msm}: {line}");
        }
    }

    /// 51457586's reply (paypal.com. A over TCP from k-root, 13 NS and 27
    /// additional records), as RIPE stored the reference's on 2026-10-13.
    const PAYPAL_860: &str = "w5aAAAABAAAADQAbBnBheXBhbANjb20AAAEAAcATAAIAAQACowAAFAFiDGd0bGQtc2VydmVycwNuZXQAwBMAAgABAAKjAAAEAWbAKsATAAIAAQACowAABAFrwCrAEwACAAEAAqMAAAQBZMAqwBMAAgABAAKjAAAEAWzAKsATAAIAAQACowAABAFhwCrAEwACAAEAAqMAAAQBZ8AqwBMAAgABAAKjAAAEAWXAKsATAAIAAQACowAABAFpwCrAEwACAAEAAqMAAAQBasAqwBMAAgABAAKjAAAEAWjAKsATAAIAAQACowAABAFjwCrAEwACAAEAAqMAAAQBbcAqwPgAHAABAAKjAAAQIAEFAbH5AAAAAAAAAAAAMMB4ABwAAQACowAAECABBQDZNwAAAAAAAAAAADDAWAAcAAEAAqMAABAgAQUDDS0AAAAAAAAAAAAwwMgAHAABAAKjAAAQIAEFAnCUAAAAAAAAAAAAMMC4ABwAAQACowAAECABBQM5wQAAAAAAAAAAADDA2AAcAAEAAqMAABAgAQUCCMwAAAAAAAAAAAAwwJgAHAABAAKjAAAQIAEFA+6jAAAAAAAAAAAAMMBIABwAAQACowAAECABBQPUFAAAAAAAAAAAADDAqAAcAAEAAqMAABAgAQUCHKEAAAAAAAAAAAAwwGgAHAABAAKjAAAQIAEFAIVuAAAAAAAAAAAAMMDoABwAAQACowAAECABBQOD6wAAAAAAAAAAADDAKAAcAAEAAqMAABAgAQUDIx0AAAAAAAAAAgAwwIgAHAABAAKjAAAQIAEFA6g+AAAAAAAAAAIAMMD4AAEAAQACowAABMA3Ux7AeAABAAEAAqMAAATAKaIewFgAAQABAAKjAAAEwDSyHsDIAAEAAQACowAABMAwTx7AuAABAAEAAqMAAATAK6wewNgAAQABAAKjAAAEwDZwHsCYAAEAAQACowAABMAqXR7ASAABAAEAAqMAAATAIzMewKgAAQABAAKjAAAEwAxeHsBoAAEAAQACowAABMAfUB7A6AABAAEAAqMAAATAGlwewCgAAQABAAKjAAAEwCEOHsCIAAEAAQACowAABMAFBh4AACkE0AAAAAAAGQADABVuczEuY2EtdmFuLmsucmlwZS5uZXQ=";

    /// N5: a resolver that does not answer is a resultset entry with
    /// `"error" : { "timeout" : 5000}` beside its addresses and qbuf, not
    /// a "result" holding an error. RIPE stored the reference's 30001
    /// (`-4 --resolv`, three resolvers) and 70502079 (`-6 --resolv -R`)
    /// that way on 2026-10-13 (archive ref-30001, ref-70502079), each entry
    /// with its own time and lts (the third reported 5 s after it was sent).
    /// Starla 5120 asked 8.8.8.8 and stored no resultset (starla-30001).
    #[test]
    fn a_resultset_with_a_silent_resolver_is_what_ripe_stored() {
        let cfg = DnsConfig {
            resolv: true,
            af: 0,
            qtype: 1,
            qclass: 1,
            qbuf: true,
            query: "$p.atlas.ripe.net.$r.".into(),
            ..Default::default()
        };
        let entries = [
            ended(1791885574, 0, udp("127.0.0.1:53"), Some("127.0.0.1"), Some("OBcBAAABAAAAAAAABzEwMDAwMDQFYXRsYXMEcmlwZQNuZXQQOTNkYmIyZGYyMmU4MTcwNwAAAQAB"), reply("OBeBgwABAAAAAQAABzEwMDAwMDQFYXRsYXMEcmlwZQNuZXQQOTNkYmIyZGYyMmU4MTcwNwAAAQABAAAGAAEAAAcIAEABYQxyb290LXNlcnZlcnMDbmV0AAVuc3RsZAx2ZXJpc2lnbi1ncnMDY29tAHjD2jQAAAcIAAADhAAJOoAAAVGA", 79.436, None), Some((1, 3))),
            ended(1791885575, 0, udp("[::1]:53"), Some("::1"), Some("ODcBAAABAAAAAAAABzEwMDAwMDQFYXRsYXMEcmlwZQNuZXQQOGVlNmIzODFhNjdmMDVhNAAAAQAB"), reply("ODeBgwABAAAAAQAABzEwMDAwMDQFYXRsYXMEcmlwZQNuZXQQOGVlNmIzODFhNjdmMDVhNAAAAQABAAAGAAEAAAcHAEABYQxyb290LXNlcnZlcnMDbmV0AAVuc3RsZAx2ZXJpc2lnbi1ncnMDY29tAHjD2jQAAAcIAAADhAAJOoAAAVGA", 30.074, None), Some((2, 3))),
            ended(1791885576, 5, udp("147.28.0.46:53"), Some("147.28.0.33"), Some("krQBAAABAAAAAAAABzEwMDAwMDQFYXRsYXMEcmlwZQNuZXQQZmYzNmRjYzk3ODBlZGU5MgAAAQAB"), Outcome::Timeout, Some((3, 3))),
        ];
        let line = uploaded(&format_line(30001, &cfg, &entries, 1791885581), 53);
        assert_eq!(
            stored(&line),
            archived(
                r#"{"fw":5130,"mver":"2.6.4","lts":46,"resultset":[{"time":1791885574,"lts":46,"subid":1,"submax":3,"dst_addr":"127.0.0.1","dst_port":"53","af":4,"src_addr":"127.0.0.1","proto":"UDP","qbuf":"OBcBAAABAAAAAAAABzEwMDAwMDQFYXRsYXMEcmlwZQNuZXQQOTNkYmIyZGYyMmU4MTcwNwAAAQAB","result":{"rt":79.436,"size":132,"abuf":"OBeBgwABAAAAAQAABzEwMDAwMDQFYXRsYXMEcmlwZQNuZXQQOTNkYmIyZGYyMmU4MTcwNwAAAQABAAAGAAEAAAcIAEABYQxyb290LXNlcnZlcnMDbmV0AAVuc3RsZAx2ZXJpc2lnbi1ncnMDY29tAHjD2jQAAAcIAAADhAAJOoAAAVGA","ID":14359,"ANCOUNT":0,"QDCOUNT":1,"NSCOUNT":1,"ARCOUNT":0}},{"time":1791885575,"lts":47,"subid":2,"submax":3,"dst_addr":"::1","dst_port":"53","af":6,"src_addr":"::1","proto":"UDP","qbuf":"ODcBAAABAAAAAAAABzEwMDAwMDQFYXRsYXMEcmlwZQNuZXQQOGVlNmIzODFhNjdmMDVhNAAAAQAB","result":{"rt":30.074,"size":132,"abuf":"ODeBgwABAAAAAQAABzEwMDAwMDQFYXRsYXMEcmlwZQNuZXQQOGVlNmIzODFhNjdmMDVhNAAAAQABAAAGAAEAAAcHAEABYQxyb290LXNlcnZlcnMDbmV0AAVuc3RsZAx2ZXJpc2lnbi1ncnMDY29tAHjD2jQAAAcIAAADhAAJOoAAAVGA","ID":14391,"ANCOUNT":0,"QDCOUNT":1,"NSCOUNT":1,"ARCOUNT":0}},{"time":1791885576,"lts":53,"subid":3,"submax":3,"dst_addr":"147.28.0.46","dst_port":"53","af":4,"src_addr":"147.28.0.33","proto":"UDP","qbuf":"krQBAAABAAAAAAAABzEwMDAwMDQFYXRsYXMEcmlwZQNuZXQQZmYzNmRjYzk3ODBlZGU5MgAAAQAB","error":{"timeout":5000}}],"msm_id":30001,"prb_id":1000004,"timestamp":1791885574,"msm_name":"Tdig","from":"147.28.0.33","type":"dns","stored_timestamp":1791885645}"#
            ),
            "{line}"
        );

        let cfg = DnsConfig {
            rd: true,
            qtype: 28,
            query: "$r-$t-$p.rpkitest6.nlnetlabs.nl.".into(),
            ..cfg
        };
        let entries = [
            ended(1791882861, 0, udp("127.0.0.1:53"), Some("127.0.0.1"), Some("3xABAAABAAAAAAAAI2FiMDA4NTkyMzZmZGQ5MDktMTc5MTg4Mjg2MS0xMDAwMDA0CXJwa2l0ZXN0NglubG5ldGxhYnMCbmwAABwAAQ=="), reply("3xCBgAABAAEAAAAAI2FiMDA4NTkyMzZmZGQ5MDktMTc5MTg4Mjg2MS0xMDAwMDA0CXJwa2l0ZXN0NglubG5ldGxhYnMCbmwAABwAAcAMABwAAQAAAAEAECoEuQcAAQAAAAAAAAAAAAY=", 324.937, None), Some((1, 3))),
            ended(1791882862, 0, udp("[::1]:53"), Some("::1"), Some("ghYBAAABAAAAAAAAI2ZkMDA3ZGM0NTE5NDBjMzAtMTc5MTg4Mjg2Mi0xMDAwMDA0CXJwa2l0ZXN0NglubG5ldGxhYnMCbmwAABwAAQ=="), reply("ghaBgAABAAEAAAAAI2ZkMDA3ZGM0NTE5NDBjMzAtMTc5MTg4Mjg2Mi0xMDAwMDA0CXJwa2l0ZXN0NglubG5ldGxhYnMCbmwAABwAAcAMABwAAQAAAAEAECoEuQcAAQAAAAAAAAAAAAY=", 320.82, None), Some((2, 3))),
            ended(1791882864, 5, udp("147.28.0.46:53"), Some("147.28.0.33"), Some("PoABAAABAAAAAAAAIzIwMGE1NTBjNGY0YjA0OWQtMTc5MTg4Mjg2NC0xMDAwMDA0CXJwa2l0ZXN0NglubG5ldGxhYnMCbmwAABwAAQ=="), Outcome::Timeout, Some((3, 3))),
        ];
        let line = uploaded(&format_line(70502079, &cfg, &entries, 1791882869), 48);
        assert_eq!(
            stored(&line),
            archived(
                r#"{"fw":5130,"mver":"2.6.4","lts":40,"resultset":[{"time":1791882861,"lts":40,"subid":1,"submax":3,"dst_addr":"127.0.0.1","dst_port":"53","af":4,"src_addr":"127.0.0.1","proto":"UDP","qbuf":"3xABAAABAAAAAAAAI2FiMDA4NTkyMzZmZGQ5MDktMTc5MTg4Mjg2MS0xMDAwMDA0CXJwa2l0ZXN0NglubG5ldGxhYnMCbmwAABwAAQ==","result":{"rt":324.937,"size":104,"abuf":"3xCBgAABAAEAAAAAI2FiMDA4NTkyMzZmZGQ5MDktMTc5MTg4Mjg2MS0xMDAwMDA0CXJwa2l0ZXN0NglubG5ldGxhYnMCbmwAABwAAcAMABwAAQAAAAEAECoEuQcAAQAAAAAAAAAAAAY=","ID":57104,"ANCOUNT":1,"QDCOUNT":1,"NSCOUNT":0,"ARCOUNT":0}},{"time":1791882862,"lts":41,"subid":2,"submax":3,"dst_addr":"::1","dst_port":"53","af":6,"src_addr":"::1","proto":"UDP","qbuf":"ghYBAAABAAAAAAAAI2ZkMDA3ZGM0NTE5NDBjMzAtMTc5MTg4Mjg2Mi0xMDAwMDA0CXJwa2l0ZXN0NglubG5ldGxhYnMCbmwAABwAAQ==","result":{"rt":320.82,"size":104,"abuf":"ghaBgAABAAEAAAAAI2ZkMDA3ZGM0NTE5NDBjMzAtMTc5MTg4Mjg2Mi0xMDAwMDA0CXJwa2l0ZXN0NglubG5ldGxhYnMCbmwAABwAAcAMABwAAQAAAAEAECoEuQcAAQAAAAAAAAAAAAY=","ID":33302,"ANCOUNT":1,"QDCOUNT":1,"NSCOUNT":0,"ARCOUNT":0}},{"time":1791882864,"lts":48,"subid":3,"submax":3,"dst_addr":"147.28.0.46","dst_port":"53","af":4,"src_addr":"147.28.0.33","proto":"UDP","qbuf":"PoABAAABAAAAAAAAIzIwMGE1NTBjNGY0YjA0OWQtMTc5MTg4Mjg2NC0xMDAwMDA0CXJwa2l0ZXN0NglubG5ldGxhYnMCbmwAABwAAQ==","error":{"timeout":5000}}],"msm_id":70502079,"prb_id":1000004,"timestamp":1791882861,"msm_name":"Tdig","from":"2001:418:1::33","type":"dns","group_id":70502079,"stored_timestamp":1791882953}"#
            ),
            "{line}"
        );
    }

    /// The shapes the twin never showed, as RIPE stored them from other
    /// probes on firmware 5130 (measurement-result-format "DNS Lookup";
    /// public results of 2026-10-13). A server given by name is reported as
    /// "name" beside dst_addr once the query is sent (218956432, 222191370),
    /// not as the documented "dst_name"; before that, and for a socket that
    /// could not connect, only as "dst_name" (211294130, 10001). TCP names
    /// the address it connects to even when the connect fails (10101).
    /// Every error is an object beside the addresses, never inside a
    /// "result".
    #[test]
    fn named_servers_and_errors_are_what_ripe_stored_from_5130_probes() {
        let udp_named = |server: &str, addr: &str| Dst::Udp {
            server: server.into(),
            addr: addr.parse().unwrap(),
        };
        let fail = std::io::Error::other(
            "failed to lookup address information: Temporary failure in name resolution",
        );
        let cases = [
            (
                // probe 1010752
                218956432,
                ended(1791887888, 5, udp_named("rcz.ns.ua", "193.46.128.10:53"), Some("193.37.71.252"), None, Outcome::Timeout, None),
                29,
                r#"{"fw":5130,"mver":"2.6.4","lts":29,"name":"rcz.ns.ua","dst_addr":"193.46.128.10","dst_port":"53","af":4,"src_addr":"193.37.71.252","proto":"UDP","error":{"timeout":5000},"msm_id":218956432,"prb_id":1010752,"timestamp":1791887888,"msm_name":"Tdig","from":"193.37.71.252","type":"dns","group_id":218956432,"stored_timestamp":1791887989}"#,
            ),
            (
                // probe 1004897
                222191370,
                ended(1791884462, 0, udp_named("b.nic.lv", "92.240.70.1:53"), Some("172.23.20.2"), None, reply("7/SFAAABAAEAAAABAWIDbmljAmx2AAABAAHADAABAAEAAA4QAARc8EYBAAApBNAAAAAAAAkAAwAFYi1uaWM=", 44.162, None), None),
                39,
                r#"{"fw":5130,"mver":"2.6.4","lts":39,"name":"b.nic.lv","dst_addr":"92.240.70.1","dst_port":"53","af":4,"src_addr":"172.23.20.2","proto":"UDP","result":{"rt":44.162,"size":62,"abuf":"7/SFAAABAAEAAAABAWIDbmljAmx2AAABAAHADAABAAEAAA4QAARc8EYBAAApBNAAAAAAAAkAAwAFYi1uaWM=","ID":61428,"ANCOUNT":1,"QDCOUNT":1,"NSCOUNT":0,"ARCOUNT":1},"msm_id":222191370,"prb_id":1004897,"timestamp":1791884462,"msm_name":"Tdig","from":"85.114.202.240","type":"dns","group_id":222191360,"stored_timestamp":1791884468}"#,
            ),
            (
                // probe 1008241: the resolver failed, not NXDOMAIN
                211294130,
                ended(1791889377, 0, Dst::Name("ns1.criteo.com.".into()), None, None, error(&resolver::getaddrinfo_failed("ns1.criteo.com.", resolver::evdns_error(&fail))), None),
                17,
                r#"{"fw":5130,"mver":"2.6.4","lts":17,"dst_name":"ns1.criteo.com.","proto":"UDP","error":{"evdns_getaddrinfo":"ns1.criteo.com. non-recoverable failure in name resolution"},"msm_id":211294130,"prb_id":1008241,"timestamp":1791889377,"msm_name":"Tdig","from":"163.253.39.162","type":"dns","group_id":211294130,"stored_timestamp":1791889498}"#,
            ),
            (
                // probe 1005060, no IPv4 route
                10001,
                ended(1791889417, 0, Dst::Name("193.0.14.129".into()), None, None, error(" \"socket\" : \"connect failed Network is unreachable\""), None),
                35,
                r#"{"fw":5130,"mver":"2.6.4","lts":35,"dst_name":"193.0.14.129","proto":"UDP","error":{"socket":"connect failed Network is unreachable"},"msm_id":10001,"prb_id":1005060,"timestamp":1791889417,"msm_name":"Tdig","from":"212.51.150.115","type":"dns","stored_timestamp":1791889505}"#,
            ),
            (
                // probe 1005060 over TCP
                10101,
                ended(1791889143, 0, tcp("193.0.14.129:53"), None, None, error(" \"TUCONNECT\" : \"Network is unreachable\""), None),
                5,
                r#"{"fw":5130,"mver":"2.6.4","lts":5,"dst_addr":"193.0.14.129","dst_port":"53","af":4,"proto":"TCP","error":{"TUCONNECT":"Network is unreachable"},"msm_id":10101,"prb_id":1005060,"timestamp":1791889143,"msm_name":"Tdig","from":"212.51.150.115","type":"dns","stored_timestamp":1791889194}"#,
            ),
            (
                // probe 1002526: no connection within CONN_TO
                10101,
                ended(1791889064, 5, tcp("193.0.14.129:53"), None, None, Outcome::Timeout, None),
                52,
                r#"{"fw":5130,"mver":"2.6.4","lts":52,"dst_addr":"193.0.14.129","dst_port":"53","af":4,"proto":"TCP","error":{"timeout":5000},"msm_id":10101,"prb_id":1002526,"timestamp":1791889064,"msm_name":"Tdig","from":"174.160.4.224","type":"dns","stored_timestamp":1791889121}"#,
            ),
            (
                // probe 1017267, a firewall refusing the send
                11001,
                ended(1791889532, 0, udp("[2001:7fd::1]:53"), Some("2001:41d0:700:243c::"), None, error(" \"senderror\" : \"AF Operation not permitted, NOT AF_INET\""), None),
                9,
                r#"{"fw":5130,"mver":"2.6.4","lts":9,"dst_addr":"2001:7fd::1","dst_port":"53","af":6,"src_addr":"2001:41d0:700:243c::","proto":"UDP","error":{"senderror":"AF Operation not permitted, NOT AF_INET"},"msm_id":11001,"prb_id":1017267,"timestamp":1791889532,"msm_name":"Tdig","from":"2001:41d0:700:243c::","type":"dns","stored_timestamp":1791889599}"#,
            ),
        ];
        for (msm, e, lts, want) in cases {
            let cfg = DnsConfig {
                tcp: matches!(e.attempt.dst, Dst::Tcp { .. }),
                ..Default::default()
            };
            let end = e.printed;
            let line = uploaded(&format_line(msm, &cfg, &[e], end), lts);
            assert_eq!(stored(&line), archived(want), "{msm}: {line}");
        }
    }

    /// A server that can't be resolved is reported in evdns's words, which
    /// are not the system resolver's: RIPE stored "rcz.ns.ua nodename nor
    /// servname provided, or not known" (218956432, probe 1017360) where
    /// glibc says "Name or service not known". An address of the other
    /// family is a name to evdns, and no such name.
    #[tokio::test]
    async fn a_server_that_does_not_resolve_is_reported_as_evdns_words_it() {
        let cfg = DnsConfig {
            af: 6,
            server: "192.0.2.1".into(),
            query: "id.server.".into(),
            ..Default::default()
        };
        let entries = run(&cfg, 1, None).await;
        let line = uploaded(&format_line(1, &cfg, &entries, entries[0].printed), 7);
        assert!(line.ends_with(r#""lts" : 7 , "dst_name" : "192.0.2.1" , "proto" : "UDP" , "error" : {"evdns_getaddrinfo": "192.0.2.1 nodename nor servname provided, or not known"} }"#), "{line}");

        let cfg = DnsConfig { tcp: true, ..cfg };
        let entries = run(&cfg, 1, None).await;
        let line = uploaded(&format_line(1, &cfg, &entries, entries[0].printed), 7);
        assert!(line.ends_with(r#""dst_name" : "192.0.2.1" , "proto" : "TCP" , "error" : { "TUDNS" : "nodename nor servname provided, or not known"} }"#), "{line}");

        // evdns answers EAI_NONAME for NXDOMAIN and EAI_FAIL for the rest
        // (evdns_err_to_getaddrinfo_err(), evdns.c:4381)
        for (system, evdns) in [
            (
                "Name or service not known",
                "nodename nor servname provided, or not known",
            ),
            (
                "Name does not resolve",
                "nodename nor servname provided, or not known",
            ),
            (
                "nodename nor servname provided, or not known",
                "nodename nor servname provided, or not known",
            ),
            (
                "Temporary failure in name resolution",
                "non-recoverable failure in name resolution",
            ),
            (
                "No address associated with hostname",
                "non-recoverable failure in name resolution",
            ),
            (
                "Non-recoverable failure in name resolution",
                "non-recoverable failure in name resolution",
            ),
        ] {
            let e =
                std::io::Error::other(format!("failed to lookup address information: {system}"));
            assert_eq!(resolver::evdns_error(&e), evdns, "{system}");
        }
    }

    /// A DNS server on a local socket whose replies leave with TTL (hop
    /// limit) 42.
    async fn udp_server_hops(addr: &str) -> SocketAddr {
        let sock = UdpSocket::bind(addr).await.unwrap();
        let local = sock.local_addr().unwrap();
        let sref = socket2::SockRef::from(&sock);
        if local.is_ipv6() {
            sref.set_unicast_hops_v6(42).unwrap();
        } else {
            sref.set_ttl_v4(42).unwrap();
        }
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
                let mut r = buf[..n].to_vec();
                r[2] |= 0x80;
                let _ = sock.send_to(&r, peer).await;
            }
        });
        local
    }

    /// `--ttl` (O_TTL, evtdig.c:2630) reports the TTL or hop limit the UDP
    /// reply arrived with, as `"ttl"` between the addresses and "proto"
    /// (printReply(), evtdig.c:3810), not in "result" where the
    /// documentation puts it. Without `--ttl`, or over TCP, there is none.
    // The hop limit arrives via IP_RECVTTL/IPV6_RECVHOPLIMIT cmsg, Unix only.
    #[cfg(unix)]
    #[tokio::test]
    async fn ttl_reports_the_hop_limit_of_the_reply() {
        for (bind, family) in [("127.0.0.1:0", 4u8), ("[::1]:0", 6)] {
            let server = udp_server_hops(bind).await;
            let cfg = DnsConfig {
                server: server.ip().to_string(),
                af: family,
                port: server.port(),
                query: "hostname.bind.".into(),
                ttl: true,
                timeout_ms: 2000,
                disable_check_addr: true,
                ..Default::default()
            };
            let entries = run(&cfg, 1, None).await;
            let line = uploaded(&format_line(11301, &cfg, &entries, entries[0].printed), 3);
            let ip = server.ip();
            assert!(
                line.contains(&format!(
                    r#""src_addr" : "{ip}" , "ttl" : 42 , "proto" : "UDP" ,"result" : {{ "rt" : "#
                )),
                "{line}"
            );
            let v = stored(&line);
            assert_eq!(v["ttl"], 42, "{line}");
            assert!(v["result"].get("ttl").is_none(), "{line}");

            let cfg = DnsConfig { ttl: false, ..cfg };
            let entries = run(&cfg, 1, None).await;
            let line = format_line(11301, &cfg, &entries, entries[0].printed);
            assert!(
                line.contains("\"rt\"") && !line.contains("\"ttl\""),
                "{line}"
            );
        }
    }

    /// atlas_check_addr() (libbb/atlas_check_addr.c): a UDP query to
    /// loopback, a private or link-local network, ULA or multicast is not
    /// sent unless with `--resolv`, and a TCP one is never connected. The
    /// reference reports `"error" : { "reason" : "address not allowed"}`
    /// with the address (evtdig.c:1358-1375), or `{ "TU_BAD_ADDR" : true}`
    /// and asks again while `--retry` allows (tcputil.c:262,
    /// evtdig.c:1612). A name that resolves to one is refused the same
    /// way. Starla asked: `evtdig hostname.bind. 127.0.0.1` reported a
    /// result from 127.0.0.1.
    #[tokio::test]
    async fn a_server_atlas_check_addr_refuses_is_reported_not_asked() {
        if std::env::var("ATLAS_DISABLE_CHECK_ADDR").is_ok() {
            return;
        }
        let (server, mut rx) = udp_server("127.0.0.1:0").await;
        let port = server.port();
        let cfg = DnsConfig {
            server: "127.0.0.1".into(),
            port,
            query: "hostname.bind.".into(),
            timeout_ms: 200,
            ..Default::default()
        };
        let entries = run(&cfg, 1, None).await;
        let line = uploaded(&format_line(11301, &cfg, &entries, entries[0].printed), 3);
        let t = entries[0].attempt.time;
        assert_eq!(
            line,
            format!(
                r#"RESULT {{ "id" : "11301" , "fw":5130, "mver": "2.6.4", "time" : {t} , "lts" : 3 , "dst_addr" : "127.0.0.1" , "dst_port" : "{port}" , "af" : 4 , "proto" : "UDP" , "error" : {{ "reason" : "address not allowed"}} }}"#
            )
        );
        let refused = |server: &str, af: u8| DnsConfig {
            server: server.into(),
            af,
            ..cfg.clone()
        };
        for (cfg, dst) in [
            (
                refused("localhost", 4),
                format!(
                    r#""name" : "localhost" , "dst_addr" : "127.0.0.1" , "dst_port" : "{port}" , "af" : 4"#
                ),
            ),
            (
                refused("::1", 6),
                format!(r#""dst_addr" : "::1" , "dst_port" : "{port}" , "af" : 6"#),
            ),
        ] {
            let entries = run(&cfg, 1, None).await;
            let line = uploaded(&format_line(11301, &cfg, &entries, entries[0].printed), 3);
            let want = format!(
                r#"{dst} , "proto" : "UDP" , "error" : {{ "reason" : "address not allowed"}} }}"#
            );
            assert!(line.ends_with(&want), "{line}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(rx.try_recv().is_err(), "the server was asked");

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg = DnsConfig {
            tcp: true,
            port,
            retry: 1,
            ..cfg
        };
        let entries = run(&cfg, 1, None).await;
        let line = uploaded(&format_line(11101, &cfg, &entries, entries[0].printed), 3);
        assert!(line.ends_with(&format!(r#""lts" : 3 , "dst_addr" : "127.0.0.1" , "dst_port" : "{port}" , "af" : 4 , "retry" : 1 , "proto" : "TCP" , "error" : {{ "TU_BAD_ADDR" : true}} }}"#)), "{line}");
        // --resolv spares UDP the check, not TCP
        let cfg = DnsConfig {
            resolv: true,
            af: 0,
            retry: 0,
            ..cfg
        };
        let entries = run(&cfg, 1, Some(vec!["127.0.0.1".into()])).await;
        let line = uploaded(&format_line(30001, &cfg, &entries, entries[0].printed), 3);
        assert!(line.ends_with(&format!(r#""subid" : 1 , "submax" : 1 , "dst_addr" : "127.0.0.1" , "dst_port" : "{port}" , "af" : 4 , "proto" : "TCP" , "error" : {{ "TU_BAD_ADDR" : true}}}}] }}"#)), "{line}");
        let accepted = tokio::time::timeout(Duration::from_millis(100), listener.accept()).await;
        assert!(accepted.is_err(), "connected");
    }

    /// The loopback interface: its name and index. Unix only:
    /// `libc::if_nametoindex` has no Windows binding, and the test that uses
    /// it checks glibc getnameinfo formatting.
    #[cfg(unix)]
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

    /// The configuration of msm 30001: `-4 --resolv --qbuf --type 1
    /// --class 1 --query $p.atlas.ripe.net.$r.`
    fn msm_30001() -> DnsConfig {
        DnsConfig {
            resolv: true,
            af: 0,
            qtype: 1,
            qclass: 1,
            qbuf: true,
            query: "$p.atlas.ripe.net.$r.".into(),
            ..Default::default()
        }
    }

    /// A link-local address keeps its zone where getnameinfo() writes it:
    /// `src_addr`, and TCP's `dst_addr` (evtdig.c:3798, 1653); UDP's
    /// `dst_addr` is inet_ntop()'s, without it. Probe 1014068's msm 30001
    /// as RIPE stored it from firmware 5130 on 2026-10-13, its interface
    /// (ens18) the test host's loopback. Starla dropped the zone from
    /// src_addr.
    #[cfg(unix)]
    #[test]
    fn link_local_addresses_keep_their_zone_where_getnameinfo_writes_them() {
        let (lo, index) = lo();
        let scoped = |ip: &str, port: u16| {
            SocketAddr::V6(std::net::SocketAddrV6::new(
                ip.parse().unwrap(),
                port,
                0,
                index,
            ))
        };
        let cfg = msm_30001();
        let mut second = ended(
            1791899373,
            0,
            Dst::Udp {
                server: format!("fe80::3163:8f21:de61:6a0a%{lo}"),
                addr: scoped("fe80::3163:8f21:de61:6a0a", 53),
            },
            None,
            Some("yNcBAAABAAAAAAAABzEwMTQwNjgFYXRsYXMEcmlwZQNuZXQQZmM4ZTg5ZmM3NDdkZGRiMgAAAQAB"),
            reply("yNeBgwABAAAAAQAABzEwMTQwNjgFYXRsYXMEcmlwZQNuZXQQZmM4ZTg5ZmM3NDdkZGRiMgAAAQABAAAGAAEAAVGAAEABYQxyb290LXNlcnZlcnMDbmV0AAVuc3RsZAx2ZXJpc2lnbi1ncnMDY29tAHjD2jQAAAcIAAADhAAJOoAAAVGA", 68.181, None),
            Some((2, 2)),
        );
        second.attempt.src = Some(scoped("fe80::b2db:876:1abb:ef37", 41234));
        let entries = [
            ended(1791899371, 1, udp("10.0.0.3:53"), Some("10.0.0.12"), Some("YHMBAAABAAAAAAAABzEwMTQwNjgFYXRsYXMEcmlwZQNuZXQQYzIzMGU2NjBhZjI5N2M5NgAAAQAB"), reply("YHOBgwABAAAAAQAABzEwMTQwNjgFYXRsYXMEcmlwZQNuZXQQYzIzMGU2NjBhZjI5N2M5NgAAAQABAAAGAAEAAVGAAEABYQxyb290LXNlcnZlcnMDbmV0AAVuc3RsZAx2ZXJpc2lnbi1ncnMDY29tAHjD2jQAAAcIAAADhAAJOoAAAVGA", 383.823, None), Some((1, 2))),
            second,
        ];
        let line = uploaded(&format_line(30001, &cfg, &entries, 1791899373), 26);
        let want = r#"{"fw":5130,"mver":"2.6.4","lts":25,"resultset":[{"time":1791899371,"lts":25,"subid":1,"submax":2,"dst_addr":"10.0.0.3","dst_port":"53","af":4,"src_addr":"10.0.0.12","proto":"UDP","qbuf":"YHMBAAABAAAAAAAABzEwMTQwNjgFYXRsYXMEcmlwZQNuZXQQYzIzMGU2NjBhZjI5N2M5NgAAAQAB","result":{"rt":383.823,"size":132,"abuf":"YHOBgwABAAAAAQAABzEwMTQwNjgFYXRsYXMEcmlwZQNuZXQQYzIzMGU2NjBhZjI5N2M5NgAAAQABAAAGAAEAAVGAAEABYQxyb290LXNlcnZlcnMDbmV0AAVuc3RsZAx2ZXJpc2lnbi1ncnMDY29tAHjD2jQAAAcIAAADhAAJOoAAAVGA","ID":24691,"ANCOUNT":0,"QDCOUNT":1,"NSCOUNT":1,"ARCOUNT":0}},{"time":1791899373,"lts":26,"subid":2,"submax":2,"name":"fe80::3163:8f21:de61:6a0a%ens18","dst_addr":"fe80::3163:8f21:de61:6a0a","dst_port":"53","af":6,"src_addr":"fe80::b2db:876:1abb:ef37%ens18","proto":"UDP","qbuf":"yNcBAAABAAAAAAAABzEwMTQwNjgFYXRsYXMEcmlwZQNuZXQQZmM4ZTg5ZmM3NDdkZGRiMgAAAQAB","result":{"rt":68.181,"size":132,"abuf":"yNeBgwABAAAAAQAABzEwMTQwNjgFYXRsYXMEcmlwZQNuZXQQZmM4ZTg5ZmM3NDdkZGRiMgAAAQABAAAGAAEAAVGAAEABYQxyb290LXNlcnZlcnMDbmV0AAVuc3RsZAx2ZXJpc2lnbi1ncnMDY29tAHjD2jQAAAcIAAADhAAJOoAAAVGA","ID":51415,"ANCOUNT":0,"QDCOUNT":1,"NSCOUNT":1,"ARCOUNT":0}}],"msm_id":30001,"prb_id":1014068,"timestamp":1791899371,"msm_name":"Tdig","from":"73.133.220.210","type":"dns","stored_timestamp":1791899466}"#;
        let want = want.replace("%ens18", &format!("%{lo}"));
        assert_eq!(stored(&line), archived(&want), "{line}");

        // TCP: the server is its own dst_addr, so no dst_name
        let cfg = DnsConfig {
            tcp: true,
            ..Default::default()
        };
        let server = format!("fe80::1%{lo}");
        let mut e = ended(
            1791885574,
            0,
            Dst::Tcp {
                server: server.clone(),
                addr: scoped("fe80::1", 53),
            },
            None,
            None,
            Outcome::Timeout,
            None,
        );
        e.attempt.src = Some(scoped("fe80::1", 50000));
        let line = uploaded(&format_line(11101, &cfg, &[e], 1791885579), 5);
        let v = stored(&line);
        assert!(v.get("dst_name").is_none(), "{line}");
        assert_eq!(v["dst_addr"], server.as_str(), "{line}");
        assert_eq!(v["src_addr"], server.as_str(), "{line}");
    }

    /// The same through a real resolver: lo0 has fe80::1 on macOS.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_link_local_resolver_reports_its_source_with_the_zone() {
        let scoped = tokio::net::lookup_host(("fe80::1%lo0", 0))
            .await
            .unwrap()
            .next()
            .unwrap();
        let (server, mut rx) = udp_server(scoped).await;
        let port = server.port();
        let cfg = DnsConfig {
            resolv: true,
            af: 0,
            port,
            qtype: 1,
            qclass: 1,
            query: "example.".into(),
            timeout_ms: 2000,
            ..Default::default()
        };
        let entries = run(&cfg, 1, Some(vec!["fe80::1%lo0".into()])).await;
        assert!(rx.recv().await.is_some());
        let line = uploaded(&format_line(30001, &cfg, &entries, entries[0].printed), 3);
        assert!(line.contains(&format!(r#""subid" : 1 , "submax" : 1 , "name" : "fe80::1%lo0" , "dst_addr" : "fe80::1" , "dst_port" : "{port}" , "af" : 6 , "src_addr" : "fe80::1%lo0" , "proto" : "UDP" ,"result" : {{ "rt" : "#)), "{line}");

        let listener = TcpListener::bind(scoped).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let answered = answer_tcp_once(listener);
        let cfg = DnsConfig {
            server: "fe80::1%lo0".into(),
            af: 6,
            tcp: true,
            port,
            query: "hostname.bind.".into(),
            disable_check_addr: true,
            ..Default::default()
        };
        let entries = run(&cfg, 1, None).await;
        answered.await.unwrap();
        let line = uploaded(&format_line(11101, &cfg, &entries, entries[0].printed), 3);
        assert!(line.contains(&format!(r#""lts" : 3 , "dst_addr" : "fe80::1%lo0" , "dst_port" : "{port}" , "af" : 6 , "src_addr" : "fe80::1%lo0" , "proto" : "TCP" ,"result" : {{ "rt" : "#)), "{line}");
    }

    /// What one query leaves, the next that lacks it reports (see
    /// [`Carry`]): the source address, `--ttl`'s TTL, and TCP's last
    /// address. The resultsets are msm 30001 as RIPE stored it from
    /// firmware 5130 on 2026-10-13. Probe 1010265's second resolver did not
    /// connect, and its entry has the first one's source; probe 1013200's
    /// first did not, and its entry has the source the run before it left.
    #[test]
    fn what_one_query_leaves_the_next_reports() {
        let attempt = |dst: Dst, src: Option<&str>, ttl: Option<i32>, outcome: Outcome| Attempt {
            time: 1791885574,
            dst,
            src: src.map(local),
            ttl,
            qbuf: None,
            outcome,
        };
        let entry = |attempt: Attempt, sub: Option<(usize, usize)>| Entry {
            attempt,
            sub,
            retry: 0,
            printed: 1791885574,
        };
        let applied = |cfg: &DnsConfig, mut e: Entry| {
            cfg.carry.apply(&mut e.attempt, false);
            e
        };
        let abuf = "i0mFAAABAAEAAAAABzEwMTgyMTIFYXRsYXMEcmlwZQNuZXQQYTdhMjM3OWViZTM2MmFiMwAAAQABwAwAAQABAAABLAAErB5KFA==";

        let cfg = msm_30001();
        let entries = [
            applied(&cfg, ended(1791899371, 0, udp("100.100.100.100:53"), Some("100.83.59.57"), Some("CrkBAAABAAAAAAAABzEwMTAyNjUFYXRsYXMEcmlwZQNuZXQQN2IxY2EzYTg5ZDVhYTQ0ZgAAAQAB"), reply("CrmBgwABAAAAAQAABzEwMTAyNjUFYXRsYXMEcmlwZQNuZXQQN2IxY2EzYTg5ZDVhYTQ0ZgAAAQABAAAGAAEAAKgQAEABYQxyb290LXNlcnZlcnMDbmV0AAVuc3RsZAx2ZXJpc2lnbi1ncnMDY29tAHjD2jQAAAcIAAADhAAJOoAAAVGA", 36.637, None), Some((1, 2)))),
            applied(&cfg, ended(1791899372, 0, Dst::Name("fd7a:115c:a1e0::53".into()), None, None, error(" \"socket\" : \"connect failed Cannot assign requested address\""), Some((2, 2)))),
        ];
        let line = uploaded(&format_line(30001, &cfg, &entries, 1791899372), 42648);
        assert_eq!(
            stored(&line),
            archived(
                r#"{"fw":5130,"mver":"2.6.4","lts":42647,"resultset":[{"time":1791899371,"lts":42647,"subid":1,"submax":2,"dst_addr":"100.100.100.100","dst_port":"53","af":4,"src_addr":"100.83.59.57","proto":"UDP","qbuf":"CrkBAAABAAAAAAAABzEwMTAyNjUFYXRsYXMEcmlwZQNuZXQQN2IxY2EzYTg5ZDVhYTQ0ZgAAAQAB","result":{"rt":36.637,"size":132,"abuf":"CrmBgwABAAAAAQAABzEwMTAyNjUFYXRsYXMEcmlwZQNuZXQQN2IxY2EzYTg5ZDVhYTQ0ZgAAAQABAAAGAAEAAKgQAEABYQxyb290LXNlcnZlcnMDbmV0AAVuc3RsZAx2ZXJpc2lnbi1ncnMDY29tAHjD2jQAAAcIAAADhAAJOoAAAVGA","ID":2745,"ANCOUNT":0,"QDCOUNT":1,"NSCOUNT":1,"ARCOUNT":0}},{"time":1791899372,"lts":42648,"subid":2,"submax":2,"dst_name":"fd7a:115c:a1e0::53","src_addr":"100.83.59.57","proto":"UDP","error":{"socket":"connect failed Cannot assign requested address"}}],"msm_id":30001,"prb_id":1010265,"timestamp":1791899371,"msm_name":"Tdig","from":"83.59.16.49","type":"dns","stored_timestamp":1791899429}"#
            ),
            "{line}"
        );

        // The run before 1013200's: its last resolver connected
        let cfg = msm_30001();
        let mut before = attempt(
            udp("[2001:4860:4860::8844]:53"),
            Some("2a10:4646:4b0:1::26"),
            None,
            reply(abuf, 9.0, None),
        );
        cfg.clone().carry.apply(&mut before, false);
        let entries = [
            applied(&cfg, ended(1791899611, 0, Dst::Name("1.1.1.1".into()), None, None, error(" \"socket\" : \"connect failed Network is unreachable\""), Some((1, 3)))),
            applied(&cfg, ended(1791899612, 0, udp("[2001:4860:4860::8888]:53"), Some("2a10:4646:4b0:1::26"), Some("AC8BAAABAAAAAAAABzEwMTMyMDAFYXRsYXMEcmlwZQNuZXQQZGQ4YjkzZTg5ZGZhYjU1NwAAAQAB"), reply("AC+BgwABAAAAAQAABzEwMTMyMDAFYXRsYXMEcmlwZQNuZXQQZGQ4YjkzZTg5ZGZhYjU1NwAAAQABAAAGAAEAAVFIAEABYQxyb290LXNlcnZlcnMDbmV0AAVuc3RsZAx2ZXJpc2lnbi1ncnMDY29tAHjD2jQAAAcIAAADhAAJOoAAAVGA", 8.842, None), Some((2, 3)))),
            applied(&cfg, ended(1791899613, 0, udp("[2001:4860:4860::8844]:53"), Some("2a10:4646:4b0:1::26"), Some("cGQBAAABAAAAAAAABzEwMTMyMDAFYXRsYXMEcmlwZQNuZXQQZjQ0YTFjYTJjYWZlNjlkZAAAAQAB"), reply("cGSBgwABAAAAAQAABzEwMTMyMDAFYXRsYXMEcmlwZQNuZXQQZjQ0YTFjYTJjYWZlNjlkZAAAAQABAAAGAAEAAVFrAEABYQxyb290LXNlcnZlcnMDbmV0AAVuc3RsZAx2ZXJpc2lnbi1ncnMDY29tAHjD2jQAAAcIAAADhAAJOoAAAVGA", 9.107, None), Some((3, 3)))),
        ];
        let line = uploaded(&format_line(30001, &cfg, &entries, 1791899613), 56);
        assert_eq!(
            stored(&line),
            archived(
                r#"{"fw":5130,"mver":"2.6.4","lts":54,"resultset":[{"time":1791899611,"lts":54,"subid":1,"submax":3,"dst_name":"1.1.1.1","src_addr":"2a10:4646:4b0:1::26","proto":"UDP","error":{"socket":"connect failed Network is unreachable"}},{"time":1791899612,"lts":55,"subid":2,"submax":3,"dst_addr":"2001:4860:4860::8888","dst_port":"53","af":6,"src_addr":"2a10:4646:4b0:1::26","proto":"UDP","qbuf":"AC8BAAABAAAAAAAABzEwMTMyMDAFYXRsYXMEcmlwZQNuZXQQZGQ4YjkzZTg5ZGZhYjU1NwAAAQAB","result":{"rt":8.842,"size":132,"abuf":"AC+BgwABAAAAAQAABzEwMTMyMDAFYXRsYXMEcmlwZQNuZXQQZGQ4YjkzZTg5ZGZhYjU1NwAAAQABAAAGAAEAAVFIAEABYQxyb290LXNlcnZlcnMDbmV0AAVuc3RsZAx2ZXJpc2lnbi1ncnMDY29tAHjD2jQAAAcIAAADhAAJOoAAAVGA","ID":47,"ANCOUNT":0,"QDCOUNT":1,"NSCOUNT":1,"ARCOUNT":0}},{"time":1791899613,"lts":56,"subid":3,"submax":3,"dst_addr":"2001:4860:4860::8844","dst_port":"53","af":6,"src_addr":"2a10:4646:4b0:1::26","proto":"UDP","qbuf":"cGQBAAABAAAAAAAABzEwMTMyMDAFYXRsYXMEcmlwZQNuZXQQZjQ0YTFjYTJjYWZlNjlkZAAAAQAB","result":{"rt":9.107,"size":132,"abuf":"cGSBgwABAAAAAQAABzEwMTMyMDAFYXRsYXMEcmlwZQNuZXQQZjQ0YTFjYTJjYWZlNjlkZAAAAQABAAAGAAEAAVFrAEABYQxyb290LXNlcnZlcnMDbmV0AAVuc3RsZAx2ZXJpc2lnbi1ncnMDY29tAHjD2jQAAAcIAAADhAAJOoAAAVGA","ID":28772,"ANCOUNT":0,"QDCOUNT":1,"NSCOUNT":1,"ARCOUNT":0}}],"msm_id":30001,"prb_id":1013200,"timestamp":1791899611,"msm_name":"Tdig","from":"","type":"dns","stored_timestamp":1791899684}"#
            ),
            "{line}"
        );
        // A new CRONLINE has nothing left over
        let mut fresh = attempt(Dst::Name("1.1.1.1".into()), None, None, Outcome::Timeout);
        msm_30001().carry.apply(&mut fresh, false);
        assert_eq!(fresh.src, None);

        // --ttl: a timeout after a reply reports the reply's TTL; a reply
        // without one (rcvdttl -42) leaves none
        let cfg = DnsConfig {
            ttl: true,
            ..Default::default()
        };
        let dst = || udp("193.0.14.129:53");
        let mut replied = attempt(
            dst(),
            Some("212.51.150.115"),
            Some(57),
            reply(abuf, 12.0, None),
        );
        let mut silent = attempt(dst(), Some("212.51.150.115"), None, Outcome::Timeout);
        cfg.carry.apply(&mut replied, false);
        cfg.carry.apply(&mut silent, false);
        let line = uploaded(
            &format_line(11301, &cfg, &[entry(silent, None)], 1791885574),
            2,
        );
        assert!(line.ends_with(r#""src_addr" : "212.51.150.115" , "ttl" : 57 , "proto" : "UDP" , "error" : { "timeout" : 5000} }"#), "{line}");
        let mut bare = attempt(dst(), Some("212.51.150.115"), None, reply(abuf, 12.0, None));
        let mut silent = attempt(dst(), None, None, Outcome::Timeout);
        cfg.carry.apply(&mut bare, false);
        cfg.carry.apply(&mut silent, false);
        assert_eq!(silent.ttl, None);

        // TCP: a server that no longer resolves is reported with the last
        // address connected to, which tcp_beforeconnect() left
        let cfg = DnsConfig {
            tcp: true,
            ..Default::default()
        };
        let mut connected = attempt(
            Dst::Tcp {
                server: "k.root-servers.net.".into(),
                addr: "193.0.14.129:53".parse().unwrap(),
            },
            Some("212.51.150.115"),
            None,
            reply(abuf, 12.0, Some(6.0)),
        );
        let tudns = " \"TUDNS\" : \"non-recoverable failure in name resolution\"";
        let mut unresolved = attempt(
            Dst::Name("k.root-servers.net.".into()),
            None,
            None,
            Outcome::Error {
                text: tudns.into(),
                retry: true,
            },
        );
        cfg.carry.apply(&mut connected, true);
        cfg.carry.apply(&mut unresolved, true);
        let line = uploaded(
            &format_line(10101, &cfg, &[entry(unresolved, None)], 1791885574),
            2,
        );
        assert!(line.ends_with(r#""lts" : 2 , "dst_name" : "k.root-servers.net." , "dst_addr" : "193.0.14.129" , "dst_port" : "53" , "af" : 4 , "src_addr" : "212.51.150.115" , "proto" : "TCP" , "error" : { "TUDNS" : "non-recoverable failure in name resolution"} }"#), "{line}");
    }

    /// The same across real runs: a TCP server that answered and then went
    /// away is reported with the source of the run that reached it, and
    /// `--ttl`'s TTL stays after a timeout. Unix only: the carry is the cmsg
    /// hop limit, and the refused text `"Connection refused"` is Linux's (the
    /// carry half also uses the Unix-only TTL path further down).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_query_that_fails_early_reports_what_the_one_before_left() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let answered = answer_tcp_once(listener);
        let cfg = DnsConfig {
            server: "127.0.0.1".into(),
            tcp: true,
            port,
            qtype: 6,
            qclass: 1,
            query: ".".into(),
            disable_check_addr: true,
            ..Default::default()
        };
        let first = run(&cfg, 1, None).await;
        answered.await.unwrap();
        assert!(matches!(first[0].attempt.outcome, Outcome::Reply { .. }));
        // The listener is gone: refused before there is a source
        let second = run(&cfg, 1, None).await;
        let line = uploaded(&format_line(11101, &cfg, &second, second[0].printed), 3);
        assert!(line.ends_with(&format!(r#""dst_addr" : "127.0.0.1" , "dst_port" : "{port}" , "af" : 4 , "src_addr" : "127.0.0.1" , "proto" : "TCP" , "error" : {{ "TUCONNECT" : "Connection refused"}} }}"#)), "{line}");
        // A new CRONLINE has nothing left over
        let fresh = DnsConfig {
            carry: Carry::default(),
            ..cfg.clone()
        };
        let third = run(&fresh, 1, None).await;
        let line = format_line(11101, &fresh, &third, third[0].printed);
        assert!(!line.contains("src_addr"), "{line}");

        let server = udp_server_hops("127.0.0.1:0").await;
        let cfg = DnsConfig {
            server: "127.0.0.1".into(),
            port: server.port(),
            query: "hostname.bind.".into(),
            ttl: true,
            timeout_ms: 2000,
            disable_check_addr: true,
            ..Default::default()
        };
        run(&cfg, 1, None).await;
        // Its server silent now
        let mute = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent = DnsConfig {
            port: mute.local_addr().unwrap().port(),
            timeout_ms: 100,
            ..cfg.clone()
        };
        let entries = run(&silent, 1, None).await;
        let line = uploaded(
            &format_line(11301, &silent, &entries, entries[0].printed),
            3,
        );
        assert!(line.ends_with(r#""src_addr" : "127.0.0.1" , "ttl" : 42 , "proto" : "UDP" , "error" : { "timeout" : 100} }"#), "{line}");
    }

    /// A server name with several addresses of the family gets the query at
    /// each, each from its own socket, and the last one's reply is the
    /// result (tdig_send_query_callback()'s loop, evtdig.c:1255-1495).
    /// Starla asked only the first.
    #[tokio::test]
    async fn every_address_of_a_server_is_asked_and_the_last_reported() {
        let (first, mut rx1) = udp_server("127.0.0.1:0").await;
        let (last, mut rx2) = udp_server("127.0.0.1:0").await;
        let cfg = DnsConfig {
            qbuf: true,
            query: "hostname.bind.".into(),
            timeout_ms: 2000,
            disable_check_addr: true,
            ..Default::default()
        };
        let a = resolver::udp_query(&cfg, "ns.example.", &[first, last], "hostname.bind.", 1).await;
        let (q1, q2) = (rx1.recv().await.unwrap(), rx2.recv().await.unwrap());
        assert_eq!(q1[2..], q2[2..], "the same query, its own id");
        assert_eq!(
            a.dst,
            Dst::Udp {
                server: "ns.example.".into(),
                addr: last
            }
        );
        assert_eq!(a.qbuf.as_deref(), Some(&q2[..]));
        let Outcome::Reply { wire, .. } = &a.outcome else {
            panic!("{:?}", a.outcome)
        };
        assert_eq!(wire[..2], q2[..2]);
    }
}
