//! Starla's traceroute held against what RIPE stored, and against a busy
//! probe's traffic.
//!
//! - Replay: the reference probe's results for the scenario's measurements
//!   (probe 1000004, firmware 5130; the latest result of each as RIPE's archive
//!   had it on 2026-10-13, in testdata/) are turned back into the packets that
//!   drew them and run through Starla's reading of replies
//!   (`wire::parse_reply`), its trace (`engine::Trace`: which probe next, when
//!   the trace ends) and its result line (`Traceroute::result_body`). The line,
//!   as RIPE stores it (`id` → `msm_id`, `time` → `timestamp`, `lts` from the
//!   results queue, the backend's own fields aside), must be the stored object:
//!   the same keys in the same order, the same values. The scenario's
//!   traceroutes are all IPv6 (UDP and ICMP); the IPv4 and TCP replies' fields
//!   are pinned against traceroute.c in engine.rs and wire.rs.
//! - Cross-talk (N3): every run's raw ICMP socket hears all ICMP to the host.
//!   Starla's stored traceroutes of 2026-10-12, read through sockets that
//!   matched nothing, listed a root server's and other measurements' replies in
//!   hop 1 (starla-6152.json). Here concurrent traces, pings and other ICMP are
//!   fed to every trace, and each keeps only its own.

use super::engine::{Probe, Trace};
use super::wire::{self, TraceId, BASE_PORT, SRC_BASE_PORT};
use super::{Traceroute, TracerouteBackend, TracerouteConfig};
use crate::target::DstName;
use serde_json::Value;
use starla_common::{MeasurementId, ProbeId, TracerouteOptions, TracerouteProtocol as P};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

/// eperd's getpid() in these tests.
const PID: u32 = 4242;

fn ms(v: f64) -> Duration {
    Duration::from_nanos((v * 1e6).round() as u64)
}

fn ip6(tclass: u8, nxt: u8, hops: u8, src: Ipv6Addr, dst: Ipv6Addr, payload: &[u8]) -> Vec<u8> {
    let mut p = (6u32 << 28 | (tclass as u32) << 20).to_be_bytes().to_vec();
    p.extend((payload.len() as u16).to_be_bytes());
    p.extend([nxt, hops]);
    p.extend(src.octets());
    p.extend(dst.octets());
    p.extend(payload);
    p
}

fn ip4(proto: u8, ttl: u8, src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0x45, 0, 0, 0, 0, 0, 0, 0, ttl, proto, 0, 0];
    p[2..4].copy_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
    p.extend(src.octets());
    p.extend(dst.octets());
    p.extend(payload);
    p
}

fn udp(sport: u16, dport: u16, cksum: u16, payload: &[u8]) -> Vec<u8> {
    let mut u = sport.to_be_bytes().to_vec();
    u.extend(dport.to_be_bytes());
    u.extend(((8 + payload.len()) as u16).to_be_bytes());
    u.extend(cksum.to_be_bytes());
    u.extend(payload);
    u
}

/// An ICMP(v6) message: type, code, the four bytes after the checksum,
/// the rest. The reading checks no ICMP checksum (the kernel did).
fn icmp(typ: u8, code: u8, rest: [u8; 4], body: &[u8]) -> Vec<u8> {
    let mut m = vec![typ, code, 0, 0];
    m.extend(rest);
    m.extend(body);
    m
}

/// An RFC 4884 extension structure of `(class, type, payload)` objects.
fn icmpext(version: u8, objs: &[(u8, u8, Vec<u8>)]) -> Vec<u8> {
    let mut e = vec![version << 4, 0, 0, 0];
    for (class, ctype, payload) in objs {
        e.extend(((4 + payload.len()) as u16).to_be_bytes());
        e.extend([*class, *ctype]);
        e.extend(payload);
    }
    let c = wire::in_cksum(&e);
    e[2..4].copy_from_slice(&c.to_be_bytes());
    e
}

/// An IPv6 trace's probe as `net::Sender::send` builds it: the next
/// header and the packet after the IPv6 header.
fn probe6(id: &TraceId, src: Ipv6Addr, size: u16, seq: u16, tv: Duration) -> (u8, Vec<u8>) {
    let IpAddr::V6(dst) = id.target else {
        unreachable!("an IPv6 trace")
    };
    match id.protocol {
        P::UDP => {
            let dport = BASE_PORT.wrapping_add(id.paris.unwrap_or(seq));
            let data = wire::udp6_payload(id.pid, id.index, seq, tv, size);
            (17, udp(SRC_BASE_PORT + id.index, dport, 0, &data))
        }
        P::ICMP => (
            58,
            wire::icmp6_echo(src, dst, id.pid, id.index, seq, tv, size, id.paris),
        ),
        P::TCP => (
            6,
            wire::tcp_syn(
                src.into(),
                dst.into(),
                SRC_BASE_PORT + id.index,
                80,
                id.index,
                seq,
                size,
            ),
        ),
    }
}

