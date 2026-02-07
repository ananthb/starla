//! Scheduler persistence operations
//!
//! Manages persistent storage of scheduled measurement tasks.

use crate::{encode_i64, Database, CF_SCHEDULED};
use rocksdb::IteratorMode;
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

/// Scheduled measurement specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledMeasurement {
    pub msm_id: i64,
    pub cron_spec: String,
    pub measurement_spec: String,
    pub enabled: bool,
    pub last_run: Option<i64>,
    pub next_run: Option<i64>,
}

/// Scheduler statistics
#[derive(Debug, Clone)]
pub struct SchedulerStats {
    pub total_tasks: i64,
    pub enabled_tasks: i64,
    pub disabled_tasks: i64,
}

impl Database {
    /// Build key for scheduled measurement
    fn scheduled_key(msm_id: i64) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + 8);
        key.push(b's');
        key.extend_from_slice(&encode_i64(msm_id));
        key
    }

    /// Save a scheduled measurement
    #[instrument(skip(self, measurement_spec), fields(msm_id, cron_spec))]
    pub fn save_scheduled_measurement(
        &self,
        msm_id: i64,
        cron_spec: String,
        measurement_spec: String,
        enabled: bool,
    ) -> Result<(), anyhow::Error> {
        debug!(msm_id, cron_spec, enabled, "Saving scheduled measurement");

        let cf = self.cf(CF_SCHEDULED);
        let key = Self::scheduled_key(msm_id);

        let record = ScheduledMeasurement {
            msm_id,
            cron_spec,
            measurement_spec,
            enabled,
            last_run: None,
            next_run: None,
        };

        let value = serde_json::to_vec(&record)?;
        self.db.put_cf(cf, key, value)?;

        Ok(())
    }

    /// Load all scheduled measurements
    #[instrument(skip(self))]
    pub fn load_scheduled_measurements(&self) -> Result<Vec<ScheduledMeasurement>, anyhow::Error> {
        debug!("Loading scheduled measurements");

        let cf = self.cf(CF_SCHEDULED);
        let mut results = Vec::new();

        let iter = self.db.iterator_cf(cf, IteratorMode::Start);

        for item in iter {
            let (key, value) = item?;

            // Scheduled keys start with 's'
            if !key.starts_with(b"s") {
                continue;
            }

            if let Ok(record) = serde_json::from_slice::<ScheduledMeasurement>(&value) {
                results.push(record);
            }
        }

        // Sort by msm_id
        results.sort_by_key(|r| r.msm_id);

        debug!(count = results.len(), "Loaded scheduled measurements");
        Ok(results)
    }

    /// Update the last run timestamp for a measurement
    #[instrument(skip(self), fields(msm_id, timestamp))]
    pub fn update_last_run(&self, msm_id: i64, timestamp: i64) -> Result<(), anyhow::Error> {
        debug!(msm_id, timestamp, "Updating last run timestamp");

        let cf = self.cf(CF_SCHEDULED);
        let key = Self::scheduled_key(msm_id);

        if let Some(value) = self.db.get_cf(cf, &key)? {
            if let Ok(mut record) = serde_json::from_slice::<ScheduledMeasurement>(&value) {
                record.last_run = Some(timestamp);
                let new_value = serde_json::to_vec(&record)?;
                self.db.put_cf(cf, key, new_value)?;
            }
        }

        Ok(())
    }

    /// Update the next run timestamp for a measurement
    #[instrument(skip(self), fields(msm_id, timestamp))]
    pub fn update_next_run(&self, msm_id: i64, timestamp: i64) -> Result<(), anyhow::Error> {
        debug!(msm_id, timestamp, "Updating next run timestamp");

        let cf = self.cf(CF_SCHEDULED);
        let key = Self::scheduled_key(msm_id);

        if let Some(value) = self.db.get_cf(cf, &key)? {
            if let Ok(mut record) = serde_json::from_slice::<ScheduledMeasurement>(&value) {
                record.next_run = Some(timestamp);
                let new_value = serde_json::to_vec(&record)?;
                self.db.put_cf(cf, key, new_value)?;
            }
        }

        Ok(())
    }

    /// Enable a scheduled measurement
    #[instrument(skip(self), fields(msm_id))]
    pub fn enable_measurement(&self, msm_id: i64) -> Result<(), anyhow::Error> {
        debug!(msm_id, "Enabling scheduled measurement");

        let cf = self.cf(CF_SCHEDULED);
        let key = Self::scheduled_key(msm_id);

        if let Some(value) = self.db.get_cf(cf, &key)? {
            if let Ok(mut record) = serde_json::from_slice::<ScheduledMeasurement>(&value) {
                record.enabled = true;
                let new_value = serde_json::to_vec(&record)?;
                self.db.put_cf(cf, key, new_value)?;
            }
        }

        Ok(())
    }

    /// Disable a scheduled measurement
    #[instrument(skip(self), fields(msm_id))]
    pub fn disable_measurement(&self, msm_id: i64) -> Result<(), anyhow::Error> {
        debug!(msm_id, "Disabling scheduled measurement");

        let cf = self.cf(CF_SCHEDULED);
        let key = Self::scheduled_key(msm_id);

        if let Some(value) = self.db.get_cf(cf, &key)? {
            if let Ok(mut record) = serde_json::from_slice::<ScheduledMeasurement>(&value) {
                record.enabled = false;
                let new_value = serde_json::to_vec(&record)?;
                self.db.put_cf(cf, key, new_value)?;
            }
        }

        Ok(())
    }

    /// Delete a scheduled measurement
    #[instrument(skip(self), fields(msm_id))]
    pub fn delete_scheduled_measurement(&self, msm_id: i64) -> Result<(), anyhow::Error> {
        debug!(msm_id, "Deleting scheduled measurement");

        let cf = self.cf(CF_SCHEDULED);
        let key = Self::scheduled_key(msm_id);
        self.db.delete_cf(cf, key)?;

        Ok(())
    }

    /// Get scheduler statistics
    #[instrument(skip(self))]
    pub fn get_scheduler_stats(&self) -> Result<SchedulerStats, anyhow::Error> {
        debug!("Getting scheduler statistics");

        let cf = self.cf(CF_SCHEDULED);
        let mut total_tasks = 0i64;
        let mut enabled_tasks = 0i64;
        let mut disabled_tasks = 0i64;

        let iter = self.db.iterator_cf(cf, IteratorMode::Start);

        for item in iter {
            let (key, value) = item?;

            if !key.starts_with(b"s") {
                continue;
            }

            if let Ok(record) = serde_json::from_slice::<ScheduledMeasurement>(&value) {
                total_tasks += 1;
                if record.enabled {
                    enabled_tasks += 1;
                } else {
                    disabled_tasks += 1;
                }
            }
        }

        Ok(SchedulerStats {
            total_tasks,
            enabled_tasks,
            disabled_tasks,
        })
    }
}
