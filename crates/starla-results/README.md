# starla-results

Result management and upload for Starla.

## Features

- **Persistent Queue**: SQLite-backed queue survives restarts
- **Batch Upload**: Efficient batching of multiple results
- **Retry Logic**: Exponential backoff for failed uploads
- **Compression**: Gzip compression with auto-negotiation
- **Format Compatibility**: Results match RIPE Atlas JSON schema

## Usage

```rust
use starla_results::{ResultHandler, ResultHandlerConfig, UploaderConfig};

let config = ResultHandlerConfig {
    batch_size: 10,
    upload_interval: Duration::from_secs(10),
    max_result_age_secs: 3600,
    max_attempts: 5,
    cleanup_interval: Duration::from_secs(300),
};

let handler = ResultHandler::new(
    "results_queue.db",
    UploaderConfig::default(),
    config,
).await?;

// Queue a result for upload
handler.queue_result(measurement_result).await?;

// Run the upload loop
handler.run(cancel_token).await?;
```

## Components

- **PersistentResultQueue**: Durable storage for pending results
- **ResultUploader**: HTTP client for uploading to controller
- **TimeSyncTracker**: Tracks time synchronization for `lts` field
- **AtlasResult**: RIPE Atlas result format wrapper
