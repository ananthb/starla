//! Host telemetry reporters: buddyinfo + rptaddrs
//!
//! The official RIPE Atlas probe schedules small "applets" via CRONLINE that
//! describe the probe's runtime environment rather than running measurements.
//! These produce RESULT lines uploaded alongside measurement data so the
//! controller knows the probe's memory pressure (buddyinfo) and network
//! connectivity (rptaddrs).
//!
//! starla matches the JSON shape of the official probe so its uploads are
//! indistinguishable to the RIPE backend. Side effects that only make sense
//! on the embedded probe (e.g. the LOWMEM_REBOOT self-reboot in buddyinfo)
//! are intentionally omitted: starla runs on a regular Linux host where the
//! host's memory manager / orchestrator is responsible for those decisions.

use std::collections::HashMap;
#[cfg(any(target_os = "linux", test))]
use std::fmt::Write as _;
#[cfg(any(target_os = "linux", test))]
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, trace};

/// What kind of host telemetry is being reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostTelemetryKind {
    Buddyinfo,
    Rptaddrs,
}

/// Shared state collecting host-telemetry RESULT lines for the next upload.
///
/// Reporters push lines via internal periodic tasks; the uploader drains
/// pending lines once per upload cycle.
#[derive(Default)]
pub struct HostTelemetry {
    pending: Mutex<Vec<String>>,
    tasks: Mutex<HashMap<HostTelemetryKind, JoinHandle<()>>>,
    #[cfg(target_os = "linux")]
    rptaddrs_cache: Mutex<RptaddrsCache>,
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct RptaddrsCache {
    last_hash: Option<u64>,
    last_emit: Option<u64>,
}

impl HostTelemetry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Push a RESULT line to be uploaded with the next batch.
    pub(crate) async fn push(&self, line: String) {
        let mut p = self.pending.lock().await;
        p.push(line);
        cap_pending(&mut p);
    }

    /// Drain all pending host-telemetry RESULT lines. Called by the uploader.
    pub async fn drain(&self) -> Vec<String> {
        let mut p = self.pending.lock().await;
        std::mem::take(&mut *p)
    }

    /// Put back the lines a FAILED upload drained, ahead of anything queued
    /// since, so the next upload carries them in their original order. The
    /// uploader requeues a failed batch's results; without this, the batch's
    /// telemetry was lost. That matters most for rptaddrs: its change cache
    /// re-sends an unchanged report only once an hour, so on 2026-10-12 the
    /// one address report of a probe that had never sent one (RIPE answered
    /// its upload 429) was gone for the hour.
    pub async fn restore(&self, mut lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        let mut p = self.pending.lock().await;
        lines.append(&mut p);
        *p = lines;
        cap_pending(&mut p);
    }

    /// Schedule periodic buddyinfo reports.
    ///
    /// `lowmem` is the threshold (KB) the controller asked the probe to flag
    /// at; we record it in the report for parity but never act on it.
    /// `interval == 0` runs once immediately.
    pub async fn schedule_buddyinfo(
        self: &Arc<Self>,
        interval: u64,
        lowmem: Option<u32>,
        msm_id: Option<u32>,
    ) {
        self.schedule(HostTelemetryKind::Buddyinfo, interval, move |_| {
            buddyinfo_line(lowmem, msm_id)
        })
        .await;
    }

    /// Schedule periodic rptaddrs reports.
    ///
    /// `msm_id` is the controller-assigned measurement id (the `-A` flag).
    pub async fn schedule_rptaddrs(self: &Arc<Self>, interval: u64, msm_id: u32) {
        self.schedule(HostTelemetryKind::Rptaddrs, interval, move |me| {
            rptaddrs_line(msm_id, &me)
        })
        .await;
    }

    /// Generic scheduler: spawns (or replaces) a task that calls `make_line()`
    /// every `interval` seconds and pushes its result. Runs once immediately
    /// to mirror eperd's behaviour of producing a first sample on schedule.
    async fn schedule<F>(self: &Arc<Self>, kind: HostTelemetryKind, interval: u64, make_line: F)
    where
        F: Fn(Arc<Self>) -> Option<String> + Send + Sync + 'static,
    {
        let me = Arc::clone(self);
        let new_handle = tokio::spawn(async move {
            // First report runs immediately, then every `interval`.
            loop {
                match make_line(Arc::clone(&me)) {
                    Some(line) => me.push(line).await,
                    None => trace!(?kind, "telemetry reporter produced no line"),
                }
                if interval == 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        });

        let mut tasks = self.tasks.lock().await;
        if let Some(old) = tasks.insert(kind, new_handle) {
            old.abort();
            debug!(?kind, interval, "replaced existing telemetry schedule");
        } else {
            debug!(?kind, interval, "scheduled telemetry");
        }
    }
}

/// The most lines kept pending. Lines are restored on every failed upload, so
/// an outage must not grow the list without bound. A report every 180 s that
/// changes is far below this; when over it, the oldest go first.
const MAX_PENDING: usize = 64;

fn cap_pending(p: &mut Vec<String>) {
    if p.len() > MAX_PENDING {
        let excess = p.len() - MAX_PENDING;
        p.drain(..excess);
    }
}

#[cfg(target_os = "linux")]
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// buddyinfo
// ---------------------------------------------------------------------------

/// Produce a buddyinfo RESULT line mirroring the official probe's id=9001
/// format. Returns None on non-Linux or if /proc/buddyinfo is unreadable.
///
/// LOWMEM_REBOOT is intentionally not implemented: starla's host is not the
/// embedded probe's tiny kernel and rebooting it is outside our authority.
pub fn buddyinfo_line(lowmem: Option<u32>, _msm_id: Option<u32>) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read_to_string("/proc/buddyinfo").ok()?;
        // /proc/buddyinfo lines look like:
        //   Node 0, zone   Normal   123  45  6  ...
        // Match the C applet: skip the first 4 tokens (Node, 0,, zone, Normal),
        // then collect integer columns and sum freemem = sum(block_kb_4 * count).
        let mut nums: Vec<u64> = Vec::new();
        for line in raw.lines() {
            for tok in line.split_whitespace().skip(4) {
                if let Ok(n) = tok.parse::<u64>() {
                    nums.push(n);
                }
            }
            // Official applet only reads the first zone block.
            if !nums.is_empty() {
                break;
            }
        }
        if nums.is_empty() {
            return None;
        }

