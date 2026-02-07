//! SSH connection management for RIPE Atlas controller communication
//!
//! This module provides:
//! - SSH tunnel management with automatic reconnection
//! - Registration protocol (INIT command)
//! - Keepalive handling (KEEP command)
//! - Reverse port forwarding for telnet interface

use async_trait::async_trait;
use russh::client::{self, Handle, Msg};
use russh::{kex, Channel, ChannelMsg, Preferred};
use russh_keys::key::{KeyPair, PublicKey};
use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep, timeout};
use tracing::{debug, error, info, trace, warn};

/// Default registration servers for RIPE Atlas (production)
/// These are the official registration servers from the RIPE Atlas probe
/// source. The probe tries each in order until one succeeds.
pub const DEFAULT_REGISTRATION_SERVERS: &[&str] = &[
    // Primary registration server (hostname)
    "reg03.atlas.ripe.net:443",
    // Primary registration server (IPv4)
    "193.0.19.246:443",
    // Primary registration server (IPv6)
    "[2001:67c:2e8:11::c100:13f6]:443",
    // Secondary registration server (hostname)
    "reg04.atlas.ripe.net:443",
    // Secondary registration server (IPv4)
    "193.0.19.247:443",
    // Secondary registration server (IPv6)
    "[2001:67c:2e8:11::c100:13f7]:443",
];

/// SSH connection configuration
#[derive(Debug, Clone)]
pub struct SshConfig {
    /// Connection timeout
    pub connect_timeout: Duration,
    /// Inactivity timeout before disconnect
    pub inactivity_timeout: Duration,
    /// Keepalive interval
    pub keepalive_interval: Duration,
    /// Reconnection delay (base)
    pub reconnect_delay: Duration,
    /// Maximum reconnection delay
    pub max_reconnect_delay: Duration,
    /// Number of reconnection attempts before giving up (0 = infinite)
    pub max_reconnect_attempts: u32,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            inactivity_timeout: Duration::from_secs(120),
            keepalive_interval: Duration::from_secs(30),
            reconnect_delay: Duration::from_secs(5),
            max_reconnect_delay: Duration::from_secs(300),
            max_reconnect_attempts: 0, // Infinite
        }
    }
}

/// Controller information returned from INIT
#[derive(Debug, Clone)]
pub struct ControllerInfo {
    pub host: String,
    pub port: u16,
    pub probe_id: u32,
}

/// INIT response variants
#[derive(Debug, Clone)]
pub enum InitResponse {
    /// Registration server responded with controller assignment
    Controller(ControllerInfo),
    /// Controller responded with remote port and session ID (ready for KEEP)
    ControllerReady {
        remote_port: u16,
        session_id: String,
    },
    /// Probe key is recognized but not yet fully registered (from reg server)
    Ok,
    /// Server tells us to wait and retry after timeout seconds
    Wait { timeout_secs: u32 },
}

/// Probe information sent during registration INIT
#[derive(Debug, Clone)]
pub struct ProbeInitInfo {
    /// Firmware version (e.g., 6000)
    pub firmware_version: u32,
    /// Reason for registration (e.g., "NEW", "REREG_TIMER_EXPIRED")
    pub reason: String,
    /// Optional MAC address for identification
    pub mac_address: Option<String>,
}

impl ProbeInitInfo {
    /// Create probe init info for a new registration
    pub fn new(firmware_version: u32) -> Self {
        Self {
            firmware_version,
            reason: "NEW".to_string(),
            mac_address: None,
        }
    }

    /// Create probe init info for re-registration
    pub fn reregister(firmware_version: u32, reason: &str) -> Self {
        Self {
            firmware_version,
            reason: reason.to_string(),
            mac_address: None,
        }
    }

    /// Set the MAC address
    pub fn with_mac(mut self, mac: &str) -> Self {
        self.mac_address = Some(mac.to_string());
        self
    }

