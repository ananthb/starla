//! SNTP client with multi-sample support, as the reference probe's evntp
//! (probe-busybox/eperd/ntp.c, firmware 5130)

use super::NtpConfig;
use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::time::timeout;

// NTP Epoch is 1900-01-01, Unix Epoch is 1970-01-01
const NTP_OFFSET: u64 = 2_208_988_800;

const FOUR_G: f64 = 4_294_967_296.0;

/// The reference id a client request carries: "RIPE", in network byte order
/// (5130 sends it with htonl, so it is "RIPE" on the wire on every
/// architecture). Starla sent 0.
const REF_ID_RIPE: [u8; 4] = *b"RIPE";

/// 5130's minimum time between requests (`-g`, in ms): the default and the
/// bounds outside which evntp refuses the measurement.
pub const GAP_MIN_MS: u64 = 2000;
pub const GAP_DEFAULT_MS: u64 = 4000;
pub const GAP_MAX_MS: u64 = 60000;

/// evntp's defaults for `-c` and `-w` (ms).
pub const COUNT_DEFAULT: u32 = 3;
pub const TIMEOUT_DEFAULT_MS: u64 = 1000;

/// Convert NTP timestamp (64-bit: 32-bit seconds + 32-bit fraction) to f64
fn ntp_ts_to_f64(ts: u64) -> f64 {
    let secs = (ts >> 32) as f64;
    let frac = (ts & 0xFFFFFFFF) as f64 / FOUR_G;
    secs + frac
}

/// Convert fixed-point 16.16 to f64 seconds
fn ntp_short_to_secs(val: u32) -> f64 {
    let secs = (val >> 16) as f64;
    let frac = (val & 0xFFFF) as f64 / 65536.0;
    secs + frac
}

