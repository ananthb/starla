//! Logging infrastructure for Starla
//!
//! Provides flexible logging with:
//! - stdout output by default (journalctl-friendly)
//! - Optional file output with rotation
//! - Feature-gated JSON structured logging
//! - Environment-based log level configuration

use std::path::PathBuf;
pub use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Logging configuration
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// Optional log directory for file output
    pub log_dir: Option<PathBuf>,

    /// Log level (trace, debug, info, warn, error)
    pub level: String,

    /// Whether to use JSON format
    pub json_format: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            log_dir: None,
            level: "info".to_string(),
            json_format: cfg!(feature = "json"),
        }
    }
}

/// Initialize logging subsystem
///
/// # Arguments
///
/// * `config` - Logging configuration
///
/// # Examples
///
/// ```no_run
/// use starla_common::logging::{init_logging, LogConfig};
///
/// // Default: stdout with info level
/// init_logging(LogConfig::default()).unwrap();
///
/// // With custom log directory
/// let config = LogConfig {
///     log_dir: Some("/var/log/starla".into()),
///     level: "debug".to_string(),
///     json_format: false,
/// };
/// init_logging(config).unwrap();
/// ```
pub fn init_logging(config: LogConfig) -> anyhow::Result<()> {
    // Build filter from config and RUST_LOG env var
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));

    match config.log_dir {
        None => init_stdout_logging(filter, config.json_format),
        Some(dir) => init_file_logging(dir, filter, config.json_format),
    }
}

/// Initialize stdout logging (default)
#[allow(unused_variables)]
fn init_stdout_logging(filter: EnvFilter, json_format: bool) -> anyhow::Result<()> {
    #[cfg(feature = "json")]
    {
        if json_format {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    fmt::layer()
                        .json()
                        .with_target(true)
                        .with_current_span(true)
                        .with_span_list(false)
                        .with_thread_ids(true)
                        .with_thread_names(true),
                )
                .init();
            return Ok(());
        }
    }

    // Default text format
    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .compact()
                .with_target(true)
                .with_thread_ids(false),
        )
        .init();

    Ok(())
}

/// Initialize file-based logging with rotation
#[allow(unused_variables)]
fn init_file_logging(log_dir: PathBuf, filter: EnvFilter, json_format: bool) -> anyhow::Result<()> {
    // Ensure log directory exists
    std::fs::create_dir_all(&log_dir)?;

    // Create daily rolling file appender
    let file_appender = tracing_appender::rolling::daily(log_dir, "probe.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    #[cfg(feature = "json")]
    {
        if json_format {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    fmt::layer()
                        .json()
                        .with_target(true)
                        .with_current_span(true)
                        .with_span_list(false)
                        .with_thread_ids(true)
                        .with_thread_names(true)
                        .with_writer(non_blocking),
                )
                .init();
            return Ok(());
        }
    }

    // Default text format for files
    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_target(true)
                .with_thread_ids(true)
                .with_ansi(false) // No ANSI colors in log files
                .with_writer(non_blocking),
        )
        .init();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = LogConfig::default();
        assert_eq!(config.level, "info");
        assert!(config.log_dir.is_none());
    }
}
