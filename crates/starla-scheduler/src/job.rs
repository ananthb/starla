//! Measurement job definitions and conversion from telnet commands
//!
//! This module bridges the gap between telnet command specifications
//! and the actual measurement implementations.

use starla_common::{MeasurementId, ProbeId};
use starla_measurements::{
    http::HttpConfig, ntp::NtpConfig, ping::PingConfig, tls::TlsConfig,
    traceroute::TracerouteConfig, Dns, Http, Measurement, Ntp, Ping, Tls, Traceroute,
};
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU32, Ordering};

/// evtdig's options, as the measurement takes them: the server and the
/// query, and everything else its command line says.
pub use starla_measurements::dns::DnsConfig as DnsJobSpec;

/// The probe id registration gave us, for evtdig's `$p` and `--p_probe_id`;
/// the reference looks it up each time it expands one (get_probe_id(),
/// libbb/atlas_probe.c). The scheduler is built before the probe registers,
/// so the probe id it hands out is 0; the probe records the real one here
/// before the controller session that sends any CRONLINE.
static REGISTERED_PROBE_ID: AtomicU32 = AtomicU32::new(0);

/// Record the probe id registration assigned (see REGISTERED_PROBE_ID).
pub fn set_registered_probe_id(probe_id: u32) {
    REGISTERED_PROBE_ID.store(probe_id, Ordering::Relaxed);
}

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

impl MeasurementSpec {
    pub fn type_name(&self) -> &'static str {
        match self {
            MeasurementSpec::Ping(_) => "ping",
            MeasurementSpec::Traceroute(_) => "traceroute",
            MeasurementSpec::Dns(_) => "dns",
            MeasurementSpec::Http(_) => "http",
            MeasurementSpec::Tls(_) => "tls",
            MeasurementSpec::Ntp(_) => "ntp",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PingJobSpec {
    pub target: String,
    pub af: u8,
    pub packets: u32,
    pub size: u16,
}

/// What a traceroute measurement keeps between its runs; the command
/// handler gets one per delivered line from `TracerouteInstance::for_line`.
pub use starla_measurements::traceroute::TracerouteInstance;

#[derive(Debug, Clone)]
pub struct TracerouteJobSpec {
    pub target: String,
    pub af: u8,
    pub opts: starla_common::TracerouteOptions,
    /// The measurement's slot and run count, shared by its runs.
    pub instance: std::sync::Arc<TracerouteInstance>,
}

#[derive(Debug, Clone)]
pub struct HttpJobSpec {
    pub url: String,
    pub method: String,
    pub af: u8,
    pub headers: Vec<String>,
    pub body: Option<String>,
    pub max_body_size: Option<u32>,
    pub store_headers: u32,
    pub http10: bool,
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
    pub timeout_ms: u32,
    pub gap_ms: u32,
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
                        backend: Default::default(),
                    },
                    probe_id,
                    msm_id,
                }))
            }
            MeasurementSpec::Traceroute(spec) => {
                let target = resolve_target(&spec.target, spec.af)?;
                Ok(Box::new(Traceroute {
                    config: TracerouteConfig {
                        target,
                        opts: spec.opts.clone(),
                        backend: Default::default(),
                        instance: spec.instance.clone(),
                    },
                    probe_id,
                    msm_id,
                }))
            }
            // The server is looked up (or the resolvers read) when the
            // query is made, with evtdig's address family
            MeasurementSpec::Dns(spec) => Ok(Box::new(Dns {
                config: spec.clone(),
                probe_id: dns_probe_id(probe_id),
                msm_id,
            })),
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
                        af: spec.af,
                        store_headers: spec.store_headers as usize,
                        http10: spec.http10,
                    },
                    probe_id,
                    msm_id,
                }))
            }
            // sslgetcert resolves the name itself (its ttr and dnserr), and
            // sends a server_name only for -h
            MeasurementSpec::Tls(spec) => Ok(Box::new(Tls {
                config: TlsConfig::new(&spec.target, spec.port, spec.af, spec.hostname.clone()),
                probe_id,
                msm_id,
            })),
            MeasurementSpec::Ntp(spec) => {
                let target = resolve_target(&spec.target, spec.af)?;
                Ok(Box::new(Ntp {
                    config: NtpConfig {
                        target,
                        dst_name: spec.target.clone(),
                        timeout_ms: spec.timeout_ms as u64,
                        port: 123,
                        count: spec.packets,
                        gap_ms: spec.gap_ms as u64,
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

/// The probe id `$p` stands for: the scheduler's, or while that is 0 the one
/// registration gave us.
fn dns_probe_id(probe_id: ProbeId) -> ProbeId {
    match probe_id {
        ProbeId(0) => ProbeId(REGISTERED_PROBE_ID.load(Ordering::Relaxed)),
        known => known,
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

    /// `$p` is the registered probe id, not the scheduler's 0: on the twin of
    /// 2026-10-13 Starla asked for 0.ripe-tld-experiment.<r> where the
    /// reference asked for 1018212.ripe-tld-experiment.<r> (msm 158899649).
    #[tokio::test]
    async fn a_dns_job_asks_with_the_registered_probe_id() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let job = MeasurementJob {
            msm_id: 158899649,
            interval: 0,
            start_time: 0,
            end_time: 0,
            spread: 0,
            spec: MeasurementSpec::Dns(DnsJobSpec {
                server: "127.0.0.1".into(),
                port: server.local_addr().unwrap().port(),
                qtype: 1,
                qclass: 1,
                query: "$p.ripe-tld-experiment.".into(),
                timeout_ms: 50,
                disable_check_addr: true,
                ..Default::default()
            }),
        };
        set_registered_probe_id(1018212);
        job.to_measurement(ProbeId(0))
            .unwrap()
            .execute()
            .await
            .unwrap();
        let mut q = [0u8; 512];
        let n = server.recv(&mut q).await.unwrap();
        let wanted = b"\x071018212\x13ripe-tld-experiment\x00";
        assert_eq!(&q[12..12 + wanted.len()], wanted, "{:?}", &q[..n]);
        assert_eq!(dns_probe_id(ProbeId(7)), ProbeId(7));
    }
}
