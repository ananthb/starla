//! RIPE Atlas Controller Communication
//!
//! This crate handles all communication with the RIPE Atlas infrastructure:
//! - SSH tunnel management for secure controller connection
//! - Registration protocol (INIT, KEEP commands)
//! - Telnet interface for receiving measurement commands
//! - Reverse port forwarding

pub mod registration;
pub mod ssh;
pub mod telnet;

// Re-export russh types for use in dependent crates
pub use russh;

pub use registration::register;
pub use ssh::{
    generate_key, load_key, save_key, ControllerInfo, ForwardedConnection,
    ForwardedConnectionReceiver, InitResponse, ProbeInitInfo, SshConfig, SshConnection,
    DEFAULT_REGISTRATION_SERVERS,
};
pub use telnet::{
    DnsSpec, HttpSpec, NtpSpec, PingSpec, ScheduleSpec, TelnetCommand, TelnetServer, TlsSpec,
    TracerouteSpec,
};
