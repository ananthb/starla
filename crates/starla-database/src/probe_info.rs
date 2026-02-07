//! Probe metadata operations
//!
//! Key-value store for probe metadata (probe ID, firmware version, registration
//! time, etc.)

use crate::{Database, CF_PROBE_INFO};
use rocksdb::IteratorMode;
use tracing::{debug, instrument};

impl Database {
    /// Get probe ID
    #[instrument(skip(self))]
    pub fn get_probe_id(&self) -> Result<u32, anyhow::Error> {
        debug!("Getting probe ID");

        let cf = self.cf(CF_PROBE_INFO);

        if let Some(value) = self.db.get_cf(cf, "probe_id")? {
            let value_str = String::from_utf8_lossy(&value);
            let probe_id = value_str.parse::<u32>().unwrap_or(0);
            Ok(probe_id)
        } else {
            Ok(0)
        }
    }

    /// Set probe ID
    #[instrument(skip(self), fields(probe_id))]
    pub fn set_probe_id(&self, probe_id: u32) -> Result<(), anyhow::Error> {
        debug!(probe_id, "Setting probe ID");

        let cf = self.cf(CF_PROBE_INFO);
        self.db.put_cf(cf, "probe_id", probe_id.to_string())?;

        Ok(())
    }

    /// Get firmware version
    #[instrument(skip(self))]
    pub fn get_firmware_version(&self) -> Result<u32, anyhow::Error> {
        debug!("Getting firmware version");

        let cf = self.cf(CF_PROBE_INFO);

        if let Some(value) = self.db.get_cf(cf, "firmware_version")? {
            let value_str = String::from_utf8_lossy(&value);
            let firmware_version = value_str.parse::<u32>().unwrap_or(6000);
            Ok(firmware_version)
        } else {
            Ok(6000)
        }
    }

    /// Get registration timestamp
    #[instrument(skip(self))]
    pub fn get_registered_at(&self) -> Result<i64, anyhow::Error> {
        debug!("Getting registration timestamp");

        let cf = self.cf(CF_PROBE_INFO);

        if let Some(value) = self.db.get_cf(cf, "registered_at")? {
            let value_str = String::from_utf8_lossy(&value);
            let registered_at = value_str.parse::<i64>().unwrap_or(0);
            Ok(registered_at)
        } else {
            Ok(0)
        }
    }

    /// Set registration timestamp
    #[instrument(skip(self), fields(timestamp))]
    pub fn set_registered_at(&self, timestamp: i64) -> Result<(), anyhow::Error> {
        debug!(timestamp, "Setting registration timestamp");

        let cf = self.cf(CF_PROBE_INFO);
        self.db.put_cf(cf, "registered_at", timestamp.to_string())?;

        Ok(())
    }

    /// Get arbitrary metadata by key
    #[instrument(skip(self), fields(key))]
    pub fn get_metadata(&self, key: &str) -> Result<Option<String>, anyhow::Error> {
        debug!(key, "Getting metadata");

        let cf = self.cf(CF_PROBE_INFO);

        if let Some(value) = self.db.get_cf(cf, key)? {
            let value_str = String::from_utf8_lossy(&value).to_string();
            Ok(Some(value_str))
        } else {
            Ok(None)
        }
    }

    /// Set arbitrary metadata by key
    #[instrument(skip(self, value), fields(key, value_len = value.len()))]
    pub fn set_metadata(&self, key: &str, value: &str) -> Result<(), anyhow::Error> {
        debug!(key, "Setting metadata");

        let cf = self.cf(CF_PROBE_INFO);
        self.db.put_cf(cf, key, value)?;

        Ok(())
    }

    /// Delete metadata by key
    #[instrument(skip(self), fields(key))]
    pub fn delete_metadata(&self, key: &str) -> Result<(), anyhow::Error> {
        debug!(key, "Deleting metadata");

        let cf = self.cf(CF_PROBE_INFO);
        self.db.delete_cf(cf, key)?;

        Ok(())
    }

    /// Get all metadata as key-value pairs
    #[instrument(skip(self))]
    pub fn get_all_metadata(&self) -> Result<Vec<(String, String)>, anyhow::Error> {
        debug!("Getting all metadata");

        let cf = self.cf(CF_PROBE_INFO);
        let mut results = Vec::new();

        let iter = self.db.iterator_cf(cf, IteratorMode::Start);

        for item in iter {
            let (key, value) = item?;
            let key_str = String::from_utf8_lossy(&key).to_string();
            let value_str = String::from_utf8_lossy(&value).to_string();
            results.push((key_str, value_str));
        }

        // Sort by key for consistent ordering
        results.sort_by(|a, b| a.0.cmp(&b.0));

        Ok(results)
    }
}