        let mut freemem: u64 = 0;
        let mut block_kb: u64 = 4;
        for n in &nums {
            freemem = freemem.saturating_add(block_kb.saturating_mul(*n));
            block_kb = block_kb.saturating_mul(2);
        }

        let arr = nums
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ");

        let uptime = read_uptime_secs().unwrap_or(0);
        let now = unix_now();

        // We don't reboot, but record the lowmem threshold the controller
        // gave us so the result reflects what the probe was asked to watch.
        let _ = lowmem;

        Some(format!(
            "RESULT {{ \"id\": \"9001\", \"time\": {}, \"uptime\": {}, \"buddyinfo\": [ {} ], \
             \"freemem\": {} }}\n",
            now, uptime, arr, freemem
        ))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = lowmem;
        None
    }
}

#[cfg(target_os = "linux")]
fn read_uptime_secs() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/uptime").ok()?;
    s.split_whitespace().next()?.split('.').next()?.parse().ok()
}

// ---------------------------------------------------------------------------
// rptaddrs
// ---------------------------------------------------------------------------

/// Produce an rptaddrs RESULT line (msm 9104): the probe's IPv4 addresses
/// and routes, "inet-dhcp", its IPv6 addresses and routes, and its resolvers,
/// byte for byte as the reference's applet writes them
/// (probe-busybox/networking/rptaddrs.c, firmware 5130). The text is the
/// applet's, not a JSON serialiser's: its spacing differs between the IPv4
/// and IPv6 halves (`"inet-addr": ` but `"inet6-addr" : `), and RIPE gets the
/// same bytes from every reference probe.
///
/// Uses a change-detection cache: emits a fresh line when state changed or
/// when the last emission was > 1h ago, as the applet's `-c` cache file does
/// (check_cache, rptaddrs.c:683-774: a byte compare of the body, and a cache
/// older than 3600 s).
pub fn rptaddrs_line(msm_id: u32, telem: &Arc<HostTelemetry>) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let body = build_rptaddrs_body()?;

        let mut hasher = DefaultHasher::new();
        body.hash(&mut hasher);
        let h = hasher.finish();
        let now = unix_now();

        // Synchronous lock acquisition via blocking_lock would deadlock the
        // runtime; the reporter task uses a non-blocking shortcut: try to
        // get the cache, and if contended just emit (safer than dropping).
        let should_emit = match telem.rptaddrs_cache.try_lock() {
            Ok(mut cache) => {
                let stale = cache
                    .last_emit
                    .map(|t| now.saturating_sub(t) > 3600)
                    .unwrap_or(true);
                let changed = cache.last_hash.map(|prev| prev != h).unwrap_or(true);
                if changed || stale {
                    cache.last_hash = Some(h);
                    cache.last_emit = Some(now);
                    true
                } else {
                    false
                }
            }
            Err(_) => true,
        };

        if !should_emit {
            return None;
        }

        Some(rptaddrs_result(msm_id, now, &body))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (msm_id, telem);
        None
    }
}

/// The applet's ifreq array: SIOCGIFCONF is handed room for this many
/// entries, so the report lists at most 10 IPv4 addresses (rptaddrs.c:218,
/// 238).
#[cfg(target_os = "linux")]
const MAX_INF: usize = 10;

/// Linux's route flags as glibc's <net/route.h> has them, which is what
/// rptaddrs.c and busybox's route_set_flags.c are compiled against. Spelled
/// out because the pure formatting below is also built (and tested) off
/// Linux, where libc's RTF_* are the BSD values.
#[cfg(any(target_os = "linux", test))]
mod rtf {
    pub const UP: u32 = 0x0001;
    pub const GATEWAY: u32 = 0x0002;
    pub const HOST: u32 = 0x0004;
    pub const REINSTATE: u32 = 0x0008;
    pub const DYNAMIC: u32 = 0x0010;
    pub const MODIFIED: u32 = 0x0020;
    pub const REJECT: u32 = 0x0200;
    pub const DEFAULT: u32 = 0x0001_0000;
    pub const ADDRCONF: u32 = 0x0004_0000;
    pub const NONEXTHOP: u32 = 0x0020_0000;
    pub const CACHE: u32 = 0x0100_0000;
}

/// What the applet reads, gathered from the host by [`build_rptaddrs_body`]
/// or given by a test.
#[cfg(any(target_os = "linux", test))]
struct RptaddrsInputs<'a> {
    /// SIOCGIFCONF's entries, in the kernel's order: interface (label),
    /// address, and the netmask SIOCGIFNETMASK gave for it.
    inet: &'a [(String, Ipv4Addr, Ipv4Addr)],
    /// /proc/net/route.
    route: &'a str,
    /// /proc/net/if_inet6 and /proc/net/ipv6_route, or None when there is
    /// no if_inet6 (a host without IPv6), in which case the applet reads
    /// neither (rptaddrs.c:384-392).
    inet6: Option<(&'a str, &'a str)>,
    /// /etc/resolv.conf, or None when it cannot be opened.
    resolv_conf: Option<&'a [u8]>,
}

/// The RESULT line around a body: rpt_ipv6's `JS(id)` and `JS1(time)`, the
/// cache file copied as is, and `}` (rptaddrs.c:805-818).
#[cfg(any(target_os = "linux", test))]
fn rptaddrs_result(msm_id: u32, time: u64, body: &str) -> String {
    format!("RESULT {{ \"id\" : \"{msm_id}\" , \"time\" : {time} , {body}}}\n")
}

