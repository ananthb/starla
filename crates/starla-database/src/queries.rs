//! Database queries

use crate::measurements::StoredMeasurement;
use crate::{encode_i64, Database, CF_MEASUREMENTS};
use rocksdb::IteratorMode;
use starla_common::MeasurementResult;
use std::collections::HashMap;
use tracing::{debug, instrument};

impl Database {
    /// Get recent measurements (reverse chronological order)
    pub fn get_recent_measurements(
        &self,
        limit: i64,
    ) -> Result<Vec<MeasurementResult>, anyhow::Error> {
        let cf = self.cf(CF_MEASUREMENTS);
        let mut results = Vec::new();

        // Iterate over primary keys in reverse order
        // Primary keys start with 'm' followed by timestamp
        let prefix = b"m";
        let iter = self.db.iterator_cf(cf, IteratorMode::End);

        for item in iter {
            let (key, value) = item?;

            // Check if this is a primary key (starts with 'm')
            if !key.starts_with(prefix) {
                continue;
            }

            // Parse the stored measurement
            if let Ok(record) = serde_json::from_slice::<StoredMeasurement>(&value) {
                // Only include completed measurements
                if record.status != "completed" {
                    continue;
                }

                // Parse the result_json if present
                if let Some(ref json) = record.result_json {
                    if let Ok(result) = serde_json::from_str::<MeasurementResult>(json) {
                        results.push(result);
                        if results.len() >= limit as usize {
                            break;
                        }
                    }
                }
            }
        }

        Ok(results)
    }

    /// Get pending measurements count
    #[instrument(skip(self))]
    pub fn get_pending_count(&self) -> Result<i64, anyhow::Error> {
        let cf = self.cf(CF_MEASUREMENTS);
        let prefix = b"idx:status:pending:";

        let iter = self.db.prefix_iterator_cf(cf, prefix);
        let mut count = 0i64;

        for item in iter {
            let (key, _) = item?;
            if !key.starts_with(prefix) {
                break;
            }
            count += 1;
        }

        Ok(count)
    }

    /// Get measurements by type
    #[instrument(skip(self), fields(msm_type, limit))]
    pub fn get_measurements_by_type(
        &self,
        msm_type: &str,
        limit: i64,
    ) -> Result<Vec<MeasurementResult>, anyhow::Error> {
        debug!(msm_type, limit, "Getting measurements by type");

        let cf = self.cf(CF_MEASUREMENTS);
        let mut results = Vec::new();

        // Build the type index prefix
        let mut prefix = Vec::with_capacity(9 + msm_type.len() + 1);
        prefix.extend_from_slice(b"idx:type:");
        prefix.extend_from_slice(msm_type.as_bytes());
        prefix.push(b':');

        // Iterate in reverse to get most recent first
        let iter = self.db.prefix_iterator_cf(cf, &prefix);

        // Collect all matching keys, then reverse
        let mut matching_keys: Vec<_> = Vec::new();
        for item in iter {
            let (key, value) = item?;
            if !key.starts_with(&prefix) {
                break;
            }
            matching_keys.push((key.to_vec(), value.to_vec()));
        }

        // Process in reverse order (most recent first)
        for (_idx_key, primary_key) in matching_keys.into_iter().rev() {
            if results.len() >= limit as usize {
                break;
            }

            // Fetch the actual record using the primary key
            if let Some(value) = self.db.get_cf(cf, &primary_key)? {
                if let Ok(record) = serde_json::from_slice::<StoredMeasurement>(&value) {
                    if record.status == "completed" {
                        if let Some(ref json) = record.result_json {
                            if let Ok(result) = serde_json::from_str::<MeasurementResult>(json) {
                                results.push(result);
                            }
                        }
                    }
                }
            }
        }

        Ok(results)
    }

