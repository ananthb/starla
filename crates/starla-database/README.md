# starla-database

SQLite database integration for persistent storage.

## Features

- **SQLite with SQLx**: Async database operations with compile-time query checking
- **Measurement Storage**: Store and query measurement results
- **Scheduler Persistence**: Track scheduled measurements across restarts
- **Upload Queue**: Persistent queue for results pending upload
- **Automatic Cleanup**: Configurable retention and size-based cleanup

## Usage

```rust
use starla_database::{Database, CleanupConfig};

// Connect to database
let db = Database::connect("measurements.db").await?;

// Store measurement results
db.store_measurement(&result).await?;

// Query measurements
let stats = db.get_measurement_stats().await?;

// Run cleanup
let cleanup_config = CleanupConfig {
    retention_days: 7,
    max_database_size_mb: 100,
    cleanup_interval_hours: 24,
};
let stats = cleanup::run_cleanup_cycle(&db, &cleanup_config).await?;
```

## Schema

The database includes tables for:
- `measurements`: Completed measurement results
- `scheduled_measurements`: Active recurring measurements
- `upload_queue`: Results waiting to be uploaded
- `probe_info`: Probe configuration and state
