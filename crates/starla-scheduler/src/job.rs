//! Measurement job definitions and conversion from telnet commands
//!
//! This module bridges the gap between telnet command specifications
//! and the actual measurement implementations.

use async_trait::async_trait;
use starla_common::{MeasurementId, MeasurementResult, MeasurementType, ProbeId};
use starla_measurements::{
    http::HttpConfig,
    ntp::NtpConfig,
    ping::PingConfig,
    target::{self, DstName, Resolver},
    tls::TlsConfig,
    traceroute::{Run as TracerouteRun, TracerouteConfig},
    Dns, Http, Measurement, Ntp, Ping, Tls, Traceroute,
};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
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
    /// `-i`: ms from one echo request to the next, each one's time to
    /// answer (ping.c:44, 785-787, 800-812)
    pub packet_interval: u32,
    pub size: u16,
    /// What the measurement's runs leave for the next: one per delivered
    /// line, from `PingKept::for_line`.
    pub kept: std::sync::Arc<PingKept>,
}

/// What a ping measurement keeps between its runs, and the line it is
/// kept for.
pub use starla_measurements::ping::{PingKept, PingLine, PingRun};

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

impl MeasurementSpec {
    /// The target ping and traceroute look up when they run, and the family
    /// -4/-6 asked for. None for the other kinds, which are built with their
    /// target resolved as before (resolve_target), or look it up themselves.
    fn target(&self) -> Option<(&str, u8)> {
        match self {
            MeasurementSpec::Ping(s) => Some((&s.target, s.af)),
            MeasurementSpec::Traceroute(s) => Some((&s.target, s.af)),
            _ => None,
        }
    }

    /// ping_start's and traceroute_start's busy check, which comes before
    /// the lookup (ping.c:1751-1752, 1787-1798; traceroute.c:4934-4938,
    /// 4940-4971): a line whose last run is still under way does not start,
    /// and reports nothing — not even a failed lookup. The flag is the
    /// line's own (PingKept, TracerouteInstance), taken here once; the run
    /// holds it until its line is reported, and a traceroute's run is
    /// handed to the measurement ([`Traceroute::execute_run`]) instead of
    /// the measurement taking it again.
    fn start(&self, msm_id: u64) -> anyhow::Result<Option<LineRun<'_>>> {
        match self {
            MeasurementSpec::Ping(spec) => match spec.kept.start() {
                Some(run) => Ok(Some(LineRun::Ping(run))),
                None => anyhow::bail!("ping {msm_id}: busy, not started"),
            },
            MeasurementSpec::Traceroute(spec) => match spec.instance.start() {
                Some(run) => Ok(Some(LineRun::Traceroute(run))),
                None => anyhow::bail!("traceroute {msm_id}: busy, can't start"),
            },
            _ => Ok(None),
        }
    }

    fn measurement_type(&self) -> MeasurementType {
        match self {
            MeasurementSpec::Ping(_) => MeasurementType::Ping,
            MeasurementSpec::Traceroute(_) => MeasurementType::Traceroute,
            MeasurementSpec::Dns(_) => MeasurementType::Dns,
            MeasurementSpec::Http(_) => MeasurementType::Http,
            MeasurementSpec::Tls(_) => MeasurementType::Tls,
            MeasurementSpec::Ntp(_) => MeasurementType::Ntp,
        }
    }
}

impl MeasurementJob {
    /// Convert this job to an executable measurement. A ping's or a
    /// traceroute's target is looked up each time it runs, as eperd looks
    /// one up at every start (ping.c:1746-1800, traceroute.c:4940-4971),
    /// with [`starla_measurements::target`].
    pub fn to_measurement(&self, probe_id: ProbeId) -> anyhow::Result<Box<dyn Measurement>> {
        if self.spec.target().is_none() {
            return self.build(probe_id, None);
        }
        Ok(Box::new(LookUpThenRun {
            job: self.clone(),
            probe_id,
            resolver: None,
        }))
    }

