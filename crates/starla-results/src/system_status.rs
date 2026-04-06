//! System status results included with every upload batch
//!
//! The RIPE Atlas controller expects periodic system health data alongside
//! measurement results. Without these, the controller may rate-limit uploads.

use std::time::{SystemTime, UNIX_EPOCH};

/// Generate system status RESULT lines to include in upload batches.
///
/// These match what the official C probe sends via `simpleping`:
/// - 9018: disk stats
/// - 9002: network interface stats
/// - 7001: uptime
/// - 9901: ongoing status line
pub fn system_status_lines(lts: i64, start_time: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut lines = String::new();

    // 9018: Disk stats
    if let Some(disk) = disk_stats() {
        lines.push_str(&format!(
            "RESULT {{ \"id\":\"9018\", \"fw\":5120, \"mver\": \"2.6.4\", \"time\": {}, \
             \"bsize\": {}, \"blocks\": {}, \"bfree\": {}, \"free\": {} }}\n",
            now, disk.block_size, disk.blocks, disk.blocks_free, disk.free
        ));
    }

    // 7001: Uptime
    let uptime = now.saturating_sub(start_time);
    lines.push_str(&format!(
        "RESULT {{ \"id\": \"7001\", \"fw\":5120, \"mver\": \"2.6.4\", \"time\": {}, \"lts\": {}, \
         \"uptime\": {} }}\n",
        now, lts, uptime
    ));

    // 9002: Network interface stats
    if let Some(ifaces) = interface_stats() {
        lines.push_str(&format!(
            "RESULT {{ \"id\": \"9002\", \"fw\":5120, \"mver\": \"2.6.4\", \"time\": {}, \"lts\": \
             {}, \"interfaces\": [ {} ] }}\n",
            now,
            lts,
            ifaces
                .iter()
                .map(|i| format!(
                    "{{ \"name\": \"{}\", \"bytes_recv\": {}, \"pkt_recv\": {}, \"errors_recv\": \
                     0, \"dropped_recv\": 0, \"fifo_recv\": 0, \"framing_recv\": 0, \
                     \"compressed_recv\": 0, \"multicast_recv\": 0, \"bytes_sent\": {}, \
                     \"pkt_sent\": {}, \"errors_sent\": 0, \"dropped_sent\": 0, \"fifo_sent\": 0, \
                     \"collisions_sent\": 0, \"carr_lost_sent\": 0, \"compressed_sent\": 0 }}",
                    i.name, i.rx_bytes, i.rx_packets, i.tx_bytes, i.tx_packets
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // 9901: ongoing status (required in every upload)
    lines.push_str(&format!("RESULT 9901 ongoing {} starla\n", now));

    lines
}

struct DiskStats {
    block_size: u64,
    blocks: u64,
    blocks_free: u64,
    free: u64,
}

fn disk_stats() -> Option<DiskStats> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        let path = CString::new("/").ok()?;
        unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(path.as_ptr(), &mut stat) == 0 {
                return Some(DiskStats {
                    block_size: stat.f_bsize as u64,
                    blocks: stat.f_blocks as u64,
                    blocks_free: stat.f_bfree as u64,
                    free: stat.f_bfree as u64 * stat.f_bsize as u64 / 1024,
                });
            }
        }
        None
    }
    #[cfg(not(unix))]
    {
        None
    }
}

struct IfaceStats {
    name: String,
    rx_bytes: u64,
    rx_packets: u64,
    tx_bytes: u64,
    tx_packets: u64,
}

fn interface_stats() -> Option<Vec<IfaceStats>> {
    #[cfg(target_os = "linux")]
    {
        let content = std::fs::read_to_string("/proc/net/dev").ok()?;
        let mut ifaces = Vec::new();
        for line in content.lines().skip(2) {
            let line = line.trim();
            let (name, rest) = line.split_once(':')?;
            let fields: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            if fields.len() >= 10 {
                ifaces.push(IfaceStats {
                    name: name.trim().to_string(),
                    rx_bytes: fields[0],
                    rx_packets: fields[1],
                    tx_bytes: fields[8],
                    tx_packets: fields[9],
                });
            }
        }
        Some(ifaces)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
