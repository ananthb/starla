//! evtraceroute's run, without the sockets: which hop and sequence number
//! the next probe gets, when the trace ends, and what each reply and
//! timeout writes into the result (probe-busybox/eperd/traceroute.c, tag
//! 5130: send_pkt, noreply_callback and the ready_* handlers).
//!
//! The reference builds its result as one string while it runs; this
//! keeps the same pieces per hop (each reply's text between "{ " and " }")
//! and joins them the same way at the end.

use super::wire::{Answer, Branch, Outcome};
use starla_common::{TracerouteOptions, TracerouteProtocol};
use std::fmt::Write;
use std::time::Duration;

/// One hop of the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    pub hop: u8,
    /// Each reply, timeout or error, rendered as the reference writes it
    /// between `{ ` and ` }`.
    pub entries: Vec<String>,
    /// A failure before the first probe (create_socket), which the
    /// reference writes in place of the hop's "result".
    pub error: Option<String>,
}

impl Hop {
    pub fn new(hop: u8) -> Self {
        Self {
            hop,
            entries: Vec::new(),
            error: None,
        }
    }
}

/// The result array: `[ { "hop":1, "result": [ { … }, { … } ] }, … ]`.
pub fn render_hops(hops: &[Hop]) -> String {
    let hops: Vec<String> = hops
        .iter()
        .map(|h| match &h.error {
            Some(e) => format!("{{ \"hop\":{}, \"error\":\"{}\" }}", h.hop, e),
            None => {
                let entries: Vec<String> =
                    h.entries.iter().map(|e| format!("{{ {} }}", e)).collect();
                format!(
                    "{{ \"hop\":{}, \"result\": [ {} ] }}",
                    h.hop,
                    entries.join(", ")
                )
            }
        })
        .collect();
    format!("[ {} ]", hops.join(", "))
}

