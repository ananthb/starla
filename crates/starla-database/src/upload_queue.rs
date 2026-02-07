//! Upload queue operations
//!
//! Manages the queue of measurement results awaiting upload to the controller.

use crate::{encode_i64, encode_u64, Database, CF_UPLOAD_QUEUE};
use rocksdb::IteratorMode;
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

/// Upload queue item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadQueueItem {
    pub id: u64,
    pub measurement_id: i64,
    pub queued_at: i64,
    pub attempts: i64,
    pub last_attempt: Option<i64>,
    pub error: Option<String>,
}

/// Statistics about the upload queue
#[derive(Debug, Clone)]
pub struct QueueStats {
    pub total_items: i64,
    pub pending_items: i64,
    pub failed_items: i64,
    pub oldest_queued_at: Option<i64>,
}

impl Database {
    /// Build key for upload queue item
    fn upload_queue_key(queued_at: i64, id: u64) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + 8 + 8);
        key.push(b'q');
        key.extend_from_slice(&encode_i64(queued_at));
        key.extend_from_slice(&encode_u64(id));
        key
    }

    /// Add a measurement result to the upload queue
    #[instrument(skip(self), fields(measurement_id))]
    pub fn enqueue_result(&self, measurement_id: i64) -> Result<u64, anyhow::Error> {
        let now = chrono::Utc::now().timestamp();
        let id = self.next_id();

        debug!(measurement_id, "Enqueueing measurement result");

        let cf = self.cf(CF_UPLOAD_QUEUE);
        let key = Self::upload_queue_key(now, id);

        let item = UploadQueueItem {
            id,
            measurement_id,
            queued_at: now,
            attempts: 0,
            last_attempt: None,
            error: None,
        };

        let value = serde_json::to_vec(&item)?;
        self.db.put_cf(cf, key, value)?;

        Ok(id)
    }

    /// Get pending items from the upload queue
    #[instrument(skip(self), fields(limit))]
    pub fn dequeue_pending(&self, limit: i64) -> Result<Vec<(i64, i64)>, anyhow::Error> {
        debug!(limit, "Dequeuing pending upload items");

        let cf = self.cf(CF_UPLOAD_QUEUE);
        let mut results = Vec::new();

        let iter = self.db.iterator_cf(cf, IteratorMode::Start);

        for item in iter {
            let (key, value) = item?;

            if !key.starts_with(b"q") {
                continue;
            }

            if let Ok(record) = serde_json::from_slice::<UploadQueueItem>(&value) {
                // Only include items with less than 10 attempts
                if record.attempts < 10 {
                    results.push((record.id as i64, record.measurement_id));
                    if results.len() >= limit as usize {
                        break;
                    }
                }
            }
        }

        Ok(results)
    }

    /// Mark an upload as successful and remove from queue
    #[instrument(skip(self), fields(queue_id))]
    pub fn mark_upload_success(&self, queue_id: i64) -> Result<(), anyhow::Error> {
        debug!(queue_id, "Marking upload as successful");

        // Find and delete the item
        let cf = self.cf(CF_UPLOAD_QUEUE);
        let iter = self.db.iterator_cf(cf, IteratorMode::Start);

        for item in iter {
            let (key, value) = item?;

            if !key.starts_with(b"q") {
                continue;
            }

            if let Ok(record) = serde_json::from_slice::<UploadQueueItem>(&value) {
                if record.id == queue_id as u64 {
                    self.db.delete_cf(cf, key)?;
                    break;
                }
            }
        }

        Ok(())
    }

    /// Mark an upload as failed and increment attempt counter
    #[instrument(skip(self, error), fields(queue_id, error_len = error.len()))]
    pub fn mark_upload_failure(&self, queue_id: i64, error: String) -> Result<(), anyhow::Error> {
        let now = chrono::Utc::now().timestamp();

        debug!(queue_id, "Marking upload as failed");

        let cf = self.cf(CF_UPLOAD_QUEUE);
        let iter = self.db.iterator_cf(cf, IteratorMode::Start);

        for item in iter {
            let (key, value) = item?;

            if !key.starts_with(b"q") {
                continue;
            }

            if let Ok(mut record) = serde_json::from_slice::<UploadQueueItem>(&value) {
                if record.id == queue_id as u64 {
                    record.attempts += 1;
                    record.last_attempt = Some(now);
                    record.error = Some(error);

                    let new_value = serde_json::to_vec(&record)?;
                    self.db.put_cf(cf, key, new_value)?;
                    break;
                }
            }
        }

        Ok(())
    }

    /// Get statistics about the upload queue
    #[instrument(skip(self))]
    pub fn get_queue_stats(&self) -> Result<QueueStats, anyhow::Error> {
        debug!("Getting upload queue statistics");

        let cf = self.cf(CF_UPLOAD_QUEUE);
        let mut total_items = 0i64;
        let mut pending_items = 0i64;
        let mut failed_items = 0i64;
        let mut oldest_queued_at: Option<i64> = None;

        let iter = self.db.iterator_cf(cf, IteratorMode::Start);

        for item in iter {
            let (key, value) = item?;

            if !key.starts_with(b"q") {
                continue;
            }

            if let Ok(record) = serde_json::from_slice::<UploadQueueItem>(&value) {
                total_items += 1;

                if record.attempts < 10 {
                    pending_items += 1;
                } else {
                    failed_items += 1;
                }

                if oldest_queued_at.is_none() || record.queued_at < oldest_queued_at.unwrap() {
                    oldest_queued_at = Some(record.queued_at);
                }
            }
        }

        Ok(QueueStats {
            total_items,
            pending_items,
            failed_items,
            oldest_queued_at,
        })
    }

    /// Clean up old upload queue items
    ///
    /// Removes items that are:
    /// - Older than the specified timestamp, OR
    /// - Have failed more than 10 attempts
    #[instrument(skip(self), fields(before_timestamp))]
    pub fn cleanup_upload_queue(&self, before_timestamp: i64) -> Result<u64, anyhow::Error> {
        debug!(before_timestamp, "Cleaning up old upload queue items");

        let cf = self.cf(CF_UPLOAD_QUEUE);
        let mut deleted = 0u64;
        let mut to_delete = Vec::new();

        let iter = self.db.iterator_cf(cf, IteratorMode::Start);

        for item in iter {
            let (key, value) = item?;

            if !key.starts_with(b"q") {
                continue;
            }

            if let Ok(record) = serde_json::from_slice::<UploadQueueItem>(&value) {
                if record.queued_at < before_timestamp || record.attempts >= 10 {
                    to_delete.push(key.to_vec());
                }
            }
        }

        for key in to_delete {
            self.db.delete_cf(cf, key)?;
            deleted += 1;
        }

        debug!(deleted, "Upload queue cleanup complete");
        Ok(deleted)
    }
}
