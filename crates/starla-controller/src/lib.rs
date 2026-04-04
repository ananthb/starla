//! RIPE Atlas Controller Communication
//!
//! This crate handles all communication with the RIPE Atlas infrastructure:
//! - SSH tunnel management for secure controller connection
//! - Registration protocol (INIT, KEEP commands)
//! - Telnet interface for receiving measurement commands
//! - Reverse port forwarding

pub mod ssh;
pub mod telnet;

// Re-export russh types for use in dependent crates
pub use russh;

pub use ssh::{
    generate_key, key_fingerprint, load_key, save_key, ControllerInfo, InitResponse, KnownHosts,
    ProbeInitInfo, SshConfig, SshConnection,
};
pub use telnet::{
    DnsSpec, HttpSpec, NtpSpec, PingSpec, ScheduleSpec, TelnetCommand, TelnetServer, TlsSpec,
    TracerouteSpec,
};