/// One reply as the reference writes it between `{ ` and ` }`: each
/// handler's fields in its own order (ready_callback4,
/// traceroute.c:1897-2030, 2123-2252, 2359-2493, 2639-2678;
/// ready_callback6, 3726-3900 and 4047-4098; ready_tcp4/6, 2906-2950 and
/// 3181-3226). `rtt` in ms; `tos` is the `-t` option, which makes the
/// TOS/traffic-class fields appear even when 0.
pub fn entry(a: &Answer, late: Option<i32>, dup: bool, rtt: f64, tos: u32) -> String {
    let mut e = String::new();
    if let Some(late) = late {
        let _ = write!(e, "\"late\":{}", late);
    } else if dup {
        // ready_callback4's TCP and UDP branch puts a space first.
        if a.branch == Branch::V4ErrL4 {
            e.push(' ');
        }
        e.push_str("\"dup\":true");
    }
    let _ = write!(
        e,
        "{}\"from\":\"{}\"",
        if late.is_some() || dup { ", " } else { "" },
        a.from
    );
    match a.branch {
        Branch::V4ErrL4 | Branch::V4ErrIcmp => {
            let _ = write!(e, ", \"ttl\":{}, \"size\":{}", a.ttl, a.size);
            if late.is_none() {
                let _ = write!(e, ", \"rtt\":{:.3}", rtt);
            }
            if a.inner_ttl != 1 {
                let _ = write!(e, ", \"ittl\":{}", a.inner_ttl);
            }
            if a.inner_tos != 0 || tos != 0 {
                let _ = write!(e, ", \"itos\":{}", a.inner_tos);
            }
            if let Some(edst) = a.edst {
                let _ = write!(e, ", \"edst\":\"{}\"", edst);
            }
            match a.outcome {
                Outcome::NeedFrag(mtu) => {
                    let _ = write!(e, ", \"mtu\":{}", mtu);
                }
                Outcome::Unreach(code) => match code {
                    0 => e.push_str(", \"err\":\"N\""),
                    1 => e.push_str(", \"err\":\"H\""),
                    2 => e.push_str(", \"err\":\"P\""),
                    // Port unreachable: the destination, no error (but "p"
                    // when it quotes an ICMP probe).
                    3 if a.branch == Branch::V4ErrIcmp => e.push_str(", \"err\":\"p\""),
                    3 => {}
                    13 => e.push_str(", \"err\":\"A\""),
                    c => {
                        let _ = write!(e, ", \"err\":{}", c);
                    }
                },
                _ => {}
            }
            if let Some(ext) = &a.icmpext {
                e.push_str(ext);
            }
        }
        Branch::V6Err => {
            let _ = write!(
                e,
                ", \"ttl\":{}, \"rtt\":{:.3}, \"size\":{}",
                a.ttl, rtt, a.size
            );
            if a.inner_ttl != 1 {
                let _ = write!(e, ", \"ittl\":{}", a.inner_ttl);
            }
            if a.inner_tos != 0 || tos != 0 {
                let _ = write!(e, ", \"itos\":{}", a.inner_tos);
            }
            if a.hbh_opt_size != 0 {
                let _ = write!(e, ", \"hbhoptsize\":{}", a.hbh_opt_size);
            }
            if a.dst_opt_size != 0 {
                let _ = write!(e, ", \"dstoptsize\":{}", a.dst_opt_size);
            }
            match a.outcome {
                Outcome::TooBig(mtu) => {
                    let _ = write!(e, ", \"mtu\":{}", mtu);
                }
                Outcome::Unreach(code) => match code {
                    0 => e.push_str(", \"err\":\"N\""),
                    1 => e.push_str(", \"err\":\"A\""),
                    2 => e.push_str(", \"err\":\"h\""),
                    3 => e.push_str(", \"err\":\"H\""),
                    4 => {}
                    c => {
                        let _ = write!(e, ", \"err\":{}", c);
                    }
                },
                _ => {}
            }
            if let Some(ext) = &a.icmpext {
                e.push_str(ext);
            }
        }
        Branch::V4Echo => {
            let _ = write!(e, ", \"ttl\":{}, \"size\":{}", a.ttl, a.size);
            if a.tos != 0 || tos != 0 {
                let _ = write!(e, ", \"tos\":{}", a.tos);
            }
            if late.is_none() {
                let _ = write!(e, ", \"rtt\":{:.3}", rtt);
            }
        }
        Branch::V6Echo => {
            let _ = write!(
                e,
                ", \"ttl\":{}, \"rtt\":{:.3}, \"size\":{}",
                a.ttl, rtt, a.size
            );
            if a.tos != 0 || tos != 0 {
                let _ = write!(e, ", \"itos\":{}", a.tos);
            }
        }
        Branch::V4Tcp | Branch::V6Tcp => {
            let _ = write!(e, ", \"ttl\":{}, \"size\":{}", a.ttl, a.size);
            if a.tos != 0 || tos != 0 {
                let _ = write!(e, ", \"tos\":{}", a.tos);
            }
            let _ = write!(e, ", \"flags\":\"{}\"", a.flags);
            if let Some(opts) = &a.hdropts {
                e.push_str(opts);
            }
            if late.is_none() {
                let _ = write!(e, ", \"rtt\":{:.3}", rtt);
            }
        }
    }
    e
}

/// The probe to send next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    /// hop limit / TTL
    pub hop: u8,
    pub seq: u16,
    /// data bytes after the protocol header (curpacksize)
    pub size: u16,
}

/// A trace in progress: trtstate's run-time fields.
#[derive(Debug)]
pub struct Trace {
    v6: bool,
    protocol: TracerouteProtocol,
    paris: bool,
    count: u8,
    max_hops: u8,
    gap_limit: u8,
    tos: u32,
    dup_timeout: Duration,
    hops: Vec<Hop>,
    hop: u8,
    sent: u8,
    seq: u16,
    curpacksize: u16,
    last_response_hop: u8,
    done: bool,
    not_done: bool,
    lastditch: bool,
    /// The probe in flight has had its answer (we wait for duplicates).
    gotresp: bool,
    finished: bool,
    /// When the probe in flight went (gettime_mono)
    xmit: Duration,
}

impl Trace {
    /// traceroute_start2 (traceroute.c:4468-4560).
    pub fn new(v6: bool, opts: &TracerouteOptions, paris: Option<u16>) -> Self {
        let first_hop = opts.first_hop;
        Self {
            v6,
            protocol: opts.protocol,
            paris: paris.is_some(),
            count: opts.count,
            max_hops: opts.max_hops,
            gap_limit: opts.gap_limit,
            tos: opts.tos,
            dup_timeout: Duration::from_millis(opts.dup_timeout_ms as u64),
            hops: vec![Hop::new(first_hop)],
            hop: first_hop,
            sent: 0,
            seq: 0,
            curpacksize: opts.size,
            last_response_hop: first_hop.saturating_sub(1),
            done: false,
            not_done: false,
            lastditch: false,
            gotresp: false,
            finished: false,
            xmit: Duration::ZERO,
        }
    }