/// Current time as a raw 64-bit NTP timestamp
fn now_ntp() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() + NTP_OFFSET;
    let frac = ((now.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (secs << 32) | (frac & 0xFFFF_FFFF)
}

/// RFC 5905 §8 delay and offset from T1 (client send), T2 (server receive),
/// T3 (server send), T4 (client receive), in 64-bit fixed point as 5130
/// computes them, so a negative offset is exact too:
/// delay = (T4 - T1) - (T3 - T2), offset = ((T2 - T1) + (T3 - T4)) / 2.
pub fn delay_and_offset(t1: u64, t2: u64, t3: u64, t4: u64) -> (f64, f64) {
    let d = |a: u64, b: u64| a.wrapping_sub(b) as i64;
    let delay = d(t4, t1).wrapping_sub(d(t3, t2)) as f64 / FOUR_G;
    let offset = d(t2, t1).wrapping_add(d(t3, t4)) as f64 / 2.0 / FOUR_G;
    (delay, offset)
}

/// A client request: version 4, mode 3, reference id "RIPE", transmit T1.
fn request(t1: u64) -> BytesMut {
    let mut packet = BytesMut::with_capacity(48);
    packet.put_u8(0x23); // LI=0, VN=4, Mode=3 (Client)
    packet.put_u8(0); // Stratum
    packet.put_u8(0); // Poll
    packet.put_u8(0); // Precision
    packet.put_u32(0); // Root Delay
    packet.put_u32(0); // Root Dispersion
    packet.put_slice(&REF_ID_RIPE);
    packet.put_u64(0); // Ref Timestamp
    packet.put_u64(0); // Origin Timestamp
    packet.put_u64(0); // Receive Timestamp
    packet.put_u64(t1); // Transmit Timestamp
    packet
}

/// A single NTP exchange sample
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtpSample {
    /// T1: client transmit time (NTP timestamp)
    pub origin_ts: f64,
    /// T2: server receive time (NTP timestamp)
    pub receive_ts: f64,
    /// T3: server transmit time (NTP timestamp)
    pub transmit_ts: f64,
    /// T4: client receive time (NTP timestamp)
    pub final_ts: f64,
    /// RTT in seconds
    pub rtt: f64,
    /// Offset in seconds
    pub offset: f64,
}

/// The server's header fields, from the first reply
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtpHeader {
    pub stratum: u8,
    pub leap: u8,
    pub version: u8,
    pub mode: u8,
    /// Raw poll exponent (log2)
    pub poll: u8,
    /// Raw precision exponent (log2)
    pub precision: i8,
    /// Root delay in seconds
    pub root_delay: f64,
    /// Root dispersion in seconds
    pub root_dispersion: f64,
    /// Reference ID
    pub ref_id: String,
    /// Reference timestamp (NTP)
    pub ref_ts: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtpResult {
    /// None when no request was answered (evntp then prints no header)
    pub header: Option<NtpHeader>,
    /// One per request sent; None for a timeout (`{ "x":"*" }`)
    pub samples: Vec<Option<NtpSample>>,
    /// Total time for all samples in seconds
    pub ttr: f64,
}

/// Perform a single NTP exchange, returning the sample and the raw reply
async fn ntp_exchange(
    socket: &UdpSocket,
    dest: SocketAddr,
    timeout_dur: Duration,
) -> anyhow::Result<(NtpSample, [u8; 48])> {
    let t1 = now_ntp();
    socket.send_to(&request(t1), dest).await?;

    let mut recv_buf = [0u8; 48];
    let (len, _) = timeout(timeout_dur, socket.recv_from(&mut recv_buf)).await??;
    let t4 = now_ntp();

    if len < 48 {
        anyhow::bail!("NTP response too short");
    }

    let mut buf = &recv_buf[32..];
    let t2 = buf.get_u64();
    let t3 = buf.get_u64();
    let (rtt, offset) = delay_and_offset(t1, t2, t3, t4);

    Ok((
        NtpSample {
            origin_ts: ntp_ts_to_f64(t1),
            receive_ts: ntp_ts_to_f64(t2),
            transmit_ts: ntp_ts_to_f64(t3),
            final_ts: ntp_ts_to_f64(t4),
            rtt,
            offset,
        },
        recv_buf,
    ))
}

/// Send `config.count` requests, each at least `config.gap_ms` after the
/// one before (5130's gap); an unanswered one within `config.timeout_ms` is
/// recorded as a timeout and the next is still sent, as evntp does.
pub async fn execute_ntp_query(config: &NtpConfig) -> anyhow::Result<NtpResult> {
    let bind_addr = if config.target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind_addr).await?;
    let dest = SocketAddr::new(config.target, config.port);
    let timeout_dur = Duration::from_millis(config.timeout_ms);
    let gap = Duration::from_millis(config.gap_ms);

    let start = Instant::now();
    let mut samples = Vec::new();
    let mut header = None;

    for i in 0..config.count {
        let sent_at = tokio::time::Instant::now();
        match ntp_exchange(&socket, dest, timeout_dur).await {
            Ok((sample, raw)) => {
                if header.is_none() {
                    header = Some(parse_header(&raw));
                }
                samples.push(Some(sample));
            }
            Err(e) => {
                tracing::debug!("NTP request {} to {} unanswered: {}", i + 1, dest, e);
                samples.push(None);
            }
        }
        if i + 1 < config.count {
            tokio::time::sleep_until(sent_at + gap).await;
        }
    }

    Ok(NtpResult {
        header,
        samples,
        ttr: start.elapsed().as_secs_f64(),
    })
}

fn parse_header(raw: &[u8; 48]) -> NtpHeader {
    let mut buf = &raw[..];
    let first = buf.get_u8();
    let li = (first >> 6) & 0x03;
    let vn = (first >> 3) & 0x07;
    let mode = first & 0x07;
    let stratum = buf.get_u8();
    let poll = buf.get_u8();
    let precision = buf.get_i8();
    let root_delay_raw = buf.get_u32();
    let root_disp_raw = buf.get_u32();
    let ref_id_raw = buf.get_u32();
    let ref_ts = buf.get_u64();

    // Reference ID: stratum <= 1 uses ASCII, >= 2 uses hex
    let ref_id = if stratum <= 1 {
        let bytes = ref_id_raw.to_be_bytes();
        String::from_utf8_lossy(&bytes)
            .trim_end_matches('\0')
            .to_string()
    } else {
        format!("{:08x}", ref_id_raw)
    };

    NtpHeader {
        stratum,
        leap: li,
        version: vn,
        mode,
        poll,
        precision,
        root_delay: ntp_short_to_secs(root_delay_raw),
        root_dispersion: ntp_short_to_secs(root_disp_raw),
        ref_id,
        ref_ts: ntp_ts_to_f64(ref_ts),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: f64) -> u64 {
        (secs * FOUR_G).round() as u64
    }

    /// 5130's fix: 5120 reported half the round trip as the offset.
    #[test]
    fn delay_and_offset_are_rfc_5905s_either_way() {
        let t1 = 3_000_000_000.0;
        // Server 0.25 s ahead, 10 ms each way, 1 ms in the server.
        let (d, o) = delay_and_offset(
            ts(t1),
            ts(t1 + 0.260),
            ts(t1 + 0.261),
            ts(t1 + 0.261 - 0.25 + 0.010),
        );
        assert!((d - 0.020).abs() < 1e-6, "{d}");
        assert!((o - 0.250).abs() < 1e-6, "{o}");
        // Server 0.25 s behind.
        let (d, o) = delay_and_offset(
            ts(t1),
            ts(t1 - 0.240),
            ts(t1 - 0.239),
            ts(t1 - 0.239 + 0.25 + 0.010),
        );
        assert!((d - 0.020).abs() < 1e-6, "{d}");
        assert!((o + 0.250).abs() < 1e-6, "{o}");
    }

    #[test]
    fn a_request_carries_ripe_and_t1() {
        let p = request(0x0123_4567_89ab_cdef);
        assert_eq!(p.len(), 48);
        assert_eq!(p[0], 0x23);
        assert_eq!(&p[12..16], b"RIPE");
        assert_eq!(&p[40..48], &0x0123_4567_89ab_cdef_u64.to_be_bytes());
    }

    /// A server that answers the first two requests and ignores the third.
    #[tokio::test]
    async fn requests_keep_the_gap_and_a_timeout_is_recorded() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut buf = [0u8; 48];
            for n in 0..3 {
                let (_, from) = server.recv_from(&mut buf).await.unwrap();
                tx.send((Instant::now(), buf)).unwrap();
                if n < 2 {
                    let mut reply = buf;
                    reply[0] = 0x24; // LI 0, VN 4, mode 4 (server)
                    reply[1] = 1; // stratum 1
                    reply[12..16].copy_from_slice(b"GPS\0");
                    let now = now_ntp().to_be_bytes();
                    reply[24..32].copy_from_slice(&buf[40..48]); // origin = T1
                    reply[32..40].copy_from_slice(&now); // T2
                    reply[40..48].copy_from_slice(&now); // T3
                    server.send_to(&reply, from).await.unwrap();
                }
            }
        });

        let config = NtpConfig {
            target: "127.0.0.1".parse().unwrap(),
            dst_name: "127.0.0.1".into(),
            timeout_ms: 300,
            port,
            count: 3,
            gap_ms: 200,
        };
        let res = execute_ntp_query(&config).await.unwrap();
        assert_eq!(res.samples.len(), 3);
        assert!(res.samples[0].is_some() && res.samples[1].is_some());
        assert!(res.samples[2].is_none(), "the third timed out");
        assert_eq!(res.header.unwrap().ref_id, "GPS");

        let mut at = Vec::new();
        while let Ok((t, req)) = rx.try_recv() {
            assert_eq!(&req[12..16], b"RIPE");
            at.push(t);
        }
        assert_eq!(at.len(), 3);
        for w in at.windows(2) {
            assert!(
                w[1] - w[0] >= Duration::from_millis(190),
                "gap {:?}",
                w[1] - w[0]
            );
        }
    }
}
