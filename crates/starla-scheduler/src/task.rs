//! Measurement Scheduler

use super::MeasurementJob;
use rand::Rng;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct ScheduledTask {
    pub id: u64,
    pub interval: u64,
    pub start_time: i64,
    pub end_time: i64,
    pub spread: u64,
    /// The job specification - DNS resolution happens at execution time
    pub job: MeasurementJob,
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
        // Calculate first run time based on spread
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let initial_delay = if spread > 0 {
            rand::thread_rng().gen_range(0..spread)
        } else {
            0
        };

        let start = if start_time > now { start_time } else { now };

        Self {
            id,
            interval,
            start_time,
            end_time,
            spread,
            job,
            next_run: start + initial_delay as i64,
        }
    }

    pub fn is_due(&self, now: i64) -> bool {
        self.next_run <= now && (self.end_time == 0 || self.next_run <= self.end_time)
    }

    pub fn schedule_next(&mut self) {
        self.next_run += self.interval as i64;
    }

    /// Check if the task has expired (past its end time)
    pub fn is_expired(&self, now: i64) -> bool {
        self.end_time > 0 && now > self.end_time
    }
}
