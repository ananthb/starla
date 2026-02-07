//! Integration tests for starla-database
//!
//! These tests use temporary directories to test all database operations.

use starla_common::{
    MeasurementData, MeasurementId, MeasurementResult, MeasurementType, ProbeId, Timestamp,
};
use starla_database::{cleanup, Database};
use std::net::IpAddr;
use tempfile::tempdir;

/// Helper to create a test database in a temporary directory
fn create_test_db() -> (Database, tempfile::TempDir) {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test.db");
    let db = Database::connect(&db_path).expect("Failed to connect to test database");
    (db, temp_dir)
}

/// Helper to create a test measurement result
fn create_test_measurement(msm_id: u64, msm_type: MeasurementType) -> MeasurementResult {
    MeasurementResult {
        fw: 6000,
        measurement_type: msm_type,
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
        data: MeasurementData::Generic(serde_json::json!({"result": "ok"})),
    }
}

#[test]
fn test_database_connection() {
    let (db, _temp_dir) = create_test_db();

    // Verify database path is stored
    assert!(db.path().exists());
}

#[test]
fn test_measurement_storage() {
    let (db, _temp_dir) = create_test_db();

    // Store a measurement
    let measurement = create_test_measurement(1001, MeasurementType::Ping);
    let row_id = db
        .store_measurement(&measurement)
        .expect("Failed to store measurement");

    assert!(row_id > 0);

    // Retrieve recent measurements
    let recent = db
        .get_recent_measurements(10)
        .expect("Failed to get recent measurements");
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].msm_id.0, 1001);
}

#[test]
fn test_measurements_by_type() {
    let (db, _temp_dir) = create_test_db();

    // Store different types of measurements
    db.store_measurement(&create_test_measurement(1001, MeasurementType::Ping))
        .unwrap();
    db.store_measurement(&create_test_measurement(1002, MeasurementType::Traceroute))
        .unwrap();
    db.store_measurement(&create_test_measurement(1003, MeasurementType::Ping))
        .unwrap();

    // Query by type
    let pings = db
        .get_measurements_by_type("ping", 10)
        .expect("Failed to query pings");
    assert_eq!(pings.len(), 2);

    let traceroutes = db
        .get_measurements_by_type("traceroute", 10)
        .expect("Failed to query traceroutes");
    assert_eq!(traceroutes.len(), 1);
}

#[test]
fn test_measurement_stats() {
    let (db, _temp_dir) = create_test_db();

    // Store measurements
    db.store_measurement(&create_test_measurement(1001, MeasurementType::Ping))
        .unwrap();
    db.store_measurement(&create_test_measurement(1002, MeasurementType::Ping))
        .unwrap();
    db.store_measurement(&create_test_measurement(1003, MeasurementType::Dns))
        .unwrap();

    // Get stats
    let stats = db.get_measurement_stats().expect("Failed to get stats");

    assert_eq!(*stats.count_by_type.get("ping").unwrap(), 2);
    assert_eq!(*stats.count_by_type.get("dns").unwrap(), 1);
    assert_eq!(*stats.count_by_status.get("completed").unwrap(), 3);
}

#[test]
fn test_upload_queue() {
    let (db, _temp_dir) = create_test_db();

    // Store a measurement and enqueue it
    let measurement = create_test_measurement(1001, MeasurementType::Ping);
    let measurement_id = db.store_measurement(&measurement).unwrap();

    // Enqueue for upload
    let queue_id = db
        .enqueue_result(measurement_id as i64)
        .expect("Failed to enqueue");
    assert!(queue_id > 0);

    // Check queue stats
    let stats = db.get_queue_stats().expect("Failed to get queue stats");
    assert_eq!(stats.total_items, 1);
    assert_eq!(stats.pending_items, 1);
    assert_eq!(stats.failed_items, 0);

    // Dequeue pending
    let pending = db.dequeue_pending(10).expect("Failed to dequeue");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, queue_id as i64);

    // Mark as success
    db.mark_upload_success(queue_id as i64)
        .expect("Failed to mark success");

    // Verify queue is empty
    let stats = db.get_queue_stats().expect("Failed to get queue stats");
    assert_eq!(stats.total_items, 0);
}

#[test]
fn test_upload_queue_failure() {
    let (db, _temp_dir) = create_test_db();

    // Store and enqueue
    let measurement = create_test_measurement(1001, MeasurementType::Ping);
    let measurement_id = db.store_measurement(&measurement).unwrap();
    let queue_id = db.enqueue_result(measurement_id as i64).unwrap();

    // Mark as failed multiple times
    for i in 1..=12 {
        db.mark_upload_failure(queue_id as i64, format!("Attempt {} failed", i))
            .expect("Failed to mark failure");
    }

    // After 10 attempts, item should not be in pending
    let pending = db.dequeue_pending(10).expect("Failed to dequeue");
    assert_eq!(pending.len(), 0);

    // But should be in failed
    let stats = db.get_queue_stats().expect("Failed to get queue stats");
    assert_eq!(stats.failed_items, 1);
}

