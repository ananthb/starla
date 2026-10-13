//! SSH connection management for RIPE Atlas controller communication
//!
//! This module provides:
//! - SSH tunnel management with automatic reconnection
//! - Registration protocol (INIT command)
//! - Keepalive handling (KEEP command)
//! - Reverse port forwarding for telnet interface

use russh::client::{self, Handle, Msg};
use russh::keys::ssh_key::{self, Algorithm};
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyBase64};
use russh::{Channel, ChannelMsg, Preferred};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};
use tracing::{debug, error, info, trace, warn};

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
#[derive(Debug, Clone, PartialEq)]
pub struct ControllerInfo {
    pub host: String,
    pub port: u16,
    pub probe_id: u32,
    /// `REREGISTER <secs>`: how long the assignment holds before the probe
    /// must ask the registration server again.
    pub rereg_secs: Option<u64>,
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
    /// `REFUSED` (the line after it is the reason): the reference probe waits
    /// and then registers again (atlasinit writes force_reg.sh).
    Refused { reason: String },
}

/// Probe information sent during registration INIT
#[derive(Debug, Clone)]
pub struct ProbeInitInfo {
    /// Firmware version (e.g., 5120)
    pub firmware_version: u32,
    /// Reason for registration (e.g., "NEW", "REREG_TIMER_EXPIRED")
    pub reason: String,
}

impl ProbeInitInfo {
    /// Create probe init info for a new registration
    pub fn new(firmware_version: u32) -> Self {
        Self {
            firmware_version,
            reason: "NEW".to_string(),
        }
    }

    /// Create probe init info for re-registration
    pub fn reregister(firmware_version: u32, reason: &str) -> Self {
        Self {
            firmware_version,
            reason: reason.to_string(),
        }
    }

    /// Format as P_TO_R_INIT message for INIT command stdin
    ///
    /// Uses the software probe format matching the official generic
    /// software probe: `TOKEN_SPECS fluffy 1000 <fw> <sub_arch>`
    pub fn to_init_message(&self) -> String {
        let mut msg = String::new();
        msg.push_str("P_TO_R_INIT\n");

        let sub_arch = detect_sub_arch();

        msg.push_str(&format!(
            "TOKEN_SPECS fluffy 1000 {} {}\n",
            self.firmware_version, sub_arch
        ));

        msg.push_str(&format!("REASON_FOR_REGISTRATION {}\n", self.reason));
        msg
    }
}

/// Which registration entry to try first: uniform over the list, as the
/// reference's reginit picks one with `rchoose $REG_1_HOST … $REG_6_HOST`.
///
/// The list is a hostname, its IPv4 literal and its IPv6 literal for each of
/// two servers, and a hostname resolves to IPv6 first on a dual-stack host,
/// so the reference registers over IPv4 one time in three. Starla walked
/// the list from the top, so it always registered over IPv6: on 2026-10-13
/// every connection RIPE had logged from probes 1018212 and 1015560
/// (msm 7000) was IPv6, and neither has an IPv4 address in RIPE's view.
fn random_start(len: usize) -> usize {
    use rand::RngExt;
    if len == 0 {
        0
    } else {
        rand::rng().random_range(0..len)
    }
}

/// The entries from `start`, then the ones before it: the first is the
/// reference's single choice, the rest are Starla's fallbacks.
fn from_random_start<T>(items: &[T], start: usize) -> impl Iterator<Item = &T> {
    let start = start.min(items.len());
    items[start..].iter().chain(items[..start].iter())
}

/// Detect the probe sub-architecture string sent during registration.
///
/// Format: `<os_id>/<os_version>/<arch>/starla/<starla_version>`,
/// e.g. `debian/13/x86_64/starla/0.3.0`. Mirrors the original C probe's
/// `get_sub_arch` (sourced `/etc/os-release`, `uname -m`) with
/// platform fallbacks so the same binary works on Linux, macOS, and Windows.
fn detect_sub_arch() -> String {
    let (id, version_id) = detect_os_id_version();
    let arch = std::env::consts::ARCH;
    let starla_version = env!("CARGO_PKG_VERSION");
    format!("{}/{}/{}/starla/{}", id, version_id, arch, starla_version)
}

