# starla-metrics

Prometheus metrics collection and export for Starla.

## Features

- **Prometheus Export**: HTTP endpoint for Prometheus scraping
- **Measurement Metrics**: Track measurement execution and results
- **System Metrics**: Resource usage and health indicators
- **Feature-Gated**: No overhead when disabled

## Usage

```rust
use starla_metrics::MetricsRegistry;

let metrics = MetricsRegistry::new()?;

// Record measurement events
metrics.record_measurement_started("ping");
metrics.record_measurement_completed("ping", duration_secs);
metrics.record_measurement_error("ping", "timeout");

// Record upload events
metrics.record_upload_success(result_count, bytes);
metrics.record_upload_error();
```

## HTTP Endpoint

When the `export` feature is enabled, metrics are available at:

```
GET /metrics    # Prometheus metrics
GET /health     # Health check
```

## Cargo Features

- `export`: Enable Prometheus metrics export (includes HTTP server)

## Metrics

| Metric | Type | Description |
|--------|------|-------------|
| `starla_measurements_total` | Counter | Total measurements by type |
| `starla_measurement_duration_seconds` | Histogram | Measurement execution time |
| `starla_uploads_total` | Counter | Result upload attempts |
| `starla_upload_bytes_total` | Counter | Bytes uploaded |
