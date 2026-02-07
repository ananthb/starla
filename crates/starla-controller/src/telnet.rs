//! Telnet command interface for receiving measurement requests
//!
//! The RIPE Atlas controller sends measurement specifications over the
//! reverse-tunneled telnet connection. This module handles parsing and
//! dispatching these commands.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, error, trace, warn};

/// Telnet command received from controller
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TelnetCommand {
    /// Ping measurement
    Ping(PingSpec),
    /// Traceroute measurement
    Traceroute(TracerouteSpec),
    /// DNS measurement
    Dns(DnsSpec),
    /// HTTP measurement
    Http(HttpSpec),
    /// TLS/SSL measurement
    Tls(TlsSpec),
    /// NTP measurement
    Ntp(NtpSpec),
    /// Status query
    Status,
    /// Stop a running measurement
    Stop(u64),
    /// Ignored command (known but not handled, e.g., CRONTAB, internal
    /// commands)
    Ignored(String),
    /// Unknown command
    Unknown(String),
}

/// Common scheduling fields for recurring measurements
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScheduleSpec {
    /// Interval between measurements in seconds (0 = one-shot)
    pub interval: u64,
    /// Start time (Unix timestamp, 0 = now)
    pub start_time: i64,
    /// Stop time (Unix timestamp, 0 = never)
    pub stop_time: i64,
}

/// Ping measurement specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingSpec {
    pub msm_id: u64,
    pub target: String,
    pub af: u8, // 4 or 6
    pub packets: u32,
    pub size: u16,
    pub packet_interval: u32, // ms between packets
    pub spread: Option<u32>,  // random spread time in seconds
    pub schedule: ScheduleSpec,
}

/// Traceroute measurement specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracerouteSpec {
    pub msm_id: u64,
    pub target: String,
    pub af: u8,
    pub protocol: String, // ICMP, UDP, TCP
    pub paris: Option<u32>,
    pub first_hop: u8,
    pub max_hops: u8,
    pub size: u16,
    pub spread: Option<u32>,
    pub schedule: ScheduleSpec,
}

/// DNS measurement specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsSpec {
    pub msm_id: u64,
    pub target: String, // DNS server
    pub af: u8,
    pub protocol: String, // UDP, TCP
    pub query_type: String,
    pub query_class: String,
    pub query_argument: String,
    pub use_dnssec: bool,
    pub recursion_desired: bool,
    pub spread: Option<u32>,
    pub schedule: ScheduleSpec,
}

/// HTTP measurement specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpSpec {
    pub msm_id: u64,
    pub url: String,
    pub method: String,
    pub af: u8,
    pub headers: Vec<String>,
    pub body: Option<String>,
    pub max_body_size: Option<u32>,
    pub spread: Option<u32>,
    pub schedule: ScheduleSpec,
}

/// TLS/SSL measurement specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsSpec {
    pub msm_id: u64,
    pub target: String,
    pub port: u16,
    pub af: u8,
    pub hostname: Option<String>,
    pub spread: Option<u32>,
    pub schedule: ScheduleSpec,
}

/// NTP measurement specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtpSpec {
    pub msm_id: u64,
    pub target: String,
    pub af: u8,
    pub packets: u32,
    pub spread: Option<u32>,
    pub schedule: ScheduleSpec,
}

/// Telnet server for receiving controller commands
pub struct TelnetServer {
    port: u16,
    probe_id: u32,
    session_id: std::sync::Arc<tokio::sync::RwLock<Option<String>>>,
    command_tx: Option<mpsc::Sender<TelnetCommand>>,
}

// RIPE Atlas telnet protocol constants
const ATLAS_LOGIN: &str = "C_TO_P_TEST_V1";
const LOGIN_PREFIX: &str = "Atlas probe, see http://atlas.ripe.net/\r\n\r\n";
const LOGIN_PROMPT: &str = " login: ";
const PASSWORD_PROMPT: &str = "\r\nPassword: ";
const RESULT_OK: &str = "OK\r\n\r\n";
const BAD_PASSWORD: &str = "BAD_PASSWORD\r\n\r\n";
#[allow(dead_code)]
const BAD_COMMAND: &str = "BAD_COMMAND\r\n\r\n";

impl TelnetServer {
    /// Create a new telnet server
    pub fn new(port: u16, probe_id: u32) -> Self {
        Self {
            port,
            probe_id,
            session_id: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            command_tx: None,
        }
    }

    /// Create a new telnet server with command channel
    pub fn with_channel(port: u16, probe_id: u32, tx: mpsc::Sender<TelnetCommand>) -> Self {
        Self {
            port,
            probe_id,
            session_id: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            command_tx: Some(tx),
        }
    }

    /// Set the session ID (received from controller INIT)
    pub async fn set_session_id(&self, session_id: String) {
        debug!("Setting session ID for telnet authentication");
        *self.session_id.write().await = Some(session_id);
    }

    /// Get the port
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Run the telnet server
    pub async fn run(&self) -> anyhow::Result<()> {
        let addr = format!("127.0.0.1:{}", self.port);
        let listener = TcpListener::bind(&addr).await?;
        debug!("Telnet server listening on {}", addr);

        loop {
            let (socket, remote_addr) = listener.accept().await?;
            debug!("Accepted telnet connection from {}", remote_addr);

            let command_tx = self.command_tx.clone();
            let probe_id = self.probe_id;
            let session_id = self.session_id.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_connection(socket, command_tx, probe_id, session_id).await {
                    error!("Error handling telnet connection: {}", e);
                }
                debug!("Telnet connection from {} closed", remote_addr);
            });
        }
    }
}

// Telnet protocol constants
const IAC: u8 = 255; // Interpret As Command
const WILL: u8 = 251;
const WONT: u8 = 252;
const DO: u8 = 253;
const DONT: u8 = 254;
const SB: u8 = 250; // Subnegotiation Begin
const SE: u8 = 240; // Subnegotiation End

// Telnet options
const TELOPT_ECHO: u8 = 1;
const TELOPT_SGA: u8 = 3; // Suppress Go Ahead
const TELOPT_NAWS: u8 = 31; // Negotiate About Window Size

/// Connection state for the Atlas telnet protocol
#[derive(Debug, Clone, Copy, PartialEq)]
enum ConnectionState {
    /// Waiting for login name
    AwaitingLoginName,
    /// Waiting for password (session ID)
    AwaitingPassword,
    /// Authenticated, ready for commands
    Authenticated,
}

