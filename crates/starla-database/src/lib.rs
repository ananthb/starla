//! Database integration using RocksDB

use rocksdb::{ColumnFamily, ColumnFamilyDescriptor, Options, DB};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::debug;

pub mod cleanup;
pub mod measurements;
pub mod probe_info;
pub mod queries;
pub mod scheduler;
pub mod upload_queue;

// Re-export key types
pub use cleanup::{CleanupConfig, CleanupStats};
pub use queries::MeasurementStats;
pub use scheduler::{ScheduledMeasurement, SchedulerStats};
pub use upload_queue::QueueStats;

/// Column family names
pub const CF_MEASUREMENTS: &str = "measurements";
pub const CF_PROBE_INFO: &str = "probe_info";
pub const CF_SCHEDULED: &str = "scheduled";
pub const CF_UPLOAD_QUEUE: &str = "upload_queue";

/// Monotonic ID generator for unique record IDs
/// Uses a simple atomic counter for guaranteed uniqueness
pub struct IdGenerator {
    counter: AtomicU64,
}

impl IdGenerator {
    pub fn new() -> Self {
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

    pub fn next_id(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::SeqCst)
    }
}

impl Default for IdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Encode a u64 as big-endian bytes for proper lexicographic ordering
#[inline]
pub fn encode_u64(v: u64) -> [u8; 8] {
    v.to_be_bytes()
}

/// Decode a u64 from big-endian bytes
#[inline]
pub fn decode_u64(bytes: &[u8]) -> u64 {
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(arr)
}

/// Encode an i64 as big-endian bytes for proper lexicographic ordering
/// Adds bias to handle negative numbers correctly
#[inline]
pub fn encode_i64(v: i64) -> [u8; 8] {
    // Add i64::MIN to shift range from [-2^63, 2^63-1] to [0, 2^64-1]
    ((v as u64).wrapping_add(0x8000000000000000u64)).to_be_bytes()
}

/// Decode an i64 from big-endian bytes
#[inline]
pub fn decode_i64(bytes: &[u8]) -> i64 {
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&bytes[..8]);
    let biased = u64::from_be_bytes(arr);
    biased.wrapping_sub(0x8000000000000000u64) as i64
}

#[derive(Clone)]
pub struct Database {
    db: Arc<DB>,
    path: std::path::PathBuf,
    id_gen: Arc<IdGenerator>,
}

impl Database {
    /// Connect to the database at the specified path
    pub fn connect(path: &Path) -> Result<Self, anyhow::Error> {
        debug!("Connecting to database at {}", path.display());

        // Ensure directory exists
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }

        // Configure RocksDB options
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.set_compression_type(rocksdb::DBCompressionType::Lz4);
        opts.set_max_background_jobs(2);
        opts.set_write_buffer_size(16 * 1024 * 1024); // 16MB write buffer
        opts.set_max_write_buffer_number(2);
        opts.set_target_file_size_base(64 * 1024 * 1024); // 64MB SST files
        opts.set_level_zero_file_num_compaction_trigger(4);

        // Define column families
        let cf_descriptors = vec![
            ColumnFamilyDescriptor::new(CF_MEASUREMENTS, Options::default()),
            ColumnFamilyDescriptor::new(CF_PROBE_INFO, Options::default()),
            ColumnFamilyDescriptor::new(CF_SCHEDULED, Options::default()),
            ColumnFamilyDescriptor::new(CF_UPLOAD_QUEUE, Options::default()),
        ];

        let db = DB::open_cf_descriptors(&opts, path, cf_descriptors)?;

        debug!("Database opened successfully");

        let db = Self {
            db: Arc::new(db),
            path: path.to_path_buf(),
            id_gen: Arc::new(IdGenerator::new()),
        };

        // Initialize default probe info if not present
        db.init_probe_info()?;

        Ok(db)
    }

    /// Get a column family handle
    pub fn cf(&self, name: &str) -> &ColumnFamily {
        self.db.cf_handle(name).expect("Column family should exist")
    }

    /// Get reference to the underlying RocksDB instance
    pub fn inner(&self) -> &DB {
        &self.db
    }

    /// Generate a unique ID
    pub fn next_id(&self) -> u64 {
        self.id_gen.next_id()
    }

    /// Close the database
    pub fn close(&self) {
        // RocksDB closes automatically when dropped
        // This is here for API compatibility
        debug!("Closing database");
    }

    /// Get the database file path
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Get the total database size in bytes
    pub fn get_database_size_bytes(&self) -> Result<u64, anyhow::Error> {
        // Get SST files size for all column families
        let mut total_size = 0u64;

        // Get live data size
        if let Some(size_str) = self.db.property_value("rocksdb.total-sst-files-size")? {
            if let Ok(size) = size_str.parse::<u64>() {
                total_size += size;
            }
        }

        // Add memtable size
        if let Some(size_str) = self.db.property_value("rocksdb.cur-size-all-mem-tables")? {
            if let Ok(size) = size_str.parse::<u64>() {
                total_size += size;
            }
        }

        Ok(total_size)
    }

    /// Initialize default probe info values
    fn init_probe_info(&self) -> Result<(), anyhow::Error> {
        let cf = self.cf(CF_PROBE_INFO);

        // Only set if not already present
        if self.db.get_cf(cf, "firmware_version")?.is_none() {
            self.db.put_cf(cf, "firmware_version", "6000")?;
        }
        if self.db.get_cf(cf, "probe_id")?.is_none() {
            self.db.put_cf(cf, "probe_id", "0")?;
        }
        if self.db.get_cf(cf, "registered_at")?.is_none() {
            self.db.put_cf(cf, "registered_at", "0")?;
        }

        Ok(())
    }
}
