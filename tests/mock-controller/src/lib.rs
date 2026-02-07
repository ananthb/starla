//! Mock controller for Starla for testing
//!
//! Provides a test harness that simulates the RIPE Atlas infrastructure:
//! - SSH server with INIT/KEEP protocol
//! - Telnet client for sending measurement commands
//! - HTTP server for receiving results

pub mod protocol;
pub mod result_collector;
pub mod ssh_server;
pub mod telnet_client;

pub use protocol::{InitResponse, KeepResponse};
pub use result_collector::ResultCollector;
pub use ssh_server::MockSshServer;
pub use telnet_client::TelnetClient;

use anyhow::Result;
use starla_common::MeasurementResult;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Mock Controller for Starla
///
/// Simulates the RIPE Atlas infrastructure for testing
pub struct MockAtlasController {
    ssh_server: MockSshServer,
    result_collector: ResultCollector,
    results: Arc<Mutex<Vec<MeasurementResult>>>,
}

impl MockAtlasController {
    /// Create and start a new mock controller
    pub async fn new() -> Result<Self> {
        let results = Arc::new(Mutex::new(Vec::new()));

        // Start SSH server on random available port
        let ssh_server = MockSshServer::new(0).await?;

        // Start HTTP result collector
        let results_clone = results.clone();
        let result_collector = ResultCollector::new(8080, results_clone).await?;

        Ok(Self {
            ssh_server,
            result_collector,
            results,
        })
    }

    /// Get the SSH port the mock controller is listening on
    pub fn ssh_port(&self) -> u16 {
        self.ssh_server.port()
    }

    /// Get the HTTP port for result uploads
    pub fn http_port(&self) -> u16 {
        self.result_collector.port()
    }

    /// Send a measurement command to the probe
    ///
    /// This simulates the controller sending a measurement request
    /// via the telnet interface
    pub async fn send_measurement(&self, spec: &str) -> Result<()> {
        // Connect to probe's telnet interface via reverse tunnel
        let mut telnet = TelnetClient::connect("127.0.0.1", 2023).await?;
        telnet.send_line(spec).await?;
        Ok(())
    }

    /// Wait for a measurement result with timeout
    pub async fn wait_for_result(&self, timeout: std::time::Duration) -> Result<MeasurementResult> {
        let start = std::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                anyhow::bail!("Timeout waiting for measurement result");
            }

            let mut results = self.results.lock().await;
            if let Some(result) = results.pop() {
                return Ok(result);
            }
            drop(results);

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Get all received results
    pub async fn get_all_results(&self) -> Vec<MeasurementResult> {
        let results = self.results.lock().await;
        results.clone()
    }

    /// Clear all results
    pub async fn clear_results(&self) {
        let mut results = self.results.lock().await;
        results.clear();
    }
}
