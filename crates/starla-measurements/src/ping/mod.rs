//! Ping measurement implementation

pub mod icmp;
#[cfg(target_os = "linux")]
pub mod scamper;

use crate::target::{DstName, LookupError};
use crate::traits::Measurement;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use starla_common::{
    MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId, Timestamp,
};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
use std::time::Duration;

/// Which implementation runs the ping.
///
/// `Native` is the in-process ICMP socket implementation and is always
/// available. `Scamper` delegates to a running scamper daemon via the
/// rscamper bindings; it is compiled in on Linux only, so selecting it on
/// any other platform is a runtime error.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PingBackend {
    #[default]
    Native,
    Scamper,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingConfig {
    pub target: IpAddr,
    pub count: u32,
    /// `-s` as given, 0 without one (ping.c:1262): the data after the ICMP
    /// header, see [`data_len`].
    pub size: u16,
    pub ttl: u8,
    /// `-i`, 1000 without one (ping.c:44, 1266): the time from one echo
    /// request to the next, which is also all the time a request has for
    /// its reply. ping_xmit arms one timer for it (ping.c:785-787), and
    /// when it fires noreply_callback reports the timeout, if any, and
    /// sends the next request (ping.c:800-812). Starla waited up to 5 s for
    /// each reply and then 1 s more.
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
    #[serde(default)]
    pub backend: PingBackend,
    /// What the measurement's runs leave for the next one.
    #[serde(skip)]
    pub kept: Arc<PingKept>,
}

/// What ping.c's pingstate keeps from one run of a measurement to the
/// next. eperd allocates it once per measurement (xzalloc, ping.c:1361)
/// and a run resets only some of it (ping_start, ping.c:1758-1765;
/// ping_start2, ping.c:1470-1481), so a line can carry what an earlier run
/// left:
///
/// - a run whose socket or connect fails reports the "size" and the
///   "src_addr" (loc_sin6) kept, 0 and "" before any run
///   (ping.c:1482-1580, 283-298), where Starla always wrote 0 and "";
/// - a reply whose TTL the backend cannot tell gets rcvd_ttl, the last
///   reply's that had one, in this run or an earlier one, 0 before any:
///   ping.c only sets it from an IPv4 header or an IPV6_HOPLIMIT
///   (ping.c:960, 1155-1162), and hands it on whether set or not
///   (ping.c:1183). Starla gave such a reply the TTL it sent with, 64;
/// - a failed lookup or a refused address keeps the `-s` as given as the
///   size (ping.c:454-466), an empty answer cursize (PING_ERR_DNS_NO_ADDR,
///   ping.c:1733-1744, 355-357), though it reports nothing;
/// - and pingstate->busy: a run is under way ([`PingKept::start`]).
///
/// One per measurement line, shared by its runs ([`PingKept::for_line`]).
#[derive(Debug, Default)]
pub struct PingKept {
    kept: Mutex<Kept>,
    /// pingstate->busy, from ping_start to report() (ping.c:1765, 324)
    busy: AtomicBool,
}

/// A run of a ping line under way, from ping_start to report(): the line
/// is busy until this goes ([`PingKept::start`]).
#[derive(Debug)]
pub struct PingRun<'a> {
    kept: &'a PingKept,
}

impl Drop for PingRun<'_> {
    /// report() clears busy (ping.c:324), whatever the run came to.
    fn drop(&mut self) {
        self.kept.busy.store(false, Ordering::Release);
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Kept {
    /// state->size
    size: usize,
    /// state->cursize: ping_start2 sets it to the `-s` as given
    /// (ping.c:1471), fmticmp4/6 to the data length at the first send
    /// (ping.c:542-546, 620-624)
    cursize: usize,
    /// loc_sin6; None while it is all zeros, which report() writes as ""
    loc: Option<IpAddr>,
    /// rcvd_ttl
    rcvd_ttl: u8,
    /// An empty answer left the line busy for good: ping_cb has no case
    /// for PING_ERR_DNS_NO_ADDR (ping.c:1733-1744, 328-482), so report()
    /// never clears busy, and every later ping_start returns at once.
    busy_for_good: bool,
}

/// A ping crontab line, as eperd's Insert() tells that a line it is given
/// is one it already runs: the same interval, start time and command
/// (eperd.c:781-827). It then keeps the measurement it has, pingstate and
/// all; here, the command is its parts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PingLine {
    pub msm_id: u64,
    pub target: String,
    pub af: u8,
    pub packets: u32,
    pub packet_interval: u32,
    pub size: u16,
    pub interval: u64,
    pub start_time: i64,
}

/// The kept state of the periodic lines that are scheduled; an entry goes
/// dead when its measurement does.
static LINES: LazyLock<Mutex<HashMap<PingLine, Weak<PingKept>>>> = LazyLock::new(Default::default);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl PingKept {
    /// The state for a line the controller delivers. A periodic line
    /// (`interval` > 0) identical to one still scheduled shares that one's
    /// state, as Insert() keeps the old line; a new or changed line, or a
    /// one-off (eooqd inits each afresh, eooqd.c:546), starts with nothing
    /// kept.
    pub fn for_line(line: PingLine) -> Arc<Self> {
        if line.interval == 0 {
            return Arc::default();
        }
        let mut lines = lock(&LINES);
        lines.retain(|_, kept| kept.strong_count() > 0);
        if let Some(live) = lines.get(&line).and_then(Weak::upgrade) {
            return live;
        }
        let new = Arc::new(Self::default());
        lines.insert(line, Arc::downgrade(&new));
        new
    }

    /// ping_start: a run starts, unless the line's last run is still under
    /// way, and it checks that before it looks the target up
    /// (`if (pingstate->busy) return;`, ping.c:1751-1752, 1787-1798): None
    /// then, and the run reports nothing. Starla's scheduler starts every
    /// due run, so a ping that outlasted the gap to its next run (`-c 16
    /// -i 60000` on a 60 s line, or a spread) reported twice, both runs
    /// sharing this state. After an empty answer, never again.
    pub fn start(&self) -> Option<PingRun<'_>> {
        if self.get().busy_for_good || self.busy.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(PingRun { kept: self })
    }

    fn get(&self) -> Kept {
        *lock(&self.kept)
    }

    fn set(&self, kept: Kept) {
        *lock(&self.kept) = kept;
    }
}

fn default_interval_ms() -> u64 {
    DEFAULT_INTERVAL_MS
}

/// DEFAULT_PING_INTERVAL (ping.c:44)
pub const DEFAULT_INTERVAL_MS: u64 = 1000;

/// The ICMP and ICMPv6 echo header: ICMP_MINLEN, and ICMP6_HDRSIZE
/// (ping.c:52).
pub const ICMP_HEADER_LEN: usize = 8;

/// The least data an echo request carries: sizeof(struct evdata), the
/// send time (a struct timespec), the uint32_t index and the 8-byte cookie
/// (ping.c:186-190). That is 32 on the 64-bit hosts the software probe
/// runs on, and it is why a ping without `-s` reports "size":32.
pub const MIN_DATA_LEN: usize = 32;

/// The most: MAX_DATA_SIZE less the ICMP header (ping.c:50, 545, 623).
pub const MAX_DATA_LEN: usize = 4096 - 20 - ICMP_HEADER_LEN;

/// The data bytes an echo request carries for `-s size`. The reference
/// sends `-s` bytes after the 8-byte header, raised to [`MIN_DATA_LEN`] and
/// capped at [`MAX_DATA_LEN`] (fmticmp4/fmticmp6, ping.c:542-546,
/// 620-624), and reports that number as "size". Starla counted the header
/// in `-s`: on the twin of 2026-10-13 its `-s 60` went out as 60 bytes of
/// ICMPv6 against the reference's 68, its `-s 1460` (1468 + 40 = 1508
/// bytes, which fragments at a 1500 MTU: the point of that size) fitted in
/// 1500, and without `-s` it sent 56 data bytes and reported "size":64
/// where the reference sends and reports 32.
pub fn data_len(size: u16) -> usize {
    usize::from(size).clamp(MIN_DATA_LEN, MAX_DATA_LEN)
}