    pub fn hops(&self) -> &[Hop] {
        &self.hops
    }

    pub fn into_hops(self) -> Vec<Hop> {
        self.hops
    }

    pub fn seq(&self) -> u16 {
        self.seq
    }

    fn entries(&mut self) -> &mut Vec<String> {
        &mut self.hops.last_mut().expect("a trace has a hop").entries
    }

    /// The top of send_pkt (traceroute.c:529-580): after `count` probes,
    /// end the trace (hop limit, or an answer from the destination and
    /// nothing saying otherwise) or go to the next hop; after `gap_limit`
    /// silent hops, one last-ditch probe at hop 255 and then the end.
    /// None: the trace is over.
    pub fn next_probe(&mut self, now: Duration) -> Option<Probe> {
        self.gotresp = false;
        if self.finished {
            return None;
        }
        if self.sent >= self.count {
            if self.hop >= self.max_hops || (self.done && !self.not_done) {
                self.finished = true;
                return None;
            }
            self.hop = self.hop.wrapping_add(1);
            self.sent = 0;
            self.done = false;
            self.not_done = false;
            if self.hop as i32 - self.last_response_hop as i32 > self.gap_limit as i32 {
                if self.lastditch {
                    self.finished = true;
                    return None;
                }
                self.lastditch = true;
                self.hop = 255;
            }
            self.hops.push(Hop::new(self.hop));
        }
        self.seq = self.seq.wrapping_add(1);
        // An IPv4 Paris UDP probe carries its sequence number in the
        // checksum, which can't be 0 (traceroute.c:1462-1471).
        if !self.v6 && self.paris && self.protocol == TracerouteProtocol::UDP && self.seq == 0 {
            self.seq = 1;
        }
        self.xmit = now;
        Some(Probe {
            hop: self.hop,
            seq: self.seq,
            size: self.curpacksize,
        })
    }

    /// The probe went (or failed in a way the reference shrugs off).
    pub fn sent(&mut self) {
        self.sent = self.sent.saturating_add(1);
    }

    /// The probe couldn't be sent: the reference writes the error and
    /// reports (e.g. traceroute.c:1040-1052).
    pub fn send_failed(&mut self, error: &str) {
        self.entries().push(format!("\"error\":\"{}\"", error));
        self.finished = true;
    }

    /// The trace couldn't start (create_socket): `{ "hop":N, "error":… }`.
    pub fn start_failed(&mut self, error: &str) {
        if let Some(h) = self.hops.last_mut() {
            h.error = Some(error.to_string());
        }
        self.finished = true;
    }

    /// noreply_callback (traceroute.c:4166-4201): the wait for the probe in
    /// flight (or for its duplicates) is over.
    pub fn timeout(&mut self) {
        if !self.gotresp {
            self.entries().push("\"x\":\"*\"".to_string());
        }
    }

