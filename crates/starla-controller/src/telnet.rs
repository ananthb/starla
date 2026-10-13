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
    /// A measurement from a ONEOFF line: telnetd queues it for eooqd
    /// (telnetd.c do_oneoff, 1615-1690), which runs it once and posts its
    /// result on its own, with `&SRC=oneoff`
    Oneoff(Box<TelnetCommand>),
    /// An httppost CRONLINE: when and where the probe posts its results
    Httppost(HttppostSpec),
    /// A crontab the controller sent whole: its CRONLINEs, handed on once
    /// a line other than a CRONLINE has ended it
    Crontab(CrontabSpec),
    /// Status query
    Status,
    /// Stop a running measurement
    Stop(u64),
    /// Ignored command (known but not handled, e.g. condmv, internal
    /// commands)
    Ignored(String),
    /// Unknown command
    Unknown(String),
}

/// A crontab, as the controller sends one: a CRONTAB line, its CRONLINEs,
/// and a line that ends it (telnetd.c:1230-1262). telnetd writes each
/// CRONLINE to `<dir>/root.new` as it comes (add_to_crontab, :1520-1533);
/// only end_crontab (:1535-1600), on the line that ends the crontab, renames
/// that to `<dir>/root` and tells the directory's cron daemon to read it
/// again (`cron.update`), and the daemon then reads the whole file anew. A
/// crontab still open when its session ends is closed and never renamed,
/// whether the connection went (kill_session, :1302-1311) or a newer session
/// took over (:1112-1122): the directory keeps the crontab it had. So a
/// crontab is handed on whole, once it has ended, or not at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrontabSpec {
    /// The directory, rebased as start_crontab rebases it (:1484-1499):
    /// `/home/atlas/crons/7` is `/var/spool/ripe-atlas/crons/7`
    pub dir: String,
    /// Its CRONLINEs, in order
    pub lines: Vec<CrontabLine>,
}

/// A CRONLINE of a crontab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrontabLine {
    /// The line as add_to_crontab writes it to the file: what follows
    /// "CRONLINE " (telnetd.c:1258)
    pub line: String,
    /// The line as parse_command reads it
    pub command: TelnetCommand,
}

impl CrontabLine {
    /// The line's command text, eperd's cl_Shell: what follows its five
    /// leading fields (interval, start time, end time, distribution and its
    /// parameter), as SynchronizeFile's config_read(…, 6, 1, "# \t",
    /// PARSE_NORMAL) cuts it (eperd.c:550, 566-567, 612;
    /// libbb/parse_config.c:162-243): the fields are apart at runs of
    /// blanks, a '#' ends the line, and the blanks at its ends are trimmed.
    /// With the interval and start time, it is what Insert() tells one line
    /// from another by (eperd.c:789-791, 806-808).
    pub fn shell(&self) -> &str {
        let blank = |c: char| c == ' ' || c == '\t';
        let line = self.line.split('#').next().unwrap_or_default();
        let mut rest = line.trim_start_matches(blank);
        for _ in 0..5 {
            let field = rest.find(blank).unwrap_or(rest.len());
            rest = rest[field..].trim_start_matches(blank);
        }
        rest.trim_end_matches(blank)
    }
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

/// An httppost CRONLINE, with the options of networking/httppost.c that
/// matter to Starla, which posts from its queues rather than spool files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttppostSpec {
    pub interval: u64,
    /// The cycles' phase: perd runs cycle k at start_time + k * interval
    pub start_time: i64,
    pub end_time: i64,
    /// UNIFORM spread in seconds (0 for NONE)
    pub spread: u32,
    /// The one URL, e.g. `http://127.0.0.1:8080/?PROBE_ID=…&SESSION_ID=…`
    pub url: String,
    /// `--post-dir`: the results go in the body (the main line); without
    /// it, header and footer only (the `&SRC=timesync` line)
    pub post_dir: bool,
    /// `--set-time N`: judge the clock against the answer's Date
    pub set_time: Option<i64>,
    /// `--local-time`: send a `Unix-Date:` header
    pub local_time: bool,
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
    /// evtraceroute's options: -I/-U/-T, -c, -f, -m, -S, -a, -w, -z, …
    pub opts: starla_common::TracerouteOptions,
    pub spread: Option<u32>,
    pub schedule: ScheduleSpec,
}

/// DNS measurement specification: evtdig's command line, with tdig_init()'s
/// defaults (eperd/evtdig.c:2397)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsSpec {
    pub msm_id: u64,
    /// `-B`: the bundle, reported as `"bundle"`
    pub bundle: Option<u64>,
    /// The server, evtdig's one argument; empty with `--resolv`
    pub target: String,
    /// 4 (`-4`, and the default) or 6 (`-6`), or 0 for either (`-4 -6`, `-a`,
    /// and `--resolv`, which asks each resolver whatever its family)
    pub af: u8,
    pub protocol: String, // UDP, TCP (`-t`)
    /// `--port` (53)
    pub port: u16,
    /// The query type and class, numbers: TXT (16) CH (3) unless an option
    /// says otherwise
    pub qtype: u16,
    pub qclass: u16,
    /// The name as given, ending with a dot, `$p`/`$r`/`$t` unexpanded
    pub query_argument: String,
    /// `--resolv`: ask the probe's own resolvers (/etc/resolv.conf)
    pub resolv: bool,
    /// `-R`: set RD; evtdig never sets it otherwise (`--resolv` aside)
    pub rd: bool,
    /// `--ad`, `--cd`
    pub ad: bool,
    pub cd: bool,
    /// `-d`: DO
    pub dnssec: bool,
    /// `-n`: NSID
    pub nsid: bool,
    /// `-c`, `--client-subnet`
    pub client_subnet: bool,
    /// `--cookies`
    pub cookies: bool,
    /// `-e`: EDNS0 UDP size (512)
    pub edns0: u32,
    /// `--edns-version`, `--edns-flags`, `--edns-option`
    pub edns_version: u8,
    pub edns_flags: u16,
    pub edns_option: u16,
    /// `--qbuf`: report the query
    pub qbuf: bool,
    /// Report the reply: true unless `--noabuf`
    pub abuf: bool,
    /// `--p_probe_id`
    pub prepend_probe_id: bool,
    /// `--timeout` (ms, 5000)
    pub timeout_ms: u32,
    /// `--retry` (0)
    pub retry: u32,
    /// `--ttl`: report a UDP reply's TTL (hop limit) as `"ttl"`
    #[serde(default)]
    pub ttl: bool,
    pub spread: Option<u32>,
    pub schedule: ScheduleSpec,
}

