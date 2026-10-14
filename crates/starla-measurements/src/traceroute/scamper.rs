//! Scamper-backed traceroute execution (`TracerouteBackend::Scamper`).
//!
//! Mirrors `ping::scamper`: drives a running scamper daemon over a
//! unix-domain socket and turns its trace into the reference's hops. Not
//! what Starla runs by default: TracerouteConfig's backend defaults to
//! Native (scheduler job.rs), and Starla's packaging starts no scamper
//! daemon. The 2026-10-13 twin capture shows the native UDP probes
//! (33434 + 3·(ttl-1) + n, empty payload), not scamper's.
//!
//! The conversion is plain data in, reference text out ([`hops`]), so
//! every platform tests it; only the daemon session is Linux-only.

use super::engine::{self, Hop};
use super::wire::{Answer, Branch, Outcome};
use starla_common::TracerouteProtocol;
use std::net::IpAddr;
use std::time::Duration;

/// What scamper says about one reply (rscamper's ScamperTraceProbe and
/// ScamperTraceReply getters).
#[derive(Debug, Clone)]
pub struct Reply {
    /// the probe's TTL: the hop
    pub probe_ttl: u8,
    /// the probe's attempt at that hop, counted from 1 (scamper's probe
    /// id). libscamperfile keeps it so in memory: its warts reader adds 1
    /// to the byte on disk and its writer takes 1 off
    /// (warts_trace_hop_read_probe_id / _write_probe_id), and
    /// scamper_trace_probe_id_get and the JSON's "probe_id" return the
    /// field as it is.
    pub probe_id: u8,
    pub from: IpAddr,
    pub rtt: Duration,
    pub reply_ttl: u8,
    pub reply_tos: u8,
    /// scamper's reply_size, taken to be the reply's IP packet with its
    /// header (IPv4 ip_len; IPv6 payload length + 40). Not checked against
    /// a recorded trace: no scamper binary was at hand, only
    /// libscamperfile, whose getter returns the field unchanged.
    pub reply_size: u16,
    /// ICMP type and code; None for a TCP reply
    pub icmp: Option<(u8, u8)>,
    /// next-hop MTU of a fragmentation-needed / packet-too-big
    pub icmp_nhmtu: u16,
    pub tcp_flags: u8,
}

/// One reply as the reference would have written it. scamper keeps no
/// quoted TTL or TOS of the probe, so there is never an "ittl"/"itos",
/// and no ICMP extensions. The reference's "size" is what follows the
/// ICMP header (IPv4: nrecv - 28 with the IP header in nrecv; IPv6:
/// nrecv - 8 without it), i.e. reply_size - 28 or - 48 if reply_size is
/// the whole IP packet (see [`Reply::reply_size`]).
fn answer(v6: bool, protocol: TracerouteProtocol, r: &Reply) -> Option<Answer> {
    let size = r.reply_size as i32;
    let (branch, outcome, size) = match (v6, r.icmp) {
        (false, None) => (Branch::V4Tcp, Outcome::Tcp, size - 40),
        (true, None) => (Branch::V6Tcp, Outcome::Tcp, size - 60),
        (false, Some((0, _))) => (Branch::V4Echo, Outcome::EchoReply, size - 28),
        (false, Some((t @ (3 | 11), code))) => {
            let branch = if protocol == TracerouteProtocol::ICMP {
                Branch::V4ErrIcmp
            } else {
                Branch::V4ErrL4
            };
            let outcome = match (t, code) {
                (11, _) => Outcome::TimeExceeded,
                (_, 4) => Outcome::NeedFrag(r.icmp_nhmtu as u32),
                (_, code) => Outcome::Unreach(code),
            };
            (branch, outcome, size - 28)
        }
        (true, Some((129, _))) => (Branch::V6Echo, Outcome::EchoReply, size - 48),
        (true, Some((t @ 1..=3, code))) => {
            let outcome = match t {
                3 => Outcome::TimeExceeded,
                2 => Outcome::TooBig(r.icmp_nhmtu as u32),
                _ => Outcome::Unreach(code),
            };
            (Branch::V6Err, outcome, size - 48)
        }
        _ => return None,
    };
    let mut a = Answer::new(branch, outcome, 0, r.from);
    a.ttl = r.reply_ttl as i32;
    a.tos = r.reply_tos as i32;
    a.size = size;
    a.flags = tcp_flags(r.tcp_flags);
    Some(a)
}

