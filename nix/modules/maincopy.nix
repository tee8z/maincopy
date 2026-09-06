{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    types
    ;
  cfg = config.services.maincopy;
  safeName =
    value: builtins.stringLength value <= 80 && builtins.match "[a-z0-9][a-z0-9_-]*" value != null;
  safePath =
    value:
    builtins.match "/[A-Za-z0-9/._-]+" value != null
    && !(lib.hasInfix "/../" value)
    && !(lib.hasSuffix "/.." value)
    && !(lib.hasInfix "//" value);
  ipv4Octet = "([0-9]|[1-9][0-9]|1[0-9][0-9]|2[0-4][0-9]|25[0-5])";
  ipv4Address =
    value: builtins.match "${ipv4Octet}\\.${ipv4Octet}\\.${ipv4Octet}\\.${ipv4Octet}" value != null;
  addressType = types.addCheck types.str (
    value: ipv4Address value || builtins.match "[[][0-9a-fA-F:]+[]]" value != null
  );
  protectedPath =
    value: safePath value && value != "/nix/store" && !(lib.hasPrefix "/nix/store/" value);
  domainType = types.addCheck types.str (
    value: builtins.match "[a-z0-9][a-z0-9.-]*[a-z0-9]" value != null && !(lib.hasInfix ".." value)
  );
  pathType = types.addCheck types.str protectedPath;
  privateAddress =
    address:
    address == "127.0.0.1"
    || address == "::1"
    || address == "[::1]"
    || builtins.match "10\\.[0-9]+\\.[0-9]+\\.[0-9]+" address != null
    || builtins.match "192\\.168\\.[0-9]+\\.[0-9]+" address != null
    || builtins.match "172\\.(1[6-9]|2[0-9]|3[01])\\.[0-9]+\\.[0-9]+" address != null
    || builtins.match "100\\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\\.[0-9]+\\.[0-9]+" address != null
    || builtins.match "[[]f[cd][0-9a-f:]+[]]" address != null;
  stateDir = "/var/lib/${cfg.stateDirectory}";
  runtimeParent = "/run/${cfg.runtimeDirectory}";
  runtimeDir = "${runtimeParent}/private";
  origin = "https://${cfg.admin.domain}${
    lib.optionalString (cfg.admin.port != 443) ":${toString cfg.admin.port}"
  }";
  sourceCredentials = lib.mapAttrs (name: _: {
    private_key_file = "${runtimeDir}/credentials/${name}-key";
    known_hosts_file = "${runtimeDir}/credentials/${name}-hosts";
  }) cfg.source.credentials;
  hostConfig = (pkgs.formats.toml { }).generate "maincopy.toml" (
    {
      paths = {
        content_root = cfg.contentRoot;
        state_root = stateDir;
        runtime_root = runtimeDir;
      };
      public.bind = "${cfg.public.bind}:${toString cfg.public.backendPort}";
      admin = {
        bind = "127.0.0.1:${toString cfg.admin.backendPort}";
        inherit origin;
      };
      metrics.bind = "127.0.0.1:${toString cfg.metricsPort}";
      database.path = "${stateDir}/database/maincopy.db";
      identity.startup_bootstrap = "require_existing";
    }
    // lib.optionalAttrs cfg.backup.enable {
      backup = {
        status_file = "/var/lib/maincopy-backup-status/backup-status.json";
        stale_after_seconds = cfg.backup.staleAfterSeconds;
      };
    }
    // lib.optionalAttrs cfg.source.managed {
      source = {
        mode = "managed_git";
        mirror_root = "${stateDir}/source-mirror";
        ssh_credentials = sourceCredentials;
      };
    }
  );
  credentials = lib.concatLists (
    lib.mapAttrsToList (name: files: [
      "${name}-key:${files.privateKeyFile}"
      "${name}-hosts:${files.knownHostsFile}"
    ]) cfg.source.credentials
  );
  prepareCredentials = pkgs.writeShellScript "maincopy-prepare-credentials" ''
    set -eu
    umask 077
    ${pkgs.coreutils}/bin/install -d -m 0700 ${lib.escapeShellArg "${stateDir}/database"}
    ${pkgs.coreutils}/bin/install -d -m 0700 ${lib.escapeShellArg "${runtimeDir}/credentials"}
    ${lib.concatStringsSep "\n" (
      lib.mapAttrsToList (name: _: ''
        ${pkgs.coreutils}/bin/install -m 0600 "$CREDENTIALS_DIRECTORY/${name}-key" ${lib.escapeShellArg "${runtimeDir}/credentials/${name}-key"}
        ${pkgs.coreutils}/bin/install -m 0600 "$CREDENTIALS_DIRECTORY/${name}-hosts" ${lib.escapeShellArg "${runtimeDir}/credentials/${name}-hosts"}
      '') cfg.source.credentials
    )}
  '';
  serviceIsolation = import ./service-isolation.nix;
  daemonService = serviceIsolation // {
    # systemd 261 RestrictSUIDSGID returns ENOSYS for every openat2 call.
    # Maincopy requires that resolver to enforce its content path boundary.
    # NoNewPrivileges, empty capabilities and private writable roots remain.
    RestrictSUIDSGID = false;
    User = "maincopy";
    Group = "maincopy";
    StateDirectory = cfg.stateDirectory;
    StateDirectoryMode = "0700";
    RuntimeDirectory = "${cfg.runtimeDirectory}/private";
    RuntimeDirectoryMode = "0700";
    WorkingDirectory = stateDir;
    ReadWritePaths = [
      stateDir
      runtimeDir
    ];
    ReadOnlyPaths = lib.optional (!cfg.source.managed) cfg.contentRoot;
    LoadCredential = credentials;
    ExecStartPre = prepareCredentials;
    ExecStart = "${cfg.package}/bin/maincopyd --config ${hostConfig}";
    Restart = "on-failure";
    RestartSec = "5s";
    TimeoutStopSec = "180s";
    KillMode = "mixed";
  };
  tlsOptions = { ... }: {
    options = {
      mode = mkOption {
        type = types.enum [
          "internal"
          "automatic"
          "provided"
        ];
        default = "internal";
        description = "Caddy certificate source. Provided private keys enter only through systemd credentials.";
      };
      certificateFile = mkOption {
        type = types.nullOr pathType;
        default = null;
        description = "Runtime certificate file for provided TLS.";
      };
      privateKeyFile = mkOption {
        type = types.nullOr pathType;
        default = null;
        description = "Protected runtime private-key file for provided TLS.";
      };
    };
  };
  tlsDirective =
    name: tls:
    if tls.mode == "internal" then
      "tls internal"
    else
      lib.optionalString (tls.mode == "provided")
        ''tls "/run/credentials/maincopy-gateway.service/${name}-certificate" "/run/credentials/maincopy-gateway.service/${name}-key"'';
  tlsCredentials =
    name: tls:
    lib.optionals (tls.mode == "provided") [
      "${name}-certificate:${tls.certificateFile}"
      "${name}-key:${tls.privateKeyFile}"
    ];
  stripHeaders = ''
    header_up -Forwarded
    header_up -Via
    header_up -X-Real-IP
    header_up -X-Forwarded-*
    header_up -X-Maincopy-Actor
    header_up -X-Maincopy-Role
    header_up -X-Maincopy-Scope
    header_up -X-Maincopy-User
    header_up -X-Maincopy-User-ID
    header_up -Remote-User
    header_up -Remote-Groups
    header_up -X-Auth-Request-*
  '';
  upstream = address: port: ''
    reverse_proxy ${address}:${toString port} {
      lb_retries 0
      lb_try_duration 0s
      ${stripHeaders}
      transport http {
        compression off
        keepalive off
        dial_timeout 2s
        response_header_timeout 30s
      }
    }
  '';
  gatewayConfig = pkgs.writeText "maincopy-Caddyfile" ''
    {
      admin off
      persist_config off
      skip_install_trust
      auto_https disable_redirects
      servers {
        protocols h1 h2
        strict_sni_host on
        0rtt off
        max_header_size 16KiB
        timeouts {
          read_header 10s
          read_body 15s
          write 60s
          idle 60s
        }
      }
    }
    https://${cfg.public.domain}:${toString cfg.public.port} {
      bind ${lib.concatStringsSep " " cfg.public.listenAddresses}
      ${tlsDirective "public" cfg.public.tls}
      @private_routes path /admin /admin/* /api/admin /api/admin/* /metrics /metrics/*
      respond @private_routes 404
      ${upstream (
        if cfg.public.bind == "0.0.0.0" then "127.0.0.1" else cfg.public.bind
      ) cfg.public.backendPort}
    }
    ${origin} {
      bind ${lib.concatStringsSep " " cfg.admin.listenAddresses}
      ${tlsDirective "admin" cfg.admin.tls}
      @metrics_routes path /metrics /metrics/*
      respond @metrics_routes 404
      ${upstream "127.0.0.1" cfg.admin.backendPort}
    }
  '';
in
{
  imports = [ ./backup.nix ];
  options.services.maincopy = {
    enable = mkEnableOption "Maincopy publication, private administration gateway, and operational services";
    package = mkOption {
      type = types.package;
      description = "Package containing maincopyd, maincopy, and maincopy-mermaid.";
    };
    stateDirectory = mkOption {
      type = types.addCheck types.str safeName;
      default = "maincopy";
      description = "Dedicated directory name under /var/lib.";
    };
    runtimeDirectory = mkOption {
      type = types.addCheck types.str safeName;
      default = "maincopy";
      description = "Stable dedicated parent under /run; systemd creates and removes its private child for each service run.";
    };
    contentRoot = mkOption {
      type = types.addCheck types.str safePath;
      default = "${stateDir}/content";
      description = "Read-only external content checkout, unused in managed-source mode.";
    };
    initialOwnerPublicKey = mkOption {
      type = types.nullOr (types.addCheck types.str (value: builtins.match "[0-9a-f]{64}" value != null));
      default = null;
      description = "Public Nostr key for the explicitly invoked offline maincopy-initialize service. Ordinary startup always requires an existing identity.";
    };
    metricsPort = mkOption {
      type = types.port;
      default = 3002;
      description = "Loopback-only Prometheus listener port.";
    };
    public = {
      domain = mkOption {
        type = domainType;
        description = "Public HTTPS hostname matching publication.toml.";
      };
      bind = mkOption {
        type = types.addCheck types.str ipv4Address;
        default = "127.0.0.1";
        description = "Public backend IPv4 bind address; validated by maincopyd.";
      };
      backendPort = mkOption {
        type = types.port;
        default = 3000;
        description = "Public backend HTTP port.";
      };
      port = mkOption {
        type = types.port;
        default = 443;
        description = "Public gateway HTTPS port.";
      };
      listenAddresses = mkOption {
        type = types.listOf addressType;
        default = [
          "0.0.0.0"
          "[::]"
        ];
        description = "Public gateway bind addresses.";
      };
      tls = mkOption {
        type = types.submodule tlsOptions;
        default = {
          mode = "automatic";
        };
      };
    };
    admin = {
      domain = mkOption {
        type = domainType;
        default = "admin.localhost";
        description = "Canonical private HTTPS administration hostname.";
      };
      backendPort = mkOption {
        type = types.port;
        default = 3001;
        description = "Loopback-only admin backend port.";
      };
      port = mkOption {
        type = types.port;
        default = 8443;
        description = "Private administration gateway HTTPS port.";
      };
      listenAddresses = mkOption {
        type = types.listOf addressType;
        default = [
          "127.0.0.1"
          "[::1]"
        ];
        description = "Private or loopback administration addresses.";
      };
      allowInternet = mkOption {
        type = types.bool;
        default = false;
        description = "Explicitly permit administration binds outside loopback or private address ranges.";
      };
      tls = mkOption {
        type = types.submodule tlsOptions;
        default = { };
      };
    };
    source = {
      managed = mkOption {
        type = types.bool;
        default = false;
        description = "Use the daemon-managed SSH Git source instead of an external checkout.";
      };
      credentials = mkOption {
        default = { };
        description = "Named SSH file references; private bytes never enter the Nix store.";
        type = types.attrsOf (
          types.submodule {
            options = {
              privateKeyFile = mkOption {
                type = pathType;
                description = "Protected host SSH private-key file.";
              };
              knownHostsFile = mkOption {
                type = pathType;
                description = "Protected host known_hosts trust anchor.";
              };
            };
          }
        );
      };
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion =
          !(lib.elem cfg.stateDirectory [
            "maincopy-gateway"
            "maincopy-backup"
            "maincopy-litestream"
            "maincopy-backup-status"
          ])
          && !(lib.elem cfg.runtimeDirectory [
            "maincopy-gateway"
            "maincopy-backup"
            "maincopy-litestream"
            "maincopy-backup-status"
          ]);
        message = "Maincopy daemon directories must not overlap gateway or backup service directories.";
      }
      {
        assertion = cfg.public.domain != cfg.admin.domain;
        message = "Maincopy public and admin hostnames must be distinct.";
      }
      {
        assertion = cfg.admin.allowInternet || lib.all privateAddress cfg.admin.listenAddresses;
        message = "Maincopy administration binds must be private unless admin.allowInternet is explicitly enabled.";
      }
      {
        assertion = cfg.public.port != cfg.admin.port;
        message = "Maincopy public and private administration gateway ports must be distinct.";
      }
      {
        assertion = cfg.public.listenAddresses != [ ] && cfg.admin.listenAddresses != [ ];
        message = "Maincopy gateway bind address lists must not be empty.";
      }
      {
        assertion =
          lib.length (
            lib.unique [
              cfg.public.backendPort
              cfg.admin.backendPort
              cfg.metricsPort
            ]
          ) == 3;
        message = "Maincopy public, admin, and metrics backend ports must be distinct.";
      }
      {
        assertion = lib.all safeName (lib.attrNames cfg.source.credentials);
        message = "Maincopy SSH credential names must be lowercase portable names.";
      }
      {
        assertion = cfg.source.managed || cfg.source.credentials == { };
        message = "Maincopy SSH credential references require managed source mode.";
      }
    ]
    ++
      map
        (tls: {
          assertion =
            if tls.mode == "provided" then
              tls.certificateFile != null && tls.privateKeyFile != null
            else
              tls.certificateFile == null && tls.privateKeyFile == null;
          message = "Provided Maincopy TLS requires both protected runtime certificate and private-key files.";
        })
        [
          cfg.public.tls
          cfg.admin.tls
        ];
    users.groups.maincopy = { };
    users.users.maincopy = {
      isSystemUser = true;
      group = "maincopy";
      home = stateDir;
    };
    users.groups.maincopy-gateway = { };
    users.users.maincopy-gateway = {
      isSystemUser = true;
      group = "maincopy-gateway";
      home = "/var/lib/maincopy-gateway";
    };
    environment.systemPackages = [
      cfg.package
      pkgs.caddy
    ];
    environment.etc."maincopy/maincopy.toml".source = hostConfig;
    environment.etc."maincopy/Caddyfile".source = gatewayConfig;
    # Keep the parent stable while systemd removes each stopped unit's child.
    # Peer namespaces mask the parent, so cleanup cannot detach their masks.
    systemd.tmpfiles.rules = [ "d ${runtimeParent} 0700 maincopy maincopy -" ];
    systemd.services.maincopy = {
      description = "Maincopy publication and administration";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      unitConfig = {
        StartLimitIntervalSec = 60;
        StartLimitBurst = 3;
      };
      serviceConfig = daemonService;
    };
    systemd.services.maincopy-initialize = mkIf (cfg.initialOwnerPublicKey != null) {
      description = "Explicit offline Maincopy owner initialization";
      conflicts = [ "maincopy.service" ];
      # Conflicts alone does not order stop/start jobs. Finish draining the
      # database writer before the offline bootstrap command starts.
      after = [ "maincopy.service" ];
      serviceConfig = daemonService // {
        Type = "oneshot";
        Restart = "no";
        ExecStart = "${cfg.package}/bin/maincopyd --config ${hostConfig} identity bootstrap nostr --public-key ${cfg.initialOwnerPublicKey}";
      };
    };
    systemd.services.maincopy-gateway = {
      description = "Maincopy isolated HTTPS gateway";
      wantedBy = [ "multi-user.target" ];
      wants = [
        "network-online.target"
        "maincopy.service"
      ];
      after = [
        "network-online.target"
        "maincopy.service"
      ];
      serviceConfig = serviceIsolation // {
        User = "maincopy-gateway";
        Group = "maincopy-gateway";
        StateDirectory = "maincopy-gateway";
        StateDirectoryMode = "0700";
        RuntimeDirectory = "maincopy-gateway";
        RuntimeDirectoryMode = "0700";
        WorkingDirectory = "/var/lib/maincopy-gateway";
        Environment = [
          "XDG_DATA_HOME=/var/lib/maincopy-gateway"
          "XDG_CONFIG_HOME=/var/lib/maincopy-gateway"
        ];
        LoadCredential = tlsCredentials "public" cfg.public.tls ++ tlsCredentials "admin" cfg.admin.tls;
        ReadWritePaths = [
          "/var/lib/maincopy-gateway"
          "/run/maincopy-gateway"
        ];
        InaccessiblePaths = [
          "-${stateDir}"
          "-${runtimeDir}"
        ];
        CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
        AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
        ExecStart = "${pkgs.caddy}/bin/caddy run --config ${gatewayConfig} --adapter caddyfile";
        Restart = "on-failure";
        TimeoutStopSec = "90s";
      };
    };
  };
}