/// Handle a single telnet connection with Atlas authentication
async fn handle_connection(
    socket: TcpStream,
    command_tx: Option<mpsc::Sender<TelnetCommand>>,
    probe_id: u32,
    session_id: std::sync::Arc<tokio::sync::RwLock<Option<String>>>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = socket.into_split();

    // Send initial telnet negotiation (like busybox telnetd does)
    let initial_iacs: &[u8] = &[
        IAC,
        DO,
        TELOPT_ECHO, // Ask client to echo
        IAC,
        DO,
        TELOPT_NAWS, // Ask client about window size
        IAC,
        WILL,
        TELOPT_ECHO, // We will echo
        IAC,
        WILL,
        TELOPT_SGA, // We will suppress go-ahead
    ];

    if let Err(e) = writer.write_all(initial_iacs).await {
        warn!("Failed to send telnet negotiation: {}", e);
    }

    // Send the Atlas login banner
    let hostname = hostname::get()
        .map(|h: std::ffi::OsString| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let banner = format!(
        "{}Probe {} ({}){}",
        LOGIN_PREFIX, probe_id, hostname, LOGIN_PROMPT
    );
    if let Err(e) = writer.write_all(banner.as_bytes()).await {
        warn!("Failed to send login banner: {}", e);
    }
    let _ = writer.flush().await;

    trace!("Sent telnet login banner for probe {}", probe_id);

    let mut reader = BufReader::new(reader);
    let mut raw_buf = [0u8; 4096];
    let mut state = ConnectionState::AwaitingLoginName;
    let mut line_buffer = String::new();

    loop {
        use tokio::io::AsyncReadExt;
        match reader.read(&mut raw_buf).await {
            Ok(0) => break, // EOF
            Ok(n) => {
                // Filter out telnet IAC sequences and extract text
                let text = filter_telnet_commands(&raw_buf[..n]);
                if text.is_empty() {
                    continue;
                }

                // Accumulate into line buffer
                line_buffer.push_str(&text);

                // Process complete lines
                while let Some(newline_pos) = line_buffer.find(['\n', '\r']) {
                    let line: String = line_buffer.drain(..=newline_pos).collect();
                    let line = line.trim();

                    if line.is_empty() {
                        continue;
                    }

                    debug!("Telnet state {:?}, received: '{}'", state, line);

                    match state {
                        ConnectionState::AwaitingLoginName => {
                            if line == ATLAS_LOGIN {
                                // Valid Atlas login, ask for password
                                trace!("Atlas login received, requesting password");
                                if let Err(e) = writer.write_all(PASSWORD_PROMPT.as_bytes()).await {
                                    warn!("Failed to send password prompt: {}", e);
                                }
                                let _ = writer.flush().await;
                                state = ConnectionState::AwaitingPassword;
                            } else {
                                // Echo back the login name (traditional mode - not supported)
                                debug!("Unknown login '{}', closing connection", line);
                                break;
                            }
                        }
                        ConnectionState::AwaitingPassword => {
                            // Check if password matches session ID
                            let valid = {
                                let sid = session_id.read().await;
                                sid.as_ref().map(|s| s == line).unwrap_or(false)
                            };

                            if valid {
                                debug!("Telnet session authenticated");
                                if let Err(e) = writer.write_all(RESULT_OK.as_bytes()).await {
                                    warn!("Failed to send OK: {}", e);
                                }
                                let _ = writer.flush().await;
                                state = ConnectionState::Authenticated;
                            } else {
                                warn!("Bad password received");
                                if let Err(e) = writer.write_all(BAD_PASSWORD.as_bytes()).await {
                                    warn!("Failed to send BAD_PASSWORD: {}", e);
                                }
                                let _ = writer.flush().await;
                                break; // Close connection on bad password
                            }
                        }
                        ConnectionState::Authenticated => {
                            trace!("Received command: {}", line);

                            let command = parse_command(line);

                            // Send acknowledgment
                            let response = match &command {
                                TelnetCommand::Unknown(s) => {
                                    format!("ERROR: Unknown command: {}\r\n\r\n", s)
                                }
                                _ => RESULT_OK.to_string(),
                            };

                            if let Err(e) = writer.write_all(response.as_bytes()).await {
                                // Broken pipe is expected when connection closes - log at debug
                                // level
                                debug!("Failed to send response: {}", e);
                            }
                            let _ = writer.flush().await;

                            // Forward command to handler
                            if let Some(ref tx) = command_tx {
                                if let Err(e) = tx.send(command).await {
                                    // Channel closed likely means shutdown is in progress
                                    debug!("Command channel closed (shutdown in progress): {}", e);
                                    break; // Stop processing commands
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!("Error reading from telnet: {}", e);
                break;
            }
        }
    }

    Ok(())
}

/// Filter out telnet IAC sequences from raw data and return just the text
fn filter_telnet_commands(data: &[u8]) -> String {
    let mut result = Vec::new();
    let mut i = 0;

    while i < data.len() {
        if data[i] == IAC && i + 1 < data.len() {
            // Handle telnet command sequences
            match data[i + 1] {
                IAC => {
                    // Escaped IAC (255 255) -> single 255
                    result.push(IAC);
                    i += 2;
                }
                WILL | WONT | DO | DONT => {
                    // 3-byte command: IAC + cmd + option
                    if i + 2 < data.len() {
                        debug!(
                            "Telnet: {:?} option {}",
                            match data[i + 1] {
                                WILL => "WILL",
                                WONT => "WONT",
                                DO => "DO",
                                DONT => "DONT",
                                _ => "?",
                            },
                            data[i + 2]
                        );
                        i += 3;
                    } else {
                        i += 2;
                    }
                }
                SB => {
                    // Subnegotiation: IAC SB ... IAC SE
                    // Skip until we find IAC SE
                    i += 2;
                    while i + 1 < data.len() {
                        if data[i] == IAC && data[i + 1] == SE {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                240..=249 => {
                    // 2-byte commands (NOP, etc.)
                    i += 2;
                }
                _ => {
                    // Unknown, skip 2 bytes
                    i += 2;
                }
            }
        } else {
            // Regular data byte
            result.push(data[i]);
            i += 1;
        }
    }

    String::from_utf8_lossy(&result).to_string()
}

/// Parse a command line into a TelnetCommand
///
/// Supports both JSON format and Atlas busybox CRONLINE format
fn parse_command(cmd: &str) -> TelnetCommand {
    // Try to parse as JSON first
    if cmd.starts_with('{') {
        if let Ok(spec) = serde_json::from_str::<serde_json::Value>(cmd) {
            return parse_json_command(&spec);
        }
    }

    // Try simple text commands
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return TelnetCommand::Unknown(cmd.to_string());
    }

    match parts[0].to_uppercase().as_str() {
        "STATUS" => TelnetCommand::Status,
        "STOP" if parts.len() >= 2 => {
            if let Ok(msm_id) = parts[1].parse() {
                TelnetCommand::Stop(msm_id)
            } else {
                TelnetCommand::Unknown(cmd.to_string())
            }
        }
        "CRONLINE" => parse_cronline(cmd),
        "CRONTAB" => {
            // CRONTAB defines which cron directory following CRONLINEs belong to
            // Format: CRONTAB <directory>
            // We acknowledge it but don't need to do anything special
            trace!("CRONTAB command: {}", cmd);
            TelnetCommand::Ignored(cmd.to_string())
        }
        _ => TelnetCommand::Unknown(cmd.to_string()),
    }
}

/// Parse a CRONLINE command from the Atlas controller
///
/// Format: CRONLINE <interval> <offset> <end_time> <spread_type> <spread_value>
/// <command> <args...>
///
/// Example:
/// CRONLINE 240 274 1770761451 UNIFORM 3 evping -4 -c 3 -A "1001" -O
/// /home/atlas/data/new/7 193.0.14.129
fn parse_cronline(cmd: &str) -> TelnetCommand {
    // Split the command, preserving quoted strings
    let tokens = tokenize_cronline(cmd);

    if tokens.len() < 7 {
        warn!("CRONLINE too short: {}", cmd);
        return TelnetCommand::Unknown(cmd.to_string());
    }

    // Parse CRONLINE header fields
    // tokens[0] = "CRONLINE"
    let interval: u64 = tokens[1].parse().unwrap_or(0);
    let _offset: u64 = tokens[2].parse().unwrap_or(0);
    let end_time: i64 = tokens[3].parse().unwrap_or(0);
    let _spread_type = &tokens[4]; // UNIFORM, etc.
    let spread: u32 = tokens[5].parse().unwrap_or(0);

    // The rest is the measurement command
    let measurement_cmd = &tokens[6];
    let measurement_args = &tokens[7..];

    trace!(
        "CRONLINE: interval={}, end_time={}, spread={}, cmd={}, args={:?}",
        interval,
        end_time,
        spread,
        measurement_cmd,
        measurement_args
    );

    // Parse the measurement command
    match measurement_cmd.as_str() {
        "evping" => parse_evping(measurement_args, interval, end_time, spread),
        "evtraceroute" => parse_evtraceroute(measurement_args, interval, end_time, spread),
        "evtdig" => parse_evtdig(measurement_args, interval, end_time, spread),
        "evhttpget" => parse_evhttpget(measurement_args, interval, end_time, spread),
        "evsslgetcert" => parse_evsslgetcert(measurement_args, interval, end_time, spread),
        "evntp" => parse_evntp(measurement_args, interval, end_time, spread),
        // Internal Atlas commands - not measurements, ignore silently
        "httppost" | "condmv" | "rptaddrs" | "buddyinfo" | "conntrack" | "dfrm" => {
            trace!("Ignoring internal Atlas command: {}", measurement_cmd);
            TelnetCommand::Ignored(cmd.to_string())
        }
        _ => {
            warn!("Unknown measurement command: {}", measurement_cmd);
            TelnetCommand::Unknown(cmd.to_string())
        }
    }
}

/// Tokenize a CRONLINE command, handling quoted strings
fn tokenize_cronline(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for c in cmd.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                // Don't include the quotes in the token
            }
            ' ' | '\t' if !in_quotes => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            _ => {
                current.push(c);
            }
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

/// Parse evping command arguments
///
/// Usage: evping [-4|-6] [-c count] [-s size] [-A msm_id] [-O output] target
fn parse_evping(args: &[String], interval: u64, end_time: i64, spread: u32) -> TelnetCommand {
    let mut af: u8 = 4;
    let mut count: u32 = 3;
    let mut size: u16 = 64;
    let mut msm_id: u64 = 0;
    let mut target = String::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-4" => af = 4,
            "-6" => af = 6,
            "-c" => {
                if i + 1 < args.len() {
                    count = args[i + 1].parse().unwrap_or(3);
                    i += 1;
                }
            }
            "-s" => {
                if i + 1 < args.len() {
                    size = args[i + 1].parse().unwrap_or(64);
                    i += 1;
                }
            }
            "-A" => {
                if i + 1 < args.len() {
                    msm_id = args[i + 1].parse().unwrap_or(0);
                    i += 1;
                }
            }
            "-O" => {
                // Output path - skip it
                if i + 1 < args.len() {
                    i += 1;
                }
            }
            "-I" => {
                // Interval between packets - skip for now
                if i + 1 < args.len() {
                    i += 1;
                }
            }
            "-R" => {
                // Resolve flag - skip
            }
            _ if !arg.starts_with('-') => {
                // This is the target
                target = arg.clone();
            }
            _ => {
                debug!("evping: ignoring unknown option {}", arg);
            }
        }
        i += 1;
    }

    if target.is_empty() {
        warn!("evping: no target specified");
        return TelnetCommand::Unknown(format!("evping {:?}", args));
    }

    trace!(
        "Parsed evping: msm_id={}, target={}, af={}, count={}, size={}, interval={}",
        msm_id,
        target,
        af,
        count,
        size,
        interval
    );

    TelnetCommand::Ping(PingSpec {
        msm_id,
        target,
        af,
        packets: count,
        size,
        packet_interval: 1000, // Default 1 second between packets
        spread: Some(spread),
        schedule: ScheduleSpec {
            interval,
            start_time: 0, // Start now
            stop_time: end_time,
        },
    })
}

/// Parse evtraceroute command arguments
///
/// Usage: evtraceroute [-4|-6] [-I|-U|-T] [-a attempts] [-c count] [-w timeout]
/// [-f first_hop]        [-m max_hops] [-p paris] [-S size] [-A msm_id] [-O
/// output] target
fn parse_evtraceroute(args: &[String], interval: u64, end_time: i64, spread: u32) -> TelnetCommand {
    let mut af: u8 = 4;
    let mut protocol = "ICMP".to_string();
    let mut first_hop: u8 = 1;
    let mut max_hops: u8 = 32;
    let mut paris: Option<u32> = None;
    let mut msm_id: u64 = 0;
    let mut target = String::new();
    let mut size: u16 = 40;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-4" => af = 4,
            "-6" => af = 6,
            "-I" => protocol = "ICMP".to_string(),
            "-U" => protocol = "UDP".to_string(),
            "-T" => protocol = "TCP".to_string(),
            "-a" => {
                // Number of attempts per hop - skip for now
                if i + 1 < args.len() {
                    i += 1;
                }
            }
            "-c" => {
                // Count/packets per hop - skip for now
                if i + 1 < args.len() {
                    i += 1;
                }
            }
            "-w" => {
                // Timeout - skip for now
                if i + 1 < args.len() {
                    i += 1;
                }
            }
            "-f" => {
                if i + 1 < args.len() {
                    first_hop = args[i + 1].parse().unwrap_or(1);
                    i += 1;
                }
            }
            "-m" => {
                if i + 1 < args.len() {
                    max_hops = args[i + 1].parse().unwrap_or(32);
                    i += 1;
                }
            }
            "-p" => {
                if i + 1 < args.len() {
                    paris = Some(args[i + 1].parse().unwrap_or(0));
                    i += 1;
                }
            }
            "-s" | "-S" => {
                if i + 1 < args.len() {
                    size = args[i + 1].parse().unwrap_or(40);
                    i += 1;
                }
            }
            "-A" => {
                if i + 1 < args.len() {
                    msm_id = args[i + 1].parse().unwrap_or(0);
                    i += 1;
                }
            }
            "-O" => {
                if i + 1 < args.len() {
                    i += 1;
                }
            }
            _ if !arg.starts_with('-') => {
                target = arg.clone();
            }
            _ => {
                debug!("evtraceroute: ignoring unknown option {}", arg);
            }
        }
        i += 1;
    }

    if target.is_empty() {
        warn!("evtraceroute: no target specified");
        return TelnetCommand::Unknown(format!("evtraceroute {:?}", args));
    }

    trace!(
        "Parsed evtraceroute: msm_id={}, target={}, af={}, protocol={}, hops={}-{}, interval={}",
        msm_id,
        target,
        af,
        protocol,
        first_hop,
        max_hops,
        interval
    );

    TelnetCommand::Traceroute(TracerouteSpec {
        msm_id,
        target,
        af,
        protocol,
        paris,
        first_hop,
        max_hops,
        size,
        spread: Some(spread),
        schedule: ScheduleSpec {
            interval,
            start_time: 0,
            stop_time: end_time,
        },
    })
}

/// Parse evtdig command arguments
///
/// Usage: evtdig [-4|-6] [-p port] [-r] [-d] [-t qtype] [-c qclass] [-A msm_id]
/// [-O output]        [@server] [query]
///
/// Atlas-specific options:
///   --soa         Query SOA record for "."
///   --resolv      Use system resolver (no @server needed)
///   -h            Query HOSTNAME.BIND (hostname identification)
///   -b            Query BIND version
///   -i            Query ID.SERVER
///   -r            Query RRSIG records (DNSSEC)
///   -t            Use TCP
///   --type N      Query type (numeric)
///   --class N     Query class (numeric)
///   --query NAME  Explicit query name
///   --a NAME      A record query
///   --aaaa NAME   AAAA record query
///   -e SIZE       EDNS buffer size
fn parse_evtdig(args: &[String], interval: u64, end_time: i64, spread: u32) -> TelnetCommand {
    let mut af: u8 = 4;
    let mut protocol = "UDP".to_string();
    let mut query_type = "A".to_string();
    let mut query_class = "IN".to_string();
    let mut use_dnssec = false;
    let mut recursion_desired = true;
    let mut msm_id: u64 = 0;
    let mut target = String::new(); // DNS server
    let mut query_argument = String::new(); // Query name
    let mut use_resolver = false; // Use system resolver

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-4" => af = 4,
            "-6" => af = 6,
            "-p" => {
                // Port - skip for now
                if i + 1 < args.len() {
                    i += 1;
                }
            }
            "--soa" => {
                // SOA query for root
                query_type = "SOA".to_string();
                if query_argument.is_empty() {
                    query_argument = ".".to_string();
                }
            }
            "--resolv" => {
                // Use system resolver
                use_resolver = true;
            }
            "-h" => {
                // Query HOSTNAME.BIND TXT CH
                query_type = "TXT".to_string();
                query_class = "CH".to_string();
                query_argument = "hostname.bind".to_string();
            }
            "-b" => {
                // Query version.bind TXT CH
                query_type = "TXT".to_string();
                query_class = "CH".to_string();
                query_argument = "version.bind".to_string();
            }
            "-i" => {
                // Query id.server TXT CH
                query_type = "TXT".to_string();
                query_class = "CH".to_string();
                query_argument = "id.server".to_string();
            }
            "-r" => {
                // Query for RRSIG (DNSSEC signing)
                use_dnssec = true;
            }
            "-d" | "-D" | "+dnssec" => {
                // DNSSEC
                use_dnssec = true;
            }
            "-t" => {
                // TCP mode (note: this is different from --type)
                protocol = "TCP".to_string();
            }
            "--type" | "-type" => {
                if i + 1 < args.len() {
                    // Numeric query type
                    let type_num: u16 = args[i + 1].parse().unwrap_or(1);
                    query_type = match type_num {
                        1 => "A",
                        2 => "NS",
                        5 => "CNAME",
                        6 => "SOA",
                        12 => "PTR",
                        15 => "MX",
                        16 => "TXT",
                        28 => "AAAA",
                        33 => "SRV",
                        43 => "DS",
                        46 => "RRSIG",
                        47 => "NSEC",
                        48 => "DNSKEY",
                        _ => "A",
                    }
                    .to_string();
                    i += 1;
                }
            }
            "--class" | "-class" => {
                if i + 1 < args.len() {
                    let class_num: u16 = args[i + 1].parse().unwrap_or(1);
                    query_class = match class_num {
                        1 => "IN",
                        3 => "CH",
                        _ => "IN",
                    }
                    .to_string();
                    i += 1;
                }
            }
            "--query" | "-query" => {
                if i + 1 < args.len() {
                    query_argument = args[i + 1].clone();
                    i += 1;
                }
            }
            "--a" | "-a" => {
                query_type = "A".to_string();
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    query_argument = args[i + 1].clone();
                    i += 1;
                }
            }
            "--aaaa" | "-aaaa" => {
                query_type = "AAAA".to_string();
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    query_argument = args[i + 1].clone();
                    i += 1;
                }
            }
            "-e" => {
                // EDNS buffer size - implies EDNS
                if i + 1 < args.len() {
                    i += 1;
                }
            }
            "-R" => {
                // Disable recursion desired
                recursion_desired = false;
            }
            "--retry" => {
                // Retry count - skip
                if i + 1 < args.len() {
                    i += 1;
                }
            }
            "--qbuf" => {
                // Include query buffer in result - skip
            }
            "-T" => {
                // TCP
                protocol = "TCP".to_string();
            }
            "-A" => {
                if i + 1 < args.len() {
                    msm_id = args[i + 1].parse().unwrap_or(0);
                    i += 1;
                }
            }
            "-O" => {
                if i + 1 < args.len() {
                    i += 1;
                }
            }
            _ if arg.starts_with('@') => {
                // DNS server
                target = arg[1..].to_string();
            }
            _ if arg.starts_with('+') => {
                // dig-style options like +dnssec, +tcp, etc.
                match arg.as_str() {
                    "+dnssec" | "+do" => use_dnssec = true,
                    "+tcp" => protocol = "TCP".to_string(),
                    "+nord" | "+norecurse" => recursion_desired = false,
                    _ => debug!("evtdig: ignoring option {}", arg),
                }
            }
            _ if arg.starts_with("--") => {
                // Unknown long option - skip
                debug!("evtdig: ignoring unknown option {}", arg);
            }
            _ if !arg.starts_with('-') => {
                // Positional argument - could be server or query
                if target.is_empty() && arg.parse::<std::net::IpAddr>().is_ok() {
                    // Looks like an IP address - it's the server
                    target = arg.clone();
                } else if query_argument.is_empty() {
                    // Query name
                    query_argument = arg.clone();
                }
            }
            _ => {
                debug!("evtdig: ignoring unknown option {}", arg);
            }
        }
        i += 1;
    }

    // Handle resolver mode - use a placeholder for system resolver
    if use_resolver && target.is_empty() {
        // Use Google DNS as fallback when --resolv is specified
        // In a real implementation, we'd read /etc/resolv.conf
        target = if af == 4 {
            "8.8.8.8"
        } else {
            "2001:4860:4860::8888"
        }
        .to_string();
    }

    // If still no server, try to use the query_argument as server
    if target.is_empty()
        && !query_argument.is_empty()
        && query_argument.parse::<std::net::IpAddr>().is_ok()
    {
        target = query_argument.clone();
        query_argument = ".".to_string();
    }

    if target.is_empty() {
        warn!("evtdig: no DNS server specified, args={:?}", args);
        return TelnetCommand::Unknown(format!("evtdig {:?}", args));
    }

    if query_argument.is_empty() {
        // Default query if none specified
        query_argument = ".".to_string();
    }

    // Remove trailing dot from query name if present
    if query_argument.ends_with('.') && query_argument.len() > 1 {
        query_argument = query_argument[..query_argument.len() - 1].to_string();
    }

    trace!(
        "Parsed evtdig: msm_id={}, server={}, query={} {} {}, protocol={}, interval={}",
        msm_id,
        target,
        query_argument,
        query_type,
        query_class,
        protocol,
        interval
    );

    TelnetCommand::Dns(DnsSpec {
        msm_id,
        target,
        af,
        protocol,
        query_type,
        query_class,
        query_argument,
        use_dnssec,
        recursion_desired,
        spread: Some(spread),
        schedule: ScheduleSpec {
            interval,
            start_time: 0,
            stop_time: end_time,
        },
    })
}

