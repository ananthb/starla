//! Registration logic
//!
//! This module provides helper functions for the registration flow.
//! The main registration logic is now in `ssh::SshConnection::init()`.

use super::ssh::{ControllerInfo, InitResponse, ProbeInitInfo, SshConnection};
use starla_common::ProbeId;
use tracing::info;

/// Registration result
#[derive(Debug)]
pub enum RegistrationResult {
    /// Successfully registered with a controller
    Registered(ControllerInfo, ProbeId),
    /// Controller ready with remote port and session ID
    ControllerReady {
        remote_port: u16,
        session_id: String,
    },
    /// Key recognized but not yet fully registered
    Pending,
    /// Server requested wait before retry
    Wait { timeout_secs: u32 },
}

/// Perform registration using an existing SSH connection to a registration
/// server
///
/// This is a convenience wrapper around `SshConnection::init()`.
pub async fn register(
    ssh: &SshConnection,
    probe_info: &ProbeInitInfo,
) -> anyhow::Result<RegistrationResult> {
    match ssh.init(Some(probe_info)).await? {
        InitResponse::Controller(info) => {
            let probe_id = ProbeId(info.probe_id);
            info!(
                "Registered with controller at {}:{}, probe_id={}",
                info.host, info.port, probe_id
            );
            Ok(RegistrationResult::Registered(info, probe_id))
        }
        InitResponse::ControllerReady {
            remote_port,
            session_id,
        } => {
            info!(
                "Controller ready with remote port {}, session_id {}",
                remote_port, session_id
            );
            Ok(RegistrationResult::ControllerReady {
                remote_port,
                session_id,
            })
        }
        InitResponse::Ok => {
            info!("Probe key recognized, but not yet fully registered");
            Ok(RegistrationResult::Pending)
        }
        InitResponse::Wait { timeout_secs } => {
            info!("Server requested wait for {} seconds", timeout_secs);
            Ok(RegistrationResult::Wait { timeout_secs })
        }
    }
}