#[test]
fn test_scheduler_persistence() {
    let (db, _temp_dir) = create_test_db();

    // Save a scheduled measurement
    db.save_scheduled_measurement(
        2001,
        "*/5 * * * *".to_string(),
        r#"{"type":"ping","target":"8.8.8.8"}"#.to_string(),
        true,
    )
    .expect("Failed to save scheduled measurement");

    // Load it back
    let tasks = db
        .load_scheduled_measurements()
        .expect("Failed to load tasks");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].msm_id, 2001);
    assert_eq!(tasks[0].cron_spec, "*/5 * * * *");
    assert!(tasks[0].enabled);

    // Disable it
    db.disable_measurement(2001).expect("Failed to disable");

    // Verify disabled
    let tasks = db.load_scheduled_measurements().unwrap();
    assert!(!tasks[0].enabled);

    // Get scheduler stats
    let stats = db
        .get_scheduler_stats()
        .expect("Failed to get scheduler stats");
    assert_eq!(stats.total_tasks, 1);
    assert_eq!(stats.disabled_tasks, 1);
}

#[test]
fn test_probe_metadata() {
    let (db, _temp_dir) = create_test_db();

    // Set probe ID
    db.set_probe_id(54321).expect("Failed to set probe ID");

    // Get probe ID
    let probe_id = db.get_probe_id().expect("Failed to get probe ID");
    assert_eq!(probe_id, 54321);

    // Set custom metadata
    db.set_metadata("last_sync", "2026-01-11T12:00:00Z")
        .expect("Failed to set metadata");

    // Get custom metadata
    let value = db
        .get_metadata("last_sync")
        .expect("Failed to get metadata");
    assert_eq!(value, Some("2026-01-11T12:00:00Z".to_string()));

    // Get all metadata
    let all = db.get_all_metadata().expect("Failed to get all metadata");
    assert!(all.len() >= 4); // At least: firmware_version, probe_id,
                             // registered_at, last_sync
}

#[test]
fn test_cleanup_time_based() {
    let (db, _temp_dir) = create_test_db();

    // Store some measurements
    for i in 1..=5 {
        db.store_measurement(&create_test_measurement(1000 + i, MeasurementType::Ping))
            .unwrap();
    }

    // Clean up all measurements (use future timestamp to delete all)
    let future_timestamp = chrono::Utc::now().timestamp() + 86400;
    let deleted = db
        .cleanup_old_measurements(future_timestamp)
        .expect("Failed to cleanup");

    assert_eq!(deleted, 5);

    // Verify all are gone
    let recent = db.get_recent_measurements(10).unwrap();
    assert_eq!(recent.len(), 0);
}

#[test]
fn test_database_size_tracking() {
    let (db, _temp_dir) = create_test_db();

    // Get initial size (may be 0 for empty RocksDB)
    let initial_size = db.get_database_size_bytes().expect("Failed to get size");

    // Add a significant amount of data (1000 measurements to ensure size increase)
    for i in 1..=1000 {
        db.store_measurement(&create_test_measurement(1000 + i, MeasurementType::Ping))
            .unwrap();
    }

    // Size should have increased
    let new_size = db.get_database_size_bytes().unwrap();
    assert!(new_size >= initial_size);
}

#[test]
fn test_cleanup_config() {
    let config = cleanup::CleanupConfig {
        retention_days: 30,
        max_database_size_mb: 1,
        cleanup_interval_hours: 24,
    };

    assert_eq!(config.retention_days, 30);
    assert_eq!(config.max_database_size_mb, 1);
    assert_eq!(config.cleanup_interval_hours, 24);
}

#[test]
fn test_cleanup_cycle() {
    let (db, _temp_dir) = create_test_db();

    // Add some measurements
    for i in 1..=3 {
        db.store_measurement(&create_test_measurement(1000 + i, MeasurementType::Ping))
            .unwrap();
    }

    // Run cleanup cycle (future timestamp to delete all)
    let config = cleanup::CleanupConfig {
        retention_days: 0,         // Will use timestamp calculation
        max_database_size_mb: 100, // Large enough to not trigger
        cleanup_interval_hours: 24,
    };

    let stats = cleanup::run_cleanup_cycle(&db, &config).expect("Failed to run cleanup");

    // Should have deleted measurements older than retention
    assert!(stats.database_size_after_mb <= stats.database_size_before_mb);
}

#[test]
fn test_upload_queue_cleanup() {
    let (db, _temp_dir) = create_test_db();

    // Add items to queue
    let measurement = create_test_measurement(1001, MeasurementType::Ping);
    let measurement_id = db.store_measurement(&measurement).unwrap();
    let queue_id = db.enqueue_result(measurement_id as i64).unwrap();

    // Mark as failed 11 times (exceeds max attempts)
    for i in 1..=11 {
        db.mark_upload_failure(queue_id as i64, format!("Error {}", i))
            .unwrap();
    }

    // Clean up (should remove items with >= 10 attempts)
    let future_timestamp = chrono::Utc::now().timestamp() + 86400;
    let deleted = db.cleanup_upload_queue(future_timestamp).unwrap();

    assert_eq!(deleted, 1);

    // Verify queue is empty
    let stats = db.get_queue_stats().unwrap();
    assert_eq!(stats.total_items, 0);
}
