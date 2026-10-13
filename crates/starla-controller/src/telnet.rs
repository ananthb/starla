//! Telnet command interface for receiving measurement requests
//!
//! The RIPE Atlas controller sends measurement specifications over the
//! reverse-tunneled telnet connection. This module handles parsing and
//! dispatching these commands.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
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
    /// Schedule a host-telemetry report (buddyinfo / rptaddrs).
    /// Not a measurement — produces a synthesized RESULT line each interval.
    HostTelemetry(HostTelemetrySpec),
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

/// Which host-telemetry reporter the controller is asking us to run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum HostTelemetryKind {
    Buddyinfo,
    Rptaddrs,
}

/// Host-telemetry scheduling spec parsed from a CRONLINE.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostTelemetrySpec {
    pub kind: HostTelemetryKind,
    /// Interval between runs (0 = one-shot).
    pub interval: u64,
    /// Controller-assigned measurement id (from `-A`).
    /// buddyinfo's official applet hardcodes 9001 so this is optional;
    /// rptaddrs is invoked as `-A 9104 ...`.
    pub msm_id: Option<u32>,
    /// buddyinfo's lowmem threshold (KB) — recorded but not acted on.
    pub lowmem: Option<u32>,
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
    /// `--store-headers N`: report the response header lines within N bytes
    /// (the ip-echo built-ins ask for 1024; RIPE reads X-Client-IP there)
    #[serde(default)]
    pub store_headers: u32,
    /// `-0`: HTTP/1.0 (evhttpget's default, and `-1`, is HTTP/1.1)
    #[serde(default)]
    pub http10: bool,
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
    /// `-w`: ms to wait for each reply (evntp default 1000)
    pub timeout_ms: u32,
    /// `-g`: minimum ms between requests (5130; default 4000, 2000..=60000)
    pub gap_ms: u32,
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

// RIPE Atlas telnet protocol constants (networking/telnetd.c:139-170)
const ATLAS_LOGIN: &str = "C_TO_P_TEST_V1";
const LOGIN_PREFIX: &str = "Atlas probe, see http://atlas.ripe.net/\r\n\r\n";
const LOGIN_PROMPT: &str = " login: ";
const PASSWORD_PROMPT: &str = "\r\nPassword: ";

// The request lines telnetd knows (telnetd.c:148-152), matched as it matches
// them: case-sensitive prefixes that include their space (strncmp), and
// REBOOT and RELOAD whole (strcmp).
const CMD_CRONTAB: &str = "CRONTAB ";
const CMD_CRONLINE: &str = "CRONLINE ";
const CMD_ONEOFF: &str = "ONEOFF ";
const CMD_REBOOT: &str = "REBOOT";
const CMD_RELOAD: &str = "RELOAD";

// Its replies (telnetd.c:154-164). The others there never apply to Starla:
// CRONTAB_BUSY cannot happen even in the reference (a CRONTAB line ends the
// open crontab, and a new session's takeover closes it, :1112-1124), and
// UNABLE_TO_CREATE_NEW_CRONTAB, IO_ERROR and BAD_FW_FILE report failures to
// write the crontab, oneoff and firmware files, which Starla does not keep.
const RESULT_OK: &str = "OK\r\n\r\n";
const BAD_PASSWORD: &str = "BAD_PASSWORD\r\n\r\n";
const BAD_COMMAND: &str = "BAD_COMMAND\r\n\r\n";
const NAME_TOO_LONG: &str = "NAME_TOO_LONG\r\n\r\n";
const BAD_PATH: &str = "BAD_PATH\r\n\r\n";
const BAD_REMOUNT: &str = "BAD_REMOUNT\r\n\r\n";

// Where request paths may point: rebased_validated_filename(ATLAS_SPOOLDIR,
// path, "crons") (libbb/validate_filename.c) takes a path under
// /home/atlas/crons/ (what RIPE's controller sends) and rebases it onto the
// spool directory, or one already under the spool directory's crons/. The
// spool directory is configure.ac:145's default, as packaged and as the
// twin's reference ran it.
const ATLAS_HOME: &str = "/home/atlas";
const ATLAS_SPOOLDIR: &str = "/var/spool/ripe-atlas";

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

/// The Atlas states of the reference's telnetd (telnetd.c:175-183), less
/// DO_TRADITIONAL: the /bin/login session it starts for any other login
/// name, which Starla does not offer.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ConnectionState {
    /// GET_LOGINNAME: waiting for the login name
    GetLoginName,
    /// GET_PASSWORD: waiting for the password (the controller session ID)
    GetPassword,
    /// GET_CMD: authenticated, waiting for a request line
    GetCmd,
    /// DO_CRONTAB: inside a CRONTAB, taking its CRONLINEs
    DoCrontab,
    /// EOM_SEEN: the empty line ended the request; nothing more is taken
    EomSeen,
}

