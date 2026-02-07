//! Simple SNTP client

use super::NtpConfig;
use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::time::timeout;

/// NTP measurement result in RIPE Atlas format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtpResult {
    /// Round-trip time in ms
    pub rt: f64,
    /// Offset from server time in ms (positive = local clock is ahead)
    pub offset: f64,
    /// Final offset after applying RTT correction
    #[serde(rename = "final-offset")]
    pub final_offset: f64,
    /// Server stratum
    pub stratum: u8,
    /// Leap indicator (0=no warning, 1=+1s, 2=-1s, 3=unsync)
    pub leap: u8,
    /// NTP version
    pub version: u8,
    /// Mode (4 = server response)
    pub mode: u8,
    /// Poll interval (as power of 2)
    pub poll: u8,
    /// Precision (as power of 2)
    pub precision: i8,
    /// Root delay in ms
    #[serde(rename = "root-delay")]
    pub root_delay: f64,
    /// Root dispersion in ms
    #[serde(rename = "root-dispersion")]
    pub root_dispersion: f64,
    /// Reference ID (stratum <= 1: ASCII, else IP)
    #[serde(rename = "ref-id")]
    pub ref_id: String,
    /// Reference timestamp
    #[serde(rename = "ref-ts")]
    pub ref_ts: f64,
    /// Receive timestamp (T2)
    #[serde(rename = "recv-ts")]
    pub recv_ts: f64,
    /// Transmit timestamp (T3)
    #[serde(rename = "trans-ts")]
    pub trans_ts: f64,
}

// NTP Epoch is 1900-01-01
// Unix Epoch is 1970-01-01
// Difference is 2,208,988,800 seconds
const NTP_OFFSET: u64 = 2_208_988_800;

/// Convert NTP timestamp (64-bit: 32-bit seconds + 32-bit fraction) to f64
/// seconds
fn ntp_ts_to_f64(ts: u64) -> f64 {
    let secs = (ts >> 32) as f64;
    let frac = (ts & 0xFFFFFFFF) as f64 / 4294967296.0;
    secs + frac
}

/// Convert fixed-point 16.16 to f64 milliseconds
fn ntp_short_to_ms(val: u32) -> f64 {
    let secs = (val >> 16) as f64;
    let frac = (val & 0xFFFF) as f64 / 65536.0;
    (secs + frac) * 1000.0
}

pub async fn execute_ntp_query(config: &NtpConfig) -> anyhow::Result<NtpResult> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let dest = SocketAddr::new(config.target, config.port);

    // Construct packet
    let mut packet = BytesMut::with_capacity(48);
    // LI = 0, VN = 4, Mode = 3 (Client) -> 00 100 011 -> 0x23
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

    // Transmit Timestamp (T1 - client send time)
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let t1_secs = now.as_secs() + NTP_OFFSET;
    let t1_frac = ((now.subsec_nanos() as u64) << 32) / 1_000_000_000;
    packet.put_u32(t1_secs as u32);
    packet.put_u32(t1_frac as u32);

    let t1 = t1_secs as f64 + (t1_frac as f64 / 4294967296.0);

    let start = Instant::now();
    socket.send_to(&packet, dest).await?;

    let mut recv_buf = [0u8; 48];
    let (len, _addr) = timeout(
        Duration::from_millis(config.timeout_ms),
        socket.recv_from(&mut recv_buf),
    )
    .await??;

    // T4 - client receive time
    let end_sys = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let t4 =
        (end_sys.as_secs() + NTP_OFFSET) as f64 + (end_sys.subsec_nanos() as f64 / 1_000_000_000.0);

    let rtt = start.elapsed().as_secs_f64() * 1000.0;

    if len < 48 {
        anyhow::bail!("NTP response too short");
    }

    let mut buf = &recv_buf[..];
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
    let _orig_ts = buf.get_u64(); // Origin timestamp (should be our T1)
    let recv_ts = buf.get_u64(); // T2 - server receive time
    let trans_ts = buf.get_u64(); // T3 - server transmit time

    // Convert timestamps
    let t2 = ntp_ts_to_f64(recv_ts);
    let t3 = ntp_ts_to_f64(trans_ts);

    // Calculate offset: ((T2 - T1) + (T3 - T4)) / 2
    let offset = ((t2 - t1) + (t3 - t4)) / 2.0;

    // Reference ID interpretation
    let ref_id = if stratum <= 1 {
        // ASCII string for stratum 0-1
        let bytes = ref_id_raw.to_be_bytes();
        String::from_utf8_lossy(&bytes)
            .trim_end_matches('\0')
            .to_string()
    } else {
        // IPv4 address for stratum >= 2
        let bytes = ref_id_raw.to_be_bytes();
        format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3])
    };

    Ok(NtpResult {
        rt: rtt,
        offset: offset * 1000.0, // Convert to ms
        final_offset: offset * 1000.0,
        stratum,
        leap: li,
        version: vn,
        mode,
        poll,
        precision,
        root_delay: ntp_short_to_ms(root_delay_raw),
        root_dispersion: ntp_short_to_ms(root_disp_raw),
        ref_id,
        ref_ts: ntp_ts_to_f64(ref_ts),
        recv_ts: t2,
        trans_ts: t3,
    })
}