fn tcp_flags(b: u8) -> String {
    [
        (0x01, 'F'),
        (0x02, 'S'),
        (0x04, 'R'),
        (0x08, 'P'),
        (0x10, 'A'),
        (0x20, 'U'),
    ]
    .into_iter()
    .filter(|(bit, _)| b & bit != 0)
    .map(|(_, c)| c)
    .collect()
}

/// The reference's hops from scamper's replies: every hop from
/// `first_hop` to `last_hop`, `attempts` entries each (attempts 1 to
/// `attempts`, or to the last one with a reply), `{ "x":"*" }` for a probe
/// without a reply, a second reply to one probe as a duplicate.
pub fn hops(
    v6: bool,
    protocol: TracerouteProtocol,
    tos: u32,
    first_hop: u8,
    last_hop: u8,
    attempts: u8,
    replies: &[Reply],
) -> Vec<Hop> {
    let mut out = Vec::new();
    for ttl in first_hop..=last_hop.max(first_hop) {
        let mut hop = Hop::new(ttl);
        let at_ttl: Vec<&Reply> = replies.iter().filter(|r| r.probe_ttl == ttl).collect();
        let last_id = at_ttl.iter().map(|r| r.probe_id).max().unwrap_or(0);
        for id in 1..=attempts.max(last_id) {
            let mut answered = false;
            for r in at_ttl.iter().filter(|r| r.probe_id.max(1) == id) {
                let Some(a) = answer(v6, protocol, r) else {
                    continue;
                };
                let rtt = r.rtt.as_secs_f64() * 1000.0;
                hop.entries
                    .push(engine::entry(&a, None, answered, rtt, tos));
                answered = true;
            }
            if !answered {
                hop.entries.push("\"x\":\"*\"".to_string());
            }
        }
        out.push(hop);
    }
    out
}

#[cfg(target_os = "linux")]
pub use daemon::execute_traceroute;

#[cfg(target_os = "linux")]
mod daemon {
    use super::super::wire::{BASE_PORT, SRC_BASE_PORT};
    use super::super::TracerouteConfig;
    use super::*;
    use anyhow::{anyhow, Context};

    const DEFAULT_SCAMPER_SOCKET: &str = "/var/run/scamper/scamperd.sock";

    pub async fn execute_traceroute(
        config: &TracerouteConfig,
        paris: Option<u16>,
        index: u16,
    ) -> anyhow::Result<Vec<Hop>> {
        let config = config.clone();
        let opts = &config.opts;
        // Up to `count` probes per hop, each waiting -w, plus the gap.
        let per_hop = opts.timeout_ms as u64 * opts.count.max(1) as u64;
        let total_timeout =
            Duration::from_millis(per_hop.saturating_mul(opts.max_hops as u64 + 1).max(5_000));

        tokio::task::spawn_blocking(move || run_blocking(&config, paris, index, total_timeout))
            .await
            .context("scamper trace task panicked")?
    }

