//! HTTP server for collecting measurement results

use anyhow::Result;
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use starla_common::MeasurementResult;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info};

/// HTTP server that collects measurement results
pub struct ResultCollector {
    port: u16,
}

type SharedResults = Arc<Mutex<Vec<MeasurementResult>>>;

impl ResultCollector {
    /// Create and start HTTP result collector
    pub async fn new(port: u16, results: SharedResults) -> Result<Self> {
        let app = Router::new()
            .route("/results", post(receive_result))
            .with_state(results);

        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse()?;

        // Bind first to get the actual port
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let actual_port = listener.local_addr()?.port();

        info!("Result collector listening on 127.0.0.1:{}", actual_port);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Ok(Self { port: actual_port })
    }

    /// Get the port the collector is listening on
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Handler for receiving measurement results
async fn receive_result(
    State(results): State<SharedResults>,
    Json(result): Json<MeasurementResult>,
) -> impl IntoResponse {
    debug!(
        "Received measurement result: type={}, probe_id={}",
        result.measurement_type, result.prb_id
    );

    // Store the result
    let mut results = results.lock().await;
    results.push(result);

    (StatusCode::OK, "Result received")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_result_collector() {
        let results = Arc::new(Mutex::new(Vec::new()));
        let collector = ResultCollector::new(0, results.clone()).await.unwrap();
        assert!(collector.port() > 0);
    }
}
