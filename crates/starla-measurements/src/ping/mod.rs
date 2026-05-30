//! Ping measurement implementation

pub mod icmp;
#[cfg(feature = "scamper")]
pub mod scamper;

use crate::traits::Measurement;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use starla_common::{
    MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId, Timestamp,
};
use starla_network::get_source_addr_for_dest;
use std::net::IpAddr;

/// Which implementation runs the ping.
///
/// `Native` is the in-process ICMP socket implementation and is always
/// available. `Scamper` delegates to a running scamper daemon via the
/// rscamper bindings; selecting it without building the crate with the
/// `scamper` feature is an error.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PingBackend {
    #[default]
    Native,
    Scamper,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingConfig {
    pub target: IpAddr,
    pub count: u32,
    pub size: u16,
    pub ttl: u8,
    pub timeout_ms: u64,
    #[serde(default)]
    pub backend: PingBackend,
}

/// Format ping results as a C-probe-compatible string.
/// Produces: `[ { "rtt":10.500000 }, { "x":"*" } ]`
fn format_ping_results(results: &[icmp::PingReplyOrTimeout]) -> String {
    let entries: Vec<String> = results
        .iter()
        .map(|r| match r {
            icmp::PingReplyOrTimeout::Reply { rtt } => format!("{{ \"rtt\":{:.6} }}", rtt),
            icmp::PingReplyOrTimeout::Timeout => "{ \"x\":\"*\" }".to_string(),
        })
        .collect();
    format!("[ {} ]", entries.join(", "))
}

pub struct Ping {
    pub config: PingConfig,
    pub probe_id: ProbeId,
    pub msm_id: MeasurementId,
}

#[async_trait]
impl Measurement for Ping {
    fn measurement_type(&self) -> starla_common::MeasurementType {
        starla_common::MeasurementType::Ping
    }

    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        let results = match self.config.backend {
            PingBackend::Native => icmp::execute_ping(&self.config).await?,
            #[cfg(feature = "scamper")]
            PingBackend::Scamper => scamper::execute_ping(&self.config).await?,
            #[cfg(not(feature = "scamper"))]
            PingBackend::Scamper => anyhow::bail!(
                "scamper backend requested but starla was built without the `scamper` feature"
            ),
        };

        // Get the source address that would be used for this destination
        let src_addr = get_source_addr_for_dest(self.config.target).ok();

        // ttl is only reported when at least one reply was received (matching C probe)
        let got_reply = results
            .iter()
            .any(|r| matches!(r, icmp::PingReplyOrTimeout::Reply { .. }));

        // Format the result array to match exact C probe output:
        // [ { "rtt":10.500000 }, { "x":"*" } ]
        let result_str = format_ping_results(&results);

        Ok(MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Ping,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: Timestamp::now(),
            af: if self.config.target.is_ipv4() { 4 } else { 6 },
            dst_addr: self.config.target,
            dst_name: None,
            src_addr,
            proto: Some("ICMP".to_string()),
            ttl: if got_reply {
                Some(self.config.ttl)
            } else {
                None
            },
            size: Some(self.config.size),
            data: MeasurementData::PreFormatted(result_str),
        })
    }
}
