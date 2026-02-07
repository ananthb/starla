//! Measurement traits

use async_trait::async_trait;
use starla_common::MeasurementResult;

/// A measurement that can be executed
#[async_trait]
pub trait Measurement: Send + Sync {
    /// Execute the measurement and return the result
    async fn execute(&self) -> anyhow::Result<MeasurementResult>;
}
