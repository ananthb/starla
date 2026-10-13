//! Traceroute measurement implementation: evtraceroute as firmware 5130
//! runs it (probe-busybox/eperd/traceroute.c).
//!
//! - `wire`: the probes and the reading of replies, byte for byte;
//! - `engine`: hops, timeouts, the end of the trace and the result text;
//! - `net` (unix): the sockets;
//! - `icmp` (Windows): ICMP echo through the ICMP helper API;
//! - `scamper`: the optional scamper-daemon backend (Linux).

pub mod engine;
#[cfg(windows)]
pub mod icmp;
#[cfg(unix)]
pub mod net;
#[cfg(test)]
mod replay;
pub mod scamper;
pub mod wire;

use crate::target::{DstName, LookupError};
use crate::traits::Measurement;
use async_trait::async_trait;
use engine::Hop;
use serde::{Deserialize, Serialize};
use starla_common::{
    MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId, Timestamp,
};
pub use starla_common::{TracerouteOptions, TracerouteProtocol};
use starla_network::get_source_addr_for_dest;
use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};

/// Which implementation runs the traceroute. See [`super::ping::PingBackend`]
/// for the same trade-off applied to ping.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TracerouteBackend {
    #[default]
    Native,
    Scamper,
}

/// The slots in use: eperd's trt_base->table (traceroute.c:74-90).
static SLOTS: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());

/// What eperd's Insert() compares to tell that a crontab line it is given
/// is one it already runs (eperd.c:781-826): the interval, the start time
/// and the command (cl_Shell) — here the command's parts: the
/// measurement id (-A), the target, the address family and the options.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LineKey {
    msm_id: u64,
    target: String,
    af: u8,
    opts: TracerouteOptions,
    interval: u64,
    start_time: i64,
}

/// The instances of the periodic lines that are scheduled; an entry goes
/// dead when its measurement does.
static LINES: LazyLock<Mutex<HashMap<LineKey, Weak<TracerouteInstance>>>> =
    LazyLock::new(Default::default);

/// What eperd keeps of one evtraceroute measurement between its runs: the
/// slot traceroute_init takes in its table (the lowest free one,
/// traceroute.c:4419-4436), which names its packets (UDP and TCP source
/// port 20480 + index, the id in every IPv6 probe's v6info, the IPv4 echo
/// id), and how many runs it has started, which steps the Paris id
/// (traceroute.c:4492-4497). Shared by every run of a measurement; the slot
/// is free again when the measurement goes (traceroute_delete).
#[derive(Debug)]
pub struct TracerouteInstance {
    index: u16,
    runs: AtomicU64,
    /// A run is in progress (trtstate.busy).
    busy: AtomicBool,
}

/// A run in progress: its instance is busy until this goes.
#[derive(Debug)]
pub struct Run<'a> {
    instance: &'a TracerouteInstance,
}

impl Run<'_> {
    /// traceroute_start2, once per run: the run gets under way and its
    /// number, 1 for the measurement's first, which picks its Paris id
    /// (traceroute.c:4492-4497). A run that ends before it — its target's
    /// lookup failed (dns_cb) — leaves the count, and the Paris id the
    /// result reports, as the last run left them
    /// ([`TracerouteInstance::runs`]).
    pub fn underway(&self) -> u64 {
        self.instance.runs.fetch_add(1, Ordering::Relaxed) + 1
    }
}

impl Drop for Run<'_> {
    /// report() clears busy (traceroute.c:454).
    fn drop(&mut self) {
        self.instance.busy.store(false, Ordering::Release);
    }
}

impl TracerouteInstance {
    /// The IPv4 echo id has 12 bits for the index (TRT_ICMP4_INSTANCE_ID_SHIFT).
    const SLOTS: u16 = 1 << 12;

    pub fn index(&self) -> u16 {
        self.index
    }

