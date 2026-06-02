//! Probe status for tray app communication
//!
//! Serialized as JSON over a Unix domain socket (or named pipe on Windows).

use crate::pause::PauseState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Current probe status, sent to the tray app on socket connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeStatus {
    /// Probe ID (0 if not yet registered)
    pub probe_id: u32,
    /// Whether connected to the controller
    pub connected: bool,
    /// Controller hostname (if connected)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controller: Option<String>,
    /// Seconds since probe started
    pub uptime_secs: u64,
    /// Scheduled measurement counts by type
    pub measurements: HashMap<String, u64>,
    /// Results waiting in the upload queue
    pub queue_depth: usize,
    /// SSH public key (for registration)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    /// Most recent controller/registration error. Lets the tray show
    /// *why* the probe is disconnected instead of just a red dot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_connection_error: Option<String>,
    /// Active pause (suppresses measurement dispatch). Tray writes the
    /// underlying file; probe reads it back on every scheduler tick.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause: Option<PauseState>,
}
