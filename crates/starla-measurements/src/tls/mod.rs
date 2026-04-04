//! TLS measurement implementation

pub mod cert;

use crate::traits::Measurement;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use starla_common::{MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId};
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

        // TLS has a non-standard envelope — all fields are flat, no nested "result"
        // C probe format: "id":"...", "fw":..., "mver": "...", "lts":..., "time":...,
        //   "dst_name":"...", "dst_port":"443", "method":"TLS", "ver":"1.2",
        //   "dst_addr":"...", "af": 4, "src_addr":"...", "ttc": ..., "rt": ...,
        //   "server_cipher": "0xc030", "cert":[ "PEM...", "PEM..." ]
        let af = if self.config.target.is_ipv4() { 4 } else { 6 };
        let src_str = src_addr.map(|ip| ip.to_string()).unwrap_or_default();
        let timestamp = starla_common::Timestamp::now().0;

        // Build cert array as PEM strings — C probe sends raw PEM
        // We don't have raw PEM from rustls, so send cert info as-is for now
        let cert_strs: Vec<String> = results
            .cert
            .iter()
            .map(|c| format!("\"{}\"", c.subject))
            .collect();

        let body = format!(
            "\"id\":\"{}\", \"fw\":{}, \"mver\": \"2.6.4\", \"lts\":0, \"time\":{}, \
             \"dst_name\":\"{}\", \"dst_port\":\"{}\", \"method\":\"TLS\", \"ver\":\"{}\", \
             \"dst_addr\":\"{}\", \"af\": {}, \"src_addr\":\"{}\", \"ttc\": {:.6}, \"rt\": {:.6}, \
             \"server_cipher\":\"{}\", \"cert\":[ {} ]",
            self.msm_id.0,
            starla_common::FIRMWARE_VERSION,
            timestamp,
            self.config.hostname,
            self.config.port,
            results.ver,
            self.config.target,
            af,
            src_str,
            results.tcp_connect_time,
            results.rt,
            results.cipher,
            cert_strs.join(", "),
        );

        Ok(MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Tls,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: starla_common::Timestamp(timestamp),
            af,
            dst_addr: self.config.target,
            dst_name: Some(self.config.hostname.clone()),
            src_addr,
            proto: Some("TCP".to_string()),
            ttl: None,
            size: None,
            data: MeasurementData::FullLine(body),
        })
    }
}