    /// Start a run, unless the last one is still going: traceroute_start
    /// refuses with "busy, can't start" (traceroute.c:4934-4938), and as
    /// traceroute_start2 never runs the Paris id does not step either.
    /// Two runs at once would share the slot — the source port, the echo
    /// id, the v6info id — and each would take the other's replies as its
    /// own, since both count their probes from 1. Starla's scheduler can
    /// start one: a slot plus a spread offset that comes before the last
    /// run's end.
    pub fn start(&self) -> Option<Run<'_>> {
        if self.busy.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(Run { instance: self })
    }

    /// How many runs got under way: the last one's Paris id is
    /// `opts.paris_id(runs())`, `-b`'s before the first (state->paris
    /// starts at parisbase, traceroute.c:4408).
    pub fn runs(&self) -> u64 {
        self.runs.load(Ordering::Relaxed)
    }

    /// The instance for a measurement line the controller delivers. A
    /// periodic line (`interval` > 0) identical to one still scheduled is,
    /// to eperd, the line it already runs: Insert() keeps the old CronLine
    /// and its trtstate (eperd.c:815-827), so the slot (source port, v6info
    /// id) stays and the Paris id keeps stepping. Anything else — a new or
    /// changed line, or a one-off, which eooqd inits afresh (eooqd.c:546)
    /// — is a new measurement with the lowest free slot and no runs yet.
    pub fn for_line(
        msm_id: u64,
        target: &str,
        af: u8,
        opts: &TracerouteOptions,
        interval: u64,
        start_time: i64,
    ) -> Arc<Self> {
        if interval == 0 {
            return Arc::default();
        }
        let key = LineKey {
            msm_id,
            target: target.to_string(),
            af,
            opts: opts.clone(),
            interval,
            start_time,
        };
        let mut lines = LINES.lock().unwrap_or_else(|e| e.into_inner());
        lines.retain(|_, w| w.strong_count() > 0);
        if let Some(live) = lines.get(&key).and_then(Weak::upgrade) {
            return live;
        }
        let new = Arc::new(Self::default());
        lines.insert(key, Arc::downgrade(&new));
        new
    }
}

/// The lowest slot not in `taken` (the last one when all are).
fn lowest_free(taken: &BTreeSet<u16>) -> u16 {
    (0..TracerouteInstance::SLOTS)
        .find(|i| !taken.contains(i))
        .unwrap_or(TracerouteInstance::SLOTS - 1)
}

impl Default for TracerouteInstance {
    /// A new measurement: takes the lowest free slot.
    fn default() -> Self {
        let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        let index = lowest_free(&slots);
        slots.insert(index);
        Self {
            index,
            runs: AtomicU64::new(0),
            busy: AtomicBool::new(false),
        }
    }
}

