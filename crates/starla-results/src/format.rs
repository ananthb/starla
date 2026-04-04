//! RIPE Atlas result format wrapper
//!
//! This module provides types for wrapping measurement results in the
//! official RIPE Atlas result format.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use starla_common::{MeasurementData, MeasurementResult, MeasurementType};
use std::net::IpAddr;

/// Format a serde_json::Value for the "result" field to match C probe spacing.
///
/// The C probe writes arrays as `[ { ... }, { ... } ]` with spaces inside
/// brackets and spaces inside braces. Objects are written as `{ "key":value }`.
fn format_result_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                return "[ ]".to_string();
            }
            let entries: Vec<String> = items.iter().map(format_result_value).collect();
            format!("[ {} ]", entries.join(", "))
        }
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                return "{ }".to_string();
            }
            let entries: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    let v_str = match v {
                        serde_json::Value::String(s) => format!("\"{}\"", s),
                        _ => v.to_string(),
                    };
                    format!("\"{}\":{}", k, v_str)
                })
                .collect();
            format!("{{ {} }}", entries.join(", "))
        }
        other => other.to_string(),
    }
}

/// Wrapper for RIPE Atlas result format
///
/// This structure matches the official RIPE Atlas result JSON format
/// as produced by the official busybox probe.
///
/// Key fields:
/// - `prb_id` is the probe ID (required for result identification)
/// - `msm_id` is the measurement ID
/// - `time` is the timestamp (not `timestamp`)
/// - `lts` is the local time sync indicator
/// - `result` is NOT flattened - it's a nested object or array
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasResult {
    /// Probe ID (not serialized — controller knows this from the URL)
    #[serde(skip_serializing)]
    pub prb_id: u32,

    /// Measurement ID (serialized as string to match official probe format)
    #[serde(rename = "id")]
    pub id: String,

    /// Firmware version (e.g., 5120)
    pub fw: u32,

    /// Measurement version (measurement engine version)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mver: Option<String>,

    /// Local time sync indicator (seconds since last time sync)
    pub lts: i64,

    /// Measurement timestamp (when measurement was executed)
    pub time: i64,

    /// Destination name (hostname, for ping/traceroute - not used for DNS)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dst_name: Option<String>,

    /// Address family (4 or 6)
    pub af: u8,

    /// Destination address
    pub dst_addr: String,

    /// Destination port (for DNS)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dst_port: Option<String>,

    /// Source address (probe's IP used for the measurement)
    pub src_addr: String,

    /// Protocol (ICMP for ping, UDP/TCP/ICMP for traceroute, etc.)
    pub proto: String,

    /// TTL used for sending (ping/traceroute)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u8>,

    /// Packet size (for ping/traceroute envelope, not DNS)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u16>,

    /// End time for traceroute
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endtime: Option<i64>,

    /// Paris ID for traceroute
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paris_id: Option<u8>,

    /// Group ID for bundled results
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<u64>,

    /// Bundle index within a group
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<u32>,

    /// Measurement-specific result data (NOT flattened - stays as nested
    /// object/array)
    pub result: serde_json::Value,
}

