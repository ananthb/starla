//! Persistent result queue backed by RocksDB
//!
//! This module provides a durable queue for measurement results that survives
//! process restarts. Results are kept in memory for fast access but are also
//! persisted to RocksDB for durability.

use rocksdb::{Options, DB};
use serde::{Deserialize, Serialize};
use starla_common::MeasurementResult;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, error, warn};

/// A queued result with metadata and persistence ID
#[derive(Debug, Clone)]
pub struct QueuedResult {
    /// Database ID (None if only in memory)
    pub id: Option<u64>,
    /// The measurement result
    pub result: MeasurementResult,
    /// When this was queued (unix timestamp)
    pub queued_at: i64,
    /// Number of upload attempts
    pub attempts: u32,
    /// Last attempt timestamp
    pub last_attempt_at: Option<i64>,
}

impl QueuedResult {
    /// Create a new queued result
    pub fn new(result: MeasurementResult) -> Self {
        let queued_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        Self {
            id: None,
            result,
            queued_at,
            attempts: 0,
            last_attempt_at: None,
        }
    }

    /// Increment attempt counter
    pub fn increment_attempts(&mut self) {
        self.attempts += 1;
        self.last_attempt_at = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
        );
    }
}

/// Stored queue record
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredQueueRecord {
    id: u64,
    msm_id: u64,
    prb_id: u64,
    measurement_type: String,
    result_json: String,
    queued_at: i64,
    attempts: u32,
    last_attempt_at: Option<i64>,
}

/// Queue statistics
#[derive(Debug, Clone, Default)]
pub struct QueueStats {
    /// Number of results in memory
    pub memory_count: usize,
    /// Number of results on disk (may overlap with memory)
    pub disk_count: usize,
    /// Approximate total bytes
    pub total_bytes: usize,
    /// Oldest result timestamp
    pub oldest_queued_at: Option<i64>,
}

/// Monotonic ID generator
struct IdGenerator {
    counter: AtomicU64,
}

impl IdGenerator {
    fn new() -> Self {
        // Initialize with current timestamp to ensure IDs are monotonically increasing
        // even across process restarts
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        Self {
            counter: AtomicU64::new(now << 16),
        }
    }

    fn next_id(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::SeqCst)
    }
}

/// Encode u64 as big-endian bytes
#[inline]
fn encode_u64(v: u64) -> [u8; 8] {
    v.to_be_bytes()
}

/// Encode i64 as big-endian bytes (with bias for proper ordering)
#[inline]
fn encode_i64(v: i64) -> [u8; 8] {
    ((v as u64).wrapping_add(0x8000000000000000u64)).to_be_bytes()
}

/// Persistent result queue with RocksDB backing
pub struct PersistentResultQueue {
    /// RocksDB instance
    db: DB,
    /// In-memory cache for fast access
    memory_cache: VecDeque<QueuedResult>,
    /// Maximum items to keep in memory
    max_memory_items: usize,
    /// Approximate bytes in memory
    memory_bytes: usize,
    /// ID generator
    id_gen: IdGenerator,
}

impl PersistentResultQueue {
    /// Create a new persistent queue
    pub fn new(db_path: &Path, max_memory_items: usize) -> anyhow::Result<Self> {
        // Create parent directory if needed
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Configure RocksDB
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_compression_type(rocksdb::DBCompressionType::Lz4);
        opts.set_write_buffer_size(4 * 1024 * 1024); // 4MB write buffer
        opts.set_max_write_buffer_number(2);

        let db = DB::open(&opts, db_path)?;

        let mut queue = Self {
            db,
            memory_cache: VecDeque::new(),
            max_memory_items,
            memory_bytes: 0,
            id_gen: IdGenerator::new(),
        };

        // Load any pending results from previous run
        let loaded = queue.load_pending()?;
        if loaded > 0 {
            debug!("Loaded {} pending results from disk", loaded);
        }

        Ok(queue)
    }