/// Splits the controller's input into lines as getline_2pty does
/// (telnetd.c:1402-1448): a line ends at a '\n' or a '\r'. One difference:
/// "\r\n" is one line end here. getline_2pty ends the line at the '\r' and
/// reads the '\n' as an empty line of its own, which would end the request
/// (in ATLAS mode nothing turns CR-LF into CR before it, whatever its comment
/// at :1412 says). RIPE's controller sends a bare "\n", so the reference never
/// meets that. The NUL a telnet client puts after a bare '\r' (RFC 854) is
/// part of the line end too; telnetd drops a NUL only when it ends a read
/// (:1059-1062). A line that still holds a NUL is a "bad line" there, and
/// skipped (:1432-1445).
#[derive(Debug, Default)]
struct LineSplitter {
    partial: Vec<u8>,
    /// The last line ended at a '\r': a '\n' or NUL right after it is part of
    /// that line end, even when it comes in the next read.
    after_cr: bool,
}

impl LineSplitter {
    /// Take in a chunk of input and return the lines it completes.
    fn feed(&mut self, data: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &b in data {
            if std::mem::take(&mut self.after_cr) && (b == b'\n' || b == 0) {
                continue;
            }
            match b {
                b'\n' | b'\r' => {
                    self.after_cr = b == b'\r';
                    let line = std::mem::take(&mut self.partial);
                    if line.contains(&0) {
                        debug!("Telnet: skipping a line with a NUL in it");
                        continue;
                    }
                    lines.push(String::from_utf8_lossy(&line).into_owned());
                }
                _ => self.partial.push(b),
            }
        }
        lines
    }

    /// Bytes of the line not yet ended
    fn pending(&self) -> usize {
        self.partial.len()
    }
}

/// Handle a single telnet connection with Atlas authentication: the ATLAS
/// half of the reference's telnetd (telnetd.c:1069-1284).
///
/// The reference answers a request once, with OK on the empty line that ends
/// it; it says nothing after the password or after a command it takes, and
/// answers only what it cannot take (BAD_PASSWORD, BAD_COMMAND, BAD_PATH …).
///
/// Accepts any async stream: works with both TCP sockets (for local testing)
/// and SSH channel streams (for production controller connections).
pub async fn handle_connection(
    stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    command_tx: Option<mpsc::Sender<TelnetCommand>>,
    probe_id: u32,
    session_id: std::sync::Arc<tokio::sync::RwLock<Option<String>>>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);

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
    let mut state = ConnectionState::GetLoginName;
    let mut lines = LineSplitter::default();
    // Set once authenticated: tells when a newer session has taken over
    let mut atlas: Option<tokio::sync::watch::Receiver<()>> = None;

    // Maximum length of a line not yet ended, to bound the memory a
    // malicious or malfunctioning controller can take
    const MAX_LINE_LEN: usize = 256 * 1024; // 256KB

    'session: loop {
        use tokio::io::AsyncReadExt;
        let read = tokio::select! {
            read = reader.read(&mut raw_buf) => read,
            () = taken_over(&mut atlas) => {
                debug!("A newer controller session took over, closing this one");
                break;
            }
        };
        let n = match read {
            Ok(0) => break, // EOF
            Ok(n) => n,
            Err(e) => {
                error!("Error reading from telnet: {}", e);
                break;
            }
        };

        // Filter out telnet IAC sequences and split the rest into lines
        for line in lines.feed(&filter_telnet_commands(&raw_buf[..n])) {
            if atlas
                .as_ref()
                .is_some_and(|rx| rx.has_changed().unwrap_or(false))
            {
                debug!("A newer controller session took over, closing this one");
                break 'session;
            }
            log_received(state, &line);

            match state {
                ConnectionState::GetLoginName => {
                    if line == ATLAS_LOGIN {
                        trace!("Atlas login received, requesting password");
                        reply(&mut writer, PASSWORD_PROMPT).await;
                        state = ConnectionState::GetPassword;
                    } else {
                        // The reference echoes any other name and runs
                        // /bin/login for it on a pty (telnetd.c:1087-1100).
                        // Starla has no login session to offer: it echoes
                        // the name the same way, and closes.
                        debug!("Unknown login '{}', closing connection", line);
                        reply(&mut writer, &line).await;
                        break 'session;
                    }
                }
                ConnectionState::GetPassword => {
                    let valid = session_id.read().await.as_deref() == Some(line.as_str());
                    if valid {
                        // No reply: straight to GET_CMD (telnetd.c:1108-1127)
                        debug!("Telnet session authenticated");
                        atlas = Some(take_over(&session_id));
                        state = ConnectionState::GetCmd;
                    } else {
                        // BAD_PASSWORD, and the state stays GET_PASSWORD
                        // (telnetd.c:1129-1136): the connection stays open
                        // and the next line is another try.
                        warn!("Bad password received");
                        reply(&mut writer, BAD_PASSWORD).await;
                    }
                }
                ConnectionState::DoCrontab if line.starts_with(CMD_CRONLINE) => {
                    // add_to_crontab, no reply (telnetd.c:1241-1262).
                    // A known difference: the reference only writes the line
                    // to root.new, and installs the crontab in end_crontab
                    // (:1535-1600) when a line other than a CRONLINE comes; a
                    // crontab still open when the session is killed
                    // (:1302-1311) or taken over (:1112-1122) is closed and
                    // never renamed into place. Starla hands each CRONLINE on
                    // as it comes, as it always has, so it acts on a crontab
                    // the controller never finished.
                    if !forward(&command_tx, parse_command(&line)).await {
                        break 'session;
                    }
                }
                ConnectionState::GetCmd | ConnectionState::DoCrontab => {
                    // Any other line ends the crontab and is the next
                    // command ("goto do_cmd", telnetd.c:1242-1256).
                    let (response, command, next) = get_cmd(&line);
                    if let Some(response) = response {
                        reply(&mut writer, response).await;
                    }
                    if let Some(command) = command {
                        if !forward(&command_tx, command).await {
                            break 'session;
                        }
                    }
                    state = next;
                }
                ConnectionState::EomSeen => {
                    // "Just eat all input and return bad command"
                    // (telnetd.c:1265-1278)
                    reply(&mut writer, BAD_COMMAND).await;
                }
            }
        }

        if lines.pending() > MAX_LINE_LEN {
            error!(
                "Telnet line exceeded {}KB, disconnecting",
                MAX_LINE_LEN / 1024
            );
            anyhow::bail!("Line buffer overflow");
        }
    }

    Ok(())
}

