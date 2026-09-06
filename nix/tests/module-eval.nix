{
  pkgs,
  module,
  package,
}:
let
  inherit (pkgs) lib;
  base = {
    nixpkgs.hostPlatform = pkgs.stdenv.hostPlatform.system;
    system.stateVersion = "26.05";
    boot.loader.grub.enable = false;
    fileSystems."/" = {
      device = "/dev/vda";
      fsType = "ext4";
    };
    services.maincopy = {
      enable = true;
      inherit package;
      public.domain = "site.example.test";
    };
  };
  evaluate =
    settings:
    (import "${pkgs.path}/nixos/lib/eval-config.nix" {
      system = pkgs.stdenv.hostPlatform.system;
      modules = [
        module
        base
        { services.maincopy = settings; }
      ];
    }).config;
  minimal = evaluate { };
  complete = evaluate {
    backup = {
      enable = true;
      keyFile = "/run/secrets/backup-crypt";
      credentialsFile = "/run/secrets/backup-b2";
    };
    initialOwnerPublicKey = "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9";
    public.tls = {
      mode = "provided";
      certificateFile = "/run/secrets/public-cert";
      privateKeyFile = "/run/secrets/public-key";
    };
    admin.listenAddresses = [ "10.20.30.40" ];
    source = {
      managed = true;
      credentials.deploy = {
        privateKeyFile = "/run/secrets/deploy-key";
        knownHostsFile = "/run/secrets/deploy-hosts";
      };
    };
  };
  internet = evaluate {
    admin = {
      allowInternet = true;
      listenAddresses = [ "0.0.0.0" ];
    };
  };
  rejected = settings: !(lib.all (entry: entry.assertion) (evaluate settings).assertions);
  typedRejected =
    optionPath: settings:
    # Force the invalid scalar itself. Deep evaluation of serviceConfig also
    # walks script derivations and nixpkgs internals, so rejection can otherwise
    # depend on traversal order or fail at the evaluator's call-depth limit.
    !(builtins.tryEval (lib.getAttrFromPath optionPath (evaluate settings).services.maincopy)).success;
in
assert lib.all (entry: entry.assertion) minimal.assertions;
assert lib.all (entry: entry.assertion) complete.assertions;
assert lib.all (entry: entry.assertion) internet.assertions;
assert minimal.systemd.services.maincopy.serviceConfig.User == "maincopy";
assert minimal.systemd.services.maincopy-gateway.serviceConfig.User == "maincopy-gateway";
assert minimal.systemd.services.maincopy.serviceConfig.UMask == "0077";
assert minimal.systemd.services.maincopy.serviceConfig.StateDirectoryMode == "0700";
assert minimal.systemd.services.maincopy.serviceConfig.PrivatePIDs;
assert minimal.systemd.services.maincopy.serviceConfig.RuntimeDirectory == "maincopy/private";
assert !minimal.systemd.services.maincopy.serviceConfig.RestrictSUIDSGID;
assert minimal.systemd.services.maincopy.serviceConfig.NoNewPrivileges;
assert minimal.systemd.services.maincopy.serviceConfig.CapabilityBoundingSet == "";
assert !(minimal.systemd.services ? maincopy-initialize);
assert complete.systemd.services.maincopy-initialize.wantedBy == [ ];
assert lib.all (
  unit: builtins.elem unit complete.systemd.services.maincopy-initialize.after
) complete.systemd.services.maincopy-initialize.conflicts;
assert builtins.elem "maincopy-litestream.service" complete.systemd.services.maincopy.wants;
assert builtins.elem "maincopy-litestream.service" complete.systemd.services.maincopy.after;
assert builtins.elem "maincopy-backup.timer" complete.systemd.services.maincopy.wants;
assert
  complete.systemd.services.maincopy-litestream.unitConfig.ConditionPathExists
  == "/var/lib/maincopy/database/maincopy.db";
assert builtins.length complete.systemd.services.maincopy.serviceConfig.LoadCredential == 2;
assert builtins.length complete.systemd.services.maincopy-gateway.serviceConfig.LoadCredential == 2;
assert rejected { admin.listenAddresses = [ "0.0.0.0" ]; };
assert rejected { admin.listenAddresses = [ ]; };
assert rejected { admin.domain = "site.example.test"; };
assert rejected { admin.backendPort = 3000; };
assert rejected { public.tls.mode = "provided"; };
assert rejected { public.tls.certificateFile = "/run/secrets/unpaired"; };
assert rejected { admin.port = 443; };
assert rejected { stateDirectory = "maincopy-backup"; };
assert rejected { backup.enable = true; };
assert complete.systemd.services.maincopy-backup.serviceConfig.User == "maincopy";
assert complete.systemd.services.maincopy-backup.serviceConfig.PrivatePIDs;
assert
  complete.systemd.services.maincopy-backup.serviceConfig.RuntimeDirectory
  == "maincopy-backup/private";
