//! Scamper-backed traceroute execution.
//!
//! Mirrors `ping::scamper`: drives a running scamper daemon over a
//! unix-domain socket and reshapes its trace results into the same
//! `TracerouteResult` shape the native ICMP/UDP/TCP paths produce.

use super::icmp::{ProbeResult, TracerouteHop, TracerouteResult};
use super::{TracerouteConfig, TracerouteProtocol};
use anyhow::{anyhow, Context};
use std::collections::BTreeMap;
use std::time::Duration;

const DEFAULT_SCAMPER_SOCKET: &str = "/var/run/scamper/scamperd.sock";

pub async fn execute_traceroute(config: &TracerouteConfig) -> anyhow::Result<TracerouteResult> {
    let config = config.clone();
    let total_timeout = Duration::from_millis(
        config
            .timeout_ms
            .saturating_mul(config.max_hops as u64)
            .max(5_000),
    );

    tokio::task::spawn_blocking(move || run_traceroute_blocking(&config, total_timeout))
        .await
        .context("scamper trace task panicked")?
}

fn run_traceroute_blocking(
    config: &TracerouteConfig,
    total_timeout: Duration,
) -> anyhow::Result<TracerouteResult> {
    use rscamper::ctrl::{ResponseItem, ScamperCtrl};
    use rscamper::file::ScamperObject;

    let socket = std::env::var("STARLA_SCAMPER_SOCKET")
        .unwrap_or_else(|_| DEFAULT_SCAMPER_SOCKET.to_string());

    let mut ctrl = ScamperCtrl::new(false, None).map_err(|e| anyhow!("ScamperCtrl::new: {}", e))?;
    let inst = ctrl
        .add_unix(&socket)
        .map_err(|e| anyhow!("connect to scamper at {}: {}", socket, e))?;

    let target = config.target.to_string();
    let method = match config.protocol {
        TracerouteProtocol::ICMP => Some("icmp-paris"),
        TracerouteProtocol::UDP => Some("udp-paris"),
        TracerouteProtocol::TCP => Some("tcp"),
    };
    ctrl.do_trace(
        &inst,
        &target,
        None,                                           // confidence
        None,                                           // dport
        None,                                           // icmp_id
        None,                                           // icmp_sum
        Some(config.first_hop),                         // firsthop
        None,                                           // gaplimit
        None,                                           // loops
        Some(config.max_hops),                          // hoplimit
        None,                                           // pmtud
        None,                                           // squeries
        None,                                           // ptr_lookup
        None,                                           // payload
        method,                                         // method
        None,                                           // attempts
        None,                                           // all_attempts
        None,                                           // rtr
        None,                                           // sport
        None,                                           // src
        None,                                           // tos
        None,                                           // userid
        Some(Duration::from_millis(config.timeout_ms)), // wait_timeout
        None,                                           // wait_probe
    )
    .map_err(|e| anyhow!("scheduling scamper trace: {}", e))?;

    let af = if config.target.is_ipv4() { 4 } else { 6 };
    let proto = match config.protocol {
        TracerouteProtocol::ICMP => "ICMP",
        TracerouteProtocol::UDP => "UDP",
        TracerouteProtocol::TCP => "TCP",
    };

    // Group probe-reply pairs by the probe's ttl (= hop number).
    let mut by_hop: BTreeMap<u8, Vec<ProbeResult>> = BTreeMap::new();

    for ResponseItem { obj, .. } in ctrl.responses(Some(total_timeout)) {
        let ScamperObject::Trace(trace) = obj else {
            continue;
        };
        for (probe, reply) in trace.hops() {
            let hop_no = probe.ttl();
            by_hop.entry(hop_no).or_default().push(ProbeResult {
                from: reply.addr().and_then(|a| a.to_string().parse().ok()),
                rtt: reply.rtt().map(|d| d.as_secs_f64() * 1000.0),
                ttl: Some(reply.ttl()),
                size: Some(reply.size() as usize),
                icmptype: Some(reply.icmp_type()),
                x: None,
                err: None,
            });
        }
        break;
    }

    if by_hop.is_empty() {
        anyhow::bail!("scamper produced no trace response before timeout");
    }

    let hops: Vec<TracerouteHop> = by_hop
        .into_iter()
        .map(|(hop, result)| TracerouteHop { hop, result })
        .collect();

    Ok(TracerouteResult {
        dst_addr: config.target,
        dst_name: config.target.to_string(),
        af,
        proto: proto.to_string(),
        size: config.size,
        paris_id: config.paris,
        result: hops,
    })
}
