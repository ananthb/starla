//! Time synchronization tracking for lts (local time sync) calculation
//!
//! The `lts` field in RIPE Atlas results indicates how many seconds have passed
//! since the probe last synchronized its clock (typically via NTP).

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const NEVER: i64 = i64::MIN;

/// Tracks time synchronization status for calculating `lts` values
///
/// The `lts` (local time sync) field in measurement results shows how many
/// seconds have elapsed since the last time synchronization. This helps
/// consumers assess the reliability of timestamps in the results.
#[derive(Debug)]
pub struct TimeSyncTracker {
    /// Unix timestamp of the last time sync
    last_sync: AtomicI64,
}

impl Default for TimeSyncTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeSyncTracker {
    /// Create a tracker that has never synced: `lts` is -1 until the first
    /// upload whose answer shows our clock agrees with the controller's, as
    /// the reference probe's get_timesync() is -1 without a timesync file.
    /// (Starla assumed a sync at start and never marked one, so its lts was
    /// its uptime: 12,336 s in probe 1018212's uploads on 2026-10-12.)
    pub fn new() -> Self {
        Self {
            last_sync: AtomicI64::new(NEVER),
        }
    }

    /// Create a tracker with a specific last sync timestamp
    pub fn with_last_sync(timestamp: i64) -> Self {
        Self {
            last_sync: AtomicI64::new(timestamp),
        }
    }

    /// Record that time was just synchronized
    pub fn mark_synced(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.last_sync.store(now, Ordering::SeqCst);
    }

    /// Record a specific sync time
    pub fn set_last_sync(&self, timestamp: i64) {
        self.last_sync.store(timestamp, Ordering::SeqCst);
    }

    /// Get the last sync timestamp
    pub fn last_sync_time(&self) -> i64 {
        self.last_sync.load(Ordering::SeqCst)
    }

    /// Calculate current lts value (seconds since last sync, -1 if never)
    pub fn lts(&self) -> i64 {
        let last = self.last_sync.load(Ordering::SeqCst);
        if last == NEVER {
            return -1;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // Ensure non-negative (in case of clock issues)
        (now - last).max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn a_new_tracker_has_never_synced() {
        let tracker = TimeSyncTracker::new();
        assert_eq!(tracker.lts(), -1);
        tracker.mark_synced();
        assert!((0..2).contains(&tracker.lts()));
    }

    #[test]
    fn test_lts_increases() {
        let tracker = TimeSyncTracker::new();
        tracker.mark_synced();
        let lts1 = tracker.lts();

        sleep(Duration::from_millis(100));

        let lts2 = tracker.lts();
        // lts2 should be >= lts1 (time passes)
        assert!(lts2 >= lts1);
    }

    #[test]
    fn test_mark_synced_resets() {
        // Create with an old timestamp
        let old_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - 100;
        let tracker = TimeSyncTracker::with_last_sync(old_time);

        // lts should be around 100
        assert!(tracker.lts() >= 100);

        // Mark as synced
        tracker.mark_synced();

        // lts should be very small again
        assert!(tracker.lts() < 2);
    }
}