fn detect_os_id_version() -> (String, String) {
    #[cfg(target_os = "linux")]
    {
        let (id, version_id) = read_os_release();
        return (
            id.unwrap_or_else(|| "generic".to_string()),
            version_id.unwrap_or_else(|| "unknown".to_string()),
        );
    }

    #[cfg(target_os = "macos")]
    {
        let version = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        return ("macos".to_string(), version);
    }

    #[cfg(target_os = "windows")]
    return ("windows".to_string(), "unknown".to_string());

    #[allow(unreachable_code)]
    ("generic".to_string(), "unknown".to_string())
}

/// Parse `/etc/os-release`, returning `ID` and `VERSION_ID` independently.
/// Each is `None` if the file is unreadable or the line is absent, letting
/// the caller apply per-field defaults (matching the original Bash, which
/// pre-set ID=generic / VERSION_ID=unknown before sourcing the file).
#[cfg(target_os = "linux")]
fn read_os_release() -> (Option<String>, Option<String>) {
    let Ok(content) = std::fs::read_to_string("/etc/os-release") else {
        return (None, None);
    };
    let mut id = None;
    let mut version_id = None;
    for line in content.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        match key.trim() {
            "ID" => id = Some(value.to_string()),
            "VERSION_ID" => version_id = Some(value.to_string()),
            _ => {}
        }
    }
    (id, version_id)
}

/// Known SSH host keys for server verification (TOFU model)
///
/// On first connection to a server, the key is saved to a known_hosts file.
/// On subsequent connections, the presented key is verified against the saved
/// one. This prevents MITM attacks after the initial connection.
#[derive(Clone)]
pub struct KnownHosts {
    path: PathBuf,
    hosts: Arc<Mutex<HashMap<String, String>>>,
}

impl KnownHosts {
    /// Load known hosts from file, or create empty if file doesn't exist
    pub fn load(path: &Path) -> Self {
        let mut hosts = HashMap::new();

        if path.exists() {
            if let Ok(contents) = std::fs::read_to_string(path) {
                for line in contents.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    // Format: "host:port key_type base64_key"
                    let parts: Vec<&str> = line.splitn(3, ' ').collect();
                    if parts.len() == 3 {
                        let host_port = parts[0].to_string();
                        let key_str = format!("{} {}", parts[1], parts[2]);
                        hosts.insert(host_port, key_str);
                    }
                }
            }
        }

        Self {
            path: path.to_path_buf(),
            hosts: Arc::new(Mutex::new(hosts)),
        }
    }

    /// Check a server's public key against known hosts.
    /// Returns Ok(true) if the key matches or was newly saved (TOFU).
    /// Returns Ok(false) if the key does NOT match a previously saved key
    /// (possible MITM).
    pub async fn verify(
        &self,
        host: &str,
        port: u16,
        key: &PublicKey,
    ) -> Result<bool, anyhow::Error> {
        let host_port = format!("{}:{}", host, port);
        let key_algo = key.algorithm();
        let key_type = key_algo.as_str();
        let key_b64 = key.public_key_base64();
        let presented = format!("{} {}", key_type, key_b64);

        let mut hosts = self.hosts.lock().await;

        if let Some(saved) = hosts.get(&host_port) {
            // Match on key blob only: RFC 8332 reuses the ssh-rsa blob for
            // rsa-sha2-{256,512}.
            let saved_blob = saved.split_whitespace().nth(1).unwrap_or("");
            if saved_blob == key_b64 {
                debug!("Host key for {} matches known key", host_port);
                Ok(true)
            } else {
                error!(
                    "HOST KEY MISMATCH for {}! Possible MITM attack.\nExpected: {}\nGot:      {}",
                    host_port, saved, presented
                );
                Ok(false)
            }
        } else {
            // TOFU: first time seeing this host, save the key.
            //
            // Debug, not info: this fires once per registration server on
            // first run -- six lines with the default server list -- which
            // is exactly when the log is also carrying the key the user
            // has to go and register. A first sighting is routine; the
            // mismatch above is the security event, and that stays loud.
            debug!(
                "New host key for {} ({}), saving to known_hosts (TOFU)",
                host_port, key_type
            );
            hosts.insert(host_port.clone(), presented);

            // Write back to file atomically (write temp then rename)
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
                let tmp_path = parent.join(".known_hosts.tmp");
                let mut lines: Vec<String> = Vec::new();
                lines.push("# Starla known hosts - do not edit manually".to_string());
                for (hp, k) in hosts.iter() {
                    lines.push(format!("{} {}", hp, k));
                }
                match std::fs::write(&tmp_path, lines.join("\n") + "\n") {
                    Ok(()) => {
                        if let Err(e) = std::fs::rename(&tmp_path, &self.path) {
                            warn!("Failed to rename known_hosts: {}", e);
                            let _ = std::fs::remove_file(&tmp_path);
                        }
                    }
                    Err(e) => warn!("Failed to save known_hosts: {}", e),
                }
            }

            Ok(true)
        }
    }
}

