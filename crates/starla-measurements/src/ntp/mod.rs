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
    /// The target as the controller named it (evntp's dst_name)
    pub dst_name: String,
    /// `-w`: how long to wait for each reply
    pub timeout_ms: u64,
    pub port: u16,
    /// `-c`: requests to send
    pub count: u32,
    /// `-g`: minimum time between requests (5130)
    pub gap_ms: u64,
}

/// evntp's RESULT body (probe-busybox/eperd/ntp.c report()).
fn result_body(
    msm_id: u64,
    config: &NtpConfig,
    src_str: &str,
    af: u8,
    timestamp: i64,
    results: &client::NtpResult,
) -> String {
    // Format samples array: a timeout is { "x":"*" }, as evntp writes it
    let mut samples_str = String::new();
    for (i, s) in results.samples.iter().enumerate() {
        if i > 0 {
            write!(samples_str, ", ").unwrap();
        }
        match s {
            Some(s) => write!(
                samples_str,
                "{{ \"origin-ts\":{:.9}, \"receive-ts\":{:.9}, \"transmit-ts\":{:.9}, \
                 \"final-ts\":{:.9}, \"rtt\":{:.6}, \"offset\":{:.6} }}",
                s.origin_ts, s.receive_ts, s.transmit_ts, s.final_ts, s.rtt, s.offset,
            )
            .unwrap(),
            None => write!(samples_str, "{{ \"x\":\"*\" }}").unwrap(),
        }
    }

    // The server's header fields, only when one answered (evntp's
    // `!state->first`).
    let header_str = match &results.header {
        Some(h) => {
            // Map leap indicator to C probe format
            let li_str = match h.leap {
                0 => "no",
                1 => "61",
                2 => "59",
                _ => "unknown",
            };
            let mode_str = match h.mode {
                1 => "symmetric_active",
                2 => "symmetric_passive",
                3 => "client",
                4 => "server",
                5 => "broadcast",
                _ => "unknown",
            };
            let precision_val = 2.0_f64.powi(h.precision as i32);
            format!(
                ", \"li\":\"{}\", \"version\":{}, \"mode\":\"{}\", \"stratum\":{}, \
                 \"poll\":{}, \"precision\":{:e}, \"root-delay\":{}, \
                 \"root-dispersion\":{}, \"ref-id\":\"{}\", \"ref-ts\":{:.9}",
                li_str,
                h.version,
                mode_str,
                h.stratum,
                h.poll,
                precision_val,
                h.root_delay,
                h.root_dispersion,
                h.ref_id,
                h.ref_ts,
            )
        }
        None => String::new(),
    };

    // "lts":0 is a placeholder the results crate replaces with the lts
    // the result was queued at.
    format!(
        "\"id\":\"{}\", \"fw\":{}, \"mver\": \"2.6.4\", \"lts\":0, \"time\":{}, \
         \"dst_name\":\"{}\", \"dst_addr\":\"{}\", \"src_addr\":\"{}\", \"proto\":\"UDP\", \
         \"af\": {}{}, \"result\": [ {} ]",
        msm_id,
        starla_common::FIRMWARE_VERSION,
        timestamp,
        config.dst_name,
        config.target,
        src_str,
        af,
        header_str,
        samples_str,
    )
}

pub struct Ntp {
    pub config: NtpConfig,
    pub probe_id: ProbeId,
    pub msm_id: MeasurementId,
}

#[async_trait]
impl Measurement for Ntp {
    fn measurement_type(&self) -> starla_common::MeasurementType {
        starla_common::MeasurementType::Ntp
    }

    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        let results = client::execute_ntp_query(&self.config).await?;

        let src_addr = get_source_addr_for_dest(self.config.target).ok();
        let af = if self.config.target.is_ipv4() { 4 } else { 6 };
        let src_str = src_addr.map(|ip| ip.to_string()).unwrap_or_default();
        let timestamp = Timestamp::now().0;

        let body = result_body(
            self.msm_id.0,
            &self.config,
            &src_str,
            af,
            timestamp,
            &results,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> NtpConfig {
        NtpConfig {
            target: "193.0.0.229".parse().unwrap(),
            dst_name: "ntp.ripe.net".into(),
            timeout_ms: 1000,
            port: 123,
            count: 3,
            gap_ms: 4000,
        }
    }

    /// No answer at all: evntp prints no header, one { "x":"*" } per request.
    #[test]
    fn an_unanswered_measurement_is_reported_not_dropped() {
        let results = client::NtpResult {
            header: None,
            samples: vec![None, None, None],
            ttr: 3.0,
        };
        let body = result_body(13023, &config(), "199.184.91.28", 4, 1, &results);
        assert!(
            body.starts_with("\"id\":\"13023\", \"fw\":5130, "),
            "{body}"
        );
        assert!(body.contains("\"lts\":0, "), "the placeholder: {body}");
        assert!(
            body.contains("\"dst_name\":\"ntp.ripe.net\", \"dst_addr\":\"193.0.0.229\""),
            "{body}"
        );
        assert!(!body.contains("\"li\""), "{body}");
        assert!(
            body.ends_with("\"result\": [ { \"x\":\"*\" }, { \"x\":\"*\" }, { \"x\":\"*\" } ]"),
            "{body}"
        );
    }

    #[test]
    fn a_timeout_between_answers_keeps_its_place() {
        let sample = client::NtpSample {
            origin_ts: 1.0,
            receive_ts: 1.5,
            transmit_ts: 1.5,
            final_ts: 1.0,
            rtt: 0.0,
            offset: 0.5,
        };
        let results = client::NtpResult {
            header: Some(client::NtpHeader {
                stratum: 1,
                leap: 0,
                version: 4,
                mode: 4,
                poll: 3,
                precision: -20,
                root_delay: 0.0,
                root_dispersion: 0.0,
                ref_id: "GPS".into(),
                ref_ts: 1.0,
            }),
            samples: vec![Some(sample.clone()), None, Some(sample)],
            ttr: 8.0,
        };
        let body = result_body(13023, &config(), "199.184.91.28", 4, 1, &results);
        assert!(
            body.contains(", \"li\":\"no\", \"version\":4, \"mode\":\"server\", \"stratum\":1"),
            "{body}"
        );
        let result = &body[body.find("\"result\"").unwrap()..];
        assert_eq!(result.matches("\"offset\":0.500000").count(), 2, "{result}");
        assert!(result.contains(" }, { \"x\":\"*\" }, { "), "{result}");
    }
}
