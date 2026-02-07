//! HTTP server for metrics and health endpoints

use crate::MetricsRegistry;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use prometheus::{Encoder, TextEncoder};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{debug, error};

/// Health check response
#[derive(serde::Serialize)]
struct HealthResponse {
    status: String,
}

/// Start the metrics HTTP server
///
/// Serves two endpoints:
/// - GET /metrics - Prometheus metrics in text format
/// - GET /health - JSON health check
///
/// Returns a handle that can be awaited to run the server.
/// The server will run until the provided cancellation token is triggered.
pub async fn start_metrics_server(
    metrics: Arc<MetricsRegistry>,
    addr: SocketAddr,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<(), anyhow::Error> {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/health", get(health_handler))
        .with_state(metrics);

    debug!(%addr, "Metrics server listening");

    let listener = TcpListener::bind(addr).await?;

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            cancel_token.cancelled().await;
            debug!("Metrics server shutting down");
        })
        .await?;

    Ok(())
}

/// Handler for /metrics endpoint
async fn metrics_handler(State(metrics): State<Arc<MetricsRegistry>>) -> Response {
    let encoder = TextEncoder::new();
    let metric_families = metrics.registry().gather();

    let mut buffer = Vec::new();
    match encoder.encode(&metric_families, &mut buffer) {
        Ok(()) => {
            // Convert to string and return as plain text
            match String::from_utf8(buffer) {
                Ok(body) => (
                    StatusCode::OK,
                    [("Content-Type", encoder.format_type())],
                    body,
                )
                    .into_response(),
                Err(e) => {
                    error!(error = %e, "Failed to convert metrics to UTF-8");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to encode metrics",
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            error!(error = %e, "Failed to encode metrics");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to encode metrics",
            )
                .into_response()
        }
    }
}

/// Handler for /health endpoint
async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_response() {
        let response = health_handler().await;
        assert_eq!(response.0.status, "ok");
    }
}
