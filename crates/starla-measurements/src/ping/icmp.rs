//! ICMP Ping execution logic: ping.c's sockets, as Starla's own.

use super::{data_len, PingConfig, PingOutcome, Session};
use std::time::Duration;

/// What every echo request carries at the start of its data, as ping.c's
/// struct evdata carries its send time and the run's random cookie
/// (ping.c:186-190, 562-571): a reply is the run's when it echoes the
/// cookie, and its rtt runs from the send time it echoes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    cookie: u64,
    /// When the request went out, from the start of the run
    sent: Duration,
}

impl Stamp {
    fn write(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&self.cookie.to_be_bytes());
        let nanos = u64::try_from(self.sent.as_nanos()).unwrap_or(u64::MAX);
        data[8..16].copy_from_slice(&nanos.to_be_bytes());
    }

    fn read(data: &[u8]) -> Option<Self> {
        let cookie = u64::from_be_bytes(data.get(..8)?.try_into().ok()?);
        let nanos = u64::from_be_bytes(data.get(8..16)?.try_into().ok()?);
        Some(Self {
            cookie,
            sent: Duration::from_nanos(nanos),
        })
    }
}

/// The echo request for `-s size`: the 8-byte header, then
/// [`data_len`]`(size)` bytes of data, as ping.c sends `cursize +
/// ICMP_MINLEN` / `cursize + ICMP6_HDRSIZE` bytes (ping.c:728-735,
/// 753-760), the data starting with `stamp`.
fn echo_request(
    v4: bool,
    identifier: u16,
    sequence: u16,
    size: u16,
    stamp: Stamp,
) -> std::io::Result<Vec<u8>> {
    use starla_network::{build_icmpv4_echo_request, build_icmpv6_echo_request};

    let mut payload = vec![0u8; data_len(size)];
    stamp.write(&mut payload);
    let mut packet = vec![0u8; super::ICMP_HEADER_LEN + payload.len()];
    let len = if v4 {
        build_icmpv4_echo_request(&mut packet, identifier, sequence, &payload)?
    } else {
        build_icmpv6_echo_request(&mut packet, identifier, sequence, &payload)?
    };
    packet.truncate(len);
    Ok(packet)
}

/// The IPv4 header in front of a datagram, when the socket hands it over:
/// raw IPv4 sockets, and macOS's datagram ones, do; Linux's datagram
/// sockets and every IPv6 socket do not. ping.c takes the reply's TTL and
/// the local address it came to from it (ip_ttl, ip_dst; ping.c:952-960).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ipv4Header {
    ttl: u8,
    dst: std::net::Ipv4Addr,
}

/// The ICMP message in a datagram an ICMP socket received, and the IPv4
/// header in front of it when there is one. Starla read the IPv4 header
/// as the ICMP message, so no IPv4 reply on macOS matched its request.
fn strip_ipv4_header(v4: bool, datagram: &[u8]) -> (&[u8], Option<Ipv4Header>) {
    if v4 && datagram.first().is_some_and(|b| b >> 4 == 4) {
        let ihl = usize::from(datagram[0] & 0x0f) * 4;
        if ihl >= 20 && datagram.len() >= ihl {
            let dst: [u8; 4] = datagram[16..20].try_into().unwrap_or_default();
            let header = Ipv4Header {
                ttl: datagram[8],
                dst: dst.into(),
            };
            return (&datagram[ihl..], Some(header));
        }
    }
    (datagram, None)
}

/// An echo reply of this run, as ready_callback4/6 take it.
#[derive(Debug, Clone, PartialEq)]
struct Echo {
    seq: u16,
    rtt: f64,
    ttl: Option<u8>,
    /// The data after the ICMP header: `nrecv - IPHDR - ICMP_MINLEN` with
    /// IPHDR a constant 20 for IPv4, `nrecv - ICMP6_HDRSIZE` for IPv6
    /// (ping.c:967-968, 1179-1180)
    size: usize,
    dst: Option<std::net::IpAddr>,
}

