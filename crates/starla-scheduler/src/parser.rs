//! Command parser for measurement specifications

use crate::task::ScheduledTask;
use starla_measurements::{
    Ping, PingConfig,
    Traceroute, TracerouteConfig, TracerouteProtocol,
    Dns, DnsConfig, DnsProtocol,
    Http, HttpConfig,
    Ntp, NtpConfig,
    Tls, TlsConfig
};
use starla_common::{ProbeId, MeasurementId};
use clap::{Command, Arg, value_parser};
use std::net::IpAddr;
use std::str::FromStr;

pub struct CommandParser;

impl CommandParser {
    pub fn parse(
        cmd_line: &str,
        probe_id: ProbeId,
        msm_id: MeasurementId
    ) -> anyhow::Result<ScheduledTask> {
        let parts: Vec<&str> = cmd_line.split_whitespace().collect();

        if parts.len() < 5 {
            anyhow::bail!("Invalid command line: too few arguments");
        }

        // Parse scheduling parameters
        let interval: u64 = parts[0].parse()?;
        let start_time: i64 = parts[1].parse()?;
        let end_time: i64 = parts[2].parse()?;
        let spread: u64 = parts[3].parse()?;

        // The rest is the measurement command
        let command_parts = &parts[4..];
        let program = command_parts[0];

        // We need to prepend a dummy executable name for clap
        let mut clap_args = vec!["starla-measurement"];
        clap_args.extend_from_slice(command_parts);

        let measurement = match program {
            "ping" | "ping6" => parse_ping(clap_args, probe_id, msm_id)?,
            "traceroute" | "traceroute6" => parse_traceroute(clap_args, probe_id, msm_id)?,
            "dig" => parse_dns(clap_args, probe_id, msm_id)?, // Atlas uses 'dig' for DNS
            "http" => parse_http(clap_args, probe_id, msm_id)?,
            "ntp" => parse_ntp(clap_args, probe_id, msm_id)?,
            "sslcert" => parse_tls(clap_args, probe_id, msm_id)?,
            _ => anyhow::bail!("Unknown measurement type: {}", program),
        };

        Ok(ScheduledTask::new(
            msm_id.0, // Use measurement ID as task ID
            interval,
            start_time,
            end_time,
            spread,
            measurement
        ))
    }
}

fn parse_ping(args: Vec<&str>, probe_id: ProbeId, msm_id: MeasurementId) -> anyhow::Result<Box<dyn starla_measurements::Measurement>> {
    let matches = Command::new("ping")
        .no_binary_name(true)
        .disable_help_flag(true)
        .arg(Arg::new("ipv4").short('4').action(clap::ArgAction::SetTrue))
        .arg(Arg::new("ipv6").short('6').action(clap::ArgAction::SetTrue))
        .arg(Arg::new("count").short('c').value_parser(value_parser!(u32)).default_value("3"))
        .arg(Arg::new("size").short('s').value_parser(value_parser!(u16)).default_value("56"))
        .arg(Arg::new("ttl").short('t').value_parser(value_parser!(u8)).default_value("64"))
        .arg(Arg::new("timeout").short('W').value_parser(value_parser!(u64)).default_value("1000"))
        .arg(Arg::new("target").required(true))
        .try_get_matches_from(args)?;

    let target_str: &String = matches.get_one("target").unwrap();
    // Resolve target IP? The probe usually receives an IP or hostname.
    // Ideally we should resolve here if it's a hostname, or let the measurement resolve.
    // Since our measurement struct takes IpAddr, we resolve here.
    // Note: blocking resolution in parser might be bad if we parse many.
    // For now, assume it's an IP or simple hostname.
    // In production, the controller usually sends IPs for most measurements except DNS/HTTP.

    // Simple resolution fallback
    let target = if let Ok(ip) = target_str.parse::<IpAddr>() {
        ip
    } else {
        // Fallback to DNS resolution (synchronous here, but parser runs in task)
        // TODO: Use async resolver or proper error handling
        use std::net::ToSocketAddrs;
        let addrs = format!("{}:0", target_str).to_socket_addrs()?;
        addrs.into_iter().next().ok_or_else(|| anyhow::anyhow!("Could not resolve target"))?.ip()
    };

    let config = PingConfig {
        target,
        count: *matches.get_one("count").unwrap(),
        size: *matches.get_one("size").unwrap(),
        ttl: *matches.get_one("ttl").unwrap(),
        timeout_ms: *matches.get_one("timeout").unwrap(),
    };

    Ok(Box::new(Ping {
        config,
        probe_id,
        msm_id,
    }))
}

