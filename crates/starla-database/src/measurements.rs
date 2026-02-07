//! Measurement storage

use crate::{encode_i64, encode_u64, Database, CF_MEASUREMENTS};
use serde::{Deserialize, Serialize};
use starla_common::MeasurementResult;

/// Stored measurement record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMeasurement {
    pub id: u64,
    pub msm_id: i64,
    #[serde(rename = "type")]
    pub msm_type: String,
    pub target: String,
    pub started_at: i64,
    pub completed_at: i64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Database {
    /// Store a measurement result
    ///
    /// Key format: "m:{timestamp_be}:{id_be}"
    /// Also creates secondary indexes for msm_id, type, and status.
    pub fn store_measurement(&self, result: &MeasurementResult) -> Result<u64, anyhow::Error> {
        let result_json = serde_json::to_string(result)?;
        let now = chrono::Utc::now().timestamp();
        let id = self.next_id();

        let msm_id = result.msm_id.0 as i64;
        let msm_type = result.measurement_type.to_string();
        let target = result.dst_addr.to_string();
        let started_at = result.timestamp.0;

        let record = StoredMeasurement {
            id,
            msm_id,
            msm_type: msm_type.clone(),
            target,
            started_at,
            completed_at: now,
            status: "completed".to_string(),
            result_json: Some(result_json),
            error: None,
        };

        self.store_measurement_record(&record)?;

        Ok(id)
    }

    /// Mark a measurement as failed
    pub fn store_failed_measurement(
        &self,
        msm_id: u64,
        msm_type: String,
        target: String,
        error: String,
    ) -> Result<u64, anyhow::Error> {
        let now = chrono::Utc::now().timestamp();
        let id = self.next_id();

        let record = StoredMeasurement {
            id,
            msm_id: msm_id as i64,
            msm_type,
            target,
            started_at: now,
            completed_at: now,
            status: "failed".to_string(),
            result_json: None,
            error: Some(error),
        };

        self.store_measurement_record(&record)?;

        Ok(id)
    }

    /// Store a measurement record with all indexes
    fn store_measurement_record(&self, record: &StoredMeasurement) -> Result<(), anyhow::Error> {
        let cf = self.cf(CF_MEASUREMENTS);
        let value = serde_json::to_vec(record)?;

        // Primary key: "m:{timestamp_be}:{id_be}"
        let mut primary_key = Vec::with_capacity(1 + 8 + 8);
        primary_key.push(b'm');
        primary_key.extend_from_slice(&encode_i64(record.started_at));
        primary_key.extend_from_slice(&encode_u64(record.id));

        self.db.put_cf(cf, &primary_key, &value)?;

        // Secondary index for msm_id: "idx:msm:{msm_id_be}:{timestamp_be}:{id_be}" ->
        // primary_key
        let mut msm_idx = Vec::with_capacity(8 + 8 + 8 + 8);
        msm_idx.extend_from_slice(b"idx:msm:");
        msm_idx.extend_from_slice(&encode_i64(record.msm_id));
        msm_idx.extend_from_slice(&encode_i64(record.started_at));
        msm_idx.extend_from_slice(&encode_u64(record.id));
        self.db.put_cf(cf, &msm_idx, &primary_key)?;

        // Secondary index for type: "idx:type:{type}:{timestamp_be}:{id_be}" ->
        // primary_key
        let mut type_idx = Vec::with_capacity(9 + record.msm_type.len() + 1 + 8 + 8);
        type_idx.extend_from_slice(b"idx:type:");
        type_idx.extend_from_slice(record.msm_type.as_bytes());
        type_idx.push(b':');
        type_idx.extend_from_slice(&encode_i64(record.started_at));
        type_idx.extend_from_slice(&encode_u64(record.id));
        self.db.put_cf(cf, &type_idx, &primary_key)?;

        // Secondary index for status: "idx:status:{status}:{timestamp_be}:{id_be}" ->
        // primary_key
        let mut status_idx = Vec::with_capacity(11 + record.status.len() + 1 + 8 + 8);
        status_idx.extend_from_slice(b"idx:status:");
        status_idx.extend_from_slice(record.status.as_bytes());
        status_idx.push(b':');
        status_idx.extend_from_slice(&encode_i64(record.started_at));
        status_idx.extend_from_slice(&encode_u64(record.id));
        self.db.put_cf(cf, &status_idx, &primary_key)?;

        Ok(())
    }

    /// Delete a measurement and all its indexes
    pub(crate) fn delete_measurement(
        &self,
        record: &StoredMeasurement,
    ) -> Result<(), anyhow::Error> {
        let cf = self.cf(CF_MEASUREMENTS);

        // Delete primary key
        let mut primary_key = Vec::with_capacity(1 + 8 + 8);
        primary_key.push(b'm');
        primary_key.extend_from_slice(&encode_i64(record.started_at));
        primary_key.extend_from_slice(&encode_u64(record.id));
        self.db.delete_cf(cf, &primary_key)?;

        // Delete msm_id index
        let mut msm_idx = Vec::with_capacity(8 + 8 + 8 + 8);
        msm_idx.extend_from_slice(b"idx:msm:");
        msm_idx.extend_from_slice(&encode_i64(record.msm_id));
        msm_idx.extend_from_slice(&encode_i64(record.started_at));
        msm_idx.extend_from_slice(&encode_u64(record.id));
        self.db.delete_cf(cf, &msm_idx)?;

        // Delete type index
        let mut type_idx = Vec::with_capacity(9 + record.msm_type.len() + 1 + 8 + 8);
        type_idx.extend_from_slice(b"idx:type:");
        type_idx.extend_from_slice(record.msm_type.as_bytes());
        type_idx.push(b':');
        type_idx.extend_from_slice(&encode_i64(record.started_at));
        type_idx.extend_from_slice(&encode_u64(record.id));
        self.db.delete_cf(cf, &type_idx)?;

        // Delete status index
        let mut status_idx = Vec::with_capacity(11 + record.status.len() + 1 + 8 + 8);
        status_idx.extend_from_slice(b"idx:status:");
        status_idx.extend_from_slice(record.status.as_bytes());
        status_idx.push(b':');
        status_idx.extend_from_slice(&encode_i64(record.started_at));
        status_idx.extend_from_slice(&encode_u64(record.id));
        self.db.delete_cf(cf, &status_idx)?;

        Ok(())
    }
}