/// The ICMPv6 message (and its hop limit and traffic class) that makes the
/// reference write the stored reply `e` for the probe `seq` sent at `tv`:
/// the target's echo reply; its port unreachable (no "err") or another
/// unreachable ("err"); or a router's time exceeded quoting the probe with
/// "ittl" hops left and "itos" as its traffic class, padded to 128 bytes
/// before an "icmpext" (RFC 4884 length in the header, or none).
fn reply6(
    id: &TraceId,
    src: Ipv6Addr,
    size: u16,
    e: &Value,
    seq: u16,
    tv: Duration,
) -> (Vec<u8>, i32, i32) {
    let IpAddr::V6(dst) = id.target else {
        unreachable!("an IPv6 trace")
    };
    let from: Ipv6Addr = e["from"].as_str().unwrap().parse().unwrap();
    let ttl = e["ttl"].as_i64().unwrap() as i32;
    let stored_size = e["size"].as_u64().unwrap() as usize;
    let byte = |k: &str| e.get(k).map(|v| v.as_u64().unwrap() as u8);
    let (nxt, probe) = probe6(id, src, size, seq, tv);
    if id.protocol == P::ICMP && from == dst && e.get("err").is_none() {
        let mut r = probe;
        r[0] = 129;
        assert_eq!(r.len() - 8, stored_size, "an echo reply's size: {e}");
        return (r, ttl, byte("itos").unwrap_or(0) as i32);
    }
    let (typ, code) = match e.get("err") {
        Some(Value::String(s)) => (
            1,
            match s.as_str() {
                "N" => 0,
                "A" => 1,
                "h" => 2,
                "H" => 3,
                other => panic!("no ICMPv6 code writes err {other}"),
            },
        ),
        Some(code) => (1, code.as_u64().unwrap() as u8),
        None if from == dst => (1, 4),
        None => (3, 0),
    };
    let mut quoted = ip6(
        byte("itos").unwrap_or(0),
        nxt,
        byte("ittl").unwrap_or(1),
        src,
        dst,
        &probe,
    );
    let mut rest = [0u8; 4];
    if let Some(ext) = e.get("icmpext") {
        quoted.resize(128, 0);
        if ext["rfc4884"] == 1 {
            rest[0] = 128 / 8;
        }
        let objs: Vec<(u8, u8, Vec<u8>)> = ext["obj"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| {
                let mpls = o.get("mpls").map(|m| {
                    m.as_array()
                        .unwrap()
                        .iter()
                        .flat_map(|l| {
                            let f = |k: &str| l[k].as_u64().unwrap() as u32;
                            (f("label") << 12 | f("exp") << 9 | f("s") << 8 | f("ttl"))
                                .to_be_bytes()
                        })
                        .collect()
                });
                let f = |k: &str| o[k].as_u64().unwrap() as u8;
                (f("class"), f("type"), mpls.unwrap_or_default())
            })
            .collect();
        quoted.extend(icmpext(ext["version"].as_u64().unwrap() as u8, &objs));
    }
    let m = icmp(typ, code, rest, &quoted);
    assert_eq!(
        m.len() - 8,
        stored_size,
        "this packet gives the stored size: {e}"
    );
    (m, ttl, 0)
}

/// Compare as RIPE's JSON: the same keys in the same order, the same values.
fn assert_same(path: &str, ours: &Value, theirs: &Value) {
    match (ours, theirs) {
        (Value::Object(a), Value::Object(b)) => {
            let (ka, kb): (Vec<_>, Vec<_>) = (a.keys().collect(), b.keys().collect());
            assert_eq!(ka, kb, "{path}: keys");
            for (k, v) in a {
                assert_same(&format!("{path}.{k}"), v, &b[k]);
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{path}: {ours} vs {theirs}");
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                assert_same(&format!("{path}[{i}]"), x, y);
            }
        }
        (Value::Number(a), Value::Number(b)) => assert_eq!(a.as_f64(), b.as_f64(), "{path}"),
        _ => assert_eq!(ours, theirs, "{path}"),
    }
}