impl AtlasResult {
    /// Create an AtlasResult from a MeasurementResult
    ///
    /// Uses fields from MeasurementResult for proto, TTL, size, etc.
    pub fn from_measurement(result: MeasurementResult, source_ip: Option<IpAddr>) -> Self {
        // Use proto from result, or derive from measurement type
        let proto = result.proto.clone().unwrap_or_else(|| {
            match result.measurement_type {
                MeasurementType::Ping => "ICMP",
                MeasurementType::Traceroute => "UDP",
                MeasurementType::Dns => "UDP",
                MeasurementType::Http => "TCP",
                MeasurementType::Tls => "TCP",
                MeasurementType::Ntp => "UDP",
            }
            .to_string()
        });

        let dst_addr_str = result.dst_addr.to_string();

        // dst_name is used for ping/traceroute, not for DNS
        // For DNS, dst_port is used instead
        let (dst_name, dst_port) = match result.measurement_type {
            MeasurementType::Dns => (None, Some("53".to_string())),
            _ => (
                Some(
                    result
                        .dst_name
                        .clone()
                        .unwrap_or_else(|| dst_addr_str.clone()),
                ),
                None,
            ),
        };

        // Use source IP from result or parameter
        let src_addr = result
            .src_addr
            .map(|ip| ip.to_string())
            .or_else(|| source_ip.map(|ip| ip.to_string()))
            .unwrap_or_default();

        // For DNS, size should NOT be in the envelope (it's in the result object)
        let size = match result.measurement_type {
            MeasurementType::Dns => None,
            _ => result.size,
        };

        Self {
            prb_id: result.prb_id.0,
            id: result.msm_id.0.to_string(),
            fw: result.fw,
            mver: Some("2.6.4".to_string()), // Match official probe version
            lts: 0,                          // Will be set by caller based on time sync status
            time: result.timestamp.0,
            dst_name,
            af: result.af,
            dst_addr: dst_addr_str,
            dst_port,
            src_addr,
            proto,
            ttl: result.ttl,
            size,
            endtime: None,
            paris_id: None,
            group_id: None,
            bundle: None,
            result: match result.data {
                MeasurementData::Generic(v) => v,
                MeasurementData::PreFormatted(s) => serde_json::Value::String(s),
                MeasurementData::FullLine(s) => {
                    serde_json::Value::String(format!("__FULLLINE__{}", s))
                }
            },
        }
    }

    /// Set the protocol field
    pub fn with_proto(mut self, proto: &str) -> Self {
        self.proto = proto.to_string();
        self
    }

    /// Set the destination name (hostname)
    pub fn with_dst_name(mut self, name: &str) -> Self {
        self.dst_name = Some(name.to_string());
        self
    }

    /// Set the destination port (for DNS)
    pub fn with_dst_port(mut self, port: &str) -> Self {
        self.dst_port = Some(port.to_string());
        self
    }

    /// Set TTL
    pub fn with_ttl(mut self, ttl: u8) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Set packet size
    pub fn with_size(mut self, size: u16) -> Self {
        self.size = Some(size);
        self
    }

    /// Set the local time sync value
    pub fn with_lts(mut self, lts: i64) -> Self {
        self.lts = lts;
        self
    }

    /// Set group and bundle for grouped results
    pub fn with_bundle(mut self, group_id: u64, bundle: u32) -> Self {
        self.group_id = Some(group_id);
        self.bundle = Some(bundle);
        self
    }

