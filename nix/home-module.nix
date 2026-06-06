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

    sshKeyFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        Path to a file containing the SSH private key (OpenSSH PEM format).
        When set, starla reads the key from this file via the STARLA_SSH_KEY
        environment variable instead of generating one in the state directory.
        Use this to inject keys from password-store, sops, etc.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # Config file
    xdg.configFile."starla/config.toml".source = configFile;

    # --- Linux: systemd user services ---

    # Probe daemon as user service
    systemd.user.services.starla = lib.mkIf pkgs.stdenv.isLinux {
      Unit = {
        Description = "Starla RIPE Atlas Software Probe";
        After = [ "default.target" ];
      };

      Service = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/starla --config %h/.config/starla/config.toml";
        Restart = "on-failure";
        RestartSec = 10;
      } // lib.optionalAttrs (cfg.sshKeyFile != null) {
        Environment = [ "STARLA_SSH_KEY=${cfg.sshKeyFile}" ];
      };

      Install = {
        WantedBy = [ "default.target" ];
      };
    };

    # Tray app as user service (desktop only)
    systemd.user.services.starla-tray = lib.mkIf (cfg.tray.enable && pkgs.stdenv.isLinux) {
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

    # Autostart desktop entry for tray (Linux)
    xdg.configFile."autostart/starla-tray.desktop" = lib.mkIf (cfg.tray.enable && pkgs.stdenv.isLinux) {
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

    # --- macOS: launchd agents ---

    launchd.agents.starla = lib.mkIf pkgs.stdenv.isDarwin {
      enable = true;
      config = {
        Label = "com.ananthb.starla";
        ProgramArguments = [
          "${cfg.package}/bin/starla"
          "--config"
          "${config.home.homeDirectory}/.config/starla/config.toml"
        ];
        RunAtLoad = true;
        KeepAlive = { SuccessfulExit = false; };
        ThrottleInterval = 10;
        StandardErrorPath = "${config.home.homeDirectory}/Library/Logs/starla.log";
        StandardOutPath = "${config.home.homeDirectory}/Library/Logs/starla.log";
        # launchd's per-user default is 256, which the scheduler bursts
        # past under load (one fd per outbound probe). Raise both limits
        # so measurements don't fail with EMFILE and the status socket
        # accept loop doesn't have to back off.
        SoftResourceLimits = { NumberOfFiles = 4096; };
        HardResourceLimits = { NumberOfFiles = 8192; };
      } // lib.optionalAttrs (cfg.sshKeyFile != null) {
        EnvironmentVariables = {
          STARLA_SSH_KEY = toString cfg.sshKeyFile;
        };
      };
    };

    launchd.agents.starla-tray = lib.mkIf (cfg.tray.enable && pkgs.stdenv.isDarwin) {
      enable = true;
      config = {
        Label = "com.ananthb.starla-tray";
        ProgramArguments = [
          "${config.home.homeDirectory}/Applications/Starla Tray.app/Contents/MacOS/starla-tray"
        ];
        RunAtLoad = true;
        KeepAlive = { SuccessfulExit = false; };
        ProcessType = "Interactive";
      };
    };

    # Copy .app bundle into ~/Applications on activation (macOS).
    # home.file with recursive=true creates per-file symlinks which macOS
    # does not recognise as a valid bundle. Copy the whole .app instead.
    home.activation.starla-tray-app = lib.mkIf (cfg.tray.enable && pkgs.stdenv.isDarwin)
      (lib.hm.dag.entryAfter [ "writeBoundary" ] ''
        app_src="${cfg.trayPackage}/Applications/Starla Tray.app"
        app_dst="$HOME/Applications/Starla Tray.app"
        if [ -d "$app_src" ]; then
          $DRY_RUN_CMD rm -rf "$app_dst"
          $DRY_RUN_CMD cp -RL "$app_src" "$app_dst"
          $DRY_RUN_CMD chmod -R u+w "$app_dst"
          $DRY_RUN_CMD xattr -dr com.apple.quarantine "$app_dst" 2>/dev/null || true
        fi
      '');
  };
}