    /// Format as P_TO_R_INIT message for INIT command stdin
    pub fn to_init_message(&self) -> String {
        let mut msg = String::new();
        msg.push_str("P_TO_R_INIT\n");

        // TOKEN_SPECS format: probev1 <kernel_version>[-<mac>] <firmware_version>
        // We use a generic kernel version since we're not on the probe hardware
        let kernel_version = "6.0.0-rust";
        if let Some(ref mac) = self.mac_address {
            msg.push_str(&format!(
                "TOKEN_SPECS probev1 {}-{} {}\n",
                kernel_version, mac, self.firmware_version
            ));
        } else {
            msg.push_str(&format!(
                "TOKEN_SPECS probev1 {} {}\n",
                kernel_version, self.firmware_version
            ));
        }

        msg.push_str(&format!("REASON_FOR_REGISTRATION {}\n", self.reason));
        msg
    }
}

/// Client handler for russh
struct AtlasClientHandler {
    /// Channel for receiving forwarded connections
    #[allow(dead_code)]
    forward_tx: mpsc::Sender<ForwardedConnection>,
}

/// A forwarded TCP connection
pub struct ForwardedConnection {
    pub address: String,
    pub port: u32,
    pub channel: Channel<Msg>,
}

#[async_trait]
impl client::Handler for AtlasClientHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        // TODO: In production, verify against known RIPE Atlas server keys
        // For now, accept all
        Ok(true)
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        mut channel: Channel<Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        info!(
            "Forwarded connection from {}:{} to {}:{}",
            originator_address, originator_port, connected_address, connected_port
        );

        // Spawn a task to bridge this channel directly to local telnet
        // This avoids issues with moving the channel through mpsc
        let local_port = connected_port;
        let address = connected_address.to_string();

        tokio::spawn(async move {
            info!("Bridging forwarded connection {}:{}", address, local_port);

            // Connect to local telnet
            let local_addr = format!("127.0.0.1:{}", local_port);
            let local = match tokio::net::TcpStream::connect(&local_addr).await {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to connect to local {}: {}", local_addr, e);
                    return;
                }
            };

            debug!("Connected to local telnet at {}", local_addr);

            let (mut local_read, mut local_write) = local.into_split();
            let mut local_buf = [0u8; 4096];

            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            debug!("Starting bridge loop for channel {:?}", channel.id());

            loop {
                tokio::select! {
                    biased; // Prioritize SSH channel data over local reads

                    // Data from SSH channel -> local
                    msg = channel.wait() => {
                        trace!("channel.wait() returned: {:?}", msg.as_ref().map(|m| match m {
                            ChannelMsg::Data { data } => format!("Data({} bytes)", data.len()),
                            ChannelMsg::Eof => "Eof".to_string(),
                            ChannelMsg::Close => "Close".to_string(),
                            other => format!("{:?}", other),
                        }));
                        match msg {
                            Some(ChannelMsg::Data { data }) => {
                                trace!("SSH -> Local: {} bytes", data.len());
                                if let Err(e) = local_write.write_all(&data).await {
                                    error!("Failed to write to local: {}", e);
                                    break;
                                }
                                let _ = local_write.flush().await;
                            }
                            Some(ChannelMsg::Eof) | None => {
                                debug!("SSH channel closed");
                                break;
                            }
                            Some(ChannelMsg::ExitStatus { exit_status }) => {
                                debug!("SSH channel exit status: {}", exit_status);
                            }
                            other => {
                                trace!("SSH channel message: {:?}", other);
                            }
                        }
                    }
                    // Data from local -> SSH channel
                    result = local_read.read(&mut local_buf) => {
                        match result {
                            Ok(0) => {
                                debug!("Local connection closed");
                                let _ = channel.eof().await;
                                break;
                            }
                            Ok(n) => {
                                trace!("Local -> SSH: {} bytes", n);
                                if let Err(e) = channel.data(&local_buf[..n]).await {
                                    error!("Failed to write to SSH: {:?}", e);
                                    break;
                                }
                            }
                            Err(e) => {
                                error!("Error reading from local: {}", e);
                                break;
                            }
                        }
                    }
                }
            }

            debug!("Forwarded connection bridge ended");
        });

        Ok(())
    }
}