/// Replay a stored IPv6 result of the reference (see the module doc) for
/// the measurement `msm`, run with `opts` (its CRONLINE's).
fn replay(msm: u64, stored: &str, opts: TracerouteOptions) {
    let theirs: Value = serde_json::from_str::<Value>(stored).unwrap()[0].clone();
    let dst: Ipv6Addr = theirs["dst_addr"].as_str().unwrap().parse().unwrap();
    let src: Ipv6Addr = theirs["src_addr"].as_str().unwrap().parse().unwrap();
    let paris = theirs.get("paris_id").map(|p| p.as_u64().unwrap() as u16);
    assert_eq!(paris.is_some(), opts.paris_mod != 0);
    let id = TraceId {
        v6: true,
        protocol: opts.protocol,
        pid: PID,
        index: 7,
        target: dst.into(),
        paris,
    };

    let mut trace = Trace::new(true, &opts, paris);
    let wait = ms(opts.timeout_ms as f64);
    let mut t = Duration::from_secs(100_000);
    let mut sent: HashMap<u16, Duration> = HashMap::new();
    let mut in_flight: Option<Probe> = None;
    for hop in theirs["result"].as_array().unwrap() {
        let n = hop["hop"].as_u64().unwrap();
        for e in hop["result"].as_array().unwrap() {
            let p = match in_flight {
                Some(p) => p,
                None => {
                    let p = trace
                        .next_probe(t)
                        .unwrap_or_else(|| panic!("{msm}: the reference probed hop {n} again"));
                    assert_eq!(p.hop as u64, n, "{msm}");
                    trace.sent();
                    sent.insert(p.seq, t);
                    in_flight = Some(p);
                    p
                }
            };
            if e.get("x").is_some() {
                t += wait;
                trace.timeout();
                in_flight = None;
                continue;
            }
            let late = e.get("late").map(|l| l.as_u64().unwrap() as u16);
            let seq = p.seq - late.unwrap_or(0);
            let tv = sent[&seq];
            let at = tv + ms(e["rtt"].as_f64().unwrap());
            let (buf, hoplimit, tclass) = reply6(&id, src, opts.size, e, seq, tv);
            let from: IpAddr = e["from"].as_str().unwrap().parse().unwrap();
            let a = wire::parse_reply(&id, false, &buf, from, hoplimit, tclass, trace.seq())
                .unwrap_or_else(|| panic!("{msm}: hop {n}'s {e} is this trace's"));
            if late.is_some() {
                // It came while the next probe waited.
                assert!(at > sent[&p.seq] && at < sent[&p.seq] + wait, "{msm}: {e}");
                assert_eq!(
                    trace.reply(&a, at),
                    None,
                    "{msm}: a late reply ends no wait"
                );
            } else {
                t = trace.reply(&a, at).expect("the probe in flight's answer");
                trace.timeout();
                in_flight = None;
            }
        }
    }
    assert_eq!(
        trace.next_probe(t),
        None,
        "{msm}: the reference's trace ended here"
    );

    // dst_name and ttr are the target lookup's (their own tests); here
    // they only take their place in the line.
    let line = Traceroute {
        config: TracerouteConfig {
            target: dst.into(),
            opts,
            backend: TracerouteBackend::Native,
            instance: Default::default(),
        },
        probe_id: ProbeId(1),
        msm_id: MeasurementId(msm),
        dst_name: DstName {
            name: theirs["dst_name"].as_str().unwrap().to_string(),
            ttr: theirs.get("ttr").map(|ttr| ttr.as_f64().unwrap()),
        },
    };
    let body = line.result_body(
        theirs["timestamp"].as_i64().unwrap(),
        theirs["endtime"].as_i64().unwrap(),
        &line.dst_name.fields(),
        Some(src.into()),
        paris,
        trace.hops(),
    );
    let mut ours: Value = serde_json::from_str(&format!("{{ {} }}", body))
        .unwrap_or_else(|e| panic!("{msm}: not JSON ({e}): {body}"));
    let o = ours.as_object_mut().unwrap();
    // What RIPE's backend renames and fills in.
    assert_eq!(o.shift_remove("id"), Some(Value::from(msm.to_string())));
    assert_eq!(o.shift_remove("lts"), Some(Value::from(0)));
    assert_eq!(o.shift_remove("time").as_ref(), theirs.get("timestamp"));
    let mut theirs = theirs.clone();
    let th = theirs.as_object_mut().unwrap();
    for k in [
        "lts",
        "msm_id",
        "prb_id",
        "timestamp",
        "msm_name",
        "from",
        "type",
        "stored_timestamp",
        "group_id",
        "destination_ip_responded",
    ] {
        th.shift_remove(k);
    }
    assert_same(&msm.to_string(), &ours, &theirs);
}

/// `evtraceroute -6 -U -c 3 -w 4000` (6001-6016)
fn udp_cron() -> TracerouteOptions {
    TracerouteOptions {
        timeout_ms: 4000,
        ..Default::default()
    }
}

/// `evtraceroute -6 -I -c 3 -w 4000` (6052, 6152)
fn icmp_cron() -> TracerouteOptions {
    TracerouteOptions {
        protocol: P::ICMP,
        ..udp_cron()
    }
}

/// `evtraceroute -6 -I -a <paris_mod> -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48
/// -p 80 -D 0 -H 0` (the scenario's ONEOFFs and 25603xxx lines)
fn oneoff_cron(paris_mod: u8) -> TracerouteOptions {
    TracerouteOptions {
        paris_mod,
        size: 48,
        ..icmp_cron()
    }
}