/// The datagram as an echo reply to one of this run's requests: an echo
/// reply with the run's identifier (a raw socket sees every process's) and
/// cookie, at least as long as the stamp (ping.c:899-941, 1094-1118). `now`
/// is when it came, from the start of the run.
fn echo_reply(
    v4: bool,
    identifier: Option<u16>,
    cookie: u64,
    datagram: &[u8],
    meta: starla_network::RecvMeta,
    now: Duration,
) -> Option<Echo> {
    use starla_network::{parse_icmpv4_echo_reply, parse_icmpv6_echo_reply};

    let (message, header) = strip_ipv4_header(v4, datagram);
    let (id, seq) = if v4 {
        parse_icmpv4_echo_reply(message)?
    } else {
        parse_icmpv6_echo_reply(message)?
    };
    if identifier.is_some_and(|identifier| identifier != id) {
        return None;
    }
    let stamp = Stamp::read(message.get(super::ICMP_HEADER_LEN..)?)?;
    if stamp.cookie != cookie {
        return None;
    }
    let size = match header {
        Some(_) => datagram.len().saturating_sub(20 + super::ICMP_HEADER_LEN),
        None => message.len() - super::ICMP_HEADER_LEN,
    };
    Some(Echo {
        seq,
        rtt: now.saturating_sub(stamp.sent).as_secs_f64() * 1e3,
        ttl: header.map(|h| h.ttl).or(meta.ttl),
        size,
        dst: header.map(|h| h.dst.into()).or(meta.dst),
    })
}

/// ping_start2, ping_xmit and the callbacks (ping.c:686-812, 840-1191,
/// 1462-1600): a socket connected to the target, then `count` requests
/// one interval apart, each interval taking whatever replies of the run
/// come, the run's sequence numbers and timeouts kept by [`Session`].
#[cfg(unix)]
pub async fn execute_ping(config: &PingConfig) -> anyhow::Result<PingOutcome> {
    use super::strerror;
    use starla_network::{new_icmpv4_socket, new_icmpv6_socket};
    use std::net::SocketAddr;
    use std::time::Instant;

    let v4 = config.target.is_ipv4();
    let socket = match if v4 {
        new_icmpv4_socket()
    } else {
        new_icmpv6_socket()
    } {
        Ok(socket) => socket,
        Err(e) => {
            return Ok(PingOutcome::Failed(format!(
                "socket failed: {}",
                strerror(&e)
            )))
        }
    };

    if config.ttl > 0 {
        socket.set_ttl(config.ttl as u32)?;
    }
    // The replies' TTL or hop limit and the address they came to; best
    // effort (an IPv4 header carries both anyway).
    if let Err(e) = socket.set_recv_ttl() {
        tracing::debug!("no TTL of replies from the kernel: {}", e);
    }
    if let Err(e) = socket.set_recv_pktinfo() {
        tracing::debug!("no destination of replies from the kernel: {}", e);
    }

    let dest = SocketAddr::new(config.target, 0);
    if let Err(e) = socket.connect(&dest) {
        return Ok(PingOutcome::Failed(format!(
            "connect failed: {}",
            strerror(&e)
        )));
    }
    // getsockname once connected (ping.c:1582-1597, 707-718, 745-747).
    // macOS leaves an ICMP socket's address unspecified after connect, as
    // three of RIPE's 4,500 latest firmware-5130 lines of msm 1001 show
    // some Linux hosts do ("src_addr":"0.0.0.0"); the address the route
    // gives is the one the requests leave from, so that is the one used.
    let local_addr = || match socket.local_addr().map(|a| a.ip()) {
        Ok(ip) if !ip.is_unspecified() => Some(ip),
        _ => starla_network::get_source_addr_for_dest(config.target).ok(),
    };
    let local = local_addr();

    // Linux's datagram sockets put their own identifier in, and pass up
    // only the replies that carry it.
    let identifier = rand::random::<u16>();
    let check_identifier = (!socket.is_dgram()).then_some(identifier);
    let cookie = rand::random::<u64>();
    let interval = Duration::from_millis(config.interval_ms);
    let start = Instant::now();
    let mut session = Session::new(!v4);
    // Replies echo the request's data, up to 4068 bytes after an IPv4
    // header of up to 60.
    let mut recv_buf = vec![0u8; 65536];

    for _ in 0..config.count {
        let stamp = Stamp {
            cookie,
            sent: start.elapsed(),
        };
        let packet = echo_request(v4, identifier, u16::from(session.seq()), config.size, stamp)?;
        let local = local_addr();
        let error = socket.send(&packet).await.err().map(|e| strerror(&e));
        session.sent(local, error);

        let deadline = tokio::time::Instant::now() + interval;
        let mut errors = 0;
        while let Ok(received) =
            tokio::time::timeout_at(deadline, socket.recv_meta(&mut recv_buf)).await
        {
            match received {
                Ok((len, meta)) => {
                    let now = start.elapsed();
                    if let Some(echo) =
                        echo_reply(v4, check_identifier, cookie, &recv_buf[..len], meta, now)
                    {
                        session.reply(echo.seq, echo.rtt, echo.ttl, echo.size, echo.dst);
                    }
                }
                // ping.c skips a failed read (ping.c:868-871, 1076-1079);
                // one that keeps failing waits out the interval.
                Err(e) => {
                    errors += 1;
                    if errors > 16 {
                        tracing::debug!("ping: receiving keeps failing: {}", e);
                        tokio::time::sleep_until(deadline).await;
                        break;
                    }
                }
            }
        }
        session.interval_over();
    }

    Ok(PingOutcome::Ran {
        local,
        events: session.into_events(),
    })
}

