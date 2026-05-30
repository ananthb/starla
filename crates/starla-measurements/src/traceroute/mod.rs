//! Traceroute measurement implementation

pub mod icmp;
#[cfg(feature = "scamper")]
pub mod scamper;
#[cfg(unix)]
pub mod tcp;
#[cfg(unix)]
pub mod udp;

use crate::traits::Measurement;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use starla_common::{
    MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId, Timestamp,
};
use starla_network::get_source_addr_for_dest;
use std::fmt::Write;
use std::net::IpAddr;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TracerouteProtocol {
    ICMP,
    UDP,
    TCP,
}

/// Which implementation runs the traceroute. See [`super::ping::PingBackend`]
/// for the same trade-off applied to ping.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TracerouteBackend {
    #[default]
    Native,
    Scamper,
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
    #[serde(default)]
    pub backend: TracerouteBackend,
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
                            write!(s, "\"rtt\":{:.3}", rtt).unwrap();
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
    fn measurement_type(&self) -> starla_common::MeasurementType {
        starla_common::MeasurementType::Traceroute
    }

    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        let start_time = Timestamp::now().0;

        let results = match self.config.backend {
            #[cfg(feature = "scamper")]
            TracerouteBackend::Scamper => scamper::execute_traceroute(&self.config).await?,
            #[cfg(not(feature = "scamper"))]
            TracerouteBackend::Scamper => anyhow::bail!(
                "scamper backend requested but starla was built without the `scamper` feature"
            ),
            TracerouteBackend::Native => match self.config.protocol {
                TracerouteProtocol::ICMP => icmp::execute_traceroute(&self.config).await?,
                #[cfg(unix)]
                TracerouteProtocol::UDP => udp::execute_traceroute(&self.config).await?,
                #[cfg(unix)]
                TracerouteProtocol::TCP => tcp::execute_traceroute(&self.config).await?,
                #[cfg(not(unix))]
                TracerouteProtocol::UDP | TracerouteProtocol::TCP => anyhow::bail!(
                    "UDP and TCP traceroute are not supported on this platform; use ICMP"
                ),
            },
        };

        let endtime = Timestamp::now().0;

        let proto = match self.config.protocol {
            TracerouteProtocol::ICMP => "ICMP",
            TracerouteProtocol::UDP => "UDP",
            TracerouteProtocol::TCP => "TCP",
        };

        let src_addr = get_source_addr_for_dest(self.config.target).ok();

        // Use FullLine to include endtime and paris_id in the envelope
        let af = if self.config.target.is_ipv4() { 4 } else { 6 };
        let src_str = src_addr.map(|ip| ip.to_string()).unwrap_or_default();
        let dst_str = self.config.target.to_string();
        let hops_str = format_traceroute_hops(&results.result);

        let body = format!(
            "\"id\":\"{}\", \"fw\":{}, \"mver\": \"2.6.4\", \"lts\":0, \"time\":{}, \
             \"endtime\":{}, \"dst_name\":\"{}\", \"dst_addr\":\"{}\", \"src_addr\":\"{}\", \
             \"proto\":\"{}\", \"af\": {}, \"size\":{}, \"paris_id\":{}, \"result\": {}",
            self.msm_id.0,
            starla_common::FIRMWARE_VERSION,
            start_time,
            endtime,
            dst_str,
            dst_str,
            src_str,
            proto,
            af,
            self.config.size,
            self.config.paris,
            hops_str,
        );

        Ok(MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Traceroute,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: Timestamp(start_time),
            af,
            dst_addr: self.config.target,
            dst_name: Some(dst_str),
            src_addr,
            proto: Some(proto.to_string()),
            ttl: None,
            size: Some(self.config.size),
            data: MeasurementData::FullLine(body),
        })
    }
}