/// One controller session at a time, as telnetd's `atlas_ts`
/// (telnetd.c:243-247): a session that authenticates takes over
/// (:1112-1124), and an older one is killed on its next pass through the
/// loop, input or not (`if (ts != atlas_ts) goto kill_session`, :1143, :1234,
/// :1266). Each authenticated session watches its probe's channel here, and
/// is over at the next send. Keyed by the address of the session-ID cell, so
/// each probe (and each test) has its own.
type AtlasSessions = std::collections::HashMap<usize, tokio::sync::watch::Sender<()>>;
static ATLAS_SESSIONS: std::sync::LazyLock<std::sync::Mutex<AtlasSessions>> =
    std::sync::LazyLock::new(Default::default);

/// This session has authenticated: it takes over from any older one.
fn take_over(
    session_id: &std::sync::Arc<tokio::sync::RwLock<Option<String>>>,
) -> tokio::sync::watch::Receiver<()> {
    let key = std::sync::Arc::as_ptr(session_id) as usize;
    let mut sessions = ATLAS_SESSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tx = sessions
        .entry(key)
        .or_insert_with(|| tokio::sync::watch::channel(()).0);
    tx.send_replace(());
    tx.subscribe()
}

/// Resolves when a newer session has taken over; never before this one has
/// authenticated.
async fn taken_over(atlas: &mut Option<tokio::sync::watch::Receiver<()>>) {
    if let Some(rx) = atlas {
        if rx.changed().await.is_ok() {
            return;
        }
    }
    std::future::pending().await
}

/// Send a reply to the controller. A failed write is not fatal here: the
/// next read finds the connection gone.
async fn reply(writer: &mut (impl AsyncWrite + Unpin), text: &str) {
    if let Err(e) = writer.write_all(text.as_bytes()).await {
        // Broken pipe is expected when the connection closes
        debug!("Failed to send telnet reply: {}", e);
    }
    let _ = writer.flush().await;
}

/// Hand a command to the probe; false once the channel is closed, which
/// ends the session.
async fn forward(command_tx: &Option<mpsc::Sender<TelnetCommand>>, command: TelnetCommand) -> bool {
    trace!("Received command: {:?}", command);
    let Some(tx) = command_tx else {
        return true;
    };
    match tx.send(command).await {
        Ok(()) => true,
        Err(e) => {
            // Channel closed likely means shutdown is in progress
            debug!("Command channel closed (shutdown in progress): {}", e);
            false
        }
    }
}