impl Drop for TracerouteInstance {
    fn drop(&mut self) {
        let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        slots.remove(&self.index);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracerouteConfig {
    pub target: IpAddr,
    pub opts: TracerouteOptions,
    #[serde(default)]
    pub backend: TracerouteBackend,
    /// The measurement this run belongs to.
    #[serde(skip)]
    pub instance: Arc<TracerouteInstance>,
}

pub struct Traceroute {
    pub config: TracerouteConfig,
    pub probe_id: ProbeId,
    pub msm_id: MeasurementId,
    /// The target as the result names it: the controller's name for it and
    /// its ttr (traceroute.c:381-397).
    pub dst_name: DstName,
}

impl Traceroute {
    /// The "paris_id" a line that runs no trace writes — dns_cb's, for a
    /// lookup that failed or an address refused: state->paris as the last
    /// run left it, parisbase before any run (traceroute.c:4408, 4492-4497,
    /// 4822-4842), and only with Paris on (parismod != 0,
    /// traceroute.c:428-431).
    fn paris_id(&self) -> Option<u16> {
        self.config.opts.paris_id(self.config.instance.runs())
    }

    fn proto(&self) -> &'static str {
        match self.config.opts.protocol {
            TracerouteProtocol::ICMP => "ICMP",
            TracerouteProtocol::UDP => "UDP",
            TracerouteProtocol::TCP => "TCP",
        }
    }

    /// The hops of one run.
    async fn trace(&self, paris: Option<u16>) -> anyhow::Result<Vec<Hop>> {
        match self.config.backend {
            #[cfg(target_os = "linux")]
            TracerouteBackend::Scamper => {
                scamper::execute_traceroute(&self.config, paris, self.config.instance.index()).await
            }
            #[cfg(not(target_os = "linux"))]
            TracerouteBackend::Scamper => {
                anyhow::bail!("scamper backend is only available on Linux")
            }
            #[cfg(unix)]
            TracerouteBackend::Native => Ok(net::run(&net::RunSpec {
                target: self.config.target,
                opts: &self.config.opts,
                paris,
                index: self.config.instance.index(),
            })
            .await),
            #[cfg(windows)]
            TracerouteBackend::Native => icmp::execute_traceroute(&self.config, paris).await,
        }
    }

    /// report()'s line (traceroute.c:344-433) without its "RESULT { " and
    /// " }", with the "lts":0 placeholder the results queue fills in.
    /// `dst_fields` is the `"dst_name":…` part ([`DstName::fields`]): the
    /// name, and its ttr when it had to be looked up, right after
    /// "endtime" (and -B's "bundle", traceroute.c:381-397).
    fn result_body(
        &self,
        time: i64,
        endtime: i64,
        dst_fields: &str,
        src_addr: Option<IpAddr>,
        paris: Option<u16>,
        hops: &[Hop],
    ) -> String {
        let opts = &self.config.opts;
        let bundle = opts
            .bundle
            .as_ref()
            .map(|b| format!("\"bundle\":{}, ", b))
            .unwrap_or_default();
        // Only with Paris (a nonzero -a) is there a "paris_id".
        let paris_id = paris
            .map(|p| format!(", \"paris_id\":{}", p))
            .unwrap_or_default();
        format!(
            "\"id\":\"{}\", \"fw\":{}, \"mver\": \"2.6.4\", \"lts\":0, \"time\":{}, \
             \"endtime\":{}, {}{}, \"dst_addr\":\"{}\", \"src_addr\":\"{}\", \
             \"proto\":\"{}\", \"af\": {}, \"size\":{}{}, \"result\": {}",
            self.msm_id.0,
            starla_common::FIRMWARE_VERSION,
            time,
            endtime,
            bundle,
            dst_fields,
            self.config.target,
            src_addr.map(|ip| ip.to_string()).unwrap_or_default(),
            self.proto(),
            if self.config.target.is_ipv4() { 4 } else { 6 },
            opts.size,
            paris_id,
            engine::render_hops(hops),
        )
    }

    /// One run of the measurement, its line busy with it: `run` is the
    /// slot traceroute_start took before the target's lookup
    /// (traceroute.c:4934-4938), which the caller holds until the line is
    /// reported. The run gets under way here (traceroute_start2): its
    /// number picks the Paris id (traceroute.c:4492-4497).
    pub async fn execute_run(&self, run: &Run<'_>) -> anyhow::Result<MeasurementResult> {
        debug_assert!(
            std::ptr::eq(run.instance, &*self.config.instance),
            "a run of another line"
        );
        let start_time = Timestamp::now().0;
        let paris = self.config.opts.paris_id(run.underway());
        let hops = self.trace(paris).await?;
        // report() writes the time it reports (traceroute.c:376).
        let endtime = Timestamp::now().0;

        let src_addr = get_source_addr_for_dest(self.config.target).ok();
        let af = if self.config.target.is_ipv4() { 4 } else { 6 };
        let body = self.result_body(
            start_time,
            endtime,
            &self.dst_name.fields(),
            src_addr,
            paris,
            &hops,
        );

        Ok(MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Traceroute,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: Timestamp(start_time),
            af,
            dst_addr: self.config.target,
            dst_name: Some(self.dst_name.name.clone()),
            src_addr,
            proto: Some(self.proto().to_string()),
            ttl: None,
            size: Some(self.config.opts.size),
            data: MeasurementData::FullLine(body),
        })
    }

