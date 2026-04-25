//! DNS measurement implementation

pub mod resolver;

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
        let proto = match self.config.protocol {
            DnsProtocol::TCP => "TCP",
            DnsProtocol::UDP => "UDP",
        };
        let src_addr = get_source_addr_for_dest(self.config.target).ok();

        // Execute DNS query: handle timeout as a valid result with error field
        // (matching official C probe behavior)
        let result_str = match resolver::execute_dns_query(&self.config, self.probe_id.0).await {
            Ok(results) => {
                let mut s = String::with_capacity(512);
                write!(
                    s,
                    "{{ \"rt\":{:.3},\"size\":{}, \"abuf\":\"{}\",\"ID\":{}, \"ANCOUNT\":{}, \
                     \"QDCOUNT\":{}, \"NSCOUNT\":{}, \"ARCOUNT\":{}",
                    results.rt,
                    results.size,
                    results.abuf,
                    results.id,
                    results.ancount,
                    results.qdcount,
                    results.nscount,
                    results.arcount,
                )
                .unwrap();

                if let Some(answers) = resolver::decode_answers(&results.abuf) {
                    write!(s, ",\"answers\":[ ").unwrap();
                    for (i, ans) in answers.iter().enumerate() {
                        if i > 0 {
                            write!(s, ", ").unwrap();
                        }
                        let rdata_json: Vec<String> =
                            ans.rdata.iter().map(|s| format!("\"{}\"", s)).collect();
                        write!(
                            s,
                            "{{\"TYPE\":\"{}\", \"NAME\":\"{}\", \"RDATA\":[ {} ]}}",
                            ans.record_type,
                            ans.name,
                            rdata_json.join(", ")
                        )
                        .unwrap();
                    }
                    write!(s, " ]").unwrap();
                }

                write!(s, " }}").unwrap();
                s
            }
            Err(_) => {
                // Timeout or network error: report as result with error field
                // matching official C probe: {"error":{"timeout":5000}}
                format!("{{ \"error\":{{\"timeout\":{}}} }}", self.config.timeout_ms)
            }
        };

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
            data: MeasurementData::PreFormatted(result_str),
        })
    }
}