/// The body rptaddrs.c writes to its cache file, in its order: setup_ipv4_rpt,
/// setup_dhcpv4, setup_ipv6_rpt, setup_dns. None where the applet gives up
/// and reports nothing (a malformed /proc/net/ipv6_route line).
///
/// setup_static_rpt appends nothing here: its three
/// $ATLAS_STATUS/network_*_static_info.json files come from a registration
/// that configures the probe's network statically ("DHCPV4 False …",
/// atlasinit.c:1235-1419, a hardware-probe setting), which Starla's
/// registration does not act on, and the applet skips each absent file
/// (report_line, rptaddrs.c:641-655).
#[cfg(any(target_os = "linux", test))]
fn rptaddrs_body(inp: &RptaddrsInputs) -> Option<String> {
    let mut out = String::new();
    inet_addresses(&mut out, inp.inet);
    inet_routes(&mut out, inp.route);
    // setup_dhcpv4 (rptaddrs.c:313-364) reads "DHCP True|False" from
    // $ATLAS_STATUS/network_v4_info.txt and, when that file does not exist,
    // writes `"inet-dhcp": true` ("configured for DHCP but didn't get a DHCP
    // lease"). Nothing in firmware 5130 writes that file (only rptaddrs.c
    // names it), so a software probe always reports true, as the reference
    // probe did in the twin run of 2026-10-13. Starla has no such file
    // either.
    out.push_str(", \"inet-dhcp\": true");
    if let Some((if_inet6, ipv6_route)) = inp.inet6 {
        inet6_addresses(&mut out, if_inet6);
        inet6_routes(&mut out, ipv6_route)?;
    }
    dns(&mut out, inp.resolv_conf);
    Some(out)
}

/// `"inet-addresses": [ … ]` from SIOCGIFCONF (rptaddrs.c:248-273).
#[cfg(any(target_os = "linux", test))]
fn inet_addresses(out: &mut String, inet: &[(String, Ipv4Addr, Ipv4Addr)]) {
    out.push_str("\"inet-addresses\": [ ");
    for (i, (name, addr, mask)) in inet.iter().enumerate() {
        let _ = write!(
            out,
            "{}{{ \"inet-addr\": \"{addr}\", \"netmask\": \"{mask}\", \"interface\": \"{name}\" }}",
            if i == 0 { "" } else { ", " }
        );
    }
    out.push_str(" ]");
}

/// `, "inet-routes": [ … ]`: every line of /proc/net/route after the header,
/// unfiltered (rptaddrs.c:275-306).
#[cfg(any(target_os = "linux", test))]
fn inet_routes(out: &mut String, route: &str) {
    out.push_str(", \"inet-routes\": [ ");
    let mut first = true;
    for line in route.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 {
            continue;
        }
        let _ = write!(
            out,
            "{}{{ \"destination\": \"{}\", \"netmask\": \"{}\", \"next-hop\": \"{}\", \
             \"interface\": \"{}\" }}",
            if first { "" } else { ", " },
            hex_to_v4(f[1]),
            hex_to_v4(f[7]),
            hex_to_v4(f[2]),
            f[0]
        );
        first = false;
    }
    out.push_str(" ]");
}

/// A /proc/net/route address: the kernel prints the __be32 as a host-order
/// integer, and the applet stores the integer it scanned straight back into
/// s_addr, so the bytes come back in memory order.
#[cfg(any(target_os = "linux", test))]
fn hex_to_v4(hex: &str) -> Ipv4Addr {
    Ipv4Addr::from(u32::from_str_radix(hex, 16).unwrap_or(0).to_ne_bytes())
}

/// `, "inet6-addresses" : [ … ]` from /proc/net/if_inet6, in the file's
/// order, nothing at all when it lists no address (rptaddrs.c:396-467).
///
/// The file's order is the kernel's: it walks its address hash table, whose
/// hash mixes in a per-network-namespace random value, so the same addresses
/// come out in a different order on another host or container (as the two
/// probes of the twin run did). Sorting would not match the reference either.
#[cfg(any(target_os = "linux", test))]
fn inet6_addresses(out: &mut String, if_inet6: &str) {
    let mut n = 0;
    for line in if_inet6.lines() {
        // "%4s"x8 " %08x %02x %02x %02x %20s": address, ifindex,
        // prefix length, scope, flags, interface.
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 6 {
            continue;
        }
        let Some(addr) = hex_to_v6(f[0]) else {
            continue;
        };
        let prefix_len = i32::from_str_radix(f[2], 16).unwrap_or(0);
        let scope = i32::from_str_radix(f[3], 16).unwrap_or(0);
        // IPV6_ADDR_SCOPE_MASK and the scope names (rptaddrs.c:56-62,
        // 412-430); an unknown scope is printed whole, in decimal.
        let scope = match scope & 0x00f0 {
            0x0000 => "Global".to_string(),
            0x0020 => "Link".to_string(),
            0x0040 => "Site".to_string(),
            0x0080 => "Compat".to_string(),
            0x0010 => "Host".to_string(),
            _ => format!("Unknown {scope}"),
        };
        let _ = write!(
            out,
            "{} {}{{\"inet6-addr\" : \"{}\", \"prefix-length\" : {prefix_len},\"scope\" : \
             \"{scope}\", \"interface\" : \"{}\"}}",
            if n == 0 {
                ", \"inet6-addresses\" : ["
            } else {
                ""
            },
            if n == 0 { "" } else { ", " },
            ntop6(addr),
            f[5]
        );
        n += 1;
    }
    if n > 0 {
        out.push(']');
    }
}

/// `, "inet6-routes" : [ … ]` from /proc/net/ipv6_route, nothing at all when
/// no route is left after the applet's filters (rptaddrs.c:471-582). None for
/// a line that is not the kernel's ten fields: the applet's fscanf then
/// matches fewer than 23 items and it gives up on the whole report.
#[cfg(any(target_os = "linux", test))]
fn inet6_routes(out: &mut String, ipv6_route: &str) -> Option<()> {
    let mut n = 0;
    for line in ipv6_route.lines() {
        // dest plen src plen next-hop metric refcnt use flags interface
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.is_empty() {
            continue; // fscanf skips white space, blank lines included
        }
        if f.len() != 10 || f[2].len() != 32 {
            return None;
        }
        let dst = hex_to_v6(f[0])?;
        let prefix_len = i32::from_str_radix(f[1], 16).ok()?;
        let nh = hex_to_v6(f[4])?;
        // Scanned with %x into an int and printed with %d, so a metric of
        // ffffffff is -1.
        let metric = u32::from_str_radix(f[5], 16).ok()? as i32;
        let flags = u32::from_str_radix(f[8], 16).ok()?;

        // The applet's filters (rptaddrs.c:495-511): routes that are not up,
        // cache entries, the ff02 and ff00 multicast routes, host routes.
        if flags & rtf::UP == 0 {
            continue;
        }
        if flags & rtf::ADDRCONF != 0 && flags & rtf::CACHE != 0 {
            continue;
        }
        if f[0].starts_with("ff02") || f[0].starts_with("ff00") {
            continue;
        }
        if prefix_len == 128 {
            continue;
        }

        let _ = write!(
            out,
            "{} {}{{\"destination\" : \"{}\", \"prefix-length\" : {prefix_len},\"next-hop\" : \
             \"{}\", \"flags\" : \"{}\", \"metric\" : {metric} , \"interface\" : \"{}\"}}",
            if n == 0 { ", \"inet6-routes\" : [" } else { "" },
            if n == 0 { "" } else { ", " },
            ntop6(dst),
            ntop6(nh),
            route_flags(flags & IPV6_MASK),
            f[9]
        );
        n += 1;
    }
    if n > 0 {
        out.push(']');
    }
    Some(())
}