/// Parse evhttpget command arguments
///
/// Usage: evhttpget [-4|-6] [-1] [-A msm_id] [-O output] [--store-headers size]
/// url
fn parse_evhttpget(args: &[String], interval: u64, end_time: i64, spread: u32) -> TelnetCommand {
    let mut af: u8 = 4;
    let mut msm_id: u64 = 0;
    let mut url = String::new();
    let mut max_body_size: Option<u32> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-4" => af = 4,
            "-6" => af = 6,
            "-1" => {
                // HTTP/1.1 mode - skip
            }
            "-A" => {
                if i + 1 < args.len() {
                    msm_id = args[i + 1].parse().unwrap_or(0);
                    i += 1;
                }
            }
            "-O" => {
                if i + 1 < args.len() {
                    i += 1;
                }
            }
            "-M" | "--max-body" => {
                if i + 1 < args.len() {
                    max_body_size = args[i + 1].parse().ok();
                    i += 1;
                }
            }
            "--store-headers" => {
                // Max header storage size - skip
                if i + 1 < args.len() {
                    i += 1;
                }
            }
            _ if !arg.starts_with('-') => {
                url = arg.clone();
            }
            _ => {
                debug!("evhttpget: ignoring unknown option {}", arg);
            }
        }
        i += 1;
    }

    if url.is_empty() {
        warn!("evhttpget: no URL specified");
        return TelnetCommand::Unknown(format!("evhttpget {:?}", args));
    }

    trace!(
        "Parsed evhttpget: msm_id={}, url={}, af={}, interval={}",
        msm_id,
        url,
        af,
        interval
    );

    TelnetCommand::Http(HttpSpec {
        msm_id,
        url,
        method: "GET".to_string(),
        af,
        headers: vec![],
        body: None,
        max_body_size,
        spread: Some(spread),
        schedule: ScheduleSpec {
            interval,
            start_time: 0,
            stop_time: end_time,
        },
    })
}

