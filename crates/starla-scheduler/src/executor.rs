//! Measurement executor

use starla_common::MeasurementResult;
use starla_database::Database;
use std::sync::Arc;
use tracing::{debug, instrument};

pub struct Executor {
    db: Arc<Database>,
}

impl Executor {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Store a measurement result in the database
    #[instrument(skip(self, result))]
    pub fn store_result(&self, result: &MeasurementResult) -> anyhow::Result<()> {
        debug!(
            msm_id = %result.msm_id,
            "Storing measurement result in database"
        );
        self.db.store_measurement(result)?;
        Ok(())
    }
}