impl Default for DnsSpec {
    fn default() -> Self {
        Self {
            msm_id: 0,
            bundle: None,
            target: String::new(),
            af: 4,
            protocol: "UDP".to_string(),
            port: 53,
            qtype: 16,
            qclass: 3,
            query_argument: String::new(),
            resolv: false,
            rd: false,
            ad: false,
            cd: false,
            dnssec: false,
            nsid: false,
            client_subnet: false,
            cookies: false,
            edns0: 512,
            edns_version: 0,
            edns_flags: 0,
            edns_option: 0,
            qbuf: false,
            abuf: true,
            prepend_probe_id: false,
            timeout_ms: 5000,
            retry: 0,
            ttl: false,
            spread: None,
            schedule: ScheduleSpec::default(),
        }
    }
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
    /// `-4`/`-6`; 0 for neither (evsslgetcert resolves either family)
    pub af: u8,
    /// `-h`: the server_name to send; evsslgetcert sends none without it
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

/// The most lines of a crontab its cron daemon reads. telnetd writes every
/// CRONLINE to the file, but eperd and perd read "root" with maxLines
/// 65535, decremented before each line (eperd.c:542-549, perd.c:458-474):
/// the 65535th line and those after it are never read ("too many lines",
/// eperd.c:638-640). config_read does not count a blank or comment-only
/// line; this counts every CRONLINE, and RIPE's controller sends no such
/// line.
const CRONTAB_MAX_LINES: usize = 65534;

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
    // The crontab DO_CRONTAB is taking, root.new: dropped unsent if the
    // session ends before a line ends the crontab
    let mut crontab: Option<CrontabSpec> = None;

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
                    // add_to_crontab, no reply (telnetd.c:1241-1262): the
                    // line goes into root.new, and nothing acts on it until
                    // the crontab has ended
                    match &mut crontab {
                        Some(crontab) if crontab.lines.len() < CRONTAB_MAX_LINES => {
                            crontab.lines.push(CrontabLine {
                                line: line[CMD_CRONLINE.len()..].to_string(),
                                command: parse_command(&line),
                            });
                        }
                        Some(crontab) => debug!(
                            "Telnet: crontab {} has {} lines already, the most its cron daemon reads; line not taken",
                            crontab.dir, CRONTAB_MAX_LINES
                        ),
                        None => {}
                    }
                }
                ConnectionState::GetCmd | ConnectionState::DoCrontab => {
                    // Any other line ends the crontab — end_crontab installs
                    // it (telnetd.c:1535-1600) — and is the next command
                    // ("goto do_cmd", :1242-1256).
                    if let Some(ended) = crontab.take() {
                        if !forward(&command_tx, TelnetCommand::Crontab(ended)).await {
                            break 'session;
                        }
                    }
                    let (response, command, next) = get_cmd(&line);
                    if let Some(response) = response {
                        reply(&mut writer, response).await;
                    }
                    match command {
                        // start_crontab opened root.new: its CRONLINEs follow
                        Some(TelnetCommand::Crontab(started)) => crontab = Some(started),
                        Some(command) => {
                            if !forward(&command_tx, command).await {
                                break 'session;
                            }
                        }
                        None => {}
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
/// command to hand to the probe, if any, and the next state. For a CRONTAB
/// line the command is the crontab it starts, still empty, which the session
/// fills and hands on once it has ended.
fn get_cmd(line: &str) -> (Option<&'static str>, Option<TelnetCommand>, ConnectionState) {
    use ConnectionState::{DoCrontab, EomSeen, GetCmd};

    if let Some(dir) = line.strip_prefix(CMD_CRONTAB) {
        // start_crontab (telnetd.c:1471-1518); its CRONLINEs follow
        return match crontab_dir_check(dir) {
            Ok(()) => {
                let crontab = CrontabSpec {
                    dir: rebased_crons_path(dir).unwrap_or_else(|| dir.to_string()),
                    lines: Vec::new(),
                };
                (None, Some(TelnetCommand::Crontab(crontab)), DoCrontab)
            }
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
/// (libbb/validate_filename.c:3-131): the path rebased onto the spool
/// directory, or None where the reference answers BAD_PATH. The path is
/// /home/atlas/crons/… or <spooldir>/crons/… (properties 1 and 2), holds no
/// "/../" and does not end in "/.." (3 and 4).
fn rebased_crons_path(path: &str) -> Option<String> {
    let under = |base: &str| {
        path.strip_prefix(base)
            .filter(|rest| rest.starts_with("/crons/"))
    };
    let rebased = if let Some(rest) = under(ATLAS_HOME) {
        format!("{ATLAS_SPOOLDIR}{rest}")
    } else if under(ATLAS_SPOOLDIR).is_some() {
        path.to_string()
    } else {
        return None;
    };
    if path.contains("/../") || path.ends_with("/..") {
        return None;
    }
    Some(rebased)
}

/// The length of [`rebased_crons_path`], which telnetd's buffers must fit.
fn rebased_crons_path_len(path: &str) -> Option<usize> {
    rebased_crons_path(path).map(|rebased| rebased.len())
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
                // Marked, so its result is posted as eooqd posts one-offs
                // and not in the next main upload.
                match parse_cronline(&fake_cronline) {
                    unknown @ TelnetCommand::Unknown(_) => unknown,
                    measurement => TelnetCommand::Oneoff(Box::new(measurement)),
                }
            } else {
                TelnetCommand::Unknown(cmd.to_string())
            }
        }
        "CRONTAB" => {
            // CRONTAB defines which cron directory following CRONLINEs
            // belong to; the session collects them (CrontabSpec)
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

        // Result upload. Starla posts from its own queues, but on this
        // line's slots and to its URL, as perd runs it.
        "httppost" => {
            let spread = if tokens[4] == "UNIFORM" { spread } else { 0 };
            parse_httppost(measurement_args, interval, start_time, end_time, spread).unwrap_or_else(
                |why| {
                    warn!("Ignoring httppost CRONLINE ({}): {}", why, cmd);
                    TelnetCommand::Ignored(cmd.to_string())
                },
            )
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

/// Parse an httppost CRONLINE's arguments as httppost_main's getopt_long
/// does (httppost.c:44-56, 135-182): `-A`/`-O` and the long options, any
/// order, `--opt=value` too, and exactly one URL. perd's atlas_run takes an
/// output redirection (`>> /home/atlas/data/new/main`, or `>>file`) out of
/// the arguments (perd.c:1031-1060). `--post-file` is refused: Starla has
/// no such file to post.
fn parse_httppost(
    args: &[String],
    interval: u64,
    start_time: i64,
    end_time: i64,
    spread: u32,
) -> Result<TelnetCommand, &'static str> {
    if interval == 0 {
        // perd.c:494-503
        return Err("interval 0");
    }
    let mut urls = Vec::new();
    let mut post_dir = false;
    let mut set_time = None;
    let mut local_time = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if let Some(file) = arg.strip_prefix(">>").or_else(|| arg.strip_prefix('>')) {
            if file.is_empty() {
                it.next();
            }
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if arg.starts_with("--") => (name, Some(value.to_string())),
            _ => (arg.as_str(), None),
        };
        let mut value = || inline.clone().or_else(|| it.next().cloned());
        match name {
            "--delete-file" => {}
            "--local-time" => local_time = true,
            "-A" | "-O" | "--maxpostsize" | "--post-header" | "--post-footer" | "--timeout" => {
                value().ok_or("option without its value")?;
            }
            "--post-dir" => {
                value().ok_or("option without its value")?;
                post_dir = true;
            }
            "--set-time" => {
                let t = value().ok_or("option without its value")?;
                set_time = Some(t.parse().map_err(|_| "bad --set-time")?);
            }
            "--post-file" => return Err("--post-file: no such file in Starla"),
            opt if opt.starts_with('-') => return Err("unknown option"),
            url => urls.push(url.to_string()),
        }
    }
    let [url] = <[String; 1]>::try_from(urls).map_err(|_| "exactly one url expected")?;
    Ok(TelnetCommand::Httppost(HttppostSpec {
        interval,
        start_time,
        end_time,
        spread,
        url,
        post_dir,
        set_time,
        local_time,
    }))
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

/// Parse evping command arguments as ping_init does (ping.c:1262-1281,
/// 1347-1350). PING_OPT_STRING is "!46eprc:s:A:B:O:i:I:R:W:": -c, -s, -A,
/// -B, -O, -i, -I, -R and -W take a value, -4, -6, -e, -p and -r do not.
/// The family is IPv6 unless -4 is given, and -s is 0 without one: the
/// least data, 32 bytes (see starla_measurements::ping::data_len).
///
/// -e is read and not followed: with it ping_init looks the name up once,
/// with libc, and takes no measurement if that fails or the address is
/// refused, and every line then has "ttr":0.000000 (ping.c:1352-1384).
/// The controller sends no -e (none of the twin's evping lines of
/// 2026-10-13 has it); only the probe's own SOS ping, msm 9017, does
/// (bin/ripe-atlas.in), which belongs to registration.
///
/// Usage: evping [-4|-6] [-c count] [-s size] [-i interval] [-A msm_id] [-O
/// output] target
fn parse_evping(args: &[String], interval: u64, end_time: i64, spread: u32) -> TelnetCommand {
    let mut v4 = false;
    let mut count: u32 = 3;
    let mut size: u16 = 0;
    let mut packet_interval: u32 = 1000;
    let mut msm_id: u64 = 0;
    let mut target = String::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-4" => v4 = true,
            "-6" | "-e" | "-p" | "-r" => {}
            "-c" | "-s" | "-A" | "-i" | "-B" | "-O" | "-I" | "-R" | "-W" => {
                let value = args.get(i + 1).map(String::as_str).unwrap_or("");
                match arg.as_str() {
                    "-c" => count = value.parse().unwrap_or(3),
                    "-s" => size = value.parse().unwrap_or(0),
                    "-A" => msm_id = value.parse().unwrap_or(0),
                    "-i" => packet_interval = value.parse().unwrap_or(1000),
                    // -B bundle, -O output file, -I interface, -R/-W
                    // fuzzing files
                    _ => {}
                }
                i += 1;
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
    // ping.c:1347-1350: -4 wins, whatever else is given
    let af: u8 = if v4 { 4 } else { 6 };

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
        packet_interval,
        spread: Some(spread),
        schedule: ScheduleSpec {
            interval,
            start_time: 0, // Start now
            stop_time: end_time,
        },
    })
}

/// Parse evtraceroute command arguments as traceroute_init does
/// (probe-busybox/eperd/traceroute.c:4203-4466). TRACEROUTE_OPT_STRING is
/// "!46IUFrTa:b:c:f:g:i:m:p:t:w:z:A:B:O:S:H:D:R:W:": -4 -6 -I -U -F -r -T
/// are flags, the rest take a value, and getopt's rules hold (flags may be
/// bundled, a value may be attached, options may follow the target).
/// Exactly one target; -a -b -c -f -g -m -t -w -z -S -H -D are
/// non-negative numbers ("+"), and -4 with -6, an unknown option, a bad
/// number or -S over 4096 refuse the measurement, as traceroute_init
/// returns NULL for them. The family is IPv4 unless -6 is given.
///
/// Usage: evtraceroute [-4|-6] [-I|-U|-T] [-F] [-a paris_mod] [-b paris_base]
/// [-c count] [-f first_hop] [-g gap_limit] [-i interface] [-m max_hops]
/// [-p port] [-t tos] [-w timeout_ms] [-z dup_timeout_ms] [-A msm_id]
/// [-B bundle] [-O output] [-S size] [-H hbh_size] [-D dest_size] target
///
/// -a is parismod, 16 without one, -b parisbase, 0 without one, and -p the
/// destination port (traceroute.c:4247-4248, 4259-4262), not a Paris id:
/// Starla read -p as the paris_id (RIPE stored "paris_id":80 for -p 80,
/// starla-25603345.json) and dropped -a.
fn parse_evtraceroute(args: &[String], interval: u64, end_time: i64, spread: u32) -> TelnetCommand {
    let refuse = |why: &str| {
        warn!("evtraceroute: {}, not scheduled: {:?}", why, args);
        TelnetCommand::Unknown(format!("evtraceroute {:?}", args))
    };
    let mut opts = starla_common::TracerouteOptions::default();
    let (mut v4, mut v6, mut icmp, mut tcp) = (false, false, false, false);
    let mut msm_id: u64 = 0;
    let mut targets: Vec<&String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        let flags = match arg.strip_prefix('-') {
            Some(f) if !f.is_empty() && arg != "--" => f,
            _ if arg == "--" => {
                targets.extend(&args[i..]);
                break;
            }
            _ => {
                targets.push(arg);
                continue;
            }
        };
        for (at, c) in flags.char_indices() {
            match c {
                '4' => v4 = true,
                '6' => v6 = true,
                'I' => icmp = true,
                'T' => tcp = true,
                'F' => opts.dont_fragment = true,
                // -U is the default; -r (delayed name lookup) is always on.
                'U' | 'r' => {}
                'a' | 'b' | 'c' | 'f' | 'g' | 'i' | 'm' | 'p' | 't' | 'w' | 'z' | 'A' | 'B'
                | 'O' | 'S' | 'H' | 'D' | 'R' | 'W' => {
                    let rest = &flags[at + c.len_utf8()..];
                    let value = if !rest.is_empty() {
                        rest.to_string()
                    } else if i < args.len() {
                        i += 1;
                        args[i - 1].clone()
                    } else {
                        return refuse(&format!("-{} without a value", c));
                    };
                    // getopt32's "+": xatoi_positive.
                    let num = || value.parse::<u32>().ok().filter(|n| *n <= i32::MAX as u32);
                    let Some(n) = (match c {
                        'i' | 'p' | 'A' | 'B' | 'O' | 'R' | 'W' => Some(0),
                        _ => num(),
                    }) else {
                        return refuse(&format!("bad number for -{}: {}", c, value));
                    };
                    // eperd keeps most of these in unsigned chars and
                    // shorts: the same truncation.
                    match c {
                        'a' => opts.paris_mod = n as u8,
                        'b' => opts.paris_base = n as u8,
                        'c' => opts.count = n as u8,
                        'f' => opts.first_hop = n as u8,
                        'g' => opts.gap_limit = n as u8,
                        'm' => opts.max_hops = n as u8,
                        't' => opts.tos = n,
                        'w' => opts.timeout_ms = n,
                        'z' => opts.dup_timeout_ms = n,
                        'S' => {
                            if n > starla_common::TracerouteOptions::MAX_SIZE as u32 {
                                return refuse("max. packet size too big");
                            }
                            opts.size = n as u16;
                        }
                        'H' => opts.hbh_opt_size = n as u16,
                        'D' => opts.dest_opt_size = n as u16,
                        'i' => opts.interface = Some(value),
                        // traceroute_init keeps -p as a string (its own
                        // check, 4349-4357, only runs without delayed name
                        // lookup, which is always on: 4278-4280); the run
                        // hands it to evdns_getaddrinfo as the service
                        // (4971), where a number from 0 to 65535 is the
                        // port, 0 included. Anything else would be a
                        // service name from /etc/services or, failing
                        // that, a "name resolution failed" result; the
                        // controller only sends numbers, and Starla
                        // refuses the rest here.
                        'p' => match value.parse::<u16>() {
                            Ok(port) => opts.dest_port = port,
                            _ => return refuse(&format!("bad port {}", value)),
                        },
                        'A' => match value.parse() {
                            Ok(id) => msm_id = id,
                            Err(_) => return refuse(&format!("bad atlas ID '{}'", value)),
                        },
                        'B' => {
                            if !value.bytes().all(|b| b.is_ascii_digit()) {
                                return refuse(&format!("bad bundle ID '{}'", value));
                            }
                            opts.bundle = Some(value);
                        }
                        // -O output file, -R/-W fuzzing files
                        _ => {}
                    }
                    break;
                }
                _ => return refuse(&format!("unknown option -{}", c)),
            }
        }
    }

    if v4 && v6 {
        return refuse("-4 and -6");
    }
    // report() calls it ICMP if -I is given, else TCP if -T, else UDP.
    opts.protocol = if icmp {
        starla_common::TracerouteProtocol::ICMP
    } else if tcp {
        starla_common::TracerouteProtocol::TCP
    } else {
        starla_common::TracerouteProtocol::UDP
    };
    let target = match targets.as_slice() {
        [t] => (*t).clone(),
        _ => return refuse("not exactly one target"),
    };
    let af: u8 = if v6 { 6 } else { 4 };

    trace!(
        "Parsed evtraceroute: msm_id={}, target={}, af={}, opts={:?}, interval={}",
        msm_id,
        target,
        af,
        opts,
        interval
    );

    TelnetCommand::Traceroute(TracerouteSpec {
        msm_id,
        target,
        af,
        opts,
        spread: Some(spread),
        schedule: ScheduleSpec {
            interval,
            start_time: 0,
            stop_time: end_time,
        },
    })
}

/// What one evtdig option sets (tdig_init()'s getopt_long() switch,
/// eperd/evtdig.c:2490).
#[derive(Debug, Clone, Copy, PartialEq)]
enum DigOpt {
    /// `--a NAME`, `--aaaa NAME`, … `--soa NAME` (`-s`): this type, class IN
    Query(u16),
    Type,
    Class,
    QueryName,
    /// `-h`, `-i`, `-b`, `-r`: this CH TXT name
    Chaos(&'static str),
    V4,
    V6,
    Both,
    Atlas,
    Bundle,
    ClientSubnet,
    Cookies,
    Do,
    Edns0,
    EdnsVersion,
    EdnsFlags,
    EdnsOption,
    Ipv6DestOption,
    Interface,
    Nsid,
    Port,
    OutFile,
    Rd,
    Tcp,
    Timeout,
    Qbuf,
    NoAbuf,
    Tls,
    Https,
    SniCertName,
    HttpsPath,
    Ttl,
    Ad,
    Cd,
    Retry,
    Resolv,
    PrependProbeId,
    Evdns,
    WriteResponse,
    ReadResponse,
    /// In the option tables but not in the switch: tdig_init() refuses it
    Refused,
}

/// evtdig's long options (eperd/evtdig.c:680): name, takes an argument,
/// what it sets.
const EVTDIG_LONGOPTS: &[(&str, bool, DigOpt)] = &[
    ("a", true, DigOpt::Query(1)),
    ("aaaa", true, DigOpt::Query(28)),
    ("any", true, DigOpt::Query(255)),
    ("afsdb", true, DigOpt::Query(18)),
    ("apl", true, DigOpt::Query(42)),
    ("axfr", true, DigOpt::Query(252)),
    ("caa", true, DigOpt::Query(257)),
    ("cert", true, DigOpt::Query(37)),
    ("cname", true, DigOpt::Query(5)),
    ("dlv", true, DigOpt::Query(32769)),
    ("dname", true, DigOpt::Query(39)),
    ("dnskey", true, DigOpt::Query(48)),
    ("ds", true, DigOpt::Query(43)),
    ("ipseckey", true, DigOpt::Query(45)),
    ("key", true, DigOpt::Query(25)),
    ("loc", true, DigOpt::Query(29)),
    ("mx", true, DigOpt::Query(15)),
    ("naptr", true, DigOpt::Query(35)),
    ("ns", true, DigOpt::Query(2)),
    ("nsec", true, DigOpt::Query(47)),
    ("nsec3", true, DigOpt::Query(50)),
    ("nsec3param", true, DigOpt::Query(51)),
    ("ptr", true, DigOpt::Query(12)),
    ("rrsig", true, DigOpt::Query(46)),
    ("rp", true, DigOpt::Query(17)),
    ("soa", true, DigOpt::Query(6)),
    ("sig", true, DigOpt::Query(24)),
    ("spf", true, DigOpt::Query(99)),
    ("sshfp", true, DigOpt::Query(44)),
    ("srv", true, DigOpt::Query(33)),
    ("ta", true, DigOpt::Query(32768)),
    ("tlsa", true, DigOpt::Query(52)),
    ("tsig", true, DigOpt::Query(250)),
    ("txt", true, DigOpt::Query(16)),
    ("type", true, DigOpt::Type),
    ("class", true, DigOpt::Class),
    ("query", true, DigOpt::QueryName),
    ("hostname.bind", false, DigOpt::Chaos("hostname.bind.")),
    ("id.server", false, DigOpt::Chaos("id.server.")),
    ("version.bind", false, DigOpt::Chaos("version.bind.")),
    ("version.server", false, DigOpt::Chaos("version.server.")),
    ("c_output", false, DigOpt::Refused),
    ("ad", false, DigOpt::Ad),
    ("cd", false, DigOpt::Cd),
    ("client-subnet", false, DigOpt::ClientSubnet),
    ("cookies", false, DigOpt::Cookies),
    ("do", false, DigOpt::Do),
    ("evdns", false, DigOpt::Evdns),
    ("noabuf", false, DigOpt::NoAbuf),
    ("nsid", false, DigOpt::Nsid),
    ("p_probe_id", false, DigOpt::PrependProbeId),
    ("qbuf", false, DigOpt::Qbuf),
    ("resolv", false, DigOpt::Resolv),
    ("tls", false, DigOpt::Tls),
    ("https", false, DigOpt::Https),
    ("sni-cert-name", true, DigOpt::SniCertName),
    ("https-path", true, DigOpt::HttpsPath),
    ("ttl", false, DigOpt::Ttl),
    ("edns0", true, DigOpt::Edns0),
    ("edns-flags", true, DigOpt::EdnsFlags),
    ("edns-option", true, DigOpt::EdnsOption),
    ("edns-version", true, DigOpt::EdnsVersion),
    ("ipv6-dest-option", true, DigOpt::Ipv6DestOption),
    ("out-file", true, DigOpt::OutFile),
    ("port", true, DigOpt::Port),
    ("retry", true, DigOpt::Retry),
    ("timeout", true, DigOpt::Timeout),
    ("write-response", true, DigOpt::WriteResponse),
    ("read-response", true, DigOpt::ReadResponse),
];

/// evtdig's short options, "46adD:e:tbhinqO:Rrs:A:B:I:?": `-D`, `-q` and
/// `-?` are in the string but not the switch, so they are refused.
fn evtdig_shortopt(c: char) -> Option<(bool, DigOpt)> {
    Some(match c {
        '4' => (false, DigOpt::V4),
        '6' => (false, DigOpt::V6),
        'a' => (false, DigOpt::Both),
        'd' => (false, DigOpt::Do),
        'D' => (true, DigOpt::Refused),
        'e' => (true, DigOpt::Edns0),
        't' => (false, DigOpt::Tcp),
        'b' => (false, DigOpt::Chaos("version.bind.")),
        'h' => (false, DigOpt::Chaos("hostname.bind.")),
        'i' => (false, DigOpt::Chaos("id.server.")),
        'n' => (false, DigOpt::Nsid),
        'q' | '?' => (false, DigOpt::Refused),
        'O' => (true, DigOpt::OutFile),
        'R' => (false, DigOpt::Rd),
        'r' => (false, DigOpt::Chaos("version.server.")),
        's' => (true, DigOpt::Query(6)),
        'A' => (true, DigOpt::Atlas),
        'B' => (true, DigOpt::Bundle),
        'I' => (true, DigOpt::Interface),
        _ => return None,
    })
}

/// getopt_long() over evtdig's arguments: options (`-46`, `-e512`,
/// `--qbuf`, `--type=1`, a unique prefix of a long name) wherever they are,
/// and the rest in order. Err on what getopt_long() would answer `?` to.
fn evtdig_getopt(args: &[String]) -> Result<(Vec<(DigOpt, String)>, Vec<String>), String> {
    let mut opts = Vec::new();
    let mut operands = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            operands.extend(it.by_ref().cloned());
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            let exact = EVTDIG_LONGOPTS.iter().find(|o| o.0 == name);
            let mut prefixed = EVTDIG_LONGOPTS.iter().filter(|o| o.0.starts_with(name));
            let &(_, has_arg, opt) = match (exact, prefixed.next(), prefixed.next()) {
                (Some(o), _, _) => o,
                (None, Some(o), None) => o,
                (None, Some(_), Some(_)) => return Err(format!("ambiguous option {arg}")),
                (None, None, _) => return Err(format!("unknown option {arg}")),
            };
            let value = match (has_arg, inline) {
                (true, Some(v)) => v.to_string(),
                (true, None) => it
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("{arg} needs an argument"))?,
                (false, Some(_)) => return Err(format!("{arg} takes no argument")),
                (false, None) => String::new(),
            };
            opts.push((opt, value));
        } else if arg.len() > 1 && arg.starts_with('-') {
            for (i, c) in arg[1..].char_indices() {
                let (has_arg, opt) =
                    evtdig_shortopt(c).ok_or_else(|| format!("unknown option -{c}"))?;
                if !has_arg {
                    opts.push((opt, String::new()));
                    continue;
                }
                let rest = &arg[1 + i + c.len_utf8()..];
                let value = if rest.is_empty() {
                    it.next()
                        .cloned()
                        .ok_or_else(|| format!("-{c} needs an argument"))?
                } else {
                    rest.to_string()
                };
                opts.push((opt, value));
                break;
            }
        } else {
            operands.push(arg.clone());
        }
    }
    Ok((opts, operands))
}

/// strtoul(s, NULL, base) as far as it reads: leading digits, 0 if none
/// (base 0 also takes 0x… and 0…).
fn strtoul(s: &str, base: u32) -> u64 {
    let s = s.trim_start();
    let (s, radix) = match base {
        0 if s.starts_with("0x") || s.starts_with("0X") => (&s[2..], 16),
        0 if s.starts_with('0') => (s, 8),
        0 => (s, 10),
        b => (s, b),
    };
    let end = s.find(|c: char| !c.is_digit(radix)).unwrap_or(s.len());
    u64::from_str_radix(&s[..end], radix).unwrap_or(0)
}

/// Parse an evtdig CRONLINE as tdig_init() and argProcess() do
/// (eperd/evtdig.c:2293, 2397): the reference's options with its defaults
/// (TXT CH, no RD, EDNS0 size 512 but no OPT record unless something needs
/// one), and what it refuses refused (an unknown option, a name without its
/// final dot, no server or more than one).
///
/// Taken but not acted on: `-I` (bind to an interface),
/// `--ipv6-dest-option`, `-O`, `--write-response` and `--read-response`.
/// Refused: `--tls` and `--https`, which Starla can't do; it would put
/// plain DNS on port 853 or 443.
fn parse_evtdig(args: &[String], interval: u64, end_time: i64, spread: u32) -> TelnetCommand {
    let unknown = |why: &str| {
        warn!("evtdig: {}, args={:?}", why, args);
        TelnetCommand::Unknown(format!("evtdig {:?}", args))
    };
    let (opts, operands) = match evtdig_getopt(args) {
        Ok(parsed) => parsed,
        Err(why) => return unknown(&why),
    };
    let mut spec = DnsSpec {
        spread: Some(spread),
        schedule: ScheduleSpec {
            interval,
            start_time: 0,
            stop_time: end_time,
        },
        ..DnsSpec::default()
    };
    let (mut v4, mut v6) = (false, false);
    let mut query: Option<String> = None;
    for (opt, value) in opts {
        let num = |base| strtoul(&value, base);
        match opt {
            DigOpt::Query(qtype) => {
                spec.qtype = qtype;
                spec.qclass = 1;
                query = Some(value);
            }
            DigOpt::Type => {
                spec.qtype = num(10) as u16;
                if spec.qclass == 0 {
                    spec.qclass = 1;
                }
            }
            DigOpt::Class => spec.qclass = num(10) as u16,
            DigOpt::QueryName => query = Some(value),
            DigOpt::Chaos(name) => query = Some(name.to_string()),
            DigOpt::V4 => v4 = true,
            DigOpt::V6 => v6 = true,
            DigOpt::Both => (v4, v6) = (true, true),
            DigOpt::Atlas | DigOpt::Bundle => {
                // validate_atlas_id(): digits only
                if !value.bytes().all(|b| b.is_ascii_digit()) {
                    return unknown("bad atlas ID");
                }
                let id = value.parse().unwrap_or(0);
                if opt == DigOpt::Atlas {
                    spec.msm_id = id;
                } else {
                    spec.bundle = Some(id);
                }
            }
            DigOpt::ClientSubnet => spec.client_subnet = true,
            DigOpt::Cookies => spec.cookies = true,
            DigOpt::Do => spec.dnssec = true,
            DigOpt::Edns0 => spec.edns0 = num(10) as u32,
            DigOpt::EdnsVersion => spec.edns_version = num(10) as u8,
            DigOpt::EdnsFlags => spec.edns_flags = num(0) as u16,
            DigOpt::EdnsOption => spec.edns_option = num(0) as u16,
            DigOpt::Nsid => spec.nsid = true,
            DigOpt::Port => spec.port = num(10) as u16,
            DigOpt::Rd => spec.rd = true,
            DigOpt::Tcp => spec.protocol = "TCP".to_string(),
            DigOpt::Timeout => spec.timeout_ms = num(10) as u32,
            DigOpt::Qbuf => spec.qbuf = true,
            DigOpt::NoAbuf => spec.abuf = false,
            DigOpt::Ad => spec.ad = true,
            DigOpt::Cd => spec.cd = true,
            DigOpt::Retry => spec.retry = num(10) as u32,
            DigOpt::Resolv => {
                spec.resolv = true;
                (v4, v6) = (true, true);
            }
            DigOpt::PrependProbeId => spec.prepend_probe_id = true,
            DigOpt::Ttl => spec.ttl = true,
            DigOpt::Tls | DigOpt::Https => return unknown("DNS over TLS/HTTPS is not supported"),
            DigOpt::Interface
            | DigOpt::Ipv6DestOption
            | DigOpt::OutFile
            | DigOpt::SniCertName
            | DigOpt::HttpsPath
            | DigOpt::WriteResponse
            | DigOpt::ReadResponse
            | DigOpt::Evdns => debug!("evtdig: not acting on {:?} {}", opt, value),
            DigOpt::Refused => return unknown("unknown option"),
        }
    }

    // argProcess(): one server unless --resolv, and a name ending in a dot
    if !spec.resolv {
        let [server] = operands.as_slice() else {
            return unknown("no server IP address in input");
        };
        spec.target = server.clone();
    }
    let Some(query) = query else {
        return unknown("no query in command line");
    };
    if !query.ends_with('.') {
        return unknown("query does not end with a dot");
    }
    spec.query_argument = query;
    spec.af = match (v4, v6) {
        (true, true) => 0,
        (_, true) => 6,
        _ => 4,
    };

    trace!(
        "Parsed evtdig: msm_id={}, server={}, query={} {} {}, protocol={}, interval={}",
        spec.msm_id,
        spec.target,
        spec.query_argument,
        spec.qtype,
        spec.qclass,
        spec.protocol,
        interval
    );
    TelnetCommand::Dns(spec)
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

/// Parse evsslgetcert command arguments as sslgetcert_init() does
/// (probe-busybox/eperd/sslgetcert.c:816-867, getopt "A:B:h:O:R:V:W:i:p:46").
///
/// Usage: evsslgetcert [-4|-6] [-p port] [-h server_name] [-A msm_id]
/// [-O output] hostname
///
/// Neither -4 nor -6 is af 0: either family. The server_name is -h's alone —
/// without it the reference sends none (state->sni is h_arg, :966), so the
/// hostname is not a default. An unknown option or other than one hostname
/// makes sslgetcert_init() refuse the line.
fn parse_evsslgetcert(args: &[String], interval: u64, end_time: i64, spread: u32) -> TelnetCommand {
    let mut af: u8 = 0;
    let mut port: u16 = 443;
    let mut msm_id: u64 = 0;
    let mut hostname: Option<String> = None;
    let mut targets: Vec<&String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-4" => af = 4,
            "-6" => af = 6,
            // -B bundle, -O output, -R/-W fuzzing files, -V protocol version
            // (the controller sends none of them but -O), -i interface
            "-p" | "-A" | "-h" | "-B" | "-O" | "-R" | "-V" | "-W" | "-i" => {
                let Some(value) = args.get(i + 1) else {
                    warn!("evsslgetcert: {} needs an argument", arg);
                    return TelnetCommand::Unknown(format!("evsslgetcert {:?}", args));
                };
                match arg.as_str() {
                    "-p" => port = value.parse().unwrap_or(443),
                    "-A" => msm_id = value.parse().unwrap_or(0),
                    "-h" => hostname = Some(value.clone()),
                    _ => {}
                }
                i += 1;
            }
            _ if !arg.starts_with('-') => targets.push(arg),
            _ => {
                warn!("evsslgetcert: bad option {}", arg);
                return TelnetCommand::Unknown(format!("evsslgetcert {:?}", args));
            }
        }
        i += 1;
    }

    let [target] = targets[..] else {
        warn!("evsslgetcert: exactly one hostname expected");
        return TelnetCommand::Unknown(format!("evsslgetcert {:?}", args));
    };
    let target = target.clone();

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
        target,
        port,
        af,
        hostname,
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

        "traceroute" => {
            let defaults = starla_common::TracerouteOptions::default();
            let num =
                |key: &str, default: u64| spec.get(key).and_then(|v| v.as_u64()).unwrap_or(default);
            TelnetCommand::Traceroute(TracerouteSpec {
                msm_id,
                target,
                af,
                opts: starla_common::TracerouteOptions {
                    protocol: match spec.get("protocol").and_then(|v| v.as_str()) {
                        Some(p) if p.eq_ignore_ascii_case("ICMP") => {
                            starla_common::TracerouteProtocol::ICMP
                        }
                        Some(p) if p.eq_ignore_ascii_case("TCP") => {
                            starla_common::TracerouteProtocol::TCP
                        }
                        _ => starla_common::TracerouteProtocol::UDP,
                    },
                    paris_mod: num("paris", defaults.paris_mod as u64) as u8,
                    first_hop: num("first_hop", defaults.first_hop as u64) as u8,
                    max_hops: num("max_hops", defaults.max_hops as u64) as u8,
                    size: num("size", defaults.size as u64) as u16,
                    ..defaults
                },
                spread,
                schedule,
            })
        }

        "dns" => {
            let text = |key: &str, default: &str| {
                spec.get(key)
                    .and_then(|v| v.as_str())
                    .unwrap_or(default)
                    .to_string()
            };
            // A type or class by name (evtdig's long option names) or number
            let code = |key: &str, default: u16| {
                let name = text(key, "").to_lowercase();
                name.parse().ok().unwrap_or_else(|| match name.as_str() {
                    "" => default,
                    "in" => 1,
                    "ch" => 3,
                    "hs" => 4,
                    _ => EVTDIG_LONGOPTS
                        .iter()
                        .find_map(|&(n, _, o)| match o {
                            DigOpt::Query(t) if n == name => Some(t),
                            _ => None,
                        })
                        .unwrap_or(default),
                })
            };
            let mut query_argument = text("query_argument", ".");
            if !query_argument.ends_with('.') {
                query_argument.push('.');
            }
            let flag = |key: &str, default: bool| {
                spec.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
            };
            TelnetCommand::Dns(DnsSpec {
                msm_id,
                target,
                af,
                protocol: text("protocol", "UDP").to_uppercase(),
                qtype: code("query_type", 1),
                qclass: code("query_class", 1),
                query_argument,
                dnssec: flag("use_dnssec", false),
                rd: flag("recursion_desired", true),
                spread,
                schedule,
                ..DnsSpec::default()
            })
        }

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
// rustfmt's format_strings reflows these literals across line continuations
// and splits the `\n` in a `\r\n\r\n` sequence, changing what the test asserts.
// Leave them exactly as written.
#[rustfmt::skip]
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
                assert_eq!(spec.query_argument, "example.com.");
                assert_eq!((spec.qtype, spec.qclass), (1, 1));
                assert!(spec.rd);
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

    /// eperd's cl_Shell, which Insert() tells lines apart by with their
    /// interval and start time (eperd.c:789-791, 806-808): the line after
    /// its five leading fields, as config_read cuts it
    /// (libbb/parse_config.c:162-243).
    #[test]
    fn a_crontab_lines_command_text_is_eperds() {
        let shell = |line: &str| {
            CrontabLine {
                line: line.to_string(),
                command: TelnetCommand::Status,
            }
            .shell()
            .to_string()
        };
        assert_eq!(
            shell(
                r#"600 1791854199 1791854229 UNIFORM 3 evtdig -4 --resolv --qbuf --type 1 --class 1 --query jd.com. -A "30002" -O /home/atlas/data/new/7"#
            ),
            r#"evtdig -4 --resolv --qbuf --type 1 --class 1 --query jd.com. -A "30002" -O /home/atlas/data/new/7"#
        );
        // Runs of blanks part the fields (PARSE_COLLAPSE); the command's own
        // are kept, and those at the ends trimmed (PARSE_TRIM)
        assert_eq!(
            shell(" 600\t1  2 NONE 0   evping  -c 3\t192.0.2.1 \t"),
            "evping  -c 3\t192.0.2.1"
        );
        // A '#' ends the line (PARSE_EOL_COMMENTS)
        assert_eq!(
            shell("600 1 2 UNIFORM 3 evping 192.0.2.1 # a note"),
            "evping 192.0.2.1"
        );
        // Five fields and no command: SynchronizeFile skips such a line
        // (n < 6, eperd.c:566-567)
        assert_eq!(shell("600 1 2 UNIFORM 3"), "");
        assert_eq!(shell("600 1 2 UNIFORM # 3 evping 192.0.2.1"), "");
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
            _ => panic!("Expected Ping command"),
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

    /// The twin's evping lines (enodia's controller session of 2026-10-13):
    /// a name target, the default size (0, sent as 32 data bytes), and
    /// -i's value taken as the interval, not as a target.
    #[test]
    fn evping_reads_its_options_as_ping_init_does() {
        let spec = |line: &str| match parse_command(line) {
            TelnetCommand::Ping(spec) => spec,
            _ => panic!("not a ping command"),
        };
        let s = spec(
            r#"CRONLINE 21600 1384530082 1791883921 UNIFORM 14 evping -6 -c 16 -A "1040114" -O /home/atlas/data/new/7 bits.wikimedia.org"#,
        );
        assert_eq!(
            (s.msm_id, s.target.as_str(), s.af, s.packets, s.size),
            (1040114, "bits.wikimedia.org", 6, 16, 0)
        );
        let s = spec(
            r#"CRONLINE 240 1737367695 1791883921 UNIFORM 14 evping -6 -c 3 -s 48 -i 1000 -A "86206978" -O /home/atlas/data/new/7 google.com"#,
        );
        assert_eq!(
            (s.target.as_str(), s.size, s.packet_interval),
            ("google.com", 48, 1000)
        );
        let s = spec(
            r#"CRONLINE 28800 1393596000 1791883921 UNIFORM 14 evping -6 -c 5 -s 1460 -A "1517509" -O /home/atlas/data/new/7 2a01:4f8:110:3169::37:134"#,
        );
        assert_eq!((s.size, s.packets), (1460, 5));
        // IPv6 unless -4 is given, and -4 wins
        assert_eq!(
            spec(r#"CRONLINE 240 1 2 UNIFORM 3 evping -c 3 -A "1" www.ripe.net"#).af,
            6
        );
        assert_eq!(
            spec(r#"CRONLINE 240 1 2 UNIFORM 3 evping -4 -6 -A "1" www.ripe.net"#).af,
            4
        );
        // every value option skips its value
        let s = spec(
            r#"CRONLINE 240 1 2 UNIFORM 3 evping -4 -B 77 -I eth0 -R in -W out -i 500 -A "1" -O /home/atlas/data/new/7 193.0.14.129"#,
        );
        assert_eq!(
            (s.target.as_str(), s.packet_interval),
            ("193.0.14.129", 500)
        );
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
                assert_eq!(spec.opts.protocol, starla_common::TracerouteProtocol::UDP);
                assert_eq!(spec.schedule.interval, 1800);
            }
            _ => panic!("Expected Traceroute command"),
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
                assert_eq!(spec.opts.protocol, starla_common::TracerouteProtocol::ICMP);
                assert_eq!(spec.opts.first_hop, 1);
                assert_eq!(spec.opts.max_hops, 64);
            }
            _ => panic!("Expected Traceroute command"),
        }
    }

    fn evtraceroute(line: &str) -> TracerouteSpec {
        match parse_command(line) {
            TelnetCommand::Traceroute(spec) => spec,
            // A ONEOFF arrives wrapped, so its result can be posted on its
            // own (&SRC=oneoff).
            TelnetCommand::Oneoff(inner) => match *inner {
                TelnetCommand::Traceroute(spec) => spec,
                _ => panic!("oneoff is not a traceroute: {line}"),
            },
            _ => panic!("not a traceroute command: {line}"),
        }
    }

    /// -a is parismod (16 without one), -b parisbase (0 without one) and
    /// -p the destination port (traceroute.c:4247-4248, 4259-4262). RIPE's
    /// built-in 6152 has none of them; its user-defined lines give
    /// `-a 16 … -p 80` (the twin's 25603345), which Starla reported as
    /// "paris_id":80 (starla-25603345.json).
    #[test]
    fn evtraceroute_paris_is_a_and_b_not_p() {
        let paris = |line: &str| match parse_command(line) {
            TelnetCommand::Traceroute(s) => {
                (s.opts.paris_mod, s.opts.paris_base, s.target, s.opts.size)
            }
            _ => panic!("not a traceroute command: {line}"),
        };
        assert_eq!(
            paris(
                r#"CRONLINE 900 2833 1794447121 UNIFORM 3 evtraceroute -6 -I -c 3 -w 4000 -A "6152" -O /home/atlas/data/new/7 topology6.dyndns.atlas.ripe.net"#
            ),
            (16, 0, "topology6.dyndns.atlas.ripe.net".into(), 40)
        );
        assert_eq!(
            paris(
                r#"CRONLINE 900 1 2000000000 UNIFORM 3 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "25603345" -O /home/atlas/data/new/7 dyna.wikimedia.org"#
            ),
            (16, 0, "dyna.wikimedia.org".into(), 48)
        );
        assert_eq!(
            paris(
                r#"CRONLINE 900 1 2000000000 UNIFORM 3 evtraceroute -4 -T -a 0 -b 3 -p 443 -A "7" 192.0.2.1"#
            ),
            (0, 3, "192.0.2.1".into(), 40)
        );
    }

    /// Every evtraceroute the controller gave enodia on 2026-10-13 (the
    /// twin's scenario enodia-2026-10-13T0131.json), one line per shape,
    /// read as traceroute_init reads them.
    #[test]
    fn evtraceroute_reads_the_controllers_lines_as_traceroute_init_does() {
        use starla_common::TracerouteProtocol::{ICMP, UDP};
        let defaults = starla_common::TracerouteOptions::default();

        // The root-server UDP set (6001-6016) and topology6 (6052).
        let s = evtraceroute(
            r#"CRONLINE 1800 2634 1949621521 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6001" -O /home/atlas/data/new/7 2001:7fd::1"#,
        );
        assert_eq!(
            (s.msm_id, s.target.as_str(), s.af),
            (6001, "2001:7fd::1", 6)
        );
        assert_eq!(
            s.opts,
            starla_common::TracerouteOptions {
                protocol: UDP,
                count: 3,
                timeout_ms: 4000,
                ..defaults.clone()
            }
        );
        assert_eq!(s.opts.paris_mod, 16, "Paris by default");
        assert_eq!(s.opts.size, 40);
        assert_eq!((s.opts.gap_limit, s.opts.dup_timeout_ms), (5, 10));
        let s = evtraceroute(
            r#"CRONLINE 900 3292 1794447121 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6052" -O /home/atlas/data/new/7 topology6.dyndns.atlas.ripe.net"#,
        );
        assert_eq!(s.target, "topology6.dyndns.atlas.ripe.net");
        // 6152: the same over ICMP.
        let s = evtraceroute(
            r#"CRONLINE 900 2833 1794447121 UNIFORM 3 evtraceroute -6 -I -c 3 -w 4000 -A "6152" -O /home/atlas/data/new/7 topology6.dyndns.atlas.ripe.net"#,
        );
        assert_eq!((s.msm_id, s.opts.protocol), (6152, ICMP));

        // The user-defined sets: -p is the TCP port, not a Paris id (it
        // was read as paris_id 80); -a is the Paris modulus.
        let full = starla_common::TracerouteOptions {
            protocol: ICMP,
            paris_mod: 16,
            count: 3,
            first_hop: 1,
            max_hops: 32,
            timeout_ms: 4000,
            dup_timeout_ms: 10,
            size: 48,
            dest_port: 80,
            dest_opt_size: 0,
            hbh_opt_size: 0,
            ..defaults.clone()
        };
        let s = evtraceroute(
            r#"CRONLINE 900 1780657137 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "177421605" -O /home/atlas/data/new/7 www.office.com"#,
        );
        assert_eq!(
            (s.msm_id, s.target.as_str(), s.af),
            (177421605, "www.office.com", 6)
        );
        assert_eq!(s.opts, full);
        let s = evtraceroute(
            r#"ONEOFF /home/atlas/crons/oneoff evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "222027444" google.com"#,
        );
        assert_eq!((s.msm_id, s.target.as_str()), (222027444, "google.com"));
        assert_eq!(s.opts, full);
        let s = evtraceroute(
            r#"CRONLINE 900 1738762905 1791883921 UNIFORM 14 evtraceroute -6 -U -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "86691235" -O /home/atlas/data/new/7 google.com"#,
        );
        assert_eq!(
            s.opts,
            starla_common::TracerouteOptions {
                protocol: UDP,
                ..full.clone()
            }
        );
        // -a 1: a modulus of one, paris_id 0 every time.
        let s = evtraceroute(
            r#"CRONLINE 7200 1591171760 1791883921 UNIFORM 14 evtraceroute -6 -I -a 1 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "25603532" -O /home/atlas/data/new/7 text-lb.eqsin.wikimedia.org"#,
        );
        assert_eq!(s.opts.paris_mod, 1);
        assert_eq!(s.opts.paris_id(1), Some(0));
        // The oldest: no -c, -w or -z, -S 40.
        let s = evtraceroute(
            r#"CRONLINE 900 1354179265 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -f 1 -m 32 -S 40 -A "1004732" -O /home/atlas/data/new/7 www.google.com"#,
        );
        assert_eq!(
            s.opts,
            starla_common::TracerouteOptions {
                protocol: ICMP,
                ..defaults.clone()
            }
        );
    }

    /// All 38 evtraceroute lines of the twin's scenario
    /// (enodia-2026-10-13T0131.json), each against its own tokens: nothing
    /// refused, no value taken for the target, and every option where
    /// traceroute_init puts it.
    #[test]
    fn every_scenario_evtraceroute_line_parses() {
        const LINES: &[&str] = &[
            r#"CRONLINE 1800 2634 1949621521 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6001" -O /home/atlas/data/new/7 2001:7fd::1"#,
            r#"CRONLINE 1800 2717 1794447121 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6004" -O /home/atlas/data/new/7 2001:500:2f::f"#,
            r#"CRONLINE 1800 2758 1794447121 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6005" -O /home/atlas/data/new/7 2001:7fe::53"#,
            r#"CRONLINE 1800 2795 1794447121 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6006" -O /home/atlas/data/new/7 2001:dc3::35"#,
            r#"CRONLINE 1800 2865 1794447121 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6008" -O /home/atlas/data/new/7 2001:500:9f::42"#,
            r#"CRONLINE 1800 2906 1794447121 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6009" -O /home/atlas/data/new/7 2001:503:ba3e::2:30"#,
            r#"CRONLINE 1800 2922 1794447121 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6010" -O /home/atlas/data/new/7 2801:1b8:10::b"#,
            r#"CRONLINE 1800 2921 1794447121 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6011" -O /home/atlas/data/new/7 2001:500:2::c"#,
            r#"CRONLINE 1800 2960 1794447121 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6012" -O /home/atlas/data/new/7 2001:500:2d::d"#,
            r#"CRONLINE 1800 2959 1794447121 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6013" -O /home/atlas/data/new/7 2001:500:a8::e"#,
            r#"CRONLINE 1800 2966 1794447121 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6014" -O /home/atlas/data/new/7 2001:500:12::d0d"#,
            r#"CRONLINE 1800 3005 1794447121 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6015" -O /home/atlas/data/new/7 2001:500:1::53"#,
            r#"CRONLINE 1800 3044 1794447121 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6016" -O /home/atlas/data/new/7 2001:503:c27::2:30"#,
            r#"CRONLINE 900 3292 1794447121 UNIFORM 3 evtraceroute -6 -U -c 3 -w 4000 -A "6052" -O /home/atlas/data/new/7 topology6.dyndns.atlas.ripe.net"#,
            r#"CRONLINE 900 2833 1794447121 UNIFORM 3 evtraceroute -6 -I -c 3 -w 4000 -A "6152" -O /home/atlas/data/new/7 topology6.dyndns.atlas.ripe.net"#,
            r#"CRONLINE 900 1354179265 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -f 1 -m 32 -S 40 -A "1004732" -O /home/atlas/data/new/7 www.google.com"#,
            r#"CRONLINE 7200 1591170214 1791883921 UNIFORM 14 evtraceroute -6 -I -a 1 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "25603345" -O /home/atlas/data/new/7 dyna.wikimedia.org"#,
            r#"CRONLINE 7200 1591170560 1791883921 UNIFORM 14 evtraceroute -6 -I -a 1 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "25603435" -O /home/atlas/data/new/7 dyna.wikimedia.org"#,
            r#"CRONLINE 7200 1591170800 1791883921 UNIFORM 14 evtraceroute -6 -I -a 1 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "25603439" -O /home/atlas/data/new/7 text-lb.eqiad.wikimedia.org"#,
            r#"CRONLINE 7200 1591171040 1791883921 UNIFORM 14 evtraceroute -6 -I -a 1 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "25603450" -O /home/atlas/data/new/7 text-lb.codfw.wikimedia.org"#,
            r#"CRONLINE 7200 1591171280 1791883921 UNIFORM 14 evtraceroute -6 -I -a 1 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "25603477" -O /home/atlas/data/new/7 text-lb.esams.wikimedia.org"#,
            r#"CRONLINE 7200 1591171520 1791883921 UNIFORM 14 evtraceroute -6 -I -a 1 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "25603503" -O /home/atlas/data/new/7 text-lb.ulsfo.wikimedia.org"#,
            r#"CRONLINE 7200 1591171760 1791883921 UNIFORM 14 evtraceroute -6 -I -a 1 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "25603532" -O /home/atlas/data/new/7 text-lb.eqsin.wikimedia.org"#,
            r#"CRONLINE 900 1738762905 1791883921 UNIFORM 14 evtraceroute -6 -U -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "86691235" -O /home/atlas/data/new/7 google.com"#,
            r#"CRONLINE 900 1738832482 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "86710304" -O /home/atlas/data/new/7 wikipedia.org"#,
            r#"CRONLINE 900 1738832540 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "86710364" -O /home/atlas/data/new/7 facebook.com"#,
            r#"CRONLINE 900 1780485734 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "176517312" -O /home/atlas/data/new/7 youtube.com"#,
            r#"CRONLINE 900 1780485740 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "176517343" -O /home/atlas/data/new/7 assets.nflxext.com"#,
            r#"CRONLINE 900 1780485744 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "176517367" -O /home/atlas/data/new/7 instagram.com"#,
            r#"CRONLINE 900 1780558237 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "176906963" -O /home/atlas/data/new/7 reddit.com"#,
            r#"CRONLINE 900 1780558244 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "176906974" -O /home/atlas/data/new/7 linkedin.com"#,
            r#"CRONLINE 900 1780558252 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "176906985" -O /home/atlas/data/new/7 cloudflare.com"#,
            r#"CRONLINE 900 1780651312 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "177401200" -O /home/atlas/data/new/7 d1.awsstatic.com"#,
            r#"CRONLINE 900 1780651319 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "177401232" -O /home/atlas/data/new/7 akamai.com"#,
            r#"CRONLINE 900 1780651327 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "177401263" -O /home/atlas/data/new/7 fastly.com"#,
            r#"CRONLINE 900 1780657137 1791883921 UNIFORM 14 evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "177421605" -O /home/atlas/data/new/7 www.office.com"#,
            r#"ONEOFF /home/atlas/crons/oneoff evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "222017431" google.com"#,
            r#"ONEOFF /home/atlas/crons/oneoff evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "222027444" google.com"#,
        ];
        assert_eq!(LINES.len(), 38);
        for line in LINES {
            let toks = tokenize_cronline(line);
            let val = |opt: &str| {
                toks.iter()
                    .position(|t| t == opt)
                    .map(|i| toks[i + 1].parse::<u32>().unwrap())
            };
            let s = evtraceroute(line);
            assert_eq!(s.target, *toks.last().unwrap(), "{line}");
            assert_eq!(s.msm_id, val("-A").unwrap() as u64, "{line}");
            assert_eq!(s.af, 6, "{line}");
            let protocol = if toks.iter().any(|t| t == "-I") {
                starla_common::TracerouteProtocol::ICMP
            } else {
                starla_common::TracerouteProtocol::UDP
            };
            let o = &s.opts;
            assert_eq!(o.protocol, protocol, "{line}");
            assert_eq!(o.paris_mod as u32, val("-a").unwrap_or(16), "{line}");
            assert_eq!(o.count as u32, val("-c").unwrap_or(3), "{line}");
            assert_eq!(o.first_hop as u32, val("-f").unwrap_or(1), "{line}");
            assert_eq!(o.max_hops as u32, val("-m").unwrap_or(32), "{line}");
            assert_eq!(o.timeout_ms, val("-w").unwrap_or(1000), "{line}");
            assert_eq!(o.dup_timeout_ms, val("-z").unwrap_or(10), "{line}");
            assert_eq!(o.size as u32, val("-S").unwrap_or(40), "{line}");
            assert_eq!(o.dest_port as u32, val("-p").unwrap_or(80), "{line}");
            assert_eq!(o.hbh_opt_size as u32, val("-H").unwrap_or(0), "{line}");
            assert_eq!(o.dest_opt_size as u32, val("-D").unwrap_or(0), "{line}");
            assert_eq!(o.paris_base, 0, "{line}");
            assert_eq!(o.gap_limit, 5, "{line}");
            assert_eq!((o.tos, o.dont_fragment), (0, false), "{line}");
        }
    }

    #[test]
    fn evtraceroute_option_forms_and_refusals() {
        let refused = |line: &str| {
            assert!(
                matches!(parse_command(line), TelnetCommand::Unknown(_)),
                "{line}"
            )
        };
        // getopt: bundled flags, attached values, options after the target.
        let s = evtraceroute(
            r#"CRONLINE 60 0 0 UNIFORM 1 evtraceroute -6TF -c5 -p 443 example.net -A 7"#,
        );
        assert_eq!(s.af, 6);
        assert_eq!(s.opts.protocol, starla_common::TracerouteProtocol::TCP);
        assert!(s.opts.dont_fragment);
        assert_eq!((s.opts.count, s.opts.dest_port, s.msm_id), (5, 443, 7));
        // IPv4 unless -6; every other value option takes its value.
        let s = evtraceroute(
            r#"CRONLINE 60 0 0 UNIFORM 1 evtraceroute -a 0 -b 3 -g 2 -t 32 -i eth0 -B 12 -R x -W y -r 192.0.2.1"#,
        );
        assert_eq!((s.af, s.target.as_str()), (4, "192.0.2.1"));
        assert_eq!(
            (
                s.opts.paris_mod,
                s.opts.paris_base,
                s.opts.gap_limit,
                s.opts.tos
            ),
            (0, 3, 2, 32)
        );
        assert_eq!(s.opts.interface.as_deref(), Some("eth0"));
        assert_eq!(s.opts.bundle.as_deref(), Some("12"));
        assert_eq!(s.opts.paris_id(1), None);
        // Refused as traceroute_init refuses them.
        refused(r#"CRONLINE 60 0 0 UNIFORM 1 evtraceroute -4 -6 192.0.2.1"#);
        refused(r#"CRONLINE 60 0 0 UNIFORM 1 evtraceroute -S 4097 192.0.2.1"#);
        refused(r#"CRONLINE 60 0 0 UNIFORM 1 evtraceroute -c three 192.0.2.1"#);
        refused(r#"CRONLINE 60 0 0 UNIFORM 1 evtraceroute -x 192.0.2.1"#);
        refused(r#"CRONLINE 60 0 0 UNIFORM 1 evtraceroute -A 5"#);
        refused(r#"CRONLINE 60 0 0 UNIFORM 1 evtraceroute 192.0.2.1 192.0.2.2"#);
        refused(r#"CRONLINE 60 0 0 UNIFORM 1 evtraceroute -w"#);
        assert_eq!(
            evtraceroute(r#"CRONLINE 60 0 0 UNIFORM 1 evtraceroute -S 4096 192.0.2.1"#)
                .opts
                .size,
            4096
        );
        // -p is the service evdns_getaddrinfo resolves: any number from 0
        // (a trace to port 0) to 65535.
        for (p, port) in [("0", 0), ("65535", 65535), ("+80", 80)] {
            let line = format!("CRONLINE 60 0 0 UNIFORM 1 evtraceroute -T -p {p} 192.0.2.1");
            assert_eq!(evtraceroute(&line).opts.dest_port, port, "{line}");
        }
        refused(r#"CRONLINE 60 0 0 UNIFORM 1 evtraceroute -T -p 65536 192.0.2.1"#);
        refused(r#"CRONLINE 60 0 0 UNIFORM 1 evtraceroute -T -p http 192.0.2.1"#);
    }

    /// The same CRONLINE delivered twice gives the scheduler the same
    /// measurement: every part TracerouteInstance::for_line keys on (eperd's
    /// Insert(): interval, start time, command) is equal, so the second
    /// delivery keeps the first one's slot and Paris id.
    #[test]
    fn evtraceroute_redelivered_line_is_the_same_line() {
        let line =
            r#"CRONLINE 900 0 0 UNIFORM 450 evtraceroute -6 -U -c 3 -w 4000 -A 6001 2001:7fd::1"#;
        let (a, b) = (evtraceroute(line), evtraceroute(line));
        assert_eq!(
            (a.msm_id, &a.target, a.af, &a.opts),
            (b.msm_id, &b.target, b.af, &b.opts)
        );
        assert_eq!(
            (a.schedule.interval, a.schedule.start_time),
            (b.schedule.interval, b.schedule.start_time)
        );
        assert_eq!(a.schedule.interval, 900);
        let changed = evtraceroute(
            r#"CRONLINE 900 0 0 UNIFORM 450 evtraceroute -6 -U -c 3 -w 3000 -A 6001 2001:7fd::1"#,
        );
        assert_ne!(changed.opts, a.opts);
    }

    fn dns(line: &str) -> DnsSpec {
        match parse_command(line) {
            TelnetCommand::Dns(spec) => spec,
            // Not `{other:?}`: printing the parsed command trips CodeQL's
            // rust/cleartext-logging (an evsslgetcert line is sensitive).
            _ => panic!("not a dns command: {line}"),
        }
    }

    #[test]
    fn test_parse_cronline_evtdig() {
        let spec =
            dns(r#"CRONLINE 900 0 0 UNIFORM 5 evtdig -4 --aaaa example.com. -A "6001" 9.9.9.9"#);
        assert_eq!(spec.msm_id, 6001);
        assert_eq!(spec.target, "9.9.9.9");
        assert_eq!(spec.query_argument, "example.com."); // as given, dot and all
        assert_eq!((spec.qtype, spec.qclass), (28, 1));
        assert_eq!(spec.af, 4);
        assert!(!spec.rd, "evtdig sets RD only with -R or --resolv");
    }

    #[test]
    fn test_parse_cronline_evtdig_soa() {
        let spec = dns(
            r#"CRONLINE 1800 795 1770762780 UNIFORM 3 evtdig -4 --soa . -A "10001" -O /home/atlas/data/new/7 193.0.14.129"#,
        );
        assert_eq!(spec.msm_id, 10001);
        assert_eq!(spec.target, "193.0.14.129");
        assert_eq!(spec.query_argument, ".");
        assert_eq!((spec.qtype, spec.qclass), (6, 1));
        assert_eq!(spec.af, 4);
        assert_eq!(spec.protocol, "UDP");
    }

    #[test]
    fn test_parse_cronline_evtdig_tcp_soa() {
        let spec = dns(
            r#"CRONLINE 1800 1945 1770762780 UNIFORM 3 evtdig -4 -t --soa . -A "10101" -O /home/atlas/data/new/7 193.0.14.129"#,
        );
        assert_eq!(spec.msm_id, 10101);
        assert_eq!(spec.target, "193.0.14.129");
        assert_eq!(spec.qtype, 6);
        assert_eq!(spec.protocol, "TCP");
    }

    /// Every evtdig line of the twin scenario (enodia 2026-10-13 01:31),
    /// parsed as tdig_init() parses it. Starla sent `-r` as ". IN A" with
    /// EDNS 4096 and DO, dropped `-e`, `-n` and `--qbuf`, set RD everywhere
    /// and sent `--resolv` to Google Public DNS.
    #[test]
    fn the_scenarios_evtdig_lines_parse_as_tdig_init_parses_them() {
        let spec = dns(
            r#"CRONLINE 600 1485933101 1794447121 UNIFORM 3 evtdig -4 --resolv --qbuf --type 1 --query $p.atlas.ripe.net.$r. --class 1  -A "30001" -O /home/atlas/data/new/7"#,
        );
        assert!(spec.resolv && spec.qbuf && !spec.rd);
        assert_eq!(spec.target, "", "the probe's resolvers, nobody else");
        assert_eq!(spec.af, 0, "--resolv asks each resolver, v4 or v6");
        assert_eq!((spec.qtype, spec.qclass), (1, 1));
        assert_eq!(spec.query_argument, "$p.atlas.ripe.net.$r.");
        assert_eq!(spec.msm_id, 30001);

        let spec = dns(
            r#"CRONLINE 3600 1714559586 1791883921 UNIFORM 3 evtdig -6 --resolv -e 512 --qbuf --aaaa $r-$t-$p.rpkitest6.nlnetlabs.nl. -R --retry 0  --timeout 5000 -A "70502079" -O /home/atlas/data/new/7"#,
        );
        assert!(spec.resolv && spec.rd && spec.qbuf);
        assert_eq!((spec.af, spec.qtype, spec.qclass), (0, 28, 1));
        assert_eq!((spec.edns0, spec.nsid, spec.dnssec), (512, false, false));
        assert_eq!((spec.retry, spec.timeout_ms), (0, 5000));
        assert_eq!(spec.query_argument, "$r-$t-$p.rpkitest6.nlnetlabs.nl.");

        let spec = dns(
            r#"CRONLINE 1800 1297 1794447121 UNIFORM 3 evtdig -6 --soa . -A "11001" -O /home/atlas/data/new/7 2001:7fd::1"#,
        );
        assert_eq!(
            (spec.af, spec.qtype, spec.qclass, spec.rd),
            (6, 6, 1, false)
        );
        assert_eq!(
            (spec.target.as_str(), spec.protocol.as_str()),
            ("2001:7fd::1", "UDP")
        );

        for (opt, name) in [
            ("-b", "version.bind."),
            ("-h", "hostname.bind."),
            ("-i", "id.server."),
            ("-r", "version.server."),
        ] {
            let spec = dns(&format!(
                r#"CRONLINE 43200 20160 1794447121 UNIFORM 3 evtdig -6 {opt} -A "11501" -O /home/atlas/data/new/7 2001:7fd::1"#
            ));
            assert_eq!(spec.query_argument, name, "{opt}");
            assert_eq!((spec.qtype, spec.qclass), (16, 3), "{opt}: TXT CH");
            assert!(!spec.rd && !spec.dnssec && !spec.nsid, "{opt}");
        }

        let spec = dns(
            r#"CRONLINE 43200 1680201148 1791883921 UNIFORM 3 evtdig -6 -t -e 512 -n --qbuf --a paypal.com. --retry 0 --timeout 5000 -A "51457586" -O /home/atlas/data/new/7 2001:7fd::1"#,
        );
        assert_eq!(spec.protocol, "TCP");
        assert!(spec.nsid && spec.qbuf && !spec.rd);
        assert_eq!((spec.edns0, spec.qtype, spec.qclass), (512, 1, 1));
        assert_eq!(spec.query_argument, "paypal.com.");
        assert_eq!(spec.msm_id, 51457586);

        let spec = dns(
            r#"CRONLINE 86400 1771519272 1791883921 UNIFORM 3 evtdig -6 -e 512 -h --retry 0 --timeout 5000 -A "156156173" -O /home/atlas/data/new/7 2620:fe::fe"#,
        );
        assert_eq!(spec.query_argument, "hostname.bind.");
        assert!(!spec.nsid && !spec.qbuf);

        let spec = dns(
            r#"CRONLINE 28800 1773332354 1791883921 UNIFORM 3 evtdig -6 -t -e 512 -n --qbuf --aaaa $p.ripe-tld-experiment.$r. --retry 0  --timeout 5000 -A "158899652" -O /home/atlas/data/new/7 2001:7fd::1"#,
        );
        assert_eq!(spec.query_argument, "$p.ripe-tld-experiment.$r.");
        assert_eq!((spec.qtype, spec.protocol.as_str()), (28, "TCP"));
    }

    /// getopt_long(): long options by unique prefix or with `=`, short ones
    /// clustered or with their argument attached, options after the server.
    #[test]
    fn evtdig_options_parse_as_getopt_long_parses_them() {
        let spec = dns(
            r#"CRONLINE 60 0 0 UNIFORM 1 evtdig 192.0.2.53 -46dn -e4096 --type=28 --class 1 --que=x. --resol --timeo 900 --edns-flags 0x10 --cd -A "7""#,
        );
        assert_eq!(spec.af, 0);
        assert!(spec.dnssec && spec.nsid && spec.cd && spec.resolv);
        assert_eq!((spec.edns0, spec.qtype, spec.qclass), (4096, 28, 1));
        assert_eq!((spec.timeout_ms, spec.edns_flags), (900, 0x10));
        assert_eq!(spec.query_argument, "x.");

        // --type keeps the CH class unless --class says otherwise
        let spec = dns(r#"CRONLINE 60 0 0 UNIFORM 1 evtdig --type 16 --query x. -A 7 192.0.2.53"#);
        assert_eq!((spec.qtype, spec.qclass), (16, 3));
        let spec = dns(
            r#"CRONLINE 60 0 0 UNIFORM 1 evtdig -B 12 --p_probe_id -h --port 5353 -A 7 -- 192.0.2.53"#,
        );
        assert_eq!((spec.bundle, spec.port), (Some(12), 5353));
        assert!(spec.prepend_probe_id);
        assert_eq!(spec.target, "192.0.2.53");
        assert!(!spec.ttl);

        // --ttl (O_TTL): report the reply's TTL, by its name or a prefix
        let spec = dns(r#"CRONLINE 60 0 0 UNIFORM 1 evtdig -6 --ttl -h -A 7 2001:db8::53"#);
        assert!(spec.ttl);
        let spec = dns(r#"CRONLINE 60 0 0 UNIFORM 1 evtdig -6 -b --tt -A 7 2001:db8::53"#);
        assert!(spec.ttl);
    }

    /// What tdig_init() refuses, Starla refuses: no measurement at all.
    #[test]
    fn evtdig_refuses_what_tdig_init_refuses() {
        for args in [
            "-4 --soa . 192.0.2.53 192.0.2.54", // two servers
            "-4 --soa .",                       // none
            "-4 --a example.com 192.0.2.53",    // no final dot
            "-4 192.0.2.53",                    // no query
            "-4 -h -q 192.0.2.53",              // -q is in the string, not the switch
            "-4 -h --c_output 192.0.2.53",      // likewise
            "-4 -h --bogus 192.0.2.53",         // unknown
            "-4 -h --ns 192.0.2.53",            // --ns needs its NAME, so no server
            "-4 -h --edns 1 192.0.2.53",        // ambiguous prefix
            "-4 -h -A 12a 192.0.2.53",          // bad atlas id
            "-4 -h -p 5353 192.0.2.53",         // no -p, only --port
            "-4 -h --tls 192.0.2.53",           // Starla can't
        ] {
            let line = format!("CRONLINE 60 0 0 UNIFORM 1 evtdig {args}");
            assert!(
                matches!(parse_command(&line), TelnetCommand::Unknown(_)),
                "{args}"
            );
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
            _ => panic!("Expected Http command"),
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
            _ => panic!("Expected Tls command"),
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

    /// The twin scenario's line (enodia-2026-10-13T0131): -h is the
    /// server_name, the hostname is what is resolved and reported.
    #[test]
    fn evsslgetcert_takes_the_server_name_from_h_only() {
        let cmd = parse_command(
            r#"CRONLINE 43200 6472 1794447121 UNIFORM 3 evsslgetcert -6 -h www.ripe.net -A "15001" -O /home/atlas/data/new/7 www.ripe.net"#,
        );
        let TelnetCommand::Tls(spec) = cmd else {
            panic!("Expected Tls command");
        };
        assert_eq!(spec.msm_id, 15001);
        assert_eq!(spec.target, "www.ripe.net");
        assert_eq!(spec.hostname.as_deref(), Some("www.ripe.net"));
        assert_eq!(spec.port, 443);
        assert_eq!(spec.af, 6);

        // Without -h no server_name, and without -4/-6 either family
        let cmd = parse_command(
            r#"CRONLINE 43200 0 0 UNIFORM 3 evsslgetcert -A "15003" -O /home/atlas/data/new/7 www.ripe.net"#,
        );
        let TelnetCommand::Tls(spec) = cmd else {
            panic!("Expected Tls command");
        };
        assert_eq!(spec.hostname, None);
        assert_eq!(spec.af, 0);
        assert_eq!(spec.target, "www.ripe.net");
    }

    /// sslgetcert_init() refuses a line without exactly one hostname or
    /// with an option it does not know.
    #[test]
    fn evsslgetcert_refuses_what_sslgetcert_init_refuses() {
        for line in [
            r#"CRONLINE 900 0 0 UNIFORM 5 evsslgetcert -6 -A "1" a.example b.example"#,
            r#"CRONLINE 900 0 0 UNIFORM 5 evsslgetcert -6 -A "1""#,
            r#"CRONLINE 900 0 0 UNIFORM 5 evsslgetcert -6 -H x -A "1" a.example"#,
            r#"CRONLINE 900 0 0 UNIFORM 5 evsslgetcert -6 -A "1" a.example -p"#,
        ] {
            assert!(
                matches!(parse_command(line), TelnetCommand::Unknown(_)),
                "{line}"
            );
        }
        // -V's argument is not taken for the hostname
        let cmd =
            parse_command(r#"CRONLINE 900 0 0 UNIFORM 5 evsslgetcert -V TLS1.2 -A "1" a.example"#);
        assert!(
            matches!(cmd, TelnetCommand::Tls(ref s) if s.target == "a.example"),
            "not a tls command"
        );
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
            _ => panic!("Expected Ntp command"),
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
            _ => panic!("Expected HostTelemetry/Buddyinfo"),
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
            _ => panic!("Expected HostTelemetry/Rptaddrs"),
        }
    }

    #[test]
    fn test_parse_cronline_httppost_ignored() {
        // A --post-file line posts a file Starla does not have.
        let cmd = parse_command(
            "CRONLINE 60 0 0 UNIFORM 5 httppost -O /home/atlas/data/out/foo --post-file foo",
        );
        assert!(matches!(cmd, TelnetCommand::Ignored(_)));
        // httppost refuses to run without exactly one URL.
        for line in [
            "CRONLINE 60 0 0 UNIFORM 5 httppost --post-dir /home/atlas/data/out",
            "CRONLINE 60 0 0 UNIFORM 5 httppost http://127.0.0.1:8080/ http://127.0.0.1:8080/",
            "CRONLINE 60 0 0 UNIFORM 5 httppost --set-time x http://127.0.0.1:8080/",
            "CRONLINE 0 0 0 UNIFORM 5 httppost http://127.0.0.1:8080/",
        ] {
            assert!(
                matches!(parse_command(line), TelnetCommand::Ignored(_)),
                "{line}"
            );
        }
    }

    const TWIN_SESSION: &str = "b6c053a85ee60b777fa64503f459764141780eea4c80d096c75cb2300531a15d";

    /// Session 2 of enodia's 2026-10-13 journal, as the twin replayed it.
    #[test]
    fn the_controllers_httppost_lines_parse() {
        let main = format!(
            "CRONLINE 60 12 2000000000 UNIFORM 20 httppost -A 9015 --set-time 2 --delete-file \
             --maxpostsize 9000000 --post-header /home/atlas/status/p_to_c_report_header \
             --post-dir /home/atlas/data/out --post-footer /home/atlas/status/con_session_id.txt \
             -O /home/atlas/data/new/main_sent.vol \
             http://127.0.0.1:8080/?PROBE_ID=1018212&SESSION_ID={TWIN_SESSION} >> \
             /home/atlas/data/new/main"
        );
        let TelnetCommand::Httppost(spec) = parse_command(&main) else {
            panic!("{main}");
        };
        assert_eq!(
            spec,
            HttppostSpec {
                interval: 60,
                start_time: 12,
                end_time: 2_000_000_000,
                spread: 20,
                url: format!("http://127.0.0.1:8080/?PROBE_ID=1018212&SESSION_ID={TWIN_SESSION}"),
                post_dir: true,
                set_time: Some(2),
                local_time: false,
            }
        );

        let timesync = format!(
            "CRONLINE 900 312 2000000000 UNIFORM 20 httppost -A 9015 --set-time 2 \
             --delete-file --maxpostsize 9000000 --post-header \
             /home/atlas/status/p_to_c_report_header --post-footer \
             /home/atlas/status/con_session_id.txt -O /home/atlas/data/new/main_sent.vol \
             http://127.0.0.1:8080/?PROBE_ID=1018212&SESSION_ID={TWIN_SESSION}&SRC=timesync \
             >> /home/atlas/data/new/main"
        );
        let TelnetCommand::Httppost(spec) = parse_command(&timesync) else {
            panic!("{timesync}");
        };
        assert_eq!(
            (spec.interval, spec.start_time, spec.spread),
            (900, 312, 20)
        );
        assert!(!spec.post_dir, "header and footer only");
        assert!(spec.url.ends_with("&SRC=timesync"));

        // NONE has no spread; --opt=value and --local-time as getopt takes
        // them; a redirection anywhere, as perd's atlas_run takes it.
        let cmd = parse_command(
            "CRONLINE 60 12 2000000000 NONE 20 httppost --set-time=3 >>/home/atlas/data/new/main \
             --local-time > /home/atlas/data/new/x --post-dir=/home/atlas/data/out \
             http://127.0.0.1:8080/?PROBE_ID=1",
        );
        let TelnetCommand::Httppost(spec) = cmd else {
            panic!("not the expected command");
        };
        assert_eq!(spec.spread, 0);
        assert_eq!(spec.set_time, Some(3));
        assert!(spec.local_time && spec.post_dir);
        assert_eq!(spec.url, "http://127.0.0.1:8080/?PROBE_ID=1");
    }

    /// The twin's ONEOFFs: marked, the measurement inside parsed as before.
    #[test]
    fn a_oneoff_line_is_marked() {
        let cmd = parse_command(
            r#"ONEOFF /home/atlas/crons/oneoff evping -6 -c 3 -s 48 -i 1000 -A "222024177" 2a06:fb00:1::1:1"#,
        );
        let TelnetCommand::Oneoff(inner) = cmd else {
            panic!("not the expected command");
        };
        let TelnetCommand::Ping(spec) = *inner else {
            panic!("oneoff is not a ping");
        };
        assert_eq!(spec.msm_id, 222_024_177);
        assert_eq!(spec.schedule.interval, 0);

        let cmd = parse_command(
            r#"ONEOFF /home/atlas/crons/oneoff evtraceroute -6 -I -a 16 -c 3 -f 1 -m 32 -w 4000 -z 10 -S 48 -p 80 -D 0 -H 0 -A "222017431" google.com"#,
        );
        let TelnetCommand::Oneoff(inner) = cmd else {
            panic!("not the expected command");
        };
        assert!(
            matches!(*inner, TelnetCommand::Traceroute(_)),
            "not a traceroute"
        );
        // A recurring CRONLINE is not one.
        assert!(matches!(
            parse_command(
                r#"CRONLINE 240 274 1770761451 UNIFORM 3 evping -4 -c 3 -A "1001" 193.0.14.129"#
            ),
            TelnetCommand::Ping(_)
        ));
        assert!(matches!(
            parse_command("ONEOFF /home/atlas/crons/oneoff nosuchtool x"),
            TelnetCommand::Unknown(_)
        ));
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
            // Not `{other:?}`: printing the parsed command trips CodeQL's
            // rust/cleartext-logging (an evsslgetcert line is sensitive).
            _ => panic!("unexpected command variant"),
        };
        assert_eq!(spec.msm_id, 13027);
        assert_eq!(spec.url, "http://[2001:67c:2e8:16::c100:68d]");
        assert_eq!((spec.af, spec.store_headers, spec.http10), (6, 1024, false));
        assert_eq!(spec.schedule.start_time, 3751);

        let spec = match parse_command(
            r#"CRONLINE 86400 24620 1794447121 UNIFORM 3 evhttpget -4 -0 --user-agent x --timeout 5000 -A "12023" -O /home/atlas/data/new/7 http://www.ripe.net/favicon.ico"#,
        ) {
            TelnetCommand::Http(s) => s,
            // Not `{other:?}`: printing the parsed command trips CodeQL's
            // rust/cleartext-logging (an evsslgetcert line is sensitive).
            _ => panic!("unexpected command variant"),
        };
        assert_eq!((spec.af, spec.store_headers, spec.http10), (4, 0, true));
        assert_eq!(
            spec.url, "http://www.ripe.net/favicon.ico",
            "option values are not the URL"
        );
    }
}