/// Receiver for forwarded connections from SSH tunnel
pub struct ForwardedConnectionReceiver {
    rx: mpsc::Receiver<ForwardedConnection>,
}

impl ForwardedConnectionReceiver {
    /// Receive next forwarded connection
    pub async fn recv(&mut self) -> Option<ForwardedConnection> {
        self.rx.recv().await
    }
}

/// SSH connection to RIPE Atlas controller
pub struct SshConnection {
    session: Arc<Mutex<Handle<AtlasClientHandler>>>,
    #[allow(dead_code)]
    config: SshConfig,
    host: String,
    port: u16,
    forward_rx: Option<mpsc::Receiver<ForwardedConnection>>,
}

impl SshConnection {
    /// Connect to a controller server
    pub async fn connect(
        host: &str,
        port: u16,
        key: &KeyPair,
        config: SshConfig,
    ) -> anyhow::Result<Self> {
        // RIPE Atlas registration servers only support diffie-hellman-group1-sha1
        // and diffie-hellman-group-exchange-sha256. Since russh doesn't support
        // group exchange, we must use group1-sha1 (despite it being considered weak).
        let preferred = Preferred {
            kex: Cow::Owned(vec![
                kex::DH_G1_SHA1,
                kex::DH_G14_SHA1,
                kex::DH_G14_SHA256,
                kex::CURVE25519,
            ]),
            ..Preferred::DEFAULT
        };

        let ssh_config = client::Config {
            inactivity_timeout: Some(config.inactivity_timeout),
            keepalive_interval: Some(config.keepalive_interval),
            preferred,
            ..Default::default()
        };

        let (forward_tx, forward_rx) = mpsc::channel(16);
        let handler = AtlasClientHandler { forward_tx };

        let addr = format!("{}:{}", host, port);
        info!("Connecting to SSH controller at {}", addr);

        let session = timeout(
            config.connect_timeout,
            client::connect(Arc::new(ssh_config), addr, handler),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Connection timeout"))??;

        let mut session = session;

        // Authenticate
        let auth_res = session
            .authenticate_publickey("atlas", Arc::new(key.clone()))
            .await?;

        if !auth_res {
            anyhow::bail!("SSH authentication failed");
        }

        info!("SSH authentication successful");

        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            config,
            host: host.to_string(),
            port,
            forward_rx: Some(forward_rx),
        })
    }

    /// Connect with automatic retry
    pub async fn connect_with_retry(
        host: &str,
        port: u16,
        key: &KeyPair,
        config: SshConfig,
    ) -> anyhow::Result<Self> {
        let mut attempts = 0u32;
        let mut delay = config.reconnect_delay;

        loop {
            attempts += 1;

            match Self::connect(host, port, key, config.clone()).await {
                Ok(conn) => return Ok(conn),
                Err(e) => {
                    if config.max_reconnect_attempts > 0
                        && attempts >= config.max_reconnect_attempts
                    {
                        return Err(anyhow::anyhow!(
                            "Failed to connect after {} attempts: {}",
                            attempts,
                            e
                        ));
                    }

                    warn!(
                        "Connection attempt {} failed: {}. Retrying in {:?}...",
                        attempts, e, delay
                    );

                    sleep(delay).await;

                    // Exponential backoff
                    delay = std::cmp::min(delay * 2, config.max_reconnect_delay);
                }
            }
        }
    }

    /// Try connecting to multiple servers in order
    pub async fn connect_to_servers(
        servers: &[&str],
        key: &KeyPair,
        config: SshConfig,
    ) -> anyhow::Result<Self> {
        for server in servers {
            let parts: Vec<&str> = server.split(':').collect();
            let (host, port) = if parts.len() == 2 {
                (parts[0], parts[1].parse().unwrap_or(443))
            } else {
                (*server, 443u16)
            };

            match Self::connect(host, port, key, config.clone()).await {
                Ok(conn) => {
                    info!("Connected to {}", server);
                    return Ok(conn);
                }
                Err(e) => {
                    warn!("Failed to connect to {}: {}", server, e);
                }
            }
        }

        anyhow::bail!("Failed to connect to any server")
    }

    /// Execute the INIT command and parse response
    ///
    /// The RIPE Atlas protocol requires sending probe identification data
    /// to the server when running INIT on a registration server.
    pub async fn init(&self, probe_info: Option<&ProbeInitInfo>) -> anyhow::Result<InitResponse> {
        let output = if let Some(info) = probe_info {
            // Send probe info to registration server
            let stdin_data = info.to_init_message();
            debug!("Sending INIT with probe info:\n{}", stdin_data);
            self.execute_with_stdin("INIT", &stdin_data).await?
        } else {
            // Controller INIT - no stdin data needed
            self.execute("INIT").await?
        };

        // Parse response line by line - the actual response may have multiple lines
        let lines: Vec<&str> = output.lines().collect();

        if lines.is_empty() {
            anyhow::bail!("Empty INIT response");
        }

        let first_line = lines[0].trim();
        debug!("INIT response: {} ({} lines)", first_line, lines.len());
        for (i, line) in lines.iter().enumerate().skip(1) {
            trace!("INIT response line {}: {}", i + 1, line);
        }

        match first_line {
            "OK" => {
                // Parse additional lines for various info
                // Registration server format: OK\nCONTROLLER <host> <port> ssh-rsa
                // <key>\nREREGISTER <secs> Controller format: OK\nREMOTE_PORT
                // <port>\nSESSION_ID <id>
                for line in lines.iter().skip(1) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.is_empty() {
                        continue;
                    }

                    // Registration server response - controller assignment
                    if parts[0] == "CONTROLLER" && parts.len() >= 4 {
                        let host = parts[1].to_string();
                        let port: u16 = parts[2]
                            .parse()
                            .map_err(|_| anyhow::anyhow!("Invalid port"))?;

                        debug!("Got controller: {}:{}", host, port);

                        return Ok(InitResponse::Controller(ControllerInfo {
                            host,
                            port,
                            probe_id: 0,
                        }));
                    }
                }

                // Parse controller response with REMOTE_PORT and SESSION_ID
                let mut remote_port: Option<u16> = None;
                let mut session_id: Option<String> = None;

                for line in lines.iter().skip(1) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.is_empty() {
                        continue;
                    }

                    if parts[0] == "REMOTE_PORT" && parts.len() >= 2 {
                        remote_port = parts[1].parse().ok();
                        if let Some(port) = remote_port {
                            debug!("Controller assigned remote port: {}", port);
                        }
                    }

                    if parts[0] == "SESSION_ID" && parts.len() >= 2 {
                        session_id = Some(parts[1].to_string());
                        debug!("Controller assigned session ID: {}", parts[1]);
                    }
                }

                if let (Some(port), Some(sid)) = (remote_port, session_id) {
                    return Ok(InitResponse::ControllerReady {
                        remote_port: port,
                        session_id: sid,
                    });
                }

                // Just OK with no additional info
                // This could mean:
                // 1. From reg server: probe key recognized but not registered yet
                // 2. From controller: should have REMOTE_PORT but doesn't
                // Return Ok and let caller decide what to do
                debug!("Got OK without CONTROLLER or REMOTE_PORT/SESSION_ID info");
                Ok(InitResponse::Ok)
            }
            "WAIT" => {
                // Parse TIMEOUT from next line
                // Format: WAIT\nTIMEOUT <seconds>
                let mut timeout_secs = 60u32; // Default timeout
                for line in lines.iter().skip(1) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 && parts[0] == "TIMEOUT" {
                        timeout_secs = parts[1].parse().unwrap_or(60);
                        break;
                    }
                }
                debug!("Server requested wait: {} seconds", timeout_secs);
                Ok(InitResponse::Wait { timeout_secs })
            }
            _ => {
                anyhow::bail!("Unknown INIT response: {}", output);
            }
        }
    }

    /// Start the KEEP session
    /// This opens a channel with the KEEP command and keeps it open
    /// The session remains active to maintain the SSH connection and tunnels
    ///
    /// Spawns a background task to monitor the KEEP channel for any data
    pub async fn start_keep_session(&self) -> anyhow::Result<()> {
        debug!("Starting KEEP session");
        let session = self.session.lock().await;
        let mut channel = session.channel_open_session().await?;
        channel.exec(true, "KEEP").await?;

        debug!("KEEP session started");

        // Spawn a task to monitor the KEEP channel
        // This keeps the channel alive and logs any data received
        tokio::spawn(async move {
            trace!("KEEP channel monitor started");
            while let Some(msg) = channel.wait().await {
                match msg {
                    ChannelMsg::Data { data } => {
                        trace!("KEEP channel data: {} bytes", data.len());
                    }
                    ChannelMsg::Eof => {
                        debug!("KEEP channel EOF");
                        break;
                    }
                    ChannelMsg::ExitStatus { exit_status } => {
                        debug!("KEEP channel exit status: {}", exit_status);
                    }
                    other => {
                        trace!("KEEP channel message: {:?}", other);
                    }
                }
            }
            debug!("KEEP channel monitor ended");
        });

        Ok(())
    }

    /// Execute the KEEP command (legacy - use start_keep_session instead)
    pub async fn keep(&self) -> anyhow::Result<()> {
        let output = self.execute("KEEP").await?;
        let output = output.trim();

        if output != "OK" {
            warn!("Unexpected KEEP response: {}", output);
        }

        debug!("KEEP acknowledged");
        Ok(())
    }

    /// Request reverse port forwarding
    pub async fn request_reverse_tunnel(&self, bind_port: u16) -> anyhow::Result<()> {
        debug!("Requesting reverse tunnel on port {}", bind_port);

        let mut session = self.session.lock().await;

        // Check if session is still connected
        if session.is_closed() {
            anyhow::bail!("SSH session is closed, cannot setup tunnel");
        }

        // Use "localhost" as the bind address, matching OpenSSH behavior
        // The server will listen on localhost:bind_port
        session.tcpip_forward("localhost", bind_port as u32).await?;

        Ok(())
    }

    /// Cancel reverse port forwarding
    pub async fn cancel_reverse_tunnel(&self, bind_port: u16) -> anyhow::Result<()> {
        debug!("Cancelling reverse tunnel on port {}", bind_port);

        let session = self.session.lock().await;
        session
            .cancel_tcpip_forward("127.0.0.1", bind_port as u32)
            .await?;

        Ok(())
    }

    /// Create a direct-tcpip channel to forward data to the controller
    /// This opens an SSH channel that connects to the specified host:port on
    /// the server side
    pub async fn open_direct_tcpip(
        &self,
        remote_host: &str,
        remote_port: u16,
    ) -> anyhow::Result<Channel<Msg>> {
        debug!(
            "Opening direct-tcpip channel to {}:{}",
            remote_host, remote_port
        );

        let session = self.session.lock().await;

        // Open a direct-tcpip channel
        // This tells the SSH server to connect to remote_host:remote_port
        // and forward data through this channel
        let channel = session
            .channel_open_direct_tcpip(
                remote_host,
                remote_port as u32,
                "127.0.0.1", // originator address (us)
                0,           // originator port (unused)
            )
            .await?;

        Ok(channel)
    }

    /// Get the controller host we're connected to
    pub fn controller_host(&self) -> &str {
        &self.host
    }

    /// Get the controller port we're connected to
    pub fn controller_port(&self) -> u16 {
        self.port
    }

    /// Start a local HTTP proxy that forwards to the controller via SSH
    /// direct-tcpip
    ///
    /// This sets up a local TCP listener on the specified port. When
    /// connections come in, they are forwarded through the SSH connection
    /// to the controller's HTTP endpoint.
    ///
    /// This mimics the behavior of `ssh -L local_port:127.0.0.1:remote_port`
    /// The Atlas protocol uses: `-L 8080:127.0.0.1:8080` to forward local:8080
    /// to controller's 127.0.0.1:8080
    ///
    /// The `reconnect_signal` token will be cancelled if the proxy detects the
    /// SSH session is dead (e.g., consecutive channel open failures or
    /// timeouts). The caller should watch this token and reconnect when
    /// it's cancelled.
    pub async fn start_http_proxy(
        &self,
        local_port: u16,
        remote_port: u16,
        reconnect_signal: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicU32, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind(format!("127.0.0.1:{}", local_port)).await?;
        info!(
            "HTTP proxy started: localhost:{} -> controller:{}",
            local_port, remote_port
        );

        let session = self.session.clone();
        let remote_port = remote_port as u32;

        // Track consecutive failures to detect dead sessions
        let consecutive_failures = Arc::new(AtomicU32::new(0));
        const MAX_CONSECUTIVE_FAILURES: u32 = 3;

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((mut local_stream, peer_addr)) => {
                        info!("HTTP proxy forwarding connection from {}", peer_addr);

                        let session = session.clone();
                        let failures = consecutive_failures.clone();
                        let reconnect = reconnect_signal.clone();

                        tokio::spawn(async move {
                            // Open SSH direct-tcpip channel to controller's localhost:remote_port
                            debug!("Opening SSH channel for HTTP forward");
                            let session_guard = session.lock().await;

                            // Add timeout to channel open to detect dead SSH sessions
                            let channel_result = tokio::time::timeout(
                                std::time::Duration::from_secs(10),
                                session_guard.channel_open_direct_tcpip(
                                    "127.0.0.1",
                                    remote_port,
                                    "127.0.0.1",
                                    0,
                                ),
                            )
                            .await;

                            let mut channel = match channel_result {
                                Ok(Ok(ch)) => {
                                    debug!("SSH channel opened successfully");
                                    // Reset failure counter on success
                                    failures.store(0, Ordering::SeqCst);
                                    ch
                                }
                                Ok(Err(e)) => {
                                    error!("Failed to open SSH channel for HTTP forward: {}", e);
                                    let count = failures.fetch_add(1, Ordering::SeqCst) + 1;
                                    if count >= MAX_CONSECUTIVE_FAILURES {
                                        error!(
                                            "Too many channel failures ({}), signaling \
                                             reconnection needed",
                                            count
                                        );
                                        reconnect.cancel();
                                    }
                                    return;
                                }
                                Err(_) => {
                                    error!("Timeout opening SSH channel - SSH session may be dead");
                                    let count = failures.fetch_add(1, Ordering::SeqCst) + 1;
                                    if count >= MAX_CONSECUTIVE_FAILURES {
                                        error!(
                                            "Too many channel timeouts ({}), signaling \
                                             reconnection needed",
                                            count
                                        );
                                        reconnect.cancel();
                                    }
                                    return;
                                }
                            };
                            drop(session_guard);

                            // Bridge local stream with SSH channel
                            // Add timeouts to detect stuck connections
                            let mut local_buf = [0u8; 8192];
                            let mut bytes_sent = 0usize;
                            let mut bytes_received = 0usize;

                            loop {
                                tokio::select! {
                                    // Read from local -> send to SSH (with timeout)
                                    result = tokio::time::timeout(
                                        std::time::Duration::from_secs(30),
                                        local_stream.read(&mut local_buf)
                                    ) => {
                                        match result {
                                            Ok(Ok(0)) => {
                                                // Local connection closed (HTTP client disconnected/timed out)
                                                debug!("Local connection closed after sending {} bytes, receiving {} bytes", bytes_sent, bytes_received);
                                                let _ = channel.eof().await;

                                                // If we sent data but got no response, count as failure
                                                // This detects when SSH tunnel is open but controller isn't responding
                                                if bytes_sent > 0 && bytes_received == 0 {
                                                    warn!("HTTP request got no response (sent {} bytes) - SSH tunnel may be broken", bytes_sent);
                                                    let count = failures.fetch_add(1, Ordering::SeqCst) + 1;
                                                    if count >= MAX_CONSECUTIVE_FAILURES {
                                                        error!("Too many no-response failures ({}), signaling reconnection needed", count);
                                                        reconnect.cancel();
                                                    }
                                                }
                                                break;
                                            }
                                            Ok(Ok(n)) => {
                                                // Add timeout to SSH send
                                                match tokio::time::timeout(
                                                    std::time::Duration::from_secs(10),
                                                    channel.data(&local_buf[..n])
                                                ).await {
                                                    Ok(Ok(())) => {
                                                        bytes_sent += n;
                                                    }
                                                    Ok(Err(e)) => {
                                                        error!("Failed to send {} bytes to SSH channel: {}", n, e);
                                                        let count = failures.fetch_add(1, Ordering::SeqCst) + 1;
                                                        if count >= MAX_CONSECUTIVE_FAILURES {
                                                            error!("Too many send failures ({}), signaling reconnection needed", count);
                                                            reconnect.cancel();
                                                        }
                                                        break;
                                                    }
                                                    Err(_) => {
                                                        error!("Timeout sending data to SSH channel - connection stalled");
                                                        let count = failures.fetch_add(1, Ordering::SeqCst) + 1;
                                                        if count >= MAX_CONSECUTIVE_FAILURES {
                                                            error!("Too many send timeouts ({}), signaling reconnection needed", count);
                                                            reconnect.cancel();
                                                        }
                                                        break;
                                                    }
                                                }
                                            }
                                            Ok(Err(e)) => {
                                                error!("Local read error: {}", e);
                                                break;
                                            }
                                            Err(_) => {
                                                // Timeout on local read is normal if waiting for response
                                                // But if we've sent data and gotten no response, something is wrong
                                                if bytes_sent > 0 && bytes_received == 0 {
                                                    error!("Timeout waiting for SSH response after sending {} bytes", bytes_sent);
                                                    let count = failures.fetch_add(1, Ordering::SeqCst) + 1;
                                                    if count >= MAX_CONSECUTIVE_FAILURES {
                                                        error!("Too many response timeouts ({}), signaling reconnection needed", count);
                                                        reconnect.cancel();
                                                    }
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    // Read from SSH -> send to local
                                    msg = channel.wait() => {
                                        match msg {
                                            Some(ChannelMsg::Data { data }) => {
                                                bytes_received += data.len();
                                                // Reset failure counter on successful data receive
                                                failures.store(0, Ordering::SeqCst);
                                                if let Err(e) = local_stream.write_all(&data).await {
                                                    error!("Local write error: {}", e);
                                                    break;
                                                }
                                            }
                                            Some(ChannelMsg::Eof) | None => {
                                                debug!("SSH channel closed after sending {} bytes, receiving {} bytes", bytes_sent, bytes_received);
                                                break;
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        error!("HTTP proxy accept error: {}", e);
                    }
                }
            }
        });

        Ok(())
    }

    /// Execute a command and return output
    pub async fn execute(&self, command: &str) -> anyhow::Result<String> {
        self.execute_with_stdin(command, "").await
    }

    /// Execute a command with stdin data and return output
    pub async fn execute_with_stdin(
        &self,
        command: &str,
        stdin_data: &str,
    ) -> anyhow::Result<String> {
        let session = self.session.lock().await;

        // Check if session is still open
        if session.is_closed() {
            anyhow::bail!("SSH session is closed");
        }

        let mut channel = session
            .channel_open_session()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to open SSH channel: {}", e))?;

        channel
            .exec(true, command)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to execute command '{}': {}", command, e))?;

        // Send stdin data if provided
        if !stdin_data.is_empty() {
            channel
                .data(stdin_data.as_bytes())
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send stdin data: {}", e))?;
            channel
                .eof()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send EOF: {}", e))?;
        }

        let mut output = String::new();
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { ref data } => {
                    output.push_str(&String::from_utf8_lossy(data));
                }
                ChannelMsg::Eof => break,
                ChannelMsg::ExitStatus { exit_status } => {
                    if exit_status != 0 {
                        debug!("Command exited with status {}", exit_status);
                    }
                }
                _ => {}
            }
        }

        Ok(output)
    }

    /// Take the forwarded connection receiver
    /// This can only be called once - subsequent calls return None
    pub fn take_forward_receiver(&mut self) -> Option<ForwardedConnectionReceiver> {
        self.forward_rx
            .take()
            .map(|rx| ForwardedConnectionReceiver { rx })
    }

    /// Run keepalive loop
    pub async fn keepalive_loop(&self, interval: Duration) -> anyhow::Result<()> {
        loop {
            sleep(interval).await;

            if let Err(e) = self.keep().await {
                error!("Keepalive failed: {}", e);
                return Err(e);
            }
        }
    }

    /// Check if connection is still alive
    pub async fn is_connected(&self) -> bool {
        let session = self.session.lock().await;
        !session.is_closed()
    }

    /// Get the host we're connected to
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Get the port we're connected to
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Load SSH key from file
pub async fn load_key(path: &Path) -> anyhow::Result<KeyPair> {
    let key_data = tokio::fs::read(path).await?;
    let key = russh_keys::decode_secret_key(&String::from_utf8(key_data)?, None)?;
    Ok(key)
}

/// Generate a new SSH key pair
pub fn generate_key() -> anyhow::Result<KeyPair> {
    let key = KeyPair::generate_ed25519()
        .ok_or_else(|| anyhow::anyhow!("Failed to generate Ed25519 key"))?;
    Ok(key)
}

/// Save SSH key pair to files
pub async fn save_key(key: &KeyPair, path: &Path) -> anyhow::Result<()> {
    use russh_keys::PublicKeyBase64;

    // Create parent directory if needed
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // For Ed25519, we can extract the key and encode it using ssh-key crate
    let public_key = key.clone_public_key()?;

    // Save public key in OpenSSH format
    let pub_path = path.with_extension("pub");
    let pub_key_str = format!(
        "{} {} starla",
        public_key.name(),
        public_key.public_key_base64()
    );
    tokio::fs::write(&pub_path, pub_key_str.as_bytes()).await?;
    info!("Public key: {}", pub_key_str);

    // For the private key, we'll use OpenSSH format via ssh-key crate
    match key {
        KeyPair::Ed25519(signing_key) => {
            use ssh_key::private::Ed25519Keypair;
            use ssh_key::PrivateKey;

            // Extract the Ed25519 key bytes
            let secret_bytes = signing_key.to_bytes();
            let public_bytes = signing_key.verifying_key().to_bytes();

            // Create ssh-key Ed25519Keypair
            let keypair = Ed25519Keypair {
                public: ssh_key::public::Ed25519PublicKey(public_bytes),
                private: ssh_key::private::Ed25519PrivateKey::from_bytes(&secret_bytes),
            };

            let private_key = PrivateKey::from(keypair);
            let openssh_pem = private_key.to_openssh(ssh_key::LineEnding::LF)?;
            tokio::fs::write(path, openssh_pem.as_bytes()).await?;
        }
        _ => {
            // For other key types, we'd need different handling
            anyhow::bail!("Unsupported key type for saving");
        }
    }

    // Set restrictive permissions on private key (Unix only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(path).await?.permissions();
        perms.set_mode(0o600);
        tokio::fs::set_permissions(path, perms).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = SshConfig::default();
        assert_eq!(config.connect_timeout, Duration::from_secs(30));
        assert_eq!(config.keepalive_interval, Duration::from_secs(30));
    }

    #[test]
    fn test_generate_key() {
        let key = generate_key().unwrap();
        let public = key.clone_public_key().unwrap();
        // Just verify we can get the public key
        assert!(matches!(public, russh_keys::key::PublicKey::Ed25519(_)));
    }
}
