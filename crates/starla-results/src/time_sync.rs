//! Time synchronization tracking for lts (local time sync) calculation
//!
//! The `lts` field in RIPE Atlas results indicates how many seconds have passed
//! since the probe last synchronized its clock (typically via NTP).

use std::path::PathBuf;
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
    /// Where to keep the last sync, so it survives a restart (the reference
    /// reads it back with get_timesync, atlas_timesync.c).
    path: Option<PathBuf>,
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
            path: None,
        }
    }

    /// Like [`new`](Self::new), but read the last sync back from `path` and
    /// rewrite it there on every sync, so `lts` survives a restart. A
    /// missing or unreadable file is "never synced", as get_timesync's -1.
    pub fn open(path: PathBuf) -> Self {
        let last = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse::<i64>().ok())
            .unwrap_or(NEVER);
        Self {
            last_sync: AtomicI64::new(last),
            path: Some(path),
        }
    }

    /// Create a tracker with a specific last sync timestamp
    pub fn with_last_sync(timestamp: i64) -> Self {
        Self {
            last_sync: AtomicI64::new(timestamp),
            path: None,
        }
    }

    /// Record that time was just synchronized
    pub fn mark_synced(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.set_last_sync(now);
    }

    /// Record a specific sync time
    pub fn set_last_sync(&self, timestamp: i64) {
        self.last_sync.store(timestamp, Ordering::SeqCst);
        if let Some(path) = &self.path {
            self.persist(path, timestamp);
        }
    }

    /// Write `timestamp` atomically (temp file, then rename), as httppost.c
    /// writes the reference's timesync file. Failure is not fatal: lts just
    /// does not survive the next restart.
    fn persist(&self, path: &std::path::Path, timestamp: i64) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("new");
        if std::fs::write(&tmp, format!("{timestamp}\n")).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
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

    #[test]
    fn a_tracker_remembers_the_last_sync_across_a_restart() {
        let dir = std::env::temp_dir().join(format!("starla-timesync-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("timesync");

        // No file yet: never synced, lts -1.
        let tracker = TimeSyncTracker::open(path.clone());
        assert_eq!(tracker.lts(), -1);

        tracker.set_last_sync(1_700_000_000);
        let restarted = TimeSyncTracker::open(path.clone());
        assert_eq!(restarted.last_sync_time(), 1_700_000_000);
        assert!(restarted.lts() >= 0);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