/// The debug line for each line from the controller.
///
/// The text logged for a request line is load-bearing: the twin that compares
/// Starla with RIPE's probe on the wire (andrii infra/vultr/atlas/twin,
/// src/scenario.rs `extract`) rebuilds the controller's requests from
/// Starla's journal by matching "Telnet state Authenticated, received: '",
/// one request line each, and adds the empty line that ends a request
/// itself (twin/src/control.rs). So every line GET_CMD or DO_CRONTAB takes
/// or answers keeps that text, with nothing after the closing quote (no
/// tracing field, which the journal would print there). The empty line and
/// what follows it are logged otherwise: under that text the replay would
/// send two empty lines, the second drawing a BAD_COMMAND from both probes.
fn log_received(state: ConnectionState, line: &str) {
    use ConnectionState::{DoCrontab, EomSeen, GetCmd, GetLoginName, GetPassword};
    match state {
        GetLoginName => debug!("Telnet state {:?}, received: '{}'", state, line),
        // The session ID stays out of the log
        GetPassword => debug!("Telnet state {:?}, received a password", state),
        GetCmd | DoCrontab if line.is_empty() => debug!("Telnet: end of request"),
        GetCmd | DoCrontab => debug!("Telnet state Authenticated, received: '{}'", line),
        EomSeen => debug!("Telnet: after the end of request, received: '{}'", line),
    }
}

/// GET_CMD for one line (telnetd.c:1138-1228): the reply, if any, the
/// command to hand to the probe, if any, and the next state.
fn get_cmd(line: &str) -> (Option<&'static str>, Option<TelnetCommand>, ConnectionState) {
    use ConnectionState::{DoCrontab, EomSeen, GetCmd};

    if let Some(dir) = line.strip_prefix(CMD_CRONTAB) {
        // start_crontab (telnetd.c:1471-1518); its CRONLINEs follow
        return match crontab_dir_check(dir) {
            Ok(()) => (None, Some(parse_command(line)), DoCrontab),
            Err(reply) => (Some(reply), None, GetCmd),
        };
    }
    if let Some(rest) = line.strip_prefix(CMD_ONEOFF) {
        // do_oneoff (telnetd.c:1616-1696), no reply when it is taken
        return match oneoff_file_check(rest) {
            Ok(()) => (None, Some(parse_command(line)), GetCmd),
            Err(reply) => (Some(reply), None, GetCmd),
        };
    }
    if line == CMD_REBOOT {
        // reboot(2) needs CAP_SYS_BOOT, which ripe-atlas.service withholds
        // from its unprivileged user: the call fails and nothing is sent
        // (telnetd.c:1179-1186).
        debug!("Telnet: REBOOT is not for a software probe");
        return (None, None, GetCmd);
    }
    if line == CMD_RELOAD {
        // RELOAD first remounts / read-write, which mount(2) refuses the
        // unprivileged ripe-atlas user: BAD_REMOUNT (telnetd.c:1187-1196).
        return (Some(BAD_REMOUNT), None, GetCmd);
    }
    if line.is_empty() {
        // End of request: the one OK (telnetd.c:1212-1221)
        return (Some(RESULT_OK), None, EomSeen);
    }

    // Anything else is a bad command (telnetd.c:1223-1227) and gets
    // BAD_COMMAND, whatever it is, so the bytes on the wire are the
    // reference's for any input. Starla still takes its own commands, which
    // no RIPE controller sends, once it has answered: STATUS and
    // STOP <msm_id> (in either letter case, as parse_command reads them) and
    // a measurement as a JSON object. Nothing else that lands here is taken,
    // a CRONLINE outside a CRONTAB among them.
    let command = match parse_command(line) {
        command @ (TelnetCommand::Status | TelnetCommand::Stop(_)) => Some(command),
        TelnetCommand::Unknown(_) => None,
        command if line.starts_with('{') => Some(command),
        _ => None,
    };
    (Some(BAD_COMMAND), command, GetCmd)
}

/// start_crontab's checks of its directory (telnetd.c:1484-1505): BAD_PATH,
/// then NAME_TOO_LONG when the rebased directory with "/root.new" after it
/// does not fit telnetd's 256-byte buffers. (Its first length check, of the
/// directory alone, is within this one.)
fn crontab_dir_check(dir: &str) -> Result<(), &'static str> {
    let len = rebased_crons_path_len(dir).ok_or(BAD_PATH)?;
    if len + "/root.new".len() + 1 > 256 {
        return Err(NAME_TOO_LONG);
    }
    Ok(())
}

