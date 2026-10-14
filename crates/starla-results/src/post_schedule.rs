//! When the controller's httppost lines run.
//!
//! RIPE's controller schedules the probe's uploads as CRONLINEs, like any
//! other job, and the reference probe's perd runs them (`bin/ripe-atlas.in`:
//! `perd -c $CRON_DIR/main`). Session 2 of enodia's 2026-10-13 journal:
//!
//! ```text
//! CRONLINE 60 12 2000000000 UNIFORM 20 httppost … --post-dir /home/atlas/data/out … http://127.0.0.1:8080/?PROBE_ID=…&SESSION_ID=… >> …
//! CRONLINE 900 312 2000000000 UNIFORM 20 httppost … http://127.0.0.1:8080/?PROBE_ID=…&SESSION_ID=…&SRC=timesync >> …
//! ```
//!
//! perd runs cycle `k` of a line at `start_time + k * interval`, moved by a
//! whole-second UNIFORM offset drawn afresh for each cycle. starla-scheduler's
//! `ScheduledTask` does the same for eperd's measurements, but that crate
//! depends on this one, so perd's arithmetic is repeated here, as perd has it.

use rand::RngExt;

/// perd's longest interval, two years (perd.c:78).
pub(crate) const MAX_INTERVAL: u64 = 2 * 366 * 24 * 3600;

/// An httppost CRONLINE's timing fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PostSchedule {
    /// Seconds between cycles
    pub interval: u64,
    /// The cycles' phase: cycle `k` is due at `start_time + k * interval`
    pub start_time: i64,
    /// No cycle runs after this (unix seconds)
    pub end_time: i64,
    /// UNIFORM spread in seconds (0 for NONE)
    pub spread: u64,
}

/// perd's per-line state: the next cycle and its offset.
#[derive(Debug, Clone)]
pub(crate) struct Slots {
    pub(crate) schedule: PostSchedule,
    cycle: i64,
    offset: i64,
}

impl Slots {
    /// A line just read from the crontab: the first cycle after `now`
    /// (perd.c:505-506, `nextcycle= (now-start_time)/interval + 1`) and a
    /// fresh offset (perd.c:529). Never "now": the twin's reference probe
    /// posted 42.8 s after KEEP, on the slot at :12 ± 10 s; Starla posted at
    /// KEEP.
    pub(crate) fn new(schedule: PostSchedule, now: i64) -> Self {
        Self {
            schedule,
            cycle: first_cycle(now, &schedule),
            offset: uniform_offset(schedule.spread),
        }
    }

    /// perd's CopyFromOld (perd.c:654-710): a crontab that repeats a line
    /// (same interval, start time and command) keeps its next cycle, and its
    /// offset too if the distribution is the same. A controller re-sending
    /// the crontab moves no upload.
    pub(crate) fn carried_over(old: &Slots, schedule: PostSchedule) -> Self {
        Self {
            schedule,
            cycle: old.cycle,
            offset: if old.schedule.spread == schedule.spread {
                old.offset
            } else {
                uniform_offset(schedule.spread)
            },
        }
    }

    /// Whether `schedule` has this line's cycles (interval and start time),
    /// as CopyFromOld compares them.
    pub(crate) fn same_cycles(&self, schedule: &PostSchedule) -> bool {
        self.schedule.interval == schedule.interval
            && self.schedule.start_time == schedule.start_time
    }

    /// When the next cycle is due (unix seconds), or None if it never will
    /// be: perd runs a line once `now` has reached both the cycle's time and
    /// `start_time`, and only while `now <= end_time` (perd.c:814-818).
    pub(crate) fn next_run(&self, now: i64) -> Option<i64> {
        let at = (self.slot(self.cycle) + self.offset).max(self.schedule.start_time);
        (now <= self.schedule.end_time && at <= self.schedule.end_time).then_some(at)
    }

    /// perd's TestJobs test (perd.c:814-818).
    pub(crate) fn is_due(&self, now: i64) -> bool {
        now >= self.slot(self.cycle) + self.offset
            && now >= self.schedule.start_time
            && now <= self.schedule.end_time
    }

    /// perd's TestJobs once it starts a job (perd.c:833-842): the next
    /// cycle, or the first one after `now` if that slot has already passed,
    /// and a new offset.
    pub(crate) fn advance(&mut self, now: i64) {
        self.cycle += 1;
        if self.slot(self.cycle) <= now {
            self.cycle = first_cycle(now, &self.schedule);
        }
        self.offset = uniform_offset(self.schedule.spread);
    }

    /// perd's guard against a clock that went back (perd.c:783-791): more
    /// than ten cycles ahead of `now` starts over from cycle 0, which is due
    /// at once.
    pub(crate) fn check_clock(&mut self, now: i64) {
        let s = &self.schedule;
        if now >= s.start_time && now < s.start_time + (self.cycle - 10) * s.interval as i64 {
            self.cycle = 0;
        }
    }

    fn slot(&self, cycle: i64) -> i64 {
        self.schedule.start_time + cycle * self.schedule.interval as i64
    }
}