    /// The measurement, once its target is looked up: the address to
    /// measure, and the name results report it by.
    fn build(
        &self,
        probe_id: ProbeId,
        looked_up: Option<(IpAddr, DstName)>,
    ) -> anyhow::Result<Box<dyn Measurement>> {
        let msm_id = MeasurementId(self.msm_id);
        let no_target = || anyhow::anyhow!("msm {}: no target looked up", self.msm_id);

        match &self.spec {
            MeasurementSpec::Ping(spec) => {
                let (target, dst_name) = looked_up.ok_or_else(no_target)?;
                // ping_init takes no measurement with an interval outside
                // 1..=60000 ms (ping.c:1280-1284)
                if !(1..=60_000).contains(&spec.packet_interval) {
                    anyhow::bail!("msm {}: bad interval", self.msm_id);
                }
                Ok(Box::new(Ping {
                    config: PingConfig {
                        target,
                        count: spec.packets,
                        size: spec.size,
                        ttl: 64,
                        interval_ms: u64::from(spec.packet_interval),
                        backend: Default::default(),
                        kept: spec.kept.clone(),
                    },
                    probe_id,
                    msm_id,
                    dst_name,
                }))
            }
            MeasurementSpec::Traceroute(spec) => {
                let (target, dst_name) = looked_up.ok_or_else(no_target)?;
                Ok(Box::new(self.traceroute(spec, probe_id, target, dst_name)))
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

    /// The traceroute of `spec`, its target looked up: the run of the
    /// line's instance, which shares its slot and Paris id.
    fn traceroute(
        &self,
        spec: &TracerouteJobSpec,
        probe_id: ProbeId,
        target: IpAddr,
        dst_name: DstName,
    ) -> Traceroute {
        Traceroute {
            config: TracerouteConfig {
                target,
                opts: spec.opts.clone(),
                backend: Default::default(),
                instance: spec.instance.clone(),
            },
            probe_id,
            msm_id: MeasurementId(self.msm_id),
            dst_name,
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

/// A run of a ping or traceroute line, from its busy check to its report:
/// report() clears the flag (ping.c:324, traceroute.c:454) when this goes.
enum LineRun<'a> {
    /// Held for its drop only: the ping measurement keeps no run.
    Ping(#[allow(dead_code)] PingRun<'a>),
    /// Handed to the traceroute, whose run gets it under way (its Paris id).
    Traceroute(TracerouteRun<'a>),
}

/// A ping or traceroute that looks its target up when it runs, then runs.
/// When the lookup fails it reports that as the reference does
/// ([`Measurement::lookup_failed`]: ping.c's PING_ERR_DNS line,
/// traceroute.c's "name resolution failed" one). A line still busy with
/// its last run does neither ([`MeasurementSpec::start`]).
struct LookUpThenRun {
    job: MeasurementJob,
    probe_id: ProbeId,
    /// None: the probe's own, from resolv.conf and hosts as they are when
    /// the job runs.
    resolver: Option<Resolver>,
}

#[async_trait]
impl Measurement for LookUpThenRun {
    fn measurement_type(&self) -> MeasurementType {
        self.job.spec.measurement_type()
    }

    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        let Some((target, af)) = self.job.spec.target() else {
            return self.job.build(self.probe_id, None)?.execute().await;
        };
        // Held until the line is reported, whatever the run comes to.
        let run = self.job.spec.start(self.job.msm_id)?;
        let lookup = match &self.resolver {
            Some(resolver) => resolver.lookup(target, af).await,
            None => target::lookup(target, af).await,
        };
        match lookup.addr {
            // an address eperd does not measure is reported, not measured
            // (ping.c:1706-1720, traceroute.c:4886-4899)
            Ok(addr) if !target::address_allowed(addr) => self
                .job
                .build(self.probe_id, Some((addr, lookup.dst_name)))?
                .address_not_allowed()
                .ok_or_else(|| anyhow::anyhow!("{target}: address {addr} not allowed")),
            Ok(addr) => match (&run, &self.job.spec) {
                // the slot taken above is the run's: execute() would take
                // it again, and find the line busy
                (Some(LineRun::Traceroute(run)), MeasurementSpec::Traceroute(spec)) => {
                    self.job
                        .traceroute(spec, self.probe_id, addr, lookup.dst_name)
                        .execute_run(run)
                        .await
                }
                _ => {
                    self.job
                        .build(self.probe_id, Some((addr, lookup.dst_name)))?
                        .execute()
                        .await
                }
            },
            Err(error) => {
                // The measurement as it would have run, with only the family
                // asked for in its address, writes the failure's line.
                let family: IpAddr = if af == 4 {
                    Ipv4Addr::UNSPECIFIED.into()
                } else {
                    Ipv6Addr::UNSPECIFIED.into()
                };
                self.job
                    .build(self.probe_id, Some((family, lookup.dst_name)))?
                    .lookup_failed(error)
                    .ok_or_else(|| anyhow::anyhow!("could not resolve {target}: {error}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starla_common::MeasurementData;
    use starla_controller::telnet::{parse_command, TelnetCommand};
    use starla_measurements::target::ResolvConf;
    use std::time::Duration;
    use tokio::net::UdpSocket;

    fn job(spec: MeasurementSpec) -> MeasurementJob {
        MeasurementJob {
            msm_id: 1040114,
            interval: 21600,
            start_time: 0,
            end_time: 0,
            spread: 0,
            spec,
        }
    }

    fn ping(target: &str) -> MeasurementSpec {
        MeasurementSpec::Ping(PingJobSpec {
            target: target.into(),
            af: 6,
            packets: 16,
            packet_interval: 1000,
            size: 0,
            kept: Default::default(),
        })
    }

    /// A resolver that answers every query with NXDOMAIN, the question
    /// echoed.
    async fn nxdomain_resolver() -> Resolver {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                buf[2] |= 0x80; // QR
                buf[3] = (buf[3] & 0xf0) | 3; // NXDOMAIN
                socket.send_to(&buf[..n], from).await.unwrap();
            }
        });
        Resolver::new(
            ResolvConf {
                nameservers: vec![addr],
                timeout: Duration::from_millis(500),
                ..Default::default()
            },
            Vec::new(),
        )
    }

    /// `evping -6 -c 16 -A 1040114 <a name that does not exist>`: one
    /// result, ping.c's PING_ERR_DNS line, where Starla logged an error
    /// and reported nothing.
    #[tokio::test]
    async fn a_ping_whose_lookup_fails_reports_it() {
        let run = LookUpThenRun {
            job: job(ping("nonexistent.example")),
            probe_id: ProbeId(1018212),
            resolver: Some(nxdomain_resolver().await),
        };
        assert_eq!(run.measurement_type(), MeasurementType::Ping);
        let result = run.execute().await.unwrap();
        let MeasurementData::FullLine(body) = result.data else {
            panic!("{:?}", result.data);
        };
        let (head, tail) = body.split_once(r#", "ttr":"#).unwrap();
        assert!(
            head.ends_with(r#""dst_name":"nonexistent.example""#),
            "{body}"
        );
        assert!(
            tail.ends_with(
                r#", "af":6, "proto":"ICMP", "size":0, "result": [ { "error":"dns resolution failed: nodename nor servname provided, or not known" } ]"#
            ),
            "{body}"
        );
    }

    /// The job the probe schedules for a CRONLINE the controller gives:
    /// parse_command's spec, field for field as main.rs maps it.
    fn traceroute_line(line: &str) -> MeasurementJob {
        let TelnetCommand::Traceroute(spec) = parse_command(line) else {
            panic!("{line}");
        };
        MeasurementJob {
            msm_id: spec.msm_id,
            interval: spec.schedule.interval,
            start_time: spec.schedule.start_time,
            end_time: spec.schedule.stop_time,
            spread: spec.spread.unwrap_or(0) as u64,
            spec: MeasurementSpec::Traceroute(TracerouteJobSpec {
                instance: TracerouteInstance::for_line(
                    spec.msm_id,
                    &spec.target,
                    spec.af,
                    &spec.opts,
                    spec.schedule.interval,
                    spec.schedule.start_time,
                ),
                target: spec.target,
                af: spec.af,
                opts: spec.opts,
            }),
        }
    }

    /// The tail of the line `job` reports when its name does not resolve.
    async fn nxdomain_line(job: MeasurementJob) -> String {
        let run = LookUpThenRun {
            job,
            probe_id: ProbeId(1018212),
            resolver: Some(nxdomain_resolver().await),
        };
        let result = run.execute().await.unwrap();
        assert_eq!(result.measurement_type, MeasurementType::Traceroute);
        let MeasurementData::FullLine(body) = result.data else {
            panic!("{:?}", result.data);
        };
        body
    }

    /// RIPE's built-in 6152, as the controller sends it (the twin of
    /// 2026-10-13), when topology6.dyndns.atlas.ripe.net does not resolve:
    /// traceroute.c's dns_cb line (traceroute.c:4822-4838), where Starla
    /// logged an error and reported nothing. No -a: parismod is 16, so the
    /// line has the paris_id, parisbase's 0 before any run
    /// (traceroute.c:4247-4248, 4408, 428-431).
    #[tokio::test]
    async fn a_traceroute_whose_lookup_fails_reports_it() {
        let body = nxdomain_line(traceroute_line(
            r#"CRONLINE 900 2833 1794447121 UNIFORM 3 evtraceroute -6 -I -c 3 -w 4000 -A "6152" -O /home/atlas/data/new/7 topology6.dyndns.atlas.ripe.net"#,
        ))
        .await;
        let (head, tail) = body.split_once(r#", "ttr":"#).unwrap();
        assert!(
            head.starts_with(r#""id":"6152", "fw":5130, "mver": "2.6.4", "lts":0, "time":"#),
            "{body}"
        );
        assert!(
            head.ends_with(r#""dst_name":"topology6.dyndns.atlas.ripe.net""#),
            "{body}"
        );
        assert!(
            tail.ends_with(
                r#", "proto":"ICMP", "af": 6, "size":40, "paris_id":0, "result": [ { "error":"name resolution failed: nodename nor servname provided, or not known" } ]"#
            ),
            "{body}"
        );
    }

    /// `-a 0` is classic traceroute, whose lines have no paris_id; -p is
    /// the destination port, which Starla took for the paris_id (RIPE
    /// stored "paris_id":80, starla-25603345.json).
    #[tokio::test]
    async fn without_paris_a_failed_lookup_has_no_paris_id() {
        let body = nxdomain_line(traceroute_line(
            r#"CRONLINE 900 1 2000000000 UNIFORM 3 evtraceroute -6 -I -a 0 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "25603345" -O /home/atlas/data/new/7 dyna.wikimedia.org"#,
        ))
        .await;
        assert!(
            body.ends_with(
                r#", "proto":"ICMP", "af": 6, "size":48, "result": [ { "error":"name resolution failed: nodename nor servname provided, or not known" } ]"#
            ),
            "{body}"
        );
        // -a 16 … -p 80, the twin's 25603345 itself: paris_id 0, not 80
        let body = nxdomain_line(traceroute_line(
            r#"CRONLINE 900 1 2000000000 UNIFORM 3 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "25603345" -O /home/atlas/data/new/7 dyna.wikimedia.org"#,
        ))
        .await;
        assert!(
            body.ends_with(
                r#", "size":48, "paris_id":0, "result": [ { "error":"name resolution failed: nodename nor servname provided, or not known" } ]"#
            ),
            "{body}"
        );
    }

    /// A name the hosts file has, but not in the family asked for, is an
    /// empty answer: ping.c reports nothing for it (PING_ERR_DNS_NO_ADDR).
    #[tokio::test]
    async fn an_empty_answer_reports_nothing() {
        let mut resolver = nxdomain_resolver().await;
        resolver.hosts = vec![("v4only.lan".into(), "192.0.2.1".parse().unwrap())];
        let run = LookUpThenRun {
            job: job(ping("v4only.lan")),
            probe_id: ProbeId(1018212),
            resolver: Some(resolver),
        };
        let err = run.execute().await.unwrap_err().to_string();
        assert!(err.contains("no address associated"), "{err}");
    }

    /// An address atlas_check_addr refuses, looked up or given, is
    /// reported, not measured: ping.c's PING_ERR_BAD_ADDR line with the
    /// address, and traceroute.c's (ping.c:1706-1720,
    /// traceroute.c:4886-4899). Starla pinged it.
    #[tokio::test]
    async fn a_refused_address_is_reported_not_measured() {
        let mut resolver = nxdomain_resolver().await;
        resolver.hosts = vec![("lan.example".into(), "fd00::20".parse().unwrap())];
        let run = LookUpThenRun {
            job: job(ping("lan.example")),
            probe_id: ProbeId(1018212),
            resolver: Some(resolver.clone()),
        };
        let result = run.execute().await.unwrap();
        let MeasurementData::FullLine(body) = result.data else {
            panic!("{:?}", result.data);
        };
        let (head, tail) = body.split_once(r#", "ttr":"#).unwrap();
        assert!(head.ends_with(r#""dst_name":"lan.example""#), "{body}");
        assert!(
            tail.ends_with(
                r#", "af":6, "dst_addr":"fd00::20", "proto":"ICMP", "size":0, "result": [ { "error":"address not allowed" } ]"#
            ),
            "{body}"
        );

        // a literal, -4, a traceroute: its paris_id is -b's, parisbase
        for (line, paris_id) in [
            (
                r#"CRONLINE 1800 331 1925935851 UNIFORM 3 evtraceroute -4 -U -c 3 -w 1000 -A "5001" -O /home/atlas/data/new/7 192.168.1.1"#,
                r#", "paris_id":0"#,
            ),
            (
                r#"CRONLINE 1800 331 1925935851 UNIFORM 3 evtraceroute -4 -U -a 4 -b 3 -A "5001" 192.168.1.1"#,
                r#", "paris_id":3"#,
            ),
            (
                r#"CRONLINE 1800 331 1925935851 UNIFORM 3 evtraceroute -4 -U -a 0 -b 3 -A "5001" 192.168.1.1"#,
                "",
            ),
        ] {
            let run = LookUpThenRun {
                job: traceroute_line(line),
                probe_id: ProbeId(1018212),
                resolver: Some(resolver.clone()),
            };
            let result = run.execute().await.unwrap();
            let MeasurementData::FullLine(body) = result.data else {
                panic!("{:?}", result.data);
            };
            assert!(
                body.ends_with(&format!(
                    r#""dst_name":"192.168.1.1", "dst_addr":"192.168.1.1", "proto":"UDP", "af": 4, "size":40{paris_id}, "result": [ {{ "error":"address not allowed" }} ]"#
                )),
                "{body}"
            );
        }
    }

    /// A resolver that counts the queries it gets, and answers none.
    async fn counting_resolver() -> (Resolver, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let queries = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = queries.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while socket.recv_from(&mut buf).await.is_ok() {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let resolver = Resolver::new(
            ResolvConf {
                nameservers: vec![addr],
                timeout: Duration::from_millis(100),
                attempts: 1,
                ..Default::default()
            },
            Vec::new(),
        );
        (resolver, queries)
    }

    /// While a run of a ping line is under way the next one does not start:
    /// no lookup, no line, not even the failed lookup's (ping_start returns
    /// before its lookup while busy, ping.c:1751-1752). Starla started every
    /// due run, so a ping that outlasted its gap reported twice.
    #[tokio::test]
    async fn a_busy_ping_line_does_not_look_up_or_report() {
        let (resolver, queries) = counting_resolver().await;
        let job = job(ping("nonexistent.example"));
        let MeasurementSpec::Ping(spec) = &job.spec else {
            unreachable!()
        };
        let kept = spec.kept.clone();
        let run = LookUpThenRun {
            job: job.clone(),
            probe_id: ProbeId(1018212),
            resolver: Some(resolver),
        };
        let under_way = kept.start().unwrap();
        let err = run.execute().await.unwrap_err().to_string();
        assert_eq!(err, "ping 1040114: busy, not started");
        assert_eq!(queries.load(std::sync::atomic::Ordering::SeqCst), 0);
        // once it is reported, the next run goes ahead: its lookup times out
        drop(under_way);
        let result = run.execute().await.unwrap();
        assert!(queries.load(std::sync::atomic::Ordering::SeqCst) > 0);
        let MeasurementData::FullLine(body) = result.data else {
            panic!("{:?}", result.data);
        };
        assert!(body.contains("dns resolution failed"), "{body}");
        // and the line is free again
        assert!(kept.start().is_some());
    }

    /// After an empty answer the reference's line stays busy for good —
    /// ping_cb has no case for PING_ERR_DNS_NO_ADDR, so report() never
    /// clears busy (ping.c:1733-1744, 1751-1752) — and reports nothing
    /// again, though its name now resolves to an address it would measure.
    #[tokio::test]
    async fn after_an_empty_answer_a_ping_line_reports_nothing_again() {
        let mut resolver = nxdomain_resolver().await;
        resolver.hosts = vec![("v4only.lan".into(), "192.0.2.1".parse().unwrap())];
        let job = job(ping("v4only.lan"));
        let mut run = LookUpThenRun {
            job,
            probe_id: ProbeId(1018212),
            resolver: Some(resolver.clone()),
        };
        assert!(run.execute().await.is_err());
        resolver.hosts = Vec::new();
        run.resolver = Some(resolver);
        let err = run.execute().await.unwrap_err().to_string();
        assert_eq!(err, "ping 1040114: busy, not started");
    }

    /// traceroute_start checks its busy flag before its lookup too
    /// (traceroute.c:4934-4938): while a run of the line is under way the
    /// next one neither looks its name up nor reports, and its Paris id
    /// does not step. The flag MeasurementSpec::start takes is the line's
    /// TracerouteInstance, and it is the one the run holds: through the
    /// lookup, and through the trace itself, whose measurement does not
    /// take it again (it would find the line busy) and steps the Paris id
    /// once. Reported, the line is free again.
    #[tokio::test]
    async fn a_busy_traceroute_line_does_not_look_up_or_report() {
        let job = traceroute_line(
            r#"CRONLINE 900 1 2000000000 UNIFORM 3 evtraceroute -4 -I -c 1 -m 1 -w 200 -A "7001" nonexistent.example"#,
        );
        let MeasurementSpec::Traceroute(spec) = &job.spec else {
            unreachable!()
        };
        let instance = spec.instance.clone();

        // An NXDOMAIN resolver that notes, for each query, whether the
        // line was busy when it came.
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server = socket.local_addr().unwrap();
        let busy_at_query = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (line, noted) = (instance.clone(), busy_at_query.clone());
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                noted.lock().unwrap().push(line.start().is_none());
                buf[2] |= 0x80; // QR
                buf[3] = (buf[3] & 0xf0) | 3; // NXDOMAIN
                socket.send_to(&buf[..n], from).await.unwrap();
            }
        });
        let mut resolver = Resolver::new(
            ResolvConf {
                nameservers: vec![server],
                timeout: Duration::from_millis(500),
                ..Default::default()
            },
            Vec::new(),
        );
        let run = LookUpThenRun {
            job,
            probe_id: ProbeId(1018212),
            resolver: Some(resolver.clone()),
        };

        let under_way = instance.start().unwrap();
        let err = run.execute().await.unwrap_err().to_string();
        assert_eq!(err, "traceroute 7001: busy, can't start");
        assert!(busy_at_query.lock().unwrap().is_empty(), "no query sent");
        assert_eq!(instance.runs(), 0, "the refused run stepped nothing");
        drop(under_way);

        // Once the run is reported, the next looks its name up, the line
        // busy with it all the while, and reports dns_cb's line: no run got
        // under way, so the paris_id is still parisbase's.
        let result = run.execute().await.unwrap();
        assert_eq!(*busy_at_query.lock().unwrap(), [true]);
        let MeasurementData::FullLine(body) = result.data else {
            panic!("{:?}", result.data);
        };
        assert!(
            body.ends_with(
                r#", "proto":"ICMP", "af": 4, "size":40, "paris_id":0, "result": [ { "error":"name resolution failed: nodename nor servname provided, or not known" } ]"#
            ),
            "{body}"
        );
        assert_eq!(instance.runs(), 0);
        assert!(instance.start().is_some(), "free again");

        // A run that gets under way holds the slot start() took, once: its
        // own measurement does not refuse it as busy, the run is the
        // line's first (paris_id 1), and the line is free once reported.
        // 0.0.0.0 is an address atlas_check_addr lets through that leaves
        // no packet off this host.
        resolver.hosts = vec![("local.example".into(), "0.0.0.0".parse().unwrap())];
        let job = traceroute_line(
            r#"CRONLINE 900 1 2000000000 UNIFORM 3 evtraceroute -4 -I -c 1 -m 1 -w 200 -A "7002" local.example"#,
        );
        let MeasurementSpec::Traceroute(spec) = &job.spec else {
            unreachable!()
        };
        let instance = spec.instance.clone();
        let run = LookUpThenRun {
            job,
            probe_id: ProbeId(1018212),
            resolver: Some(resolver),
        };
        let result = run.execute().await.unwrap();
        let MeasurementData::FullLine(body) = result.data else {
            panic!("{:?}", result.data);
        };
        assert!(
            body.contains(r#""dst_name":"local.example", "ttr":"#),
            "{body}"
        );
        assert!(body.contains(r#""dst_addr":"0.0.0.0""#), "{body}");
        assert!(
            body.contains(
                r#", "proto":"ICMP", "af": 4, "size":40, "paris_id":1, "result": [ { "hop":1"#
            ),
            "{body}"
        );
        assert_eq!(instance.runs(), 1);
        assert!(instance.start().is_some(), "free again");
    }

    /// `-i` outside 1..=60000: ping_init takes no such measurement
    /// (ping.c:1280-1284), so it reports nothing.
    #[test]
    fn a_bad_interval_is_no_measurement() {
        for interval in [0, 60_001] {
            let mut spec = ping("2001:7fd::1");
            let MeasurementSpec::Ping(p) = &mut spec else {
                unreachable!()
            };
            p.packet_interval = interval;
            let looked_up = Some((
                "2001:7fd::1".parse().unwrap(),
                DstName {
                    name: "2001:7fd::1".into(),
                    ttr: None,
                },
            ));
            assert!(
                job(spec).build(ProbeId(1), looked_up).is_err(),
                "{interval}"
            );
        }
        let looked_up = Some((
            "2001:7fd::1".parse().unwrap(),
            DstName {
                name: "2001:7fd::1".into(),
                ttr: None,
            },
        ));
        assert!(job(ping("2001:7fd::1"))
            .build(ProbeId(1), looked_up)
            .is_ok());
    }

    /// Only ping and traceroute look their target up here. DNS and TLS
    /// resolve on their own, with their own ttr and errors (evtdig's resolver,
    /// tls/resolve.rs), and NTP through resolve_target.
    #[test]
    fn only_ping_and_traceroute_are_looked_up_first() {
        let dns = MeasurementSpec::Dns(DnsJobSpec {
            server: "k.root-servers.net".into(),
            ..Default::default()
        });
        let tls = MeasurementSpec::Tls(TlsJobSpec {
            target: "www.ripe.net".into(),
            port: 443,
            af: 6,
            hostname: None,
        });
        let ntp = MeasurementSpec::Ntp(NtpJobSpec {
            target: "pool.ntp.org".into(),
            af: 6,
            packets: 3,
            timeout_ms: 4000,
            gap_ms: 0,
        });
        for spec in [dns, tls, ntp] {
            assert_eq!(spec.target(), None, "{}", spec.type_name());
        }
        assert_eq!(
            ping("bits.wikimedia.org").target(),
            Some(("bits.wikimedia.org", 6))
        );
    }

    #[test]
    fn the_wrapper_keeps_the_kind() {
        for (spec, kind) in [
            (ping("bits.wikimedia.org"), MeasurementType::Ping),
            (
                MeasurementSpec::Http(HttpJobSpec {
                    url: "http://ip-echo.ripe.net/".into(),
                    method: "GET".into(),
                    af: 6,
                    headers: Vec::new(),
                    body: None,
                    max_body_size: None,
                    store_headers: 1024,
                    http10: false,
                }),
                MeasurementType::Http,
            ),
        ] {
            let m = job(spec).to_measurement(ProbeId(1)).unwrap();
            assert_eq!(m.measurement_type(), kind);
        }
    }

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
