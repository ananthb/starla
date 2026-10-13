//! Scamper-backed ping execution.
//!
//! Talks to a running scamper daemon over a unix-domain socket using the
//! rscamper bindings, runs a ping, and plays its probes and replies
//! through the same [`super::Session`] the native ICMP path drives, so the
//! downstream result formatting stays identical.
//!
//! The socket path is read from the `STARLA_SCAMPER_SOCKET` environment
//! variable, falling back to `/var/run/scamper/scamperd.sock`. Build with
//! `--features scamper` and ensure libscamperfile / libscamperctrl are
//! available at link time.

use super::{data_len, replay, PingConfig, PingOutcome, Probe, ProbeReply, ICMP_HEADER_LEN};
use anyhow::{anyhow, Context};
use std::net::IpAddr;
use std::time::Duration;

const DEFAULT_SCAMPER_SOCKET: &str = "/var/run/scamper/scamperd.sock";

/// scamper's `ping -s` for the reference's `-s size`. scamper's probe size
/// counts the IP and ICMP headers ("The probe size includes the length of
/// the IP and ICMP headers", scamper(1); its defaults are 84 bytes for
/// IPv4 and 56 for IPv6), where the reference's `-s` is the data after the
/// ICMP header ([`data_len`]). Passing `-s` through made every scamper
/// ping 28 (IPv4) or 48 (IPv6) bytes shorter than the reference's.
pub(crate) fn probe_size(target: IpAddr, size: u16) -> u16 {
    (ip_header_len(target) + ICMP_HEADER_LEN + data_len(size)) as u16
}

fn ip_header_len(target: IpAddr) -> usize {
    if target.is_ipv4() {
        20
    } else {
        40
    }
}

/// A reply's data after the ICMP header, from the size scamper records
/// for it, which counts the IP header as its probe size does; None when
/// scamper could not tell.
fn reply_data_len(target: IpAddr, size: u16) -> Option<usize> {
    usize::from(size).checked_sub(ip_header_len(target) + ICMP_HEADER_LEN)
}

pub async fn execute_ping(config: &PingConfig) -> anyhow::Result<PingOutcome> {
    // rscamper's control loop is synchronous and blocks waiting for the
    // daemon to push responses back, so run it on a blocking thread to
    // keep the tokio runtime healthy.
    let config = config.clone();
    // count intervals, and the last one's wait, and some slack
    let total_timeout = Duration::from_millis(
        config
            .interval_ms
            .saturating_mul(u64::from(config.count) + 1)
            .saturating_add(5_000),
    );

    tokio::task::spawn_blocking(move || run_ping_blocking(&config, total_timeout))
        .await
        .context("scamper ping task panicked")?
}

/// scamper sends the requests one interval apart (-i) and waits one
/// interval after the last (-W), as ping.c does; its probes and their
/// echo replies are then played through ping.c's sequence numbers and
/// timeouts ([`replay`]), so a lost request after the first reply leaves
/// no entry, a second reply to a request is a duplicate, and a reply later
/// than its interval counts as ping.c would count it.
fn run_ping_blocking(config: &PingConfig, total_timeout: Duration) -> anyhow::Result<PingOutcome> {
    use rscamper::ctrl::{ResponseItem, ScamperCtrl};
    use rscamper::file::ScamperObject;

    let socket = std::env::var("STARLA_SCAMPER_SOCKET")
        .unwrap_or_else(|_| DEFAULT_SCAMPER_SOCKET.to_string());

    let mut ctrl = ScamperCtrl::new(false, None).map_err(|e| anyhow!("ScamperCtrl::new: {}", e))?;
    let inst = ctrl
        .add_unix(&socket)
        .map_err(|e| anyhow!("connect to scamper at {}: {}", socket, e))?;

    let interval = Duration::from_millis(config.interval_ms);
    let target = config.target.to_string();
    ctrl.do_ping(
        &inst,
        &target,
        None,                                         // tcp_ack
        None,                                         // tcp_seq
        Some(config.count as u16),                    // attempts
        None,                                         // icmp_id
        None,                                         // icmp_seq
        None,                                         // icmp_sum
        None,                                         // dport
        None,                                         // sport
        Some(interval),                               // wait_probe
        Some(config.ttl),                             // ttl
        None,                                         // mtu
        None,                                         // stop_count
        None,                                         // method
        None,                                         // payload
        None,                                         // rtr
        None,                                         // recordroute
        Some(probe_size(config.target, config.size)), // size
        None,                                         // src
        Some(interval),                               // wait_timeout
        None,                                         // tos
        None,                                         // userid
    )
    .map_err(|e| anyhow!("scheduling scamper ping: {}", e))?;

    for ResponseItem { obj, .. } in ctrl.responses(Some(total_timeout)) {
        if let ScamperObject::Ping(ping) = obj {
            let local = ping.src().and_then(|a| a.to_ipaddr());
            let mut first_tx = None;
            let mut probes = Vec::new();
            for i in 0..ping.sent() {
                let Some(probe) = ping.probe(i) else {
                    continue;
                };
                let tx = probe.tx();
                let first = *first_tx.get_or_insert(tx);
                let sent = match (tx, first) {
                    (Some(tx), Some(first)) => tx.duration_since(first).unwrap_or_default(),
                    // when it is not known: one interval after the last
                    _ => interval * i as u32,
                };
                let mut replies = Vec::new();
                for j in 0.. {
                    let Some(reply) = probe.reply(j) else {
                        break;
                    };
                    let Some(rtt) = reply.rtt() else {
                        continue;
                    };
                    if !reply.is_icmp_echo_reply() {
                        continue;
                    }
                    replies.push(ProbeReply {
                        rtt,
                        // the reply's TTL / hop limit as scamper recorded
                        // it (0 when it could not tell)
                        ttl: Some(reply.ttl()).filter(|&ttl| ttl != 0),
                        size: reply_data_len(config.target, reply.size())
                            .unwrap_or_else(|| data_len(config.size)),
                    });
                }
                probes.push(Probe {
                    sent,
                    local,
                    replies,
                });
            }
            if probes.is_empty() {
                break;
            }
            let events = replay(config.target.is_ipv6(), interval, &probes);
            return Ok(PingOutcome::Ran { local, events });
        }
    }

    anyhow::bail!("scamper produced no ping response before timeout")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same packets as the native backend and the reference: -s 60 to
    /// an IPv6 target is 40 + 8 + 60 bytes, -s 1460 is 1508 (fragmented
    /// at a 1500 MTU), no -s is 32 data bytes.
    #[test]
    fn scamper_sizes_count_the_headers() {
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        let v4: IpAddr = "192.0.2.1".parse().unwrap();
        assert_eq!(probe_size(v6, 60), 108);
        assert_eq!(probe_size(v6, 1440), 1488);
        assert_eq!(probe_size(v6, 1460), 1508);
        assert_eq!(probe_size(v6, 0), 80);
        assert_eq!(probe_size(v4, 0), 60);
        assert_eq!(probe_size(v4, 1460), 1488);
        assert_eq!(probe_size(v6, u16::MAX), 40 + 8 + 4068);
    }

    /// A reply's size as scamper records it counts the IP header.
    #[test]
    fn reply_sizes_lose_the_headers() {
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        let v4: IpAddr = "192.0.2.1".parse().unwrap();
        assert_eq!(reply_data_len(v6, 80), Some(32));
        assert_eq!(reply_data_len(v6, 1508), Some(1460));
        assert_eq!(reply_data_len(v4, 60), Some(32));
        assert_eq!(reply_data_len(v4, 0), None);
    }
}
