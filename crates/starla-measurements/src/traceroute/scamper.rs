//! Scamper-backed traceroute execution.
//!
//! Mirrors `ping::scamper`: drives a running scamper daemon over a
//! unix-domain socket and reshapes its trace results into the same
//! `TracerouteResult` shape the native ICMP/UDP/TCP paths produce.

use super::icmp::{ProbeResult, TracerouteHop, TracerouteResult};
use super::{TracerouteConfig, TracerouteProtocol};
use anyhow::Context;
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
    use rscamper::ctrl::{ResponseItem, ScamperCtrl, ScamperObject};

    let socket = std::env::var("STARLA_SCAMPER_SOCKET")
        .unwrap_or_else(|_| DEFAULT_SCAMPER_SOCKET.to_string());

    let mut ctrl = ScamperCtrl::new(false, None).context("ScamperCtrl::new failed")?;
    let inst = ctrl
        .add_unix(&socket)
        .with_context(|| format!("failed to connect to scamper at {}", socket))?;

    let target = config.target.to_string();
    ctrl.do_trace(
        &inst,
        &target,
        Some(config.first_hop),
        Some(config.max_hops),
        Some(config.size),
        Some(Duration::from_millis(config.timeout_ms)),
    )
    .context("scheduling scamper trace failed")?;

    let af = if config.target.is_ipv4() { 4 } else { 6 };
    let proto = match config.protocol {
        TracerouteProtocol::ICMP => "ICMP",
        TracerouteProtocol::UDP => "UDP",
        TracerouteProtocol::TCP => "TCP",
    };

    let mut hops: Vec<TracerouteHop> = Vec::new();
    for item in ctrl.responses(Some(total_timeout)) {
        let ResponseItem { obj, .. } = item;
        if let ScamperObject::Trace(trace) = obj {
            let hopc = trace.hop_count();
            for i in 0..hopc {
                let hop_no = i.saturating_add(config.first_hop as u16) as u8;
                let mut probes: Vec<ProbeResult> = Vec::new();
                for j in 0..trace.attempts() {
                    let Some(hop) = trace.hop(i, j) else {
                        probes.push(ProbeResult {
                            from: None,
                            rtt: None,
                            ttl: None,
                            size: None,
                            icmptype: None,
                            x: Some("*".to_string()),
                            err: None,
                        });
                        continue;
                    };
                    probes.push(ProbeResult {
                        from: hop.addr().and_then(|a| a.parse().ok()),
                        rtt: hop.rtt().map(|d| d.as_secs_f64() * 1000.0),
                        ttl: hop.reply_ttl(),
                        size: hop.reply_size().map(|s| s as usize),
                        icmptype: hop.icmp_type(),
                        x: None,
                        err: None,
                    });
                }
                hops.push(TracerouteHop {
                    hop: hop_no,
                    result: probes,
                });
            }
            break;
        }
    }

    if hops.is_empty() {
        anyhow::bail!("scamper produced no trace response before timeout");
    }

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