    /// Format as a RESULT line matching the exact output of the official C
    /// probe.
    ///
    /// The official probe uses fprintf with specific spacing:
    /// `RESULT { "id":"<id>", "fw":<fw>, "mver": "<mver>", "lts":<lts>,
    /// "time":<time>, ... }\n`
    ///
    /// Note the quirks: space after `{`, space before `}`, `"mver": ` has a
    /// space after the colon (all other fields don't). We must match this
    /// exactly because the controller may do rigid string parsing.
    pub fn to_result_line(&self) -> String {
        use std::fmt::Write;

        // FullLine: the measurement provided the complete line body
        if let serde_json::Value::String(ref v) = self.result {
            if let Some(body) = v.strip_prefix("__FULLLINE__") {
                return format!("RESULT {{ {} }}\n", body);
            }
        }

        let mut s = String::with_capacity(512);

        // Envelope fields (id, fw, mver, lts, time) — always present
        write!(
            s,
            "RESULT {{ \"id\":\"{}\", \"fw\":{}, \"mver\": \"{}\", \"lts\":{}, \"time\":{}",
            self.id,
            self.fw,
            self.mver.as_deref().unwrap_or("2.6.4"),
            self.lts,
            self.time,
        )
        .unwrap();

        // Optional bundle
        if let Some(ref bundle) = self.bundle {
            write!(s, ", \"bundle\":{}", bundle).unwrap();
        }

        // dst_name (always present for ping/traceroute, absent for DNS)
        if let Some(ref name) = self.dst_name {
            write!(s, ", \"dst_name\":\"{}\"", name).unwrap();
        }

        // af
        write!(s, ", \"af\":{}", self.af).unwrap();

        // dst_addr
        if !self.dst_addr.is_empty() {
            write!(s, ", \"dst_addr\":\"{}\"", self.dst_addr).unwrap();
        }

        // dst_port (DNS)
        if let Some(ref port) = self.dst_port {
            write!(s, ", \"dst_port\":\"{}\"", port).unwrap();
        }

        // src_addr
        if !self.src_addr.is_empty() {
            write!(s, ", \"src_addr\":\"{}\"", self.src_addr).unwrap();
        }

        // proto
        write!(s, ", \"proto\":\"{}\"", self.proto).unwrap();

        // ttl
        if let Some(ttl) = self.ttl {
            write!(s, ", \"ttl\":{}", ttl).unwrap();
        }

        // size
        if let Some(size) = self.size {
            write!(s, ", \"size\":{}", size).unwrap();
        }

        // endtime (traceroute)
        if let Some(endtime) = self.endtime {
            write!(s, ", \"endtime\":{}", endtime).unwrap();
        }

        // paris_id (traceroute)
        if let Some(paris_id) = self.paris_id {
            write!(s, ", \"paris_id\":{}", paris_id).unwrap();
        }

        // result — if PreFormatted, use the string directly; otherwise format via
        // format_result_value
        match &self.result {
            serde_json::Value::String(pre) => {
                // PreFormatted result string — use verbatim
                write!(s, ", \"result\": {}", pre).unwrap();
            }
            other => {
                let result_json = format_result_value(other);
                write!(s, ", \"result\": {}", result_json).unwrap();
            }
        }

        // Close with space before brace, matching C probe's fprintf
        s.push_str(" }\n");

        s
    }
}

/// Result bundle for grouped measurements
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultBundle {
    /// Bundle ID
    pub bundle_id: u64,
    /// Results in this bundle
    pub results: Vec<AtlasResult>,
    /// When bundle was created
    pub created_at: i64,
}

impl ResultBundle {
    /// Create a new result bundle
    pub fn new(bundle_id: u64) -> Self {
        Self {
            bundle_id,
            results: Vec::new(),
            created_at: Utc::now().timestamp(),
        }
    }

    /// Add a result to the bundle
    pub fn add(&mut self, mut result: AtlasResult) {
        let bundle_index = self.results.len() as u32;
        result.group_id = Some(self.bundle_id);
        result.bundle = Some(bundle_index);
        self.results.push(result);
    }

    /// Get the number of results in the bundle
    pub fn len(&self) -> usize {
        self.results.len()
    }

    /// Check if bundle is empty
    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starla_common::{MeasurementId, ProbeId, Timestamp};

    fn make_ping_result() -> MeasurementResult {
        MeasurementResult {
            fw: 5080,
            measurement_type: MeasurementType::Ping,
            prb_id: ProbeId(12345),
            msm_id: MeasurementId(1001),
            timestamp: Timestamp(999999999),
            af: 4,
            dst_addr: "193.0.14.129".parse().unwrap(),
            dst_name: Some("193.0.14.129".to_string()),
            src_addr: Some("10.0.0.1".parse().unwrap()),
            proto: Some("ICMP".to_string()),
            ttl: Some(56),
            size: Some(32),
            data: MeasurementData::Generic(serde_json::json!([
                { "rtt": 10.500000 },
                { "rtt": 11.200000 },
                { "rtt": 10.800000 }
            ])),
        }
    }