/// What ping_cb is called with during a run, in order (ping.c:328-482).
#[derive(Debug, Clone, PartialEq)]
pub enum PingEvent {
    /// ping_xmit sent an echo request (ping.c:686-791). `local` is the
    /// socket's address as getsockname gives it then (ping.c:707-718,
    /// 745-747), which report() writes as "src_addr" and replies are
    /// compared with. `error` is sendto's strerror when it failed: an
    /// `{ "error":"sendto failed: …" }` entry (PING_ERR_SENDTO,
    /// ping.c:768-781, 442-448).
    Sent {
        local: Option<IpAddr>,
        error: Option<String>,
    },
    /// An echo reply to one of this run's requests (ready_callback4/6,
    /// ping.c:944-982, 1120-1191): `dup` when its sequence number is not
    /// the one awaited (PING_ERR_DUP). `size` is its data after the ICMP
    /// header, `ttl` its IPv4 TTL or IPv6 hop limit (None when the backend
    /// cannot tell), `dst` the local address it came to (None when the
    /// backend cannot tell).
    Reply {
        dup: bool,
        rtt: f64,
        ttl: Option<u8>,
        size: usize,
        dst: Option<IpAddr>,
    },
    /// The interval ran out before any reply of the run had come
    /// (noreply_callback, PING_ERR_TIMEOUT).
    Timeout,
}

/// What a backend's run came to.
#[derive(Debug, Clone, PartialEq)]
pub enum PingOutcome {
    /// The requests went out: the socket's address once connected
    /// (ping_start2's getsockname, ping.c:1582-1597) and the events.
    Ran {
        local: Option<IpAddr>,
        events: Vec<PingEvent>,
    },
    /// No socket, or it would not connect: ping_start2's one entry,
    /// "socket failed: <strerror>" or "connect failed: <strerror>"
    /// (ping.c:1482-1495, 1514-1525, 1569-1580).
    Failed(String),
}

/// strerror's text for an OS error: what ping.c writes after "sendto
/// failed: " and the like. std's Display adds " (os error N)".
pub fn strerror(e: &std::io::Error) -> String {
    let text = e.to_string();
    match text.rfind(" (os error ") {
        Some(at) if e.raw_os_error().is_some() => text[..at].to_string(),
        _ => text,
    }
}

/// The sequence numbers and timeouts of a run, as ping.c's socket side
/// keeps them. `seq` starts at 1 (ping.c:1402) and wraps at 256; every
/// request carries the current one. A reply whose number is not the
/// current one is a duplicate, "This is not quite right, it could be a
/// late packet" (ping.c:962-965, 1174-1177). IPv4 moves to the next number
/// on a reply that is not a duplicate (ping.c:974-979), IPv6 on any reply
/// (ping.c:1186-1190), both on a reported timeout (ping.c:809). And a
/// timeout is only reported while no reply has come in the whole run, nor
/// a send failed (`!got_reply && !send_error`, ping.c:804): after the
/// first reply a lost request leaves no entry at all, so a 3-packet ping
/// that loses its second reply reports `[ {rtt}, {rtt} ]`. Of RIPE's
/// latest results of msm 1001 and 2001 (-c 3) on 2026-10-13, the 7,735
/// from probes on firmware 5130 include 28 with fewer than three entries
/// and no error, and none with a timeout after a reply; Starla wrote
/// `{ "x":"*" }` for every lost request.
#[derive(Debug)]
pub(crate) struct Session {
    v6: bool,
    seq: u8,
    got_reply: bool,
    send_error: bool,
    events: Vec<PingEvent>,
}

impl Session {
    pub(crate) fn new(v6: bool) -> Self {
        Self {
            v6,
            seq: 1,
            got_reply: false,
            send_error: false,
            events: Vec::new(),
        }
    }

    /// The sequence number the next request carries.
    pub(crate) fn seq(&self) -> u8 {
        self.seq
    }

    /// ping_xmit sent a request, or tried to.
    pub(crate) fn sent(&mut self, local: Option<IpAddr>, error: Option<String>) {
        self.send_error |= error.is_some();
        self.events.push(PingEvent::Sent { local, error });
    }

    /// An echo reply of this run came with sequence number `seq`.
    pub(crate) fn reply(
        &mut self,
        seq: u16,
        rtt: f64,
        ttl: Option<u8>,
        size: usize,
        dst: Option<IpAddr>,
    ) {
        let dup = seq != u16::from(self.seq);
        self.events.push(PingEvent::Reply {
            dup,
            rtt,
            ttl,
            size,
            dst,
        });
        self.got_reply |= !dup;
        if self.v6 || !dup {
            self.seq = self.seq.wrapping_add(1);
        }
    }

    /// The interval since the last request ran out.
    pub(crate) fn interval_over(&mut self) {
        if !self.got_reply && !self.send_error {
            self.events.push(PingEvent::Timeout);
            self.seq = self.seq.wrapping_add(1);
        }
    }

    pub(crate) fn into_events(self) -> Vec<PingEvent> {
        self.events
    }
}

/// One echo request as a backend that runs the whole ping itself
/// (scamper) recorded it: when it went out, counted from the first, and
/// its replies.
#[derive(Debug, Clone, Default)]
pub(crate) struct Probe {
    pub sent: Duration,
    pub local: Option<IpAddr>,
    pub replies: Vec<ProbeReply>,
}

#[derive(Debug, Clone)]
pub(crate) struct ProbeReply {
    pub rtt: Duration,
    pub ttl: Option<u8>,
    pub size: usize,
}

/// Plays a recorded run through [`Session`] in time order, as ping.c
/// would have seen it: each request has `interval` for replies, and its
/// interval is over when the next goes out, a reply counts as the
/// sequence number its request was sent with, and replies after the last
/// interval are not seen (the run is reported then).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn replay(v6: bool, interval: Duration, probes: &[Probe]) -> Vec<PingEvent> {
    // (when, what): 0 the interval of request i is over, 1 request i went
    // out, 2 reply j of request i came; at the same instant ping.c's timer
    // goes first, as it sends the next request.
    let mut timeline: Vec<(Duration, u8, usize, usize)> = Vec::new();
    for (i, probe) in probes.iter().enumerate() {
        // ping.c sends the next request from the timer that ends this
        // one's interval, after it reported the timeout and moved to the
        // next sequence number (ping.c:800-812): when scamper sent the next
        // one a little early, the interval ended then. Ordered by
        // scamper's times alone, the next request took the old number and
        // its reply came out a duplicate.
        let mut over = probe.sent + interval;
        if let Some(next) = probes.get(i + 1) {
            over = over.min(next.sent.max(probe.sent));
        }
        timeline.push((probe.sent, 1, i, 0));
        timeline.push((over, 0, i, 0));
        for (j, reply) in probe.replies.iter().enumerate() {
            timeline.push((probe.sent + reply.rtt, 2, i, j));
        }
    }
    timeline.sort_by_key(|&(when, what, i, j)| (when, what, i, j));
    let mut session = Session::new(v6);
    let mut seq_of = vec![0u8; probes.len()];
    for (_, what, i, j) in timeline {
        match what {
            0 => {
                session.interval_over();
                if i + 1 == probes.len() {
                    break;
                }
            }
            1 => {
                seq_of[i] = session.seq();
                session.sent(probes[i].local, None);
            }
            _ => {
                let reply = &probes[i].replies[j];
                session.reply(
                    u16::from(seq_of[i]),
                    reply.rtt.as_secs_f64() * 1e3,
                    reply.ttl,
                    reply.size,
                    None,
                );
            }
        }
    }
    session.into_events()
}

/// What ping_cb leaves for report() once the run is over.
#[derive(Debug, PartialEq)]
struct Entries {
    /// The result array, brackets included
    result: String,
    /// The TTL kept, once a reply came (got_reply, ping.c:295-296)
    ttl: Option<u8>,
    /// The size kept (ping.c:355-357, 395-401)
    size: usize,
    /// loc_sin6 when report() writes it
    src_addr: Option<IpAddr>,
    /// rcvd_ttl at the end of the run
    rcvd_ttl: u8,
}