/// UDP to k-root: two routers, then the target's port unreachables, which
/// carry no "err" (ready_callback6's ICMP6_DST_UNREACH_NOPORT, 3893).
#[test]
fn ref_6001_udp_ends_at_the_targets_port_unreachable() {
    replay(6001, include_str!("testdata/ref-6001.json"), udp_cron());
}

/// A silent probe in hop 7; the target answers "administratively
/// prohibited" ("err":"A", code 1), which ends the trace; "itos":72, the
/// traffic class the routers found on the probe.
#[test]
fn ref_6011_udp_admin_prohibited_and_itos() {
    replay(6011, include_str!("testdata/ref-6011.json"), udp_cron());
}

#[test]
fn ref_6012_udp_a_silent_hop_then_err_a() {
    replay(6012, include_str!("testdata/ref-6012.json"), udp_cron());
}

/// ICMP with the default -a 16 (paris_id 12 in this run although the
/// line has no -a), two silent hops, then the target's echo replies.
#[test]
fn ref_6152_icmp_silent_hops_and_echo_replies() {
    replay(6152, include_str!("testdata/ref-6152.json"), icmp_cron());
}

/// `-a 1`: paris_id 0 on every run. Hop 10 starts with hop 9's first
/// probe's answer, 3 probes late, its rtt from the v6info it quotes
/// (15704 ms), then three silent probes.
#[test]
fn ref_25603439_a_late_reply_and_its_rtt() {
    replay(
        25603439,
        include_str!("testdata/ref-25603439.json"),
        oneoff_cron(1),
    );
}

/// The target's echo replies with "itos" (the reply's own traffic class,
/// 32; ready_callback6 4093-4098).
#[test]
fn ref_176906963_echo_replies_with_their_traffic_class() {
    replay(
        176906963,
        include_str!("testdata/ref-176906963.json"),
        oneoff_cron(16),
    );
}

/// MPLS labels after 128 bytes of quoted probe without an RFC 4884 length
/// ("rfc4884":0, size 144 and 140), "ittl":2 where the router saw two hops
/// left, a router with "ttl":247.
#[test]
fn ref_177401232_mpls_extensions_and_ittl() {
    replay(
        177401232,
        include_str!("testdata/ref-177401232.json"),
        oneoff_cron(16),
    );
}

/// The same with an RFC 4884 length ("rfc4884":1).
#[test]
fn ref_177421605_rfc4884_extensions() {
    replay(
        177421605,
        include_str!("testdata/ref-177421605.json"),
        oneoff_cron(16),
    );
}

/// The stored paris_ids are the runs' (Starla's were a constant 0, or 80
/// from -p): with the default `-a 16` — 6001's and 6152's lines have no
/// -a — the 8th run has 8 (ref-6001) and the 12th 12 (ref-6152), the 16th
/// 0; with `-a 1` every run has 0 (ref-25603439).
#[test]
fn the_stored_paris_ids_are_the_runs() {
    let instance = super::TracerouteInstance::default();
    let ids: Vec<Option<u16>> = (0..17)
        .map(|_| icmp_cron().paris_id(instance.start().unwrap().underway()))
        .collect();
    assert_eq!(ids[0], Some(1));
    assert_eq!(ids[7], Some(8));
    assert_eq!(ids[11], Some(12));
    assert_eq!(ids[15], Some(0));
    assert_eq!(ids[16], Some(1));
    assert!((1..=40).all(|run| oneoff_cron(1).paris_id(run) == Some(0)));
}

/// A packet as one of a run's sockets gets it.
struct Pkt {
    buf: Vec<u8>,
    from: IpAddr,
    hoplimit: i32,
    tclass: i32,
    /// on a TCP trace's raw TCP socket (else every run's ICMP socket)
    tcp: bool,
    /// after the round's probes went
    after: Duration,
}

impl Pkt {
    fn icmp6(buf: Vec<u8>, from: Ipv6Addr, hoplimit: i32, after_ms: f64) -> Self {
        Self {
            buf,
            from: from.into(),
            hoplimit,
            tclass: 0,
            tcp: false,
            after: ms(after_ms),
        }
    }

    fn icmp4(buf: Vec<u8>, after_ms: f64) -> Self {
        let from = Ipv4Addr::new(buf[12], buf[13], buf[14], buf[15]);
        Self {
            buf,
            from: from.into(),
            hoplimit: -42,
            tclass: -42,
            tcp: false,
            after: ms(after_ms),
        }
    }

    fn tcp4(buf: Vec<u8>, after_ms: f64) -> Self {
        Self {
            tcp: true,
            ..Self::icmp4(buf, after_ms)
        }
    }
}

/// One trace in progress, and how its own path answers its probes.
struct Run {
    id: TraceId,
    src: IpAddr,
    opts: TracerouteOptions,
    trace: Trace,
    /// The answer to a probe: (hop, seq, send time) -> packet
    path: fn(&Run, &Probe, Duration) -> Pkt,
}