/// The flags the applet reports: rptaddrs.c:53's IPV6_MASK. RTF_UP is not
/// in it, so an ordinary connected route reports "".
#[cfg(any(target_os = "linux", test))]
const IPV6_MASK: u32 = rtf::GATEWAY | rtf::HOST | rtf::DEFAULT | rtf::ADDRCONF | rtf::CACHE;

/// busybox's route_set_flags (libbb/route_set_flags.c, built with
/// FEATURE_IPV6): one letter per set flag, in this order. RTF_DYNAMIC and
/// RTF_DEFAULT are both "D".
#[cfg(any(target_os = "linux", test))]
fn route_flags(flags: u32) -> String {
    const FLAGS: [(u32, char); 11] = [
        (rtf::UP, 'U'),
        (rtf::GATEWAY, 'G'),
        (rtf::HOST, 'H'),
        (rtf::REINSTATE, 'R'),
        (rtf::DYNAMIC, 'D'),
        (rtf::MODIFIED, 'M'),
        (rtf::DEFAULT, 'D'),
        (rtf::ADDRCONF, 'A'),
        (rtf::CACHE, 'C'),
        (rtf::REJECT, '!'),
        (rtf::NONEXTHOP, 'n'),
    ];
    FLAGS
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|(_, c)| c)
        .collect()
}

/// An address in /proc/net's 32-hex-digit form.
#[cfg(any(target_os = "linux", test))]
fn hex_to_v6(hex: &str) -> Option<Ipv6Addr> {
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u128::from_str_radix(hex, 16).ok().map(Ipv6Addr::from)
}

/// An IPv6 address as the applet prints it: INET6_rresolve(…, 0x0fff) is
/// getnameinfo(NI_NUMERICHOST) with no scope id, i.e. glibc's inet_ntop, the
/// RFC 5952 form that Ipv6Addr's Display also writes (the longest run of two
/// or more zero groups, the first of equal runs, as "::"). One difference:
/// glibc still writes an IPv4-compatible address (::a.b.c.d, RFC 4291
/// 2.5.5.1) with its last 32 bits dotted, and Rust does not. Those are the
/// addresses a sit0 tunnel carries, with scope Compat.
#[cfg(any(target_os = "linux", test))]
fn ntop6(addr: Ipv6Addr) -> String {
    let s = addr.segments();
    if s[..6] == [0; 6] && s[6] != 0 {
        let o = addr.octets();
        return format!("::{}.{}.{}.{}", o[12], o[13], o[14], o[15]);
    }
    addr.to_string()
}

/// `, "dns": [ … ]` (setup_dns, rptaddrs.c:595-616) from
/// get_local_resolvers (eperd/readresolv.c): /etc/resolv.conf read with
/// fgets into a 92-byte buffer (LINEL, INET6_ADDRSTRLEN * 2), at most MAXNS
/// (3) servers, a line counting when its first " \t" token is exactly
/// "nameserver"; the server is the next token, less one trailing newline.
#[cfg(any(target_os = "linux", test))]
fn dns(out: &mut String, resolv_conf: Option<&[u8]>) {
    const LINEL: usize = 46 * 2;
    const MAXNS: usize = 3;

    out.push_str(", \"dns\": [ ");
    let mut i = 0;
    for line in fgets(resolv_conf.unwrap_or_default(), LINEL) {
        if i == MAXNS {
            break;
        }
        // A C string ends at its first NUL.
        let line = line.split(|&b| b == 0).next().unwrap_or_default();
        let mut tokens = line
            .split(|&b| b == b' ' || b == b'\t')
            .filter(|t| !t.is_empty());
        if tokens.next() != Some(b"nameserver".as_slice()) {
            continue;
        }
        let Some(ns) = tokens.next() else {
            continue;
        };
        let ns = ns.strip_suffix(b"\n").unwrap_or(ns);
        let _ = write!(
            out,
            "{}{{ \"nameserver\": \"{}\" }}",
            if i == 0 { "" } else { ", " },
            String::from_utf8_lossy(ns)
        );
        i += 1;
    }
    out.push_str(" ]");
}

/// What successive fgets(buf, size, f) calls return: a line with its
/// newline, or the first size - 1 bytes of a longer one, whose rest comes
/// back as the next piece.
#[cfg(any(target_os = "linux", test))]
fn fgets(data: &[u8], size: usize) -> impl Iterator<Item = &[u8]> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let max = rest.len().min(size - 1);
        let end = rest[..max]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(max, |p| p + 1);
        let (piece, tail) = rest.split_at(end);
        rest = tail;
        Some(piece)
    })
}

/// Gather what the applet reads and build its body, failing where it fails:
/// no socket or SIOCGIFCONF/SIOCGIFNETMASK error, no /proc/net/route, an
/// if_inet6 that exists but cannot be read, an ipv6_route that cannot be
/// opened when if_inet6 exists.
#[cfg(target_os = "linux")]
fn build_rptaddrs_body() -> Option<String> {
    let inet = siocgifconf()?;
    let route = std::fs::read_to_string("/proc/net/route").ok()?;
    let if_inet6 = match std::fs::read_to_string("/proc/net/if_inet6") {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return None,
    };
    let ipv6_route = if if_inet6.is_some() {
        Some(std::fs::read_to_string("/proc/net/ipv6_route").ok()?)
    } else {
        None
    };
    let resolv_conf = std::fs::read("/etc/resolv.conf").ok();
    rptaddrs_body(&RptaddrsInputs {
        inet: &inet,
        route: &route,
        inet6: if_inet6.as_deref().zip(ipv6_route.as_deref()),
        resolv_conf: resolv_conf.as_deref(),
    })
}

