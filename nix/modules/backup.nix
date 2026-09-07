{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.maincopy;
  backup = cfg.backup;
  stateDir = "/var/lib/${cfg.stateDirectory}";
  databaseDir = "${stateDir}/database";
  candidateDir = "${stateDir}/content-candidates";
  backupDir = "/var/lib/maincopy-backup";
  replicaDir = "/var/lib/maincopy-litestream";
  credentialDir = "/run/maincopy-backup/private";
  expiryCredentialDir = "/run/maincopy-backup-expire/private";
  litestream = pkgs.callPackage ../packages/litestream.nix { };
  isolation = import ./service-isolation.nix;
  protectedPath =
    value:
    builtins.match "/[A-Za-z0-9/._-]+" value != null
    && value != "/nix/store"
    && !(lib.hasPrefix "/nix/store/" value)
    && !(lib.hasInfix "/../" value)
    && !(lib.hasSuffix "/.." value)
    && !(lib.hasInfix "//" value);
  fileOption =
    description:
    lib.mkOption {
      type = lib.types.nullOr (lib.types.addCheck lib.types.str protectedPath);
      default = null;
      inherit description;
    };
  litestreamConfig = (pkgs.formats.yaml { }).generate "maincopy-litestream.yml" {
    # This is a local replica. Only the checkpoint publisher can report an
    # off-site success. Replica retention and ciphertext retention are separate.
    snapshot = {
      interval = "24h";
      retention = "168h";
    };
    l0-retention = "168h";
    l0-retention-check-interval = "5m";
    retention.enabled = true;
    verify-compaction = true;
    shutdown-sync-timeout = "30s";
    shutdown-sync-interval = "1s";
    socket = {
      enabled = true;
      path = "/run/maincopy-litestream/private/control.sock";
    };
    logging = {
      level = "warn";
      type = "json";
    };
    dbs = [
      {
        path = "${databaseDir}/maincopy.db";
        meta-path = "${replicaDir}/active/metadata";
        replica = {
          type = "file";
          path = "${replicaDir}/active/replica";
          sync-interval = "1s";
        };
      }
    ];
  };
  commonArguments = [
    "--config"
    "/etc/maincopy/maincopy.toml"
    "--maincopyd"
    "${cfg.package}/bin/maincopyd"
    "--litestream"
    "${litestream}/bin/litestream"
    "--rclone"
    "${pkgs.rclone}/bin/rclone"
    "--key"
    "${credentialDir}/crypt-key"
    "--credentials"
    "${credentialDir}/b2-credentials"
    "--bucket"
    backup.bucket
    "--prefix"
    backup.prefix
  ];
  arguments = commonArguments ++ [
    "--runtime-directory"
    credentialDir
    "--directory"
    backupDir
    "--replica"
    "${replicaDir}/active/replica"
    "--socket"
    "/run/maincopy-litestream/private/control.sock"
    "--database"
    "${databaseDir}/maincopy.db"
    "--artifacts"
    candidateDir
    "--remote-retention-days"
    (toString backup.remoteRetentionDays)
    "--status-file"
    "/var/lib/maincopy-backup-status/backup-status.json"
  ];
  epochArguments = [
    "--directory"
    backupDir
    "--replica-root"
    replicaDir
    "--epoch-seconds"
    (toString backup.epochSeconds)
    "--retention-days"
    (toString backup.localRetentionDays)
    "--remote-retention-days"
    (toString backup.remoteRetentionDays)
  ];
  remoteExpiryArguments = epochArguments ++ [
    "--runtime-directory"
    expiryCredentialDir
    "--rclone"
    "${pkgs.rclone}/bin/rclone"
    "--credentials"
    "${expiryCredentialDir}/b2-credentials"
    "--bucket"
    backup.bucket
    "--prefix"
    backup.prefix
  ];
  expireCredentials = pkgs.writeShellScript "maincopy-backup-expire-credentials" ''
    set -eu
    umask 077
    ${pkgs.coreutils}/bin/install -m 0600 "$CREDENTIALS_DIRECTORY/b2-credentials" ${expiryCredentialDir}/b2-credentials
  '';
  prepareCredentials = pkgs.writeShellScript "maincopy-backup-prepare-credentials" ''
    set -eu
    umask 077
    # systemd grants service access through an ACL. Copy into the private
    # runtime directory so the application's owner-only file policy still
    # applies, just as it does to the daemon's SSH credentials.
    ${pkgs.coreutils}/bin/install -m 0600 "$CREDENTIALS_DIRECTORY/crypt-key" ${credentialDir}/crypt-key
    ${pkgs.coreutils}/bin/install -m 0600 "$CREDENTIALS_DIRECTORY/b2-credentials" ${credentialDir}/b2-credentials
  '';
  restore = pkgs.writeShellScriptBin "maincopy-restore-checkpoint" ''
    exec ${pkgs.python3}/bin/python3 ${../scripts}/checkpoint-restore.py "$@" \
      --maincopyd ${cfg.package}/bin/maincopyd \
      --litestream ${litestream}/bin/litestream \
      --rclone ${pkgs.rclone}/bin/rclone
  '';
in
{
  options.services.maincopy.backup = {
    enable = lib.mkEnableOption "continuous Litestream replication with encrypted complete checkpoints to Backblaze B2";
    keyFile = fileOption "Protected canonical base64 encoding of 32 random bytes for rclone crypt. Keep an independently recoverable copy. Only systemd credentials and protected runtime configuration contain the key.";
    credentialsFile = fileOption "Protected rclone configuration containing only [maincopy-b2], type=b2, account, and key. Use a dedicated bucket-scoped application key.";
    bucket = lib.mkOption {
      type = lib.types.addCheck lib.types.str (
        value: builtins.match "[A-Za-z0-9][A-Za-z0-9-]{5,49}" value != null
      );
      default = "maincopy-backup";
      description = "Dedicated Backblaze B2 bucket.";
    };
    prefix = lib.mkOption {
      type = lib.types.addCheck lib.types.str (
        value:
        builtins.stringLength value <= 256
        && builtins.match "[A-Za-z0-9_-]+(/[A-Za-z0-9_-]+)*" value != null
      );
      default = "maincopy";
      description = "Immutable ciphertext object namespace. Use a separate prefix for every independent site and encryption key.";
    };
    intervalSeconds = lib.mkOption {
      type = lib.types.ints.between 60 3600;
      default = 60;
      description = "Delay after each completed checkpoint job. Actual recovery lag also includes replay, validation, encryption, and upload time.";
    };
    epochSeconds = lib.mkOption {
      type = lib.types.ints.between 3600 86400;
      default = 86400;
      description = "Maximum upload lifetime of one independently recoverable native replica epoch. Native replication restarts 10 minutes earlier to reserve publication time; Maincopy stays running.";
    };
    localRetentionDays = lib.mkOption {
      type = lib.types.ints.between 1 30;
      default = 7;
      description = "Local complete checkpoint lifetime. Epoch caches and retired native files retain at most one additional dependency day; independent hourly cleanup also runs during upload failure.";
    };
    remoteRetentionDays = lib.mkOption {
      type = lib.types.ints.between 3 90;
      default = 9;
      description = "Required B2 lifecycle days from upload to hiding, followed by one day to deletion. Must exceed local retention by at least two days. The operator configures the dedicated bucket; Maincopy only validates it.";
    };
    staleAfterSeconds = lib.mkOption {
      type = lib.types.ints.between 60 604800;
      default = 300;
      description = "Maximum age of a complete off-site checkpoint, including upload and manifest publication.";
    };
  };
  config = lib.mkIf (cfg.enable && backup.enable) {
    assertions = [
      {
        assertion = backup.keyFile != null && backup.credentialsFile != null;
        message = "Maincopy encrypted checkpoints require protected runtime crypt-key and B2 credential files.";
      }
      {
        assertion = backup.remoteRetentionDays >= backup.localRetentionDays + 2;
        message = "Maincopy remote lifecycle must preserve the finite epoch dependency window beyond local retention.";
      }
    ];
    environment.systemPackages = [
      litestream
      pkgs.rclone
      restore
    ];
    systemd.tmpfiles.rules = [
      "d ${backupDir} 0700 maincopy maincopy -"
      "f ${backupDir}/.checkpoint.lock 0600 maincopy maincopy -"
      "d ${replicaDir} 0700 maincopy maincopy -"
      "d /run/maincopy-backup-expire 0700 maincopy maincopy -"
      "d /var/lib/maincopy-backup-status 0700 maincopy maincopy -"
      "d /run/maincopy-backup 0700 maincopy maincopy -"
      "d /run/maincopy-litestream 0700 maincopy maincopy -"
    ];
    environment.etc."maincopy/litestream.yml".source = litestreamConfig;
    systemd.services.maincopy = {
      wants = [
        "maincopy-litestream.service"
        "maincopy-backup.timer"
        "maincopy-backup-expire-local.timer"
        "maincopy-backup-expire-remote.timer"
      ];
      # Stop the application first, then let Litestream flush its final WAL.
      after = [ "maincopy-litestream.service" ];
      serviceConfig = {
        # Empty mounts remain effective even when peers create or recreate
        # their host directories after this namespace has already started.
        TemporaryFileSystem = [
          "${backupDir}:ro"
          "${replicaDir}:ro"
          "/run/credentials:ro"
          "/run/maincopy-backup:ro"
          "/run/maincopy-backup-expire:ro"
          "/run/maincopy-litestream:ro"
        ];
        ReadOnlyPaths = [ "/var/lib/maincopy-backup-status" ];
        BindReadOnlyPaths = lib.optional (
          cfg.source.credentials != { } || cfg.mail.mode == "ses"
        ) "/run/credentials/maincopy.service";
      };
    };
    systemd.services.maincopy-initialize = lib.mkIf (cfg.initialOwnerPublicKey != null) {
      conflicts = [
        "maincopy-litestream.service"
        "maincopy-backup.service"
        "maincopy-backup.timer"
        "maincopy-backup-expire-local.timer"
        "maincopy-backup-expire-remote.timer"
        "maincopy-backup-expire-local.service"
        "maincopy-backup-expire-remote.service"
      ];
      # Stop the timer too, so it cannot reactivate the conflicting uploader
      # during bootstrap. Normal daemon startup pulls the timer back in.
      after = [
        "maincopy-litestream.service"
        "maincopy-backup.service"
        "maincopy-backup.timer"
        "maincopy-backup-expire-local.timer"
        "maincopy-backup-expire-remote.timer"
        "maincopy-backup-expire-local.service"
        "maincopy-backup-expire-remote.service"
      ];
    };
    systemd.services.maincopy-litestream = {
      description = "Maincopy local Litestream replica";
      wantedBy = [ "multi-user.target" ];
      before = [ "maincopy.service" ];
      unitConfig.ConditionPathExists = "${databaseDir}/maincopy.db";
      serviceConfig = isolation // {
        # Reviewed exception: SQLite enforces owner UID and mode 0600. This
        # separate, isolated unit shares only the dedicated database owner UID.
        User = "maincopy";
        Group = "maincopy";
        StateDirectory = "maincopy-litestream";
        StateDirectoryMode = "0700";
        RuntimeDirectory = "maincopy-litestream/private";
        RuntimeDirectoryPreserve = "restart";
        RuntimeDirectoryMode = "0700";
        WorkingDirectory = replicaDir;
        TemporaryFileSystem = [
          "${stateDir}:ro"
          "/run/${cfg.runtimeDirectory}:ro"
          "/run/credentials:ro"
          "/run/maincopy-backup:ro"
          "/run/maincopy-backup-expire:ro"
          "${backupDir}:ro"
        ];
        BindPaths = [
          databaseDir
          "${backupDir}/.checkpoint.lock"
        ];
        ReadWritePaths = [
          databaseDir
          replicaDir
          "/run/maincopy-litestream/private"
        ];
        RestrictAddressFamilies = [ "AF_UNIX" ];
        # Litestream best-effort mirrors the database owner on replica files.
        # The dedicated UID already owns them: deny changes without killing
        # those optional calls, and keep every other privileged call blocked.
        SystemCallFilter = isolation.SystemCallFilter ++ [ "~@chown:EPERM" ];
        # A simple wrapper lets Maincopy consume pending restore acceptance
        # before native Litestream can create WAL/SHM or change database bytes.
        Type = "simple";
        ExecStartPre = "${pkgs.python3}/bin/python3 ${../scripts}/backup_epochs.py prepare ${lib.escapeShellArgs epochArguments}";
        # Keep native state protected from expiry even after a wall-clock jump.
        # --no-fork preserves systemd's direct ownership of the native process.
        ExecStart = "${pkgs.util-linux}/bin/flock --exclusive --nonblock --no-fork ${replicaDir}/.native.lock ${pkgs.python3}/bin/python3 ${../scripts}/replica-start.py --marker ${databaseDir}/maincopy.db.restore.json --timeout-seconds 180 -- ${litestream}/bin/litestream replicate -config ${litestreamConfig}";
        # A fresh initial snapshot and new metadata/file replica are the
        # retention boundary. Native compaction alone preserves old bases.
        RuntimeMaxSec = backup.epochSeconds - 600;
        Restart = "always";
        TimeoutStartSec = "11min";
        RestartSec = 5;
        TimeoutStopSec = 45;
        KillMode = "control-group";
        MemoryMax = "1G";
        LimitFSIZE = "20G";
      };
    };
    systemd.services.maincopy-backup = {
      description = "Publish a complete encrypted Maincopy checkpoint to Backblaze B2";
      wants = [ "network-online.target" ];
      after = [
        "network-online.target"
        "maincopy.service"
        "maincopy-litestream.service"
      ];
      serviceConfig = isolation // {
        # Rust checkpoint validation requires openat2, which this systemd
        # setting otherwise rejects unconditionally with ENOSYS.
        RestrictSUIDSGID = false;
        Type = "oneshot";
        User = "maincopy";
        Group = "maincopy";
        StateDirectory = [
          "maincopy-backup"
          "maincopy-backup-status"
        ];
        StateDirectoryMode = "0700";
        RuntimeDirectory = "maincopy-backup/private";
        RuntimeDirectoryMode = "0700";
        WorkingDirectory = backupDir;
        TemporaryFileSystem = [
          "${stateDir}:ro"
          "/run/${cfg.runtimeDirectory}:ro"
          "/run/credentials:ro"
        ];
        InaccessiblePaths = [ "/run/maincopy-backup-expire" ];
        BindReadOnlyPaths = [
          # Bind the parent before taking the lock so rotation cannot leave
          # this namespace pinned to the previous active directory inode.
          replicaDir
          "/run/maincopy-litestream/private"
          "-${candidateDir}"
          "/run/credentials/maincopy-backup.service"
        ];
        ReadWritePaths = [
          backupDir
          "/var/lib/maincopy-backup-status"
          "/run/maincopy-backup/private"
        ];
        LoadCredential = [
          "crypt-key:${if backup.keyFile == null then "/missing-crypt-key" else backup.keyFile}"
          "b2-credentials:${
            if backup.credentialsFile == null then "/missing-b2-credentials" else backup.credentialsFile
          }"
        ];
        # Native Litestream replay uses the same optional ownership calls.
        SystemCallFilter = isolation.SystemCallFilter ++ [ "~@chown:EPERM" ];
        ExecStartPre = prepareCredentials;
        ExecStart = "${pkgs.python3}/bin/python3 ${../scripts}/checkpoint-backup.py ${lib.escapeShellArgs arguments}";
        ExecStopPost = "${pkgs.python3}/bin/python3 ${../scripts}/checkpoint-backup.py ${lib.escapeShellArgs arguments} --cleanup-only";
        TimeoutStartSec = "10min";
        KillMode = "control-group";
        Nice = 10;
        CPUWeight = 25;
        IOWeight = 25;
        MemoryMax = "1G";
        LimitFSIZE = "20G";
      };
    };
    # Local expiry has no credentials or network dependency. A failed remote
    # unit setup must not retain local subscriber history indefinitely.
    systemd.services.maincopy-backup-expire-local = {
      description = "Expire local Maincopy checkpoint epochs";
      serviceConfig = isolation // {
        Type = "oneshot";
        User = "maincopy";
        Group = "maincopy";
        TemporaryFileSystem = [
          "${stateDir}:ro"
          "/run/${cfg.runtimeDirectory}:ro"
          "/run/credentials:ro"
          "/run/maincopy-backup:ro"
          "/run/maincopy-backup-expire:ro"
          "/run/maincopy-litestream:ro"
        ];
        ReadWritePaths = [
          backupDir
          replicaDir
        ];
        RestrictAddressFamilies = [ "AF_UNIX" ];
        ExecStart = "${pkgs.python3}/bin/python3 ${../scripts}/backup_epochs.py expire-local ${lib.escapeShellArgs epochArguments}";
        TimeoutStartSec = "11min";
        MemoryMax = "256M";
      };
    };
    systemd.services.maincopy-backup-expire-remote = {
      description = "Expire remote Maincopy checkpoint epochs and versions";
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      serviceConfig = isolation // {
        Type = "oneshot";
        User = "maincopy";
        Group = "maincopy";
        RuntimeDirectory = "maincopy-backup-expire/private";
        RuntimeDirectoryMode = "0700";
        TemporaryFileSystem = [
          "${backupDir}:ro"
          "${stateDir}:ro"
          "${replicaDir}:ro"
          "/run/${cfg.runtimeDirectory}:ro"
          "/run/credentials:ro"
          "/run/maincopy-backup:ro"
          "/run/maincopy-litestream:ro"
        ];
        BindReadOnlyPaths = [ "/run/credentials/maincopy-backup-expire-remote.service" ];
        # Remote expiration needs the publisher lock, never its plaintext
        # capture workspace or cached recovery contents.
        BindPaths = [ "${backupDir}/.checkpoint.lock" ];
        ReadWritePaths = [ expiryCredentialDir ];
        LoadCredential = [
          "b2-credentials:${
            if backup.credentialsFile == null then "/missing-b2-credentials" else backup.credentialsFile
          }"
        ];
        ExecStartPre = expireCredentials;
        ExecStart = "${pkgs.python3}/bin/python3 ${../scripts}/backup_epochs.py expire-remote ${lib.escapeShellArgs remoteExpiryArguments}";
        TimeoutStartSec = "11min";
        KillMode = "control-group";
        MemoryMax = "256M";
      };
    };
    systemd.timers.maincopy-backup-expire-local = {
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnBootSec = "2min";
        OnUnitInactiveSec = "1h";
        AccuracySec = "1s";
      };
    };
    systemd.timers.maincopy-backup-expire-remote = {
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnBootSec = "3min";
        OnUnitInactiveSec = "1h";
        AccuracySec = "1s";
      };
    };
    systemd.timers.maincopy-backup = {
      description = "Publish encrypted checkpoints at a one-minute target interval";
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnBootSec = "1min";
        OnUnitInactiveSec = "${toString backup.intervalSeconds}s";
        AccuracySec = "1s";
      };
    };
  };
}
