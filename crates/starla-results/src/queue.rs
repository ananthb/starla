//! Result queue for buffering measurements before upload

use starla_common::MeasurementResult;
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

/// A queued result with metadata
#[derive(Debug, Clone)]
pub struct QueuedResult {
    /// The measurement result
    pub result: MeasurementResult,
    /// When this was queued (unix timestamp)
    pub queued_at: i64,
    /// Number of upload attempts
    pub attempts: u32,
}

impl QueuedResult {
    /// Create a new queued result
    pub fn new(result: MeasurementResult) -> Self {
        let queued_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        Self {
            result,
            queued_at,
            attempts: 0,
        }
    }
}

/// Queue for buffering results before upload
pub struct ResultQueue {
    /// Queued results
    queue: VecDeque<QueuedResult>,
    /// Maximum queue size
    max_size: usize,
    /// Total bytes in queue (approximate)
    total_bytes: usize,
}

impl ResultQueue {
    /// Create a new result queue
    pub fn new(max_size: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            max_size,
            total_bytes: 0,
        }
    }

    /// Enqueue a result
    pub fn enqueue(&mut self, result: MeasurementResult) -> anyhow::Result<()> {
        // Estimate size
        let size = serde_json::to_string(&result)
            .map(|s| s.len())
            .unwrap_or(1000);

        if self.queue.len() >= self.max_size {
            // Drop oldest result
            if let Some(old) = self.queue.pop_front() {
                let old_size = serde_json::to_string(&old.result)
                    .map(|s| s.len())
                    .unwrap_or(0);
                self.total_bytes = self.total_bytes.saturating_sub(old_size);
            }
        }

        self.queue.push_back(QueuedResult::new(result));
        self.total_bytes += size;
        Ok(())
    }

    /// Get the number of queued results
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Check if queue is empty
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Get approximate total bytes
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Drain up to n results from the front
    pub fn drain_batch(&mut self, n: usize) -> Vec<QueuedResult> {
        let count = std::cmp::min(n, self.queue.len());
        let mut results = Vec::with_capacity(count);

        for _ in 0..count {
            if let Some(r) = self.queue.pop_front() {
                let size = serde_json::to_string(&r.result)
                    .map(|s| s.len())
                    .unwrap_or(0);
                self.total_bytes = self.total_bytes.saturating_sub(size);
                results.push(r);
            }
        }

        results
    }

    /// Peek at the front result without removing
    pub fn peek(&self) -> Option<&QueuedResult> {
        self.queue.front()
    }

    /// Clear all results
    pub fn clear(&mut self) {
        self.queue.clear();
        self.total_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starla_common::{MeasurementData, MeasurementId, MeasurementType, ProbeId, Timestamp};
    use std::net::IpAddr;

    fn make_result() -> MeasurementResult {
        MeasurementResult {
            fw: 6000,
            measurement_type: MeasurementType::Ping,
            prb_id: ProbeId(12345),
            msm_id: MeasurementId(1001),
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
    fn test_queue_operations() {
        let mut queue = ResultQueue::new(10);
        assert!(queue.is_empty());

        queue.enqueue(make_result()).unwrap();
        assert_eq!(queue.len(), 1);

        let batch = queue.drain_batch(1);
        assert_eq!(batch.len(), 1);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_queue_overflow() {
        let mut queue = ResultQueue::new(3);

        for _ in 0..5 {
            queue.enqueue(make_result()).unwrap();
        }

        // Should only have 3 results (oldest dropped)
        assert_eq!(queue.len(), 3);
    }
}
