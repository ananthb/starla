//! RIPE Atlas result format wrapper
//!
//! This module provides types for wrapping measurement results in the
//! official RIPE Atlas result format.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use starla_common::{MeasurementData, MeasurementResult, MeasurementType};
use std::net::IpAddr;

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
    /// Probe ID (required for result identification)
    pub prb_id: u32,

    /// Measurement ID
    #[serde(rename = "msm_id")]
    pub msm_id: u64,

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
            msm_id: result.msm_id.0,
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

    fn make_measurement_result() -> MeasurementResult {
        MeasurementResult {
            fw: 6000,
            measurement_type: MeasurementType::Ping,
            prb_id: ProbeId(12345),
            msm_id: MeasurementId(1001),
            timestamp: Timestamp::now(),
            af: 4,
            dst_addr: "8.8.8.8".parse().unwrap(),
            dst_name: None,
            src_addr: None,
            proto: Some("ICMP".to_string()),
            ttl: Some(64),
            size: Some(32),
            // For ping, result is just an array (not wrapped in an object)
            data: MeasurementData::Generic(serde_json::json!([
                { "rtt": 10.5 },
                { "rtt": 15.2 },
                { "rtt": 12.3 }
            ])),
        }
    }

    #[test]
    fn test_atlas_result_format() {
        let result = make_measurement_result();
        let atlas = AtlasResult::from_measurement(result, Some("192.168.1.1".parse().unwrap()));

        assert_eq!(atlas.fw, 6000);
        assert_eq!(atlas.prb_id, 12345);
        assert_eq!(atlas.msm_id, 1001);
        assert_eq!(atlas.af, 4);
        assert_eq!(atlas.src_addr, "192.168.1.1");
        assert_eq!(atlas.dst_addr, "8.8.8.8");
        assert_eq!(atlas.proto, "ICMP"); // Ping uses ICMP
    }

    #[test]
    fn test_atlas_result_serialization() {
        let result = make_measurement_result();
        let atlas =
            AtlasResult::from_measurement(result.clone(), Some("10.15.16.123".parse().unwrap()))
                .with_ttl(64)
                .with_size(32)
                .with_lts(10);

        let json = serde_json::to_string_pretty(&atlas).unwrap();
        eprintln!("AtlasResult format:\n{}", json);

        // Should contain result array with RTT values (official format)
        assert!(json.contains("\"result\":"));
        assert!(json.contains("\"rtt\":"));
        // Should have prb_id and msm_id
        assert!(json.contains("\"prb_id\":"));
        assert!(json.contains("\"msm_id\":"));
        // Should have time (not timestamp)
        assert!(json.contains("\"time\":"));
        // Should have proto
        assert!(json.contains("\"proto\":"));
        assert!(json.contains("\"ICMP\""));
        // Should have ttl and size
        assert!(json.contains("\"ttl\":"));
        assert!(json.contains("\"size\":"));
    }

    #[test]
    fn test_result_bundle() {
        let result1 = make_measurement_result();
        let result2 = make_measurement_result();

        let atlas1 = AtlasResult::from_measurement(result1, None);
        let atlas2 = AtlasResult::from_measurement(result2, None);

        let mut bundle = ResultBundle::new(999);
        bundle.add(atlas1);
        bundle.add(atlas2);

        assert_eq!(bundle.len(), 2);
        assert_eq!(bundle.results[0].group_id, Some(999));
        assert_eq!(bundle.results[0].bundle, Some(0));
        assert_eq!(bundle.results[1].bundle, Some(1));
    }
}
