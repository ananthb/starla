//! The recurring lines the scheduler runs, kept as eperd keeps the lines of
//! its crontab (eperd/eperd.c, LineBase).
//!
//! eperd tells one line from another by its interval, start time and
//! command text (cl_Shell), as Insert() compares them (eperd.c:782-836, at
//! 789-791 and 806-808), and never by the measurement id. RIPE's controller
//! sends msm 30002 as 26 lines, each a 30 s window ten minutes after the
//! last, one per `--query`. Keyed by msm id, Starla kept only the last of
//! them: on enodia, 2 results in 12 h of the 78 windows it was sent.
//!
//! A crontab the controller sends is read as SynchronizeFile reads one
//! (eperd.c:513-645): every line of that file is marked to delete
//! (:528-529); a line of the new file that the file already had keeps its
//! cycle, its offset and its test state, and takes only the new end_time
//! (Insert, :815-827) — unless it had stopped at its old end time and its
//! time has passed by more than 10 s, when it waits for its first slot
//! after now (RunJob, :1268-1310); a new line waits for its first slot
//! after now, with an offset of its own (Start, :838-848, and set_timeout,
//! :736-777); and DeleteFile (:856-895) removes the lines still marked.
//! RIPE's controller sends the whole crontab again on every session (the
//! hourly REREGISTER, each reconnect). Starla made each re-sent line a new
//! one, with a new offset, from the first slot after now, so a line that
//! had run early in its cycle (a negative offset) and was re-sent before
//! that cycle's slot ran in it again: two results for one slot.

use crate::task::ScheduledTask;
use crate::MeasurementJob;
use std::collections::HashMap;
use tracing::info;

/// A crontab file's recurring measurement lines, as the controller last
/// sent the file.
#[derive(Debug)]
pub struct Crontab {
    /// The crontab: the directory its CRONTAB line named, as telnetd
    /// rebases it (`/var/spool/ripe-atlas/crons/7`). Each directory's is a
    /// line set of its own, as each is a cron daemon's of its own: perd's
    /// `crons/main`, eperd's `crons/7` (bin/ripe-atlas.in:126-133; 5130 runs
    /// no eperd on `crons/2`, which RIPE's controller sends with a condmv
    /// line only).
    pub file: String,
    /// Its lines, in the order the file has them
    pub lines: Vec<CrontabLine>,
}

/// One line of a crontab, scheduled.
#[derive(Debug, Clone)]
pub struct CrontabLine {
    /// The line's command and arguments, eperd's cl_Shell (everything after
    /// its fifth field): with the job's interval and start time, what makes
    /// it the line it is.
    pub command: String,
    pub job: MeasurementJob,
}

/// Which line a scheduled task is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LineKey {
    /// A crontab's line, by eperd's identity within its file
    Crontab {
        file: String,
        interval: u64,
        start_time: i64,
        command: String,
    },
    /// A recurring job scheduled on its own, outside any crontab (Starla's
    /// JSON commands): by its msm id, as Starla has always kept those
    Own(u64),
}

impl LineKey {
    fn in_file(&self, name: &str) -> bool {
        matches!(self, LineKey::Crontab { file, .. } if file == name)
    }
}

/// What a crontab did to its file's lines.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Synchronized {
    /// Lines the file already had, kept with their slots
    pub kept: usize,
    /// New lines
    pub added: usize,
    /// Lines the file no longer has, removed
    pub dropped: usize,
}

/// The recurring lines, every crontab's and Starla's own.
#[derive(Default)]
pub(crate) struct Lines {
    lines: HashMap<LineKey, ScheduledTask>,
}

impl Lines {
    /// A job scheduled on its own replaces the one with its msm id, and
    /// starts afresh: the first slot after `now`.
    pub(crate) fn schedule(&mut self, job: MeasurementJob, now: i64) {
        self.lines.insert(LineKey::Own(job.msm_id), start(job, now));
    }