/// Parse evsslgetcert command arguments
///
/// Usage: evsslgetcert [-4|-6] [-p port] [-h hostname] [-A msm_id] [-O output]
/// target
fn parse_evsslgetcert(args: &[String], interval: u64, end_time: i64, spread: u32) -> TelnetCommand {
    let mut af: u8 = 4;
    let mut port: u16 = 443;
    let mut msm_id: u64 = 0;
    let mut target = String::new();
    let mut hostname: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-4" => af = 4,
            "-6" => af = 6,
            "-p" => {
                if i + 1 < args.len() {
                    port = args[i + 1].parse().unwrap_or(443);
                    i += 1;
                }
            }
            "-A" => {
                if i + 1 < args.len() {
                    msm_id = args[i + 1].parse().unwrap_or(0);
                    i += 1;
                }
            }
            "-O" => {
                if i + 1 < args.len() {
                    i += 1;
                }
            }
            // Note: Atlas uses -h for hostname (SNI), not -H
            "-h" | "-H" | "--hostname" => {
                if i + 1 < args.len() {
                    hostname = Some(args[i + 1].clone());
                    i += 1;
                }
            }
            _ if !arg.starts_with('-') => {
                target = arg.clone();
            }
            _ => {
                debug!("evsslgetcert: ignoring unknown option {}", arg);
            }
        }
        i += 1;
    }

    if target.is_empty() {
        warn!("evsslgetcert: no target specified");
        return TelnetCommand::Unknown(format!("evsslgetcert {:?}", args));
    }

    trace!(
        "Parsed evsslgetcert: msm_id={}, target={}, hostname={:?}, port={}, af={}, interval={}",
        msm_id,
        target,
        hostname,
        port,
        af,
        interval
    );

    TelnetCommand::Tls(TlsSpec {
        msm_id,
        target: target.clone(),
        port,
        af,
        hostname: hostname.or(Some(target)),
        spread: Some(spread),
        schedule: ScheduleSpec {
            interval,
            start_time: 0,
            stop_time: end_time,
        },
    })
}