/// do_oneoff's checks of its file (telnetd.c:1623-1660): the name runs to
/// the first white space; NAME_TOO_LONG when it with ".new" after it does
/// not fit 256 bytes, then BAD_PATH. (The ".new" name's own BAD_PATH check
/// cannot fail where the name's passed.)
fn oneoff_file_check(rest: &str) -> Result<(), &'static str> {
    let name = &rest[..rest.find(c_isspace).unwrap_or(rest.len())];
    if name.len() + ".new".len() + 1 > 256 {
        return Err(NAME_TOO_LONG);
    }
    rebased_crons_path_len(name).map(|_| ()).ok_or(BAD_PATH)
}

/// rebased_validated_filename(ATLAS_SPOOLDIR, path, "crons")
/// (libbb/validate_filename.c:3-131): the path's length once rebased onto
/// the spool directory, or None where the reference answers BAD_PATH. The
/// path is /home/atlas/crons/… or <spooldir>/crons/… (properties 1 and 2),
/// holds no "/../" and does not end in "/.." (3 and 4).
fn rebased_crons_path_len(path: &str) -> Option<usize> {
    let under = |base: &str| {
        path.strip_prefix(base)
            .is_some_and(|rest| rest.starts_with("/crons/"))
    };
    let len = if under(ATLAS_HOME) {
        path.len() - ATLAS_HOME.len() + ATLAS_SPOOLDIR.len()
    } else if under(ATLAS_SPOOLDIR) {
        path.len()
    } else {
        return None;
    };
    if path.contains("/../") || path.ends_with("/..") {
        return None;
    }
    Some(len)
}

/// isspace(3) in the C locale, which do_oneoff splits the name off with
fn c_isspace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

/// Filter out telnet IAC sequences from raw data and return just the data
/// bytes (made text a whole line at a time, by LineSplitter)
fn filter_telnet_commands(data: &[u8]) -> Vec<u8> {
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

    result
}

