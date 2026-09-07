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
  mailSettings = {
    mode = "ses";
    sender = "news@example.test";
    region = "us-east-1";
    configurationSet = "maincopy-newsletter";
    credentialFile = "/run/secrets/mail-ses.json";
    controlSigningKeyFile = "/run/secrets/mail-controls";
    subscriptions = {
      operatorName = "Maincopy fixture";
      postalAddress = "123 Fixture Street, Test City";
      purpose = "Receive published articles.";
      privacyUrl = "https://site.example.test/privacy/";
      contactAddress = "contact@example.test";
    };
  };
  pausedMail = evaluate {
    runtimeDirectory = "mail-fixture";
    backup = {
      enable = true;
      keyFile = "/run/secrets/backup-crypt";
      credentialsFile = "/run/secrets/backup-b2";
    };
    mail = mailSettings;
  };
  activeMail = evaluate {
    mail = lib.recursiveUpdate mailSettings {
      subscriptions.mode = "enabled";
      feedback = {
        queueUrl = "https://sqs.us-east-1.amazonaws.com/123456789012/maincopy-feedback";
        topicArn = "arn:aws:sns:us-east-1:123456789012:maincopy-feedback";
      };
    };
    source = {
      managed = true;
      credentials.mail = {
        privateKeyFile = "/run/secrets/source-key";
        knownHostsFile = "/run/secrets/source-hosts";
      };
    };
  };
  hostValue =
    configuration: configuration.environment.etc."maincopy/maincopy.toml".source.drvAttrs.value;
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
assert lib.all (entry: entry.assertion) pausedMail.assertions;
assert lib.all (entry: entry.assertion) activeMail.assertions;
assert minimal.services.maincopy.mail.mode == "disabled";
assert (hostValue minimal).mail == { mode = "disabled"; };
assert minimal.systemd.services.maincopy.serviceConfig.LoadCredential == [ ];
assert pausedMail.services.maincopy.mail.subscriptions.mode == "paused";
assert pausedMail.services.maincopy.mail.feedback == null;
assert
  (hostValue pausedMail).mail == {
    mode = "ses";
    sender = "news@example.test";
    region = "us-east-1";
    configuration_set = "maincopy-newsletter";
    credential_file = "/run/mail-fixture/private/credentials/mail-ses";
    control_signing_key_file = "/run/mail-fixture/private/credentials/mail-controls";
    max_campaign_recipients = 2000;
    max_daily_messages = 5000;
    max_daily_confirmation_messages = 100;
    send_interval_milliseconds = 1000;
    subscriptions = {
      mode = "paused";
      operator_name = "Maincopy fixture";
      postal_address = "123 Fixture Street, Test City";
      purpose = "Receive published articles.";
      privacy_url = "https://site.example.test/privacy/";
      contact_address = "contact@example.test";
    };
  };
assert (hostValue activeMail).mail.subscriptions.mode == "enabled";
assert
  (hostValue activeMail).mail.feedback == {
    queue_url = "https://sqs.us-east-1.amazonaws.com/123456789012/maincopy-feedback";
    topic_arn = "arn:aws:sns:us-east-1:123456789012:maincopy-feedback";
  };
assert
  pausedMail.systemd.services.maincopy.serviceConfig.LoadCredential == [
    "mail-controls:/run/secrets/mail-controls"
    "mail-ses:/run/secrets/mail-ses.json"
  ];
assert builtins.length activeMail.systemd.services.maincopy.serviceConfig.LoadCredential == 4;
assert
  builtins.length (lib.unique activeMail.systemd.services.maincopy.serviceConfig.LoadCredential) == 4;
assert builtins.elem "/run/credentials/maincopy.service"
  pausedMail.systemd.services.maincopy.serviceConfig.BindReadOnlyPaths;
assert builtins.elem "/run/mail-fixture:ro"
  pausedMail.systemd.services.maincopy-backup.serviceConfig.TemporaryFileSystem;
assert builtins.elem "/run/mail-fixture:ro"
  pausedMail.systemd.services.maincopy-litestream.serviceConfig.TemporaryFileSystem;