    /// A reply to one of this trace's probes arrived at `now`. Returns the
    /// new end of the wait when it answers the probe in flight (then the
    /// reference waits `-z` ms for duplicates before the next probe), None
    /// when the wait goes on (a late reply, a duplicate, or one from the
    /// future, which is ignored).
    pub fn reply(&mut self, a: &Answer, now: Duration) -> Option<Duration> {
        let cur = self.seq as u32;
        let tcp = matches!(a.branch, Branch::V4Tcp | Branch::V6Tcp);
        // A TCP answer acknowledges seq+1; ready_tcp4/6 only call it late
        // when more than 0x2000 behind (traceroute.c:2891-2914, 3166-3189).
        let stale = if tcp {
            a.seq.wrapping_sub(cur) > 0x2000
        } else {
            a.seq != cur
        };
        if stale && a.seq > cur {
            return None;
        }
        let late = stale.then(|| cur.wrapping_sub(a.seq) as i32);
        let dup = late.is_none() && self.gotresp;

        let since = |t: Duration| (now.as_secs_f64() - t.as_secs_f64()) * 1000.0;
        // A late IPv6 reply's rtt is from the send time it quotes; a TCP
        // probe quotes none, and the reference's lint value shows through
        // (traceroute.c:3278, 3777-3787). IPv4 writes no rtt when late.
        let rtt = match (a.branch, late, a.sent_tv) {
            (Branch::V6Err | Branch::V6Echo, Some(_), Some(tv)) => since(tv),
            (Branch::V6Err, Some(_), None) => -42.0,
            _ => since(self.xmit),
        };
        let e = entry(a, late, dup, rtt, self.tos);
        self.entries().push(e);

        // What the reply says about the trace.
        let fresh = late.is_none() && !dup;
        match (a.branch, a.outcome) {
            // ready_callback4's TCP/UDP branch counts duplicates too.
            (Branch::V4ErrL4, Outcome::TimeExceeded) => {
                if late.is_none() {
                    self.not_done = true;
                }
            }
            (_, Outcome::TimeExceeded) => {
                if fresh {
                    self.not_done = true;
                }
            }
            (_, Outcome::NeedFrag(mtu)) => {
                if late.is_none() {
                    self.shrink(mtu, 20 + a.inner_l4_len);
                    self.done = true;
                    self.not_done = true;
                }
            }
            (_, Outcome::TooBig(mtu)) => {
                if late.is_none() {
                    // "This is IPv6, no need to go below 1280."
                    self.shrink(mtu.max(1200), 40 + a.inner_l4_len);
                    self.not_done = true;
                }
            }
            (_, Outcome::Unreach(_)) => {
                if late.is_none() {
                    self.done = true;
                }
            }
            (Branch::V6Echo, _) => {
                if fresh {
                    self.done = true;
                }
            }
            // An IPv4 echo reply or a TCP answer ends the trace whatever
            // it is.
            _ => self.done = true,
        }
        if fresh
            && matches!(
                a.branch,
                Branch::V4ErrL4 | Branch::V4ErrIcmp | Branch::V6Err | Branch::V6Echo
            )
        {
            self.last_response_hop = self.hop;
        }

        if fresh {
            self.gotresp = true;
            Some(now + self.dup_timeout)
        } else {
            None
        }
    }

