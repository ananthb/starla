//! HTTP measurement implementation

pub mod client;
pub mod raw;

use crate::traits::Measurement;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use starla_common::{
    MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId, Timestamp,
};
use starla_network::get_source_addr_for_dest;
use std::fmt::Write;
use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    pub url: String,
    pub method: String, // GET, POST, etc.
    pub headers: Option<std::collections::HashMap<String, String>>,
    pub body: Option<String>,
    pub timeout_ms: u64,
    pub verify_ssl: bool,
    pub version: Option<String>, // "1.1" or "2"
    /// `-4` / `-6`: the address family to fetch over (0: either)
    #[serde(default)]
    pub af: u8,
    /// `--store-headers N`: report response header lines within N bytes
    #[serde(default)]
    pub store_headers: usize,
    /// `-0`: send HTTP/1.0 instead of HTTP/1.1
    #[serde(default)]
    pub http10: bool,
}

pub struct Http {
    pub config: HttpConfig,
    pub probe_id: ProbeId,
    pub msm_id: MeasurementId,
}

/// evhttpget's RESULT body (probe-busybox/eperd/httpget.c report()): the
/// envelope is id, version, lts, time and (for a name) ttr, with everything
/// else inside the result object. "lts":0 is the placeholder the results
/// crate replaces with the lts the result was queued at.
///
/// 2026-10-13: RIPE had stored no HTTP result from either Starla probe
/// (1015560 in 33 days, 1018212), while a reference probe's 12023/12027/
/// 13023/13027 land hourly. Starla's line carried dst_name/af/dst_addr/proto
/// in the envelope and no stored headers, so the ip-echo built-ins never
/// told RIPE the probe's addresses.
pub fn result_body(msm_id: u64, gstart: i64, out: &raw::RawHttpOutcome) -> String {
    let mut r = String::from("{ ");
    if let Some(header) = &out.header {
        r.push_str("\"header\": [");
        for (i, line) in header.iter().enumerate() {
            if i > 0 {
                r.push(',');
            }
            r.push(' ');
            r.push_str(&raw::quoted(line));
        }
        r.push_str(" ], ");
    }
    if let Some(err) = &out.err {
        let _ = write!(r, "\"err\":{}, ", raw::quoted(err));
    }
    let _ = write!(r, "\"method\":\"GET\", \"af\": {}", out.af);
    if let Some(dst) = out.dst_addr {
        let _ = write!(r, ", \"dst_addr\":\"{dst}\"");
    }
    if let Some(src) = out.src_addr {
        let _ = write!(r, ", \"src_addr\":\"{src}\"");
    }
    if out.done {
        let _ = write!(
            r,
            ", \"rt\":{:.6}, \"res\":{}, \"ver\":\"{}\", \"hsize\":{}, \"bsize\":{}",
            out.rt, out.res, out.ver, out.hsize, out.bsize
        );
    }
    r.push_str(" }");

    let mut body = format!(
        "\"id\":\"{}\", \"fw\":{}, \"mver\": \"2.6.4\", \"lts\":0, \"time\":{}, ",
        msm_id,
        starla_common::FIRMWARE_VERSION,
        gstart
    );
    if let Some(ttr) = out.ttr {
        let _ = write!(body, "\"ttr\":{ttr:.6}, ");
    }
    let _ = write!(body, "\"result\":[ {r} ]");
    body
}

#[async_trait]
impl Measurement for Http {
    fn measurement_type(&self) -> starla_common::MeasurementType {
        starla_common::MeasurementType::Http
    }

    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        let gstart = Timestamp::now();
        let timeout = Duration::from_millis(self.config.timeout_ms);
        let out = match raw::parse_http_url(&self.config.url) {
            Ok(url) => {
                raw::fetch(
                    &url,
                    self.config.af,
                    self.config.http10,
                    self.config.store_headers,
                    timeout,
                )
                .await?
            }
            // https: still through reqwest, reported in the same shape.
            Err(_) => self.fetch_tls().await?,
        };

        let af = out.af;
        let dst_addr = out
            .dst_addr
            .unwrap_or_else(|| "0.0.0.0".parse().expect("literal"));
        Ok(MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Http,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: gstart,
            af,
            dst_addr,
            dst_name: None,
            src_addr: out.src_addr,
            proto: None,
            ttl: None,
            size: None,
            data: MeasurementData::FullLine(result_body(self.msm_id.0, gstart.0, &out)),
        })
    }
}

