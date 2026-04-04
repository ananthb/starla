{ config, lib, pkgs, ... }:

let
  cfg = config.services.starla;
  settingsFormat = pkgs.formats.toml { };

  configFile = settingsFormat.generate "config.toml" (
    lib.recursiveUpdate
      {
        probe.log_level = cfg.logLevel;
        network = {
          telnet_port = cfg.telnetPort;
          http_post_port = cfg.httpPostPort;
        };
        controller.registration_servers = cfg.registrationServers;
      }
      cfg.settings
  );
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

    telnetPort = lib.mkOption {
      type = lib.types.port;
      default = 2023;
      description = "Port for receiving measurement commands from the controller.";
    };

    httpPostPort = lib.mkOption {
      type = lib.types.port;
      default = 8080;
      description = "Local port for uploading results via the SSH tunnel.";
    };

    registrationServers = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [
        "reg03.atlas.ripe.net:443"
        "reg04.atlas.ripe.net:443"
      ];
      description = "RIPE Atlas registration servers.";
    };

    settings = lib.mkOption {
      type = settingsFormat.type;
      default = { };
      description = ''
        Additional settings merged into config.toml.
        See config.toml.example for available options.
      '';
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