/// The result array as ping_cb builds it (ping.c:328-482). The first
/// entry sets the size and TTL kept: a reply's own, the request's data
/// size and -1 (255 in its unsigned char) for a timeout, the data size and
/// 0 for a failed send (ping.c:355-360). A reply is `{ "rtt":… }`, with
/// `"dup":1, ` first when it is a duplicate, then its `"size"` if it
/// differs from the size kept and its `"ttl"` if it differs from the TTL
/// kept, each of which it then becomes, and its `"src_addr"` if the local
/// address it came to is not the socket's (ping.c:362-431). So a reply's
/// "ttl" is written where the TTL changes from the previous entry's, not,
/// as RIPE's documentation has it, wherever it differs from the first
/// reply's; and the line's "ttl" is the last one kept, not the first
/// reply's. Starla reported the TTL it sent with (64) and no reply's ttl,
/// size, src_addr or dup. A reply whose TTL the backend could not tell
/// (Windows over IPv6, scamper without one) counts as rcvd_ttl, the last
/// reply's that had one, starting from the `rcvd_ttl` the measurement kept
/// ([`PingKept`]).
///
/// `local` is the socket's address once connected; every request
/// refreshes it from getsockname, and the first reply that is not a
/// duplicate sets it to the address it came to (ping.c:381-387): what the
/// line's "src_addr" is at the end.
fn ping_cb(
    events: &[PingEvent],
    data_len: usize,
    local: Option<IpAddr>,
    mut rcvd_ttl: u8,
) -> Entries {
    let mut entries: Vec<String> = Vec::new();
    let mut size = data_len;
    let mut ttl: u8 = 0;
    let mut got_reply = false;
    let mut loc = local;
    for event in events {
        let first = entries.is_empty();
        match event {
            PingEvent::Sent { local, error } => {
                loc = *local;
                if let Some(error) = error {
                    if first {
                        size = data_len;
                        ttl = 0;
                    }
                    entries.push(format!("{{ \"error\":\"sendto failed: {error}\" }}"));
                }
            }
            PingEvent::Timeout => {
                if first {
                    size = data_len;
                    ttl = u8::MAX;
                }
                entries.push("{ \"x\":\"*\" }".to_string());
            }
            PingEvent::Reply {
                dup,
                rtt,
                ttl: reply_ttl,
                size: bytes,
                dst,
            } => {
                if let Some(ttl) = reply_ttl {
                    rcvd_ttl = *ttl;
                }
                let reply_ttl = rcvd_ttl;
                if first {
                    size = *bytes;
                    ttl = reply_ttl;
                }
                let mut entry = String::from("{ ");
                if *dup {
                    entry.push_str("\"dup\":1, ");
                }
                entry.push_str(&format!("\"rtt\":{rtt:.6}"));
                if !got_reply && !dup {
                    if dst.is_some() {
                        loc = *dst;
                    }
                    got_reply = true;
                }
                if size != *bytes {
                    entry.push_str(&format!(", \"size\":{bytes}"));
                    size = *bytes;
                }
                if ttl != reply_ttl {
                    entry.push_str(&format!(", \"ttl\":{reply_ttl}"));
                    ttl = reply_ttl;
                }
                if let (Some(dst), Some(loc)) = (dst, loc) {
                    if *dst != loc {
                        entry.push_str(&format!(", \"src_addr\":\"{dst}\""));
                    }
                }
                entry.push_str(" }");
                entries.push(entry);
            }
        }
    }
    Entries {
        result: format!("[ {} ]", entries.join(", ")),
        ttl: got_reply.then_some(ttl),
        size,
        src_addr: loc,
        rcvd_ttl,
    }
}

/// What ping.c's report() writes, in its order (ping.c:238-303).
struct Report<'a> {
    msm_id: u64,
    /// atlas_time() when the report is written
    time: i64,
    dst_name: &'a DstName,
    /// The family -4/-6 asked for
    af: u8,
    /// None after a failed lookup (no_dst, ping.c:1764, 274-281)
    dst_addr: Option<IpAddr>,
    /// None after a failed lookup or a disallowed address (no_src,
    /// ping.c:283-292, 473-474); "" for a socket that never got one
    /// (getnameinfo of a zeroed loc_sin6)
    src_addr: Option<String>,
    /// The TTL kept, only once a reply came (got_reply, ping.c:295-296)
    ttl: Option<u8>,
    size: usize,
    /// The result array, brackets included
    result: &'a str,
}

impl Report<'_> {
    /// The RESULT line's body. `"lts":0` is the placeholder the results
    /// crate replaces with the lts the result was queued at.
    fn body(&self) -> String {
        let mut s = format!(
            "\"id\":\"{}\", \"fw\":{}, \"mver\": \"2.6.4\", \"lts\":0, \"time\":{}, {}, \"af\":{}",
            self.msm_id,
            starla_common::FIRMWARE_VERSION,
            self.time,
            self.dst_name.fields(),
            self.af,
        );
        if let Some(dst) = self.dst_addr {
            s.push_str(&format!(", \"dst_addr\":\"{dst}\""));
            if let Some(src) = &self.src_addr {
                s.push_str(&format!(", \"src_addr\":\"{src}\""));
            }
        }
        s.push_str(", \"proto\":\"ICMP\"");
        if let Some(ttl) = self.ttl {
            s.push_str(&format!(", \"ttl\":{ttl}"));
        }
        s.push_str(&format!(
            ", \"size\":{}, \"result\": {}",
            self.size, self.result
        ));
        s
    }
}

pub struct Ping {
    pub config: PingConfig,
    pub probe_id: ProbeId,
    pub msm_id: MeasurementId,
    /// The target as the result names it: the controller's name for it,
    /// and the time its lookup took.
    pub dst_name: DstName,
}

impl Ping {
    fn af(&self) -> u8 {
        if self.config.target.is_ipv4() {
            4
        } else {
            6
        }
    }

    fn result(&self, report: Report<'_>) -> MeasurementResult {
        MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Ping,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: Timestamp(report.time),
            af: report.af,
            dst_addr: self.config.target,
            dst_name: Some(self.dst_name.name.clone()),
            src_addr: report.src_addr.as_deref().and_then(|s| s.parse().ok()),
            proto: Some("ICMP".to_string()),
            ttl: report.ttl,
            size: u16::try_from(report.size).ok(),
            data: MeasurementData::FullLine(report.body()),
        }
    }

    /// The line for a run's outcome (report(), ping.c:238-303), and what
    /// the run leaves kept.
    fn outcome_result(&self, outcome: &PingOutcome) -> MeasurementResult {
        let time = Timestamp::now().0;
        let mut kept = self.config.kept.get();
        let maxsize = usize::from(self.config.size);
        let result = match outcome {
            PingOutcome::Ran { local, events } => {
                let data_len = data_len(self.config.size);
                let entries = ping_cb(events, data_len, *local, kept.rcvd_ttl);
                let sent = events.iter().any(|e| matches!(e, PingEvent::Sent { .. }));
                kept = Kept {
                    size: entries.size,
                    cursize: if sent { data_len } else { maxsize },
                    loc: entries.src_addr,
                    rcvd_ttl: entries.rcvd_ttl,
                    ..kept
                };
                self.result(Report {
                    msm_id: self.msm_id.0,
                    time,
                    dst_name: &self.dst_name,
                    af: self.af(),
                    dst_addr: Some(self.config.target),
                    src_addr: Some(
                        entries
                            .src_addr
                            .map(|ip| ip.to_string())
                            .unwrap_or_default(),
                    ),
                    ttl: entries.ttl,
                    size: entries.size,
                    result: &entries.result,
                })
            }
            // ping_start2 reports before any ping_cb of the run: the size
            // and loc_sin6 kept, 0 and all zeros (written "") before any
            // run — RIPE stores `"src_addr":"", "size":0` for "connect
            // failed: Network is unreachable" from probes without IPv6.
            PingOutcome::Failed(error) => {
                kept.cursize = maxsize;
                self.result(Report {
                    msm_id: self.msm_id.0,
                    time,
                    dst_name: &self.dst_name,
                    af: self.af(),
                    dst_addr: Some(self.config.target),
                    src_addr: Some(kept.loc.map(|ip| ip.to_string()).unwrap_or_default()),
                    ttl: None,
                    size: kept.size,
                    result: &format!("[ {{ \"error\":\"{error}\" }} ]"),
                })
            }
        };
        self.config.kept.set(kept);
        result
    }

    /// dns_cb's reports that run no ping: the name and ttr, "size" the `-s`
    /// value as given (maxsize: 0 without `-s`), no ttl, and one error
    /// entry (ping.c:454-476, 1649-1662, 1706-1720).
    fn dns_cb_result(&self, dst_addr: Option<IpAddr>, error: &str) -> MeasurementResult {
        let mut kept = self.config.kept.get();
        kept.size = usize::from(self.config.size);
        self.config.kept.set(kept);
        self.result(Report {
            msm_id: self.msm_id.0,
            time: Timestamp::now().0,
            dst_name: &self.dst_name,
            af: self.af(),
            dst_addr,
            src_addr: None,
            ttl: None,
            size: usize::from(self.config.size),
            result: &format!("[ {{ \"error\":\"{error}\" }} ]"),
        })
    }
}