impl Http {
    /// An https:// fetch through reqwest. Header lines are rebuilt from the
    /// parsed map (names lowercased), so stored headers are approximate here.
    async fn fetch_tls(&self) -> anyhow::Result<raw::RawHttpOutcome> {
        let results = client::execute_http_request(&self.config).await?;
        let url = reqwest::Url::parse(&self.config.url)?;
        let port = url.port_or_known_default().unwrap_or(443);
        let host = url.host_str().unwrap_or("unknown");
        let dst: Option<IpAddr> = format!("{host}:{port}")
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| {
                addrs.find(|a| match self.config.af {
                    4 => a.is_ipv4(),
                    6 => a.is_ipv6(),
                    _ => true,
                })
            })
            .map(|a| a.ip());
        Ok(raw::RawHttpOutcome {
            ttr: None,
            af: if dst.is_some_and(|d| d.is_ipv6()) {
                6
            } else {
                4
            },
            dst_addr: dst,
            src_addr: dst.and_then(|d| get_source_addr_for_dest(d).ok()),
            rt: results.rt,
            res: results.status,
            ver: results.ver,
            hsize: results.header_size,
            bsize: results.body_size,
            header: raw::stored_headers(
                results.response_headers.as_deref().unwrap_or(&[]),
                self.config.store_headers,
            ),
            err: None,
            done: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape RIPE stored for reference probe 1000004's 13027 result.
    #[test]
    fn the_body_is_evhttpgets() {
        let p = raw::parse_response(
            b"HTTP/1.0 200 OK\r\nX-Client-IP: 2602:f337:ed:4174:2020:2041:746c:6173\r\nContent-Length: 37\r\n\r\n2602:f337:ed:4174:2020:2041:746c:6173",
            1024,
        )
        .unwrap();
        let out = raw::RawHttpOutcome {
            ttr: None,
            af: 6,
            dst_addr: Some("2001:67c:2e8:16::c100:68d".parse().unwrap()),
            src_addr: Some("2602:f337:ed:4174:2020:2041:746c:6173".parse().unwrap()),
            rt: 269.242169,
            res: p.res,
            ver: p.ver,
            hsize: p.hsize,
            bsize: p.bsize,
            header: p.header,
            err: None,
            done: true,
        };
        let body = result_body(13027, 1791860335, &out);
        assert_eq!(
            body,
            format!(
                "\"id\":\"13027\", \"fw\":{}, \"mver\": \"2.6.4\", \"lts\":0, \"time\":1791860335, \
                 \"result\":[ {{ \"header\": [ \"X-Client-IP: 2602:f337:ed:4174:2020:2041:746c:6173\", \
                 \"Content-Length: 37\", \"\" ], \"method\":\"GET\", \"af\": 6, \
                 \"dst_addr\":\"2001:67c:2e8:16::c100:68d\", \
                 \"src_addr\":\"2602:f337:ed:4174:2020:2041:746c:6173\", \"rt\":269.242169, \
                 \"res\":200, \"ver\":\"1.0\", \"hsize\":72, \"bsize\":37 }} ]",
                starla_common::FIRMWARE_VERSION
            )
        );
        // RIPE parses it as JSON: it must be an object once wrapped.
        let line: serde_json::Value = serde_json::from_str(&format!("{{ {body} }}")).unwrap();
        assert_eq!(
            line["result"][0]["header"][0],
            "X-Client-IP: 2602:f337:ed:4174:2020:2041:746c:6173"
        );
        for gone in ["dst_name", "proto", "af", "dst_addr"] {
            assert!(
                line.get(gone).is_none(),
                "{gone} is not in evhttpget's envelope"
            );
        }
    }

    #[test]
    fn a_name_reports_ttr_and_a_failure_reports_err() {
        let out = raw::RawHttpOutcome {
            ttr: Some(1.5),
            af: 4,
            dst_addr: Some("193.0.6.141".parse().unwrap()),
            err: Some("connect: timeout".into()),
            ..Default::default()
        };
        let body = result_body(12023, 1, &out);
        assert!(body.contains("\"time\":1, \"ttr\":1.500000, \"result\":[ { \"err\":\"connect: timeout\", \"method\":\"GET\", \"af\": 4, \"dst_addr\":\"193.0.6.141\" } ]"), "{body}");
        assert!(!body.contains("\"rt\""), "{body}");
    }
}