/// Windows. Over IPv6, a raw ICMPv6 socket when Windows gives one (an
/// elevated process): ping.c's own socket and loop ([`raw_ping_v6`]), the
/// replies' hop limit from WSARecvMsg. Otherwise, and over IPv4,
/// IcmpSendEcho2/Icmp6SendEcho2 per request ([`helper_ping`]).
#[cfg(windows)]
pub async fn execute_ping(config: &PingConfig) -> anyhow::Result<PingOutcome> {
    use starla_network::windows_icmp::RawIcmp6Socket;
    use std::net::IpAddr;

    if let IpAddr::V6(target) = config.target {
        match RawIcmp6Socket::open() {
            Ok(socket) => {
                let config = config.clone();
                return tokio::task::spawn_blocking(move || raw_ping_v6(&socket, target, &config))
                    .await?;
            }
            Err(e) => tracing::debug!(
                "ping: no raw ICMPv6 socket ({}), so Icmp6SendEcho2, which tells no hop limit",
                e
            ),
        }
    }
    helper_ping(config).await
}

/// ping_start2, ping_xmit and ready_callback6 over a raw ICMPv6 socket,
/// as [`execute_ping`] runs them on unix: bound to the address the route
/// gives (the checksum's pseudo-header names it) and connected, `count`
/// requests one interval apart, each interval taking the replies that
/// come. Each reply has its own hop limit, the doc's "ttl" (ping.c:1155-
/// 1162); Starla reported 64 for every Windows IPv6 ping, and after
/// ping_cb's rcvd_ttl came in, 0.
#[cfg(windows)]
fn raw_ping_v6(
    socket: &starla_network::windows_icmp::RawIcmp6Socket,
    target: std::net::Ipv6Addr,
    config: &PingConfig,
) -> anyhow::Result<PingOutcome> {
    use super::strerror;
    use std::net::IpAddr;
    use std::time::Instant;

    if config.ttl > 0 {
        socket.set_hop_limit(u32::from(config.ttl))?;
    }
    // No route is ping.c's failed connect (ping.c:1569-1580).
    let connect_failed =
        |e: std::io::Error| PingOutcome::Failed(format!("connect failed: {}", strerror(&e)));
    let src = match starla_network::get_source_addr_for_dest(config.target) {
        Ok(IpAddr::V6(src)) => src,
        Ok(IpAddr::V4(_)) => anyhow::bail!("an IPv4 source for {target}"),
        Err(e) => return Ok(connect_failed(e)),
    };
    if let Err(e) = socket.bind_connect(src, target) {
        return Ok(connect_failed(e));
    }
    let local = Some(IpAddr::V6(src));

    // A raw socket sees every process's echo replies from the target.
    let identifier = rand::random::<u16>();
    let cookie = rand::random::<u64>();
    let interval = Duration::from_millis(config.interval_ms);
    let start = Instant::now();
    let mut session = Session::new(true);
    let mut recv_buf = vec![0u8; 65536];

    for _ in 0..config.count {
        let stamp = Stamp {
            cookie,
            sent: start.elapsed(),
        };
        let mut packet = echo_request(
            false,
            identifier,
            u16::from(session.seq()),
            config.size,
            stamp,
        )?;
        starla_network::set_icmpv6_checksum(&mut packet, src, target);
        let error = socket.send(&packet).err().map(|e| strerror(&e));
        session.sent(local, error);

        let deadline = Instant::now() + interval;
        let mut errors = 0;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match socket.recv_meta(&mut recv_buf, left) {
                Ok(Some((len, meta))) => {
                    let now = start.elapsed();
                    if let Some(echo) =
                        echo_reply(false, Some(identifier), cookie, &recv_buf[..len], meta, now)
                    {
                        session.reply(echo.seq, echo.rtt, echo.ttl, echo.size, echo.dst);
                    }
                }
                Ok(None) => {}
                // ping.c skips a failed read (ping.c:1066-1071); one that
                // keeps failing waits out the interval.
                Err(e) => {
                    errors += 1;
                    if errors > 16 {
                        tracing::debug!("ping: receiving keeps failing: {}", e);
                        std::thread::sleep(left);
                        break;
                    }
                }
            }
        }
        session.interval_over();
    }

    Ok(PingOutcome::Ran {
        local,
        events: session.into_events(),
    })
}

