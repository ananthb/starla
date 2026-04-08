{ config, lib, pkgs, ... }:

let
  cfg = config.services.starla;
  settingsFormat = pkgs.formats.toml { };

  configFile = settingsFormat.generate "config.toml" {
    probe.log_level = cfg.logLevel;
    network = {
      rxtxrpt = cfg.reportInterfaceStats;
      status_socket = cfg.tray.enable;
    };
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
    enable = lib.mkEnableOption "Starla RIPE Atlas software probe (user service)";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The starla package to use.";
    };

    trayPackage = lib.mkOption {
      type = lib.types.package;
      description = "The starla-tray package to use.";
    };

    logLevel = lib.mkOption {
      type = lib.types.enum [ "trace" "debug" "info" "warn" "error" ];
      default = "info";
      description = "Log verbosity level.";
    };

    reportInterfaceStats = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Report network interface traffic statistics.";
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

    tray = {
      enable = lib.mkEnableOption "Starla tray icon (desktop only)";
    };
  };

  config = lib.mkIf cfg.enable {
    # Config file
    xdg.configFile."starla/config.toml".source = configFile;

    # Probe daemon as user service
    systemd.user.services.starla = {
      Unit = {
        Description = "Starla RIPE Atlas Software Probe";
        After = [ "default.target" ];
      };

      Service = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/starla --config %h/.config/starla/config.toml";
        Restart = "on-failure";
        RestartSec = 10;
      };

      Install = {
        WantedBy = [ "default.target" ];
      };
    };

    # Tray app as user service (desktop only)
    systemd.user.services.starla-tray = lib.mkIf cfg.tray.enable {
      Unit = {
        Description = "Starla System Tray";
        After = [ "graphical-session-pre.target" ];
        PartOf = [ "graphical-session.target" ];
      };

      Service = {
        Type = "simple";
        ExecStart = "${cfg.trayPackage}/bin/starla-tray";
        Restart = "on-failure";
        RestartSec = 5;
      };

      Install = {
        WantedBy = [ "graphical-session.target" ];
      };
    };

    # Autostart desktop entry for tray
    xdg.configFile."autostart/starla-tray.desktop" = lib.mkIf cfg.tray.enable {
      text = ''
        [Desktop Entry]
        Type=Application
        Name=Starla Tray
        Comment=RIPE Atlas probe status
        Exec=${cfg.trayPackage}/bin/starla-tray
        Icon=starla
        Categories=System;Monitor;
        StartupNotify=false
        X-GNOME-Autostart-enabled=true
      '';
    };
  };
}