/// Parse a command line into a TelnetCommand
///
/// Supports both JSON format and Atlas busybox CRONLINE format
pub fn parse_command(cmd: &str) -> TelnetCommand {
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
        "ONEOFF" => {
            // ONEOFF <path> <measurement_command> <args...>
            // One-shot measurement: parse as CRONLINE with interval=0
            if parts.len() >= 3 {
                // Reconstruct as a CRONLINE with interval=0 for the parser
                let measurement_parts = &parts[2..]; // skip ONEOFF and path
                let fake_cronline =
                    format!("CRONLINE 0 0 0 UNIFORM 0 {}", measurement_parts.join(" "));
                parse_cronline(&fake_cronline)
            } else {
                TelnetCommand::Unknown(cmd.to_string())
            }
        }
        "CRONTAB" => {
            // CRONTAB defines which cron directory following CRONLINEs belong to
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
    let interval: u64 = match tokens[1].parse() {
        Ok(v) => v,
        Err(_) => {
            warn!("CRONLINE invalid interval '{}': {}", tokens[1], cmd);
            return TelnetCommand::Unknown(cmd.to_string());
        }
    };
    // The cycle's phase: eperd runs cycle k at start_time + k * interval.
    let start_time: i64 = tokens[2].parse().unwrap_or(0);
    let end_time: i64 = tokens[3].parse().unwrap_or(0);
    let _spread_type = &tokens[4]; // UNIFORM, etc.
    let spread: u32 = tokens[5].parse().unwrap_or(0);

    // The rest is the measurement command
    let measurement_cmd = &tokens[6];
    let measurement_args = &tokens[7..];

    trace!(
        "CRONLINE: interval={}, start_time={}, end_time={}, spread={}, cmd={}, args={:?}",
        interval,
        start_time,
        end_time,
        spread,
        measurement_cmd,
        measurement_args
    );

    // Parse the measurement command
    let mut parsed = match measurement_cmd.as_str() {
        "evping" => parse_evping(measurement_args, interval, end_time, spread),
        "evtraceroute" => parse_evtraceroute(measurement_args, interval, end_time, spread),
        "evtdig" => parse_evtdig(measurement_args, interval, end_time, spread),
        "evhttpget" => parse_evhttpget(measurement_args, interval, end_time, spread),
        "evsslgetcert" => parse_evsslgetcert(measurement_args, interval, end_time, spread),
        "evntp" => parse_evntp(measurement_args, interval, end_time, spread),

        // Host-telemetry reporters. Match the official probe's RESULT JSON
        // so the RIPE backend sees no difference.
        "buddyinfo" => parse_buddyinfo(measurement_args, interval),
        "rptaddrs" => parse_rptaddrs(measurement_args, interval),

        // Result-upload transport. starla uploads natively via
        // starla-results::uploader, so this CRONLINE is redundant.
        "httppost" => {
            trace!("Ignoring httppost CRONLINE (uploader handles uploads natively)");
            TelnetCommand::Ignored(cmd.to_string())
        }

        // Hardware-probe spool plumbing: rotates result files between
        // data/new/ → data/out/ → data/storage/ and clears stale spool.
        // starla queues results in memory, so the spool model doesn't apply.
        "condmv" | "dfrm" => {
            trace!(
                "Ignoring {} CRONLINE (no on-disk spool to manage)",
                measurement_cmd
            );
            TelnetCommand::Ignored(cmd.to_string())
        }

        // conntrack: the controller occasionally schedules a 'conntrack'
        // applet, but it's not in the reference busybox tree we vendored
        // (reference/probe-busybox/), so the RESULT id and field names are
        // unknown. Stay silent rather than guess — see doc/en/protocol.html.
        "conntrack" => {
            trace!("Ignoring conntrack CRONLINE (no reference impl available)");
            TelnetCommand::Ignored(cmd.to_string())
        }

        _ => {
            warn!("Unknown measurement command: {}", measurement_cmd);
            TelnetCommand::Unknown(cmd.to_string())
        }
    };
    set_start_time(&mut parsed, start_time);
    parsed
}

/// Give a parsed measurement the CRONLINE's start time; the per-tool parsers
/// only see the tool's own arguments.
fn set_start_time(cmd: &mut TelnetCommand, start_time: i64) {
    let schedule = match cmd {
        TelnetCommand::Ping(s) => &mut s.schedule,
        TelnetCommand::Traceroute(s) => &mut s.schedule,
        TelnetCommand::Dns(s) => &mut s.schedule,
        TelnetCommand::Http(s) => &mut s.schedule,
        TelnetCommand::Tls(s) => &mut s.schedule,
        TelnetCommand::Ntp(s) => &mut s.schedule,
        _ => return,
    };
    schedule.start_time = start_time;
}

/// Parse a `buddyinfo [lowmem_kb] [logfile]` CRONLINE.
/// The applet hardcodes id=9001 and accepts a single positional `lowmem`.
fn parse_buddyinfo(args: &[String], interval: u64) -> TelnetCommand {
    let lowmem = args.first().and_then(|s| s.parse::<u32>().ok());
    TelnetCommand::HostTelemetry(HostTelemetrySpec {
        kind: HostTelemetryKind::Buddyinfo,
        interval,
        msm_id: None,
        lowmem,
    })
}

/// Parse `rptaddrs [-A msm_id] [-c cache] [-O output]` CRONLINE.
/// Per reference/probe-busybox/networking/rptaddrs.c: only `-A`, `-c`, `-O`
/// are accepted. We need `-A` (the result id); other paths are ignored.
fn parse_rptaddrs(args: &[String], interval: u64) -> TelnetCommand {
    let mut msm_id: Option<u32> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-A" => {
                msm_id = it.next().and_then(|s| s.parse().ok());
            }
            "-c" | "-O" => {
                // Path arg — drop next token; starla doesn't use the spool.
                let _ = it.next();
            }
            _ => {}
        }
    }
    TelnetCommand::HostTelemetry(HostTelemetrySpec {
        kind: HostTelemetryKind::Rptaddrs,
        interval,
        msm_id,
        lowmem: None,
    })
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
    let mut store_headers: u32 = 0;
    let mut http10 = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-4" => af = 4,
            "-6" => af = 6,
            "-0" => http10 = true,
            "-1" => http10 = false,
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
                if i + 1 < args.len() {
                    store_headers = args[i + 1].parse().unwrap_or(0);
                    i += 1;
                }
            }
            // httpget options with a value Starla does not use: their value
            // must not be taken for the URL.
            "-I" | "-R" | "-W" | "-b" | "--host" | "--post-file" | "--post-header"
            | "--post-footer" | "--read-limit" | "--store-body" | "--user-agent" | "--sni"
            | "--timeout" => {
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
        store_headers,
        http10,
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
/// evntp's defaults and 5130's gap bounds (probe-busybox/eperd/ntp.c).
pub const NTP_TIMEOUT_DEFAULT_MS: u32 = 1000;
pub const NTP_GAP_DEFAULT_MS: u32 = 4000;
pub const NTP_GAP_MIN_MS: u32 = 2000;
pub const NTP_GAP_MAX_MS: u32 = 60000;

fn parse_evntp(args: &[String], interval: u64, end_time: i64, spread: u32) -> TelnetCommand {
    let mut af: u8 = 4;
    let mut packets: u32 = 3;
    let mut timeout_ms = NTP_TIMEOUT_DEFAULT_MS;
    let mut gap_ms = NTP_GAP_DEFAULT_MS;
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
            "-w" => {
                if i + 1 < args.len() {
                    timeout_ms = args[i + 1].parse().unwrap_or(NTP_TIMEOUT_DEFAULT_MS);
                    i += 1;
                }
            }
            "-g" => {
                if i + 1 < args.len() {
                    // Unparsable is out of range: evntp's getopt32 "g+"
                    // would not have accepted it either.
                    gap_ms = args[i + 1].parse().unwrap_or(0);
                    i += 1;
                }
            }
            "-A" => {
                if i + 1 < args.len() {
                    msm_id = args[i + 1].parse().unwrap_or(0);
                    i += 1;
                }
            }
            // Options with a value Starla does not use.
            "-O" | "-s" | "-i" | "-B" => {
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

    // 5130's evntp refuses a gap outside its bounds ("bad gap").
    if !(NTP_GAP_MIN_MS..=NTP_GAP_MAX_MS).contains(&gap_ms) {
        warn!("evntp: bad gap {} ms, not scheduled", gap_ms);
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
        timeout_ms,
        gap_ms,
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
            store_headers: 0,
            http10: false,
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
            timeout_ms: NTP_TIMEOUT_DEFAULT_MS,
            gap_ms: NTP_GAP_DEFAULT_MS,
            spread,
            schedule,
        }),

        _ => TelnetCommand::Unknown(format!("Unknown measurement type: {}", msm_type)),
    }
}

