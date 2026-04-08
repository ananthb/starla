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
pub mod status;
pub mod types;

pub use config::*;
pub use error::*;
pub use paths::{
    config_dir, config_file, ensure_config_dir, ensure_state_dir, known_hosts_path, probe_id_path,
    probe_key_path, probe_pubkey_path, read_probe_id, runtime_dir, set_runtime_dir, set_state_dir,
    state_dir, status_socket_path, write_probe_id,
};
pub use types::*;

/// Firmware version — must match the reference probe version for controller
/// acceptance
pub const FIRMWARE_VERSION: u32 = 5120;

/// Version string
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
