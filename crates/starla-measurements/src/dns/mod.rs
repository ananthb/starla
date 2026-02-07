//! DNS measurement implementation

pub mod resolver;

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
pub enum DnsProtocol {
    UDP,
    TCP,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsConfig {
    pub target: IpAddr, // The DNS server to query
    pub protocol: DnsProtocol,
    pub query_name: String,
    pub query_type: String,  // A, AAAA, TXT, etc.
    pub query_class: String, // IN, CH, etc.
    pub recursion_desired: bool,
    pub edns_buf_size: Option<u16>,
    pub dnssec: bool,
    pub timeout_ms: u64,
}

pub struct Dns {
    pub config: DnsConfig,
    pub probe_id: ProbeId,
    pub msm_id: MeasurementId,
}

#[async_trait]
impl Measurement for Dns {
    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        let results = resolver::execute_dns_query(&self.config).await?;

        let proto = match self.config.protocol {
            DnsProtocol::TCP => "TCP",
            DnsProtocol::UDP => "UDP",
        };

        // Get the source address that would be used for this destination
        let src_addr = get_source_addr_for_dest(self.config.target).ok();

        Ok(MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Dns,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: Timestamp::now(),
            af: if self.config.target.is_ipv4() { 4 } else { 6 },
            dst_addr: self.config.target,
            dst_name: None,
            src_addr,
            proto: Some(proto.to_string()),
            ttl: None,
            size: None,
            data: MeasurementData::Generic(serde_json::to_value(results)?),
        })
    }
}
