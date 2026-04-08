{ config, lib, pkgs, ... }:

let
  cfg = config.services.starla;
  settingsFormat = pkgs.formats.toml { };

  configFile = settingsFormat.generate "config.toml" {
    probe.log_level = cfg.logLevel;
    controller = {
      registration_servers = cfg.controller.registrationServers;
      ssh_timeout = cfg.controller.sshTimeout;
      keepalive_interval = cfg.controller.keepaliveInterval;
    };
    storage = {
      max_queue_size = cfg.storage.maxQueueSize;
      retention_days = cfg.storage.retentionDays;
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

    controller = {
      registrationServers = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [
          "reg03.atlas.ripe.net:443"
          "reg04.atlas.ripe.net:443"
        ];
        description = "RIPE Atlas registration servers.";
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
      maxQueueSize = lib.mkOption {
        type = lib.types.ints.positive;
        default = 10000;
        description = "Maximum number of results in the upload queue.";
      };

      retentionDays = lib.mkOption {
        type = lib.types.ints.positive;
        default = 30;
        description = "Result retention period in days.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    environment.etc."starla/config.toml".source = configFile;

    launchd.daemons.starla = {
      serviceConfig = {
        Label = "com.ananthb.starla";
        ProgramArguments = [
          "${cfg.package}/bin/starla"
          "--config"
          "/etc/starla/config.toml"
        ];
        RunAtLoad = true;
        KeepAlive = true;
        ThrottleInterval = 10;
        StandardErrorPath = "/var/log/starla.log";
        StandardOutPath = "/var/log/starla.log";
      };
    };
  };
}
