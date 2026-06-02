//! Starla - RIPE Atlas Software Probe
//!
//! Main orchestration binary for the Atlas probe.
//!
//! This binary ties together all probe components:
//! - SSH tunnel to controller for secure communication
//! - Registration protocol (INIT/KEEP)
//! - Telnet server for receiving measurement commands
//! - Scheduler for running measurements
//! - Results uploader for reporting back

use anyhow::Result;
use clap::Parser;
use starla_common::logging::{init_logging, LogConfig};
use starla_controller::{
    InitResponse, KnownHosts, ProbeInitInfo, SshConfig, SshConnection, TelnetCommand,
};
#[cfg(feature = "metrics-export")]
use starla_metrics::MetricsRegistry;
use starla_results::{
    ResultHandler, ResultHandlerConfig, UploadStream, UploadTransport, UploaderConfig,
};
use starla_scheduler::{
    DnsJobSpec, HttpJobSpec, MeasurementJob, MeasurementSpec, NtpJobSpec, PingJobSpec,
    SchedulerCommand, TlsJobSpec, TracerouteJobSpec,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Run the status socket server for tray app communication.
/// Listens on a Unix domain socket; on each connection writes current
/// probe status as JSON and closes.
#[cfg(unix)]
async fn run_status_socket(
    status: Arc<tokio::sync::Mutex<starla_common::status::ProbeStatus>>,
    start_time: u64,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    let socket_path = starla_common::status_socket_path();

    // Remove stale socket
    let _ = std::fs::remove_file(&socket_path);

    // Ensure parent directory exists
    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let listener = UnixListener::bind(&socket_path)?;
    debug!("Status socket listening on {}", socket_path.display());

    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut s = status.lock().await;

        // Update uptime
        s.uptime_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(start_time);

        let json = serde_json::to_string_pretty(&*s).unwrap_or_default();
        drop(s);
        let _ = stream.write_all(json.as_bytes()).await;
        let _ = stream.shutdown().await;
    }
}

#[cfg(not(unix))]
async fn run_status_socket(
    _status: Arc<tokio::sync::Mutex<starla_common::status::ProbeStatus>>,
    _start_time: u64,
) -> anyhow::Result<()> {
    // TODO: Windows named pipe support
    Ok(())
}

/// SSH-based upload transport that opens direct-tcpip channels
/// to the controller's HTTP result endpoint.
///
/// The inner SSH connection is set after the KEEP session is established
/// and updated on reconnection.
struct SshUploadTransport {
    ssh: Arc<tokio::sync::Mutex<Option<Arc<SshConnection>>>>,
}

impl UploadTransport for SshUploadTransport {
    fn open(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<Box<dyn UploadStream>>> + Send + '_>,
    > {
        Box::pin(async {
            let guard = self.ssh.lock().await;
            let ssh = guard
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("SSH connection not established"))?;
            let channel = ssh.open_direct_tcpip("127.0.0.1", 8080).await?;
            let stream = starla_controller::channel_to_stream(channel);
            Ok(Box::new(stream) as Box<dyn UploadStream>)
        })
    }
}

