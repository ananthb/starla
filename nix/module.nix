{ config, lib, pkgs, ... }:

let
  cfg = config.services.starla;
  settingsFormat = pkgs.formats.toml { };

  configFile = settingsFormat.generate "config.toml" {
    probe.log_level = cfg.logLevel;
    network.rxtxrpt = cfg.reportInterfaceStats;
    controller = {
      registration_servers = cfg.controller.registrationServers;
      ssh_timeout = cfg.controller.sshTimeout;
      keepalive_interval = cfg.controller.keepaliveInterval;
    };
    storage = {
      max_queue_size_mb = cfg.storage.maxQueueSizeMB;
      retention_days = cfg.storage.retentionDays;
      max_database_size_mb = cfg.storage.maxDatabaseSizeMB;
      cleanup_interval_hours = cfg.storage.cleanupIntervalHours;
    };
    metrics = {
      enabled = cfg.metrics.enable;
      listen_addr = cfg.metrics.listenAddr;
    };
    logging = {
      format = cfg.logging.format;
      output = cfg.logging.output;
      max_file_size_mb = cfg.logging.maxFileSizeMB;
      max_files = cfg.logging.maxFiles;
    };
  };
in
{
  options.services.starla = {
    enable = lib.mkEnableOption "Starla RIPE Atlas software probe";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The starla package to use.";
    };

    logLevel = lib.mkOption {
      type = lib.types.enum [ "trace" "debug" "info" "warn" "error" ];
      default = "info";
      description = "Log verbosity level.";
    };

    reportInterfaceStats = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Report network interface traffic statistics (rx/tx bytes, packets, errors) to the RIPE Atlas controller.";
    };

    controller = {
      registrationServers = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [
          "reg03.atlas.ripe.net:443"
          "reg04.atlas.ripe.net:443"
        ];
        description = "RIPE Atlas registration servers (host:port).";
      };

      sshTimeout = lib.mkOption {
        type = lib.types.ints.positive;
        default = 30;
        description = "SSH connection timeout in seconds.";
      };

      keepaliveInterval = lib.mkOption {
        type = lib.types.ints.positive;
        default = 60;
        description = "SSH keepalive interval in seconds.";
      };
    };

    storage = {
      maxQueueSizeMB = lib.mkOption {
        type = lib.types.ints.positive;
        default = 100;
        description = "Maximum result queue size in MB.";
      };

      retentionDays = lib.mkOption {
        type = lib.types.ints.positive;
        default = 30;
        description = "Result retention period in days.";
      };

      maxDatabaseSizeMB = lib.mkOption {
        type = lib.types.ints.positive;
        default = 1;
        description = "Maximum measurement database size in MB.";
      };

      cleanupIntervalHours = lib.mkOption {
        type = lib.types.ints.positive;
        default = 24;
        description = "Database cleanup interval in hours.";
      };
    };

    metrics = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Enable Prometheus metrics export.";
      };

      listenAddr = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1:9090";
        description = "Metrics server listen address.";
      };
    };

    logging = {
      format = lib.mkOption {
        type = lib.types.enum [ "json" "text" ];
        default = "json";
        description = "Log output format.";
      };

      output = lib.mkOption {
        type = lib.types.enum [ "stdout" "file" "syslog" ];
        default = "stdout";
        description = "Log output destination.";
      };

      maxFileSizeMB = lib.mkOption {
        type = lib.types.ints.positive;
        default = 10;
        description = "Maximum log file size in MB (when output=file).";
      };

      maxFiles = lib.mkOption {
        type = lib.types.ints.positive;
        default = 5;
        description = "Maximum number of rotated log files.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.starla = {
      description = "Starla RIPE Atlas Software Probe";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/starla --config %E/starla/config.toml";
        Restart = "on-failure";
        RestartSec = 10;

        DynamicUser = true;

        # Directories — systemd sets CONFIGURATION_DIRECTORY, STATE_DIRECTORY,
        # RUNTIME_DIRECTORY which starla's path resolution picks up automatically
        ConfigurationDirectory = "starla";
        ConfigurationDirectoryMode = "0750";
        StateDirectory = "starla";
        StateDirectoryMode = "0750";
        RuntimeDirectory = "starla";
        RuntimeDirectoryMode = "0750";

        # Raw sockets for ping and traceroute
        AmbientCapabilities = [ "CAP_NET_RAW" ];
        CapabilityBoundingSet = [ "CAP_NET_RAW" ];

        # Hardening
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        ProtectControlGroups = true;
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        NoNewPrivileges = true;
        SystemCallArchitectures = "native";
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" "AF_NETLINK" ];
        SystemCallFilter = [ "@system-service" "~@privileged" "@network-io" "@raw-io" ];
      };
    };

    # Install the config file into the configuration directory
    environment.etc."starla/config.toml".source = configFile;
  };

  meta.maintainers = [ ];
}