#[cfg(test)]
mod session_tests;

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
                assert_eq!(spec.schedule.start_time, 274);
                assert_eq!(spec.schedule.stop_time, 1770761451);
                assert_eq!(spec.spread, Some(3));
            }
            _ => panic!("Expected Ping command, got {:?}", cmd),
        }
    }

    /// Every measurement kind keeps the CRONLINE's start time (the phase of
    /// its slots); they were all scheduled from "now" before.
    #[test]
    fn every_measurement_keeps_the_cronlines_start_time() {
        for line in [
            r#"CRONLINE 900 312 2000000000 UNIFORM 14 evtraceroute -6 -I -A "1004732" -O /home/atlas/data/new/7 www.google.com"#,
            r#"CRONLINE 86400 77 2000000000 UNIFORM 3 evtdig -6 -e 512 -h --retry 0 --timeout 5000 -A "156156173" -O /home/atlas/data/new/7 2620:fe::fe"#,
            r#"CRONLINE 86400 77 2000000000 UNIFORM 3 evhttpget -6 -1 -A "13023" -O /home/atlas/data/new/7 http://www.ripe.net/favicon.ico"#,
        ] {
            let start = line.split(' ').nth(2).unwrap().parse::<i64>().unwrap();
            let got = match parse_command(line) {
                TelnetCommand::Traceroute(s) => s.schedule.start_time,
                TelnetCommand::Dns(s) => s.schedule.start_time,
                TelnetCommand::Http(s) => s.schedule.start_time,
                // Not `{other:?}`: printing the parsed command trips CodeQL's
                // rust/cleartext-logging (an evsslgetcert line is sensitive).
                _ => panic!("unexpected command for {line}"),
            };
            assert_eq!(got, start, "{line}");
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

    #[test]
    fn test_parse_cronline_buddyinfo() {
        let cmd = parse_command("CRONLINE 600 0 0 UNIFORM 5 buddyinfo 2048");
        match cmd {
            TelnetCommand::HostTelemetry(spec) => {
                assert_eq!(spec.kind, HostTelemetryKind::Buddyinfo);
                assert_eq!(spec.interval, 600);
                assert_eq!(spec.lowmem, Some(2048));
                assert_eq!(spec.msm_id, None);
            }
            _ => panic!("Expected HostTelemetry/Buddyinfo, got {:?}", cmd),
        }
    }

    #[test]
    fn test_parse_cronline_rptaddrs() {
        let cmd = parse_command(
            r#"CRONLINE 14400 0 0 UNIFORM 30 rptaddrs -A 9104 -c /home/atlas/data/new/v6addr.vol -O /home/atlas/data/new/v6addr.txt"#,
        );
        match cmd {
            TelnetCommand::HostTelemetry(spec) => {
                assert_eq!(spec.kind, HostTelemetryKind::Rptaddrs);
                assert_eq!(spec.interval, 14400);
                assert_eq!(spec.msm_id, Some(9104));
            }
            _ => panic!("Expected HostTelemetry/Rptaddrs, got {:?}", cmd),
        }
    }

    #[test]
    fn test_parse_cronline_httppost_ignored() {
        // httppost shouldn't be dispatched — we upload natively.
        let cmd = parse_command(
            "CRONLINE 60 0 0 UNIFORM 5 httppost -O /home/atlas/data/out/foo --post-file foo",
        );
        assert!(matches!(cmd, TelnetCommand::Ignored(_)));
    }

    #[test]
    fn test_parse_cronline_condmv_ignored() {
        let cmd = parse_command(
            "CRONLINE 60 0 0 UNIFORM 5 condmv /home/atlas/data/new/foo /home/atlas/data/out/foo",
        );
        assert!(matches!(cmd, TelnetCommand::Ignored(_)));
    }

    #[test]
    fn test_parse_cronline_conntrack_ignored() {
        let cmd = parse_command("CRONLINE 600 0 0 UNIFORM 5 conntrack");
        assert!(matches!(cmd, TelnetCommand::Ignored(_)));
    }

    fn ntp(line: &str) -> Option<NtpSpec> {
        match parse_command(line) {
            TelnetCommand::Ntp(spec) => Some(spec),
            _ => None,
        }
    }

    /// evntp's defaults, -c/-w/-g honoured, and 5130's gap bounds.
    #[test]
    fn evntp_options_follow_5130() {
        let spec = ntp(r#"CRONLINE 1800 12 2000000000 UNIFORM 30 evntp -4 -A "30001" -O /home/atlas/data/new/7 ntp.ripe.net"#).unwrap();
        assert_eq!(
            (spec.packets, spec.timeout_ms, spec.gap_ms),
            (3, 1000, 4000)
        );
        assert_eq!(spec.target, "ntp.ripe.net");

        let spec = ntp(r#"CRONLINE 1800 12 2000000000 UNIFORM 30 evntp -6 -c 5 -w 2500 -g 6000 -s 48 -i eth0 -B 7 -A "30002" ntp.ripe.net"#).unwrap();
        assert_eq!(
            (spec.packets, spec.timeout_ms, spec.gap_ms),
            (5, 2500, 6000)
        );
        assert_eq!(
            spec.target, "ntp.ripe.net",
            "option values are not the target"
        );

        for bad in ["1999", "60001", "x"] {
            let line = format!(
                r#"CRONLINE 1800 12 2000000000 UNIFORM 30 evntp -4 -g {bad} -A "30003" ntp.ripe.net"#
            );
            assert!(ntp(&line).is_none(), "gap {bad} must be refused");
        }
        for ok in ["2000", "60000"] {
            let line = format!(
                r#"CRONLINE 1800 12 2000000000 UNIFORM 30 evntp -4 -g {ok} -A "30003" ntp.ripe.net"#
            );
            assert!(ntp(&line).is_some(), "gap {ok} is in bounds");
        }
    }

    /// The controller's ip-echo and favicon built-ins, as pushed to 1018212.
    #[test]
    fn evhttpget_keeps_store_headers_family_and_version() {
        let spec = match parse_command(
            r#"CRONLINE 3600 3751 1794447121 UNIFORM 3 evhttpget -6 -1 --store-headers 1024 -A "13027" -O /home/atlas/data/new/7 http://[2001:67c:2e8:16::c100:68d]"#,
        ) {
            TelnetCommand::Http(s) => s,
            other => panic!("{other:?}"),
        };
        assert_eq!(spec.msm_id, 13027);
        assert_eq!(spec.url, "http://[2001:67c:2e8:16::c100:68d]");
        assert_eq!((spec.af, spec.store_headers, spec.http10), (6, 1024, false));
        assert_eq!(spec.schedule.start_time, 3751);

        let spec = match parse_command(
            r#"CRONLINE 86400 24620 1794447121 UNIFORM 3 evhttpget -4 -0 --user-agent x --timeout 5000 -A "12023" -O /home/atlas/data/new/7 http://www.ripe.net/favicon.ico"#,
        ) {
            TelnetCommand::Http(s) => s,
            other => panic!("{other:?}"),
        };
        assert_eq!((spec.af, spec.store_headers, spec.http10), (4, 0, true));
        assert_eq!(
            spec.url, "http://www.ripe.net/favicon.ico",
            "option values are not the URL"
        );
    }
}
