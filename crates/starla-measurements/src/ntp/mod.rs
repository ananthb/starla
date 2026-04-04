//! NTP measurement implementation

pub mod client;

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
        let results = client::execute_ntp_query(&self.config, 3).await?;

        let src_addr = get_source_addr_for_dest(self.config.target).ok();
        let af = if self.config.target.is_ipv4() { 4 } else { 6 };
        let src_str = src_addr.map(|ip| ip.to_string()).unwrap_or_default();
        let timestamp = Timestamp::now().0;

        // Map leap indicator to C probe format
        let li_str = match results.leap {
            0 => "no",
            1 => "61",
            2 => "59",
            _ => "unknown",
        };
        let mode_str = match results.mode {
            1 => "symmetric_active",
            2 => "symmetric_passive",
            3 => "client",
            4 => "server",
            5 => "broadcast",
            _ => "unknown",
        };
        let precision_val = 2.0_f64.powi(results.precision as i32);

        // Format samples array
        let mut samples_str = String::new();
        for (i, s) in results.samples.iter().enumerate() {
            if i > 0 {
                write!(samples_str, ", ").unwrap();
            }
            write!(
                samples_str,
                "{{ \"origin-ts\":{:.9}, \"receive-ts\":{:.9}, \"transmit-ts\":{:.9}, \
                 \"final-ts\":{:.9}, \"rtt\":{:.6}, \"offset\":{:.6} }}",
                s.origin_ts, s.receive_ts, s.transmit_ts, s.final_ts, s.rtt, s.offset,
            )
            .unwrap();
        }

        let body = format!(
            "\"id\":\"{}\", \"fw\":{}, \"mver\": \"2.6.4\", \"lts\":0, \"time\":{}, \
             \"dst_addr\":\"{}\", \"src_addr\":\"{}\", \"proto\":\"UDP\", \"af\": {}, \
             \"li\":\"{}\", \"version\":{}, \"mode\":\"{}\", \"stratum\":{}, \"poll\":{}, \
             \"precision\":{:e}, \"root-delay\":{}, \"root-dispersion\":{}, \"ref-id\":\"{}\", \
             \"ref-ts\":{:.9}, \"result\": [ {} ]",
            self.msm_id.0,
            starla_common::FIRMWARE_VERSION,
            timestamp,
            self.config.target,
            src_str,
            af,
            li_str,
            results.version,
            mode_str,
            results.stratum,
            results.poll,
            precision_val,
            results.root_delay,
            results.root_dispersion,
            results.ref_id,
            results.ref_ts,
            samples_str,
        );

        Ok(MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Ntp,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: Timestamp(timestamp),
            af,
            dst_addr: self.config.target,
            dst_name: None,
            src_addr,
            proto: Some("UDP".to_string()),
            ttl: None,
            size: None,
            data: MeasurementData::FullLine(body),
        })
    }
}