    /// The result traceroute.c reports when the target's lookup fails:
    /// dns_cb takes the ttr, sets the start time to now and adds `{
    /// "error":"name resolution failed: <evutil_gai_strerror>" }`
    /// (traceroute.c:4810-4838), and report() leaves dst_addr and src_addr
    /// out while dnsip is still set, with "af" the family -4/-6 asked for
    /// (traceroute.c:399-421). `self.config.target` only carries that
    /// family. Ping words the same failure "dns resolution failed".
    ///
    /// An answer with no address of the family (a hosts file entry of the
    /// other one) is "name resolution failed: out of addresses"
    /// (traceroute.c:4914-4923); the reference closes that entry with an
    /// extra " } ] }", which makes its line no JSON at all, and Starla
    /// leaves that out. An address atlas_check_addr refuses is "address not
    /// allowed" with dst_addr (dnsip is 0 by then) and no src_addr
    /// (traceroute.c:4840-4842, 4886-4899).
    ///
    /// `paris` is the paris_id the line carries: the reference reports
    /// state->paris, which only traceroute_start2 steps
    /// (traceroute.c:4492-4497), so a run that fails its lookup reports
    /// the id of the last run that got that far, parisbase before any.
    fn lookup_failed_line(
        &self,
        paris: Option<u16>,
        dst_addr: Option<IpAddr>,
        error: &str,
    ) -> MeasurementResult {
        let now = Timestamp::now();
        let af = if self.config.target.is_ipv4() { 4 } else { 6 };
        let bundle = self
            .config
            .opts
            .bundle
            .as_ref()
            .map(|b| format!("\"bundle\":{}, ", b))
            .unwrap_or_default();
        let paris_id = paris
            .map(|p| format!(", \"paris_id\":{}", p))
            .unwrap_or_default();
        let dst_addr = dst_addr
            .map(|addr| format!(", \"dst_addr\":\"{addr}\""))
            .unwrap_or_default();
        let body = format!(
            "\"id\":\"{}\", \"fw\":{}, \"mver\": \"2.6.4\", \"lts\":0, \"time\":{}, \
             \"endtime\":{}, {}{}{}, \"proto\":\"{}\", \"af\": {}, \"size\":{}{}, \
             \"result\": [ {{ \"error\":\"{}\" }} ]",
            self.msm_id.0,
            starla_common::FIRMWARE_VERSION,
            now.0,
            now.0,
            bundle,
            self.dst_name.fields(),
            dst_addr,
            self.proto(),
            af,
            self.config.opts.size,
            paris_id,
            error,
        );
        MeasurementResult {
            fw: starla_common::FIRMWARE_VERSION,
            measurement_type: MeasurementType::Traceroute,
            prb_id: self.probe_id,
            msm_id: self.msm_id,
            timestamp: now,
            af,
            dst_addr: self.config.target,
            dst_name: Some(self.dst_name.name.clone()),
            src_addr: None,
            proto: Some(self.proto().to_string()),
            ttl: None,
            size: Some(self.config.opts.size),
            data: MeasurementData::FullLine(body),
        }
    }
}

#[async_trait]
impl Measurement for Traceroute {
    fn measurement_type(&self) -> starla_common::MeasurementType {
        starla_common::MeasurementType::Traceroute
    }

    /// A run that nothing has started: it takes its line's slot itself,
    /// unless the last run is still going — traceroute_start refuses with
    /// "busy, can't start" (traceroute.c:4934-4938). The scheduler's runs
    /// come through the target lookup, which takes the slot before it looks
    /// the name up and calls [`Traceroute::execute_run`].
    async fn execute(&self) -> anyhow::Result<MeasurementResult> {
        let Some(run) = self.config.instance.start() else {
            anyhow::bail!("traceroute {}: busy, can't start", self.msm_id.0);
        };
        self.execute_run(&run).await
    }