/// Client handler for russh
struct AtlasClientHandler {
    /// Known hosts for server key verification
    known_hosts: KnownHosts,
    /// The host we're connecting to (for key verification)
    connect_host: String,
    /// The port we're connecting to
    connect_port: u16,
    /// Command sender for telnet handler (forwarded connections go here
    /// directly)
    command_tx: Option<tokio::sync::mpsc::Sender<crate::telnet::TelnetCommand>>,
    /// Probe ID for telnet authentication
    probe_id: u32,
    /// Session ID for telnet authentication
    session_id: Arc<tokio::sync::RwLock<Option<String>>>,
}

impl client::Handler for AtlasClientHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        self.known_hosts
            .verify(&self.connect_host, self.connect_port, server_public_key)
            .await
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        debug!(
            "Forwarded connection from {}:{} to {}:{}",
            originator_address, originator_port, connected_address, connected_port
        );

        // Convert SSH channel to an async stream and handle directly :
        // no local TCP port needed
        let stream = crate::channel_stream::channel_to_stream(channel);
        let command_tx = self.command_tx.clone();
        let probe_id = self.probe_id;
        let session_id = self.session_id.clone();

        tokio::spawn(async move {
            if let Err(e) =
                crate::telnet::handle_connection(stream, command_tx, probe_id, session_id).await
            {
                error!("Error handling forwarded telnet connection: {}", e);
            }
            debug!("Forwarded telnet connection ended");
        });

        Ok(())
    }
}

/// State needed for handling telnet connections directly in the SSH handler
#[derive(Clone)]
pub struct TelnetState {
    pub command_tx: tokio::sync::mpsc::Sender<crate::telnet::TelnetCommand>,
    pub probe_id: u32,
    pub session_id: Arc<tokio::sync::RwLock<Option<String>>>,
}

/// The russh client configuration for every registration-server and
/// controller connection.
///
/// RIPE's registration servers offer diffie-hellman-group1-sha1 and
/// diffie-hellman-group-exchange-sha256 (plus ext-info-s and
/// kex-strict-s-v00@openssh.com; read from reg03 and reg-05 on 2026-10-10).
/// russh's default list includes group exchange, so the 1024-bit group1 is
/// never needed. Keep the default whole: a custom list also drops the
/// ext-info-c and kex-strict-c markers, and with them the Terrapin
/// (CVE-2023-48795) countermeasure.
fn client_config(config: &SshConfig) -> client::Config {
    client::Config {
        inactivity_timeout: Some(config.inactivity_timeout),
        keepalive_interval: Some(config.keepalive_interval),
        preferred: Preferred::DEFAULT,
        ..Default::default()
    }
}

/// SSH connection to RIPE Atlas controller
pub struct SshConnection {
    session: Arc<Mutex<Handle<AtlasClientHandler>>>,
    host: String,
    port: u16,
}

