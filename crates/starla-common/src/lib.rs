//! Common types and utilities for Starla
//!
//! This crate provides shared functionality across all Starla components:
//! - Core types (ProbeId, MeasurementId, etc.)
//! - Configuration management
//! - Path resolution (XDG/systemd)
//! - Logging infrastructure
//! - Error types

pub mod config;
pub mod error;
pub mod logging;
pub mod paths;
pub mod types;

pub use config::*;
pub use error::*;
pub use paths::{
    config_dir, config_file, database_path, ensure_config_dir, ensure_state_dir, probe_id_path,
    probe_key_path, probe_pubkey_path, read_probe_id, results_queue_path, state_dir,
    write_probe_id,
};
pub use types::*;

/// Firmware version for the Rust implementation
pub const FIRMWARE_VERSION: u32 = 6000;

/// Version string
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
