//! Measurement job definitions and conversion from telnet commands
//!
//! This module bridges the gap between telnet command specifications
//! and the actual measurement implementations.

use starla_common::{MeasurementId, ProbeId};
use starla_measurements::{
    dns::{DnsConfig, DnsProtocol},
    http::HttpConfig,
    ntp::NtpConfig,
    ping::PingConfig,
    tls::TlsConfig,
    traceroute::{TracerouteConfig, TracerouteProtocol},
    Dns, Http, Measurement, Ntp, Ping, Tls, Traceroute,
};
use std::net::{IpAddr, ToSocketAddrs};

/// A measurement job that can be scheduled
#[derive(Debug, Clone)]
pub struct MeasurementJob {
    /// Measurement ID
    pub msm_id: u64,
    /// Interval between measurements (0 for one-shot)
    pub interval: u64,
    /// Start time (unix timestamp, 0 = now)
    pub start_time: i64,
    /// End time (unix timestamp, 0 = never)
    pub end_time: i64,
    /// Random spread in seconds
    pub spread: u64,
    /// The measurement specification
    pub spec: MeasurementSpec,
}

/// Measurement specification that can be converted to an executable measurement
#[derive(Debug, Clone)]
pub enum MeasurementSpec {
    Ping(PingJobSpec),
    Traceroute(TracerouteJobSpec),
    Dns(DnsJobSpec),
    Http(HttpJobSpec),
    Tls(TlsJobSpec),
    Ntp(NtpJobSpec),
}

#[derive(Debug, Clone)]
pub struct PingJobSpec {
    pub target: String,
    pub af: u8,
    pub packets: u32,
    pub size: u16,
}

#[derive(Debug, Clone)]
pub struct TracerouteJobSpec {
    pub target: String,
    pub af: u8,
    pub protocol: String,
    pub paris: u32,
    pub first_hop: u8,
    pub max_hops: u8,
    pub size: u16,
}

#[derive(Debug, Clone)]
pub struct DnsJobSpec {
    pub target: String,
    pub af: u8,
    pub protocol: String,
    pub query_type: String,
    pub query_class: String,
    pub query_argument: String,
    pub use_dnssec: bool,
    pub recursion_desired: bool,
}

#[derive(Debug, Clone)]
pub struct HttpJobSpec {
    pub url: String,
    pub method: String,
    pub af: u8,
    pub headers: Vec<String>,
    pub body: Option<String>,
    pub max_body_size: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct TlsJobSpec {
    pub target: String,
    pub port: u16,
    pub af: u8,
    pub hostname: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NtpJobSpec {
    pub target: String,
    pub af: u8,
    pub packets: u32,
}

impl MeasurementJob {
    /// Convert this job to an executable measurement
    pub fn to_measurement(&self, probe_id: ProbeId) -> anyhow::Result<Box<dyn Measurement>> {
        let msm_id = MeasurementId(self.msm_id);

        match &self.spec {
            MeasurementSpec::Ping(spec) => {
                let target = resolve_target(&spec.target, spec.af)?;
                Ok(Box::new(Ping {
                    config: PingConfig {
                        target,
                        count: spec.packets,
                        size: spec.size,
                        ttl: 64,
                        timeout_ms: 5000,
                    },
                    probe_id,
                    msm_id,
                }))
            }
            MeasurementSpec::Traceroute(spec) => {
                let target = resolve_target(&spec.target, spec.af)?;
                let protocol = match spec.protocol.to_uppercase().as_str() {
                    "TCP" => TracerouteProtocol::TCP,
                    "UDP" => TracerouteProtocol::UDP,
                    _ => TracerouteProtocol::ICMP,
                };
                Ok(Box::new(Traceroute {
                    config: TracerouteConfig {
                        target,
                        protocol,
                        first_hop: spec.first_hop,
                        max_hops: spec.max_hops,
                        paris: spec.paris as u16,
                        size: spec.size,
                        timeout_ms: 5000,
                    },
                    probe_id,
                    msm_id,
                }))
            }
            MeasurementSpec::Dns(spec) => {
                let target = resolve_target(&spec.target, spec.af)?;
                let protocol = match spec.protocol.to_uppercase().as_str() {
                    "TCP" => DnsProtocol::TCP,
                    _ => DnsProtocol::UDP,
                };
                Ok(Box::new(Dns {
                    config: DnsConfig {
                        target,
                        protocol,
                        query_name: spec.query_argument.clone(),
                        query_type: spec.query_type.clone(),
                        query_class: spec.query_class.clone(),
                        recursion_desired: spec.recursion_desired,
                        edns_buf_size: if spec.use_dnssec { Some(4096) } else { None },
                        dnssec: spec.use_dnssec,
                        timeout_ms: 5000,
                    },
                    probe_id,
                    msm_id,
                }))
            }
            MeasurementSpec::Http(spec) => {
                let mut headers = std::collections::HashMap::new();
                for h in &spec.headers {
                    if let Some((k, v)) = h.split_once(':') {
                        headers.insert(k.trim().to_string(), v.trim().to_string());
                    }
                }
                Ok(Box::new(Http {
                    config: HttpConfig {
                        url: spec.url.clone(),
                        method: spec.method.clone(),
                        headers: if headers.is_empty() {
                            None
                        } else {
                            Some(headers)
                        },
                        body: spec.body.clone(),
                        timeout_ms: 30000,
                        verify_ssl: true,
                        version: None,
                    },
                    probe_id,
                    msm_id,
                }))
            }
            MeasurementSpec::Tls(spec) => {
                let target = resolve_target(&spec.target, spec.af)?;
                let hostname = spec.hostname.clone().unwrap_or_else(|| spec.target.clone());
                Ok(Box::new(Tls {
                    config: TlsConfig {
                        target,
                        port: spec.port,
                        hostname,
                        timeout_ms: 10000,
                    },
                    probe_id,
                    msm_id,
                }))
            }
            MeasurementSpec::Ntp(spec) => {
                let target = resolve_target(&spec.target, spec.af)?;
                Ok(Box::new(Ntp {
                    config: NtpConfig {
                        target,
                        timeout_ms: 5000,
                        port: 123,
                    },
                    probe_id,
                    msm_id,
                }))
            }
        }
    }

    /// Check if this is a one-shot measurement (no interval)
    pub fn is_one_shot(&self) -> bool {
        self.interval == 0
    }
}

/// Resolve a hostname or IP string to an IpAddr
fn resolve_target(target: &str, af: u8) -> anyhow::Result<IpAddr> {
    // First try to parse as IP address directly
    if let Ok(ip) = target.parse::<IpAddr>() {
        return Ok(ip);
    }

    // Try DNS resolution
    let addr_str = format!("{}:0", target);
    let addrs: Vec<_> = addr_str.to_socket_addrs()?.collect();

    // Filter by address family if specified
    for addr in &addrs {
        let ip = addr.ip();
        match af {
            4 if ip.is_ipv4() => return Ok(ip),
            6 if ip.is_ipv6() => return Ok(ip),
            _ => {}
        }
    }

    // If no AF preference or no match, return first result
    addrs
        .first()
        .map(|a| a.ip())
        .ok_or_else(|| anyhow::anyhow!("Could not resolve target: {}", target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_ipv4() {
        let ip = resolve_target("8.8.8.8", 4).unwrap();
        assert!(ip.is_ipv4());
    }

    #[test]
    fn test_resolve_ipv6() {
        let ip = resolve_target("::1", 6).unwrap();
        assert!(ip.is_ipv6());
    }
}
