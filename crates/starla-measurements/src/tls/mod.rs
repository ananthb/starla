//! TLS measurement implementation

pub mod cert;

use crate::traits::Measurement;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use starla_common::{
    MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId, Timestamp,
};
use starla_network::get_source_addr_for_dest;
use std::net::IpAddr;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    pub target: IpAddr,
    pub port: u16,
    pub hostname: String, // SNI
    pub timeout_ms: u64,
}

pub struct Tls {
    pub config: TlsConfig,
    pub probe_id: ProbeId,
    pub msm_id: MeasurementId,
}

#[async_trait]
impl Measurement for Tls {
    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        let results = cert::execute_tls_check(&self.config).await?;

        // Get the source address that would be used for this destination
        let src_addr = get_source_addr_for_dest(self.config.target).ok();

        Ok(MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Tls,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: Timestamp::now(),
            af: if self.config.target.is_ipv4() { 4 } else { 6 },
            dst_addr: self.config.target,
            dst_name: Some(self.config.hostname.clone()),
            src_addr,
            proto: Some("TCP".to_string()),
            ttl: None,
            size: None,
            data: MeasurementData::Generic(serde_json::to_value(results)?),
        })
    }
}
