//! Traceroute measurement implementation

pub mod icmp;
pub mod tcp;
pub mod udp;

use crate::traits::Measurement;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use starla_common::{
    MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId, Timestamp,
};
use starla_network::get_source_addr_for_dest;
use std::net::IpAddr;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TracerouteProtocol {
    ICMP,
    UDP,
    TCP,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracerouteConfig {
    pub target: IpAddr,
    pub protocol: TracerouteProtocol,
    pub first_hop: u8,
    pub max_hops: u8,
    pub paris: u16, // Paris traceroute ID
    pub size: u16,
    pub timeout_ms: u64,
}

/// Format traceroute hops matching exact C probe output.
/// C probe uses %.3f for RTT (3 decimal places).
fn format_traceroute_hops(hops: &[icmp::TracerouteHop]) -> String {
    let hop_strs: Vec<String> = hops
        .iter()
        .map(|hop| {
            let probe_strs: Vec<String> = hop
                .result
                .iter()
                .map(|p| {
                    if let Some(ref x) = p.x {
                        format!("{{ \"x\":\"{}\" }}", x)
                    } else if let Some(ref err) = p.err {
                        format!("{{ \"error\":\"{}\" }}", err)
                    } else {
                        let mut s = String::from("{ ");
                        if let Some(from) = p.from {
                            s.push_str(&format!("\"from\":\"{}\", ", from));
                        }
                        if let Some(ttl) = p.ttl {
                            s.push_str(&format!("\"ttl\":{}, ", ttl));
                        }
                        if let Some(size) = p.size {
                            s.push_str(&format!("\"size\":{}, ", size));
                        }
                        if let Some(rtt) = p.rtt {
                            s.push_str(&format!("\"rtt\":{:.3}", rtt));
                        }
                        s.push_str(" }");
                        s
                    }
                })
                .collect();
            format!(
                "{{ \"hop\":{}, \"result\": [ {} ] }}",
                hop.hop,
                probe_strs.join(", ")
            )
        })
        .collect();
    format!("[ {} ]", hop_strs.join(", "))
}

pub struct Traceroute {
    pub config: TracerouteConfig,
    pub probe_id: ProbeId,
    pub msm_id: MeasurementId,
}

#[async_trait]
impl Measurement for Traceroute {
    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        let results = match self.config.protocol {
            TracerouteProtocol::ICMP => icmp::execute_traceroute(&self.config).await?,
            TracerouteProtocol::UDP => udp::execute_traceroute(&self.config).await?,
            TracerouteProtocol::TCP => tcp::execute_traceroute(&self.config).await?,
        };

        let proto = match self.config.protocol {
            TracerouteProtocol::ICMP => "ICMP",
            TracerouteProtocol::UDP => "UDP",
            TracerouteProtocol::TCP => "TCP",
        };

        // Get the source address that would be used for this destination
        let src_addr = get_source_addr_for_dest(self.config.target).ok();

        // Format the hops array to match C probe output exactly:
        // [ { "hop":1, "result": [ { "from":"1.2.3.4", "ttl":64, "size":28, "rtt":1.234
        // }, ... ] }, ... ]
        let result_str = format_traceroute_hops(&results.result);

        Ok(MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Traceroute,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: Timestamp::now(),
            af: if self.config.target.is_ipv4() { 4 } else { 6 },
            dst_addr: self.config.target,
            dst_name: None,
            src_addr,
            proto: Some(proto.to_string()),
            ttl: None,
            size: Some(self.config.size),
            data: MeasurementData::PreFormatted(result_str),
        })
    }
}