    /// Get measurements by status
    #[instrument(skip(self), fields(status, limit))]
    pub fn get_measurements_by_status(
        &self,
        status: &str,
        limit: i64,
    ) -> Result<Vec<MeasurementResult>, anyhow::Error> {
        debug!(status, limit, "Getting measurements by status");

        let cf = self.cf(CF_MEASUREMENTS);
        let mut results = Vec::new();

        // Build the status index prefix
        let mut prefix = Vec::with_capacity(11 + status.len() + 1);
        prefix.extend_from_slice(b"idx:status:");
        prefix.extend_from_slice(status.as_bytes());
        prefix.push(b':');

        // Iterate in reverse to get most recent first
        let iter = self.db.prefix_iterator_cf(cf, &prefix);

        // Collect all matching keys, then reverse
        let mut matching_keys: Vec<_> = Vec::new();
        for item in iter {
            let (key, value) = item?;
            if !key.starts_with(&prefix) {
                break;
            }
            matching_keys.push((key.to_vec(), value.to_vec()));
        }

        // Process in reverse order (most recent first)
        for (_idx_key, primary_key) in matching_keys.into_iter().rev() {
            if results.len() >= limit as usize {
                break;
            }

            // Fetch the actual record using the primary key
            if let Some(value) = self.db.get_cf(cf, &primary_key)? {
                if let Ok(record) = serde_json::from_slice::<StoredMeasurement>(&value) {
                    if let Some(ref json) = record.result_json {
                        if let Ok(result) = serde_json::from_str::<MeasurementResult>(json) {
                            results.push(result);
                        }
                    }
                }
            }
        }

        Ok(results)
    }

    /// Get measurements in a time range
    #[instrument(skip(self), fields(start, end))]
    pub fn get_measurements_in_timerange(
        &self,
        start: i64,
        end: i64,
    ) -> Result<Vec<MeasurementResult>, anyhow::Error> {
        debug!(start, end, "Getting measurements in time range");

        let cf = self.cf(CF_MEASUREMENTS);
        let mut results = Vec::new();

        // Build start key
        let mut start_key = Vec::with_capacity(1 + 8);
        start_key.push(b'm');
        start_key.extend_from_slice(&encode_i64(start));

        // Build end key
        let mut end_key = Vec::with_capacity(1 + 8);
        end_key.push(b'm');
        end_key.extend_from_slice(&encode_i64(end + 1)); // +1 to include end timestamp

        let iter = self.db.iterator_cf(
            cf,
            IteratorMode::From(&start_key, rocksdb::Direction::Forward),
        );

        for item in iter {
            let (key, value) = item?;

            // Stop if we've passed the end key
            if key.as_ref() >= end_key.as_slice() {
                break;
            }

            // Check if this is a primary key
            if !key.starts_with(b"m") {
                continue;
            }

            if let Ok(record) = serde_json::from_slice::<StoredMeasurement>(&value) {
                if record.status == "completed" {
                    if let Some(ref json) = record.result_json {
                        if let Ok(result) = serde_json::from_str::<MeasurementResult>(json) {
                            results.push(result);
                        }
                    }
                }
            }
        }

        // Reverse to get descending order
        results.reverse();

        Ok(results)
    }

    /// Get aggregate measurement statistics
    #[instrument(skip(self))]
    pub fn get_measurement_stats(&self) -> Result<MeasurementStats, anyhow::Error> {
        debug!("Getting measurement statistics");

        let cf = self.cf(CF_MEASUREMENTS);
        let mut count_by_type: HashMap<String, i64> = HashMap::new();
        let mut count_by_status: HashMap<String, i64> = HashMap::new();
        let mut duration_sums: HashMap<String, (f64, i64)> = HashMap::new(); // (sum, count)

        // Scan all primary keys
        let iter = self.db.prefix_iterator_cf(cf, b"m");

        for item in iter {
            let (key, value) = item?;

            if !key.starts_with(b"m") {
                break;
            }

            if let Ok(record) = serde_json::from_slice::<StoredMeasurement>(&value) {
                // Count by type
                *count_by_type.entry(record.msm_type.clone()).or_insert(0) += 1;

                // Count by status
                *count_by_status.entry(record.status.clone()).or_insert(0) += 1;

                // Calculate duration for completed measurements
                if record.status == "completed" {
                    let duration = (record.completed_at - record.started_at) as f64;
                    let entry = duration_sums
                        .entry(record.msm_type.clone())
                        .or_insert((0.0, 0));
                    entry.0 += duration;
                    entry.1 += 1;
                }
            }
        }

        // Calculate average durations
        let avg_duration_by_type: HashMap<String, f64> = duration_sums
            .into_iter()
            .filter(|(_, (_, count))| *count > 0)
            .map(|(typ, (sum, count))| (typ, sum / count as f64))
            .collect();

        Ok(MeasurementStats {
            count_by_type,
            count_by_status,
            avg_duration_by_type,
        })
    }