/// IcmpSendEcho2 / Icmp6SendEcho2 per request, each with the interval to
/// answer in. IcmpSendEcho2's reply has the IPv4 TTL; Icmp6SendEcho2's
/// has no hop limit, so an IPv6 reply here counts as ping_cb's rcvd_ttl
/// ([`super::PingKept`]): 0, as ping.c would report a reply that came
/// without IPV6_HOPLIMIT. That is the limit of a Windows probe that is not
/// elevated: its IPv6 pings report "ttl":0, not the replies' hop limit.
#[cfg(windows)]
async fn helper_ping(config: &PingConfig) -> anyhow::Result<PingOutcome> {
    use starla_network::windows_icmp::{self, IcmpStatus};

    let payload = vec![0u8; data_len(config.size)];
    let interval = Duration::from_millis(config.interval_ms);
    let local = starla_network::get_source_addr_for_dest(config.target).ok();
    let mut session = Session::new(config.target.is_ipv6());

    for _ in 0..config.count {
        let started = std::time::Instant::now();
        let seq = session.seq();
        session.sent(local, None);
        let reply = windows_icmp::icmp_ping(
            config.target,
            config.ttl,
            config.interval_ms as u32,
            payload.clone(),
        )
        .await?;
        if matches!(reply.status, IcmpStatus::Success) {
            // 0 for Icmp6SendEcho2's, which has none
            let ttl = (reply.reply_ttl != 0).then_some(reply.reply_ttl);
            session.reply(u16::from(seq), reply.rtt_ms, ttl, payload.len(), None);
        }
        tokio::time::sleep(interval.saturating_sub(started.elapsed())).await;
        session.interval_over();
    }

    Ok(PingOutcome::Ran {
        local,
        events: session.into_events(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use starla_network::RecvMeta;

    const STAMP: Stamp = Stamp {
        cookie: 0x0123_4567_89ab_cdef,
        sent: Duration::from_millis(1500),
    };

    /// The twin's echo requests from the reference (probe-ref.pcap,
    /// ICMPv6 payload lengths): 40 bytes without -s, 56 for -s 48, 68 for
    /// -s 60, 72 for -s 64, 1368 for -s 1360, 1448 for -s 1440; and -s
    /// 1460's 1468, which with its 40-byte IPv6 header is 1508 bytes and
    /// went out in two fragments. Starla's were 64, 48, 60, 64, 1360, 1440
    /// and 1460.
    #[test]
    fn echo_requests_are_as_long_as_the_references() {
        for (size, len) in [
            (0, 40),
            (48, 56),
            (60, 68),
            (64, 72),
            (1360, 1368),
            (1440, 1448),
            (1460, 1468),
            (1560, 1568),
        ] {
            let packet = echo_request(false, 1, 1, size, STAMP).unwrap();
            assert_eq!(packet.len(), len, "-s {size}");
            assert_eq!(packet[0], 128, "ICMPv6 echo request");
        }
        assert!(40 + echo_request(false, 1, 1, 1460, STAMP).unwrap().len() > 1500);
        assert!(40 + echo_request(false, 1, 1, 1440, STAMP).unwrap().len() <= 1500);

        let packet = echo_request(true, 7, 3, 0, STAMP).unwrap();
        assert_eq!(packet.len(), 40);
        assert_eq!(packet[0], 8, "ICMP echo request");
        assert_eq!(&packet[4..8], &[0, 7, 0, 3]);
        assert_eq!(Stamp::read(&packet[8..]), Some(STAMP));
        assert_eq!(echo_request(true, 7, 3, 60, STAMP).unwrap().len(), 68);
        assert_eq!(echo_request(true, 7, 3, 9000, STAMP).unwrap().len(), 4076);
    }

    /// The request as its echo reply comes back: type 0 / 129.
    fn reply_to(v4: bool, request: &[u8]) -> Vec<u8> {
        let mut reply = request.to_vec();
        reply[0] = if v4 { 0 } else { 129 };
        reply
    }

    fn ipv4_header(ttl: u8, dst: [u8; 4]) -> Vec<u8> {
        let mut header = vec![0x45, 0, 0, 0, 0, 0, 0, 0, ttl, 1, 0, 0, 192, 0, 2, 1];
        header.extend_from_slice(&dst);
        header
    }

    /// An IPv4 header in front of the message (raw sockets; macOS's
    /// datagram ones) is skipped and its TTL and destination taken;
    /// Linux's datagram sockets, and IPv6, hand over the message alone.
    #[test]
    fn an_ipv4_header_gives_the_ttl_and_destination() {
        let mut datagram = ipv4_header(57, [198, 51, 100, 7]);
        let message = echo_request(true, 7, 3, 0, STAMP).unwrap();
        datagram.extend_from_slice(&message);
        let header = Ipv4Header {
            ttl: 57,
            dst: [198, 51, 100, 7].into(),
        };
        assert_eq!(
            strip_ipv4_header(true, &datagram),
            (&message[..], Some(header))
        );
        assert_eq!(strip_ipv4_header(true, &message), (&message[..], None));
        let message = echo_request(false, 7, 3, 0, STAMP).unwrap();
        assert_eq!(strip_ipv4_header(false, &message), (&message[..], None));
    }

    /// A reply is the run's when it echoes the cookie (and, on a raw
    /// socket, the identifier); its rtt runs from the send time it echoes,
    /// its size is the data after the header (IPHDR a constant 20 for
    /// IPv4), its TTL and destination come from the header or the
    /// ancillary data.
    #[test]
    fn replies_are_taken_as_ready_callback_takes_them() {
        let now = Duration::from_millis(1512);
        let meta = RecvMeta {
            ttl: Some(59),
            dst: Some("2001:db8::31".parse().unwrap()),
        };
        // IPv6, -s 48: 56 bytes of ICMPv6
        let v6 = reply_to(false, &echo_request(false, 9, 4, 48, STAMP).unwrap());
        let echo = echo_reply(false, None, STAMP.cookie, &v6, meta, now).unwrap();
        assert_eq!(
            echo,
            Echo {
                seq: 4,
                rtt: 12.0,
                ttl: Some(59),
                size: 48,
                dst: meta.dst,
            }
        );
        // another run's cookie, another process's identifier, a request
        assert_eq!(echo_reply(false, None, 1, &v6, meta, now), None);
        assert_eq!(
            echo_reply(false, Some(8), STAMP.cookie, &v6, meta, now),
            None
        );
        assert!(echo_reply(false, Some(9), STAMP.cookie, &v6, meta, now).is_some());
        let request = echo_request(false, 9, 4, 48, STAMP).unwrap();
        assert_eq!(
            echo_reply(false, None, STAMP.cookie, &request, meta, now),
            None
        );

        // IPv4 with its header: TTL and destination from it
        let mut v4 = ipv4_header(55, [192, 0, 2, 33]);
        v4.extend(reply_to(true, &echo_request(true, 9, 2, 0, STAMP).unwrap()));
        let echo = echo_reply(true, None, STAMP.cookie, &v4, RecvMeta::default(), now).unwrap();
        assert_eq!(
            (echo.seq, echo.ttl, echo.size, echo.dst),
            (2, Some(55), 32, Some("192.0.2.33".parse().unwrap()))
        );
        // Linux's datagram socket: no header, the ancillary data
        let v4 = reply_to(true, &echo_request(true, 9, 2, 0, STAMP).unwrap());
        let meta = RecvMeta {
            ttl: Some(54),
            dst: Some("192.0.2.34".parse().unwrap()),
        };
        let echo = echo_reply(true, None, STAMP.cookie, &v4, meta, now).unwrap();
        assert_eq!((echo.ttl, echo.size, echo.dst), (Some(54), 32, meta.dst));
    }

    /// A reply on Windows' raw ICMPv6 socket carries the hop limit
    /// WSARecvMsg hands over with it (IPV6_HOPLIMIT): the line's "ttl" is
    /// the replies' 57, as ref-86206980.json has it for facebook.com
    /// (`-6 -c 3 -s 48`). Through Icmp6SendEcho2, which tells none, the
    /// same replies count as rcvd_ttl, 0; Starla stored 64.
    #[test]
    fn a_windows_ipv6_reply_carries_its_hop_limit() {
        use crate::ping::Ping;
        use crate::target::DstName;
        use starla_common::{MeasurementData, MeasurementId, ProbeId};
        use starla_network::wsacmsg::{self, cmsg, IPPROTO_IPV6, IPV6_HOPLIMIT, IPV6_PKTINFO};

        let local: std::net::IpAddr = "2001:418:1::33".parse().unwrap();
        let mut pktinfo = "2001:418:1::33"
            .parse::<std::net::Ipv6Addr>()
            .unwrap()
            .octets()
            .to_vec();
        pktinfo.extend_from_slice(&12u32.to_ne_bytes());
        let mut control = cmsg(IPPROTO_IPV6, IPV6_PKTINFO, &pktinfo);
        control.extend(cmsg(IPPROTO_IPV6, IPV6_HOPLIMIT, &57i32.to_ne_bytes()));
        let windows = wsacmsg::recv_meta(&control);
        assert_eq!(windows.ttl, Some(57));

        let ping = || Ping {
            config: PingConfig {
                target: "2a03:2880:f35a:1:face:b00c:0:25de".parse().unwrap(),
                count: 3,
                size: 48,
                ttl: 64,
                interval_ms: 1000,
                backend: Default::default(),
                kept: Default::default(),
            },
            probe_id: ProbeId(1018212),
            msm_id: MeasurementId(86206980),
            dst_name: DstName {
                name: "facebook.com".into(),
                ttr: Some(18.216213),
            },
        };
        let run = |meta: RecvMeta| {
            let mut session = Session::new(true);
            for (i, rtt_us) in [66_512u64, 66_597, 66_627].into_iter().enumerate() {
                let stamp = Stamp {
                    cookie: STAMP.cookie,
                    sent: Duration::from_secs(i as u64),
                };
                let seq = session.seq();
                let request = echo_request(false, 9, u16::from(seq), 48, stamp).unwrap();
                session.sent(Some(local), None);
                let now = stamp.sent + Duration::from_micros(rtt_us);
                let echo = echo_reply(
                    false,
                    Some(9),
                    STAMP.cookie,
                    &reply_to(false, &request),
                    meta,
                    now,
                )
                .unwrap();
                session.reply(echo.seq, echo.rtt, echo.ttl, echo.size, echo.dst);
                session.interval_over();
            }
            let result = ping().outcome_result(&PingOutcome::Ran {
                local: Some(local),
                events: session.into_events(),
            });
            let MeasurementData::FullLine(body) = &result.data else {
                panic!("{:?}", result.data);
            };
            (result.ttl, body.clone())
        };
        let (ttl, body) = run(windows);
        assert_eq!(ttl, Some(57));
        assert!(
            body.ends_with(
                r#""src_addr":"2001:418:1::33", "proto":"ICMP", "ttl":57, "size":48, "result": [ { "rtt":66.512000 }, { "rtt":66.597000 }, { "rtt":66.627000 } ]"#
            ),
            "{body}"
        );
        // Icmp6SendEcho2: no hop limit, so rcvd_ttl's 0
        let (ttl, body) = run(RecvMeta::default());
        assert_eq!(ttl, Some(0));
        assert!(body.contains(r#""ttl":0, "size":48"#), "{body}");
    }

    #[cfg(unix)]
    fn config(target: &str) -> PingConfig {
        PingConfig {
            target: target.parse().unwrap(),
            count: 2,
            size: 0,
            ttl: 64,
            interval_ms: 300,
            backend: Default::default(),
            kept: Default::default(),
        }
    }

    /// Over loopback the replies come back with the kernel's 64, from the
    /// IPv4 header or the IPv6 hop limit, to the address the socket has,
    /// with the 32 data bytes sent, one interval apart. Starla's IPv4 pings
    /// on macOS never matched their reply (it read the header as the
    /// message). Skipped where no ICMP socket can be had.
    #[cfg(unix)]
    #[tokio::test]
    async fn loopback_replies_carry_their_ttl() {
        use crate::ping::PingEvent;

        for target in ["127.0.0.1", "::1"] {
            let started = std::time::Instant::now();
            let outcome = match execute_ping(&config(target)).await {
                Ok(outcome) => outcome,
                Err(e) => {
                    eprintln!("skipping {target}: {e}");
                    continue;
                }
            };
            let (local, events) = match outcome {
                PingOutcome::Ran { local, events } => (local, events),
                PingOutcome::Failed(e) => {
                    eprintln!("skipping {target}: {e}");
                    continue;
                }
            };
            assert!(started.elapsed() >= Duration::from_millis(600), "{target}");
            assert_eq!(local, Some(target.parse().unwrap()), "{target}");
            let replies: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    PingEvent::Reply {
                        dup,
                        ttl,
                        size,
                        dst,
                        ..
                    } => Some((*dup, *ttl, *size, *dst)),
                    _ => None,
                })
                .collect();
            let reply = (false, Some(64), 32, local);
            assert_eq!(replies, [reply, reply], "{target}: {events:?}");
        }
    }
}
