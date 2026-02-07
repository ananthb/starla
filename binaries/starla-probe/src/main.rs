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
use starla_controller::russh::ChannelMsg;
use starla_controller::{
    ForwardedConnection, InitResponse, ProbeInitInfo, SshConfig, TelnetCommand, TelnetServer,
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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

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
    /// 2. $XDG_CONFIG_HOME/starla/ (if XDG_CONFIG_HOME is set)
    /// 3. ~/.config/starla/
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Skip controller connection (for testing)
    #[arg(long)]
    standalone: bool,
}

/// Bridge a forwarded SSH connection to a local TCP connection
async fn bridge_connection(
    conn: &mut ForwardedConnection,
    mut local: tokio::net::TcpStream,
) -> Result<()> {
    let (mut local_read, mut local_write) = local.split();

    // Buffer for reading from local
    let mut local_buf = [0u8; 4096];

    debug!(
        "Starting connection bridge for {}:{}",
        conn.address, conn.port
    );

    loop {
        tokio::select! {
            // Data from SSH channel -> local
            msg = conn.channel.wait() => {
                trace!("SSH channel message: {:?}", msg.as_ref().map(|m| match m {
                    ChannelMsg::Data { data } => format!("Data({} bytes)", data.len()),
                    ChannelMsg::Eof => "Eof".to_string(),
                    ChannelMsg::ExitStatus { exit_status } => format!("ExitStatus({})", exit_status),
                    other => format!("{:?}", other),
                }));
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        trace!("SSH -> Local: {} bytes", data.len());
                        local_write.write_all(&data).await?;
                        local_write.flush().await?;
                    }
                    Some(ChannelMsg::Eof) | None => {
                        debug!("SSH channel closed");
                        break;
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        debug!("SSH channel exit status: {}", exit_status);
                    }
                    other => {
                        trace!("SSH channel other message: {:?}", other);
                    }
                }
            }
            // Data from local -> SSH channel
            result = local_read.read(&mut local_buf) => {
                match result {
                    Ok(0) => {
                        debug!("Local connection closed");
                        conn.channel.eof().await?;
                        break;
                    }
                    Ok(n) => {
                        trace!("Local -> SSH: {} bytes", n);
                        conn.channel.data(&local_buf[..n]).await?;
                    }
                    Err(e) => {
                        error!("Error reading from local: {}", e);
                        break;
                    }
                }
            }
        }
    }

    debug!("Connection bridge ended");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

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
    debug!("State directory: {}", starla_common::state_dir().display());

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
        "Config: fw={}, telnet={}, http={}, data_dir={}",
        config.probe.firmware_version,
        config.network.telnet_port,
        config.network.http_post_port,
        config.storage.data_dir.display()
    );

    // Ensure state/data directory exists
    if let Err(e) = std::fs::create_dir_all(&config.storage.data_dir) {
        error!(
            "Failed to create data directory {}: {}",
            config.storage.data_dir.display(),
            e
        );
        return Err(e.into());
    }

    // Read probe ID from state directory (written after registration)
    let probe_id = match starla_common::read_probe_id() {
        Some(id) => {
            info!("Probe ID: {}", id);
            starla_common::ProbeId(id)
        }
        None => {
            info!("Probe ID: not yet registered");
            starla_common::ProbeId(0)
        }
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
    let db_path = config.storage.data_dir.join("measurements.db");
    let db = Arc::new(starla_database::Database::connect(&db_path)?);

    // Initialize Results Handler with persistent queue
    let results_db_path = config.storage.data_dir.join("results_queue.db");
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

    // Start Results Upload Loop with cancellation support
    let result_cancel_token = CancellationToken::new();
    let result_handler_loop = result_handler.clone();
    let result_cancel_clone = result_cancel_token.clone();
    tokio::spawn(async move {
        if let Err(e) = result_handler_loop.run(result_cancel_clone).await {
            error!("Result handler failed: {}", e);
        }
    });

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
            let result = match cmd {
                TelnetCommand::Ping(spec) => {
                    let is_recurring = spec.schedule.interval > 0;
                    if is_recurring {
                        debug!(
                            msm_id = spec.msm_id,
                            interval = spec.schedule.interval,
                            "Scheduled ping measurement"
                        );
                        *scheduled_counts.entry("ping").or_insert(0) += 1;
                    } else {
                        debug!(msm_id = spec.msm_id, "One-shot ping");
                    }
                    #[cfg(feature = "metrics-export")]
                    metrics_cmd.record_measurement_started("ping");
                    let job = MeasurementJob {
                        msm_id: spec.msm_id,
                        interval: spec.schedule.interval,
                        start_time: spec.schedule.start_time,
                        end_time: spec.schedule.stop_time,
                        spread: spec.spread.unwrap_or(0) as u64,
                        spec: MeasurementSpec::Ping(PingJobSpec {
                            target: spec.target,
                            af: spec.af,
                            packets: spec.packets,
                            size: spec.size,
                        }),
                    };
                    if is_recurring {
                        scheduler_tx_cmd.send(SchedulerCommand::Schedule(job)).await
                    } else {
                        scheduler_tx_cmd
                            .send(SchedulerCommand::ExecuteNow(job))
                            .await
                    }
                }
                TelnetCommand::Traceroute(spec) => {
                    let is_recurring = spec.schedule.interval > 0;
                    if is_recurring {
                        debug!(
                            msm_id = spec.msm_id,
                            interval = spec.schedule.interval,
                            "Scheduled traceroute measurement"
                        );
                        *scheduled_counts.entry("traceroute").or_insert(0) += 1;
                    } else {
                        debug!(msm_id = spec.msm_id, "One-shot traceroute");
                    }
                    #[cfg(feature = "metrics-export")]
                    metrics_cmd.record_measurement_started("traceroute");
                    let job = MeasurementJob {
                        msm_id: spec.msm_id,
                        interval: spec.schedule.interval,
                        start_time: spec.schedule.start_time,
                        end_time: spec.schedule.stop_time,
                        spread: spec.spread.unwrap_or(0) as u64,
                        spec: MeasurementSpec::Traceroute(TracerouteJobSpec {
                            target: spec.target,
                            af: spec.af,
                            protocol: spec.protocol,
                            paris: spec.paris.unwrap_or(0),
                            first_hop: spec.first_hop,
                            max_hops: spec.max_hops,
                            size: spec.size,
                        }),
                    };
                    if is_recurring {
                        scheduler_tx_cmd.send(SchedulerCommand::Schedule(job)).await
                    } else {
                        scheduler_tx_cmd
                            .send(SchedulerCommand::ExecuteNow(job))
                            .await
                    }
                }
                TelnetCommand::Dns(spec) => {
                    let is_recurring = spec.schedule.interval > 0;
                    if is_recurring {
                        debug!(
                            msm_id = spec.msm_id,
                            interval = spec.schedule.interval,
                            "Scheduled DNS measurement"
                        );
                        *scheduled_counts.entry("dns").or_insert(0) += 1;
                    } else {
                        debug!(msm_id = spec.msm_id, "One-shot DNS");
                    }
                    #[cfg(feature = "metrics-export")]
                    metrics_cmd.record_measurement_started("dns");
                    let job = MeasurementJob {
                        msm_id: spec.msm_id,
                        interval: spec.schedule.interval,
                        start_time: spec.schedule.start_time,
                        end_time: spec.schedule.stop_time,
                        spread: spec.spread.unwrap_or(0) as u64,
                        spec: MeasurementSpec::Dns(DnsJobSpec {
                            target: spec.target,
                            af: spec.af,
                            protocol: spec.protocol,
                            query_type: spec.query_type,
                            query_class: spec.query_class,
                            query_argument: spec.query_argument,
                            use_dnssec: spec.use_dnssec,
                            recursion_desired: spec.recursion_desired,
                        }),
                    };
                    if is_recurring {
                        scheduler_tx_cmd.send(SchedulerCommand::Schedule(job)).await
                    } else {
                        scheduler_tx_cmd
                            .send(SchedulerCommand::ExecuteNow(job))
                            .await
                    }
                }
                TelnetCommand::Http(spec) => {
                    let is_recurring = spec.schedule.interval > 0;
                    if is_recurring {
                        debug!(
                            msm_id = spec.msm_id,
                            interval = spec.schedule.interval,
                            "Scheduled HTTP measurement"
                        );
                        *scheduled_counts.entry("http").or_insert(0) += 1;
                    } else {
                        debug!(msm_id = spec.msm_id, "One-shot HTTP");
                    }
                    #[cfg(feature = "metrics-export")]
                    metrics_cmd.record_measurement_started("http");
                    let job = MeasurementJob {
                        msm_id: spec.msm_id,
                        interval: spec.schedule.interval,
                        start_time: spec.schedule.start_time,
                        end_time: spec.schedule.stop_time,
                        spread: spec.spread.unwrap_or(0) as u64,
                        spec: MeasurementSpec::Http(HttpJobSpec {
                            url: spec.url,
                            method: spec.method,
                            af: spec.af,
                            headers: spec.headers,
                            body: spec.body,
                            max_body_size: spec.max_body_size,
                        }),
                    };
                    if is_recurring {
                        scheduler_tx_cmd.send(SchedulerCommand::Schedule(job)).await
                    } else {
                        scheduler_tx_cmd
                            .send(SchedulerCommand::ExecuteNow(job))
                            .await
                    }
                }
                TelnetCommand::Tls(spec) => {
                    let is_recurring = spec.schedule.interval > 0;
                    if is_recurring {
                        debug!(
                            msm_id = spec.msm_id,
                            interval = spec.schedule.interval,
                            "Scheduled TLS measurement"
                        );
                        *scheduled_counts.entry("tls").or_insert(0) += 1;
                    } else {
                        debug!(msm_id = spec.msm_id, "One-shot TLS");
                    }
                    #[cfg(feature = "metrics-export")]
                    metrics_cmd.record_measurement_started("sslcert");
                    let job = MeasurementJob {
                        msm_id: spec.msm_id,
                        interval: spec.schedule.interval,
                        start_time: spec.schedule.start_time,
                        end_time: spec.schedule.stop_time,
                        spread: spec.spread.unwrap_or(0) as u64,
                        spec: MeasurementSpec::Tls(TlsJobSpec {
                            target: spec.target,
                            port: spec.port,
                            af: spec.af,
                            hostname: spec.hostname,
                        }),
                    };
                    if is_recurring {
                        scheduler_tx_cmd.send(SchedulerCommand::Schedule(job)).await
                    } else {
                        scheduler_tx_cmd
                            .send(SchedulerCommand::ExecuteNow(job))
                            .await
                    }
                }
                TelnetCommand::Ntp(spec) => {
                    let is_recurring = spec.schedule.interval > 0;
                    if is_recurring {
                        debug!(
                            msm_id = spec.msm_id,
                            interval = spec.schedule.interval,
                            "Scheduled NTP measurement"
                        );
                        *scheduled_counts.entry("ntp").or_insert(0) += 1;
                    } else {
                        debug!(msm_id = spec.msm_id, "One-shot NTP");
                    }
                    #[cfg(feature = "metrics-export")]
                    metrics_cmd.record_measurement_started("ntp");
                    let job = MeasurementJob {
                        msm_id: spec.msm_id,
                        interval: spec.schedule.interval,
                        start_time: spec.schedule.start_time,
                        end_time: spec.schedule.stop_time,
                        spread: spec.spread.unwrap_or(0) as u64,
                        spec: MeasurementSpec::Ntp(NtpJobSpec {
                            target: spec.target,
                            af: spec.af,
                            packets: spec.packets,
                        }),
                    };
                    if is_recurring {
                        scheduler_tx_cmd.send(SchedulerCommand::Schedule(job)).await
                    } else {
                        scheduler_tx_cmd
                            .send(SchedulerCommand::ExecuteNow(job))
                            .await
                    }
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

    info!("Probe initialization complete");

    // Controller connection (if not standalone)
    if !args.standalone {
        info!("Connecting to controller...");

        // Load or generate SSH key - use data_dir for key storage
        let key_path = config.storage.data_dir.join("probe_key");
        let key = if key_path.exists() {
            match starla_controller::load_key(&key_path).await {
                Ok(k) => {
                    info!("Loaded SSH key from {}", key_path.display());
                    k
                }
                Err(e) => {
                    warn!("Failed to load SSH key: {}. Generating new key...", e);
                    let new_key = starla_controller::generate_key()?;
                    // Save the new key
                    if let Err(e) = starla_controller::save_key(&new_key, &key_path).await {
                        warn!("Failed to save SSH key: {}", e);
                    } else {
                        info!("Saved new SSH key to {}", key_path.display());
                    }
                    new_key
                }
            }
        } else {
            info!(
                "No SSH key found at {}, generating new key...",
                key_path.display()
            );
            let new_key = starla_controller::generate_key()?;
            // Save the new key
            if let Err(e) = starla_controller::save_key(&new_key, &key_path).await {
                warn!("Failed to save SSH key: {}", e);
            } else {
                info!("Saved new SSH key to {}", key_path.display());
                // Also print the public key for registration
                let pub_key_path = config.storage.data_dir.join("probe_key.pub");
                info!("Public key saved to: {}", pub_key_path.display());
                info!("Register your probe at: https://atlas.ripe.net/apply/swprobe/");
                info!(
                    "After registration, save your probe ID to: {}",
                    starla_common::probe_id_path().display()
                );
            }
            new_key
        };

        // Try to connect to registration servers
        let ssh_config = SshConfig::default();
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
        )
        .await
        {
            Ok(reg_ssh) => {
                info!("Connected to registration server");

                // Step 1: Registration server INIT - send probe info, get controller assignment
                match reg_ssh.init(Some(&probe_info)).await {
                    Ok(InitResponse::Controller(controller_info)) => {
                        info!(
                            "Got controller assignment: {}:{}",
                            controller_info.host, controller_info.port
                        );

                        // Step 2: Connect to the assigned controller
                        let controller_addr =
                            format!("{}:{}", controller_info.host, controller_info.port);
                        info!("Connecting to controller at {}", controller_addr);

                        match starla_controller::SshConnection::connect(
                            &controller_info.host,
                            controller_info.port,
                            &key,
                            ssh_config,
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
                                            info!(
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
                                                    info!("Retrying controller INIT...");
                                                    continue;
                                                }
                                            }
                                        }
                                        Ok(InitResponse::Ok) => {
                                            // Controller said OK but no REMOTE_PORT - retry after
                                            // delay
                                            info!(
                                                "Controller said OK but no REMOTE_PORT, retrying \
                                                 in 30 seconds"
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
                                                    info!("Retrying controller INIT...");
                                                    continue;
                                                }
                                            }
                                        }
                                        Ok(InitResponse::Controller(_)) => {
                                            // This shouldn't happen from a controller
                                            error!(
                                                "Got CONTROLLER response from controller \
                                                 (expected REMOTE_PORT)"
                                            );
                                            return Ok(());
                                        }
                                        Err(e) => {
                                            // Connection may have dropped during the wait period
                                            // Exit so the probe can be restarted for a fresh
                                            // connection
                                            error!(
                                                "Controller INIT failed: {} (connection may have \
                                                 timed out during wait)",
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
                                        "Probe ID is 0 (not yet registered). Results may not \
                                         upload correctly."
                                    );
                                    warn!(
                                        "Register your probe at https://atlas.ripe.net/apply/swprobe/"
                                    );
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
                                info!("Result upload endpoint: {}", result_endpoint);
                                result_handler.set_endpoint(result_endpoint).await;

                                // Set session ID for upload body footer (per httppost --post-footer
                                // behavior)
                                result_handler.set_session_id(session_id.clone()).await;

                                // Step 4: Connection loop with automatic reconnection
                                let mut reconnect_delay = Duration::from_secs(5);
                                let max_reconnect_delay = Duration::from_secs(300);
                                let mut connection_attempt = 0u32;

                                'connection_loop: loop {
                                    connection_attempt += 1;

                                    // Create a NEW connection for KEEP with reverse tunnel
                                    info!(
                                        "Creating connection for KEEP session (attempt {})",
                                        connection_attempt
                                    );
                                    let mut keep_ssh =
                                        match starla_controller::SshConnection::connect(
                                            &controller_info.host,
                                            controller_info.port,
                                            &key,
                                            SshConfig::default(),
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
                                                warn!(
                                                    "Retrying connection in {:?}...",
                                                    reconnect_delay
                                                );
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
                                    if let Err(e) =
                                        keep_ssh.request_reverse_tunnel(remote_port).await
                                    {
                                        error!("Failed to setup reverse tunnel: {}", e);
                                        warn!("Retrying connection in {:?}...", reconnect_delay);
                                        tokio::time::sleep(reconnect_delay).await;
                                        reconnect_delay =
                                            std::cmp::min(reconnect_delay * 2, max_reconnect_delay);
                                        continue 'connection_loop;
                                    }
                                    info!(
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

                                    // Start the KEEP session to tell controller we're ready
                                    if let Err(e) = keep_ssh.start_keep_session().await {
                                        error!("Failed to start KEEP session: {}", e);
                                        warn!("Retrying connection in {:?}...", reconnect_delay);
                                        tokio::time::sleep(reconnect_delay).await;
                                        reconnect_delay =
                                            std::cmp::min(reconnect_delay * 2, max_reconnect_delay);
                                        continue 'connection_loop;
                                    }

                                    info!("Controller connection established successfully");

                                    // Take the forwarded connection receiver to handle in separate
                                    // task
                                    let forward_rx = keep_ssh.take_forward_receiver();

                                    // Spawn task to handle forwarded connections
                                    let local_telnet_port = telnet_port;
                                    let forward_task = tokio::spawn(async move {
                                        debug!("Forward handler task started");
                                        if let Some(mut rx) = forward_rx {
                                            while let Some(mut conn) = rx.recv().await {
                                                debug!(
                                                    "Handling forwarded connection to {}:{}",
                                                    conn.address, conn.port
                                                );

                                                let local_addr =
                                                    format!("127.0.0.1:{}", local_telnet_port);
                                                match tokio::net::TcpStream::connect(&local_addr)
                                                    .await
                                                {
                                                    Ok(local_stream) => {
                                                        tokio::spawn(async move {
                                                            if let Err(e) = bridge_connection(
                                                                &mut conn,
                                                                local_stream,
                                                            )
                                                            .await
                                                            {
                                                                debug!(
                                                                    "Connection bridge ended: {}",
                                                                    e
                                                                );
                                                            }
                                                        });
                                                    }
                                                    Err(e) => {
                                                        error!(
                                                            "Failed to connect to local telnet: {}",
                                                            e
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    });

                                    // Start keepalive loop
                                    let keepalive_task = tokio::spawn(async move {
                                        if let Err(e) =
                                            keep_ssh.keepalive_loop(Duration::from_secs(60)).await
                                        {
                                            error!("Keepalive loop failed: {}", e);
                                        }
                                    });

                                    // Wait for shutdown or connection loss
                                    let should_reconnect = tokio::select! {
                                        _ = tokio::signal::ctrl_c() => {
                                            info!("Received Ctrl+C");
                                            false // Don't reconnect, shutdown
                                        }
                                        _ = &mut scheduler_task => {
                                            error!("Scheduler task ended unexpectedly");
                                            false // Don't reconnect
                                        }
                                        _ = keepalive_task => {
                                            warn!("Connection to controller lost (keepalive failed), will reconnect...");
                                            true // Reconnect
                                        }
                                        _ = forward_task => {
                                            warn!("Forward handler ended, will reconnect...");
                                            true // Reconnect
                                        }
                                        _ = proxy_reconnect_signal.cancelled() => {
                                            warn!("HTTP proxy detected dead SSH session, will reconnect...");
                                            true // Reconnect
                                        }
                                    };

                                    if !should_reconnect {
                                        break 'connection_loop;
                                    }

                                    // Wait before reconnecting
                                    info!("Reconnecting in {:?}...", reconnect_delay);
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
                    Ok(InitResponse::Ok) | Ok(InitResponse::Wait { .. }) => {
                        // Probe is not yet fully registered
                        info!("Probe not yet fully registered.");
                        info!("Register at: https://atlas.ripe.net/apply/swprobe/");
                        info!(
                            "Public key: {}",
                            config.storage.data_dir.join("probe_key.pub").display()
                        );
                        info!(
                            "After registration, save your probe ID to: {}",
                            starla_common::probe_id_path().display()
                        );

                        // Retry loop - check registration status periodically
                        let retry_interval = Duration::from_secs(60);
                        loop {
                            tokio::select! {
                                _ = tokio::signal::ctrl_c() => {
                                    info!("Received Ctrl+C");
                                    metrics_cancel_token.cancel();
                                    result_cancel_token.cancel();
                                    break;
                                }
                                _ = tokio::time::sleep(retry_interval) => {
                                    info!("Retrying INIT...");
                                    match reg_ssh.init(Some(&probe_info)).await {
                                        Ok(InitResponse::Controller(controller_info)) => {
                                            info!(
                                                "Got controller: {}:{}",
                                                controller_info.host, controller_info.port
                                            );
                                            // TODO: Connect to controller and complete setup
                                            // For now, just log and exit
                                            info!("Please restart the probe to connect to controller");
                                            break;
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
                    }
                    Err(e) => {
                        error!("Registration failed: {}", e);
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
