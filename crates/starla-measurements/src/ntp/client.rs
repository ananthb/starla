//! Simple SNTP client with multi-sample support

use super::NtpConfig;
use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::time::timeout;

// NTP Epoch is 1900-01-01, Unix Epoch is 1970-01-01
const NTP_OFFSET: u64 = 2_208_988_800;

/// Convert NTP timestamp (64-bit: 32-bit seconds + 32-bit fraction) to f64
fn ntp_ts_to_f64(ts: u64) -> f64 {
    let secs = (ts >> 32) as f64;
    let frac = (ts & 0xFFFFFFFF) as f64 / 4294967296.0;
    secs + frac
}

/// Convert fixed-point 16.16 to f64 seconds
fn ntp_short_to_secs(val: u32) -> f64 {
    let secs = (val >> 16) as f64;
    let frac = (val & 0xFFFF) as f64 / 65536.0;
    secs + frac
}

/// Get current time as NTP timestamp (seconds since 1900, f64)
fn now_ntp() -> (u32, u32, f64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() + NTP_OFFSET;
    let frac = ((now.subsec_nanos() as u64) << 32) / 1_000_000_000;
    let f = secs as f64 + (frac as f64 / 4294967296.0);
    (secs as u32, frac as u32, f)
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

/// NTP server response header fields (from first sample)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtpResult {
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
    /// Individual samples
    pub samples: Vec<NtpSample>,
    /// Total time for all samples in seconds
    pub ttr: f64,
}

/// Perform a single NTP exchange, returning the sample and parsed header
async fn ntp_exchange(
    socket: &UdpSocket,
    dest: SocketAddr,
    timeout_dur: Duration,
) -> anyhow::Result<(NtpSample, [u8; 48])> {
    let mut packet = BytesMut::with_capacity(48);
    // LI=0, VN=4, Mode=3 (Client) -> 0x23
    packet.put_u8(0x23);
    packet.put_u8(0); // Stratum
    packet.put_u8(0); // Poll
    packet.put_u8(0); // Precision
    packet.put_u32(0); // Root Delay
    packet.put_u32(0); // Root Dispersion
    packet.put_u32(0); // Reference ID
    packet.put_u64(0); // Ref Timestamp
    packet.put_u64(0); // Origin Timestamp
    packet.put_u64(0); // Receive Timestamp

    // Transmit Timestamp (T1)
    let (t1_secs, t1_frac, t1) = now_ntp();
    packet.put_u32(t1_secs);
    packet.put_u32(t1_frac);

    socket.send_to(&packet, dest).await?;

    let mut recv_buf = [0u8; 48];
    let (len, _) = timeout(timeout_dur, socket.recv_from(&mut recv_buf)).await??;

    // T4
    let (_, _, t4) = now_ntp();

    if len < 48 {
        anyhow::bail!("NTP response too short");
    }

    let mut buf = &recv_buf[..];
    let _header = buf.get_u8(); // skip header for sample parsing
    let _stratum = buf.get_u8();
    let _poll = buf.get_u8();
    let _precision = buf.get_i8();
    let _root_delay = buf.get_u32();
    let _root_disp = buf.get_u32();
    let _ref_id = buf.get_u32();
    let _ref_ts = buf.get_u64();
    let _orig_ts = buf.get_u64();
    let recv_ts = buf.get_u64();
    let trans_ts = buf.get_u64();

    let t2 = ntp_ts_to_f64(recv_ts);
    let t3 = ntp_ts_to_f64(trans_ts);
    let rtt = (t4 - t1) - (t3 - t2);
    let offset = ((t2 - t1) + (t3 - t4)) / 2.0;

    Ok((
        NtpSample {
            origin_ts: t1,
            receive_ts: t2,
            transmit_ts: t3,
            final_ts: t4,
            rtt,
            offset,
        },
        recv_buf,
    ))
}

pub async fn execute_ntp_query(config: &NtpConfig, num_samples: u32) -> anyhow::Result<NtpResult> {
    let bind_addr = if config.target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind_addr).await?;
    let dest = SocketAddr::new(config.target, config.port);
    let timeout_dur = Duration::from_millis(config.timeout_ms);

    let start = Instant::now();
    let mut samples = Vec::new();
    let mut header_buf = [0u8; 48];

    for _ in 0..num_samples {
        match ntp_exchange(&socket, dest, timeout_dur).await {
            Ok((sample, raw)) => {
                if samples.is_empty() {
                    header_buf = raw;
                }
                samples.push(sample);
            }
            Err(e) => {
                if samples.is_empty() {
                    return Err(e);
                }
                break;
            }
        }
    }

    let ttr = start.elapsed().as_secs_f64();

    // Parse header from first response
    let mut buf = &header_buf[..];
    let header = buf.get_u8();
    let li = (header >> 6) & 0x03;
    let vn = (header >> 3) & 0x07;
    let mode = header & 0x07;
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

    Ok(NtpResult {
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
        samples,
        ttr,
    })
}
