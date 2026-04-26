//! Measurement traits

use async_trait::async_trait;
use starla_common::MeasurementResult;

/// A measurement that can be executed
#[async_trait]
pub trait Measurement: Send + Sync {
    /// Get the type of this measurement
    fn measurement_type(&self) -> starla_common::MeasurementType;

    /// Execute the measurement and return the result
    async fn execute(&self) -> anyhow::Result<MeasurementResult>;
}