/// Parse evntp command arguments
///
/// Usage: evntp [-4|-6] [-c count] [-A msm_id] [-O output] target
fn parse_evntp(args: &[String], interval: u64, end_time: i64, spread: u32) -> TelnetCommand {
    let mut af: u8 = 4;
    let mut packets: u32 = 3;
    let mut msm_id: u64 = 0;
    let mut target = String::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-4" => af = 4,
            "-6" => af = 6,
            "-c" => {
                if i + 1 < args.len() {
                    packets = args[i + 1].parse().unwrap_or(3);
                    i += 1;
                }
            }
            "-A" => {
                if i + 1 < args.len() {
                    msm_id = args[i + 1].parse().unwrap_or(0);
                    i += 1;
                }
            }
            "-O" => {
                if i + 1 < args.len() {
                    i += 1;
                }
            }
            _ if !arg.starts_with('-') => {
                target = arg.clone();
            }
            _ => {
                debug!("evntp: ignoring unknown option {}", arg);
            }
        }
        i += 1;
    }

    if target.is_empty() {
        warn!("evntp: no target specified");
        return TelnetCommand::Unknown(format!("evntp {:?}", args));
    }

    trace!(
        "Parsed evntp: msm_id={}, target={}, af={}, packets={}, interval={}",
        msm_id,
        target,
        af,
        packets,
        interval
    );

    TelnetCommand::Ntp(NtpSpec {
        msm_id,
        target,
        af,
        packets,
        spread: Some(spread),
        schedule: ScheduleSpec {
            interval,
            start_time: 0,
            stop_time: end_time,
        },
    })
}

