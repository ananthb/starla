//! Measurement Scheduler

use super::MeasurementJob;
use rand::RngExt;
use std::time::{SystemTime, UNIX_EPOCH};

/// A recurring measurement on the reference probe's (eperd's) slots: cycle
/// `k` runs at `start_time + k * interval`, moved by a uniform offset in
/// `[-spread/2, +spread/2]` drawn afresh each cycle. The first run is the
/// first slot after the task was loaded, never "now", so a restart does not
/// run every measurement at once (2026-10-12: three restarts in 30 min sent
/// RIPE three runs of daily measurements within the half hour).
pub struct ScheduledTask {
    pub id: u64,
    pub interval: u64,
    pub start_time: i64,
    pub end_time: i64,
    pub spread: u64,
    /// The job specification - DNS resolution happens at execution time
    pub job: MeasurementJob,
    /// The cycle `next_run` belongs to
    pub cycle: i64,
    pub next_run: i64,
}

impl ScheduledTask {
    pub fn new(
        id: u64,
        interval: u64,
        start_time: i64,
        end_time: i64,
        spread: u64,
        job: MeasurementJob,
    ) -> Self {
        Self::new_at(unix_now(), id, interval, start_time, end_time, spread, job)
    }

    pub(crate) fn new_at(
        now: i64,
        id: u64,
        interval: u64,
        start_time: i64,
        end_time: i64,
        spread: u64,
        job: MeasurementJob,
    ) -> Self {
        let mut task = Self {
            id,
            interval,
            start_time,
            end_time,
            spread,
            job,
            cycle: 0,
            next_run: 0,
        };
        task.draw_cycle(now);
        task
    }

    pub fn is_due(&self, now: i64) -> bool {
        self.next_run <= now && (self.end_time == 0 || self.next_run <= self.end_time)
    }

    /// A crontab repeats this line, with `end_time`. Insert() takes the end
    /// time and re-arms the line's timer for the time it had,
    /// set_timeout(oldLine, 0) (eperd.c:815-824): the line keeps its cycle
    /// and offset. Except a line that had stopped — its next run past its
    /// old end time, where RunJob returned without arming its timer again
    /// (eperd.c:1313-1317) — whose next run is now more than 10 s past: the
    /// re-armed timer fires at once, and RunJob, finding the time more than
    /// 10 s from nexttime, runs nothing and draws the first cycle after now
    /// (set_timeout(line, 1), eperd.c:1268-1310). Such a line does not run
    /// between slots.
    pub(crate) fn take_end_time(&mut self, end_time: i64, now: i64) {
        let had_stopped = self.end_time != 0 && self.next_run > self.end_time;
        self.end_time = end_time;
        self.job.end_time = end_time;
        if had_stopped && now > self.next_run + TIME_SLACK {
            self.draw_cycle(now);
        }
    }

    /// Move to the next cycle after a run; if that slot has already passed
    /// (a long run, a suspended host), skip to the first one after `now`, as
    /// eperd's RunJob does.
    pub fn schedule_next(&mut self, now: i64) {
        self.cycle += 1;
        if slot(self.start_time, self.interval, self.cycle) < now {
            self.cycle = first_cycle_after(now, self.start_time, self.interval);
        }
        self.next_run =
            slot(self.start_time, self.interval, self.cycle) + uniform_offset(self.spread);
    }

    /// Check if the task has expired (past its end time)
    pub fn is_expired(&self, now: i64) -> bool {
        self.end_time > 0 && now > self.end_time
    }

    /// eperd's set_timeout(line, 1) (eperd.c:744-753): the first cycle
    /// after `now`, its offset drawn afresh (do_distr, :489-511).
    fn draw_cycle(&mut self, now: i64) {
        self.cycle = first_cycle_after(now, self.start_time, self.interval);
        self.next_run =
            slot(self.start_time, self.interval, self.cycle) + uniform_offset(self.spread);
    }
}

/// How far from its time RunJob still runs a line (eperd.c:1268): further,
/// the time is "inconsistent", and the line's cycle is drawn again.
const TIME_SLACK: i64 = 10;

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// eperd's set_timeout: cycle 0 while `start_time` is still ahead, else the
/// first cycle whose slot is strictly after `now`.
fn first_cycle_after(now: i64, start_time: i64, interval: u64) -> i64 {
    if interval == 0 || now < start_time {
        0
    } else {
        (now - start_time) / interval as i64 + 1
    }
}

fn slot(start_time: i64, interval: u64, cycle: i64) -> i64 {
    start_time + cycle * interval as i64
}

/// eperd's do_distr for UNIFORM: an integer in `[0, spread]`, less
/// `spread / 2`.
fn uniform_offset(spread: u64) -> i64 {
    if spread == 0 {
        return 0;
    }
    rand::rng().random_range(0..=spread) as i64 - (spread / 2) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{MeasurementSpec, PingJobSpec};

    fn job() -> MeasurementJob {
        MeasurementJob {
            msm_id: 1001,
            interval: 900,
            start_time: 312,
            end_time: 2_000_000_000,
            spread: 0,
            spec: MeasurementSpec::Ping(PingJobSpec {
                target: "193.0.14.129".into(),
                af: 4,
                packets: 3,
                packet_interval: 1000,
                size: 48,
                kept: Default::default(),
            }),
        }
    }

    fn task(now: i64, interval: u64, start_time: i64, spread: u64) -> ScheduledTask {
        ScheduledTask::new_at(now, 1, interval, start_time, 2_000_000_000, spread, job())
    }

    #[test]
    fn a_loaded_task_waits_for_its_next_slot() {
        // CRONLINE 900 312 …, loaded at 1791836000: slots are 312 + k*900.
        let now = 1_791_836_000;
        let t = task(now, 900, 312, 0);
        assert_eq!(t.next_run, 1_791_836_412);
        assert!(!t.is_due(now), "a restart must not run it at once");

        // A daily one loaded just after its slot waits a day.
        let t = task(now, 86_400, 1_791_835_990, 0);
        assert_eq!(t.next_run, 1_791_835_990 + 86_400);

        // A start time still ahead is the first slot.
        let t = task(now, 900, now + 5_000, 0);
        assert_eq!(t.next_run, now + 5_000);
    }

    #[test]
    fn runs_advance_slot_by_slot_and_skip_missed_ones() {
        let now = 1_791_836_000;
        let mut t = task(now, 900, 312, 0);
        let first = t.next_run;
        t.schedule_next(first);
        assert_eq!(t.next_run, first + 900);

        // Suspended for an hour: the missed slots are skipped, not replayed.
        let late = t.next_run + 3_600 + 1;
        t.schedule_next(late);
        assert!(
            t.next_run > late && t.next_run <= late + 900,
            "{}",
            t.next_run
        );
        assert_eq!((t.next_run - 312) % 900, 0);
    }

    #[test]
    fn the_spread_centres_on_the_slot() {
        let now = 1_791_836_000;
        for _ in 0..200 {
            let t = task(now, 900, 312, 20);
            assert!((t.next_run - 1_791_836_412).abs() <= 10, "{}", t.next_run);
        }
    }
}
