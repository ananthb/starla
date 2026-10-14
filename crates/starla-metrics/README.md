# starla-metrics

Prometheus metrics collection and export for Starla.

## Features

- **Prometheus export**: HTTP endpoint for scraping (with the `export` feature)
- **Measurement metrics**: executed, completed, failed, and duration
- **Upload metrics**: attempts, successes, failures, duration, queue depth
- **No overhead when disabled**: without `export`, every method is a no-op

## Usage

```rust
use starla_metrics::{start_metrics_server, MetricsRegistry};

let metrics = MetricsRegistry::new()?;

// Measurement events
metrics.record_measurement_started("ping");
metrics.record_measurement_completed("ping", duration_secs);
metrics.record_measurement_failed("ping", duration_secs);

// Upload events
metrics.record_upload_attempt();
metrics.record_upload_success();
metrics.record_upload_failure();
metrics.record_upload_duration(duration_secs);

// State
metrics.update_queue_depth(depth);
metrics.record_queue_drop();
metrics.update_scheduler_tasks(count);
metrics.set_connected(true);
metrics.record_connection_attempt();

// Serve /metrics and /health (runs until the cancel token fires).
start_metrics_server(metrics, "127.0.0.1:9695".parse()?, cancel_token).await?;
```

## HTTP Endpoint

With the `export` feature enabled:

```
GET /metrics    # Prometheus metrics
GET /health     # Health check
```

## Cargo Features

- `export`: enable the Prometheus exporter and its HTTP server

## Metrics

| Metric | Type | Description |
|--------|------|-------------|
| `starla_measurements_started_total` | Counter | Measurements started, by type |
| `starla_measurements_completed_total` | Counter | Measurements completed, by type |
| `starla_measurements_failed_total` | Counter | Measurements failed, by type |
| `starla_measurement_duration_seconds` | Histogram | Measurement execution time, by type |
| `starla_scheduler_tasks_active` | Gauge | Active scheduled tasks |
| `starla_upload_attempts_total` | Counter | Result upload attempts |
| `starla_upload_success_total` | Counter | Successful result uploads |
| `starla_upload_failures_total` | Counter | Failed result uploads |
| `starla_upload_duration_seconds` | Histogram | Result upload duration |
| `starla_upload_queue_depth` | Gauge | Results waiting to upload |
| `starla_queue_dropped_total` | Counter | Results dropped from the queue |
| `starla_controller_connected` | Gauge | 1 while connected to the controller |
| `starla_connection_attempts_total` | Counter | Controller connection attempts |