    /// The file's lines are now the crontab's, as SynchronizeFile then
    /// DeleteFile leave them (eperd.c:513-645, 856-895). The other files'
    /// lines are untouched.
    pub(crate) fn synchronize(&mut self, crontab: Crontab, now: i64) -> Synchronized {
        let Crontab { file, lines } = crontab;
        // Every line of the file needs delete (eperd.c:528-529)
        let (mut old, others): (HashMap<_, _>, HashMap<_, _>) = std::mem::take(&mut self.lines)
            .into_iter()
            .partition(|(key, _)| key.in_file(&file));
        self.lines = others;

        let mut synchronized = Synchronized::default();
        for CrontabLine { command, job } in lines {
            let key = LineKey::Crontab {
                file: file.clone(),
                interval: job.interval,
                start_time: job.start_time,
                command,
            };
            // Insert(): a line the file had keeps its cycle, its offset and
            // its job, whose test state (PingKept, TracerouteInstance) its
            // runs share; only the end time is the new line's, and
            // set_timeout(oldLine, 0) arms the timer it had
            // (eperd.c:820-824) — unless the line had stopped at its old end
            // time and its time has long passed (take_end_time). A line
            // twice in one file is one line, the first, with the later end
            // time: Insert() finds the first among the lines it has just
            // inserted.
            let had = old.remove(&key);
            let kept = had.is_some();
            match had.or_else(|| self.lines.remove(&key)) {
                Some(mut task) => {
                    task.take_end_time(job.end_time, now);
                    synchronized.kept += usize::from(kept);
                    self.lines.insert(key, task);
                }
                // Start(): a new line, on the first slot after now
                // (set_timeout(line, 1), eperd.c:847)
                None => {
                    synchronized.added += 1;
                    self.lines.insert(key, start(job, now));
                }
            }
        }
        // DeleteFile(): the lines still marked go. One whose test is still
        // running stays until it is done, but never starts again (RunJob
        // returns on needs_delete, eperd.c:1258-1262; DeleteFile,
        // :875-883): a run under way here has its own copy of the job, and
        // finishes and reports whatever becomes of its line.
        synchronized.dropped = old.len();
        synchronized
    }

    /// STOP <msm_id>, Starla's own command: every line with that id, in
    /// any crontab. How many there were.
    pub(crate) fn stop(&mut self, msm_id: u64) -> usize {
        let before = self.lines.len();
        self.lines.retain(|_, task| task.job.msm_id != msm_id);
        before - self.lines.len()
    }

    /// The jobs due at `now`, each line once, its next cycle drawn (RunJob,
    /// eperd.c:1245-1339). A crontab's line past its end time stays, never
    /// due again, until a push leaves it out: eperd removes lines only in
    /// DeleteFile (856-895), never on expiry. Dropped here instead, a line
    /// the controller then sends again with a later end time would come
    /// back as a new line, on a fresh first slot that can be the cycle it
    /// has just run. A job of Starla's own goes once it has run past its end.
    pub(crate) fn take_due(&mut self, now: i64) -> Vec<MeasurementJob> {
        let mut due = Vec::new();
        self.lines.retain(|key, task| {
            let own = matches!(key, LineKey::Own(_));
            // An Own line whose next slot is already past its end time will
            // never run again: drop it rather than leave it in the map (and
            // counted in metrics) forever.
            if own && task.is_expired(now) && !task.is_due(now) {
                info!(
                    msm_id = task.job.msm_id,
                    "Measurement expired, removing from schedule"
                );
                return false;
            }
            if !task.is_due(now) {
                return true;
            }
            due.push(task.job.clone());
            task.schedule_next(now);
            if own && task.is_expired(now) {
                info!(
                    msm_id = task.job.msm_id,
                    "Measurement expired, removing from schedule"
                );
                return false;
            }
            true
        });
        due
    }

    pub(crate) fn len(&self) -> usize {
        self.lines.len()
    }

    /// The lines by measurement type ("ping", "dns", ...).
    pub(crate) fn counts(&self) -> HashMap<String, u64> {
        let mut counts: HashMap<String, u64> = HashMap::new();
        for task in self.lines.values() {
            *counts
                .entry(task.job.spec.type_name().to_string())
                .or_insert(0) += 1;
        }
        counts
    }

    /// The task of a crontab's line, if the file has it.
    #[cfg(test)]
    fn line(&self, file: &str, line: &CrontabLine) -> Option<&ScheduledTask> {
        self.lines.get(&LineKey::Crontab {
            file: file.to_string(),
            interval: line.job.interval,
            start_time: line.job.start_time,
            command: line.command.clone(),
        })
    }

    #[cfg(test)]
    fn line_mut(&mut self, file: &str, line: &CrontabLine) -> Option<&mut ScheduledTask> {
        self.lines.get_mut(&LineKey::Crontab {
            file: file.to_string(),
            interval: line.job.interval,
            start_time: line.job.start_time,
            command: line.command.clone(),
        })
    }
}

