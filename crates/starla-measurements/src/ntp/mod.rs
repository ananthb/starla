//! NTP measurement implementation

pub mod client;

use crate::traits::Measurement;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use starla_common::{
    MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId, Timestamp,
};
use starla_network::get_source_addr_for_dest;
use std::net::IpAddr;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtpConfig {
    pub target: IpAddr,
    pub timeout_ms: u64,
    pub port: u16,
}

pub struct Ntp {
    pub config: NtpConfig,
    pub probe_id: ProbeId,
    pub msm_id: MeasurementId,
}

#[async_trait]
impl Measurement for Ntp {
    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        let results = client::execute_ntp_query(&self.config).await?;

        // Get the source address that would be used for this destination
        let src_addr = get_source_addr_for_dest(self.config.target).ok();

        Ok(MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Ntp,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: Timestamp::now(),
            af: if self.config.target.is_ipv4() { 4 } else { 6 },
            dst_addr: self.config.target,
            dst_name: None,
            src_addr,
            proto: Some("UDP".to_string()),
            ttl: None,
            size: None,
            data: MeasurementData::Generic(serde_json::to_value(results)?),
        })
    }
}