impl SshConnection {
    /// Connect to a controller server
    ///
    /// If `telnet_state` is provided, forwarded SSH connections (reverse
    /// tunnel) will be handled directly by the telnet command parser
    /// without a local TCP listener.
    pub async fn connect(
        host: &str,
        port: u16,
        key: &PrivateKey,
        config: SshConfig,
        known_hosts: KnownHosts,
        telnet_state: Option<TelnetState>,
    ) -> anyhow::Result<Self> {
        let ssh_config = client_config(&config);

        let (command_tx, probe_id, session_id) = match telnet_state {
            Some(ts) => (Some(ts.command_tx), ts.probe_id, ts.session_id),
            None => (None, 0, Arc::new(tokio::sync::RwLock::new(None))),
        };

        let handler = AtlasClientHandler {
            known_hosts: known_hosts.clone(),
            connect_host: host.to_string(),
            connect_port: port,
            command_tx,
            probe_id,
            session_id,
        };

        let addr = format!("{}:{}", host, port);
        debug!("Connecting to SSH controller at {}", addr);

        let session = timeout(
            config.connect_timeout,
            client::connect(Arc::new(ssh_config), addr, handler),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Connection timeout"))??;

        let mut session = session;

        // Authenticate. Ed25519 keys don't need an RSA hash; pass None.
        let auth_res = session
            .authenticate_publickey(
                "atlas",
                PrivateKeyWithHashAlg::new(Arc::new(key.clone()), None),
            )
            .await?;

        if !auth_res.success() {
            anyhow::bail!("SSH authentication failed");
        }

        debug!("SSH authentication successful");

        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            host: host.to_string(),
            port,
        })
    }

    /// Connect with automatic retry
    pub async fn connect_with_retry(
        host: &str,
        port: u16,
        key: &PrivateKey,
        config: SshConfig,
        known_hosts: KnownHosts,
        telnet_state: Option<TelnetState>,
    ) -> anyhow::Result<Self> {
        let mut attempts = 0u32;
        let mut delay = config.reconnect_delay;

        loop {
            attempts += 1;

            match Self::connect(
                host,
                port,
                key,
                config.clone(),
                known_hosts.clone(),
                telnet_state.clone(),
            )
            .await
            {
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
        key: &PrivateKey,
        config: SshConfig,
        known_hosts: KnownHosts,
    ) -> anyhow::Result<Self> {
        fn parse_server(server: &str) -> (&str, u16) {
            if let Some(rest) = server.strip_prefix('[') {
                if let Some((host, tail)) = rest.split_once(']') {
                    if let Some(port_str) = tail.strip_prefix(':') {
                        if let Ok(port) = port_str.parse() {
                            return (host, port);
                        }
                    }
                    return (host, 443);
                }
            }

            if let Some((host, port_str)) = server.rsplit_once(':') {
                if let Ok(port) = port_str.parse() {
                    return (host, port);
                }
            }

            (server, 443)
        }

        let mut last_error: Option<anyhow::Error> = None;

        for server in from_random_start(servers, random_start(servers.len())) {
            let (host, port) = parse_server(server);

            match Self::connect(host, port, key, config.clone(), known_hosts.clone(), None).await {
                Ok(conn) => {
                    info!("Connected to {}", server);
                    return Ok(conn);
                }
                Err(e) => {
                    // Debug, not warn: the default server list is six
                    // entries (hostname, IPv4 and IPv6 for each of two
                    // registration servers), so a single unreachable
                    // network warned six lines per retry, forever, and
                    // buried the one thing the user needs to read -- the
                    // key to register. The caller sees the summary below
                    // and decides how loudly to say it.
                    debug!("Failed to connect to {}: {}", server, e);
                    last_error = Some(e);
                }
            }
        }

        match last_error {
            Some(e) => Err(e.context(format!(
                "failed to connect to any of {} registration servers",
                servers.len()
            ))),
            None => anyhow::bail!("no registration servers configured"),
        }
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

        parse_init_response(&output)
    }
}

/// Parse a registration server's or controller's INIT reply.
pub fn parse_init_response(output: &str) -> anyhow::Result<InitResponse> {
    {
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
                // Parse all registration response fields before returning
                let mut controller_host: Option<String> = None;
                let mut controller_port: Option<u16> = None;
                let mut probe_id: u32 = 0;
                let mut rereg_secs: Option<u64> = None;

                for line in lines.iter().skip(1) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.is_empty() {
                        continue;
                    }

                    if parts[0] == "CONTROLLER" && parts.len() >= 4 {
                        controller_host = Some(parts[1].to_string());
                        controller_port = parts[2].parse().ok();
                    }

                    if parts[0] == "REREGISTER" && parts.len() >= 2 {
                        rereg_secs = parts[1].parse().ok();
                    }

                    if parts[0] == "PROBE_ID" && parts.len() >= 2 {
                        if let Ok(id) = parts[1].parse() {
                            probe_id = id;
                            debug!("Got probe ID: {}", id);
                        }
                    }
                }

                if let (Some(host), Some(port)) = (controller_host, controller_port) {
                    debug!("Got controller: {}:{}", host, port);
                    return Ok(InitResponse::Controller(ControllerInfo {
                        host,
                        port,
                        probe_id,
                        rereg_secs,
                    }));
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
            "REFUSED" => {
                let reason = lines.get(1).map(|l| l.trim()).unwrap_or("").to_string();
                debug!("Server refused us: {}", reason);
                Ok(InitResponse::Refused { reason })
            }
            _ => {
                anyhow::bail!("Unknown INIT response: {}", output);
            }
        }
    }
}

