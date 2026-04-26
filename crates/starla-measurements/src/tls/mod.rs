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
    fn measurement_type(&self) -> starla_common::MeasurementType {
        starla_common::MeasurementType::Tls
    }

    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        let results = cert::execute_tls_check(&self.config).await?;

        let src_addr = get_source_addr_for_dest(self.config.target).ok();
        let af = if self.config.target.is_ipv4() { 4 } else { 6 };
        let src_str = src_addr.map(|ip| ip.to_string()).unwrap_or_default();
        let timestamp = starla_common::Timestamp::now().0;

        // Build cert array as PEM strings matching C probe
        let cert_strs: Vec<String> = results
            .pem_certs
            .iter()
            .map(|pem| {
                // Escape newlines in PEM for JSON
                let escaped = pem.replace('\n', "\\n");
                format!("\"{}\"", escaped)
            })
            .collect();

        let body = format!(
            "\"id\":\"{}\", \"fw\":{}, \"mver\": \"2.6.4\", \"lts\":0, \"time\":{}, \
             \"dst_name\":\"{}\", \"dst_port\":\"{}\", \"ttr\":{:.6}, \"method\":\"TLS\", \
             \"ver\":\"{}\", \"dst_addr\":\"{}\", \"af\": {}, \"src_addr\":\"{}\", \"ttc\":{:.6}, \
             \"rt\":{:.6}, \"server_cipher\":\"{}\", \"cert\":[ {} ]",
            self.msm_id.0,
            starla_common::FIRMWARE_VERSION,
            timestamp,
            self.config.hostname,
            self.config.port,
            results.rt / 1000.0, // ttr in seconds
            results.ver,
            self.config.target,
            af,
            src_str,
            results.tcp_connect_time / 1000.0, // ttc in seconds
            results.rt / 1000.0,               // rt in seconds
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
