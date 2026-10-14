//! Measurement traits

use crate::target::LookupError;
use async_trait::async_trait;
use starla_common::MeasurementResult;

/// A measurement that can be executed
#[async_trait]
pub trait Measurement: Send + Sync {
    /// Get the type of this measurement
    fn measurement_type(&self) -> starla_common::MeasurementType;

    /// Execute the measurement and return the result
    async fn execute(&self) -> anyhow::Result<MeasurementResult>;

    /// The result to report when the target's name lookup failed, instead
    /// of running: None for a measurement that reports nothing then. Ping
    /// and traceroute write the reference's line for it.
    fn lookup_failed(&self, _error: LookupError) -> Option<MeasurementResult> {
        None
    }

    /// The result to report when the target's address is one eperd does
    /// not measure ([`crate::target::address_allowed`]), instead of
    /// running: None for a measurement that reports nothing then. Ping and
    /// traceroute write the reference's "address not allowed" line.
    fn address_not_allowed(&self) -> Option<MeasurementResult> {
        None
    }
}