    /// dns_cb's line (lookup_failed_line), with the paris_id the last run
    /// left ([`Traceroute::paris_id`]): `"paris_id":0` before any run for
    /// RIPE's 6152 and 6052, which give no -a or -b (ref-6152.json has
    /// "paris_id":12 under that line, a run's stepped id), and none with
    /// `-a 0`. Starla decided it from -p, the destination port.
    fn lookup_failed(&self, error: LookupError) -> Option<MeasurementResult> {
        let paris = self.paris_id();
        let error = match error {
            LookupError::NoAddress => "name resolution failed: out of addresses".to_string(),
            error => format!("name resolution failed: {error}"),
        };
        Some(self.lookup_failed_line(paris, None, &error))
    }

    /// dns_cb's line for an address atlas_check_addr refuses
    /// (lookup_failed_line), with the same paris_id.
    fn address_not_allowed(&self) -> Option<MeasurementResult> {
        Some(self.lookup_failed_line(
            self.paris_id(),
            Some(self.config.target),
            "address not allowed",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn traceroute(target: &str, opts: TracerouteOptions, msm: u64) -> Traceroute {
        Traceroute {
            config: TracerouteConfig {
                target: target.parse().unwrap(),
                opts,
                backend: TracerouteBackend::Native,
                instance: Default::default(),
            },
            probe_id: ProbeId(1),
            msm_id: MeasurementId(msm),
            dst_name: DstName {
                name: target.into(),
                ttr: None,
            },
        }
    }

    /// The reference's line for msm 6016 (`evtraceroute -6 -U -c 3 -w 4000
    /// -A 6016 2001:503:c27::2:30`) in the 2026-10-13 twin run
    /// (wire-ctrl.jsonl, probe "ref"), with lts as the placeholder.
    #[test]
    fn the_udp6_result_line_is_the_references() {
        let opts = TracerouteOptions {
            timeout_ms: 4000,
            ..Default::default()
        };
        let t = traceroute("2001:db8:7769:6e00::20", opts, 6016);
        let mut hop = Hop::new(1);
        for rtt in ["0.155", "0.119", "0.229"] {
            hop.entries.push(format!(
                "\"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \"rtt\":{}, \"size\":88",
                rtt
            ));
        }
        let body = t.result_body(
            1791871244,
            1791871244,
            "\"dst_name\":\"2001:db8:7769:6e00::20\"",
            Some("2001:db8:7769:6e00::31".parse().unwrap()),
            t.config.opts.paris_id(1),
            &[hop],
        );
        assert_eq!(
            format!("RESULT {{ {} }}", body),
            "RESULT { \"id\":\"6016\", \"fw\":5130, \"mver\": \"2.6.4\", \"lts\":0, \
             \"time\":1791871244, \"endtime\":1791871244, \"dst_name\":\"2001:db8:7769:6e00::20\", \
             \"dst_addr\":\"2001:db8:7769:6e00::20\", \"src_addr\":\"2001:db8:7769:6e00::31\", \
             \"proto\":\"UDP\", \"af\": 6, \"size\":40, \"paris_id\":1, \"result\": [ { \"hop\":1, \
             \"result\": [ { \"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \"rtt\":0.155, \"size\":88 }, \
             { \"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \"rtt\":0.119, \"size\":88 }, \
             { \"from\":\"2001:db8:7769:6e00::20\", \"ttl\":64, \"rtt\":0.229, \"size\":88 } ] } ] }"
        );
    }

    /// `-a 0` is classic traceroute: no "paris_id" at all; `-B` puts a
    /// "bundle" before dst_name.
    #[test]
    fn paris_id_only_with_paris_and_bundle_before_dst_name() {
        let opts = TracerouteOptions {
            protocol: TracerouteProtocol::ICMP,
            paris_mod: 0,
            bundle: Some("77".into()),
            ..Default::default()
        };
        let t = traceroute("192.0.2.1", opts, 5);
        let body = t.result_body(
            1,
            2,
            "\"dst_name\":\"192.0.2.1\"",
            None,
            None,
            &[Hop::new(1)],
        );
        assert!(
            body.contains("\"endtime\":2, \"bundle\":77, \"dst_name\":\"192.0.2.1\""),
            "{body}"
        );
        assert!(
            body.ends_with("\"proto\":\"ICMP\", \"af\": 4, \"size\":40, \"result\": [ { \"hop\":1, \"result\": [  ] } ]"),
            "{body}"
        );
    }

    /// A CRONLINE the controller delivers again while it is scheduled
    /// keeps its slot and its run count, as eperd's Insert() keeps the
    /// line; a changed line, a one-off, or the same line after its
    /// measurement went, starts over.
    #[test]
    fn a_redelivered_line_keeps_its_slot_and_paris_id() {
        let opts = TracerouteOptions {
            timeout_ms: 4000,
            ..Default::default()
        };
        let line = |opts: &TracerouteOptions, interval| {
            TracerouteInstance::for_line(6001, "2001:7fd::1", 6, opts, interval, 0)
        };
        let first = line(&opts, 900);
        assert_eq!(first.start().unwrap().underway(), 1);
        assert_eq!(opts.paris_id(1), Some(1));

        let again = line(&opts, 900);
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(again.index(), first.index());
        assert_eq!(again.start().unwrap().underway(), 2);

        // What the scheduler holds now: the redelivered spec's instance.
        drop(first);
        assert_eq!(line(&opts, 900).start().unwrap().underway(), 3);

        let changed = TracerouteOptions {
            count: 1,
            ..opts.clone()
        };
        let other = line(&changed, 900);
        assert!(!Arc::ptr_eq(&other, &again));
        assert_ne!(other.index(), again.index());
        assert_eq!(other.start().unwrap().underway(), 1);
        assert!(!Arc::ptr_eq(&line(&opts, 1800), &again));
        assert!(!Arc::ptr_eq(&line(&opts, 0), &again));
        assert!(!Arc::ptr_eq(
            &TracerouteInstance::for_line(6002, "2001:7fd::1", 6, &opts, 900, 0),
            &again
        ));

        // Stopped, then delivered again: a new measurement.
        drop(again);
        assert_eq!(line(&opts, 900).start().unwrap().underway(), 1);
    }

    /// Each measurement has its own slot while it exists, the lowest free
    /// one; its runs count up from 1.
    #[test]
    fn instances_take_the_lowest_free_slot_and_count_runs() {
        assert_eq!(lowest_free(&BTreeSet::new()), 0);
        assert_eq!(lowest_free(&[0, 1, 3].into_iter().collect()), 2);
        let a = TracerouteInstance::default();
        let b = TracerouteInstance::default();
        assert_ne!(a.index(), b.index());
        assert_eq!(a.start().unwrap().underway(), 1);
        assert_eq!(a.start().unwrap().underway(), 2);
    }

    /// traceroute_start's busy check (traceroute.c:4934-4938): while a run
    /// is going, the next one doesn't start — it would share the slot and
    /// read the other's replies as its own — and the Paris id doesn't step
    /// for it.
    #[test]
    fn a_run_does_not_start_while_the_last_one_is_busy() {
        let opts = TracerouteOptions::default();
        let inst = TracerouteInstance::default();
        assert_eq!(
            opts.paris_id(inst.runs()),
            Some(0),
            "parisbase before any run"
        );
        let first = inst.start().unwrap();
        assert_eq!(first.underway(), 1);
        assert!(inst.start().is_none());
        assert!(inst.start().is_none());
        assert_eq!(inst.runs(), 1);
        drop(first);
        let second = inst.start().unwrap();
        assert_eq!(second.underway(), 2);
        assert_eq!(opts.paris_id(2), Some(2));
        drop(second);
        // A run whose lookup failed: started, never under way.
        drop(inst.start().unwrap());
        assert_eq!(inst.runs(), 2);
        assert_eq!(inst.start().unwrap().underway(), 3);
    }

    /// The measurement says so and sends nothing: no result, as eperd
    /// prints "busy, can't start" and returns.
    #[tokio::test]
    async fn execute_refuses_a_second_run_at_once() {
        let t = traceroute("192.0.2.1", TracerouteOptions::default(), 6001);
        let busy = t.config.instance.start().unwrap();
        let Err(e) = t.execute().await else {
            panic!("a second run at once")
        };
        assert_eq!(e.to_string(), "traceroute 6001: busy, can't start");
        drop(busy);
        assert_eq!(
            t.config.instance.runs(),
            0,
            "the refused run stepped nothing"
        );
    }

    /// RIPE's built-in 6152 (`evtraceroute -6 -I -c 3 -w 4000 -A 6152
    /// <name>`): ICMP, -S 40, Paris with -a 16 and no run yet.
    fn traceroute_6152(target: &str, dst_name: DstName) -> Traceroute {
        let opts = TracerouteOptions {
            protocol: TracerouteProtocol::ICMP,
            timeout_ms: 4000,
            ..Default::default()
        };
        let mut t = traceroute(target, opts, 6152);
        t.probe_id = ProbeId(1018212);
        t.dst_name = dst_name;
        t
    }

    /// The reference's line for msm 6152 (`evtraceroute -6 -I -c 3 -w 4000
    /// -A 6152 topology6.dyndns.atlas.ripe.net`) on the twin of 2026-10-13,
    /// up to its hops: the name and its ttr between "endtime" and
    /// "dst_addr". Starla wrote the address as dst_name and no ttr.
    #[test]
    fn a_name_target_reports_the_name_and_ttr() {
        let t = traceroute_6152(
            "2001:db8:7769:6e00::20",
            DstName {
                name: "topology6.dyndns.atlas.ripe.net".into(),
                ttr: Some(1.003708),
            },
        );
        let body = t.result_body(
            1791871334,
            1791871334,
            &t.dst_name.fields(),
            Some("2001:db8:7769:6e00::31".parse().unwrap()),
            Some(1),
            &[],
        );
        let reference = "\"id\":\"6152\", \"fw\":5130, \"mver\": \"2.6.4\", \"lts\":0, \
             \"time\":1791871334, \"endtime\":1791871334, \
             \"dst_name\":\"topology6.dyndns.atlas.ripe.net\", \"ttr\":1.003708, \
             \"dst_addr\":\"2001:db8:7769:6e00::20\", \"src_addr\":\"2001:db8:7769:6e00::31\", \
             \"proto\":\"ICMP\", \"af\": 6, \"size\":40, \"paris_id\":1, \"result\": ";
        assert!(body.starts_with(reference), "{body}");

        // an address literal has no ttr (msm 6016's line)
        let t = traceroute_6152(
            "2001:db8:7769:6e00::20",
            DstName {
                name: "2001:db8:7769:6e00::20".into(),
                ttr: None,
            },
        );
        let body = t.result_body(1, 2, &t.dst_name.fields(), None, Some(1), &[]);
        assert!(
            body.contains(
                "\"endtime\":2, \"dst_name\":\"2001:db8:7769:6e00::20\", \"dst_addr\":\"2001:db8:7769:6e00::20\", \"src_addr\":\"\""
            ),
            "{body}"
        );
    }

    /// dns_cb's line for a name that does not resolve (traceroute.c:4822-4838
    /// and report()): name and ttr, no dst_addr or src_addr, the family
    /// asked for, and "name resolution failed" where ping says "dns
    /// resolution failed".
    #[test]
    fn a_failed_lookup_reports_as_traceroute_c_does() {
        let t = traceroute_6152(
            "::",
            DstName {
                name: "nonexistent.example".into(),
                ttr: Some(2.5),
            },
        );
        let r = t.lookup_failed(LookupError::NoName).unwrap();
        let MeasurementData::FullLine(body) = r.data else {
            panic!("{:?}", r.data);
        };
        let time = r.timestamp.0;
        assert_eq!(
            body,
            format!(
                "\"id\":\"6152\", \"fw\":5130, \"mver\": \"2.6.4\", \"lts\":0, \"time\":{time}, \
                 \"endtime\":{time}, \"dst_name\":\"nonexistent.example\", \"ttr\":2.500000, \
                 \"proto\":\"ICMP\", \"af\": 6, \"size\":40, \"paris_id\":0, \"result\": [ {{ \
                 \"error\":\"name resolution failed: nodename nor servname provided, or not known\" }} ]"
            )
        );
        assert_eq!(r.src_addr, None);

        // -4, UDP, no Paris
        let mut t = traceroute_6152("0.0.0.0", t.dst_name.clone());
        t.config.opts.protocol = TracerouteProtocol::UDP;
        t.config.opts.paris_mod = 0;
        let r = t.lookup_failed(LookupError::Fail).unwrap();
        let MeasurementData::FullLine(body) = r.data else {
            panic!("{:?}", r.data);
        };
        assert!(
            body.ends_with(
                "\"proto\":\"UDP\", \"af\": 4, \"size\":40, \"result\": [ { \"error\":\"name resolution failed: non-recoverable failure in name resolution\" } ]"
            ),
            "{body}"
        );
    }

    /// dns_cb's other two lines: a hosts entry of the other family only
    /// ("out of addresses", without the reference's stray " } ] }"), and an
    /// address atlas_check_addr refuses, with dst_addr and no src_addr
    /// (traceroute.c:4840-4842, 4886-4923).
    #[test]
    fn out_of_addresses_and_not_allowed() {
        let dst_name = DstName {
            name: "lan.example".into(),
            ttr: Some(0.5),
        };
        let r = traceroute_6152("::", dst_name.clone())
            .lookup_failed(LookupError::NoAddress)
            .unwrap();
        let MeasurementData::FullLine(body) = r.data else {
            panic!("{:?}", r.data);
        };
        assert!(
            body.ends_with(
                "\"ttr\":0.500000, \"proto\":\"ICMP\", \"af\": 6, \"size\":40, \"paris_id\":0, \"result\": [ { \"error\":\"name resolution failed: out of addresses\" } ]"
            ),
            "{body}"
        );

        let r = traceroute_6152("fd00::7", dst_name)
            .address_not_allowed()
            .unwrap();
        let MeasurementData::FullLine(body) = r.data else {
            panic!("{:?}", r.data);
        };
        assert!(
            body.ends_with(
                "\"dst_name\":\"lan.example\", \"ttr\":0.500000, \"dst_addr\":\"fd00::7\", \"proto\":\"ICMP\", \"af\": 6, \"size\":40, \"paris_id\":0, \"result\": [ { \"error\":\"address not allowed\" } ]"
            ),
            "{body}"
        );
        let line: serde_json::Value = serde_json::from_str(&format!("{{{body}}}")).unwrap();
        assert!(line.get("src_addr").is_none());
    }

    /// dns_cb's lines go through report() as a run's does, so -B's
    /// "bundle" comes before dst_name in them too (traceroute.c:381-397),
    /// and their paris_id is the one the last run left: parisbase before
    /// any, then the run's ([`TracerouteInstance::runs`]).
    #[test]
    fn a_failed_lookup_line_has_the_bundle_and_the_last_runs_paris_id() {
        let mut t = traceroute_6152(
            "::",
            DstName {
                name: "nonexistent.example".into(),
                ttr: Some(2.5),
            },
        );
        t.config.opts.bundle = Some("77".into());
        let line = |t: &Traceroute| match t.lookup_failed(LookupError::NoName).unwrap().data {
            MeasurementData::FullLine(body) => body,
            other => panic!("{other:?}"),
        };
        let body = line(&t);
        assert!(
            body.contains(", \"bundle\":77, \"dst_name\":\"nonexistent.example\", \"ttr\":2.500000, \"proto\""),
            "{body}"
        );
        assert!(body.contains("\"paris_id\":0, "), "{body}");
        for _ in 0..3 {
            t.config.instance.start().unwrap().underway();
        }
        let body = line(&t);
        assert!(body.contains("\"paris_id\":3, "), "{body}");
    }
}