/// Starla - RIPE Atlas Software Probe
#[derive(Parser, Debug)]
#[command(
    name = "starla",
    version = starla_common::VERSION,
    about = "Starla - A Rust implementation of the RIPE Atlas Software Probe",
    long_about = None
)]
struct Args {
    /// Log directory (defaults to stdout)
    #[arg(long)]
    log_dir: Option<PathBuf>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(short = 'l', long, default_value = "info")]
    log_level: String,

    /// Configuration file path
    ///
    /// If not specified, looks for config.toml in:
    /// 1. $CONFIGURATION_DIRECTORY (if set by systemd)
    /// 2. $XDG_CONFIG_HOME/starla/
    /// 3. Root: /etc/starla/, non-root: ~/.config/starla/
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// State directory for databases, keys, and probe ID
    ///
    /// If not specified, resolved via:
    /// 1. $STATE_DIRECTORY (if set by systemd)
    /// 2. $XDG_STATE_HOME/starla/
    /// 3. Root: /var/lib/starla/, non-root: ~/.local/state/starla/
    #[arg(short, long)]
    state_dir: Option<PathBuf>,

    /// Runtime directory for ephemeral databases and caches
    ///
    /// If not specified, resolved via:
    /// 1. $RUNTIME_DIRECTORY (if set by systemd)
    /// 2. $XDG_RUNTIME_DIR/starla/
    /// 3. Root: /run/starla/, non-root: /tmp/starla-<uid>/
    #[arg(short, long)]
    runtime_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Apply CLI overrides before any path resolution
    if let Some(state_dir) = args.state_dir {
        starla_common::set_state_dir(state_dir);
    }
    if let Some(runtime_dir) = args.runtime_dir {
        starla_common::set_runtime_dir(runtime_dir);
    }

    // Install rustls crypto provider (aws-lc-rs) before any TLS operations
    // This is required for rustls 0.23+
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Initialize logging
    let log_config = LogConfig {
        log_dir: args.log_dir,
        level: args.log_level,
        json_format: cfg!(feature = "structured-logging"),
    };

    init_logging(log_config)?;

    info!(
        "Starla v{} (fw {})",
        starla_common::VERSION,
        starla_common::FIRMWARE_VERSION
    );

    // Resolve config file path
    let config_path = args.config.unwrap_or_else(starla_common::config_file);
    debug!("Config file: {}", config_path.display());

    let state_dir = starla_common::state_dir();
    debug!("State directory: {}", state_dir.display());

    // Load configuration
    let config = if config_path.exists() {
        starla_common::ProbeConfig::from_file(&config_path)?
    } else {
        debug!(
            "Configuration file not found at {}, using defaults",
            config_path.display()
        );
        starla_common::ProbeConfig::default()
    };

    debug!("Config: state_dir={}", state_dir.display());

    // Ensure state and runtime directories exist
    if let Err(e) = std::fs::create_dir_all(&state_dir) {
        error!(
            "Failed to create state directory {}: {}",
            state_dir.display(),
            e
        );
        return Err(e.into());
    }
    let runtime_dir = starla_common::runtime_dir();
    if let Err(e) = std::fs::create_dir_all(&runtime_dir) {
        error!(
            "Failed to create runtime directory {}: {}",
            runtime_dir.display(),
            e
        );
        return Err(e.into());
    }
    debug!("Runtime directory: {}", runtime_dir.display());

    // Probe ID is received from the registration server during INIT
    let mut probe_id = starla_common::ProbeId(0);

    // Initialize Metrics (only when feature is enabled)
    #[cfg(feature = "metrics-export")]
    let metrics = MetricsRegistry::new()?;

    // Start Metrics Server if enabled
    #[allow(unused_variables)]
    let metrics_cancel_token = CancellationToken::new();
    #[cfg(feature = "metrics-export")]
    {
        if config.metrics.enabled {
            let metrics_clone = metrics.clone();
            let addr_str = config.metrics.listen_addr.clone();
            let cancel_clone = metrics_cancel_token.clone();
            tokio::spawn(async move {
                match addr_str.parse() {
                    Ok(addr) => {
                        debug!(%addr, "Starting metrics server");
                        if let Err(e) = starla_metrics::server::start_metrics_server(
                            metrics_clone,
                            addr,
                            cancel_clone,
                        )
                        .await
                        {
                            error!("Metrics server error: {}", e);
                        }
                    }
                    Err(e) => error!("Invalid metrics listen address '{}': {}", addr_str, e),
                }
            });
        }
    }

    // Initialize Results Handler with in-memory queue
    let ssh_for_upload: Arc<tokio::sync::Mutex<Option<Arc<SshConnection>>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let transport = Box::new(SshUploadTransport {
        ssh: ssh_for_upload.clone(),
    });

    #[cfg(feature = "metrics-export")]
    let metrics_for_results = metrics.clone();
    #[cfg(not(feature = "metrics-export"))]
    let metrics_for_results = starla_metrics::MetricsRegistry;

    let result_handler = Arc::new(ResultHandler::new(
        transport,
        UploaderConfig::default(),
        ResultHandlerConfig {
            max_queue_size: config.storage.max_queue_size,
            ..ResultHandlerConfig::default()
        },
        metrics_for_results,
    ));

    // Initialize Scheduler
    #[cfg(feature = "metrics-export")]
    let metrics_for_scheduler = metrics.clone();
    #[cfg(not(feature = "metrics-export"))]
    let metrics_for_scheduler = starla_metrics::MetricsRegistry;

    let mut scheduler = starla_scheduler::Scheduler::new(probe_id, metrics_for_scheduler);
    scheduler.set_result_handler(result_handler.clone());

    let scheduler_tx = scheduler.command_sender();
    let scheduler_cancel = scheduler.cancel_token();

    let result_cancel_token = CancellationToken::new();

    // Create channel for receiving telnet commands
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<TelnetCommand>(100);

    // Telnet state: passed to SSH connections so forwarded connections
    // are handled directly without a local TCP listener
    let telnet_session_id = std::sync::Arc::new(tokio::sync::RwLock::new(None));
    let telnet_state = starla_controller::TelnetState {
        command_tx: cmd_tx,
        probe_id: 0, // Updated after registration
        session_id: telnet_session_id.clone(),
    };

    // Command handler task - converts telnet commands to scheduler jobs
    #[cfg(feature = "metrics-export")]
    let metrics_cmd = metrics.clone();
    let scheduler_tx_cmd = scheduler_tx.clone();
    let scheduler_cancel_cmd = scheduler_cancel.clone();
    let host_telemetry_cmd = result_handler.host_telemetry();
    tokio::spawn(async move {
        // Track scheduled measurements for batched logging
        let mut scheduled_counts: std::collections::HashMap<&'static str, u32> =
            std::collections::HashMap::new();
        let mut last_log_time = std::time::Instant::now();
        const LOG_BATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

        // Helper to log and reset counts
        let log_scheduled_summary = |counts: &mut std::collections::HashMap<&'static str, u32>| {
            if counts.is_empty() {
                return;
            }
            let summary: Vec<String> = counts
                .iter()
                .map(|(typ, count)| format!("{}: {}", typ, count))
                .collect();
            info!("Scheduled measurements: {}", summary.join(", "));
            counts.clear();
        };

        loop {
            // Use timeout to batch log messages
            let cmd = tokio::select! {
                cmd = cmd_rx.recv() => cmd,
                _ = tokio::time::sleep(LOG_BATCH_INTERVAL) => {
                    // Timeout - log any accumulated counts
                    log_scheduled_summary(&mut scheduled_counts);
                    last_log_time = std::time::Instant::now();
                    continue;
                }
            };

            let Some(cmd) = cmd else {
                // Channel closed - log final counts
                log_scheduled_summary(&mut scheduled_counts);
                break;
            };

            // Check if scheduler has been cancelled
            if scheduler_cancel_cmd.is_cancelled() {
                debug!("Scheduler cancelled, stopping command handler");
                break;
            }

            // Log accumulated counts if enough time has passed
            if last_log_time.elapsed() >= LOG_BATCH_INTERVAL && !scheduled_counts.is_empty() {
                log_scheduled_summary(&mut scheduled_counts);
                last_log_time = std::time::Instant::now();
            }

            debug!("Processing command: {:?}", cmd);

            // Helper macro to schedule/execute a measurement job.
            // Handles logging, metrics, and recurring vs one-shot dispatch.
            macro_rules! dispatch_measurement {
                ($msm_id:expr, $schedule:expr, $spread:expr,
                 $type_name:expr, $metric_name:expr, $measurement_spec:expr) => {{
                    let is_recurring = $schedule.interval > 0;
                    if is_recurring {
                        debug!(
                            msm_id = $msm_id,
                            interval = $schedule.interval,
                            "Scheduled {} measurement",
                            $type_name
                        );
                        *scheduled_counts.entry($type_name).or_insert(0) += 1;
                    } else {
                        debug!(msm_id = $msm_id, "One-shot {}", $type_name);
                    }
                    #[cfg(feature = "metrics-export")]
                    metrics_cmd.record_measurement_started($metric_name);
                    let job = MeasurementJob {
                        msm_id: $msm_id,
                        interval: $schedule.interval,
                        start_time: $schedule.start_time,
                        end_time: $schedule.stop_time,
                        spread: $spread.unwrap_or(0) as u64,
                        spec: $measurement_spec,
                    };
                    if is_recurring {
                        scheduler_tx_cmd.send(SchedulerCommand::Schedule(job)).await
                    } else {
                        scheduler_tx_cmd
                            .send(SchedulerCommand::ExecuteNow(job))
                            .await
                    }
                }};
            }

            let result = match cmd {
                TelnetCommand::Ping(spec) => dispatch_measurement!(
                    spec.msm_id,
                    spec.schedule,
                    spec.spread,
                    "ping",
                    "ping",
                    MeasurementSpec::Ping(PingJobSpec {
                        target: spec.target,
                        af: spec.af,
                        packets: spec.packets,
                        size: spec.size,
                    })
                ),
                TelnetCommand::Traceroute(spec) => dispatch_measurement!(
                    spec.msm_id,
                    spec.schedule,
                    spec.spread,
                    "traceroute",
                    "traceroute",
                    MeasurementSpec::Traceroute(TracerouteJobSpec {
                        target: spec.target,
                        af: spec.af,
                        protocol: spec.protocol,
                        paris: spec.paris.unwrap_or(0),
                        first_hop: spec.first_hop,
                        max_hops: spec.max_hops,
                        size: spec.size,
                    })
                ),
                TelnetCommand::Dns(spec) => dispatch_measurement!(
                    spec.msm_id,
                    spec.schedule,
                    spec.spread,
                    "dns",
                    "dns",
                    MeasurementSpec::Dns(DnsJobSpec {
                        target: spec.target,
                        af: spec.af,
                        protocol: spec.protocol,
                        query_type: spec.query_type,
                        query_class: spec.query_class,
                        query_argument: spec.query_argument,
                        use_dnssec: spec.use_dnssec,
                        recursion_desired: spec.recursion_desired,
                    })
                ),
                TelnetCommand::Http(spec) => dispatch_measurement!(
                    spec.msm_id,
                    spec.schedule,
                    spec.spread,
                    "http",
                    "http",
                    MeasurementSpec::Http(HttpJobSpec {
                        url: spec.url,
                        method: spec.method,
                        af: spec.af,
                        headers: spec.headers,
                        body: spec.body,
                        max_body_size: spec.max_body_size,
                    })
                ),
                TelnetCommand::Tls(spec) => dispatch_measurement!(
                    spec.msm_id,
                    spec.schedule,
                    spec.spread,
                    "tls",
                    "sslcert",
                    MeasurementSpec::Tls(TlsJobSpec {
                        target: spec.target,
                        port: spec.port,
                        af: spec.af,
                        hostname: spec.hostname,
                    })
                ),
                TelnetCommand::Ntp(spec) => dispatch_measurement!(
                    spec.msm_id,
                    spec.schedule,
                    spec.spread,
                    "ntp",
                    "ntp",
                    MeasurementSpec::Ntp(NtpJobSpec {
                        target: spec.target,
                        af: spec.af,
                        packets: spec.packets,
                    })
                ),
                TelnetCommand::HostTelemetry(spec) => {
                    debug!(?spec.kind, interval = spec.interval, "Scheduling host telemetry");
                    match spec.kind {
                        starla_controller::HostTelemetryKind::Buddyinfo => {
                            host_telemetry_cmd
                                .schedule_buddyinfo(spec.interval, spec.lowmem, spec.msm_id)
                                .await;
                        }
                        starla_controller::HostTelemetryKind::Rptaddrs => {
                            let msm_id = spec.msm_id.unwrap_or(9104);
                            host_telemetry_cmd
                                .schedule_rptaddrs(spec.interval, msm_id)
                                .await;
                        }
                    }
                    Ok(())
                }
                TelnetCommand::Status => {
                    debug!("Status request received");
                    Ok(())
                }
                TelnetCommand::Stop(msm_id) => {
                    debug!(msm_id, "Stop measurement request");
                    scheduler_tx_cmd.send(SchedulerCommand::Stop(msm_id)).await
                }
                TelnetCommand::Ignored(_) => {
                    // Known commands that we don't need to handle (CRONTAB, internal Atlas
                    // commands)
                    Ok(())
                }
                TelnetCommand::Unknown(s) => {
                    warn!("Unknown command: {}", s);
                    Ok(())
                }
            };

            if let Err(e) = result {
                // Channel closed likely means shutdown is in progress
                if scheduler_cancel_cmd.is_cancelled() {
                    debug!("Scheduler cancelled during command send");
                } else {
                    warn!(
                        "Failed to send command to scheduler: {} (scheduler may be shutting down)",
                        e
                    );
                }
                break;
            }
        }
        debug!("Command handler task ended");
    });

    // Start Scheduler Loop
    let mut scheduler_task = tokio::spawn(async move {
        scheduler.run().await;
    });

    debug!("Probe initialization complete");

    // Status socket for tray app communication
    let probe_status = Arc::new(tokio::sync::Mutex::new(
        starla_common::status::ProbeStatus {
            probe_id: probe_id.0,
            connected: false,
            controller: None,
            uptime_secs: 0,
            measurements: std::collections::HashMap::new(),
            queue_depth: 0,
            public_key: std::fs::read_to_string(starla_common::probe_pubkey_path())
                .ok()
                .map(|s| s.trim().to_string()),
            last_connection_error: None,
            pause: starla_common::read_pause_state(),
        },
    ));
    if config.network.status_socket {
        let status = probe_status.clone();
        let start_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        tokio::spawn(async move {
            if let Err(e) = run_status_socket(status, start_time).await {
                debug!("Status socket ended: {}", e);
            }
        });

        // Mirror the on-disk pause file into the in-memory status so
        // the tray's read_status() shows current state without having
        // to also read the file itself.
        let status = probe_status.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let current = starla_common::read_pause_state().and_then(|s| {
                    if s.is_active(chrono::Utc::now()) {
                        Some(s)
                    } else {
                        None
                    }
                });
                let mut s = status.lock().await;
                if s.pause != current {
                    s.pause = current;
                }
            }
        });
    }

    // Controller connection
    debug!("Connecting to controller...");

    // Load SSH key from: env var > systemd credential > file > generate new
    let key = if let Ok(pem) = std::env::var("STARLA_SSH_KEY") {
        match starla_controller::load_key_from_string(&pem) {
            Ok(k) => {
                debug!("Loaded SSH key from STARLA_SSH_KEY env var");
                k
            }
            Err(e) => {
                error!("Failed to parse STARLA_SSH_KEY: {}", e);
                anyhow::bail!("Invalid STARLA_SSH_KEY");
            }
        }
    } else if let Ok(creds_dir) = std::env::var("CREDENTIALS_DIRECTORY") {
        let cred_path = std::path::PathBuf::from(creds_dir).join("ssh-key");
        if cred_path.exists() {
            match starla_controller::load_key(&cred_path).await {
                Ok(k) => {
                    debug!("Loaded SSH key from systemd credential");
                    k
                }
                Err(e) => {
                    error!("Failed to load SSH key from credential: {}", e);
                    anyhow::bail!("Invalid ssh-key credential");
                }
            }
        } else {
            // Credentials dir exists but no ssh-key: fall through to file
            let key_path = starla_common::probe_key_path();
            starla_controller::load_key(&key_path).await?
        }
    } else {
        let key_path = starla_common::probe_key_path();
        if key_path.exists() {
            match starla_controller::load_key(&key_path).await {
                Ok(k) => {
                    debug!("Loaded SSH key from {}", key_path.display());
                    k
                }
                Err(e) => {
                    warn!("Failed to load SSH key: {}. Generating new key...", e);
                    let new_key = starla_controller::generate_key()?;
                    if let Err(e) = starla_controller::save_key(&new_key, &key_path).await {
                        warn!("Failed to save SSH key: {}", e);
                    }
                    new_key
                }
            }
        } else {
            debug!("No SSH key found, generating new key...");
            let new_key = starla_controller::generate_key()?;
            if let Err(e) = starla_controller::save_key(&new_key, &key_path).await {
                warn!("Failed to save SSH key: {}", e);
            }
            if let Ok(fp) = starla_controller::key_fingerprint(&new_key) {
                info!("Generated new probe key: {}", fp);
            }
            let pubkey_path = starla_common::probe_pubkey_path();
            match std::fs::read_to_string(&pubkey_path) {
                Ok(contents) => info!("Public key:\n{}", contents.trim()),
                Err(_) => info!("Public key file: {}", pubkey_path.display()),
            }
            info!("Register your probe at: https://atlas.ripe.net/apply/swprobe/");
            new_key
        }
    };

    // Log probe identity and update status with public key
    if probe_id.0 != 0 {
        if let Ok(fp) = starla_controller::key_fingerprint(&key) {
            info!("Probe {} ({})", probe_id.0, fp);
        }
    }
    {
        let pubkey_path = starla_common::probe_pubkey_path();
        if let Ok(contents) = std::fs::read_to_string(&pubkey_path) {
            probe_status.lock().await.public_key = Some(contents.trim().to_string());
        }
    }

    // Load known SSH host keys for server verification
    let known_hosts_path = starla_common::known_hosts_path();
    let known_hosts = KnownHosts::load(&known_hosts_path);

    // Try to connect to registration servers
    let ssh_config = SshConfig {
        connect_timeout: Duration::from_secs(config.controller.ssh_timeout),
        keepalive_interval: Duration::from_secs(config.controller.keepalive_interval),
        ..SshConfig::default()
    };
    let servers: Vec<&str> = config
        .controller
        .registration_servers
        .iter()
        .map(|s| s.as_str())
        .collect();

    // Create probe info for registration
    let probe_info = ProbeInitInfo::new(starla_common::FIRMWARE_VERSION);

    // Registration loop: retries indefinitely with backoff
    let mut reg_delay = Duration::from_secs(5);
    let max_reg_delay = Duration::from_secs(300);
    let mut printed_key = false;

    // Outer loop: re-runs register → controller-connect if the initial
    // controller handshake fails (typically the controller hasn't sync'd
    // the freshly-approved probe key yet). Keeps the process alive so
    // launchd / systemd don't penalty-box a fast-exit cycle.
    let mut ctrl_delay = Duration::from_secs(5);
    let max_ctrl_delay = Duration::from_secs(60);

    'register_and_connect: loop {
        let controller_info = loop {
            // Connect to registration server
            #[cfg(feature = "metrics-export")]
            metrics.record_connection_attempt();
            let reg_ssh = match starla_controller::SshConnection::connect_to_servers(
                &servers,
                &key,
                ssh_config.clone(),
                known_hosts.clone(),
            )
            .await
            {
                Ok(ssh) => {
                    reg_delay = Duration::from_secs(5);
                    ssh
                }
                Err(e) => {
                    debug!("Failed to connect to registration server: {}", e);
                    probe_status.lock().await.last_connection_error = Some(e.to_string());
                    if !printed_key {
                        let pubkey_path = starla_common::probe_pubkey_path();
                        if let Ok(contents) = std::fs::read_to_string(&pubkey_path) {
                            info!("Public key:\n{}", contents.trim());
                        }
                        info!("Retrying registration until probe is connected...");
                        printed_key = true;
                    }
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            info!("Received Ctrl+C");
                            scheduler_cancel.cancel();
                            metrics_cancel_token.cancel();
                            result_cancel_token.cancel();
                            return Ok(());
                        }
                        _ = tokio::time::sleep(reg_delay) => {
                            reg_delay = std::cmp::min(reg_delay * 2, max_reg_delay);
                            continue;
                        }
                    }
                }
            };

            // Send INIT
            match reg_ssh.init(Some(&probe_info)).await {
                Ok(InitResponse::Controller(info)) => {
                    info!("Got controller assignment: {}:{}", info.host, info.port);

                    {
                        let mut s = probe_status.lock().await;
                        if info.probe_id != 0 {
                            probe_id = starla_common::ProbeId(info.probe_id);
                            info!("Probe ID: {}", info.probe_id);
                            s.probe_id = info.probe_id;
                        }
                        s.controller = Some(format!("{}:{}", info.host, info.port));
                        s.last_connection_error = None;
                    }

                    break info;
                }
                Ok(InitResponse::Ok) | Ok(InitResponse::Wait { .. }) => {
                    // Not yet registered: print key and keep retrying
                    if !printed_key {
                        let pubkey_path = starla_common::probe_pubkey_path();
                        if let Ok(contents) = std::fs::read_to_string(&pubkey_path) {
                            info!("Public key:\n{}", contents.trim());
                        }
                        info!("Register at: https://atlas.ripe.net/apply/swprobe/");
                        info!("Retrying registration until probe is approved...");
                        printed_key = true;
                    }
                    probe_status.lock().await.last_connection_error = Some(
                        "Probe key not yet approved by RIPE Atlas — register the public key above"
                            .to_string(),
                    );
                    debug!("Waiting for registration approval...");
                }
                Ok(InitResponse::ControllerReady { .. }) => {
                    debug!("Unexpected ControllerReady from registration server");
                }
                Err(e) => {
                    debug!("Registration INIT failed: {}", e);
                }
            }

            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl+C");
                    scheduler_cancel.cancel();
                    metrics_cancel_token.cancel();
                    result_cancel_token.cancel();
                    return Ok(());
                }
                _ = tokio::time::sleep(reg_delay) => {
                    reg_delay = std::cmp::min(reg_delay * 2, max_reg_delay);
                }
            }
        };

        // Step 2: Connect to the assigned controller
        let controller_addr = format!("{}:{}", controller_info.host, controller_info.port);
        debug!("Connecting to controller at {}", controller_addr);

        match starla_controller::SshConnection::connect(
            &controller_info.host,
            controller_info.port,
            &key,
            ssh_config.clone(),
            known_hosts.clone(),
            None,
        )
        .await
        {
            Ok(ctrl_ssh) => {
                info!("Connected to controller");

                {
                    let mut s = probe_status.lock().await;
                    s.connected = true;
                    s.last_connection_error = None;
                }

                // Step 3: Controller INIT - get REMOTE_PORT (may need to retry on
                // WAIT/OK)
                let (remote_port, session_id) = loop {
                    match ctrl_ssh.init(None).await {
                        Ok(InitResponse::ControllerReady {
                            remote_port,
                            session_id,
                        }) => {
                            info!(
                                "Controller ready, remote port: {}, session_id: {}",
                                remote_port, session_id
                            );
                            break (remote_port, session_id);
                        }
                        Ok(InitResponse::Wait { timeout_secs }) => {
                            debug!(
                                "Controller requested wait, retrying in {} seconds",
                                timeout_secs
                            );
                            tokio::select! {
                                _ = tokio::signal::ctrl_c() => {
                                    info!("Received Ctrl+C during wait");
                                    metrics_cancel_token.cancel();
                                    result_cancel_token.cancel();
                                    return Ok(());
                                }
                                _ = tokio::time::sleep(Duration::from_secs(timeout_secs as u64)) => {
                                    // Check if connection is still alive before retrying
                                    if !ctrl_ssh.is_connected().await {
                                        error!("Controller connection lost during wait");
                                        error!("Please restart the probe to reconnect");
                                        return Ok(());
                                    }
                                    debug!("Retrying controller INIT...");
                                    continue;
                                }
                            }
                        }
                        Ok(InitResponse::Ok) => {
                            // Controller said OK but no REMOTE_PORT - retry after
                            // delay
                            debug!("Controller said OK but no REMOTE_PORT, retrying in 30 seconds");
                            tokio::select! {
                                _ = tokio::signal::ctrl_c() => {
                                    info!("Received Ctrl+C during wait");
                                    metrics_cancel_token.cancel();
                                    result_cancel_token.cancel();
                                    return Ok(());
                                }
                                _ = tokio::time::sleep(Duration::from_secs(30)) => {
                                    // Check if connection is still alive before retrying
                                    if !ctrl_ssh.is_connected().await {
                                        error!("Controller connection lost during wait");
                                        error!("Please restart the probe to reconnect");
                                        return Ok(());
                                    }
                                    debug!("Retrying controller INIT...");
                                    continue;
                                }
                            }
                        }
                        Ok(InitResponse::Controller(_)) => {
                            // This shouldn't happen from a controller
                            error!(
                                "Got CONTROLLER response from controller (expected REMOTE_PORT)"
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            // Connection may have dropped during the wait period
                            // Exit so the probe can be restarted for a fresh
                            // connection
                            error!(
                                "Controller INIT failed: {} (connection may have timed out during \
                                 wait)",
                                e
                            );
                            error!("Please restart the probe to reconnect");
                            return Ok(());
                        }
                    }
                };

                // INIT connection served its purpose, drop it
                drop(ctrl_ssh);

                // Session ID is set on TelnetState before each KEEP connection

                let actual_probe_id = probe_id.0;

                // Set result upload endpoint path (query params for HTTP POST)
                let endpoint_path =
                    format!("/?PROBE_ID={}&SESSION_ID={}", actual_probe_id, session_id);
                debug!("Result upload endpoint path: {}", endpoint_path);
                result_handler
                    .set_endpoint_path(endpoint_path)
                    .await
                    .expect("endpoint path is always well-formed");

                // Set session ID for upload body footer (per httppost --post-footer
                // behavior)
                result_handler.set_session_id(session_id.clone()).await;

                // Step 4: Connection loop with automatic reconnection
                let mut reconnect_delay = Duration::from_secs(5);
                let max_reconnect_delay = Duration::from_secs(300);
                let mut connection_attempt = 0u32;
                let mut upload_loop_started = false;

                'connection_loop: loop {
                    connection_attempt += 1;

                    // Create a NEW connection for KEEP with reverse tunnel
                    debug!(
                        "Creating connection for KEEP session (attempt {})",
                        connection_attempt
                    );
                    // Update telnet state with current probe ID and session ID
                    let mut keep_telnet = telnet_state.clone();
                    keep_telnet.probe_id = actual_probe_id;
                    *keep_telnet.session_id.write().await = Some(session_id.clone());

                    let keep_ssh = match starla_controller::SshConnection::connect(
                        &controller_info.host,
                        controller_info.port,
                        &key,
                        ssh_config.clone(),
                        known_hosts.clone(),
                        Some(keep_telnet),
                    )
                    .await
                    {
                        Ok(ssh) => {
                            // Reset delay on successful connection
                            reconnect_delay = Duration::from_secs(5);
                            ssh
                        }
                        Err(e) => {
                            error!("Failed to connect for KEEP: {}", e);
                            warn!("Retrying connection in {:?}...", reconnect_delay);
                            tokio::select! {
                                _ = tokio::signal::ctrl_c() => {
                                    info!("Received Ctrl+C during reconnect wait");
                                    break 'connection_loop;
                                }
                                _ = tokio::time::sleep(reconnect_delay) => {
                                    reconnect_delay = std::cmp::min(
                                        reconnect_delay * 2,
                                        max_reconnect_delay
                                    );
                                    continue 'connection_loop;
                                }
                            }
                        }
                    };

                    // Setup reverse tunnel: controller connects to remote_port,
                    // SSH handler routes directly to telnet command parser
                    if let Err(e) = keep_ssh.request_reverse_tunnel(remote_port).await {
                        error!("Failed to setup reverse tunnel: {}", e);
                        warn!("Retrying connection in {:?}...", reconnect_delay);
                        tokio::time::sleep(reconnect_delay).await;
                        reconnect_delay = std::cmp::min(reconnect_delay * 2, max_reconnect_delay);
                        continue 'connection_loop;
                    }
                    debug!("Reverse tunnel established: remote port {}", remote_port);

                    // Share the SSH connection for result uploads (direct-tcpip)
                    // and KEEP session monitoring
                    let keep_ssh = Arc::new(keep_ssh);
                    *ssh_for_upload.lock().await = Some(
                        // SAFETY: We need the inner SshConnection for both the
                        // upload transport and run_keep_session. Clone the Arc.
                        keep_ssh.clone(),
                    );

                    info!("Controller connection established successfully");
                    probe_status.lock().await.connected = true;
                    #[cfg(feature = "metrics-export")]
                    metrics.set_connected(true);

                    // Start the result upload loop once (on first successful connection)
                    if !upload_loop_started {
                        upload_loop_started = true;
                        let result_handler_loop = result_handler.clone();
                        let result_cancel_clone = result_cancel_token.clone();
                        tokio::spawn(async move {
                            if let Err(e) = result_handler_loop.run(result_cancel_clone).await {
                                error!("Result handler failed: {}", e);
                            }
                        });
                        debug!("Result upload loop started");
                    }

                    // Run KEEP session: blocks until connection drops.
                    let keep_ssh_for_keep = {
                        let guard = ssh_for_upload.lock().await;
                        match guard.as_ref() {
                            Some(ssh) => ssh.clone(),
                            None => {
                                error!("SSH connection not set before KEEP session");
                                continue 'connection_loop;
                            }
                        }
                    };
                    let keep_task =
                        tokio::spawn(async move { keep_ssh_for_keep.run_keep_session().await });

                    // Wait for shutdown or connection loss
                    let should_reconnect = tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            info!("Received Ctrl+C");
                            false
                        }
                        _ = &mut scheduler_task => {
                            error!("Scheduler task ended unexpectedly");
                            false
                        }
                        result = keep_task => {
                            match result {
                                Ok(Err(e)) => warn!("KEEP session lost: {}", e),
                                Err(e) => warn!("KEEP task panicked: {}", e),
                                _ => warn!("KEEP session ended"),
                            }
                            probe_status.lock().await.connected = false;
                            #[cfg(feature = "metrics-export")]
                            metrics.set_connected(false);
                            // Clear SSH connection so uploads fail fast
                            *ssh_for_upload.lock().await = None;
                            true // Reconnect
                        }
                    };

                    if !should_reconnect {
                        break 'connection_loop;
                    }

                    // Wait before reconnecting
                    debug!("Reconnecting in {:?}...", reconnect_delay);
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            info!("Received Ctrl+C during reconnect wait");
                            break 'connection_loop;
                        }
                        _ = tokio::time::sleep(reconnect_delay) => {
                            // Continue to reconnect
                        }
                    }
                }

                // Cancel all tasks gracefully
                info!("Initiating graceful shutdown...");
                scheduler_cancel.cancel();
                metrics_cancel_token.cancel();
                result_cancel_token.cancel();
            }
            Err(e) => {
                error!(
                    "Failed to connect to controller: {} — retrying in {}s",
                    e,
                    ctrl_delay.as_secs()
                );
                {
                    let mut s = probe_status.lock().await;
                    s.connected = false;
                    s.last_connection_error = Some(e.to_string());
                }
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        info!("Received Ctrl+C during controller-connect retry");
                        scheduler_cancel.cancel();
                        metrics_cancel_token.cancel();
                        result_cancel_token.cancel();
                        return Ok(());
                    }
                    _ = tokio::time::sleep(ctrl_delay) => {
                        ctrl_delay = std::cmp::min(ctrl_delay * 2, max_ctrl_delay);
                        continue 'register_and_connect;
                    }
                }
            }
        }

        // Inner connection_loop ran to completion (graceful shutdown) — exit
        // the outer retry loop instead of re-registering.
        break 'register_and_connect;
    } // end 'register_and_connect

    info!("Shutting down probe");

    Ok(())
}
