//! Status RESULT lines, on the reference probe's cadence
//!
//! The reference's main loop (bin/ripe-atlas.in) writes, every 180 s, an
//! uptime report (`rptuptime`, 7001) and, once the host has been up 10 min,
//! a disk report (`dfrm -A 9018 $ATLAS_DATA`), plus interface counters
//! (`rxtxrpt -A 9002`) only when RXTXRPT=yes, which it is not by default.
//! condmv then appends a marker when it moves a spool file to data/out: 9907
//! after eperd's results (data/new/7), 9901 after httppost's own lines
//! (data/new/main). Starla sent 9018, 7001, 9002 and a `9901 ... starla`
//! line in every upload instead.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The reference's main-loop period (`sleepkick 180`).
pub const STATUS_INTERVAL_SECS: u64 = 180;

/// dfrm runs only once reboot-count.txt has been moved away, after 600 s of
/// uptime.
const DFRM_AFTER_UPTIME_SECS: u64 = 600;

/// atlas_get_version_json_str(): `"fw":<firmware>, "mver": "2.6.4"`.
fn version_json() -> String {
    format!(
        "\"fw\":{}, \"mver\": \"2.6.4\"",
        starla_common::FIRMWARE_VERSION
    )
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// One main-loop pass: 7001, then 9018 once the host has been up 10 min,
/// then 9002 if asked for.
pub fn status_lines(lts: i64, data_dir: &Path, rxtxrpt: bool) -> Vec<String> {
    let uptime = system_uptime();
    let mut lines = vec![uptime_line(lts, uptime)];
    if uptime > DFRM_AFTER_UPTIME_SECS {
        lines.extend(disk_line(data_dir));
    }
    if rxtxrpt {
        lines.extend(rxtxrpt_line(lts));
    }
    lines
}

/// rptuptime: `RESULT { "id": "7001", <version>, "time": t, "lts": l,
/// "uptime": u }`, u the host's uptime (sysinfo).
pub fn uptime_line(lts: i64, uptime: u64) -> String {
    format!(
        "RESULT {{ \"id\": \"7001\", {}, \"time\": {}, \"lts\": {lts}, \"uptime\": {uptime} }}\n",
        version_json(),
        unix_now()
    )
}

/// dfrm: `RESULT { "id":"9018", <version>, "time": t, "bsize": …, "blocks":
/// …, "bfree": …, "free": … }` for the probe's data directory.
pub fn disk_line(data_dir: &Path) -> Option<String> {
    let disk = disk_stats(data_dir)?;
    Some(format!(
        "RESULT {{ \"id\":\"9018\", {}, \"time\": {}, \"bsize\": {}, \"blocks\": {}, \"bfree\": \
         {}, \"free\": {} }}\n",
        version_json(),
        unix_now(),
        disk.block_size,
        disk.blocks,
        disk.blocks_free,
        disk.free
    ))
}

/// rxtxrpt -A 9002 (only with RXTXRPT=yes on the reference).
pub fn rxtxrpt_line(lts: i64) -> Option<String> {
    let ifaces = interface_stats()?;
    Some(format!(
        "RESULT {{ \"id\": \"9002\", {}, \"time\": {}, \"lts\": {lts}, \"interfaces\": [ {} ] }}\n",
        version_json(),
        unix_now(),
        ifaces
            .iter()
            .map(|i| format!(
                "{{ \"name\": \"{}\", \"bytes_recv\": {}, \"pkt_recv\": {}, \"errors_recv\": 0, \
                 \"dropped_recv\": 0, \"fifo_recv\": 0, \"framing_recv\": 0, \"compressed_recv\": \
                 0, \"multicast_recv\": 0, \"bytes_sent\": {}, \"pkt_sent\": {}, \"errors_sent\": \
                 0, \"dropped_sent\": 0, \"fifo_sent\": 0, \"collisions_sent\": 0, \
                 \"carr_lost_sent\": 0, \"compressed_sent\": 0 }}",
                i.name, i.rx_bytes, i.rx_packets, i.tx_bytes, i.tx_packets
            ))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// condmv -A "RESULT <id> ongoing": the marker appended to a spool file as it
/// is moved to data/out.
pub fn spool_marker(id: u32, spool: &str) -> String {
    format!("RESULT {id} ongoing {} {spool}\n", unix_now())
}

/// Measurement results: eperd's crons/7, `condmv -A "RESULT 9907 ongoing"`.
pub fn results_marker() -> String {
    spool_marker(9907, &spool_path("7"))
}

/// httppost's own lines: crons/main, `condmv -A "RESULT 9901 ongoing"`.
pub fn main_marker() -> String {
    spool_marker(9901, &spool_path("main"))
}

/// The spool path condmv reports for a file under `data/new`. The reference
/// software probe's spool is under `@atlas_spooldir@`, not the hardware
/// probe's `/home/atlas/data/new`, so hardcoding the latter was wrong for a
/// software probe. Use the probe's own state directory.
fn spool_path(name: &str) -> String {
    starla_common::state_dir()
        .join("data/new")
        .join(name)
        .display()
        .to_string()
}

/// The host's uptime in seconds (sysinfo on Linux, as rptuptime).
fn system_uptime() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
        if unsafe { libc::sysinfo(&mut info) } == 0 {
            return info.uptime.max(0) as u64;
        }
    }
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split('.').next().and_then(|n| n.parse().ok()))
        .unwrap_or(0)
}

struct DiskStats {
    block_size: u64,
    blocks: u64,
    blocks_free: u64,
    free: u64,
}

fn disk_stats(dir: &Path) -> Option<DiskStats> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uptime_line_is_rptuptimes() {
        let line = uptime_line(-1, 4242);
        let fw = starla_common::FIRMWARE_VERSION;
        assert!(
            line.starts_with(&format!(
                "RESULT {{ \"id\": \"7001\", \"fw\":{fw}, \"mver\": \"2.6.4\", \"time\": "
            )),
            "{line}"
        );
        assert!(
            line.ends_with(", \"lts\": -1, \"uptime\": 4242 }\n"),
            "{line}"
        );
    }

    #[test]
    fn a_status_pass_starts_with_uptime_and_has_no_counters_unless_asked() {
        let dir = std::env::temp_dir();
        let lines = status_lines(-1, &dir, false);
        assert!(lines[0].contains("\"id\": \"7001\""), "{lines:?}");
        assert!(lines.iter().all(|l| !l.contains("\"9002\"")), "{lines:?}");
        assert!(lines.len() <= 2, "{lines:?}");
    }

    #[test]
    fn markers_are_condmvs() {
        let spool = starla_common::state_dir().join("data/new");
        let m = results_marker();
        assert!(m.starts_with("RESULT 9907 ongoing "), "{m}");
        assert!(
            m.ends_with(&format!(" {}\n", spool.join("7").display())),
            "{m}"
        );
        let m = main_marker();
        assert!(m.starts_with("RESULT 9901 ongoing "), "{m}");
        assert!(
            m.ends_with(&format!(" {}\n", spool.join("main").display())),
            "{m}"
        );
    }
}
