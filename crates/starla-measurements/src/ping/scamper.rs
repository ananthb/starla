//! Scamper-backed ping execution.
//!
//! Talks to a running scamper daemon over a unix-domain socket using the
//! rscamper bindings, runs a ping, and reshapes the response into the same
//! `PingReplyOrTimeout` sequence the native ICMP path produces so the
//! downstream result formatting stays identical.
//!
//! The socket path is read from the `STARLA_SCAMPER_SOCKET` environment
//! variable, falling back to `/var/run/scamper/scamperd.sock`. Build with
//! `--features scamper` and ensure libscamperfile / libscamperctrl are
//! available at link time.

use super::icmp::PingReplyOrTimeout;
use super::PingConfig;
use anyhow::Context;
use std::time::Duration;

const DEFAULT_SCAMPER_SOCKET: &str = "/var/run/scamper/scamperd.sock";

pub async fn execute_ping(config: &PingConfig) -> anyhow::Result<Vec<PingReplyOrTimeout>> {
    // rscamper's control loop is synchronous and blocks waiting for the
    // daemon to push responses back, so run it on a blocking thread to
    // keep the tokio runtime healthy.
    let config = config.clone();
    let total_timeout =
        Duration::from_millis(config.timeout_ms.saturating_mul(config.count as u64).max(1_000));

    tokio::task::spawn_blocking(move || run_ping_blocking(&config, total_timeout))
        .await
        .context("scamper ping task panicked")?
}

fn run_ping_blocking(
    config: &PingConfig,
    total_timeout: Duration,
) -> anyhow::Result<Vec<PingReplyOrTimeout>> {
    use rscamper::ctrl::ScamperCtrl;

    let socket = std::env::var("STARLA_SCAMPER_SOCKET")
        .unwrap_or_else(|_| DEFAULT_SCAMPER_SOCKET.to_string());

    let mut ctrl = ScamperCtrl::new(false, None).context("ScamperCtrl::new failed")?;
    let inst = ctrl
        .add_unix(&socket)
        .with_context(|| format!("failed to connect to scamper at {}", socket))?;

    let target = config.target.to_string();
    ctrl.do_ping(
        &inst,
        &target,
        Some(config.count as u16),
        Some(config.size),
        Some(config.ttl),
        Some(Duration::from_millis(config.timeout_ms)),
    )
    .context("scheduling scamper ping failed")?;

    let mut replies: Vec<PingReplyOrTimeout> = Vec::with_capacity(config.count as usize);

    for item in ctrl.responses(Some(total_timeout)) {
        let rscamper::ctrl::ResponseItem { obj, .. } = item;
        if let rscamper::ctrl::ScamperObject::Ping(ping) = obj {
            let sent = ping.sent();
            for i in 0..sent {
                let rtt_ms = ping
                    .probe(i)
                    .and_then(|p| p.reply(0))
                    .and_then(|r| r.rtt())
                    .map(|d| d.as_secs_f64() * 1000.0);
                replies.push(match rtt_ms {
                    Some(rtt) => PingReplyOrTimeout::Reply { rtt },
                    None => PingReplyOrTimeout::Timeout,
                });
            }
            break;
        }
    }

    if replies.is_empty() {
        anyhow::bail!("scamper produced no ping response before timeout");
    }
    Ok(replies)
}