assert
  complete.systemd.services.maincopy-litestream.serviceConfig.RuntimeDirectory
  == "maincopy-litestream/private";
assert builtins.elem "/run/credentials:ro"
  complete.systemd.services.maincopy.serviceConfig.TemporaryFileSystem;
assert builtins.elem "/run/credentials/maincopy.service"
  complete.systemd.services.maincopy.serviceConfig.BindReadOnlyPaths;
assert lib.hasInfix "/run/maincopy-backup/private/crypt-key"
  complete.systemd.services.maincopy-backup.serviceConfig.ExecStart;
assert builtins.elem "/run/maincopy-backup:ro"
  complete.systemd.services.maincopy.serviceConfig.TemporaryFileSystem;
assert builtins.elem "/run/maincopy-backup:ro"
  complete.systemd.services.maincopy-litestream.serviceConfig.TemporaryFileSystem;
assert !complete.systemd.services.maincopy-backup.serviceConfig.RestrictSUIDSGID;
assert complete.systemd.services.maincopy-litestream.serviceConfig.User == "maincopy";
assert complete.systemd.services.maincopy-litestream.serviceConfig.PrivatePIDs;
assert complete.systemd.services.maincopy-litestream.serviceConfig.RestrictSUIDSGID;
assert complete.systemd.services.maincopy-litestream.serviceConfig.Type == "simple";
assert lib.hasInfix "/replica-start.py --marker /var/lib/maincopy/database/maincopy.db.restore.json"
  complete.systemd.services.maincopy-litestream.serviceConfig.ExecStart;
assert builtins.elem "~@chown:EPERM"
  complete.systemd.services.maincopy-litestream.serviceConfig.SystemCallFilter;
assert builtins.elem "~@chown:EPERM"
  complete.systemd.services.maincopy-backup.serviceConfig.SystemCallFilter;
assert
  complete.systemd.services.maincopy-litestream.serviceConfig.RestrictAddressFamilies
  == [ "AF_UNIX" ];
assert builtins.elem "/var/lib/maincopy/database"
  complete.systemd.services.maincopy-litestream.serviceConfig.BindPaths;
assert builtins.elem "/var/lib/maincopy-backup:ro"
  complete.systemd.services.maincopy.serviceConfig.TemporaryFileSystem;
assert builtins.elem "/var/lib/maincopy-backup-status"
  complete.systemd.services.maincopy.serviceConfig.ReadOnlyPaths;
assert
  complete.systemd.services.maincopy-backup.serviceConfig.BindReadOnlyPaths == [
    "/var/lib/maincopy-litestream/replica"
    "/run/maincopy-litestream/private"
    "-/var/lib/maincopy/content-candidates"
    "/run/credentials/maincopy-backup.service"
  ];
assert complete.systemd.timers.maincopy-backup.timerConfig.OnUnitInactiveSec == "60s";
assert complete.services.maincopy.backup.staleAfterSeconds == 300;
assert typedRejected [ "public" "bind" ] { public.bind = "999.0.0.1"; };
assert typedRejected [ "source" "credentials" "deploy" "privateKeyFile" ] {
  source = {
    managed = true;
    credentials.deploy = {
      privateKeyFile = "/run/secrets/../bad";
      knownHostsFile = "/run/secrets/known-hosts";
    };
  };
};
assert typedRejected [ "stateDirectory" ] { stateDirectory = "../shared"; };
assert typedRejected [ "source" "credentials" "deploy" "privateKeyFile" ] {
  source = {
    managed = true;
    credentials.deploy = {
      privateKeyFile = "/nix/store/visible-key";
      knownHostsFile = "/run/secrets/known-hosts";
    };
  };
};
{
  passed = true;
  hostConfig = minimal.environment.etc."maincopy/maincopy.toml".source;
  gatewayConfig = minimal.environment.etc."maincopy/Caddyfile".source;
  daemon = minimal.systemd.services.maincopy.serviceConfig.ExecStart;
  gateway = minimal.systemd.services.maincopy-gateway.serviceConfig.ExecStart;
}