/// Parse scheduling fields from JSON spec
fn parse_schedule(spec: &serde_json::Value) -> ScheduleSpec {
    ScheduleSpec {
        interval: spec.get("interval").and_then(|v| v.as_u64()).unwrap_or(0),
        start_time: spec.get("start_time").and_then(|v| v.as_i64()).unwrap_or(0),
        stop_time: spec.get("stop_time").and_then(|v| v.as_i64()).unwrap_or(0),
    }
}

/// Parse a JSON measurement specification
fn parse_json_command(spec: &serde_json::Value) -> TelnetCommand {
    let msm_type = spec
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();

    let msm_id = spec.get("msm_id").and_then(|v| v.as_u64()).unwrap_or(0);

    let target = spec
        .get("target")
        .or_else(|| spec.get("dst_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let af = spec.get("af").and_then(|v| v.as_u64()).unwrap_or(4) as u8;

    let spread = spec
        .get("spread")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let schedule = parse_schedule(spec);

    match msm_type.as_str() {
        "ping" => TelnetCommand::Ping(PingSpec {
            msm_id,
            target,
            af,
            packets: spec.get("packets").and_then(|v| v.as_u64()).unwrap_or(3) as u32,
            size: spec.get("size").and_then(|v| v.as_u64()).unwrap_or(64) as u16,
            packet_interval: spec
                .get("packet_interval")
                .and_then(|v| v.as_u64())
                .unwrap_or(1000) as u32,
            spread,
            schedule,
        }),

        "traceroute" => TelnetCommand::Traceroute(TracerouteSpec {
            msm_id,
            target,
            af,
            protocol: spec
                .get("protocol")
                .and_then(|v| v.as_str())
                .unwrap_or("ICMP")
                .to_string(),
            paris: spec.get("paris").and_then(|v| v.as_u64()).map(|v| v as u32),
            first_hop: spec.get("first_hop").and_then(|v| v.as_u64()).unwrap_or(1) as u8,
            max_hops: spec.get("max_hops").and_then(|v| v.as_u64()).unwrap_or(32) as u8,
            size: spec.get("size").and_then(|v| v.as_u64()).unwrap_or(40) as u16,
            spread,
            schedule,
        }),

        "dns" => TelnetCommand::Dns(DnsSpec {
            msm_id,
            target,
            af,
            protocol: spec
                .get("protocol")
                .and_then(|v| v.as_str())
                .unwrap_or("UDP")
                .to_string(),
            query_type: spec
                .get("query_type")
                .and_then(|v| v.as_str())
                .unwrap_or("A")
                .to_string(),
            query_class: spec
                .get("query_class")
                .and_then(|v| v.as_str())
                .unwrap_or("IN")
                .to_string(),
            query_argument: spec
                .get("query_argument")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            use_dnssec: spec
                .get("use_dnssec")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            recursion_desired: spec
                .get("recursion_desired")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            spread,
            schedule,
        }),

        "http" | "https" => TelnetCommand::Http(HttpSpec {
            msm_id,
            url: spec
                .get("url")
                .or_else(|| spec.get("target"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            method: spec
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET")
                .to_string(),
            af,
            headers: spec
                .get("headers")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            body: spec.get("body").and_then(|v| v.as_str()).map(String::from),
            max_body_size: spec
                .get("max_body_size")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            spread,
            schedule,
        }),

        "sslcert" | "tls" => TelnetCommand::Tls(TlsSpec {
            msm_id,
            target: target.clone(),
            port: spec.get("port").and_then(|v| v.as_u64()).unwrap_or(443) as u16,
            af,
            hostname: spec
                .get("hostname")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or(Some(target)),
            spread,
            schedule,
        }),

        "ntp" => TelnetCommand::Ntp(NtpSpec {
            msm_id,
            target,
            af,
            packets: spec.get("packets").and_then(|v| v.as_u64()).unwrap_or(3) as u32,
            spread,
            schedule,
        }),

        _ => TelnetCommand::Unknown(format!("Unknown measurement type: {}", msm_type)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ping_command() {
        let json = r#"{"type":"ping","msm_id":1001,"target":"8.8.8.8","af":4,"packets":3}"#;
        let cmd = parse_command(json);
        match cmd {
            TelnetCommand::Ping(spec) => {
                assert_eq!(spec.msm_id, 1001);
                assert_eq!(spec.target, "8.8.8.8");
                assert_eq!(spec.packets, 3);
                assert_eq!(spec.schedule.interval, 0); // default one-shot
            }
            _ => panic!("Expected Ping command"),
        }
    }

    #[test]
    fn test_parse_recurring_ping_command() {
        let json = r#"{"type":"ping","msm_id":1001,"target":"8.8.8.8","af":4,"packets":3,"interval":300,"start_time":1700000000,"stop_time":1700086400}"#;
        let cmd = parse_command(json);
        match cmd {
            TelnetCommand::Ping(spec) => {
                assert_eq!(spec.msm_id, 1001);
                assert_eq!(spec.schedule.interval, 300);
                assert_eq!(spec.schedule.start_time, 1700000000);
                assert_eq!(spec.schedule.stop_time, 1700086400);
            }
            _ => panic!("Expected Ping command"),
        }
    }

    #[test]
    fn test_parse_dns_command() {
        let json = r#"{"type":"dns","msm_id":1002,"target":"9.9.9.9","query_argument":"example.com","query_type":"A"}"#;
        let cmd = parse_command(json);
        match cmd {
            TelnetCommand::Dns(spec) => {
                assert_eq!(spec.msm_id, 1002);
                assert_eq!(spec.target, "9.9.9.9");
                assert_eq!(spec.query_argument, "example.com");
            }
            _ => panic!("Expected Dns command"),
        }
    }

    #[test]
    fn test_parse_status_command() {
        let cmd = parse_command("STATUS");
        assert!(matches!(cmd, TelnetCommand::Status));
    }

    #[test]
    fn test_parse_stop_command() {
        let cmd = parse_command("STOP 12345");
        match cmd {
            TelnetCommand::Stop(id) => assert_eq!(id, 12345),
            _ => panic!("Expected Stop command"),
        }
    }

    // CRONLINE tests

    #[test]
    fn test_tokenize_cronline() {
        let tokens = tokenize_cronline(
            r#"CRONLINE 240 274 1770761451 UNIFORM 3 evping -4 -c 3 -A "1001" -O /home/atlas/data/new/7 193.0.14.129"#,
        );
        assert_eq!(tokens[0], "CRONLINE");
        assert_eq!(tokens[1], "240");
        assert_eq!(tokens[6], "evping");
        assert_eq!(tokens[11], "1001"); // Quoted string without quotes
        assert_eq!(tokens[tokens.len() - 1], "193.0.14.129");
    }

    #[test]
    fn test_parse_cronline_evping() {
        let cmd = parse_command(
            r#"CRONLINE 240 274 1770761451 UNIFORM 3 evping -4 -c 3 -A "1001" -O /home/atlas/data/new/7 193.0.14.129"#,
        );
        match cmd {
            TelnetCommand::Ping(spec) => {
                assert_eq!(spec.msm_id, 1001);
                assert_eq!(spec.target, "193.0.14.129");
                assert_eq!(spec.af, 4);
                assert_eq!(spec.packets, 3);
                assert_eq!(spec.schedule.interval, 240);
                assert_eq!(spec.schedule.stop_time, 1770761451);
                assert_eq!(spec.spread, Some(3));
            }
            _ => panic!("Expected Ping command, got {:?}", cmd),
        }
    }

    #[test]
    fn test_parse_cronline_evping_ipv6() {
        let cmd = parse_command(
            r#"CRONLINE 900 123 1800000000 UNIFORM 5 evping -6 -c 5 -s 128 -A "2001" 2001:4860:4860::8888"#,
        );
        match cmd {
            TelnetCommand::Ping(spec) => {
                assert_eq!(spec.msm_id, 2001);
                assert_eq!(spec.target, "2001:4860:4860::8888");
                assert_eq!(spec.af, 6);
                assert_eq!(spec.packets, 5);
                assert_eq!(spec.size, 128);
                assert_eq!(spec.schedule.interval, 900);
            }
            _ => panic!("Expected Ping command"),
        }
    }

    #[test]
    fn test_parse_cronline_evtraceroute_udp() {
        let cmd = parse_command(
            r#"CRONLINE 1800 331 1925935851 UNIFORM 3 evtraceroute -4 -U -c 3 -w 1000 -A "5001" -O /home/atlas/data/new/7 193.0.14.129"#,
        );
        match cmd {
            TelnetCommand::Traceroute(spec) => {
                assert_eq!(spec.msm_id, 5001);
                assert_eq!(spec.target, "193.0.14.129");
                assert_eq!(spec.af, 4);
                assert_eq!(spec.protocol, "UDP");
                assert_eq!(spec.schedule.interval, 1800);
            }
            _ => panic!("Expected Traceroute command, got {:?}", cmd),
        }
    }

    #[test]
    fn test_parse_cronline_evtraceroute_icmp() {
        let cmd = parse_command(
            r#"CRONLINE 3600 0 0 UNIFORM 10 evtraceroute -4 -I -m 64 -f 1 -A "5002" 8.8.8.8"#,
        );
        match cmd {
            TelnetCommand::Traceroute(spec) => {
                assert_eq!(spec.msm_id, 5002);
                assert_eq!(spec.target, "8.8.8.8");
                assert_eq!(spec.protocol, "ICMP");
                assert_eq!(spec.first_hop, 1);
                assert_eq!(spec.max_hops, 64);
            }
            _ => panic!("Expected Traceroute command"),
        }
    }

    #[test]
    fn test_parse_cronline_evtdig() {
        // Test with --aaaa (Atlas format for AAAA query)
        let cmd = parse_command(
            r#"CRONLINE 900 0 0 UNIFORM 5 evtdig -4 --aaaa example.com. -A "6001" @9.9.9.9"#,
        );
        match cmd {
            TelnetCommand::Dns(spec) => {
                assert_eq!(spec.msm_id, 6001);
                assert_eq!(spec.target, "9.9.9.9");
                assert_eq!(spec.query_argument, "example.com"); // trailing dot stripped
                assert_eq!(spec.query_type, "AAAA");
                assert_eq!(spec.af, 4);
            }
            _ => panic!("Expected Dns command, got {:?}", cmd),
        }
    }

    #[test]
    fn test_parse_cronline_evtdig_dnssec() {
        // Test with -d for DNSSEC and --a for A record query
        let cmd = parse_command(
            r#"CRONLINE 1800 0 0 UNIFORM 3 evtdig -4 -d --a dnssec-test.org. -A "6002" @8.8.8.8"#,
        );
        match cmd {
            TelnetCommand::Dns(spec) => {
                assert_eq!(spec.msm_id, 6002);
                assert!(spec.use_dnssec);
                assert_eq!(spec.query_argument, "dnssec-test.org");
            }
            _ => panic!("Expected Dns command"),
        }
    }

    #[test]
    fn test_parse_cronline_evtdig_soa() {
        // Test real Atlas SOA query format
        let cmd = parse_command(
            r#"CRONLINE 1800 795 1770762780 UNIFORM 3 evtdig -4 --soa . -A "10001" -O /home/atlas/data/new/7 193.0.14.129"#,
        );
        match cmd {
            TelnetCommand::Dns(spec) => {
                assert_eq!(spec.msm_id, 10001);
                assert_eq!(spec.target, "193.0.14.129");
                assert_eq!(spec.query_argument, ".");
                assert_eq!(spec.query_type, "SOA");
                assert_eq!(spec.af, 4);
                assert_eq!(spec.protocol, "UDP");
            }
            _ => panic!("Expected Dns command, got {:?}", cmd),
        }
    }

    #[test]
    fn test_parse_cronline_evtdig_tcp_soa() {
        // Test TCP SOA query (with -t for TCP)
        let cmd = parse_command(
            r#"CRONLINE 1800 1945 1770762780 UNIFORM 3 evtdig -4 -t --soa . -A "10101" -O /home/atlas/data/new/7 193.0.14.129"#,
        );
        match cmd {
            TelnetCommand::Dns(spec) => {
                assert_eq!(spec.msm_id, 10101);
                assert_eq!(spec.target, "193.0.14.129");
                assert_eq!(spec.query_type, "SOA");
                assert_eq!(spec.protocol, "TCP");
            }
            _ => panic!("Expected Dns command, got {:?}", cmd),
        }
    }

    #[test]
    fn test_parse_cronline_evtdig_hostname() {
        // Test -h for hostname.bind query
        let cmd = parse_command(
            r#"CRONLINE 240 659 1770762780 UNIFORM 3 evtdig -4 -h -A "10301" -O /home/atlas/data/new/7 193.0.14.129"#,
        );
        match cmd {
            TelnetCommand::Dns(spec) => {
                assert_eq!(spec.msm_id, 10301);
                assert_eq!(spec.target, "193.0.14.129");
                assert_eq!(spec.query_argument, "hostname.bind");
                assert_eq!(spec.query_type, "TXT");
                assert_eq!(spec.query_class, "CH");
            }
            _ => panic!("Expected Dns command, got {:?}", cmd),
        }
    }

    #[test]
    fn test_parse_cronline_evhttpget() {
        let cmd = parse_command(
            r#"CRONLINE 3600 0 0 UNIFORM 60 evhttpget -4 -A "7001" http://example.com/path"#,
        );
        match cmd {
            TelnetCommand::Http(spec) => {
                assert_eq!(spec.msm_id, 7001);
                assert_eq!(spec.url, "http://example.com/path");
                assert_eq!(spec.method, "GET");
                assert_eq!(spec.af, 4);
            }
            _ => panic!("Expected Http command, got {:?}", cmd),
        }
    }

    #[test]
    fn test_parse_cronline_evsslgetcert() {
        let cmd = parse_command(
            r#"CRONLINE 1800 0 0 UNIFORM 10 evsslgetcert -4 -p 443 -A "8001" example.com"#,
        );
        match cmd {
            TelnetCommand::Tls(spec) => {
                assert_eq!(spec.msm_id, 8001);
                assert_eq!(spec.target, "example.com");
                assert_eq!(spec.port, 443);
                assert_eq!(spec.af, 4);
            }
            _ => panic!("Expected Tls command, got {:?}", cmd),
        }
    }

    #[test]
    fn test_parse_cronline_evsslgetcert_custom_port() {
        let cmd = parse_command(
            r#"CRONLINE 900 0 0 UNIFORM 5 evsslgetcert -6 -p 8443 -A "8002" secure.example.com"#,
        );
        match cmd {
            TelnetCommand::Tls(spec) => {
                assert_eq!(spec.msm_id, 8002);
                assert_eq!(spec.target, "secure.example.com");
                assert_eq!(spec.port, 8443);
                assert_eq!(spec.af, 6);
            }
            _ => panic!("Expected Tls command"),
        }
    }

    #[test]
    fn test_parse_cronline_evntp() {
        let cmd =
            parse_command(r#"CRONLINE 1800 0 0 UNIFORM 3 evntp -4 -c 5 -A "9001" pool.ntp.org"#);
        match cmd {
            TelnetCommand::Ntp(spec) => {
                assert_eq!(spec.msm_id, 9001);
                assert_eq!(spec.target, "pool.ntp.org");
                assert_eq!(spec.packets, 5);
                assert_eq!(spec.af, 4);
            }
            _ => panic!("Expected Ntp command, got {:?}", cmd),
        }
    }
}
