//! RIPE Atlas Controller Communication
//!
//! This crate handles all communication with the RIPE Atlas infrastructure:
//! - SSH tunnel management for secure controller connection
//! - Registration protocol (INIT, KEEP commands)
//! - Telnet interface for receiving measurement commands
//! - Reverse port forwarding

pub mod channel_stream;
pub mod ssh;
pub mod telnet;

// Re-export russh types for use in dependent crates
pub use russh;

pub use channel_stream::channel_to_stream;
pub use ssh::{
    generate_key, key_fingerprint, load_key, load_key_from_string, save_key, ControllerInfo,
    InitResponse, KnownHosts, ProbeInitInfo, SshConfig, SshConnection, TelnetState,
};
pub use telnet::{
    DnsSpec, HostTelemetryKind, HostTelemetrySpec, HttpSpec, NtpSpec, PingSpec, ScheduleSpec,
    TelnetCommand, TelnetServer, TlsSpec, TracerouteSpec,
};