fn parse_traceroute(args: Vec<&str>, probe_id: ProbeId, msm_id: MeasurementId) -> anyhow::Result<Box<dyn starla_measurements::Measurement>> {
    let matches = Command::new("traceroute")
        .no_binary_name(true)
        .disable_help_flag(true)
        .arg(Arg::new("ipv4").short('4').action(clap::ArgAction::SetTrue))
        .arg(Arg::new("ipv6").short('6').action(clap::ArgAction::SetTrue))
        .arg(Arg::new("icmp").short('I').action(clap::ArgAction::SetTrue))
        .arg(Arg::new("udp").short('U').action(clap::ArgAction::SetTrue))
        .arg(Arg::new("tcp").short('T').action(clap::ArgAction::SetTrue))
        .arg(Arg::new("first_hop").short('f').value_parser(value_parser!(u8)).default_value("1"))
        .arg(Arg::new("max_hops").short('m').value_parser(value_parser!(u8)).default_value("30"))
        .arg(Arg::new("size").long("size").value_parser(value_parser!(u16)).default_value("64")) // Non-standard flag maybe
        .arg(Arg::new("timeout").short('w').value_parser(value_parser!(u64)).default_value("1000")) // traceroute uses seconds usually, but we use ms
        .arg(Arg::new("target").required(true))
        .try_get_matches_from(args)?;

    let target_str: &String = matches.get_one("target").unwrap();
    let target = target_str.parse::<IpAddr>()
        .or_else(|_| {
            use std::net::ToSocketAddrs;
            let addrs = format!("{}:0", target_str).to_socket_addrs()?;
            Ok::<IpAddr, anyhow::Error>(addrs.into_iter().next().unwrap().ip())
        })?;

    let protocol = if matches.get_flag("icmp") {
        TracerouteProtocol::ICMP
    } else if matches.get_flag("tcp") {
        TracerouteProtocol::TCP
    } else {
        TracerouteProtocol::UDP
    };

    let config = TracerouteConfig {
        target,
        protocol,
        first_hop: *matches.get_one("first_hop").unwrap(),
        max_hops: *matches.get_one("max_hops").unwrap(),
        paris: 0,
        size: *matches.get_one("size").unwrap(),
        timeout_ms: *matches.get_one("timeout").unwrap(),
    };

    Ok(Box::new(Traceroute {
        config,
        probe_id,
        msm_id,
    }))
}

// TODO: Implement other parsers (DNS, HTTP, NTP, TLS)
fn parse_dns(_args: Vec<&str>, _probe_id: ProbeId, _msm_id: MeasurementId) -> anyhow::Result<Box<dyn starla_measurements::Measurement>> {
    anyhow::bail!("DNS parsing not implemented")
}

fn parse_http(_args: Vec<&str>, _probe_id: ProbeId, _msm_id: MeasurementId) -> anyhow::Result<Box<dyn starla_measurements::Measurement>> {
    anyhow::bail!("HTTP parsing not implemented")
}

fn parse_ntp(_args: Vec<&str>, _probe_id: ProbeId, _msm_id: MeasurementId) -> anyhow::Result<Box<dyn starla_measurements::Measurement>> {
    anyhow::bail!("NTP parsing not implemented")
}

fn parse_tls(_args: Vec<&str>, _probe_id: ProbeId, _msm_id: MeasurementId) -> anyhow::Result<Box<dyn starla_measurements::Measurement>> {
    anyhow::bail!("TLS parsing not implemented")
}