    fn run_blocking(
        config: &TracerouteConfig,
        paris: Option<u16>,
        index: u16,
        total_timeout: Duration,
    ) -> anyhow::Result<Vec<Hop>> {
        use rscamper::ctrl::ScamperCtrl;
        use rscamper::trace::TraceStopReason;

        let socket = std::env::var("STARLA_SCAMPER_SOCKET")
            .unwrap_or_else(|_| DEFAULT_SCAMPER_SOCKET.to_string());

        let mut ctrl =
            ScamperCtrl::new(false, None).map_err(|e| anyhow!("ScamperCtrl::new: {}", e))?;
        let inst = ctrl
            .add_unix(&socket)
            .map_err(|e| anyhow!("connect to scamper at {}: {}", socket, e))?;

        let opts = &config.opts;
        let v6 = config.target.is_ipv6();
        // The reference's flows: a UDP Paris trace goes to BASE_PORT + id
        // from SRC_BASE_PORT + index; an ICMP Paris trace keeps its
        // checksum at the id (IPv6: id + 1).
        let (method, sport, dport, icmp_sum) = match (opts.protocol, paris) {
            (TracerouteProtocol::UDP, Some(p)) => (
                "udp-paris",
                Some(SRC_BASE_PORT.wrapping_add(index)),
                Some(BASE_PORT.wrapping_add(p)),
                None,
            ),
            (TracerouteProtocol::UDP, None) => ("udp", None, Some(BASE_PORT + 1), None),
            (TracerouteProtocol::ICMP, Some(p)) => {
                ("icmp-paris", None, None, Some(if v6 { p + 1 } else { p }))
            }
            (TracerouteProtocol::ICMP, None) => ("icmp", None, None, None),
            (TracerouteProtocol::TCP, _) => (
                "tcp",
                Some(SRC_BASE_PORT.wrapping_add(index)),
                Some(opts.dest_port),
                None,
            ),
        };
        let target = config.target.to_string();
        let mut trace = |firsthop: u8, hoplimit: u8, timeout: Duration| {
            ctrl.do_trace(
                &inst,
                &target,
                None,                                                    // confidence
                dport,                                                   // dport
                None,                                                    // icmp_id
                icmp_sum,                                                // icmp_sum
                Some(firsthop),                                          // firsthop
                Some(opts.gap_limit),                                    // gaplimit
                None,                                                    // loops
                Some(hoplimit),                                          // hoplimit
                None,                                                    // pmtud
                None,                                                    // squeries
                None,                                                    // ptr_lookup
                None,                                                    // payload
                Some(method),                                            // method
                Some(opts.count),                                        // attempts
                Some(true),                                              // all_attempts
                None,                                                    // rtr
                sport,                                                   // sport
                None,                                                    // src
                (opts.tos != 0).then_some(opts.tos as u8),               // tos
                None,                                                    // userid
                Some(Duration::from_millis(opts.timeout_ms as u64)),     // wait_timeout
                Some(Duration::from_millis(opts.dup_timeout_ms as u64)), // wait_probe
            )
            .map_err(|e| anyhow!("scheduling scamper trace: {}", e))?;
            ctrl.responses(Some(timeout))
                .find_map(|item| match item.obj {
                    rscamper::file::ScamperObject::Trace(t) => Some(t),
                    _ => None,
                })
                .ok_or_else(|| anyhow!("scamper produced no trace response before timeout"))
        };

        let main = trace(opts.first_hop, opts.max_hops, total_timeout)?;
        let main_replies = replies(&main);
        let last = main_replies
            .iter()
            .map(|r| r.probe_ttl)
            .max()
            .unwrap_or(0)
            .max(main.hop_count().min(255) as u8);
        let attempts = main.attempts().max(1);
        let mut out = hops(
            v6,
            opts.protocol,
            opts.tos,
            main.firsthop().max(1),
            last,
            attempts,
            &main_replies,
        );
        // After -g hops without an answer the reference tries hop 255
        // once more before its result (traceroute.c:551-575, "lastditch");
        // scamper just stops. A failed last try leaves the result as it is.
        if main.stop_reason() == TraceStopReason::GapLimit && last < 255 {
            let per_hop = opts.timeout_ms as u64 * opts.count.max(1) as u64;
            let timeout = Duration::from_millis(per_hop.max(1_000) * 2);
            if let Ok(ditch) = trace(255, 255, timeout) {
                out.extend(hops(
                    v6,
                    opts.protocol,
                    opts.tos,
                    255,
                    255,
                    attempts,
                    &replies(&ditch),
                ));
            }
        }
        Ok(out)
    }

