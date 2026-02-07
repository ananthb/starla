//! Automatic database cleanup task
//!
//! Runs periodic cleanup based on dual criteria:
//! 1. Time-based: Delete measurements older than retention_days
//! 2. Size-based: Delete oldest measurements when database exceeds
//!    max_database_size_mb

use crate::Database;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, error, warn};

/// Cleanup configuration
#[derive(Debug, Clone)]
pub struct CleanupConfig {
    /// Maximum age of measurements in days
    pub retention_days: u32,
    /// Maximum database size in megabytes
    pub max_database_size_mb: u64,
    /// How often to run cleanup checks (in hours)
    pub cleanup_interval_hours: u64,
}

/// Cleanup statistics from a single run
#[derive(Debug, Clone)]
pub struct CleanupStats {
    pub measurements_deleted_by_time: u64,
    pub measurements_deleted_by_size: u64,
    pub queue_items_deleted: u64,
    pub database_size_before_mb: f64,
    pub database_size_after_mb: f64,
}

/// Run a single cleanup cycle
///
/// This implements the dual-criteria cleanup strategy:
/// 1. Delete measurements older than retention threshold
/// 2. If database still exceeds size limit, delete oldest until under limit
/// 3. Clean up old/failed upload queue items
pub fn run_cleanup_cycle(
    db: &Database,
    config: &CleanupConfig,
) -> Result<CleanupStats, anyhow::Error> {
    debug!(
        retention_days = config.retention_days,
        max_size_mb = config.max_database_size_mb,
        "Starting cleanup cycle"
    );

    // Record initial database size
    let size_before = db.get_database_size_bytes()?;
    let size_before_mb = size_before as f64 / (1024.0 * 1024.0);

    // 1. Time-based cleanup
    let time_threshold = chrono::Utc::now().timestamp() - (config.retention_days as i64 * 86400);
    let time_deleted = db.cleanup_old_measurements(time_threshold)?;

    if time_deleted > 0 {
        debug!(deleted = time_deleted, "Time-based cleanup completed");
    }

    // 2. Size-based cleanup
    let mut size_deleted = 0u64;
    let current_size = db.get_database_size_bytes()?;
    let current_size_mb = current_size as f64 / (1024.0 * 1024.0);

    if current_size_mb > config.max_database_size_mb as f64 {
        warn!(
            current_mb = current_size_mb,
            max_mb = config.max_database_size_mb,
            "Database exceeds size limit, starting size-based cleanup"
        );

        size_deleted = db.cleanup_by_size(config.max_database_size_mb)?;

        if size_deleted > 0 {
            debug!(deleted = size_deleted, "Size-based cleanup completed");
        }
    }

    // 3. Upload queue cleanup
    let queue_deleted = db.cleanup_upload_queue(time_threshold)?;

    if queue_deleted > 0 {
        debug!(deleted = queue_deleted, "Upload queue cleanup completed");
    }

    // Record final database size
    let size_after = db.get_database_size_bytes()?;
    let size_after_mb = size_after as f64 / (1024.0 * 1024.0);

    let stats = CleanupStats {
        measurements_deleted_by_time: time_deleted,
        measurements_deleted_by_size: size_deleted,
        queue_items_deleted: queue_deleted,
        database_size_before_mb: size_before_mb,
        database_size_after_mb: size_after_mb,
    };

    debug!(
        time_deleted = time_deleted,
        size_deleted = size_deleted,
        queue_deleted = queue_deleted,
        size_before_mb = format!("{:.2}", size_before_mb),
        size_after_mb = format!("{:.2}", size_after_mb),
        freed_mb = format!("{:.2}", size_before_mb - size_after_mb),
        "Cleanup cycle complete"
    );

    Ok(stats)
}

/// Spawn a background cleanup task
///
/// This task runs periodically based on cleanup_interval_hours.
/// It will continue running until the provided cancellation token is triggered.
///
/// # Example
///
/// ```no_run
/// use starla_database::{cleanup, Database};
/// use tokio_util::sync::CancellationToken;
///
/// async fn example() {
///     let db = Database::connect(std::path::Path::new("atlas.db")).unwrap();
///     let config = cleanup::CleanupConfig {
///         retention_days: 30,
///         max_database_size_mb: 1,
///         cleanup_interval_hours: 24,
///     };
///     let cancel_token = CancellationToken::new();
///
///     tokio::spawn(cleanup::spawn_cleanup_task(
///         db,
///         config,
///         cancel_token.clone(),
///     ));
///
///     // Later, to stop the cleanup task:
///     // cancel_token.cancel();
/// }
/// ```
pub async fn spawn_cleanup_task(
    db: Database,
    config: CleanupConfig,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    let interval_duration = Duration::from_secs(config.cleanup_interval_hours * 3600);
    let mut ticker = interval(interval_duration);

    debug!(
        interval_hours = config.cleanup_interval_hours,
        "Cleanup task started"
    );

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match run_cleanup_cycle(&db, &config) {
                    Ok(stats) => {
                        debug!(
                            total_deleted = stats.measurements_deleted_by_time + stats.measurements_deleted_by_size,
                            "Cleanup cycle succeeded"
                        );
                    }
                    Err(e) => {
                        error!(error = %e, "Cleanup cycle failed");
                    }
                }
            }
            _ = cancel_token.cancelled() => {
                debug!("Cleanup task shutting down");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cleanup_config() {
        let config = CleanupConfig {
            retention_days: 30,
            max_database_size_mb: 1,
            cleanup_interval_hours: 24,
        };

        assert_eq!(config.retention_days, 30);
        assert_eq!(config.max_database_size_mb, 1);
        assert_eq!(config.cleanup_interval_hours, 24);
    }
}