#[async_trait]
impl Measurement for Ping {
    fn measurement_type(&self) -> starla_common::MeasurementType {
        starla_common::MeasurementType::Ping
    }

    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        let outcome = match self.config.backend {
            PingBackend::Native => icmp::execute_ping(&self.config).await?,
            #[cfg(target_os = "linux")]
            PingBackend::Scamper => scamper::execute_ping(&self.config).await?,
            #[cfg(not(target_os = "linux"))]
            PingBackend::Scamper => {
                anyhow::bail!("scamper backend is only available on Linux")
            }
        };
        Ok(self.outcome_result(&outcome))
    }

    /// The result ping.c reports when the target's lookup fails: dns_cb
    /// calls ping_cb with PING_ERR_DNS (ping.c:1649-1662, 454-462), which
    /// reports the name and ttr, the family asked for, no dst_addr or
    /// src_addr, "size" the `-s` value as given and one error entry with
    /// evutil_gai_strerror's text. An empty answer (PING_ERR_DNS_NO_ADDR)
    /// reports nothing (ping.c:1733-1744, ping_cb has no case for it), and
    /// as report() never runs the line stays busy: it reports nothing
    /// again ([`PingKept::start`]). `self.config.target` only carries the
    /// family asked for.
    fn lookup_failed(&self, error: LookupError) -> Option<MeasurementResult> {
        if error == LookupError::NoAddress {
            // ping_cb keeps cursize as the size, and reports nothing
            let mut kept = self.config.kept.get();
            kept.size = kept.cursize;
            kept.busy_for_good = true;
            self.config.kept.set(kept);
            return None;
        }
        Some(self.dns_cb_result(None, &format!("dns resolution failed: {error}")))
    }

    /// The result ping.c reports when the target's address is one
    /// atlas_check_addr refuses: PING_ERR_BAD_ADDR, with dst_addr but no
    /// src_addr (no_dst 0, no_src 1) and "address not allowed"
    /// (ping.c:1706-1720, 464-476).
    fn address_not_allowed(&self) -> Option<MeasurementResult> {
        Some(self.dns_cb_result(Some(self.config.target), "address not allowed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, Value};

    /// fmticmp's bounds: -s is the data after the header, at least
    /// sizeof(struct evdata), at most MAX_DATA_SIZE - 8.
    #[test]
    fn s_is_the_data_after_the_header() {
        assert_eq!(data_len(0), 32);
        assert_eq!(data_len(1), 32);
        assert_eq!(data_len(32), 32);
        assert_eq!(data_len(48), 48);
        assert_eq!(data_len(60), 60);
        assert_eq!(data_len(1460), 1460);
        assert_eq!(data_len(4068), 4068);
        assert_eq!(data_len(5000), 4068);
    }

    fn ip(addr: &str) -> Option<IpAddr> {
        Some(addr.parse().unwrap())
    }

    fn literal(addr: &str) -> DstName {
        DstName {
            name: addr.into(),
            ttr: None,
        }
    }

    fn ping(target: &str, count: u32, size: u16, dst_name: DstName) -> Ping {
        Ping {
            config: PingConfig {
                target: target.parse().unwrap(),
                count,
                size,
                ttl: 64,
                interval_ms: DEFAULT_INTERVAL_MS,
                backend: PingBackend::Native,
                kept: Default::default(),
            },
            probe_id: ProbeId(1018212),
            msm_id: MeasurementId(2001),
            dst_name,
        }
    }

    fn body(result: &MeasurementResult) -> &str {
        match &result.data {
            MeasurementData::FullLine(body) => body,
            other => panic!("{other:?}"),
        }
    }

    /// The line as RIPE stores it, less "lts": "id" is its msm_id and
    /// "time" its timestamp, which the comparison leaves out.
    fn line(result: &MeasurementResult) -> Map<String, Value> {
        let line: Value = serde_json::from_str(&format!("{{{}}}", body(result))).unwrap();
        let mut line = line.as_object().unwrap().clone();
        for key in ["id", "time", "lts"] {
            assert!(line.remove(key).is_some(), "{key}");
        }
        line
    }

    /// What the probe sent, of a result RIPE stored: without what its
    /// backend adds (msm_id from "id", timestamp from "time", prb_id,
    /// from, msm_name, type, step, stored_timestamp, group_id, and the
    /// aggregates dup, rcvd, sent, min, max, avg) and "lts".
    fn probe_fields(stored: &str) -> Map<String, Value> {
        let stored: Value = serde_json::from_str(stored).unwrap();
        let mut stored = stored.as_object().unwrap().clone();
        for key in [
            "msm_id",
            "timestamp",
            "prb_id",
            "from",
            "msm_name",
            "type",
            "step",
            "stored_timestamp",
            "group_id",
            "dup",
            "rcvd",
            "sent",
            "min",
            "max",
            "avg",
            "lts",
        ] {
            stored.remove(key);
        }
        stored
    }

    fn assert_stored_as(result: &MeasurementResult, stored: &str) {
        assert_eq!(line(result), probe_fields(stored), "{}", body(result));
    }

    /// A run of `replies` (rtt, ttl) to one request each, as ping.c sees
    /// it: send, replies, interval over.
    fn run(v6: bool, local: Option<IpAddr>, replies: &[Option<(f64, u8)>]) -> PingOutcome {
        let mut session = Session::new(v6);
        for reply in replies {
            let seq = session.seq();
            session.sent(local, None);
            if let Some((rtt, ttl)) = *reply {
                session.reply(u16::from(seq), rtt, Some(ttl), 32, local);
            }
            session.interval_over();
        }
        PingOutcome::Ran {
            local,
            events: session.into_events(),
        }
    }

    /// ref-2001.json, the reference probe's latest msm 2001 as RIPE
    /// stored it: the replies' hop limit, 59, where Starla's 44 stored
    /// lines of 2001 all have "ttl":64 (starla-2001.json), and "size":32.
    #[test]
    fn ref_2001_as_stored() {
        let p = ping("2001:7fd::1", 3, 0, literal("2001:7fd::1"));
        let outcome = run(
            true,
            ip("2001:418:1::33"),
            &[
                Some((3.643373, 59)),
                Some((3.692488, 59)),
                Some((3.747431, 59)),
            ],
        );
        let result = p.outcome_result(&outcome);
        assert_stored_as(
            &result,
            r#"{"fw":5130,"mver":"2.6.4","lts":41,"dst_name":"2001:7fd::1","af":6,"dst_addr":"2001:7fd::1","src_addr":"2001:418:1::33","proto":"ICMP","ttl":59,"size":32,"result":[{"rtt":3.643373},{"rtt":3.692488},{"rtt":3.747431}],"dup":0,"rcvd":3,"sent":3,"min":3.643373,"max":3.747431,"avg":3.694430666666667,"msm_id":2001,"prb_id":1000004,"timestamp":1791885799,"msm_name":"Ping","from":"2001:418:1::33","type":"ping","step":240,"stored_timestamp":1791885883}"#,
        );
        assert_eq!(result.ttl, Some(59));
        assert_eq!(result.src_addr, ip("2001:418:1::33"));
        // in report()'s order, with its spacing
        assert!(body(&result).ends_with(
            r#""dst_name":"2001:7fd::1", "af":6, "dst_addr":"2001:7fd::1", "src_addr":"2001:418:1::33", "proto":"ICMP", "ttl":59, "size":32, "result": [ { "rtt":3.643373 }, { "rtt":3.692488 }, { "rtt":3.747431 } ]"#
        ), "{}", body(&result));
    }

    /// ref-86206980.json: a name (`evping -6 -c 3 -s 48 -i 1000`), its
    /// ttr, its hop limit 57 and "size":48. Starla stored the address as
    /// dst_name, no ttr and "ttl":64 (starla-86206980.json).
    #[test]
    fn ref_86206980_as_stored() {
        let mut p = ping(
            "2a03:2880:f35a:1:face:b00c:0:25de",
            3,
            48,
            DstName {
                name: "facebook.com".into(),
                ttr: Some(18.216213),
            },
        );
        p.msm_id = MeasurementId(86206980);
        let mut session = Session::new(true);
        for rtt in [66.511808, 66.596867, 66.626635] {
            let seq = session.seq();
            session.sent(ip("2001:418:1::33"), None);
            session.reply(u16::from(seq), rtt, Some(57), 48, ip("2001:418:1::33"));
            session.interval_over();
        }
        let outcome = PingOutcome::Ran {
            local: ip("2001:418:1::33"),
            events: session.into_events(),
        };
        assert_stored_as(
            &p.outcome_result(&outcome),
            r#"{"fw":5130,"mver":"2.6.4","lts":29,"dst_name":"facebook.com","ttr":18.216213,"af":6,"dst_addr":"2a03:2880:f35a:1:face:b00c:0:25de","src_addr":"2001:418:1::33","proto":"ICMP","ttl":57,"size":48,"result":[{"rtt":66.511808},{"rtt":66.596867},{"rtt":66.626635}],"dup":0,"rcvd":3,"sent":3,"min":66.511808,"max":66.626635,"avg":66.57843666666666,"msm_id":86206980,"prb_id":1000004,"timestamp":1791886098,"msm_name":"Ping","from":"2001:418:1::33","type":"ping","group_id":86206978,"step":240,"stored_timestamp":1791886176}"#,
        );
    }

    /// ref-1517506.json (`-c 5 -s 60`, nothing answers): a timeout each,
    /// "size":60 and no ttl.
    #[test]
    fn ref_1517506_as_stored() {
        let p = ping(
            "2a01:4f8:110:3169::37:134",
            5,
            60,
            literal("2a01:4f8:110:3169::37:134"),
        );
        let outcome = run(true, ip("2001:418:1::33"), &[None; 5]);
        assert_stored_as(
            &p.outcome_result(&outcome),
            r#"{"fw":5130,"mver":"2.6.4","lts":23,"dst_name":"2a01:4f8:110:3169::37:134","af":6,"dst_addr":"2a01:4f8:110:3169::37:134","src_addr":"2001:418:1::33","proto":"ICMP","size":60,"result":[{"x":"*"},{"x":"*"},{"x":"*"},{"x":"*"},{"x":"*"}],"dup":0,"rcvd":0,"sent":5,"min":-1,"max":-1,"avg":-1,"msm_id":1517506,"prb_id":1000004,"timestamp":1791867602,"msm_name":"Ping","from":"2001:418:1::33","type":"ping","group_id":1517506,"step":28800,"stored_timestamp":1791867650}"#,
        );
    }

    /// ref-1040114.json: a name that does not resolve is reported, by
    /// name and ttr, with no dst_addr, src_addr or ttl, "size":0 (no -s)
    /// and dns_cb's error. Starla stored nothing for 1040114 or 1040121.
    #[test]
    fn ref_1040114_as_stored() {
        let p = ping(
            "::",
            16,
            0,
            DstName {
                name: "bits.wikimedia.org".into(),
                ttr: Some(5170.500508),
            },
        );
        let result = p.lookup_failed(LookupError::NoName).unwrap();
        assert_stored_as(
            &result,
            r#"{"fw":5130,"mver":"2.6.4","lts":104,"dst_name":"bits.wikimedia.org","ttr":5170.500508,"af":6,"proto":"ICMP","size":0,"result":[{"error":"dns resolution failed: nodename nor servname provided, or not known"}],"dup":0,"rcvd":0,"sent":0,"min":-1,"max":-1,"avg":-1,"msm_id":1040114,"prb_id":1000004,"timestamp":1791884486,"msm_name":"Ping","from":"2001:418:1::33","type":"ping","group_id":1040114,"step":21600,"stored_timestamp":1791884570}"#,
        );
        assert!(body(&result).ends_with(r#""dst_name":"bits.wikimedia.org", "ttr":5170.500508, "af":6, "proto":"ICMP", "size":0, "result": [ { "error":"dns resolution failed: nodename nor servname provided, or not known" } ]"#));
        assert_eq!((result.src_addr, result.ttl), (None, None));
        // -4 with -s 48: the -s as given; any other failure's text
        let result = ping("0.0.0.0", 3, 48, p.dst_name.clone())
            .lookup_failed(LookupError::Fail)
            .unwrap();
        assert!(body(&result).ends_with(r#""af":4, "proto":"ICMP", "size":48, "result": [ { "error":"dns resolution failed: non-recoverable failure in name resolution" } ]"#), "{}", body(&result));
        // an empty answer: nothing (PING_ERR_DNS_NO_ADDR)
        assert!(p.lookup_failed(LookupError::NoAddress).is_none());
    }

    /// PING_ERR_BAD_ADDR: dst_addr, no src_addr, the -s as given, "address
    /// not allowed" (ping.c:464-476, 1706-1720).
    #[test]
    fn a_refused_address_is_reported() {
        let p = ping(
            "fd00::20",
            3,
            0,
            DstName {
                name: "lan.example".into(),
                ttr: Some(1.5),
            },
        );
        let result = p.address_not_allowed().unwrap();
        assert!(body(&result).ends_with(r#""dst_name":"lan.example", "ttr":1.500000, "af":6, "dst_addr":"fd00::20", "proto":"ICMP", "size":0, "result": [ { "error":"address not allowed" } ]"#), "{}", body(&result));
    }

    /// RIPE's latest msm 1001 and 2001 results from probes on firmware
    /// 5130, as stored on 2026-10-13 (api/v2/measurements/<msm>/latest),
    /// less what the backend adds: each comes out of the run that makes it.
    mod as_ripe_stored_them {
        use super::*;

        fn check(p: &Ping, outcome: PingOutcome, stored: &str) {
            assert_stored_as(&p.outcome_result(&outcome), stored);
        }

        fn k_root_v4() -> Ping {
            ping("193.0.14.129", 3, 0, literal("193.0.14.129"))
        }

        fn k_root_v6() -> Ping {
            ping("2001:7fd::1", 3, 0, literal("2001:7fd::1"))
        }

        /// A timeout, then the first reply carries its ttl: the TTL kept
        /// after a timeout is -1 (ping.c:355-360).
        #[test]
        fn timeout_then_replies() {
            let local = ip("2a06:a002:a101:0:48f0:60ff:fe66:c591");
            check(
                &k_root_v6(),
                run(
                    true,
                    local,
                    &[None, Some((20.041044, 53)), Some((22.720919, 53))],
                ),
                r#"{"fw":5130,"mver":"2.6.4","dst_name":"2001:7fd::1","af":6,"dst_addr":"2001:7fd::1","src_addr":"2a06:a002:a101:0:48f0:60ff:fe66:c591","proto":"ICMP","ttl":53,"size":32,"result":[{"x":"*"},{"rtt":20.041044,"ttl":53},{"rtt":22.720919}]}"#,
            );
            check(
                &k_root_v4(),
                run(
                    false,
                    ip("192.168.0.2"),
                    &[None, None, Some((35.151733, 56))],
                ),
                r#"{"fw":5130,"mver":"2.6.4","dst_name":"193.0.14.129","af":4,"dst_addr":"193.0.14.129","src_addr":"192.168.0.2","proto":"ICMP","ttl":56,"size":32,"result":[{"x":"*"},{"x":"*"},{"rtt":35.151733,"ttl":56}]}"#,
            );
        }

        /// After the first reply a lost request leaves no entry
        /// (`!got_reply`, ping.c:804): -c 3 with the second or third reply
        /// lost is stored with two entries; and one with the first reply
        /// lost has its timeout. Starla wrote `{ "x":"*" }` for each.
        #[test]
        fn no_timeout_after_a_reply() {
            let local = ip("2a00:6020:a312:ad00:983c:6fff:fe3a:a5c0");
            check(
                &k_root_v6(),
                run(
                    true,
                    local,
                    &[Some((8.257689, 58)), None, Some((7.445205, 58))],
                ),
                r#"{"fw":5130,"mver":"2.6.4","dst_name":"2001:7fd::1","af":6,"dst_addr":"2001:7fd::1","src_addr":"2a00:6020:a312:ad00:983c:6fff:fe3a:a5c0","proto":"ICMP","ttl":58,"size":32,"result":[{"rtt":8.257689},{"rtt":7.445205}]}"#,
            );
            check(
                &k_root_v4(),
                run(
                    false,
                    ip("10.63.63.194"),
                    &[None, Some((385.856553, 47)), None],
                ),
                r#"{"fw":5130,"mver":"2.6.4","dst_name":"193.0.14.129","af":4,"dst_addr":"193.0.14.129","src_addr":"10.63.63.194","proto":"ICMP","ttl":47,"size":32,"result":[{"x":"*"},{"rtt":385.856553,"ttl":47}]}"#,
            );
            let local = ip("2001:4453:73b:a000:28c:faff:fed3:b001");
            check(
                &k_root_v6(),
                run(true, local, &[Some((252.828905, 58)), None, None]),
                r#"{"fw":5130,"mver":"2.6.4","dst_name":"2001:7fd::1","af":6,"dst_addr":"2001:7fd::1","src_addr":"2001:4453:73b:a000:28c:faff:fed3:b001","proto":"ICMP","ttl":58,"size":32,"result":[{"rtt":252.828905}]}"#,
            );
        }

        /// Every request answered twice: the second reply is a duplicate,
        /// `{ "dup":1, "rtt":… }`, on IPv4 (which keeps its sequence number
        /// on a duplicate) and IPv6 (which moves on: ping.c:1186).
        #[test]
        fn duplicates() {
            for (v6, p, local, rtts, ttl, stored) in [
                (
                    false,
                    k_root_v4(),
                    "192.168.103.20",
                    [
                        127.959025, 127.990424, 127.170506, 127.202582, 127.212352, 127.253896,
                    ],
                    51,
                    r#"{"fw":5130,"mver":"2.6.4","dst_name":"193.0.14.129","af":4,"dst_addr":"193.0.14.129","src_addr":"192.168.103.20","proto":"ICMP","ttl":51,"size":32,"result":[{"rtt":127.959025},{"dup":1,"rtt":127.990424},{"rtt":127.170506},{"dup":1,"rtt":127.202582},{"rtt":127.212352},{"dup":1,"rtt":127.253896}]}"#,
                ),
                (
                    true,
                    k_root_v6(),
                    "2600:1701:480:546c:9471:4bff:fea5:91f1",
                    [
                        114.999036, 115.049831, 115.409236, 115.455889, 114.971387, 115.009597,
                    ],
                    48,
                    r#"{"fw":5130,"mver":"2.6.4","dst_name":"2001:7fd::1","af":6,"dst_addr":"2001:7fd::1","src_addr":"2600:1701:480:546c:9471:4bff:fea5:91f1","proto":"ICMP","ttl":48,"size":32,"result":[{"rtt":114.999036},{"dup":1,"rtt":115.049831},{"rtt":115.409236},{"dup":1,"rtt":115.455889},{"rtt":114.971387},{"dup":1,"rtt":115.009597}]}"#,
                ),
            ] {
                let local = ip(local);
                let mut session = Session::new(v6);
                for pair in rtts.chunks(2) {
                    let seq = u16::from(session.seq());
                    session.sent(local, None);
                    session.reply(seq, pair[0], Some(ttl), 32, local);
                    session.reply(seq, pair[1], Some(ttl), 32, local);
                    session.interval_over();
                }
                let outcome = PingOutcome::Ran {
                    local,
                    events: session.into_events(),
                };
                check(&p, outcome, stored);
            }
        }

        /// A reply later than its interval comes while the next request
        /// is awaited: a duplicate, with the ttl the timeout's -1 makes it
        /// carry; the next request's own reply is not.
        #[test]
        fn a_late_reply_is_a_duplicate() {
            let local = ip("143.246.178.201");
            let mut session = Session::new(false);
            session.sent(local, None);
            session.interval_over();
            session.sent(local, None);
            session.reply(1, 1054.094967, Some(55), 32, local);
            session.reply(2, 52.644335, Some(55), 32, local);
            session.interval_over();
            session.sent(local, None);
            session.reply(3, 39.14021, Some(55), 32, local);
            session.interval_over();
            let outcome = PingOutcome::Ran {
                local,
                events: session.into_events(),
            };
            let stored = r#"{"fw":5130,"mver":"2.6.4","dst_name":"193.0.14.129","af":4,"dst_addr":"193.0.14.129","src_addr":"143.246.178.201","proto":"ICMP","ttl":55,"size":32,"result":[{"x":"*"},{"dup":1,"rtt":1054.094967,"ttl":55},{"rtt":52.644335},{"rtt":39.14021}]}"#;
            check(&k_root_v4(), outcome.clone(), stored);
            assert!(body(&k_root_v4().outcome_result(&outcome)).ends_with(r#""result": [ { "x":"*" }, { "dup":1, "rtt":1054.094967, "ttl":55 }, { "rtt":52.644335 }, { "rtt":39.140210 } ]"#));

            // the same run as a backend that runs the whole ping records it
            let probe = |sent_ms: u64, rtt_us: u64| Probe {
                sent: Duration::from_millis(sent_ms),
                local,
                replies: vec![ProbeReply {
                    rtt: Duration::from_micros(rtt_us),
                    ttl: Some(55),
                    size: 32,
                }],
            };
            let probes = [
                probe(0, 1_054_095),
                probe(1002, 52_644),
                probe(2003, 39_140),
            ];
            let events = replay(false, Duration::from_millis(1000), &probes);
            let PingOutcome::Ran { events: live, .. } = outcome else {
                unreachable!()
            };
            assert_eq!(events.len(), live.len());
            for (replayed, live) in events.iter().zip(&live) {
                match (replayed, live) {
                    (
                        PingEvent::Reply { dup, ttl, size, .. },
                        PingEvent::Reply {
                            dup: d,
                            ttl: t,
                            size: s,
                            ..
                        },
                    ) => assert_eq!((dup, ttl, size), (d, t, s)),
                    (PingEvent::Sent { local: l, .. }, PingEvent::Sent { local: m, .. }) => {
                        assert_eq!(l, m)
                    }
                    (a, b) => assert_eq!(a, b),
                }
            }
        }

        /// sendto fails for every request: an error entry each, no
        /// timeouts (send_error), "size" the data size and no ttl.
        #[test]
        fn sendto_failed() {
            let local = ip("2001:41d0:700:243c::");
            let mut session = Session::new(true);
            for _ in 0..3 {
                session.sent(local, Some("Operation not permitted".into()));
                session.interval_over();
            }
            let outcome = PingOutcome::Ran {
                local,
                events: session.into_events(),
            };
            check(
                &k_root_v6(),
                outcome,
                r#"{"fw":5130,"mver":"2.6.4","dst_name":"2001:7fd::1","af":6,"dst_addr":"2001:7fd::1","src_addr":"2001:41d0:700:243c::","proto":"ICMP","size":32,"result":[{"error":"sendto failed: Operation not permitted"},{"error":"sendto failed: Operation not permitted"},{"error":"sendto failed: Operation not permitted"}]}"#,
            );
        }

        /// No socket, or no route: one error entry, `"src_addr":""` and
        /// `"size":0`, nothing having been kept.
        #[test]
        fn socket_and_connect_failures() {
            check(
                &k_root_v4(),
                PingOutcome::Failed("connect failed: Network is unreachable".into()),
                r#"{"fw":5130,"mver":"2.6.4","dst_name":"193.0.14.129","af":4,"dst_addr":"193.0.14.129","src_addr":"","proto":"ICMP","size":0,"result":[{"error":"connect failed: Network is unreachable"}]}"#,
            );
            check(
                &k_root_v6(),
                PingOutcome::Failed("socket failed: Operation not permitted".into()),
                r#"{"fw":5130,"mver":"2.6.4","dst_name":"2001:7fd::1","af":6,"dst_addr":"2001:7fd::1","src_addr":"","proto":"ICMP","size":0,"result":[{"error":"socket failed: Operation not permitted"}]}"#,
            );
        }

        /// A reply that came to another address than the socket's own
        /// carries it: the first reply sets the address kept, and every
        /// request refreshes it from getsockname, here 0.0.0.0 (ping.c:381-
        /// 387, 417-428, 745-747).
        #[test]
        fn a_reply_to_another_address_carries_it() {
            let unbound = ip("0.0.0.0");
            let to = ip("31.58.102.40");
            let mut session = Session::new(false);
            for rtt in [12.404398, 12.30382, 12.34703] {
                let seq = u16::from(session.seq());
                session.sent(unbound, None);
                session.reply(seq, rtt, Some(54), 32, to);
                session.interval_over();
            }
            let outcome = PingOutcome::Ran {
                local: unbound,
                events: session.into_events(),
            };
            check(
                &k_root_v4(),
                outcome,
                r#"{"fw":5130,"mver":"2.6.4","dst_name":"193.0.14.129","af":4,"dst_addr":"193.0.14.129","src_addr":"0.0.0.0","proto":"ICMP","ttl":54,"size":32,"result":[{"rtt":12.404398},{"rtt":12.30382,"src_addr":"31.58.102.40"},{"rtt":12.34703,"src_addr":"31.58.102.40"}]}"#,
            );
        }
    }

    /// ping_cb writes a reply's "ttl" where it changes from the entry
    /// before, which then is the one kept, and the line has the last kept
    /// (ping.c:405-411, 295-296); RIPE's documentation says "if different
    /// from ttl in first reply". A reply's size is written likewise, before
    /// its ttl.
    #[test]
    fn ttl_and_size_are_written_where_they_change() {
        let reply = |rtt: f64, ttl: u8, size: usize| PingEvent::Reply {
            dup: false,
            rtt,
            ttl: Some(ttl),
            size,
            dst: None,
        };
        let entries = ping_cb(
            &[
                reply(1.0, 57, 32),
                reply(2.0, 56, 32),
                reply(3.0, 56, 32),
                reply(4.0, 57, 24),
            ],
            32,
            None,
            0,
        );
        assert_eq!(
            entries,
            Entries {
                result: r#"[ { "rtt":1.000000 }, { "rtt":2.000000, "ttl":56 }, { "rtt":3.000000 }, { "rtt":4.000000, "size":24, "ttl":57 } ]"#.into(),
                ttl: Some(57),
                size: 24,
                src_addr: None,
                rcvd_ttl: 57,
            }
        );
    }

    /// A reply whose TTL the backend cannot tell (Windows over IPv6,
    /// scamper without one) gets rcvd_ttl as ping.c hands it on
    /// (ping.c:1155-1162, 1183): the last reply's that had one, in this run
    /// or one before, 0 before any — never the 64 Starla sent with, which
    /// is what RIPE stored for every Starla ping (starla-2001.json: 44 of
    /// 44 lines "ttl":64, where ref-2001.json has 59).
    #[test]
    fn an_unknown_ttl_is_the_last_one_received() {
        let reply = |ttl: Option<u8>| PingEvent::Reply {
            dup: false,
            rtt: 1.0,
            ttl,
            size: 32,
            dst: None,
        };
        let entries = ping_cb(&[reply(None)], 32, None, 0);
        assert_eq!((entries.ttl, entries.rcvd_ttl), (Some(0), 0));
        // after one with a TTL: that one's, so no "ttl" of its own
        let entries = ping_cb(&[reply(Some(57)), reply(None)], 32, None, 0);
        assert_eq!(
            entries.result,
            r#"[ { "rtt":1.000000 }, { "rtt":1.000000 } ]"#
        );
        assert_eq!((entries.ttl, entries.rcvd_ttl), (Some(57), 57));
        // an earlier run's, kept
        let entries = ping_cb(&[reply(None), reply(Some(58))], 32, None, 59);
        assert_eq!(
            entries.result,
            r#"[ { "rtt":1.000000 }, { "rtt":1.000000, "ttl":58 } ]"#
        );
        assert_eq!(entries.rcvd_ttl, 58);
        // and through the measurement: the next run starts from it
        let p = ping("2001:7fd::1", 1, 0, literal("2001:7fd::1"));
        let unknown = |p: &Ping| {
            p.outcome_result(&PingOutcome::Ran {
                local: ip("2001:db8::31"),
                events: vec![
                    PingEvent::Sent {
                        local: ip("2001:db8::31"),
                        error: None,
                    },
                    reply(None),
                ],
            })
        };
        assert_eq!(unknown(&p).ttl, Some(0));
        p.outcome_result(&run(true, ip("2001:db8::31"), &[Some((1.0, 59))]));
        assert_eq!(unknown(&p).ttl, Some(59));
    }

    /// The TTL kept after a timeout, and with only duplicates (ping.c:355-
    /// 360, 381-387).
    #[test]
    fn a_timeout_keeps_minus_one_and_duplicates_no_ttl() {
        let reply = |rtt: f64, ttl: u8, size: usize| PingEvent::Reply {
            dup: false,
            rtt,
            ttl: Some(ttl),
            size,
            dst: None,
        };
        // a timeout keeps -1, an unsigned char: a reply with 255 has no ttl
        assert_eq!(
            ping_cb(&[PingEvent::Timeout, reply(1.0, 255, 32)], 32, None, 0).result,
            r#"[ { "x":"*" }, { "rtt":1.000000 } ]"#
        );
        // only duplicates: no reply, no ttl
        let dup = PingEvent::Reply {
            dup: true,
            rtt: 1.0,
            ttl: Some(57),
            size: 32,
            dst: None,
        };
        assert_eq!(ping_cb(&[dup], 32, None, 0).ttl, None);
    }

    /// A run whose socket or connect fails reports what the measurement
    /// kept: the last run's "size" and loc_sin6 as "src_addr", 0 and ""
    /// before any run (ping_start2 resets neither, ping.c:1470-1481, and
    /// report() writes both, ping.c:283-298). A failed lookup or a refused
    /// address keeps the `-s` as given, an empty answer cursize.
    #[test]
    fn a_failed_run_reports_what_the_last_run_kept() {
        let connect_failed =
            || PingOutcome::Failed("connect failed: Network is unreachable".into());
        let tail = |r: &MeasurementResult| {
            let body = body(r);
            body[body.find(r#""dst_addr""#).unwrap()..].to_string()
        };
        let p = ping("2001:7fd::1", 3, 48, literal("2001:7fd::1"));
        assert_eq!(
            tail(&p.outcome_result(&connect_failed())),
            r#""dst_addr":"2001:7fd::1", "src_addr":"", "proto":"ICMP", "size":0, "result": [ { "error":"connect failed: Network is unreachable" } ]"#
        );
        let local = ip("2001:418:1::33");
        let mut session = Session::new(true);
        let seq = u16::from(session.seq());
        session.sent(local, None);
        session.reply(seq, 3.6, Some(59), 48, local);
        session.interval_over();
        let ran = p.outcome_result(&PingOutcome::Ran {
            local,
            events: session.into_events(),
        });
        assert_eq!(ran.ttl, Some(59));
        let failed = p.outcome_result(&connect_failed());
        assert_eq!(
            tail(&failed),
            r#""dst_addr":"2001:7fd::1", "src_addr":"2001:418:1::33", "proto":"ICMP", "size":48, "result": [ { "error":"connect failed: Network is unreachable" } ]"#
        );
        assert_eq!((failed.ttl, failed.size), (None, Some(48)));
        // and again: nothing changed it
        assert!(tail(&p.outcome_result(&connect_failed()))
            .contains(r#""src_addr":"2001:418:1::33", "proto":"ICMP", "size":48,"#));

        // -s 0: a run sends 32 bytes (cursize), a lookup failure keeps the
        // 0 as given, an empty answer cursize
        let p = ping("2001:7fd::1", 1, 0, literal("2001:7fd::1"));
        p.outcome_result(&run(true, local, &[Some((1.0, 59))]));
        let size = |p: &Ping| p.outcome_result(&connect_failed()).size;
        assert_eq!(size(&p), Some(32));
        p.lookup_failed(LookupError::NoName).unwrap();
        assert_eq!(size(&p), Some(0));
        assert!(p.lookup_failed(LookupError::NoAddress).is_none());
        // cursize was the -s as given since the failed connect
        assert_eq!(size(&p), Some(0));
        p.outcome_result(&run(true, local, &[Some((1.0, 59))]));
        assert!(p.lookup_failed(LookupError::NoAddress).is_none());
        assert_eq!(size(&p), Some(32));
        p.address_not_allowed().unwrap();
        assert_eq!(size(&p), Some(0));
    }

    /// One kept state per measurement line: a periodic line delivered
    /// again while scheduled is the line eperd already runs (Insert(),
    /// eperd.c:781-827) and keeps it; a changed line, a one-off, or the
    /// same line once its measurement went, starts with nothing kept.
    #[test]
    fn a_redelivered_line_keeps_its_state() {
        let line = PingLine {
            msm_id: 2001,
            target: "2001:7fd::1".into(),
            af: 6,
            packets: 3,
            packet_interval: 1000,
            size: 0,
            interval: 240,
            start_time: 0,
        };
        let kept = PingKept::for_line(line.clone());
        assert!(Arc::ptr_eq(&kept, &PingKept::for_line(line.clone())));
        let changed = PingLine {
            size: 48,
            ..line.clone()
        };
        assert!(!Arc::ptr_eq(&kept, &PingKept::for_line(changed)));
        let one_off = PingLine {
            interval: 0,
            ..line.clone()
        };
        assert!(!Arc::ptr_eq(
            &PingKept::for_line(one_off.clone()),
            &PingKept::for_line(one_off)
        ));
        kept.set(Kept {
            size: 32,
            ..Kept::default()
        });
        assert_eq!(PingKept::for_line(line.clone()).get().size, 32);
        drop(kept);
        assert_eq!(PingKept::for_line(line).get(), Kept::default());
    }

    /// IPv6 moves to the next sequence number on any reply, so after a
    /// late reply the next request's own reply is a duplicate too; IPv4
    /// does not (ping.c:974-979, 1186-1190).
    #[test]
    fn sequence_numbers_as_ping_c_keeps_them() {
        for (v6, dups) in [(false, vec![true, false]), (true, vec![true, true])] {
            let mut session = Session::new(v6);
            session.sent(None, None);
            session.interval_over(); // timeout: 2
            assert_eq!(session.seq(), 2);
            session.sent(None, None);
            session.reply(1, 1200.0, Some(60), 32, None); // late
            session.reply(2, 200.0, Some(60), 32, None);
            let got: Vec<bool> = session
                .into_events()
                .iter()
                .filter_map(|e| match e {
                    PingEvent::Reply { dup, .. } => Some(*dup),
                    _ => None,
                })
                .collect();
            assert_eq!(got, dups, "v6 {v6}");
        }
        // 256 requests later the number wraps to 0
        let mut session = Session::new(true);
        for _ in 0..255 {
            session.interval_over();
        }
        assert_eq!(session.seq(), 0);
    }

    /// Replies after the last interval are not seen: the run is reported.
    #[test]
    fn replay_stops_with_the_last_interval() {
        let probe = |sent_ms: u64, rtt_ms: u64| Probe {
            sent: Duration::from_millis(sent_ms),
            local: None,
            replies: vec![ProbeReply {
                rtt: Duration::from_millis(rtt_ms),
                ttl: Some(60),
                size: 32,
            }],
        };
        let events = replay(
            true,
            Duration::from_millis(1000),
            &[probe(0, 10), probe(1000, 1500)],
        );
        let entries = ping_cb(&events, 32, None, 0);
        assert_eq!(entries.result, r#"[ { "rtt":10.000000 } ]"#);
    }

    /// scamper sent the second request 1 µs before a whole interval after
    /// the first, which got no reply: its interval ended then, with the
    /// timeout and the next sequence number, so the second reply is the
    /// second request's, not a duplicate (ping.c:800-812).
    #[test]
    fn replay_ends_an_interval_when_the_next_request_goes_out() {
        let interval = Duration::from_millis(1000);
        for v6 in [false, true] {
            let probes = [
                Probe {
                    sent: Duration::ZERO,
                    local: None,
                    replies: Vec::new(),
                },
                Probe {
                    sent: interval - Duration::from_micros(1),
                    local: None,
                    replies: vec![ProbeReply {
                        rtt: Duration::from_millis(10),
                        ttl: Some(60),
                        size: 32,
                    }],
                },
            ];
            let events = replay(v6, interval, &probes);
            let entries = ping_cb(&events, 32, None, 0);
            assert_eq!(
                entries.result, r#"[ { "x":"*" }, { "rtt":10.000000, "ttl":60 } ]"#,
                "v6 {v6}"
            );
            assert_eq!(entries.ttl, Some(60));
        }
    }

    /// ping_start returns at once while the line's last run is under way
    /// (ping.c:1751-1752); report() frees it (ping.c:324). After an empty
    /// answer report() never runs, so the line stays busy for good.
    #[test]
    fn a_busy_line_does_not_start() {
        let p = ping("2001:7fd::1", 1, 0, literal("2001:7fd::1"));
        let kept = &p.config.kept;
        let run = kept.start().unwrap();
        assert!(kept.start().is_none());
        drop(run);
        let run = kept.start().unwrap();
        assert!(p.lookup_failed(LookupError::NoName).is_some());
        drop(run);
        assert!(kept.start().is_some());
        let run = kept.start().unwrap();
        assert!(p.lookup_failed(LookupError::NoAddress).is_none());
        drop(run);
        assert!(kept.start().is_none());
    }

    #[test]
    fn strerror_is_the_bare_text() {
        let e = std::io::Error::from_raw_os_error(libc_enetunreach());
        let text = strerror(&e);
        assert!(!text.contains("os error"), "{text}");
        assert!(text.to_lowercase().contains("unreachable"), "{text}");
    }

    fn libc_enetunreach() -> i32 {
        std::io::Error::from(std::io::ErrorKind::NetworkUnreachable)
            .raw_os_error()
            .unwrap_or(if cfg!(target_os = "linux") { 101 } else { 51 })
    }
}