/// A line just loaded: its first slot after `now`, never `now` itself.
fn start(job: MeasurementJob, now: i64) -> ScheduledTask {
    ScheduledTask::new_at(
        now,
        job.msm_id,
        job.interval,
        job.start_time,
        job.end_time,
        job.spread,
        job,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{DnsJobSpec, MeasurementSpec, PingJobSpec};

    const CRONS_2: &str = "/var/spool/ripe-atlas/crons/2";
    const CRONS_7: &str = "/var/spool/ripe-atlas/crons/7";

    fn crontab(file: &str, lines: &[CrontabLine]) -> Crontab {
        Crontab {
            file: file.to_string(),
            lines: lines.to_vec(),
        }
    }

    /// `CRONLINE <interval> <start> <end> UNIFORM <spread> <command>`, an
    /// evping line, as the controller sends one.
    fn ping_line(
        msm_id: u64,
        interval: u64,
        start_time: i64,
        end_time: i64,
        spread: u64,
        target: &str,
    ) -> CrontabLine {
        CrontabLine {
            command: format!("evping -4 -c 3 -A \"{msm_id}\" -O /home/atlas/data/new/7 {target}"),
            job: MeasurementJob {
                msm_id,
                interval,
                start_time,
                end_time,
                spread,
                spec: MeasurementSpec::Ping(PingJobSpec {
                    target: target.into(),
                    af: 4,
                    packets: 3,
                    packet_interval: 1000,
                    size: 0,
                    kept: Default::default(),
                }),
            },
        }
    }

    /// The 26 lines of msm 30002 that ctr-dub-sw02 sent enodia on
    /// 2026-10-13 (the twin's scenario enodia-2026-10-13T0131): `CRONLINE
    /// 600 <start> <start + 30> UNIFORM 3 evtdig -4 --resolv --qbuf --type 1
    /// --class 1 --query <name> -A "30002" -O /home/atlas/data/new/7`, the
    /// starts ten minutes apart.
    fn msm_30002() -> Vec<CrontabLine> {
        let names = [
            "jd.com.",
            "google.co.uk.",
            "ebay.com.",
            "google.fr.",
            "msn.com.",
            "bing.com.",
            "google.ru.",
            "weibo.com.",
            "netflix.com.",
            "wordpress.com.",
            "microsoft.com.",
            "tumblr.com.",
            "hao123.com.",
            "amazon.co.jp.",
            "google.com.br.",
            "yandex.ru.",
            "imdb.com.",
            "google.it.",
            "stackoverflow.com.",
            "apple.com.",
            "onclkds.com.",
            "google.es.",
            "pinterest.com.",
            "office.com.",
            "google.com.hk.",
            "t.co.",
        ];
        names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let start_time = 1_791_854_199 + 600 * i as i64;
                CrontabLine {
                    command: format!(
                        "evtdig -4 --resolv --qbuf --type 1 --class 1 --query {name} -A \"30002\" \
                         -O /home/atlas/data/new/7"
                    ),
                    job: MeasurementJob {
                        msm_id: 30002,
                        interval: 600,
                        start_time,
                        end_time: start_time + 30,
                        spread: 3,
                        spec: MeasurementSpec::Dns(DnsJobSpec {
                            resolv: true,
                            qbuf: true,
                            qtype: 1,
                            qclass: 1,
                            query: (*name).into(),
                            ..Default::default()
                        }),
                    },
                }
            })
            .collect()
    }

    /// The query of a msm 30002 run.
    fn query(job: &MeasurementJob) -> String {
        let MeasurementSpec::Dns(spec) = &job.spec else {
            panic!("{job:?}");
        };
        spec.query.clone()
    }

    /// Every run from `from` to `to`, a scheduler tick each second.
    fn runs(lines: &mut Lines, from: i64, to: i64) -> Vec<(i64, MeasurementJob)> {
        (from..=to)
            .flat_map(|now| lines.take_due(now).into_iter().map(move |job| (now, job)))
            .collect()
    }

    #[test]
    fn every_line_of_one_msm_id_runs_in_its_own_window() {
        let push = msm_30002();
        let first = push[0].job.start_time;
        let last = push[25].job.start_time;

        // The crontab arrives two minutes before the first window: all 26
        // windows are ahead, and each line runs once, in its window — at
        // its slot, moved by UNIFORM 3's -1..=2 s (do_distr, eperd.c:489-511)
        let mut lines = Lines::default();
        let synced = lines.synchronize(crontab(CRONS_7, &push), first - 120);
        assert_eq!(synced.added, 26);
        assert_eq!(lines.len(), 26, "keyed by msm id, Starla kept 1");
        assert_eq!(lines.counts(), HashMap::from([("dns".to_string(), 26)]));
        let ran = runs(&mut lines, first - 120, last + 3_600);
        assert_eq!(ran.len(), 26, "{ran:?}");
        for (line, (at, job)) in push.iter().zip(&ran) {
            assert_eq!(query(job), query(&line.job));
            let slot = line.job.start_time;
            assert!(
                (slot - 1..=slot + 2).contains(at),
                "{} ran at {at}, its slot {slot}",
                query(job)
            );
        }

        // As enodia got it, at 1791855121: jd.com.'s and google.co.uk.'s
        // windows had passed (cycle 1 is after their end time,
        // set_timeout's first cycle after now, eperd.c:744-753), and the
        // 24 others each run once.
        let mut lines = Lines::default();
        let pushed = 1_791_855_121;
        lines.synchronize(crontab(CRONS_7, &push), pushed);
        let ran = runs(&mut lines, pushed, last + 3_600);
        let queries: Vec<String> = ran.iter().map(|(_, job)| query(job)).collect();
        let wanted: Vec<String> = push[2..].iter().map(|line| query(&line.job)).collect();
        assert_eq!(queries, wanted);
    }

    #[test]
    fn a_resent_line_keeps_its_cycle_and_offset_and_runs_once_a_cycle() {
        // `CRONLINE 900 312 … UNIFORM 20 evping …`: slots at 312 + k * 900,
        // each moved by -10..=10 s
        let line = ping_line(1001, 900, 312, 2_000_000_000, 20, "193.0.14.129");
        let slot = 1_791_836_412;
        let mut lines = Lines::default();
        lines.synchronize(crontab(CRONS_7, std::slice::from_ref(&line)), slot - 400);
        let task = lines.line_mut(CRONS_7, &line).unwrap();
        assert!((slot - 10..=slot + 10).contains(&task.next_run));
        // This cycle's offset came out -8: the line runs 8 s before its slot
        task.next_run = slot - 8;
        assert!(runs(&mut lines, slot - 400, slot - 9).is_empty());
        assert_eq!(lines.take_due(slot - 8).len(), 1);
        let task = lines.line(CRONS_7, &line).unwrap();
        let after_the_run = (task.cycle, task.next_run);
        assert!((slot + 890..=slot + 910).contains(&task.next_run));

        // The controller sends the crontab again 5 s later, before this
        // cycle's slot, the line as it was but for its end time. On main,
        // Starla made it a new task, whose first slot after now is this
        // cycle's: it would have run again.
        assert_eq!(
            ScheduledTask::new_at(
                slot - 3,
                1001,
                900,
                312,
                2_000_000_000,
                20,
                line.job.clone()
            )
            .cycle,
            after_the_run.0 - 1
        );
        let mut resent = line.clone();
        resent.job.end_time = 2_100_000_000;
        let synced = lines.synchronize(crontab(CRONS_7, &[resent]), slot - 3);
        assert_eq!(
            synced,
            Synchronized {
                kept: 1,
                added: 0,
                dropped: 0
            }
        );
        let task = lines.line(CRONS_7, &line).unwrap();
        assert_eq!((task.cycle, task.next_run), after_the_run);
        assert_eq!(task.end_time, 2_100_000_000);
        assert_eq!(task.job.end_time, 2_100_000_000);
        // Nothing more in this cycle; the next runs on the slot it had
        assert!(runs(&mut lines, slot - 3, after_the_run.1 - 1).is_empty());
        assert_eq!(lines.take_due(after_the_run.1).len(), 1);

        // The same job is kept, and with it what its runs share: the
        // re-sent line's own (a PingKept of its own here) is dropped.
        let kept = |lines: &Lines| {
            let MeasurementSpec::Ping(spec) = &lines.line(CRONS_7, &line).unwrap().job.spec else {
                unreachable!()
            };
            spec.kept.clone()
        };
        let before = kept(&lines);
        let resent = ping_line(1001, 900, 312, 2_000_000_000, 20, "193.0.14.129");
        lines.synchronize(crontab(CRONS_7, &[resent]), slot + 1_000);
        assert!(std::sync::Arc::ptr_eq(&before, &kept(&lines)));

        // An end time moved before the next slot is taken: no more runs
        let task = lines.line(CRONS_7, &line).unwrap();
        let next = task.next_run;
        let mut ending = line.clone();
        ending.job.end_time = next - 1;
        lines.synchronize(crontab(CRONS_7, &[ending]), slot + 1_000);
        assert!(runs(&mut lines, slot + 1_000, next + 3_600).is_empty());
    }

    #[test]
    fn a_line_that_stopped_at_its_end_time_runs_on_its_slots_once_extended() {
        // `CRONLINE 900 312 <slot + 100> UNIFORM 20 evping …`: it runs in
        // the cycle of `slot`, and then stops, the next slot being past its
        // end time. It stays a line of its file, as it stays in eperd's
        // LineBase.
        let slot = 1_791_836_412;
        let at = |ran: Vec<(i64, MeasurementJob)>| ran.into_iter().map(|(at, _)| at).collect();
        let kept = |lines: &Lines, line: &CrontabLine| {
            let MeasurementSpec::Ping(spec) = &lines.line(CRONS_7, line).unwrap().job.spec else {
                unreachable!()
            };
            spec.kept.clone()
        };
        let line = ping_line(1001, 900, 312, slot + 100, 20, "193.0.14.129");
        let mut lines = Lines::default();
        lines.synchronize(crontab(CRONS_7, std::slice::from_ref(&line)), slot - 400);
        let ran: Vec<i64> = at(runs(&mut lines, slot - 400, slot + 900 * 40));
        assert_eq!(ran.len(), 1);
        assert!((slot - 10..=slot + 10).contains(&ran[0]), "{ran:?}");
        assert_eq!(lines.len(), 1);
        let before = kept(&lines, &line);

        // The controller is away for 10 h, longer than the 8 h RIPE gives a
        // user measurement (end time = its session + 28800 in the twin's
        // scenario enodia-2026-10-13T0131), and comes back mid-cycle with
        // the line, its end time 8 h on. Kept with the next run it had, the
        // line ran at once, between slots, and again at the next: two
        // results in one interval. eperd's re-armed timer fires at once
        // and RunJob, finding the time inconsistent, runs nothing and draws
        // the first cycle after now (eperd.c:1268-1310).
        let back = slot + 36_000 + 450;
        let next = slot + 36_900;
        let mut extended = line.clone();
        extended.job.end_time = back + 28_800;
        let synced = lines.synchronize(crontab(CRONS_7, &[extended]), back);
        assert_eq!(synced.kept, 1);
        let task = lines.line(CRONS_7, &line).unwrap();
        assert_eq!(task.end_time, back + 28_800);
        assert!(
            (next - 10..=next + 10).contains(&task.next_run),
            "{} is not the slot after {back}",
            task.next_run
        );
        let next_run = task.next_run;
        // (the cycle after starts at next + 890 at the earliest)
        assert_eq!(at(runs(&mut lines, back, next + 889)), vec![next_run]);
        // still the line it was, with its job
        assert!(std::sync::Arc::ptr_eq(&before, &kept(&lines, &line)));

        // Within 10 s of the time it stopped at, the line runs at once, as
        // RunJob runs a line within 10 s of its time (eperd.c:1268); from
        // 11 s on, it waits for its next slot.
        for (late, wanted) in [
            (10, vec![slot + 910, slot + 1_800]),
            (11, vec![slot + 1_800]),
        ] {
            let line = ping_line(1001, 900, 312, slot + 100, 0, "193.0.14.129");
            let mut lines = Lines::default();
            lines.synchronize(crontab(CRONS_7, std::slice::from_ref(&line)), slot - 400);
            assert_eq!(
                at(runs(&mut lines, slot - 400, slot + 900 + late)),
                vec![slot]
            );
            let mut extended = line.clone();
            extended.job.end_time = 2_000_000_000;
            lines.synchronize(crontab(CRONS_7, &[extended]), slot + 900 + late);
            let ran = at(runs(&mut lines, slot + 900 + late, slot + 1_800));
            assert_eq!(ran, wanted, "re-sent {late} s after its time");
        }

        // A line that had not stopped keeps its time however late the tick
        // that takes it: the scheduler loop runs a one-off in line, and a
        // tick held up behind it runs a line late, as it did before
        let line = ping_line(1001, 900, 312, 2_000_000_000, 0, "193.0.14.129");
        let mut lines = Lines::default();
        lines.synchronize(crontab(CRONS_7, std::slice::from_ref(&line)), slot - 400);
        lines.synchronize(crontab(CRONS_7, std::slice::from_ref(&line)), slot + 30);
        assert_eq!(lines.line(CRONS_7, &line).unwrap().next_run, slot);
        assert_eq!(lines.take_due(slot + 30).len(), 1);
    }

    /// A line that ran late in its last cycle, past its end time, stays;
    /// sent again with a later end time it keeps the cycle it is on, so the
    /// cycle it ran early in does not run twice (Insert, eperd.c:820-824).
    #[test]
    fn an_expired_line_stays_and_keeps_its_cycle_when_extended() {
        let line = ping_line(1001, 900, 312, 0, 20, "193.0.14.129");
        let slot = 1_791_836_412;
        let mut ending = line.clone();
        ending.job.end_time = slot - 7;
        let mut lines = Lines::default();
        lines.synchronize(crontab(CRONS_7, std::slice::from_ref(&ending)), slot - 400);
        // This cycle's offset came out -8, inside the end time, and the
        // tick that runs it comes late, after the end time has passed
        lines.line_mut(CRONS_7, &line).unwrap().next_run = slot - 8;
        assert_eq!(lines.take_due(slot - 6).len(), 1);
        assert!(runs(&mut lines, slot - 5, slot + 2_000).is_empty());
        let task = lines.line(CRONS_7, &line).expect("kept past its end time");
        let after_the_run = (task.cycle, task.next_run);

        // The controller sends the line again, before this cycle's slot,
        // with a later end time
        let mut extended = line.clone();
        extended.job.end_time = 2_000_000_000;
        let synced = lines.synchronize(crontab(CRONS_7, &[extended]), slot - 3);
        assert_eq!(synced.kept, 1);
        let task = lines.line(CRONS_7, &line).unwrap();
        assert_eq!((task.cycle, task.next_run), after_the_run);
        assert!(runs(&mut lines, slot - 3, after_the_run.1 - 1).is_empty());
        assert_eq!(lines.take_due(after_the_run.1).len(), 1);

        // A push without it is what removes it
        lines.synchronize(crontab(CRONS_7, &[]), slot + 1_000);
        assert!(lines.line(CRONS_7, &line).is_none());
    }

    #[test]
    fn a_line_with_another_interval_start_or_command_is_another_line() {
        let line = ping_line(1001, 900, 312, 2_000_000_000, 20, "193.0.14.129");
        let now = 1_791_836_000;
        let changed = |f: &dyn Fn(&mut CrontabLine)| {
            let mut changed = line.clone();
            f(&mut changed);
            changed
        };
        for other in [
            changed(&|l| l.job.interval = 1800),
            changed(&|l| l.job.start_time = 313),
            changed(&|l| l.command = l.command.replace("-c 3", "-c 4")),
        ] {
            let mut lines = Lines::default();
            lines.synchronize(crontab(CRONS_7, std::slice::from_ref(&line)), now);
            let synced = lines.synchronize(crontab(CRONS_7, std::slice::from_ref(&other)), now + 1);
            assert_eq!(
                synced,
                Synchronized {
                    kept: 0,
                    added: 1,
                    dropped: 1
                },
                "{other:?}"
            );
            assert_eq!(lines.len(), 1);
            assert!(lines.line(CRONS_7, &line).is_none());
            let task = lines.line(CRONS_7, &other).unwrap();
            assert!(task.next_run > now + 1, "a new line waits for its slot");
        }

        // The end time and the spread are not what a line is: one that
        // changes only those is the same line, which keeps its spread and
        // offset and takes the end time (Insert, eperd.c:820)
        let mut lines = Lines::default();
        lines.synchronize(crontab(CRONS_7, std::slice::from_ref(&line)), now);
        let before = lines.line(CRONS_7, &line).unwrap().next_run;
        let other = changed(&|l| {
            l.job.end_time = 2_000_000_100;
            l.job.spread = 200;
        });
        let synced = lines.synchronize(crontab(CRONS_7, &[other]), now + 1);
        assert_eq!(synced.kept, 1);
        let task = lines.line(CRONS_7, &line).unwrap();
        assert_eq!((task.next_run, task.spread), (before, 20));
        assert_eq!(task.end_time, 2_000_000_100);

        // A line twice in one crontab is one line, with the later end time
        let mut lines = Lines::default();
        let twice = [line.clone(), changed(&|l| l.job.end_time = 2_000_000_200)];
        let synced = lines.synchronize(crontab(CRONS_7, &twice), now);
        assert_eq!((synced.added, lines.len()), (1, 1));
        assert_eq!(lines.line(CRONS_7, &line).unwrap().end_time, 2_000_000_200);
    }

    #[test]
    fn a_crontab_replaces_its_own_files_lines_only() {
        let now = 1_791_836_000;
        let a = ping_line(1001, 900, 312, 2_000_000_000, 20, "193.0.14.129");
        let b = ping_line(1002, 240, 17, 2_000_000_000, 3, "2001:7fd::1");
        let c = ping_line(1003, 240, 18, 2_000_000_000, 3, "192.0.2.1");
        let mut lines = Lines::default();
        lines.synchronize(crontab(CRONS_2, std::slice::from_ref(&c)), now);
        lines.synchronize(crontab(CRONS_7, &[a.clone(), b.clone()]), now);
        lines.schedule(ping_line(7, 60, 0, 0, 0, "192.0.2.7").job, now);
        assert_eq!(lines.len(), 4);

        // crons/7 again without b: b stops, a keeps its slot, crons/2's line
        // and Starla's own job are untouched
        let a_next = lines.line(CRONS_7, &a).unwrap().next_run;
        let c_next = lines.line(CRONS_2, &c).unwrap().next_run;
        let synced = lines.synchronize(crontab(CRONS_7, std::slice::from_ref(&a)), now + 10);
        assert_eq!(
            synced,
            Synchronized {
                kept: 1,
                added: 0,
                dropped: 1
            }
        );
        assert!(lines.line(CRONS_7, &b).is_none());
        assert_eq!(lines.line(CRONS_7, &a).unwrap().next_run, a_next);
        assert_eq!(lines.line(CRONS_2, &c).unwrap().next_run, c_next);
        assert_eq!(lines.len(), 3);
        let ran: Vec<u64> = runs(&mut lines, now + 10, now + 1_000)
            .into_iter()
            .map(|(_, job)| job.msm_id)
            .collect();
        assert!(!ran.contains(&1002), "{ran:?}");
        assert!(ran.contains(&1001) && ran.contains(&1003) && ran.contains(&7));

        // The same line in another file is another line
        lines.synchronize(crontab(CRONS_2, &[c.clone(), a.clone()]), now + 1_000);
        assert!(lines.line(CRONS_2, &a).is_some());
        assert!(lines.line(CRONS_7, &a).is_some());
        assert_eq!(lines.len(), 4);

        // An empty crontab empties its file
        let synced = lines.synchronize(crontab(CRONS_2, &[]), now + 1_001);
        assert_eq!(synced.dropped, 2);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn stop_takes_every_line_with_the_id() {
        let now = 1_791_836_000;
        let mut push = msm_30002();
        let other = ping_line(1001, 900, 312, 2_000_000_000, 20, "193.0.14.129");
        push.push(other.clone());
        let mut lines = Lines::default();
        lines.synchronize(crontab(CRONS_7, &push), now);
        lines.synchronize(crontab(CRONS_2, &msm_30002()[..3]), now);
        lines.schedule(ping_line(30002, 60, 0, 0, 0, "192.0.2.7").job, now);
        assert_eq!(lines.len(), 31);
        assert_eq!(lines.stop(30002), 30);
        assert_eq!(lines.len(), 1);
        assert!(lines.line(CRONS_7, &other).is_some());
        assert_eq!(lines.stop(30002), 0);
    }

    #[test]
    fn a_job_of_its_own_replaces_the_one_with_its_id() {
        let now = 1_791_836_000;
        let mut lines = Lines::default();
        lines.schedule(ping_line(7, 60, 0, 0, 0, "192.0.2.7").job, now);
        lines.schedule(ping_line(7, 120, 0, 0, 0, "192.0.2.7").job, now);
        assert_eq!(lines.len(), 1);
        // and no crontab touches it
        lines.synchronize(crontab(CRONS_7, &[]), now);
        assert_eq!(lines.len(), 1);
    }
}
