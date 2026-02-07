//! Mock SSH server implementation using russh

use anyhow::{Context, Result};
use async_trait::async_trait;
use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId, CryptoVec};
use russh_keys::key::KeyPair;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

/// Mock SSH server that implements the RIPE Atlas registration protocol
pub struct MockSshServer {
    port: u16,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl MockSshServer {
    /// Create and start new SSH server
    ///
    /// # Arguments
    ///
    /// * `port` - Port to listen on (0 for random available port)
    pub async fn new(port: u16) -> Result<Self> {
        // Generate Ed25519 server key
        let server_key = KeyPair::generate_ed25519().context("Failed to generate server key")?;

        let config = russh::server::Config {
            server_id: russh::SshId::Standard(format!(
                "SSH-2.0-MockAtlasController_{}",
                env!("CARGO_PKG_VERSION")
            )),
            keys: vec![server_key],
            auth_rejection_time: std::time::Duration::from_secs(1),
            auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
            ..Default::default()
        };

        let config = Arc::new(config);

        // Bind to the requested port
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse()?;
        let listener = TcpListener::bind(addr).await?;
        let actual_port = listener.local_addr()?.port();

        info!("Mock SSH server listening on 127.0.0.1:{}", actual_port);

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

        // Spawn the server loop
        let config_clone = config.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((socket, peer_addr)) => {
                                debug!("New SSH connection from {}", peer_addr);
                                let config = config_clone.clone();
                                let handler = SshSession::new();
                                tokio::spawn(async move {
                                    let session = russh::server::run_stream(config, socket, handler).await;
                                    if let Err(e) = session {
                                        warn!("SSH session error: {}", e);
                                    }
                                });
                            }
                            Err(e) => {
                                warn!("Accept error: {}", e);
                            }
                        }
                    }
                    _ = &mut shutdown_rx => {
                        info!("SSH server shutting down");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            port: actual_port,
            _shutdown: shutdown_tx,
        })
    }

    /// Get the port the server is listening on
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Individual SSH session handler
struct SshSession {
    /// Channels that have been opened
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl SshSession {
    fn new() -> Self {
        Self {
            channels: HashMap::new(),
        }
    }
}

#[async_trait]
impl russh::server::Handler for SshSession {
    type Error = anyhow::Error;

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        debug!("Channel opened for session: {:?}", channel.id());
        self.channels.insert(channel.id(), channel);
        Ok(true)
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        _public_key: &russh_keys::key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        debug!("Public key authentication attempt for user: {}", user);

        // Accept any public key for testing
        // In production, this would verify against known probe keys
        if user == "atlas" {
            info!("Authenticated probe via public key");
            Ok(Auth::Accept)
        } else {
            warn!("Authentication rejected for user: {}", user);
            Ok(Auth::Reject {
                proceed_with_methods: None,
            })
        }
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data);
        debug!("Received command: {}", command);

        let response = match command.trim() {
            cmd if cmd.starts_with("INIT") => handle_init_command(cmd),
            cmd if cmd.starts_with("KEEP") => handle_keep_command(cmd),
            cmd if cmd.starts_with("FIRMWARE_APPS") => handle_firmware_command(cmd),
            _ => format!("Unknown command: {}", command),
        };

        // Send response
        session.data(channel, CryptoVec::from(response.into_bytes()));
        session.exit_status_request(channel, 0);
        session.eof(channel);
        session.close(channel);

        Ok(())
    }

    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        info!("TCP/IP forward request: {}:{}", address, port);

        // Accept the forward request
        // In a real implementation, we'd track forwarded ports
        Ok(true)
    }
}

/// Handle INIT command from probe registration
fn handle_init_command(cmd: &str) -> String {
    info!("Handling INIT command: {}", cmd);

    // Simulate controller assignment response
    // Format: CONTROLLER <host> <port> <probe_id>
    "CONTROLLER 127.0.0.1 2222 99999\n".to_string()
}

/// Handle KEEP command (keepalive)
fn handle_keep_command(cmd: &str) -> String {
    debug!("Handling KEEP command: {}", cmd);

    // Acknowledge keepalive
    "OK\n".to_string()
}

/// Handle firmware update query
fn handle_firmware_command(cmd: &str) -> String {
    debug!("Handling FIRMWARE_APPS command: {}", cmd);

    // No firmware updates available
    "CURRENT\n".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_ssh_server_creation() {
        let server = MockSshServer::new(0).await.unwrap();
        assert!(server.port() > 0);
    }
}
