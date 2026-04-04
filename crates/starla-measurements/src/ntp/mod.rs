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

        // NTP has non-standard envelope: header fields between af and result
        // C probe: "dst_name":"...", "dst_addr":"...", "src_addr":"...", "proto":"UDP",
        // "af": 4,   "li":"no", "version":4, "mode":"server", "stratum":1,
        // "poll":8,   "precision":3.8147e-06, "root-delay":0,
        // "root-dispersion":0.00105286,   "ref-id":"GPS",
        // "ref-ts":3835786747.036665440,   "result": [ { "origin-ts":...,
        // "receive-ts":..., "transmit-ts":..., "final-ts":..., "rtt":..., "offset":...
        // } ]
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
        // Map mode to string
        let mode_str = match results.mode {
            1 => "symmetric_active",
            2 => "symmetric_passive",
            3 => "client",
            4 => "server",
            5 => "broadcast",
            _ => "unknown",
        };
        // Convert poll/precision to powers of 2
        let poll_val = 1u64 << (results.poll as u64);
        let precision_val = 2.0_f64.powi(results.precision as i32);
        // Root delay/dispersion: our code stores in ms, C probe uses seconds
        let root_delay_s = results.root_delay / 1000.0;
        let root_disp_s = results.root_dispersion / 1000.0;

        // Single-sample result array
        let result_entry = format!(
            "{{ \"origin-ts\": {:.9}, \"receive-ts\": {:.9}, \"transmit-ts\": {:.9}, \
             \"final-ts\": 0.000000000, \"rtt\": {:.6}, \"offset\": {:.6} }}",
            results.ref_ts, // use ref_ts as origin placeholder
            results.recv_ts,
            results.trans_ts,
            results.rt / 1000.0,     // convert ms to seconds
            results.offset / 1000.0, // convert ms to seconds
        );

        let body = format!(
            "\"id\":\"{}\", \"fw\":{}, \"mver\": \"2.6.4\", \"lts\":0, \"time\":{}, \
             \"dst_addr\":\"{}\", \"src_addr\":\"{}\", \"proto\":\"UDP\", \"af\": {}, \"li\": \
             \"{}\", \"version\": {}, \"mode\": \"{}\", \"stratum\": {}, \"poll\": {}, \
             \"precision\": {:e}, \"root-delay\": {}, \"root-dispersion\": {}, \"ref-id\": \
             \"{}\", \"ref-ts\": {:.9}, \"result\": [ {} ]",
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
            poll_val,
            precision_val,
            root_delay_s,
            root_disp_s,
            results.ref_id,
            results.ref_ts,
            result_entry,
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
