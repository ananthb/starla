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
    InitResponse, KnownHosts, ProbeInitInfo, SshConfig, TelnetCommand, TelnetServer,
};
#[cfg(feature = "metrics-export")]
use starla_metrics::MetricsRegistry;
use starla_results::{CompressionMode, ResultHandler, ResultHandlerConfig, UploaderConfig};
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

    /// Skip controller connection (for testing)
    #[arg(long)]
    standalone: bool,
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

    debug!(
        "Config: telnet={}, http={}, state_dir={}",
        config.network.telnet_port,
        config.network.http_post_port,
        state_dir.display()
    );

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

    // Read probe ID from state directory (written after registration)
    let probe_id = match starla_common::read_probe_id() {
        Some(id) => starla_common::ProbeId(id),
        None => starla_common::ProbeId(0),
    };

    // Initialize Metrics (only when feature is enabled)
    #[cfg(feature = "metrics-export")]
    let metrics = Arc::new(MetricsRegistry::new()?);

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

    // Initialize Database
    let db_path = starla_common::database_path();
    let db = Arc::new(starla_database::Database::connect(&db_path)?);

    // Initialize Results Handler with persistent queue
    let results_db_path = starla_common::results_queue_path();
    let uploader_config = UploaderConfig {
        endpoint: String::new(),          // Will be set after controller connection
        timeout: Duration::from_secs(15), // 15s to fail faster and trigger reconnection
        compression: CompressionMode::Auto,
        ..Default::default()
    };
    let result_handler_config = ResultHandlerConfig {
        batch_size: 10,
        upload_interval: Duration::from_secs(10),
        max_result_age_secs: 3600, // 1 hour
        max_attempts: 5,
        cleanup_interval: Duration::from_secs(300),
    };
    let result_handler = Arc::new(ResultHandler::new(
        &results_db_path,
        uploader_config,
        result_handler_config,
    )?);

    // Initialize Scheduler with result handler
    let mut scheduler = starla_scheduler::Scheduler::new(db.clone(), probe_id);
    scheduler.set_result_handler(result_handler.clone());

    // Get command sender and cancellation token for the scheduler
    let scheduler_tx = scheduler.command_sender();
    let scheduler_cancel = scheduler.cancel_token();

    // Start Background Cleanup Task
    let db_cleanup = db.clone();
    #[cfg(feature = "metrics-export")]
    let metrics_cleanup = metrics.clone();
    let cleanup_config = starla_database::CleanupConfig {
        retention_days: config.storage.retention_days,
        max_database_size_mb: config.storage.max_database_size_mb,
        cleanup_interval_hours: config.storage.cleanup_interval_hours,
    };
    let cleanup_interval =
        std::time::Duration::from_secs(config.storage.cleanup_interval_hours * 3600);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(cleanup_interval);
        loop {
            interval.tick().await;
            debug!("Running database cleanup");
            match starla_database::cleanup::run_cleanup_cycle(&db_cleanup, &cleanup_config) {
                Ok(stats) => {
                    let freed_mb = stats.database_size_before_mb - stats.database_size_after_mb;
                    debug!(
                        "Cleanup: deleted {} measurements, freed {:.2} MB",
                        stats.measurements_deleted_by_time + stats.measurements_deleted_by_size,
                        freed_mb
                    );
                    #[cfg(feature = "metrics-export")]
                    metrics_cleanup.record_cleanup_run(
                        stats.measurements_deleted_by_time,
                        stats.measurements_deleted_by_size,
                        (freed_mb.max(0.0) * 1024.0 * 1024.0) as u64,
                    );
                }
                Err(e) => error!("Cleanup error: {}", e),
            }
        }
    });

    // Result upload loop is started AFTER controller connection is established
    // (endpoint and HTTP proxy must be ready before uploads can work).
    // Results enqueued before that are held in the persistent queue.
    let result_cancel_token = CancellationToken::new();

    // Create channel for receiving telnet commands
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<TelnetCommand>(100);

    // Start Telnet Server with command channel
    // Use a temporary probe_id of 0 - will be updated after registration
    let telnet_port = config.network.telnet_port;
    let telnet_server = Arc::new(TelnetServer::with_channel(telnet_port, 0, cmd_tx));
    let telnet_server_clone = telnet_server.clone();
    tokio::spawn(async move {
        if let Err(e) = telnet_server_clone.run().await {
            error!("Telnet server error: {}", e);
        }
    });

    // Command handler task - converts telnet commands to scheduler jobs
    #[cfg(feature = "metrics-export")]
    let metrics_cmd = metrics.clone();
    let scheduler_tx_cmd = scheduler_tx.clone();
    let scheduler_cancel_cmd = scheduler_cancel.clone();
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

    // Controller connection (if not standalone)
    if !args.standalone {
        debug!("Connecting to controller...");

        // Load or generate SSH key
        let key_path = starla_common::probe_key_path();
        let key = if key_path.exists() {
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
            info!("Register your probe at: https://atlas.ripe.net/apply/swprobe/");
            new_key
        };

        // Log probe identity when both probe ID and key are available
        if probe_id.0 != 0 {
            if let Ok(fp) = starla_controller::key_fingerprint(&key) {
                info!("Probe {} ({})", probe_id.0, fp);
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

        match starla_controller::SshConnection::connect_to_servers(
            &servers,
            &key,
            ssh_config.clone(),
            known_hosts.clone(),
        )
        .await
        {
            Ok(mut reg_ssh) => {
                debug!("Connected to registration server");

                // Step 1: Get controller assignment
                // Either received immediately or after waiting for registration approval
                let controller_info = match reg_ssh.init(Some(&probe_info)).await {
                    Ok(InitResponse::Controller(info)) => {
                        info!("Got controller assignment: {}:{}", info.host, info.port);
                        info
                    }
                    Ok(InitResponse::Ok) | Ok(InitResponse::Wait { .. }) => {
                        // Probe is not yet fully registered - retry until approved
                        info!("Probe not yet fully registered.");
                        info!("Register at: https://atlas.ripe.net/apply/swprobe/");
                        info!(
                            "Public key: {}",
                            starla_common::probe_pubkey_path().display()
                        );
                        info!(
                            "After registration, save your probe ID to: {}",
                            starla_common::probe_id_path().display()
                        );

                        let retry_interval = Duration::from_secs(60);
                        loop {
                            tokio::select! {
                                _ = tokio::signal::ctrl_c() => {
                                    info!("Received Ctrl+C");
                                    metrics_cancel_token.cancel();
                                    result_cancel_token.cancel();
                                    return Ok(());
                                }
                                _ = tokio::time::sleep(retry_interval) => {
                                    debug!("Retrying INIT...");
                                    if !reg_ssh.is_connected().await {
                                        warn!("Registration SSH session closed, reconnecting...");
                                        match starla_controller::SshConnection::connect_to_servers(
                                            &servers,
                                            &key,
                                            ssh_config.clone(),
                                            known_hosts.clone(),
                                        )
                                        .await
                                        {
                                            Ok(new_reg) => {
                                                debug!("Reconnected to registration server");
                                                reg_ssh = new_reg;
                                            }
                                            Err(e) => {
                                                warn!("Failed to reconnect to registration server: {}", e);
                                                continue;
                                            }
                                        }
                                    }

                                    match reg_ssh.init(Some(&probe_info)).await {
                                        Ok(InitResponse::Controller(info)) => {
                                            info!(
                                                "Got controller: {}:{}",
                                                info.host, info.port
                                            );
                                            break info;
                                        }
                                        Ok(InitResponse::Ok)
                                        | Ok(InitResponse::Wait { .. })
                                        | Ok(InitResponse::ControllerReady { .. }) => {
                                            debug!("Still waiting for registration...");
                                        }
                                        Err(e) => {
                                            warn!("INIT retry failed: {}", e);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(InitResponse::ControllerReady { .. }) => {
                        error!("Unexpected ControllerReady from registration server");
                        scheduler_cancel.cancel();
                        metrics_cancel_token.cancel();
                        result_cancel_token.cancel();
                        return Ok(());
                    }
                    Err(e) => {
                        error!("Registration failed: {}", e);
                        scheduler_cancel.cancel();
                        metrics_cancel_token.cancel();
                        result_cancel_token.cancel();
                        return Ok(());
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
                )
                .await
                {
                    Ok(ctrl_ssh) => {
                        info!("Connected to controller");

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
                                    debug!(
                                        "Controller said OK but no REMOTE_PORT, retrying in 30 \
                                         seconds"
                                    );
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
                                        "Got CONTROLLER response from controller (expected \
                                         REMOTE_PORT)"
                                    );
                                    return Ok(());
                                }
                                Err(e) => {
                                    // Connection may have dropped during the wait period
                                    // Exit so the probe can be restarted for a fresh
                                    // connection
                                    error!(
                                        "Controller INIT failed: {} (connection may have timed \
                                         out during wait)",
                                        e
                                    );
                                    error!("Please restart the probe to reconnect");
                                    return Ok(());
                                }
                            }
                        };

                        // INIT connection served its purpose, drop it
                        drop(ctrl_ssh);

                        // Set session ID for telnet authentication
                        telnet_server.set_session_id(session_id.clone()).await;

                        // Get probe ID - should have been read from state dir at startup
                        // If not set (0), the probe hasn't been registered yet
                        let actual_probe_id = probe_id.0;
                        if actual_probe_id == 0 {
                            warn!(
                                "Probe ID is 0 (not yet registered). Results may not upload \
                                 correctly."
                            );
                            warn!("Register your probe at https://atlas.ripe.net/apply/swprobe/");
                            warn!(
                                "After registration, save your probe ID to: {}",
                                starla_common::probe_id_path().display()
                            );
                        }

                        // Set result upload endpoint
                        // The Atlas protocol uses HTTP POST with PROBE_ID and SESSION_ID as
                        // query parameters Results are
                        // uploaded via the SSH local port forward tunnel to the controller
                        let result_endpoint = format!(
                            "http://127.0.0.1:{}/?PROBE_ID={}&SESSION_ID={}",
                            config.network.http_post_port, actual_probe_id, session_id
                        );
                        debug!("Result upload endpoint: {}", result_endpoint);
                        result_handler.set_endpoint(result_endpoint).await;

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
                            let keep_ssh = match starla_controller::SshConnection::connect(
                                &controller_info.host,
                                controller_info.port,
                                &key,
                                ssh_config.clone(),
                                known_hosts.clone(),
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

                            // Setup reverse tunnel: remote_port on controller -> local
                            // telnet_port
                            if let Err(e) = keep_ssh.request_reverse_tunnel(remote_port).await {
                                error!("Failed to setup reverse tunnel: {}", e);
                                warn!("Retrying connection in {:?}...", reconnect_delay);
                                tokio::time::sleep(reconnect_delay).await;
                                reconnect_delay =
                                    std::cmp::min(reconnect_delay * 2, max_reconnect_delay);
                                continue 'connection_loop;
                            }
                            debug!(
                                "Reverse tunnel established: remote {} -> local {}",
                                remote_port, telnet_port
                            );

                            // Start HTTP proxy for result uploads
                            // Create a signal token that the proxy can use to trigger
                            // reconnection
                            let proxy_reconnect_signal = CancellationToken::new();
                            let http_post_port = config.network.http_post_port;
                            if let Err(e) = keep_ssh
                                .start_http_proxy(
                                    http_post_port,
                                    8080,
                                    proxy_reconnect_signal.clone(),
                                )
                                .await
                            {
                                error!(
                                    "Failed to start HTTP proxy on port {}: {}",
                                    http_post_port, e
                                );
                                error!(
                                    "Another process may be using this port. Configure a \
                                     different port in config.toml:"
                                );
                                error!("  [network]");
                                error!("  http_post_port = 8081");
                                break 'connection_loop;
                            }

                            info!("Controller connection established successfully");

                            // Start the result upload loop once (on first successful connection)
                            if !upload_loop_started {
                                upload_loop_started = true;
                                let result_handler_loop = result_handler.clone();
                                let result_cancel_clone = result_cancel_token.clone();
                                tokio::spawn(async move {
                                    if let Err(e) =
                                        result_handler_loop.run(result_cancel_clone).await
                                    {
                                        error!("Result handler failed: {}", e);
                                    }
                                });
                                debug!("Result upload loop started");
                            }

                            // Run KEEP session — blocks until connection drops.
                            // Also start the KEEP in a task so we can select on it.
                            let keep_task =
                                tokio::spawn(async move { keep_ssh.run_keep_session().await });

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
                                    true // Reconnect
                                }
                                _ = proxy_reconnect_signal.cancelled() => {
                                    warn!("HTTP proxy detected dead SSH session, will reconnect...");
                                    true
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
                        error!("Failed to connect to controller: {}", e);
                    }
                }
            }
            Err(e) => {
                error!("Failed to connect to any registration server: {}", e);
                info!("Running in standalone mode...");

                // Still wait for shutdown
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        info!("Received Ctrl+C");
                    }
                    _ = scheduler_task => {
                        error!("Scheduler task ended unexpectedly");
                    }
                }

                // Cancel all tasks gracefully
                scheduler_cancel.cancel();
                metrics_cancel_token.cancel();
                result_cancel_token.cancel();
            }
        }
    } else {
        info!("Running in standalone mode (no controller connection)");

        // Keep running until interrupted
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received Ctrl+C");
            }
            _ = scheduler_task => {
                error!("Scheduler task ended unexpectedly");
            }
        }

        // Cancel all tasks gracefully
        scheduler_cancel.cancel();
        metrics_cancel_token.cancel();
        result_cancel_token.cancel();
    }

    info!("Shutting down probe");

    // Flush result handler
    if let Err(e) = result_handler.flush().await {
        error!("Failed to flush result queue: {}", e);
    }

    db.close();

    Ok(())
}
