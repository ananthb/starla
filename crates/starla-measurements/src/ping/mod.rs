//! Ping measurement implementation

pub mod icmp;

use crate::traits::Measurement;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use starla_common::{
    MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId, Timestamp,
};
use starla_network::get_source_addr_for_dest;
use std::net::IpAddr;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingConfig {
    pub target: IpAddr,
    pub count: u32,
    pub size: u16,
    pub ttl: u8,
    pub timeout_ms: u64,
}

pub struct Ping {
    pub config: PingConfig,
    pub probe_id: ProbeId,
    pub msm_id: MeasurementId,
}

#[async_trait]
impl Measurement for Ping {
    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        let results = icmp::execute_ping(&self.config).await?;

        // Get the source address that would be used for this destination
        let src_addr = get_source_addr_for_dest(self.config.target).ok();

        Ok(MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Ping,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: Timestamp::now(),
            af: if self.config.target.is_ipv4() { 4 } else { 6 },
            dst_addr: self.config.target,
            dst_name: None, // To be filled with hostname if available
            src_addr,
            proto: Some("ICMP".to_string()),
            ttl: Some(self.config.ttl),
            size: Some(self.config.size),
            data: MeasurementData::Generic(serde_json::to_value(results)?),
        })
    }
}
