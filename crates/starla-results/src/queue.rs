//! In-memory result queue with configurable capacity
//!
//! A simple VecDeque-based queue that holds measurement results
//! until they are uploaded. Drops oldest results when at capacity.

use starla_common::MeasurementResult;
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// A queued measurement result with metadata
#[derive(Debug, Clone)]
pub struct QueuedResult {
    /// The measurement result
    pub result: MeasurementResult,
    /// When this result was queued (unix timestamp)
    pub queued_at: i64,
    /// Number of upload attempts
    pub attempts: u32,
    /// Timestamp of last upload attempt
    pub last_attempt_at: Option<i64>,
}

/// Queue statistics
#[derive(Debug, Clone)]
pub struct QueueStats {
    /// Number of items in the queue
    pub count: usize,
    /// Maximum capacity
    pub capacity: usize,
}

/// In-memory result queue with a maximum capacity.
/// When full, oldest results are dropped to make room.
pub struct ResultQueue {
    items: VecDeque<QueuedResult>,
    max_capacity: usize,
}

impl ResultQueue {
    /// Create a new queue with the given maximum capacity
    pub fn new(max_capacity: usize) -> Self {
        Self {
            items: VecDeque::with_capacity(max_capacity.min(1024)),
            max_capacity,
        }
    }

    /// Add a result to the queue. Drops oldest if at capacity.
    pub fn enqueue(&mut self, result: MeasurementResult) {
        if self.items.len() >= self.max_capacity {
            if let Some(d) = self.items.pop_front() {
                warn!(
                    msm_id = d.result.msm_id.0,
                    "Queue full ({}), dropping oldest result", self.max_capacity
                );
            }
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.items.push_back(QueuedResult {
            result,
            queued_at: now,
            attempts: 0,
            last_attempt_at: None,
        });
    }

    /// Take up to `n` results from the front of the queue
    pub fn drain_batch(&mut self, n: usize) -> Vec<QueuedResult> {
        let count = n.min(self.items.len());
        self.items.drain(..count).collect()
    }

    /// Take all results from the queue
    pub fn drain_all(&mut self) -> Vec<QueuedResult> {
        self.items.drain(..).collect()
    }

    /// Put failed results back at the front with incremented attempt counter
    pub fn requeue_failed(&mut self, mut results: Vec<QueuedResult>) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        for r in &mut results {
            r.attempts += 1;
            r.last_attempt_at = Some(now);
        }

        for r in results.into_iter().rev() {
            self.items.push_front(r);
        }

        while self.items.len() > self.max_capacity {
            self.items.pop_back();
        }
    }

    /// Remove results older than `max_age_secs`. Returns number of items
    /// removed.
    pub fn cleanup_expired(&mut self, max_age_secs: i64) -> usize {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let before = self.items.len();
        self.items.retain(|r| now - r.queued_at < max_age_secs);
        let removed = before - self.items.len();
        if removed > 0 {
            debug!("Cleaned up {} expired results", removed);
        }
        removed
    }

    /// Remove results that exceeded max attempts. Returns number of items
    /// removed.
    pub fn cleanup_failed(&mut self, max_attempts: u32) -> usize {
        let before = self.items.len();
        self.items.retain(|r| r.attempts < max_attempts);
        let removed = before - self.items.len();
        if removed > 0 {
            warn!("Dropped {} results that exceeded max attempts", removed);
        }
        removed
    }

    /// Get queue statistics
    pub fn stats(&self) -> QueueStats {
        QueueStats {
            count: self.items.len(),
            capacity: self.max_capacity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starla_common::{MeasurementData, MeasurementId, MeasurementType, ProbeId, Timestamp};
    use std::net::IpAddr;

    fn make_result(msm_id: u64) -> MeasurementResult {
        MeasurementResult {
            fw: 5120,
            measurement_type: MeasurementType::Ping,
            prb_id: ProbeId(12345),
            msm_id: MeasurementId(msm_id),
            timestamp: Timestamp::now(),
            af: 4,
            dst_addr: "8.8.8.8".parse::<IpAddr>().unwrap(),
            dst_name: None,
            src_addr: None,
            proto: Some("ICMP".to_string()),
            ttl: Some(64),
            size: Some(32),
            data: MeasurementData::Generic(serde_json::json!([{"rtt": 12.5}])),
        }
    }

    #[test]
    fn test_enqueue_and_drain() {
        let mut q = ResultQueue::new(100);
        q.enqueue(make_result(1));
        q.enqueue(make_result(2));
        q.enqueue(make_result(3));

        assert_eq!(q.stats().count, 3);

        let batch = q.drain_batch(2);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].result.msm_id.0, 1);
        assert_eq!(batch[1].result.msm_id.0, 2);
        assert_eq!(q.stats().count, 1);
    }

    #[test]
    fn test_capacity_eviction() {
        let mut q = ResultQueue::new(3);
        q.enqueue(make_result(1));
        q.enqueue(make_result(2));
        q.enqueue(make_result(3));
        q.enqueue(make_result(4));

        assert_eq!(q.stats().count, 3);
        let batch = q.drain_batch(3);
        assert_eq!(batch[0].result.msm_id.0, 2);
    }

    #[test]
    fn test_requeue_failed() {
        let mut q = ResultQueue::new(100);
        q.enqueue(make_result(1));
        q.enqueue(make_result(2));

        let batch = q.drain_batch(2);
        assert_eq!(q.stats().count, 0);

        q.requeue_failed(batch);
        assert_eq!(q.stats().count, 2);

        let batch = q.drain_batch(1);
        assert_eq!(batch[0].attempts, 1);
    }

    #[test]
    fn test_cleanup_failed() {
        let mut q = ResultQueue::new(100);
        q.enqueue(make_result(1));

        for _ in 0..5 {
            let batch = q.drain_batch(1);
            q.requeue_failed(batch);
        }

        assert_eq!(q.stats().count, 1);
        q.cleanup_failed(5);
        assert_eq!(q.stats().count, 0);
    }
}