impl SshConnection {
    /// Start the KEEP session and monitor it.
    ///
    /// Opens a channel with the KEEP command and blocks until the channel
    /// closes (which means the controller disconnected). Returns when the
    /// connection is lost. The caller should use this as the connection
    /// health signal.
    pub async fn run_keep_session(&self) -> anyhow::Result<()> {
        debug!("Starting KEEP session");
        let session = self.session.lock().await;
        let mut channel = session.channel_open_session().await?;
        channel.exec(true, "KEEP").await?;
        drop(session); // Release lock so other operations can use the session

        debug!("KEEP session started, monitoring channel");

        // Block until the KEEP channel closes: this is our connection health signal
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    trace!("KEEP channel data: {} bytes", data.len());
                }
                ChannelMsg::Eof => {
                    debug!("KEEP channel EOF: connection lost");
                    break;
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    debug!("KEEP channel exit status: {}", exit_status);
                }
                _ => {}
            }
        }

        warn!("KEEP session ended: controller disconnected");
        anyhow::bail!("KEEP session ended")
    }

    /// Request reverse port forwarding
    pub async fn request_reverse_tunnel(&self, bind_port: u16) -> anyhow::Result<()> {
        debug!("Requesting reverse tunnel on port {}", bind_port);

        let session = self.session.lock().await;

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
        debug!(
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
                        debug!(
                            "HTTP proxy accepted connection from {} (local:{} -> remote:{})",
                            peer_addr, local_port, remote_port
                        );

                        let session = session.clone();
                        let failures = consecutive_failures.clone();
                        let reconnect = reconnect_signal.clone();

                        tokio::spawn(async move {
                            // Open SSH direct-tcpip channel to controller's localhost:remote_port
                            debug!("Opening SSH channel for HTTP forward");
                            let session_guard = session.lock().await;
                            if session_guard.is_closed() {
                                error!(
                                    "SSH session closed before opening HTTP channel (local:{} -> \
                                     remote:{})",
                                    local_port, remote_port
                                );
                                let count = failures.fetch_add(1, Ordering::SeqCst) + 1;
                                if count >= MAX_CONSECUTIVE_FAILURES {
                                    error!(
                                        "Too many channel failures ({}), signaling reconnection \
                                         needed",
                                        count
                                    );
                                    reconnect.cancel();
                                }
                                return;
                            }

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

                            // Bridge local stream with SSH channel.
                            // Supports TCP half-close: when the local client finishes
                            // sending (read returns 0), we send EOF to the SSH side
                            // but keep reading the SSH response back to the client.
                            let mut local_buf = [0u8; 8192];
                            let mut local_done = false;

                            loop {
                                tokio::select! {
                                    biased; // Prioritize SSH data over local reads

                                    // Read from SSH -> send to local
                                    msg = channel.wait() => {
                                        match msg {
                                            Some(ChannelMsg::Data { data }) => {
                                                failures.store(0, Ordering::SeqCst);
                                                if let Err(e) = local_stream.write_all(&data).await {
                                                    debug!("Local write error: {}", e);
                                                    break;
                                                }
                                            }
                                            Some(ChannelMsg::Eof) | None => {
                                                debug!("SSH channel closed");
                                                break;
                                            }
                                            _ => {}
                                        }
                                    }

                                    // Read from local -> send to SSH
                                    result = local_stream.read(&mut local_buf), if !local_done => {
                                        match result {
                                            Ok(0) => {
                                                // Local finished sending: half-close the SSH side
                                                // but keep looping to read the response
                                                let _ = channel.eof().await;
                                                local_done = true;
                                            }
                                            Ok(n) => {
                                                if let Err(e) = channel.data(&local_buf[..n]).await {
                                                    debug!("SSH write error: {}", e);
                                                    let count = failures.fetch_add(1, Ordering::SeqCst) + 1;
                                                    if count >= MAX_CONSECUTIVE_FAILURES {
                                                        reconnect.cancel();
                                                    }
                                                    break;
                                                }
                                            }
                                            Err(e) => {
                                                debug!("Local read error: {}", e);
                                                break;
                                            }
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
                ChannelMsg::ExitStatus { exit_status } if exit_status != 0 => {
                    debug!("Command exited with status {}", exit_status);
                }
                _ => {}
            }
        }

        Ok(output)
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

/// Compute the SHA256 fingerprint of a public key (e.g., "SHA256:abc123...")
pub fn key_fingerprint(key: &PrivateKey) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};

    let public_key = key.public_key();
    let key_algo = public_key.algorithm();
    let key_type = key_algo.as_str();
    let key_b64 = public_key.public_key_base64();

    // The SSH fingerprint is SHA256 of the raw public key wire format
    // (type string length + type string + key data), which is what base64 decodes
    // to
    use base64::Engine;
    let raw_bytes = base64::engine::general_purpose::STANDARD.decode(&key_b64)?;
    let hash = Sha256::digest(&raw_bytes);
    let fingerprint = base64::engine::general_purpose::STANDARD_NO_PAD.encode(hash);

    Ok(format!("{} SHA256:{}", key_type, fingerprint))
}

/// Load SSH key from file
pub async fn load_key(path: &Path) -> anyhow::Result<PrivateKey> {
    let key_data = tokio::fs::read(path).await?;
    let key = russh::keys::decode_secret_key(&String::from_utf8(key_data)?, None)?;
    Ok(key)
}

/// Load SSH key pair from a PEM string (e.g. from an environment variable)
pub fn load_key_from_string(pem: &str) -> anyhow::Result<PrivateKey> {
    let key = russh::keys::decode_secret_key(pem, None)?;
    Ok(key)
}

/// Generate a new SSH key pair
pub fn generate_key() -> anyhow::Result<PrivateKey> {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    Ok(key)
}

/// The probe's public key in OpenSSH `authorized_keys` form.
///
/// This is the exact string RIPE Atlas wants pasted into the registration
/// form, and the one written to `probe_key.pub`. Derived from the private
/// key rather than read back from that file, so it is also available when
/// the key arrived through `STARLA_SSH_KEY` or a systemd credential --
/// neither of which writes a `.pub` file, which is why those two key
/// sources previously left the user with no way to see the key at all.
pub fn public_key_openssh(key: &PrivateKey) -> String {
    let public_key = key.public_key();
    format!(
        "{} {} starla",
        public_key.algorithm().as_str(),
        public_key.public_key_base64()
    )
}

/// Save SSH key pair to files
pub async fn save_key(key: &PrivateKey, path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Save public key in OpenSSH format: "<algo> <base64> starla"
    let pub_path = path.with_extension("pub");
    let pub_key_str = public_key_openssh(key);
    tokio::fs::write(&pub_path, pub_key_str.as_bytes()).await?;
    debug!("Public key: {}", pub_key_str);

    // Save private key in OpenSSH format. It is created 0600 from the first
    // byte (never written under the umask and chmodded afterwards, which left
    // a window -- or, after a crash, a key -- readable by others) and renamed
    // into place, so a reader never sees a half-written key.
    let openssh_pem = key.to_openssh(ssh_key::LineEnding::LF)?;
    write_private_file(path, openssh_pem.as_bytes())?;

    Ok(())
}

/// Write `contents` to `path` through a `0600` temp file and a rename.
fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

/// Load the probe key from `path`, or create it there if there is none.
///
/// Returns the key and whether it was just generated. The key IS the probe's
/// identity at RIPE Atlas: a new one registers as a different probe, with
/// none of the old one's history or credits. So a key file that exists but
/// cannot be read or parsed is an error -- it is never replaced -- and so is
/// a new key that cannot be saved, since an unsaved key would change on
/// every restart.
pub async fn load_or_create_key(path: &Path) -> anyhow::Result<(PrivateKey, bool)> {
    let exists = path.try_exists().map_err(|e| {
        anyhow::anyhow!(
            "cannot check for the probe key at {}: {}",
            path.display(),
            e
        )
    })?;
    if exists {
        let key = load_key(path).await.map_err(|e| {
            anyhow::anyhow!(
                "the probe key at {} exists but cannot be loaded ({}). Refusing to replace it: \
                 the key is this probe's identity at RIPE Atlas. Restore or fix the file, or move \
                 it aside deliberately to start a new probe",
                path.display(),
                e
            )
        })?;
        return Ok((key, false));
    }
    let key = generate_key()?;
    save_key(&key, path).await.map_err(|e| {
        anyhow::anyhow!(
            "generated a probe key but cannot save it to {} ({}); an unsaved key would change at \
             every restart",
            path.display(),
            e
        )
    })?;
    Ok((key, true))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn public_key_openssh_matches_the_pub_file() {
        // The banner shows this string and probe_key.pub holds it; if the
        // two ever drift, a user registers one key and the probe presents
        // another, which fails as an opaque auth error at RIPE.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe_key");
        let key = generate_key().unwrap();

        save_key(&key, &path).await.unwrap();

        let written = std::fs::read_to_string(path.with_extension("pub")).unwrap();
        assert_eq!(written, public_key_openssh(&key));
    }

    #[test]
    fn public_key_openssh_is_authorized_keys_shaped() {
        let key = generate_key().unwrap();
        let rendered = public_key_openssh(&key);
        let mut parts = rendered.split(' ');

        assert_eq!(parts.next(), Some("ssh-ed25519"));
        assert!(!parts.next().unwrap().is_empty(), "base64 body");
        assert_eq!(parts.next(), Some("starla"));
        assert_eq!(parts.next(), None);
        assert!(!rendered.contains('\n'), "must stay on one line");
    }

    use super::*;

    #[test]
    fn key_exchange_never_offers_group1_and_keeps_strict_kex() {
        let cfg = client_config(&SshConfig::default());
        let kex = &cfg.preferred.kex;
        assert!(
            !kex.contains(&russh::kex::DH_G1_SHA1),
            "1024-bit diffie-hellman-group1-sha1 must not be offered"
        );
        assert!(
            kex.contains(&russh::kex::DH_GEX_SHA256),
            "RIPE's servers need group exchange"
        );
        assert!(
            kex.contains(&russh::kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT),
            "dropping the strict-kex marker disables the Terrapin countermeasure"
        );
    }

    #[test]
    fn test_default_config() {
        let config = SshConfig::default();
        assert_eq!(config.connect_timeout, Duration::from_secs(30));
        assert_eq!(config.keepalive_interval, Duration::from_secs(30));
    }

    #[test]
    fn test_generate_key() {
        let key = generate_key().unwrap();
        let public = key.public_key();
        // Just verify we can get the public key
        assert_eq!(public.algorithm(), Algorithm::Ed25519);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_private_key_is_written_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe_key");
        save_key(&generate_key().unwrap(), &path).await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(
            !path.with_extension("tmp").exists(),
            "temp file renamed away"
        );
    }

    #[tokio::test]
    async fn a_missing_key_is_created_and_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe_key");
        let (first, generated) = load_or_create_key(&path).await.unwrap();
        assert!(generated);
        let (again, generated) = load_or_create_key(&path).await.unwrap();
        assert!(!generated);
        assert_eq!(
            first.public_key().public_key_base64(),
            again.public_key().public_key_base64()
        );
    }

    #[tokio::test]
    async fn an_unreadable_key_is_an_error_and_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe_key");
        std::fs::write(&path, "not a key\n").unwrap();
        let err = load_or_create_key(&path).await.unwrap_err().to_string();
        assert!(err.contains("Refusing to"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a key\n");
        assert!(!path.with_extension("pub").exists());
    }

    fn tmp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "starla-kh-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[tokio::test]
    async fn test_verify_matches_on_blob_across_algorithm_names() {
        let path = tmp_path("xalgo");
        let kh = KnownHosts::load(&path);

        let priv_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let pub_key = priv_key.public_key();
        let blob = pub_key.public_key_base64();

        kh.hosts
            .lock()
            .await
            .insert("atlas.example.com:443".into(), format!("ssh-rsa {}", blob));

        let ok = kh.verify("atlas.example.com", 443, pub_key).await.unwrap();
        assert!(ok, "blob match should win over algorithm-prefix difference");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_verify_rejects_different_blob() {
        let path = tmp_path("mitm");
        let kh = KnownHosts::load(&path);

        let pinned = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let attacker = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();

        kh.hosts.lock().await.insert(
            "atlas.example.com:443".into(),
            format!("ssh-ed25519 {}", pinned.public_key().public_key_base64()),
        );

        let ok = kh
            .verify("atlas.example.com", 443, attacker.public_key())
            .await
            .unwrap();
        assert!(!ok, "verify must reject a different key blob");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_verify_tofu_on_first_sight() {
        let path = tmp_path("tofu");
        let kh = KnownHosts::load(&path);

        let priv_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let ok = kh
            .verify("atlas.example.com", 443, priv_key.public_key())
            .await
            .unwrap();
        assert!(ok, "first sight should TOFU-trust the key");

        let ok = kh
            .verify("atlas.example.com", 443, priv_key.public_key())
            .await
            .unwrap();
        assert!(ok, "subsequent verifications with the same key must match");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn registration_starts_at_a_random_entry_and_keeps_the_rest_as_fallbacks() {
        let servers = [
            "reg-05",
            "193.0.19.75",
            "[2001::b]",
            "reg-06",
            "193.0.19.76",
            "[2001::c]",
        ];
        let order: Vec<_> = from_random_start(&servers, 4).copied().collect();
        assert_eq!(
            order,
            [
                "193.0.19.76",
                "[2001::c]",
                "reg-05",
                "193.0.19.75",
                "[2001::b]",
                "reg-06"
            ]
        );
        assert_eq!(from_random_start(&servers, 0).count(), 6);
        assert_eq!(from_random_start::<&str>(&[], 0).count(), 0);

        // Uniform like rchoose: every entry comes first, the IPv4 literals
        // about one time in three between them.
        let mut first = [0usize; 6];
        for _ in 0..6000 {
            first[random_start(6)] += 1;
        }
        assert!(first.iter().all(|&n| (800..1200).contains(&n)), "{first:?}");
        let v4 = first[1] + first[4];
        assert!((1700..2300).contains(&v4), "{v4}");
    }
}