    fn make_dns_result() -> MeasurementResult {
        MeasurementResult {
            fw: 5080,
            measurement_type: MeasurementType::Dns,
            prb_id: ProbeId(12345),
            msm_id: MeasurementId(8310237),
            timestamp: Timestamp(999999999),
            af: 4,
            dst_addr: "8.8.8.8".parse().unwrap(),
            dst_name: None, // DNS uses dst_addr directly, no dst_name
            src_addr: Some("10.0.0.1".parse().unwrap()),
            proto: Some("UDP".to_string()),
            ttl: None,
            size: None, // DNS doesn't use envelope size
            data: MeasurementData::Generic(serde_json::json!({
                "rt": 35.265,
                "size": 62,
                "ID": 12345,
                "ANCOUNT": 1,
                "QDCOUNT": 1,
                "NSCOUNT": 0,
                "ARCOUNT": 0
            })),
        }
    }

    #[test]
    fn test_ping_result_line_format() {
        let result = make_ping_result();
        let atlas = AtlasResult::from_measurement(result, None).with_lts(10);
        let line = atlas.to_result_line();

        // Must start with RESULT { and end with }\n
        assert!(line.starts_with("RESULT { "));
        assert!(line.ends_with(" }\n"));

        // Must have correct field order: id, fw, mver, lts, time, dst_name, af, ...
        let id_pos = line.find("\"id\":").unwrap();
        let fw_pos = line.find("\"fw\":").unwrap();
        let mver_pos = line.find("\"mver\":").unwrap();
        let lts_pos = line.find("\"lts\":").unwrap();
        let time_pos = line.find("\"time\":").unwrap();
        let dst_name_pos = line.find("\"dst_name\":").unwrap();
        let af_pos = line.find("\"af\":").unwrap();
        let result_pos = line.find("\"result\":").unwrap();
        assert!(id_pos < fw_pos);
        assert!(fw_pos < mver_pos);
        assert!(mver_pos < lts_pos);
        assert!(lts_pos < time_pos);
        assert!(time_pos < dst_name_pos);
        assert!(dst_name_pos < af_pos);
        assert!(af_pos < result_pos);

        // mver must have space after colon (C probe quirk)
        assert!(line.contains("\"mver\": \""));

        // Must NOT have prb_id
        assert!(!line.contains("prb_id"));

        // Must NOT have double commas
        assert!(!line.contains(", ,"));

        // Result field must have space after colon and C-probe array spacing
        assert!(line.contains("\"result\": [ { \"rtt\":"));
    }

    #[test]
    fn test_dns_result_line_no_double_comma() {
        // DNS has no dst_name — this previously caused a double comma bug
        let result = make_dns_result();
        let atlas = AtlasResult::from_measurement(result, None).with_lts(10);
        let line = atlas.to_result_line();

        // Must NOT have double commas (the bug that prevented uploads)
        assert!(
            !line.contains(", ,"),
            "Double comma found in DNS result line: {}",
            line
        );

        // DNS result is an object, not an array
        assert!(line.contains("\"result\": { "));

        // Must have dst_port for DNS
        assert!(line.contains("\"dst_port\":\"53\""));

        // Must NOT have dst_name (DNS doesn't use it)
        assert!(!line.contains("\"dst_name\":"));
    }

    #[test]
    fn test_ping_result_line_no_ttl_on_timeout() {
        // When all pings timeout, ttl should NOT be in the result line
        let mut result = make_ping_result();
        result.ttl = None; // No reply received
        result.data = MeasurementData::Generic(serde_json::json!([
            { "x": "*" },
            { "x": "*" }
        ]));

        let atlas = AtlasResult::from_measurement(result, None).with_lts(10);
        let line = atlas.to_result_line();

        assert!(!line.contains("\"ttl\":"));
        assert!(line.contains("\"x\":\"*\""));
    }

