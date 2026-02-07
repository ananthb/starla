//! HTTP measurement implementation

pub mod client;

use crate::traits::Measurement;
use async_trait::async_trait;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use starla_common::{
    MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId, Timestamp,
};
use starla_network::get_source_addr_for_dest;
use std::net::ToSocketAddrs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    pub url: String,
    pub method: String, // GET, POST, etc.
    pub headers: Option<std::collections::HashMap<String, String>>,
    pub body: Option<String>,
    pub timeout_ms: u64,
    pub verify_ssl: bool,
    pub version: Option<String>, // "1.1" or "2"
}

pub struct Http {
    pub config: HttpConfig,
    pub probe_id: ProbeId,
    pub msm_id: MeasurementId,
}

#[async_trait]
impl Measurement for Http {
    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        let results = client::execute_http_request(&self.config).await?;

        // Parse target from URL for the result
        let url = Url::parse(&self.config.url)?;
        let _target_host = url.host_str().unwrap_or("unknown").to_string();

        // Resolve target IP - try to get the actual IP address
        let port = url
            .port()
            .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
        let host = url.host_str().unwrap_or("unknown");
        let (dst_addr, af, src_addr) = if let Some(socket_addr) = format!("{}:{}", host, port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
        {
            let dst_ip = socket_addr.ip();
            let af = if dst_ip.is_ipv4() { 4 } else { 6 };
            let src = get_source_addr_for_dest(dst_ip).ok();
            (dst_ip, af, src)
        } else {
            ("0.0.0.0".parse().unwrap(), 0, None)
        };

        Ok(MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Http,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: Timestamp::now(),
            af,
            dst_addr,
            dst_name: Some(host.to_string()),
            src_addr,
            proto: Some("TCP".to_string()),
            ttl: None,
            size: None,
            data: MeasurementData::Generic(serde_json::to_value(results)?),
        })
    }
}