impl Run {
    fn new(
        id: TraceId,
        src: IpAddr,
        opts: TracerouteOptions,
        path: fn(&Run, &Probe, Duration) -> Pkt,
    ) -> Self {
        let trace = Trace::new(id.v6, &opts, id.paris);
        Self {
            id,
            src,
            opts,
            trace,
            path,
        }
    }

    fn src6(&self) -> Ipv6Addr {
        match self.src {
            IpAddr::V6(a) => a,
            _ => unreachable!(),
        }
    }

    fn src4(&self) -> Ipv4Addr {
        match self.src {
            IpAddr::V4(a) => a,
            _ => unreachable!(),
        }
    }

    fn dst4(&self) -> Ipv4Addr {
        match self.id.target {
            IpAddr::V4(a) => a,
            _ => unreachable!(),
        }
    }
}

/// Hand `pkt` to every run that would hear it: the ICMP socket of every
/// run of its family, the TCP socket of a TCP run.
fn deliver(runs: &mut [Run], pkt: &Pkt, at: Duration) {
    for r in runs.iter_mut() {
        if r.id.v6 != pkt.from.is_ipv6() || (pkt.tcp && r.id.protocol != P::TCP) {
            continue;
        }
        let seq = r.trace.seq();
        if let Some(a) = wire::parse_reply(
            &r.id,
            pkt.tcp,
            &pkt.buf,
            pkt.from,
            pkt.hoplimit,
            pkt.tclass,
            seq,
        ) {
            r.trace.reply(&a, at);
        }
    }
}

/// Run the traces side by side, each probe answered by its own path, with
/// `noise(runs, round)` — other measurements' and processes' ICMP — mixed
/// in; return each trace's result.
fn side_by_side(mut runs: Vec<Run>, noise: fn(&[Run], u16) -> Vec<Pkt>) -> Vec<String> {
    let mut t = Duration::from_secs(5000);
    for round in 0.. {
        let mut pkts = vec![];
        for r in runs.iter_mut() {
            if let Some(p) = r.trace.next_probe(t) {
                r.trace.sent();
                pkts.push((r.path)(r, &p, t));
            }
        }
        if pkts.is_empty() {
            break;
        }
        pkts.extend(noise(&runs, round));
        // In the order they arrive; the noise is interleaved by its times.
        pkts.sort_by_key(|p| p.after);
        for p in &pkts {
            deliver(&mut runs, p, t + p.after);
        }
        for r in runs.iter_mut() {
            r.trace.timeout();
        }
        t += Duration::from_secs(1);
    }
    runs.iter()
        .map(|r| super::engine::render_hops(r.trace.hops()))
        .collect()
}

const ME6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x33);
const K_ROOT: Ipv6Addr = Ipv6Addr::new(0x2001, 0x7fd, 0, 0, 0, 0, 0, 1);
const TOPOLOGY6: Ipv6Addr = Ipv6Addr::new(0x2804, 0x5bf8, 0, 0, 0, 0, 0, 1);
const A_ROOT: Ipv6Addr = Ipv6Addr::new(0x2001, 0x500, 1, 0, 0, 0, 0, 0x53);
const ROUTER_A: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0xa, 0, 0, 0, 0, 1);
const ROUTER_B: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0xb, 0, 0, 0, 0, 1);

/// Hop 1 a router's time exceeded, hop 2 the target: for UDP its port
/// unreachable, for ICMP its echo reply.
fn path6(r: &Run, p: &Probe, tv: Duration) -> Pkt {
    let IpAddr::V6(dst) = r.id.target else {
        unreachable!()
    };
    let (nxt, probe) = probe6(&r.id, r.src6(), r.opts.size, p.seq, tv);
    let router = if r.id.protocol == P::UDP {
        ROUTER_A
    } else {
        ROUTER_B
    };
    match (p.hop, r.id.protocol) {
        (1, _) => {
            let quoted = ip6(0, nxt, 1, r.src6(), dst, &probe);
            Pkt::icmp6(icmp(3, 0, [0; 4], &quoted), router, 64, 0.5)
        }
        (_, P::UDP) => {
            let quoted = ip6(0, nxt, 1, r.src6(), dst, &probe);
            Pkt::icmp6(icmp(1, 4, [0; 4], &quoted), dst, 59, 3.7)
        }
        _ => {
            let mut reply = probe;
            reply[0] = 129;
            Pkt::icmp6(reply, dst, 54, 170.8)
        }
    }
}