assert builtins.elem "/run/mail-fixture:ro"
  pausedMail.systemd.services.maincopy-backup-expire-remote.serviceConfig.TemporaryFileSystem;
assert pausedMail.systemd.services.maincopy.serviceConfig.TimeoutStopSec == "180s";
assert rejected { mail.mode = "ses"; };
assert rejected {
  mail = lib.recursiveUpdate mailSettings { subscriptions.mode = "enabled"; };
};
assert rejected {
  mail = mailSettings // {
    maxDailyMessages = 99;
  };
};
assert typedRejected [ "mail" "credentialFile" ] {
  mail = mailSettings // {
    credentialFile = "/nix/store/visible-mail-secret";
  };
};
assert typedRejected [ "mail" "credentialFile" ] {
  mail = mailSettings // {
    credentialFile = ./module-eval.nix;
  };
};
assert typedRejected [ "mail" "controlSigningKeyFile" ] {
  mail = mailSettings // {
    controlSigningKeyFile = "/run/secrets/../mail-controls";
  };
};
assert typedRejected [ "mail" "sendIntervalMilliseconds" ] {
  mail = mailSettings // {
    sendIntervalMilliseconds = 99;
  };
};
assert typedRejected [ "mail" "subscriptions" "privacyUrl" ] {
  mail = lib.recursiveUpdate mailSettings {
    subscriptions.privacyUrl = "http://example.test/privacy/";
  };
};
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
    "/var/lib/maincopy-litestream"
    "/run/maincopy-litestream/private"
    "-/var/lib/maincopy/content-candidates"
    "/run/credentials/maincopy-backup.service"
  ];
assert complete.systemd.services.maincopy-litestream.serviceConfig.Restart == "always";
assert complete.systemd.services.maincopy-litestream.serviceConfig.RuntimeMaxSec == 85800;
assert
  complete.systemd.services.maincopy-litestream.serviceConfig.RuntimeDirectoryPreserve == "restart";
assert lib.hasInfix "/backup_epochs.py prepare"
  complete.systemd.services.maincopy-litestream.serviceConfig.ExecStartPre;
assert lib.hasInfix
  "/bin/flock --exclusive --nonblock --no-fork /var/lib/maincopy-litestream/.native.lock"
  complete.systemd.services.maincopy-litestream.serviceConfig.ExecStart;
assert builtins.elem "/var/lib/maincopy-backup/.checkpoint.lock"
  complete.systemd.services.maincopy-litestream.serviceConfig.BindPaths;
assert !(complete.systemd.services.maincopy-backup-expire-local.serviceConfig ? LoadCredential);
assert
  complete.systemd.services.maincopy-backup-expire-local.serviceConfig.RestrictAddressFamilies
  == [ "AF_UNIX" ];
assert
  builtins.length complete.systemd.services.maincopy-backup-expire-remote.serviceConfig.LoadCredential
  == 1;
assert builtins.elem "/var/lib/maincopy-backup:ro"
  complete.systemd.services.maincopy-backup-expire-remote.serviceConfig.TemporaryFileSystem;
assert
  complete.systemd.services.maincopy-backup-expire-remote.serviceConfig.BindPaths
  == [ "/var/lib/maincopy-backup/.checkpoint.lock" ];
assert
  complete.systemd.services.maincopy-backup-expire-remote.serviceConfig.ReadWritePaths
  == [ "/run/maincopy-backup-expire/private" ];
assert lib.hasInfix "--runtime-directory"
  complete.systemd.services.maincopy-backup.serviceConfig.ExecStart;
assert lib.hasInfix "--runtime-directory"
  complete.systemd.services.maincopy-backup-expire-remote.serviceConfig.ExecStart;
assert complete.systemd.timers.maincopy-backup-expire-local.timerConfig.OnUnitInactiveSec == "1h";
assert complete.services.maincopy.backup.localRetentionDays == 7;
assert complete.services.maincopy.backup.remoteRetentionDays == 9;
assert rejected {
  backup = {
    enable = true;
    keyFile = "/run/secrets/key";
    credentialsFile = "/run/secrets/b2";
    remoteRetentionDays = 8;
  };
};
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