    #[test]
    fn test_format_result_value_spacing() {
        // Array of objects should use C-probe spacing
        let val = serde_json::json!([{"rtt": 10.5}, {"rtt": 11.2}]);
        let formatted = format_result_value(&val);
        assert_eq!(formatted, "[ { \"rtt\":10.5 }, { \"rtt\":11.2 } ]");

        // Single object
        let val = serde_json::json!({"rt": 35.2, "size": 62});
        let formatted = format_result_value(&val);
        assert!(formatted.starts_with("{ "));
        assert!(formatted.ends_with(" }"));

        // Empty array
        let val = serde_json::json!([]);
        assert_eq!(format_result_value(&val), "[ ]");
    }

    #[test]
    fn test_preformatted_result_used_verbatim() {
        // PreFormatted result strings should appear verbatim in the RESULT line
        let mut result = make_ping_result();
        result.data = MeasurementData::PreFormatted(
            "[ { \"rtt\":10.500000 }, { \"rtt\":11.200000 } ]".to_string(),
        );
        let atlas = AtlasResult::from_measurement(result, None).with_lts(10);
        let line = atlas.to_result_line();

        // Must contain the exact pre-formatted string
        assert!(line.contains("\"result\": [ { \"rtt\":10.500000 }, { \"rtt\":11.200000 } ]"));
        // Must NOT re-format through format_result_value
        assert!(!line.contains("\"result\": [ { \"rtt\":10.5 }"));
    }

    #[test]
    fn test_fullline_bypasses_envelope() {
        // FullLine should produce the complete RESULT line body, bypassing the envelope
        let mut result = make_ping_result();
        result.data = MeasurementData::FullLine(
            "\"id\":\"42\", \"fw\":5080, \"custom_field\":\"test\", \"cert\":[ \"PEM\" ]"
                .to_string(),
        );
        let atlas = AtlasResult::from_measurement(result, None).with_lts(10);
        let line = atlas.to_result_line();

        // Must use the FullLine body directly
        assert!(line.starts_with("RESULT { \"id\":\"42\""));
        assert!(line.contains("\"custom_field\":\"test\""));
        assert!(line.contains("\"cert\":[ \"PEM\" ]"));
        assert!(line.ends_with(" }\n"));

        // Must NOT contain the standard envelope fields from AtlasResult
        assert!(!line.contains("\"mver\":"));
        assert!(!line.contains("\"dst_name\":"));
    }

    #[test]
    fn test_http_result_minimal_envelope() {
        // HTTP result should have all fields inside the result array, not in envelope
        let mut result = make_ping_result();
        result.measurement_type = MeasurementType::Http;
        result.dst_name = None;
        result.proto = None;
        result.ttl = None;
        result.size = None;
        result.data = MeasurementData::PreFormatted(
            "[ { \"method\":\"GET\", \"af\": 4, \"dst_addr\":\"1.2.3.4\", \
             \"src_addr\":\"5.6.7.8\", \"rt\":123.456000, \"res\":200, \"ver\":\"1.1\", \
             \"hsize\":100, \"bsize\":500 } ]"
                .to_string(),
        );
        let atlas = AtlasResult::from_measurement(result, None).with_lts(10);
        let line = atlas.to_result_line();

        // Must contain the HTTP result array
        assert!(line.contains("\"method\":\"GET\""));
        assert!(line.contains("\"res\":200"));
    }

    #[test]
    fn test_traceroute_preformatted_hops() {
        // Traceroute result should be just the hops array with 3-decimal RTTs
        let mut result = make_ping_result();
        result.measurement_type = MeasurementType::Traceroute;
        result.data = MeasurementData::PreFormatted(
            "[ { \"hop\":1, \"result\": [ { \"from\":\"10.0.0.1\", \"ttl\":64, \"size\":28, \
             \"rtt\":1.234 } ] }, { \"hop\":2, \"result\": [ { \"x\":\"*\" } ] } ]"
                .to_string(),
        );
        let atlas = AtlasResult::from_measurement(result, None).with_lts(10);
        let line = atlas.to_result_line();

        assert!(line.contains("\"hop\":1"));
        assert!(line.contains("\"rtt\":1.234"));
        assert!(line.contains("\"x\":\"*\""));
    }
}
