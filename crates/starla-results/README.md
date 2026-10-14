# starla-results

Result management and upload for Starla.

## Features

- **Bounded in-memory queue**: configurable capacity; the oldest results are dropped when the buffer is full
- **Batch upload**: the queue is drained and posted in one request, on the controller's own `httppost` slots
- **Retry and backoff**: a failed upload is re-queued, and a `429` holds uploads with an increasing backoff
- **RIPE Atlas format**: results are formatted to match the official C probe exactly — field order, spacing, numeric precision
- **Clock sync (`lts`)**: tracks the last agreement with the controller, persisted across restarts

## Usage

```rust
use starla_results::{ResultHandler, ResultHandlerConfig, UploaderConfig};
use std::time::Duration;

let handler = ResultHandler::new(
    transport, // Box<dyn UploadTransport>
    UploaderConfig::default(),
    ResultHandlerConfig {
        upload_interval: Duration::from_secs(60),
        max_result_age_secs: 3600,
        max_attempts: 5,
        cleanup_interval: Duration::from_secs(300),
        max_queue_size: 10_000,
        ..Default::default()
    },
    metrics, // starla_metrics::MetricsRegistry
);

// Once the controller assigns a session, point uploads at its endpoint.
handler
    .set_endpoint_path("/?PROBE_ID=12345&SESSION_ID=…".into())
    .await?;

// Queue a result for the next upload.
handler.submit(measurement_result).await;
```

## Components

- **`ResultQueue`**: the in-memory queue (a `VecDeque`, bounded by `max_queue_size`); no on-disk storage
- **`ResultUploader`**: builds and sends the raw HTTP/1.1 `POST` over the `UploadTransport`
- **`TimeSyncTracker`**: the last clock sync, for the `lts` field
- **`HostTelemetry`**: the 7001/9018/9002 status lines and the rptaddrs report
- **`AtlasResult`**: RIPE Atlas result formatting