    /// Build key for queue item
    fn queue_key(queued_at: i64, id: u64) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + 8 + 8);
        key.push(b'q');
        key.extend_from_slice(&encode_i64(queued_at));
        key.extend_from_slice(&encode_u64(id));
        key
    }

    /// Enqueue a result
    pub fn enqueue(&mut self, result: MeasurementResult) -> anyhow::Result<()> {
        let queued = QueuedResult::new(result);
        let size = self.estimate_size(&queued);

        // Always persist to disk first for durability
        let id = self.persist(&queued)?;

        // Add to memory cache
        let mut queued_with_id = queued;
        queued_with_id.id = Some(id);

        // Evict oldest from memory if needed (but keep on disk)
        while self.memory_cache.len() >= self.max_memory_items {
            if let Some(evicted) = self.memory_cache.pop_front() {
                let evicted_size = self.estimate_size(&evicted);
                self.memory_bytes = self.memory_bytes.saturating_sub(evicted_size);
                debug!("Evicted result from memory cache (kept on disk)");
            }
        }

        self.memory_cache.push_back(queued_with_id);
        self.memory_bytes += size;

        Ok(())
    }

    /// Persist a result to RocksDB
    fn persist(&self, queued: &QueuedResult) -> anyhow::Result<u64> {
        let result_json = serde_json::to_string(&queued.result)?;
        let id = self.id_gen.next_id();

        let record = StoredQueueRecord {
            id,
            msm_id: queued.result.msm_id.0,
            prb_id: queued.result.prb_id.0.into(),
            measurement_type: queued.result.measurement_type.to_string(),
            result_json,
            queued_at: queued.queued_at,
            attempts: queued.attempts,
            last_attempt_at: queued.last_attempt_at,
        };

        let key = Self::queue_key(queued.queued_at, id);
        let value = serde_json::to_vec(&record)?;
        self.db.put(&key, &value)?;

        Ok(id)
    }

    /// Drain a batch of results for upload
    pub fn drain_batch(&mut self, n: usize) -> Vec<QueuedResult> {
        let mut results = Vec::with_capacity(n);

        // First try memory cache
        while results.len() < n && !self.memory_cache.is_empty() {
            if let Some(queued) = self.memory_cache.pop_front() {
                let size = self.estimate_size(&queued);
                self.memory_bytes = self.memory_bytes.saturating_sub(size);
                results.push(queued);
            }
        }

        // If we need more, load from disk (excluding items we already have)
        if results.len() < n {
            let needed = n - results.len();
            // Collect IDs we already drained from memory to exclude from disk query
            let drained_ids: Vec<u64> = results.iter().filter_map(|r| r.id).collect();
            match self.load_from_disk_excluding(needed, &drained_ids) {
                Ok(disk_results) => {
                    results.extend(disk_results);
                }
                Err(e) => {
                    error!("Failed to load results from disk: {}", e);
                }
            }
        }

        results
    }

    /// Load results from disk (excluding those in memory)
    fn load_from_disk(&self, limit: usize) -> anyhow::Result<Vec<QueuedResult>> {
        let memory_ids: Vec<u64> = self.memory_cache.iter().filter_map(|r| r.id).collect();
        self.load_from_disk_excluding(limit, &memory_ids)
    }

    /// Load results from disk excluding specific IDs
    fn load_from_disk_excluding(
        &self,
        limit: usize,
        exclude_ids: &[u64],
    ) -> anyhow::Result<Vec<QueuedResult>> {
        let mut results = Vec::new();
        let iter = self.db.prefix_iterator(b"q");

        for item in iter {
            let (key, value) = item?;

            if !key.starts_with(b"q") {
                break;
            }

            if let Ok(record) = serde_json::from_slice::<StoredQueueRecord>(&value) {
                // Skip excluded IDs
                if exclude_ids.contains(&record.id) {
                    continue;
                }

                match serde_json::from_str::<MeasurementResult>(&record.result_json) {
                    Ok(result) => {
                        results.push(QueuedResult {
                            id: Some(record.id),
                            result,
                            queued_at: record.queued_at,
                            attempts: record.attempts,
                            last_attempt_at: record.last_attempt_at,
                        });

                        if results.len() >= limit {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Failed to deserialize result {}: {}", record.id, e);
                        // Delete corrupt entry
                        let _ = self.db.delete(&key);
                    }
                }
            }
        }

        Ok(results)
    }

    /// Mark results as successfully uploaded (delete from disk)
    pub fn mark_uploaded(&mut self, results: &[QueuedResult]) -> anyhow::Result<()> {
        for queued in results {
            if let Some(id) = queued.id {
                let key = Self::queue_key(queued.queued_at, id);
                self.db.delete(&key)?;
            }
        }

        debug!("Marked {} results as uploaded", results.len());
        Ok(())
    }

    /// Re-queue failed uploads with incremented attempt counter
    pub fn requeue_failed(&mut self, mut results: Vec<QueuedResult>) -> anyhow::Result<()> {
        for queued in &mut results {
            queued.increment_attempts();

            // Update in database
            if let Some(id) = queued.id {
                let key = Self::queue_key(queued.queued_at, id);

                // Read existing record and update
                if let Some(value) = self.db.get(&key)? {
                    if let Ok(mut record) = serde_json::from_slice::<StoredQueueRecord>(&value) {
                        record.attempts = queued.attempts;
                        record.last_attempt_at = queued.last_attempt_at;
                        let new_value = serde_json::to_vec(&record)?;
                        self.db.put(&key, &new_value)?;
                    }
                }
            }

            // Add back to memory cache if there's room
            if self.memory_cache.len() < self.max_memory_items {
                let size = self.estimate_size(queued);
                self.memory_cache.push_back(queued.clone());
                self.memory_bytes += size;
            }
        }

        debug!("Re-queued {} failed results", results.len());
        Ok(())
    }

    /// Load pending results from disk on startup
    pub fn load_pending(&mut self) -> anyhow::Result<usize> {
        // Count total items
        let mut count = 0usize;
        let iter = self.db.prefix_iterator(b"q");
        for item in iter {
            let (key, _) = item?;
            if !key.starts_with(b"q") {
                break;
            }
            count += 1;
        }

        if count == 0 {
            return Ok(0);
        }

        // Load up to max_memory_items into memory
        let results = self.load_from_disk(self.max_memory_items)?;
        let loaded = results.len();

        for queued in results {
            let size = self.estimate_size(&queued);
            self.memory_cache.push_back(queued);
            self.memory_bytes += size;
        }

        Ok(loaded)
    }

    /// Flush memory cache to disk (for shutdown)
    pub fn flush(&mut self) -> anyhow::Result<()> {
        // All items are already persisted on enqueue, so just clear memory
        self.memory_cache.clear();
        self.memory_bytes = 0;
        Ok(())
    }

    /// Get queue length
    pub fn len(&self) -> usize {
        self.memory_cache.len()
    }

    /// Check if queue is empty
    pub fn is_empty(&self) -> bool {
        self.memory_cache.is_empty()
    }

    /// Get queue statistics
    pub fn stats(&self) -> anyhow::Result<QueueStats> {
        // Count disk items
        let mut disk_count = 0usize;
        let mut oldest_queued_at: Option<i64> = None;

        let iter = self.db.prefix_iterator(b"q");
        for item in iter {
            let (key, value) = item?;
            if !key.starts_with(b"q") {
                break;
            }
            disk_count += 1;

            if let Ok(record) = serde_json::from_slice::<StoredQueueRecord>(&value) {
                if oldest_queued_at.is_none() || record.queued_at < oldest_queued_at.unwrap() {
                    oldest_queued_at = Some(record.queued_at);
                }
            }
        }

        Ok(QueueStats {
            memory_count: self.memory_cache.len(),
            disk_count,
            total_bytes: self.memory_bytes,
            oldest_queued_at,
        })
    }

    /// Estimate size of a queued result in bytes
    fn estimate_size(&self, queued: &QueuedResult) -> usize {
        serde_json::to_string(&queued.result)
            .map(|s| s.len())
            .unwrap_or(1000)
    }

    /// Delete old results that have exceeded max age
    pub fn cleanup_expired(&mut self, max_age_secs: i64) -> anyhow::Result<usize> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let cutoff = now - max_age_secs;

        // Remove from memory cache
        let before_len = self.memory_cache.len();
        self.memory_cache.retain(|r| r.queued_at >= cutoff);
        let memory_removed = before_len - self.memory_cache.len();

        // Remove from disk
        let mut to_delete = Vec::new();
        let iter = self.db.prefix_iterator(b"q");
        for item in iter {
            let (key, value) = item?;
            if !key.starts_with(b"q") {
                break;
            }

            if let Ok(record) = serde_json::from_slice::<StoredQueueRecord>(&value) {
                if record.queued_at < cutoff {
                    to_delete.push(key.to_vec());
                }
            }
        }

        let disk_removed = to_delete.len();
        for key in to_delete {
            self.db.delete(&key)?;
        }

        if memory_removed > 0 || disk_removed > 0 {
            debug!(
                "Cleaned up {} expired results ({} memory, {} disk)",
                memory_removed.max(disk_removed),
                memory_removed,
                disk_removed
            );
        }

        Ok(disk_removed)
    }

    /// Delete results that have exceeded max attempts
    pub fn cleanup_failed(&mut self, max_attempts: u32) -> anyhow::Result<usize> {
        // Remove from memory cache
        let before_len = self.memory_cache.len();
        self.memory_cache.retain(|r| r.attempts < max_attempts);
        let memory_removed = before_len - self.memory_cache.len();

        // Remove from disk
        let mut to_delete = Vec::new();
        let iter = self.db.prefix_iterator(b"q");
        for item in iter {
            let (key, value) = item?;
            if !key.starts_with(b"q") {
                break;
            }

            if let Ok(record) = serde_json::from_slice::<StoredQueueRecord>(&value) {
                if record.attempts >= max_attempts {
                    to_delete.push(key.to_vec());
                }
            }
        }

        let disk_removed = to_delete.len();
        for key in to_delete {
            self.db.delete(&key)?;
        }

        if memory_removed > 0 || disk_removed > 0 {
            warn!(
                "Dropped {} results that exceeded max attempts",
                memory_removed.max(disk_removed)
            );
        }

        Ok(disk_removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starla_common::{MeasurementData, MeasurementId, MeasurementType, ProbeId, Timestamp};
    use std::net::IpAddr;
    use tempfile::tempdir;

    fn make_result(msm_id: u64) -> MeasurementResult {
        MeasurementResult {
            fw: 6000,
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
    fn test_persistent_queue() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test_results.db");

        let mut queue = PersistentResultQueue::new(&db_path, 100).unwrap();
        assert!(queue.is_empty());

        // Enqueue a result
        queue.enqueue(make_result(1001)).unwrap();
        assert_eq!(queue.len(), 1);

        // Drain it
        let batch = queue.drain_batch(10);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].result.msm_id.0, 1001);

        // Mark as uploaded
        queue.mark_uploaded(&batch).unwrap();

        // Queue should be empty now
        let stats = queue.stats().unwrap();
        assert_eq!(stats.disk_count, 0);
    }

    #[test]
    fn test_persistence_across_restarts() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test_results.db");

        // Create queue and add results
        {
            let mut queue = PersistentResultQueue::new(&db_path, 100).unwrap();
            queue.enqueue(make_result(1001)).unwrap();
            queue.enqueue(make_result(1002)).unwrap();
            queue.flush().unwrap();
        }

        // Reopen - should still have results
        {
            let mut queue = PersistentResultQueue::new(&db_path, 100).unwrap();
            assert_eq!(queue.len(), 2);

            let batch = queue.drain_batch(10);
            assert_eq!(batch.len(), 2);
        }
    }

    #[test]
    fn test_requeue_failed() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test_results.db");

        let mut queue = PersistentResultQueue::new(&db_path, 100).unwrap();
        queue.enqueue(make_result(1001)).unwrap();

        // Drain and requeue as failed
        let batch = queue.drain_batch(10);
        assert_eq!(batch[0].attempts, 0);

        queue.requeue_failed(batch).unwrap();

        // Drain again - attempts should be incremented
        let batch = queue.drain_batch(10);
        assert_eq!(batch[0].attempts, 1);
    }
}