    /// Clean up old measurements (time-based deletion)
    #[instrument(skip(self), fields(before_timestamp))]
    pub fn cleanup_old_measurements(&self, before_timestamp: i64) -> Result<u64, anyhow::Error> {
        debug!(
            before_timestamp,
            "Cleaning up measurements older than timestamp"
        );

        let cf = self.cf(CF_MEASUREMENTS);
        let mut deleted = 0u64;
        let mut to_delete = Vec::new();

        // Build end key for the range
        let mut end_key = Vec::with_capacity(1 + 8);
        end_key.push(b'm');
        end_key.extend_from_slice(&encode_i64(before_timestamp));

        // Collect records to delete
        let iter = self.db.prefix_iterator_cf(cf, b"m");

        for item in iter {
            let (key, value) = item?;

            if !key.starts_with(b"m") {
                break;
            }

            // Check if before the cutoff timestamp
            if key.as_ref() >= end_key.as_slice() {
                break;
            }

            if let Ok(record) = serde_json::from_slice::<StoredMeasurement>(&value) {
                to_delete.push(record);
            }
        }

        // Delete records
        for record in to_delete {
            self.delete_measurement(&record)?;
            deleted += 1;
        }

        debug!(deleted, "Time-based cleanup complete");
        Ok(deleted)
    }

    /// Clean up measurements by size (delete oldest until target size reached)
    #[instrument(skip(self), fields(target_size_mb))]
    pub fn cleanup_by_size(&self, target_size_mb: u64) -> Result<u64, anyhow::Error> {
        debug!(target_size_mb, "Cleaning up database by size");

        let target_bytes = target_size_mb * 1024 * 1024;
        let mut total_deleted = 0u64;

        loop {
            // Check current size
            let current_size = self.get_database_size_bytes()?;
            if current_size <= target_bytes {
                debug!(total_deleted, "Size-based cleanup complete");
                break;
            }

            // Delete oldest batch (100 at a time)
            let cf = self.cf(CF_MEASUREMENTS);
            let mut to_delete = Vec::new();

            let iter = self.db.prefix_iterator_cf(cf, b"m");

            for item in iter {
                let (key, value) = item?;

                if !key.starts_with(b"m") {
                    break;
                }

                if let Ok(record) = serde_json::from_slice::<StoredMeasurement>(&value) {
                    to_delete.push(record);
                    if to_delete.len() >= 100 {
                        break;
                    }
                }
            }

            if to_delete.is_empty() {
                debug!(
                    total_deleted,
                    "No more measurements to delete, size still above target"
                );
                break;
            }

            let batch_size = to_delete.len() as u64;
            for record in to_delete {
                self.delete_measurement(&record)?;
            }

            total_deleted += batch_size;
            debug!(deleted = batch_size, total_deleted, "Deleted batch");
        }

        Ok(total_deleted)
    }
}

/// Measurement statistics
#[derive(Debug, Clone)]
pub struct MeasurementStats {
    pub count_by_type: HashMap<String, i64>,
    pub count_by_status: HashMap<String, i64>,
    pub avg_duration_by_type: HashMap<String, f64>,
}