    /// Every reply of a scamper trace, with the probe it answers.
    fn replies(trace: &rscamper::trace::ScamperTrace) -> Vec<Reply> {
        let mut out = Vec::new();
        for (probe, reply) in trace.hops() {
            let Some(from) = reply.addr().and_then(|a| a.to_string().parse().ok()) else {
                continue;
            };
            out.push(Reply {
                probe_ttl: probe.ttl(),
                probe_id: probe.id(),
                from,
                rtt: reply.rtt().unwrap_or_default(),
                reply_ttl: reply.ttl(),
                reply_tos: reply.tos(),
                reply_size: reply.size(),
                icmp: reply
                    .is_icmp()
                    .then(|| (reply.icmp_type(), reply.icmp_code())),
                icmp_nhmtu: reply.icmp_nhmtu(),
                tcp_flags: reply.tcp_flags(),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DST: &str = "2001:db8:7769:6e00::20";

    fn unreachable(id: u8, rtt_us: u64) -> Reply {
        Reply {
            probe_ttl: 1,
            probe_id: id,
            from: DST.parse().unwrap(),
            rtt: Duration::from_micros(rtt_us),
            reply_ttl: 64,
            reply_tos: 0,
            // 40-byte IPv6 header + the 96-byte ICMPv6 error
            reply_size: 136,
            icmp: Some((1, 4)),
            icmp_nhmtu: 0,
            tcp_flags: 0,
        }
    }

    /// A scamper trace of the twin's msm 6016 target: the destination's
    /// port unreachables at hop 1 make the reference's single hop. scamper
    /// numbers the attempts 1, 2, 3: no leading { "x":"*" } for an
    /// attempt 0 that never was.
    #[test]
    fn a_port_unreachable_from_the_target_is_a_one_hop_result() {
        let replies = [
            unreachable(1, 155),
            unreachable(2, 119),
            unreachable(3, 229),
        ];
        let h = hops(true, TracerouteProtocol::UDP, 0, 1, 1, 3, &replies);
        assert_eq!(
            engine::render_hops(&h),
            "[ { \"hop\":1, \"result\": [ { \"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \
             \"rtt\":0.155, \"size\":88 }, { \"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \
             \"rtt\":0.119, \"size\":88 }, { \"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \
             \"rtt\":0.229, \"size\":88 } ] } ]"
        );
    }

    /// Unanswered probes are { "x":"*" }, silent hops too; a router's
    /// time exceeded is a plain reply.
    #[test]
    fn unanswered_probes_and_silent_hops_are_stars() {
        let mut router = unreachable(2, 1500);
        router.probe_ttl = 1;
        router.icmp = Some((3, 0));
        router.from = "2001:db8::1".parse().unwrap();
        let mut dst = unreachable(1, 2000);
        dst.probe_ttl = 3;
        let h = hops(true, TracerouteProtocol::UDP, 0, 1, 3, 3, &[router, dst]);
        assert_eq!(
            engine::render_hops(&h),
            "[ { \"hop\":1, \"result\": [ { \"x\":\"*\" }, { \"from\":\"2001:db8::1\", \
             \"ttl\":64, \"rtt\":1.500, \"size\":88 }, { \"x\":\"*\" } ] }, { \"hop\":2, \
             \"result\": [ { \"x\":\"*\" }, { \"x\":\"*\" }, { \"x\":\"*\" } ] }, { \"hop\":3, \
             \"result\": [ { \"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \"rtt\":2.000, \
             \"size\":88 }, { \"x\":\"*\" }, { \"x\":\"*\" } ] } ]"
        );
    }

    #[test]
    fn ipv4_echo_replies_and_errors() {
        let echo = Reply {
            probe_ttl: 1,
            probe_id: 1,
            from: "192.0.2.7".parse().unwrap(),
            rtt: Duration::from_micros(1250),
            reply_ttl: 57,
            reply_tos: 0,
            reply_size: 68,
            icmp: Some((0, 0)),
            icmp_nhmtu: 0,
            tcp_flags: 0,
        };
        let mut admin = echo.clone();
        admin.probe_id = 2;
        admin.icmp = Some((3, 13));
        admin.reply_size = 96;
        let h = hops(false, TracerouteProtocol::ICMP, 0, 1, 1, 2, &[echo, admin]);
        assert_eq!(
            engine::render_hops(&h),
            "[ { \"hop\":1, \"result\": [ { \"from\":\"192.0.2.7\", \"ttl\":57, \"size\":40, \
             \"rtt\":1.250 }, { \"from\":\"192.0.2.7\", \"ttl\":57, \"size\":68, \"rtt\":1.250, \
             \"err\":\"A\" } ] } ]"
        );
    }

    /// The reference's last try after the gap limit is hop 255, numbered
    /// as such; a second reply to one attempt is a duplicate.
    #[test]
    fn the_last_ditch_hop_and_duplicates() {
        let mut first = unreachable(1, 300);
        first.probe_ttl = 255;
        let mut dup = first.clone();
        dup.rtt = Duration::from_micros(400);
        let h = hops(true, TracerouteProtocol::UDP, 0, 255, 255, 2, &[first, dup]);
        assert_eq!(
            engine::render_hops(&h),
            "[ { \"hop\":255, \"result\": [ { \"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \
             \"rtt\":0.300, \"size\":88 }, { \"dup\":true, \"from\":\"2001:db8:7769:6e00::20\", \
             \"ttl\":64, \"rtt\":0.400, \"size\":88 }, { \"x\":\"*\" } ] } ]"
        );
    }
}
