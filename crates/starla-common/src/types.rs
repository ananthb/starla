//! Core types for Starla measurements

use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::IpAddr;

/// Probe identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProbeId(pub u32);

impl fmt::Display for ProbeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Measurement identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MeasurementId(pub u64);

impl fmt::Display for MeasurementId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unix timestamp
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub fn now() -> Self {
        Self(chrono::Utc::now().timestamp())
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Measurement type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MeasurementType {
    Ping,
    Traceroute,
    Dns,
    Http,
    Tls,
    Ntp,
}

impl fmt::Display for MeasurementType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MeasurementType::Ping => write!(f, "ping"),
            MeasurementType::Traceroute => write!(f, "traceroute"),
            MeasurementType::Dns => write!(f, "dns"),
            MeasurementType::Http => write!(f, "http"),
            MeasurementType::Tls => write!(f, "tls"),
            MeasurementType::Ntp => write!(f, "ntp"),
        }
    }
}

/// Measurement result envelope
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeasurementResult {
    /// Firmware version
    pub fw: u32,

    /// Measurement type
    #[serde(rename = "type")]
    pub measurement_type: MeasurementType,

    /// Probe ID
    pub prb_id: ProbeId,

    /// Measurement ID
    pub msm_id: MeasurementId,

    /// Timestamp
    pub timestamp: Timestamp,

    /// Address family (4 or 6)
    pub af: u8,

    /// Destination address
    pub dst_addr: IpAddr,

    /// Destination name (hostname, defaults to dst_addr if not specified)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dst_name: Option<String>,

    /// Source address (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src_addr: Option<IpAddr>,

    /// Protocol (ICMP, UDP, TCP)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proto: Option<String>,

    /// TTL (for ping/traceroute)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u8>,

    /// Packet size (for ping/traceroute)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u16>,

    /// Protocol-specific result data
    pub data: MeasurementData,
}

/// Protocol-specific measurement data
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MeasurementData {
    /// Placeholder for actual measurement results
    /// Will be expanded in the measurements crate
    Generic(serde_json::Value),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_id_serde() {
        let id = ProbeId(12345);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "12345");
        let parsed: ProbeId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_measurement_type_serde() {
        let mt = MeasurementType::Ping;
        let json = serde_json::to_string(&mt).unwrap();
        assert_eq!(json, "\"ping\"");
    }
}