/// The IPv4 addresses as the applet lists them: SIOCGIFCONF into room for
/// MAX_INF entries, then SIOCGIFNETMASK for each (rptaddrs.c:229-273).
#[cfg(target_os = "linux")]
fn siocgifconf() -> Option<Vec<(String, Ipv4Addr, Ipv4Addr)>> {
    use std::ffi::CStr;
    use std::mem::{size_of, zeroed};

    fn sin_addr(sa: &libc::sockaddr) -> Ipv4Addr {
        // SIOCGIFCONF lists AF_INET addresses only; s_addr is in network
        // order in memory, which is what inet_ntoa prints.
        let sin = unsafe { &*(sa as *const libc::sockaddr as *const libc::sockaddr_in) };
        Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes())
    }

    unsafe {
        let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_IP);
        if s < 0 {
            return None;
        }
        let mut req_buf: [libc::ifreq; MAX_INF] = zeroed();
        let mut ifc: libc::ifconf = zeroed();
        ifc.ifc_len = (size_of::<libc::ifreq>() * req_buf.len()) as i32;
        ifc.ifc_ifcu.ifcu_buf = req_buf.as_mut_ptr() as *mut libc::c_char;

        if libc::ioctl(s, libc::SIOCGIFCONF as _, &mut ifc) < 0 {
            libc::close(s);
            return None;
        }

        let count = ifc.ifc_len as usize / size_of::<libc::ifreq>();
        let mut out = Vec::with_capacity(count);
        for ifr in req_buf.iter().take(count) {
            let mut ifr_mask: libc::ifreq = zeroed();
            ifr_mask.ifr_name = ifr.ifr_name;
            if libc::ioctl(s, libc::SIOCGIFNETMASK as _, &mut ifr_mask) < 0 {
                libc::close(s);
                return None;
            }
            out.push((
                CStr::from_ptr(ifr.ifr_name.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
                sin_addr(&ifr.ifr_ifru.ifru_addr),
                sin_addr(&ifr_mask.ifr_ifru.ifru_addr),
            ));
        }
        libc::close(s);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- rptaddrs fixtures: the reference probe of the twin run ----------
    //
    // What a container like the twin's reference probe (172.30.74.31,
    // 2001:db8:7769:6e00::31 on eth0, no gateway) has in /proc, in the
    // kernel's own formats ("%pi6 %02x %02x %02x %02x %8s" for if_inet6,
    // "%pi6 %02x %pi6 %02x %pi6 %08x %08x %08x %08x %8s" for ipv6_route,
    // laid out as a Docker container's were dumped on 2026-10-13).

    fn twin_inet() -> Vec<(String, Ipv4Addr, Ipv4Addr)> {
        vec![
            (
                "lo".into(),
                Ipv4Addr::new(127, 0, 0, 1),
                Ipv4Addr::new(255, 0, 0, 0),
            ),
            (
                "eth0".into(),
                Ipv4Addr::new(172, 30, 74, 31),
                Ipv4Addr::new(255, 255, 255, 0),
            ),
        ]
    }

    /// As a little-endian kernel writes it, less the spaces it pads each line
    /// with to 127 bytes (the tests that read its addresses are little-endian
    /// only).
    const TWIN_ROUTE: &str = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
eth0\t004A1EAC\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0
";

    /// In the order the reference's file listed them.
    const TWIN_IF_INET6: &str = "\
20010db877696e000000000000000031 8d 40 00 80     eth0
fe80000000000000387cedfffe5e33a2 8d 40 20 80     eth0
00000000000000000000000000000001 01 80 10 80       lo
";

    const TWIN_IPV6_ROUTE: &str =
        "\
20010db877696e000000000000000000 40 00000000000000000000000000000000 00 \
         00000000000000000000000000000000 00000100 00000001 00000000 00000001     eth0
fe800000000000000000000000000000 40 00000000000000000000000000000000 00 \
         00000000000000000000000000000000 00000100 00000001 00000000 00000001     eth0
00000000000000000000000000000001 80 00000000000000000000000000000000 00 \
         00000000000000000000000000000000 00000000 00000003 00000000 80200001       lo
20010db877696e000000000000000031 80 00000000000000000000000000000000 00 \
         00000000000000000000000000000000 00000000 00000002 00000000 80200001     eth0
fe80000000000000387cedfffe5e33a2 80 00000000000000000000000000000000 00 \
         00000000000000000000000000000000 00000000 00000002 00000000 80200001     eth0
ff000000000000000000000000000000 08 00000000000000000000000000000000 00 \
         00000000000000000000000000000000 00000100 00000002 00000000 00000001     eth0
00000000000000000000000000000000 00 00000000000000000000000000000000 00 \
         00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo
";

    /// The twin's run/resolv.conf, mounted into both probes.
    const TWIN_RESOLV_CONF: &[u8] = b"\
# atlas-twin: every name resolves through the target container
nameserver 172.30.74.20
nameserver 2001:db8:7769:6e00::20
";

    /// The reference probe's 9104 line in the twin run of 2026-10-13
    /// (wire-ctrl.jsonl, its upload's body), verbatim.
    const TWIN_REFERENCE_LINE: &str =
        "RESULT { \"id\" : \"9104\" , \"time\" : 1791871112 , \"inet-addresses\": [ { \
         \"inet-addr\": \"127.0.0.1\", \"netmask\": \"255.0.0.0\", \"interface\": \"lo\" }, { \
         \"inet-addr\": \"172.30.74.31\", \"netmask\": \"255.255.255.0\", \"interface\": \"eth0\" \
         } ], \"inet-routes\": [ { \"destination\": \"172.30.74.0\", \"netmask\": \
         \"255.255.255.0\", \"next-hop\": \"0.0.0.0\", \"interface\": \"eth0\" } ], \
         \"inet-dhcp\": true, \"inet6-addresses\" : [ {\"inet6-addr\" : \
         \"2001:db8:7769:6e00::31\", \"prefix-length\" : 64,\"scope\" : \"Global\", \"interface\" \
         : \"eth0\"} , {\"inet6-addr\" : \"fe80::387c:edff:fe5e:33a2\", \"prefix-length\" : \
         64,\"scope\" : \"Link\", \"interface\" : \"eth0\"} , {\"inet6-addr\" : \"::1\", \
         \"prefix-length\" : 128,\"scope\" : \"Host\", \"interface\" : \"lo\"}], \"inet6-routes\" \
         : [ {\"destination\" : \"2001:db8:7769:6e00::\", \"prefix-length\" : 64,\"next-hop\" : \
         \"::\", \"flags\" : \"\", \"metric\" : 256 , \"interface\" : \"eth0\"} , \
         {\"destination\" : \"fe80::\", \"prefix-length\" : 64,\"next-hop\" : \"::\", \"flags\" : \
         \"\", \"metric\" : 256 , \"interface\" : \"eth0\"}], \"dns\": [ { \"nameserver\": \
         \"172.30.74.20\" }, { \"nameserver\": \"2001:db8:7769:6e00::20\" } ]}\n";

    fn body(inet6: Option<(&str, &str)>, resolv_conf: Option<&[u8]>) -> Option<String> {
        rptaddrs_body(&RptaddrsInputs {
            inet: &twin_inet(),
            route: TWIN_ROUTE,
            inet6,
            resolv_conf,
        })
    }

    #[cfg(target_endian = "little")]
    #[test]
    fn rptaddrs_line_is_the_reference_line_of_the_twin_run() {
        let body = body(
            Some((TWIN_IF_INET6, TWIN_IPV6_ROUTE)),
            Some(TWIN_RESOLV_CONF),
        )
        .expect("a report");
        assert_eq!(
            rptaddrs_result(9104, 1_791_871_112, &body),
            TWIN_REFERENCE_LINE
        );
        // The line is also JSON, as RIPE parses it.
        let json = TWIN_REFERENCE_LINE
            .trim_end()
            .strip_prefix("RESULT ")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(v["inet-dhcp"], true);
        assert_eq!(v["inet6-routes"][0]["flags"], "");
    }

    /// The kernel lists if_inet6 in its hash order, which differs per network
    /// namespace: Starla's container listed its addresses Link, Global, Host
    /// in the twin run where the reference's listed Global, Link, Host. The
    /// applet writes them in the file's order; so does Starla, unsorted.
    #[test]
    fn inet6_addresses_keep_the_files_order() {
        let mut out = String::new();
        inet6_addresses(
            &mut out,
            "\
fe80000000000000787c53fffea68c95 8d 40 20 80     eth0
20010db877696e000000000000000032 8d 40 00 80     eth0
00000000000000000000000000000001 01 80 10 80       lo
",
        );
        assert_eq!(
            out,
            ", \"inet6-addresses\" : [ {\"inet6-addr\" : \"fe80::787c:53ff:fea6:8c95\", \
             \"prefix-length\" : 64,\"scope\" : \"Link\", \"interface\" : \"eth0\"} , \
             {\"inet6-addr\" : \"2001:db8:7769:6e00::32\", \"prefix-length\" : 64,\"scope\" : \
             \"Global\", \"interface\" : \"eth0\"} , {\"inet6-addr\" : \"::1\", \"prefix-length\" \
             : 128,\"scope\" : \"Host\", \"interface\" : \"lo\"}]"
        );
    }

    /// An empty report has the applet's empty forms: "[  ]" for the IPv4
    /// lists and resolvers, and no inet6 keys at all, whether the host has
    /// no IPv6 (no if_inet6, so ipv6_route is not read) or lists nothing.
    #[test]
    fn empty_parts_are_written_as_the_applet_writes_them() {
        let empty = |inet6: Option<(&'static str, &'static str)>| {
            rptaddrs_body(&RptaddrsInputs {
                inet: &[],
                route: TWIN_ROUTE.lines().next().unwrap(),
                inet6,
                resolv_conf: None,
            })
            .unwrap()
        };
        let want =
            "\"inet-addresses\": [  ], \"inet-routes\": [  ], \"inet-dhcp\": true, \"dns\": [  ]";
        assert_eq!(empty(None), want);
        assert_eq!(empty(Some(("", ""))), want);
        // Addresses but no route left after the filters: only the addresses.
        let only_lo = empty(Some((
            "00000000000000000000000000000001 01 80 10 80       lo\n",
            "00000000000000000000000000000001 80 00000000000000000000000000000000 00 \
             00000000000000000000000000000000 00000000 00000003 00000000 80200001       lo\n",
        )));
        assert!(
            only_lo.ends_with("\"interface\" : \"lo\"}], \"dns\": [  ]"),
            "{only_lo}"
        );
        assert!(!only_lo.contains("inet6-routes"), "{only_lo}");
    }

    /// INET6_rresolve(…, 0x0fff) is getnameinfo(NI_NUMERICHOST), i.e.
    /// glibc's inet_ntop. Every expected string here is what glibc 2.43 wrote
    /// for the address (2026-10-13, in the twin's Ubuntu 26.04 build image).
    #[test]
    fn ipv6_addresses_are_written_as_glibc_writes_them() {
        for (addr, glibc) in [
            ("2001:db8:7769:6e00:0:0:0:32", "2001:db8:7769:6e00::32"),
            ("2001:db8:7769:6e00:0:0:0:0", "2001:db8:7769:6e00::"),
            (
                "fe80:0:0:0:387c:edff:fe5e:33a2",
                "fe80::387c:edff:fe5e:33a2",
            ),
            ("0:0:0:0:0:0:0:1", "::1"),
            ("0:0:0:0:0:0:0:0", "::"),
            ("2001:db8:0:1:0:0:0:1", "2001:db8:0:1::1"),
            ("1:0:0:2:0:0:0:3", "1:0:0:2::3"),
            ("2001:db8:0:0:1:0:0:1", "2001:db8::1:0:0:1"),
            ("0:0:0:0:0:1:0:0", "::1:0:0"),
            ("1:0:2:0:3:0:4:0", "1:0:2:0:3:0:4:0"),
            ("0:0:0:0:0:ffff:102:304", "::ffff:1.2.3.4"),
            // IPv4-compatible: glibc dots the last 32 bits, Rust does not.
            ("0:0:0:0:0:0:ac1e:4a1f", "::172.30.74.31"),
            ("0:0:0:0:0:0:1:2", "::0.1.0.2"),
            ("0:0:0:0:0:0:100:0", "::1.0.0.0"),
        ] {
            assert_eq!(ntop6(addr.parse().unwrap()), glibc, "{addr}");
        }
        assert_eq!(
            hex_to_v6("20010db877696e000000000000000031"),
            Some("2001:db8:7769:6e00::31".parse().unwrap())
        );
        assert_eq!(hex_to_v6("20010db8"), None);
    }

    /// Scope names from IPV6_ADDR_SCOPE_MASK, an unknown scope as
    /// "Unknown <decimal>", and a sit0 address in glibc's dotted form.
    #[test]
    fn inet6_scopes_are_the_applets() {
        let mut out = String::new();
        inet6_addresses(
            &mut out,
            "\
000000000000000000000000ac1e4a1f 04 60 80 80     sit0
fec00000000000000000000000000001 02 40 40 80     eth0
20010db8000000000000000000000001 02 40 50 80     eth0
",
        );
        assert_eq!(
            out,
            ", \"inet6-addresses\" : [ {\"inet6-addr\" : \"::172.30.74.31\", \"prefix-length\" : \
             96,\"scope\" : \"Compat\", \"interface\" : \"sit0\"} , {\"inet6-addr\" : \
             \"fec0::1\", \"prefix-length\" : 64,\"scope\" : \"Site\", \"interface\" : \"eth0\"} \
             , {\"inet6-addr\" : \"2001:db8::1\", \"prefix-length\" : 64,\"scope\" : \"Unknown \
             80\", \"interface\" : \"eth0\"}]"
        );
    }

    /// busybox's route_set_flags letters for the bits in IPV6_MASK: an
    /// ordinary route is "" (RTF_UP is not in the mask), a router's default
    /// route learned from an RA is "GDA".
    #[test]
    fn route_flags_are_busyboxs_letters_under_ipv6_mask() {
        assert_eq!(route_flags(0x0000_0001 & IPV6_MASK), "");
        assert_eq!(route_flags(0x0000_0003 & IPV6_MASK), "G");
        // RTF_UP|GATEWAY|DEFAULT|ADDRCONF|EXPIRES: an RA default route.
        assert_eq!(route_flags(0x0045_0003 & IPV6_MASK), "GDA");
        assert_eq!(route_flags(IPV6_MASK), "GHDAC");
        // The whole table, both D's included.
        assert_eq!(route_flags(0xffff_ffff), "UGHRDMDAC!n");
    }

    /// The applet's route filters (not up, ADDRCONF+CACHE, ff00/ff02, /128),
    /// glibc's <net/route.h> values for them, and the metric printed with %d.
    #[test]
    fn inet6_routes_are_filtered_and_flagged_as_the_applet_does() {
        let route = |dst: &str, plen: &str, nh: &str, metric: &str, flags: &str| {
            format!(
                "{dst} {plen} 00000000000000000000000000000000 00 {nh} {metric} 00000001 00000000 \
                 {flags}     eth0\n"
            )
        };
        let zero = "00000000000000000000000000000000";
        let fe80_1 = "fe800000000000000000000000000001";
        let table = [
            // A default route from an RA: kept, "GDA".
            route(zero, "00", fe80_1, "00000400", "00450003"),
            // A static default route through a gateway: "G".
            route(zero, "00", fe80_1, "00000400", "00000003"),
            // Down: skipped.
            route(
                "20010db8000000010000000000000000",
                "40",
                zero,
                "00000100",
                "00000000",
            ),
            // RTF_ADDRCONF|RTF_CACHE: skipped.
            route(
                "20010db8000000020000000000000000",
                "40",
                zero,
                "00000100",
                "01040001",
            ),
            // RTF_POLICY|RTF_CACHE (0x04000000 is not RTF_ADDRCONF): kept, "C".
            route(
                "20010db8000000030000000000000000",
                "40",
                zero,
                "00000100",
                "05000001",
            ),
            // ff02 and ff00: skipped; ff0e is not.
            route(
                "ff020000000000000000000000000000",
                "10",
                zero,
                "00000100",
                "00000001",
            ),
            route(
                "ff000000000000000000000000000000",
                "08",
                zero,
                "00000100",
                "00000001",
            ),
            route(
                "ff0e0000000000000000000000000000",
                "10",
                zero,
                "00000100",
                "00000001",
            ),
            // A host route: skipped.
            route(
                "20010db8000000040000000000000001",
                "80",
                zero,
                "00000000",
                "00000001",
            ),
            // Metric ffffffff, scanned into an int: -1.
            route(
                "20010db8000000050000000000000000",
                "40",
                zero,
                "ffffffff",
                "00000001",
            ),
        ]
        .concat();
        let mut out = String::new();
        inet6_routes(&mut out, &table).unwrap();
        assert_eq!(
            out,
            ", \"inet6-routes\" : [ {\"destination\" : \"::\", \"prefix-length\" : 0,\"next-hop\" \
             : \"fe80::1\", \"flags\" : \"GDA\", \"metric\" : 1024 , \"interface\" : \"eth0\"} , \
             {\"destination\" : \"::\", \"prefix-length\" : 0,\"next-hop\" : \"fe80::1\", \
             \"flags\" : \"G\", \"metric\" : 1024 , \"interface\" : \"eth0\"} , {\"destination\" \
             : \"2001:db8:0:3::\", \"prefix-length\" : 64,\"next-hop\" : \"::\", \"flags\" : \
             \"C\", \"metric\" : 256 , \"interface\" : \"eth0\"} , {\"destination\" : \"ff0e::\", \
             \"prefix-length\" : 16,\"next-hop\" : \"::\", \"flags\" : \"\", \"metric\" : 256 , \
             \"interface\" : \"eth0\"} , {\"destination\" : \"2001:db8:0:5::\", \"prefix-length\" \
             : 64,\"next-hop\" : \"::\", \"flags\" : \"\", \"metric\" : -1 , \"interface\" : \
             \"eth0\"}]"
        );
    }

    /// A line the applet's fscanf cannot match in full makes it give up on
    /// the report (rptaddrs.c:486-491), so Starla reports nothing either.
    #[test]
    fn a_malformed_ipv6_route_line_means_no_report() {
        let cut = TWIN_IPV6_ROUTE.replacen("00000001     eth0", "00000001", 1);
        assert!(body(Some((TWIN_IF_INET6, &cut)), None).is_none());
        assert!(body(Some((TWIN_IF_INET6, TWIN_IPV6_ROUTE)), None).is_some());
    }

    /// get_local_resolvers: the first " \t" token must be "nameserver", the
    /// server is the next token without its newline, at most three, read in
    /// fgets pieces of 91 bytes.
    #[test]
    fn dns_is_read_as_get_local_resolvers_reads_it() {
        let dns_of = |conf: &[u8]| {
            let mut out = String::new();
            dns(&mut out, Some(conf));
            out
        };
        assert_eq!(
            dns_of(
                b"# comment\nsearch example.net\nnameservers 9.9.9.9\nnameserver\n\
                  nameserver 1.1.1.1 # a comment\n \tnameserver\t2.2.2.2\n\
                  nameserver 3.3.3.3\nnameserver 4.4.4.4\n"
            ),
            ", \"dns\": [ { \"nameserver\": \"1.1.1.1\" }, { \"nameserver\": \"2.2.2.2\" }, { \
             \"nameserver\": \"3.3.3.3\" } ]"
        );
        // A line longer than the buffer goes on in the next piece, which is
        // read as a line of its own.
        let mut long = vec![b'#'];
        long.extend([b'x'; 90]);
        long.extend(b"nameserver 5.5.5.5\n");
        assert_eq!(
            dns_of(&long),
            ", \"dns\": [ { \"nameserver\": \"5.5.5.5\" } ]"
        );
        // No file: no servers.
        let mut out = String::new();
        dns(&mut out, None);
        assert_eq!(out, ", \"dns\": [  ]");
    }

    #[cfg(target_endian = "little")]
    #[test]
    fn hex_v4_round_trip() {
        // /proc/net/route prints the __be32 as a host-order integer.
        assert_eq!(hex_to_v4("0100A8C0"), Ipv4Addr::new(192, 168, 0, 1));
        assert_eq!(hex_to_v4("00000000"), Ipv4Addr::UNSPECIFIED);
    }

    #[tokio::test]
    async fn restore_puts_lines_back_ahead_of_new_ones_and_caps() {
        let t = HostTelemetry::new();
        t.push("new".into()).await;
        t.restore(vec!["old-1".into(), "old-2".into()]).await;
        assert_eq!(t.drain().await, ["old-1", "old-2", "new"]);
        t.restore((0..MAX_PENDING + 5).map(|i| i.to_string()).collect())
            .await;
        let kept = t.drain().await;
        assert_eq!(kept.len(), MAX_PENDING);
        assert_eq!(kept[0], "5", "the oldest go first");
    }

    #[tokio::test]
    async fn drain_is_idempotent() {
        let t = HostTelemetry::new();
        let drained = t.drain().await;
        assert!(drained.is_empty());
    }

    #[tokio::test]
    async fn buddyinfo_returns_something_on_linux() {
        if cfg!(target_os = "linux") {
            // Don't assert format details; just that it runs without panic
            // on a real Linux host. CI on macOS gets None and skips.
            let _ = buddyinfo_line(None, None);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn buddyinfo_line_shape_matches_official_probe() {
        // Should produce a line of the form:
        //   RESULT { "id": "9001", "time": ..., "uptime": ..., \
        //            "buddyinfo": [...], "freemem": ... }
        let Some(line) = buddyinfo_line(Some(4), None) else {
            return; // CI host might not have /proc/buddyinfo (rare)
        };
        assert!(line.starts_with("RESULT { "));
        assert!(line.contains("\"id\": \"9001\""));
        assert!(line.contains("\"buddyinfo\":"));
        assert!(line.contains("\"freemem\":"));
        assert!(line.ends_with("}\n"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn rptaddrs_emits_once_then_caches() {
        let t = HostTelemetry::new();
        // First call: state is "new" relative to empty cache → emits.
        let first = rptaddrs_line(9104, &t);
        assert!(first.is_some(), "rptaddrs should emit first time");
        // Immediately again: unchanged + recent → suppressed.
        let second = rptaddrs_line(9104, &t);
        assert!(second.is_none(), "rptaddrs should suppress unchanged");
    }

    /// The probe schedules rptaddrs itself at startup, because RIPE's
    /// controllers never do (binaries/starla-probe, `RPTADDRS_*`). That only
    /// reaches RIPE if the first report is queued at once, not after the
    /// first 180 s, and carries the IPv4 side: addresses AND routes, which is
    /// what RIPE judges IPv4 by.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_scheduled_rptaddrs_reports_at_once_with_both_families() {
        let t = HostTelemetry::new();
        t.schedule_rptaddrs(180, 9104).await;
        let mut lines = Vec::new();
        for _ in 0..50 {
            tokio::task::yield_now().await;
            lines = t.drain().await;
            if !lines.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(lines.len(), 1, "one report, queued at once: {lines:?}");
        let line = &lines[0];
        assert!(
            line.starts_with("RESULT { \"id\" : \"9104\" , \"time\" : "),
            "{line}"
        );
        assert!(
            line.contains("\"inet-addresses\": [ { \"inet-addr\": \"127.0.0.1\""),
            "{line}"
        );
        assert!(line.contains("\"inet-routes\": ["), "{line}");
        assert!(line.contains("\"inet-dhcp\": true"), "{line}");
        assert!(line.ends_with(" ]}\n"), "{line}");
    }

    #[tokio::test]
    async fn one_shot_schedule_drains() {
        let t = HostTelemetry::new();
        // interval=0 runs once and exits. Use buddyinfo on Linux, otherwise
        // skip — non-linux producers are None and won't push anything.
        if cfg!(target_os = "linux") {
            t.schedule_buddyinfo(0, None, None).await;
            // Spin briefly for the spawn to land. One-shot is synchronous
            // inside the spawned task; we only need the runtime to step.
            for _ in 0..20 {
                tokio::task::yield_now().await;
                let pending = t.drain().await;
                if !pending.is_empty() {
                    assert!(pending[0].contains("\"id\": \"9001\""));
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("buddyinfo never landed in drain");
        }
    }
}