/// What else a probe's ICMPv6 socket hears while two traces run.
fn noise6(runs: &[Run], round: u16) -> Vec<Pkt> {
    let udp_run = &runs[0];
    let seq = udp_run.trace.seq();
    let tv = Duration::from_secs(5000);
    let mut v = vec![];
    // Starla's ping to a.root: a random id, zeros after the header.
    let mut ping = vec![129, 0, 0, 0, 0x5a, 0x17, 0, round as u8];
    ping.extend([0u8; 56]);
    v.push(Pkt::icmp6(ping, A_ROOT, 57, 0.1));
    // eperd's ping: a timespec, an index and a cookie after the header.
    let mut evping = vec![129, 0, 0, 0, 0x10, 0x92, 0, seq as u8];
    evping.extend(5000u64.to_ne_bytes());
    evping.extend(123_456_789u64.to_ne_bytes());
    evping.extend(1u32.to_be_bytes());
    evping.extend([0xa5u8; 16]);
    v.push(Pkt::icmp6(evping, A_ROOT, 57, 0.2));
    // A DNS query to a.root that drew a port unreachable.
    let query = udp_dns(&[0xab, 0xcd]);
    let quoted = ip6(0, 17, 60, ME6, A_ROOT, &udp(41234, 53, 0x1234, &query));
    v.push(Pkt::icmp6(icmp(1, 4, [0; 4], &quoted), A_ROOT, 57, 0.3));
    // Another process's traceroute with the same index and sequence
    // number: its v6info has another pid.
    let mut other = udp_run.id.clone();
    other.pid = PID + 1;
    let (nxt, probe) = probe6(&other, ME6, 40, seq, tv);
    let quoted = ip6(0, nxt, 1, ME6, K_ROOT, &probe);
    v.push(Pkt::icmp6(
        icmp(3, 0, [0; 4], &quoted),
        Ipv6Addr::new(0x2001, 0xdb8, 0xdead, 0, 0, 0, 0, 1),
        64,
        0.4,
    ));
    // The UDP trace's own probe, but a sequence number it hasn't sent yet.
    let (nxt, probe) = probe6(&udp_run.id, ME6, 40, seq + 5, tv);
    let quoted = ip6(0, nxt, 1, ME6, K_ROOT, &probe);
    v.push(Pkt::icmp6(
        icmp(3, 0, [0; 4], &quoted),
        Ipv6Addr::new(0x2001, 0xdb8, 0xf, 0, 0, 0, 0, 1),
        64,
        0.45,
    ));
    // Neighbour and router advertisements.
    v.push(Pkt::icmp6(
        icmp(136, 0, [0x60, 0, 0, 0], &[0; 16]),
        "fe80::1".parse().unwrap(),
        255,
        0.6,
    ));
    v.push(Pkt::icmp6(
        icmp(134, 0, [64, 0, 0x07, 0x08], &[0; 8]),
        "fe80::1".parse().unwrap(),
        255,
        0.7,
    ));
    v
}

