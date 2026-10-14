//! Windows: ICMP echo traceroute through the ICMP helper API
//! (IcmpSendEcho2 / Icmp6SendEcho2). The API sends only echo requests and
//! hands back a status, not the reply's bytes, so the reference's fields
//! that need them (an error's size, "ittl", "itos", "icmpext", the
//! unreachable code) are not there; hops, timeouts and the end of the
//! trace follow the same engine as on unix.

use super::engine::{Hop, Trace};
use super::wire::{Answer, Branch, Outcome};
use super::{TracerouteConfig, TracerouteProtocol};
use std::time::{Duration, Instant};

pub async fn execute_traceroute(
    config: &TracerouteConfig,
    paris: Option<u16>,
) -> anyhow::Result<Vec<Hop>> {
    use starla_network::windows_icmp::{self, IcmpStatus};

    let opts = &config.opts;
    if opts.protocol != TracerouteProtocol::ICMP {
        anyhow::bail!("UDP and TCP traceroute are not supported on this platform; use ICMP");
    }
    let v6 = config.target.is_ipv6();
    let (err_branch, echo_branch) = if v6 {
        (Branch::V6Err, Branch::V6Echo)
    } else {
        (Branch::V4ErrIcmp, Branch::V4Echo)
    };
    let mut trace = Trace::new(v6, opts, paris);
    let clock = Instant::now();

    loop {
        let sent_at = clock.elapsed();
        let Some(probe) = trace.next_probe(sent_at) else {
            break;
        };
        let payload = vec![0u8; probe.size as usize];
        let reply =
            windows_icmp::icmp_ping(config.target, probe.hop, opts.timeout_ms, payload).await;
        trace.sent();
        let r = match reply {
            Ok(r) => r,
            Err(e) => {
                trace.send_failed(&format!("sendto failed: {}", e));
                break;
            }
        };
        let outcome = match r.status {
            IcmpStatus::Success => Some(Outcome::EchoReply),
            IcmpStatus::TtlExpired => Some(Outcome::TimeExceeded),
            // The API folds net, host, protocol and port together.
            IcmpStatus::DestUnreachable => Some(Outcome::Unreach(if v6 { 4 } else { 3 })),
            IcmpStatus::TimedOut | IcmpStatus::Other(_) => None,
        };
        if let Some(outcome) = outcome {
            let branch = if outcome == Outcome::EchoReply {
                echo_branch
            } else {
                err_branch
            };
            let mut a = Answer::new(branch, outcome, probe.seq as u32, r.from);
            a.ttl = r.reply_ttl as i32;
            a.size = probe.size as i32;
            let at = sent_at + Duration::from_secs_f64(r.rtt_ms.max(0.0) / 1000.0);
            trace.reply(&a, at);
        }
        trace.timeout();
    }
    Ok(trace.into_hops())
}