/// `(now - start_time) / interval + 1`, truncated as C truncates.
fn first_cycle(now: i64, schedule: &PostSchedule) -> i64 {
    if schedule.interval == 0 {
        return 0;
    }
    (now - schedule.start_time) / schedule.interval as i64 + 1
}

/// perd's do_distr (perd.c:412-431): for UNIFORM, a whole number in
/// `[0, spread]`, less `spread / 2`.
fn uniform_offset(spread: u64) -> i64 {
    if spread == 0 {
        return 0;
    }
    rand::rng().random_range(0..=spread) as i64 - (spread / 2) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The twin's KEEP, 2026-10-13 (wire-ctrl.jsonl: 1791871112.115).
    const KEEP: i64 = 1_791_871_112;

    fn main_line(spread: u64) -> PostSchedule {
        // CRONLINE 60 12 2000000000 UNIFORM 20 httppost … --post-dir …
        PostSchedule {
            interval: 60,
            start_time: 12,
            end_time: 2_000_000_000,
            spread,
        }
    }

    #[test]
    fn the_first_post_is_the_next_slot_not_now() {
        let s = Slots::new(main_line(0), KEEP);
        // :12 of the next minute: 1791871152, 40 s after KEEP.
        assert_eq!(s.next_run(KEEP), Some(1_791_871_152));
        assert!(!s.is_due(KEEP));
        assert!(s.is_due(1_791_871_152));

        // The timesync line, CRONLINE 900 312 …: 400 s after KEEP.
        let ts = PostSchedule {
            interval: 900,
            start_time: 312,
            ..main_line(0)
        };
        assert_eq!(Slots::new(ts, KEEP).next_run(KEEP), Some(1_791_871_512));
    }

    #[test]
    fn slots_advance_by_the_interval_and_skip_missed_ones() {
        let mut s = Slots::new(main_line(0), KEEP);
        s.advance(1_791_871_152);
        assert_eq!(s.next_run(1_791_871_152), Some(1_791_871_212));
        // Suspended for ten minutes: the missed slots are not replayed.
        s.advance(1_791_871_212 + 600);
        assert_eq!(s.next_run(1_791_871_812), Some(1_791_871_872));
    }

    #[test]
    fn the_spread_is_whole_seconds_around_the_slot() {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..2_000 {
            let s = Slots::new(main_line(20), KEEP);
            let off = s.next_run(KEEP).unwrap() - 1_791_871_152;
            assert!((-10..=10).contains(&off), "{off}");
            seen.insert(off);
        }
        assert_eq!(seen.len(), 21, "every offset in [-10, 10]: {seen:?}");
    }

    #[test]
    fn a_repeated_line_keeps_its_cycle_and_offset() {
        let mut old = Slots::new(main_line(20), KEEP);
        old.advance(1_791_871_160);
        let again = Slots::carried_over(&old, main_line(20));
        assert_eq!(again.next_run(KEEP), old.next_run(KEEP));
        assert!(old.same_cycles(&main_line(20)));
        assert!(!old.same_cycles(&PostSchedule {
            start_time: 13,
            ..main_line(20)
        }));
    }

    #[test]
    fn nothing_runs_after_the_end_time() {
        let s = Slots::new(
            PostSchedule {
                end_time: KEEP + 10,
                ..main_line(0)
            },
            KEEP,
        );
        assert_eq!(s.next_run(KEEP), None);
        assert!(!s.is_due(1_791_871_152));
        // A cycle due before the end, but the end has passed (a suspended
        // host): it never runs, so nothing waits for it.
        let s = Slots::new(main_line(0), KEEP);
        let line = PostSchedule {
            end_time: 1_791_871_160,
            ..main_line(0)
        };
        let s = Slots::carried_over(&s, line);
        assert_eq!(s.next_run(1_791_871_159), Some(1_791_871_152));
        assert_eq!(s.next_run(1_791_871_161), None);
    }

    #[test]
    fn nothing_runs_before_the_start_time() {
        // Two cycles ahead: C's truncating division puts the first cycle
        // before start_time, and perd waits for start_time itself.
        let line = PostSchedule {
            start_time: KEEP + 130,
            ..main_line(0)
        };
        let s = Slots::new(line, KEEP);
        assert_eq!(s.next_run(KEEP), Some(KEEP + 130));
        assert!(!s.is_due(KEEP + 70));
        assert!(s.is_due(KEEP + 130));
    }

    #[test]
    fn a_clock_that_went_back_starts_over() {
        let mut s = Slots::new(main_line(0), KEEP);
        // The clock steps back an hour: 60 cycles ahead of now.
        let now = KEEP - 3_600;
        s.check_clock(now);
        assert!(s.is_due(now), "cycle 0 is due at once");
        s.advance(now);
        assert_eq!(s.next_run(now), Some(1_791_867_552));
        // An ordinary next slot is left alone.
        let mut s = Slots::new(main_line(0), KEEP);
        s.check_clock(KEEP);
        assert_eq!(s.next_run(KEEP), Some(1_791_871_152));
    }
}