fn udp_dns(id: &[u8]) -> Vec<u8> {
    let mut q = id.to_vec();
    q.extend([1, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    q.extend(b"\x07example\x03com\x00\x00\x1c\x00\x01");
    q.extend([0; 12]);
    q
}

/// N3 on IPv6, as in production: a UDP trace to k-root (index 1, Paris 8)
/// and an ICMP trace to topology6 (index 2, Paris 12) at once, while pings
/// to a.root answer, a DNS query draws a port unreachable, another
/// process traces with the same index, and routers advertise. Each trace's
/// hops hold its own path's answers and nothing else — not a.root's
/// (starla-6152.json's hop 1), not the other trace's.
#[test]
fn concurrent_ipv6_traces_and_pings_do_not_cross_talk() {
    let udp_id = TraceId {
        v6: true,
        protocol: P::UDP,
        pid: PID,
        index: 1,
        target: K_ROOT.into(),
        paris: Some(8),
    };
    let icmp_id = TraceId {
        protocol: P::ICMP,
        index: 2,
        target: TOPOLOGY6.into(),
        paris: Some(12),
        ..udp_id.clone()
    };
    let runs = vec![
        Run::new(udp_id, ME6.into(), udp_cron(), path6),
        Run::new(icmp_id, ME6.into(), icmp_cron(), path6),
    ];
    let results = side_by_side(runs, noise6);
    let three = |e: &str| [e, e, e].join(", ");
    assert_eq!(
        results[0],
        format!(
            "[ {{ \"hop\":1, \"result\": [ {} ] }}, {{ \"hop\":2, \"result\": [ {} ] }} ]",
            three("{ \"from\":\"2001:db8:a::1\", \"ttl\":64, \"rtt\":0.500, \"size\":88 }"),
            three("{ \"from\":\"2001:7fd::1\", \"ttl\":59, \"rtt\":3.700, \"size\":88 }")
        )
    );
    assert_eq!(
        results[1],
        format!(
            "[ {{ \"hop\":1, \"result\": [ {} ] }}, {{ \"hop\":2, \"result\": [ {} ] }} ]",
            three("{ \"from\":\"2001:db8:b::1\", \"ttl\":64, \"rtt\":0.500, \"size\":88 }"),
            three("{ \"from\":\"2804:5bf8::1\", \"ttl\":54, \"rtt\":170.800, \"size\":40 }")
        )
    );
}

const ME4: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 33);
const QUAD9: Ipv4Addr = Ipv4Addr::new(9, 9, 9, 9);

/// IPv4: hop 1 a router's time exceeded quoting the probe (the whole of
/// it, as Linux routers do), hop 2 the target: ICMP its echo reply, UDP its
/// port unreachable, TCP its SYN-ACK on the TCP socket.
fn path4(r: &Run, p: &Probe, _tv: Duration) -> Pkt {
    let (me, dst) = (r.src4(), r.dst4());
    let index = r.id.index;
    let sport = SRC_BASE_PORT + index;
    let (proto, l4) = match r.id.protocol {
        P::ICMP => (1, wire::icmp4_echo(index, p.seq, r.opts.size, r.id.paris)),
        P::UDP => {
            let dport = BASE_PORT + r.id.paris.unwrap();
            let data = wire::udp4_payload(me, dst, sport, dport, r.opts.size, p.seq);
            let sum = wire::udp4_cksum(me, dst, sport, dport, &data);
            (17, udp(sport, dport, sum, &data))
        }
        P::TCP => (
            6,
            wire::tcp_syn(me.into(), dst.into(), sport, 80, index, p.seq, p.size),
        ),
    };
    if p.hop == 1 {
        let router = Ipv4Addr::new(10, 0, 0, index as u8);
        let quoted = ip4(proto, 1, me, dst, &l4);
        return Pkt::icmp4(ip4(1, 255, router, me, &icmp(11, 0, [0; 4], &quoted)), 0.3);
    }
    match r.id.protocol {
        P::ICMP => {
            let mut reply = l4;
            reply[0] = 0;
            Pkt::icmp4(ip4(1, 57, dst, me, &reply), 12.0)
        }
        P::UDP => {
            let quoted = ip4(proto, 1, me, dst, &l4);
            Pkt::icmp4(ip4(1, 50, dst, me, &icmp(3, 3, [0; 4], &quoted)), 20.0)
        }
        P::TCP => {
            let ack = (index as u32) << 16 | (p.seq as u32 + 1);
            let mut seg = vec![0, 80];
            seg.extend(sport.to_be_bytes());
            seg.extend(0x1234_5678u32.to_be_bytes());
            seg.extend(ack.to_be_bytes());
            seg.extend([0x50, 0x12, 0xff, 0xff, 0, 0, 0, 0]);
            Pkt::tcp4(ip4(6, 60, dst, me, &seg), 5.0)
        }
    }
}

/// What else a probe's ICMP socket hears: the worst case for IPv4 is
/// Starla's ping drawing the ICMP trace's index as its echo id.
fn noise4(runs: &[Run], _round: u16) -> Vec<Pkt> {
    let icmp_run = &runs[0];
    let (index, seq) = (icmp_run.id.index, icmp_run.trace.seq());
    let mut v = vec![];
    // Starla's ping to 9.9.9.9 with that id and this sequence number: a
    // random id, zeros after the header.
    let mut ping = vec![0, 0, 0, 0];
    ping.extend(index.to_be_bytes());
    ping.extend(seq.to_be_bytes());
    ping.extend([0u8; 56]);
    v.push(Pkt::icmp4(ip4(1, 58, QUAD9, ME4, &ping), 0.1));
    // The same ping's request quoted by a router that can't reach it.
    let mut request = ping.clone();
    request[0] = 8;
    let quoted = ip4(1, 3, ME4, QUAD9, &request);
    v.push(Pkt::icmp4(
        ip4(
            1,
            250,
            Ipv4Addr::new(10, 9, 9, 9),
            ME4,
            &icmp(3, 1, [0; 4], &quoted),
        ),
        0.2,
    ));
    // eperd instance 1's ICMP trace with the same index (id 0x1000 | 3).
    let mut other = wire::icmp4_echo(index, seq, 40, Some(5));
    other[4] |= 0x10;
    let quoted = ip4(1, 1, ME4, Ipv4Addr::new(198, 51, 100, 7), &other);
    v.push(Pkt::icmp4(
        ip4(
            1,
            255,
            Ipv4Addr::new(10, 8, 8, 8),
            ME4,
            &icmp(11, 0, [0; 4], &quoted),
        ),
        0.25,
    ));
    // A DNS query to 9.9.9.9 that drew a port unreachable.
    let quoted = ip4(
        17,
        60,
        ME4,
        QUAD9,
        &udp(41234, 53, 0x9999, &udp_dns(&[0xab, 0xcd])),
    );
    v.push(Pkt::icmp4(
        ip4(1, 58, QUAD9, ME4, &icmp(3, 3, [0; 4], &quoted)),
        0.35,
    ));
    // A reset for another index's port on the TCP socket.
    let mut seg = vec![0, 80];
    seg.extend((SRC_BASE_PORT + 6).to_be_bytes());
    seg.extend([0; 4]);
    seg.extend((6u32 << 16 | 1).to_be_bytes());
    seg.extend([0x50, 0x14, 0, 0, 0, 0, 0, 0]);
    v.push(Pkt::tcp4(
        ip4(6, 60, Ipv4Addr::new(192, 0, 2, 80), ME4, &seg),
        0.4,
    ));
    v
}

/// N3 on IPv4: ICMP (index 3), UDP (index 4) and TCP (index 5) traces at
/// once, while Starla's ping draws the ICMP trace's index as its echo id
/// and the same sequence number, a router quotes that ping's request,
/// another eperd instance traces with index 3, a DNS query draws a port
/// unreachable and another index's reset arrives. Without the id string
/// check (`wire::carries_id_string`) the ICMP trace's hops would read
/// 9.9.9.9's echo replies as its answers.
#[test]
fn concurrent_ipv4_traces_and_a_colliding_ping_do_not_cross_talk() {
    let id = |protocol, index, target: Ipv4Addr, paris| TraceId {
        v6: false,
        protocol,
        pid: PID,
        index,
        target: target.into(),
        paris,
    };
    let opts = |protocol| TracerouteOptions {
        protocol,
        ..Default::default()
    };
    let runs = vec![
        Run::new(
            id(P::ICMP, 3, Ipv4Addr::new(198, 51, 100, 7), Some(5)),
            ME4.into(),
            opts(P::ICMP),
            path4,
        ),
        Run::new(
            id(P::UDP, 4, Ipv4Addr::new(203, 0, 113, 9), Some(9)),
            ME4.into(),
            opts(P::UDP),
            path4,
        ),
        Run::new(
            id(P::TCP, 5, Ipv4Addr::new(192, 0, 2, 80), None),
            ME4.into(),
            opts(P::TCP),
            path4,
        ),
    ];
    let results = side_by_side(runs, noise4);
    let three = |e: &str| [e, e, e].join(", ");
    let two_hops = |a: &str, b: &str| {
        format!(
            "[ {{ \"hop\":1, \"result\": [ {} ] }}, {{ \"hop\":2, \"result\": [ {} ] }} ]",
            three(a),
            three(b)
        )
    };
    assert_eq!(
        results[0],
        two_hops(
            "{ \"from\":\"10.0.0.3\", \"ttl\":255, \"size\":68, \"rtt\":0.300 }",
            "{ \"from\":\"198.51.100.7\", \"ttl\":57, \"size\":40, \"rtt\":12.000 }"
        )
    );
    assert_eq!(
        results[1],
        two_hops(
            "{ \"from\":\"10.0.0.4\", \"ttl\":255, \"size\":68, \"rtt\":0.300 }",
            "{ \"from\":\"203.0.113.9\", \"ttl\":50, \"size\":68, \"rtt\":20.000 }"
        )
    );
    assert_eq!(
        results[2],
        two_hops(
            "{ \"from\":\"10.0.0.5\", \"ttl\":255, \"size\":80, \"rtt\":0.300 }",
            "{ \"from\":\"192.0.2.80\", \"ttl\":60, \"size\":0, \"flags\":\"SA\", \"rtt\":5.000 }"
        )
    );
}

/// The id string check on its own: what it lets through and what not.
#[test]
fn an_ipv4_echo_is_ours_only_with_the_probes_data() {
    let id = TraceId {
        v6: false,
        protocol: P::ICMP,
        pid: PID,
        index: 3,
        target: Ipv4Addr::new(198, 51, 100, 7).into(),
        paris: Some(5),
    };
    let reply = |data: &[u8]| {
        let mut m = vec![0, 0, 0, 0, 0, 3, 0, 1];
        m.extend(data);
        ip4(1, 57, Ipv4Addr::new(198, 51, 100, 7), ME4, &m)
    };
    let ours = |b: &[u8]| wire::parse_icmp4(&id, b, Ipv4Addr::new(198, 51, 100, 7), 1).is_some();
    // Our probe's data, whole, cut short, or with -S 2 (no id string).
    let probe = wire::icmp4_echo(3, 1, 40, Some(5));
    assert!(ours(&reply(&probe[8..])));
    assert!(ours(&reply(&probe[8..20])));
    assert!(ours(&reply(&probe[8..10])));
    assert!(ours(&reply(&wire::icmp4_echo(3, 1, 2, Some(5))[8..])));
    // A ping's zeros, or anything else after the two Paris bytes.
    assert!(!ours(&reply(&[0; 56])));
    assert!(!ours(&reply(
        b"\x12\x34http://atlas.ripe.net Atlas says Bye"
    )));
}