    /// A smaller next-hop MTU makes the next probes smaller.
    fn shrink(&mut self, mtu: u32, headers: u32) {
        if mtu >= headers {
            let room = mtu - headers;
            if room < self.curpacksize as u32 {
                self.curpacksize = room as u16;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv6Addr};

    const DST: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0x7769, 0x6e00, 0, 0, 0, 0x20);

    fn ms(v: f64) -> Duration {
        Duration::from_secs_f64(1000.0 + v / 1000.0)
    }

    fn port_unreachable(seq: u32) -> Answer {
        let mut a = Answer::new(Branch::V6Err, Outcome::Unreach(4), seq, IpAddr::V6(DST));
        a.ttl = 64;
        a.size = 88;
        a
    }

    fn udp6() -> TracerouteOptions {
        TracerouteOptions {
            timeout_ms: 4000,
            ..Default::default()
        }
    }

    /// msm 6016 (`evtraceroute -6 -U -c 3 -w 4000`) from the reference in
    /// the 2026-10-13 twin run: the target's port unreachables end the
    /// trace at hop 1.
    #[test]
    fn port_unreachables_from_the_target_end_a_udp6_trace_at_hop_1() {
        let mut t = Trace::new(true, &udp6(), Some(1));
        for (i, rtt) in [0.155, 0.119, 0.229].into_iter().enumerate() {
            let sent = i as f64 * 20.0;
            let p = t.next_probe(ms(sent)).unwrap();
            assert_eq!(
                p,
                Probe {
                    hop: 1,
                    seq: i as u16 + 1,
                    size: 40
                }
            );
            t.sent();
            let until = t.reply(&port_unreachable(p.seq as u32), ms(sent + rtt));
            // The next probe after -z (10 ms) of waiting for duplicates.
            assert_eq!(until, Some(ms(sent + rtt) + Duration::from_millis(10)));
            t.timeout();
        }
        assert_eq!(t.next_probe(ms(100.0)), None);
        assert_eq!(
            render_hops(t.hops()),
            "[ { \"hop\":1, \"result\": [ \
             { \"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \"rtt\":0.155, \"size\":88 }, \
             { \"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \"rtt\":0.119, \"size\":88 }, \
             { \"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \"rtt\":0.229, \"size\":88 } ] } ]"
        );
    }

    /// Without answers the reference gives up after `-g` (5) silent hops,
    /// with one last-ditch hop 255: 18 probes, not Starla's 96 (msm 6001
    /// ran 481 s).
    #[test]
    fn silence_ends_after_the_gap_limit_with_a_last_ditch_hop() {
        let mut t = Trace::new(true, &udp6(), Some(1));
        let mut probes = vec![];
        while let Some(p) = t.next_probe(ms(0.0)) {
            probes.push(p.hop);
            t.sent();
            t.timeout();
        }
        assert_eq!(
            probes,
            [1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 5, 255, 255, 255]
        );
        let hops: Vec<u8> = t.hops().iter().map(|h| h.hop).collect();
        assert_eq!(hops, [1, 2, 3, 4, 5, 255]);
        assert!(render_hops(t.hops()).starts_with(
            "[ { \"hop\":1, \"result\": [ { \"x\":\"*\" }, { \"x\":\"*\" }, { \"x\":\"*\" } ] }, { \"hop\":2"
        ));
    }

    /// Routers answer: hops go on; an answer resets the gap count; the hop
    /// limit ends it.
    #[test]
    fn time_exceeded_goes_on_to_the_hop_limit() {
        let opts = TracerouteOptions {
            max_hops: 3,
            count: 1,
            ..Default::default()
        };
        let mut t = Trace::new(true, &opts, Some(1));
        while let Some(p) = t.next_probe(ms(0.0)) {
            t.sent();
            let mut a = port_unreachable(p.seq as u32);
            a.outcome = Outcome::TimeExceeded;
            a.inner_ttl = 1;
            t.reply(&a, ms(1.0));
            t.timeout();
        }
        assert_eq!(t.hops().len(), 3);
    }

    #[test]
    fn late_and_duplicate_replies() {
        let mut t = Trace::new(true, &udp6(), Some(1));
        let p1 = t.next_probe(ms(0.0)).unwrap();
        t.sent();
        t.timeout();
        let p2 = t.next_probe(ms(4000.0)).unwrap();
        t.sent();
        // p1's answer arrives during p2's wait: late by 1, rtt from the
        // v6info it quotes.
        let mut late = port_unreachable(p1.seq as u32);
        late.sent_tv = Some(ms(0.0));
        assert_eq!(t.reply(&late, ms(4001.5)), None);
        // p2's own answer, then a duplicate of it.
        t.reply(&port_unreachable(p2.seq as u32), ms(4002.0))
            .unwrap();
        assert_eq!(t.reply(&port_unreachable(p2.seq as u32), ms(4003.0)), None);
        t.timeout();
        let e = &t.hops()[0].entries;
        assert_eq!(e[0], "\"x\":\"*\"");
        assert_eq!(
            e[1],
            "\"late\":1, \"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \"rtt\":4001.500, \"size\":88"
        );
        assert_eq!(
            e[3],
            "\"dup\":true, \"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \"rtt\":3.000, \"size\":88"
        );
        assert_eq!(e.len(), 4, "the duplicate's wait adds no x:*");
        // An answer from the future is no answer.
        assert_eq!(t.reply(&port_unreachable(9), ms(4004.0)), None);
        assert_eq!(t.hops()[0].entries.len(), 4);
    }

    #[test]
    fn ipv6_error_fields_in_the_references_order() {
        let mut t = Trace::new(true, &TracerouteOptions { tos: 0, ..udp6() }, Some(1));
        let p = t.next_probe(ms(0.0)).unwrap();
        t.sent();
        let mut a = port_unreachable(p.seq as u32);
        a.outcome = Outcome::Unreach(1);
        a.inner_ttl = 3;
        a.inner_tos = 4;
        a.hbh_opt_size = 8;
        a.icmpext = Some(", \"icmpext\": { }".into());
        t.reply(&a, ms(1.0));
        assert_eq!(
            t.hops()[0].entries[0],
            "\"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \"rtt\":1.000, \"size\":88, \
             \"ittl\":3, \"itos\":4, \"hbhoptsize\":8, \"err\":\"A\", \"icmpext\": { }"
        );
    }

    #[test]
    fn ipv4_reply_fields_in_the_references_order() {
        let opts = TracerouteOptions {
            protocol: TracerouteProtocol::ICMP,
            ..Default::default()
        };
        let mut t = Trace::new(false, &opts, Some(1));
        let p = t.next_probe(ms(0.0)).unwrap();
        t.sent();
        let from = "192.0.2.9".parse().unwrap();
        let mut a = Answer::new(Branch::V4ErrIcmp, Outcome::Unreach(3), p.seq as u32, from);
        a.ttl = 250;
        a.size = 68;
        a.inner_ttl = 2;
        a.edst = Some("192.0.2.200".parse().unwrap());
        t.reply(&a, ms(2.0));
        assert_eq!(
            t.hops()[0].entries[0],
            "\"from\":\"192.0.2.9\", \"ttl\":250, \"size\":68, \"rtt\":2.000, \"ittl\":2, \
             \"edst\":\"192.0.2.200\", \"err\":\"p\""
        );
        t.timeout();
        let p = t.next_probe(ms(10.0)).unwrap();
        t.sent();
        let from = "192.0.2.200".parse().unwrap();
        let mut echo = Answer::new(Branch::V4Echo, Outcome::EchoReply, p.seq as u32, from);
        echo.ttl = 55;
        echo.size = 40;
        t.reply(&echo, ms(13.25));
        assert_eq!(
            t.hops()[0].entries[1],
            "\"from\":\"192.0.2.200\", \"ttl\":55, \"size\":40, \"rtt\":3.250"
        );
    }

    /// msm 6152 (`evtraceroute -6 -I -c 3 -w 4000`) from the reference:
    /// echo replies end the trace at hop 1, written from, ttl, rtt, size.
    #[test]
    fn icmp6_echo_replies_end_the_trace_at_hop_1() {
        let opts = TracerouteOptions {
            protocol: TracerouteProtocol::ICMP,
            timeout_ms: 4000,
            ..Default::default()
        };
        let mut t = Trace::new(true, &opts, Some(1));
        for (i, rtt) in [0.145, 0.2, 0.31].into_iter().enumerate() {
            let sent = i as f64 * 20.0;
            let p = t.next_probe(ms(sent)).unwrap();
            t.sent();
            let mut a = Answer::new(
                Branch::V6Echo,
                Outcome::EchoReply,
                p.seq as u32,
                IpAddr::V6(DST),
            );
            a.ttl = 64;
            a.size = 40;
            t.reply(&a, ms(sent + rtt)).unwrap();
            t.timeout();
        }
        assert_eq!(t.next_probe(ms(100.0)), None);
        assert_eq!(
            t.hops()[0].entries[0],
            "\"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \"rtt\":0.145, \"size\":40"
        );
        assert_eq!(t.hops().len(), 1);
    }

    #[test]
    fn errors_are_entries_or_the_hop() {
        let mut t = Trace::new(true, &udp6(), Some(1));
        t.start_failed("socket failed: Operation not permitted");
        assert_eq!(t.next_probe(ms(0.0)), None);
        assert_eq!(
            render_hops(t.hops()),
            "[ { \"hop\":1, \"error\":\"socket failed: Operation not permitted\" } ]"
        );

        let mut t = Trace::new(true, &udp6(), Some(1));
        t.next_probe(ms(0.0)).unwrap();
        t.sent();
        t.timeout();
        t.next_probe(ms(0.0)).unwrap();
        t.send_failed("sendto failed: Network is unreachable");
        assert_eq!(t.next_probe(ms(0.0)), None);
        assert_eq!(
            render_hops(t.hops()),
            "[ { \"hop\":1, \"result\": [ { \"x\":\"*\" }, \
             { \"error\":\"sendto failed: Network is unreachable\" } ] } ]"
        );
    }

    #[test]
    fn packet_too_big_shrinks_the_next_probes() {
        let opts = TracerouteOptions {
            size: 1500,
            count: 2,
            ..Default::default()
        };
        let mut t = Trace::new(true, &opts, Some(1));
        let p = t.next_probe(ms(0.0)).unwrap();
        t.sent();
        let mut a = port_unreachable(p.seq as u32);
        a.outcome = Outcome::TooBig(1280);
        a.inner_l4_len = 8;
        t.reply(&a, ms(1.0));
        t.timeout();
        assert_eq!(t.next_probe(ms(2.0)).unwrap().size, 1280 - 48);
        assert!(t.hops()[0].entries[0].ends_with(", \"size\":88, \"mtu\":1280"));
    }
}
