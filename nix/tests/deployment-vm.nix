{
  pkgs,
  module,
  package,
}:
let
  litestream = pkgs.callPackage ../packages/litestream.nix { };
  publication = pkgs.writeTextDir "publication.toml" ''
    [site]
    title = "Deployment VM"
    base_url = "https://site.example.test"
    description = "Maincopy deployment and encrypted recovery fixture."
    [author]
    name = "Deployment fixture"
  '';
  uploadProbe = pkgs.writeScript "b2-ciphertext-upload-probe" ''
    #!${pkgs.python3}/bin/python3
    import configparser, json, os, pathlib, sys
    args = sys.argv[1:]
    assert "fixture-secret-key" not in " ".join(args)
    offsite = "/var/lib/maincopy-backup/offsite-fixture"
    if any(value.startswith("maincopy-b2:") for value in args):
        if pathlib.Path("/var/lib/maincopy-backup/interrupt-upload").exists():
            sys.exit(41)
        if "lifecycle" in args:
            print(json.dumps([{"fileNamePrefix":"", "daysFromUploadingToHiding":9, "daysFromHidingToDeleting":1, "daysFromStartingToCancelingUnfinishedLargeFiles":1}]))
            sys.exit(0)
        if "--files-from" in args:
            listing = pathlib.Path(args[args.index("--files-from") + 1]).read_text().splitlines()
            for name in listing:
                with (pathlib.Path(args[-2]) / name).open("rb") as incoming:
                    assert incoming.read(8) == b"RCLONE\x00\x00"
        else:
            with pathlib.Path(args[-2]).open("rb") as incoming:
                assert incoming.read(8) == b"RCLONE\x00\x00"
        args = [offsite + value.split("maincopy-b2:fixture-bucket/maincopy", 1)[1] if value.startswith("maincopy-b2:") else value for value in args]
    if "--config" in args:
        path = pathlib.Path(args[args.index("--config") + 1])
        config = configparser.ConfigParser(interpolation=None)
        config.read(path)
        for remote in ("offsitecrypt", "selectioncrypt"):
            target = config[remote]["remote"]
            if target.startswith("maincopy-b2:fixture-bucket/maincopy"):
                config[remote]["remote"] = offsite + target.split("maincopy-b2:fixture-bucket/maincopy", 1)[1]
        with path.open("w") as output:
            config.write(output)
    os.execv("${pkgs.rclone}/bin/rclone", ["${pkgs.rclone}/bin/rclone", *args])
  '';
  expiryIsolationProbe = pkgs.writeShellScript "maincopy-expiry-isolation-probe" ''
    set -eu
    test ! -r /run/maincopy-backup/private/.config-visibility/rclone.conf
    test ! -r /run/credentials/maincopy-backup.service/crypt-key
    test ! -r /run/maincopy/private/credentials/mail-ses
    test ! -r /run/maincopy/private/credentials/mail-controls
    test ! -r /var/lib/fixture-secrets/crypt-key
    if test "$1" = remote; then
      test ! -r /var/lib/maincopy-backup/visibility-plaintext
      test -w /var/lib/maincopy-backup/.checkpoint.lock
      test -r /run/maincopy-backup-expire/private/b2-credentials
    else
      test -r /var/lib/maincopy-backup/visibility-plaintext
      test ! -r /run/maincopy-backup-expire/private/b2-credentials
    fi
  '';
  commonArguments = [
    "--config"
    "/etc/maincopy/maincopy.toml"
    "--maincopyd"
    "${package}/bin/maincopyd"
    "--litestream"
    "${litestream}/bin/litestream"
    "--rclone"
    "${uploadProbe}"
    "--key"
    "/run/maincopy-backup/private/crypt-key"
    "--credentials"
    "/run/maincopy-backup/private/b2-credentials"
    "--bucket"
    "fixture-bucket"
    "--prefix"
    "maincopy"
  ];
  backupCommand =
    "${pkgs.python3}/bin/python3 ${../scripts}/checkpoint-backup.py "
    + pkgs.lib.escapeShellArgs (
      commonArguments
      ++ [
        "--runtime-directory"
        "/run/maincopy-backup/private"
        "--directory"
        "/var/lib/maincopy-backup"
        "--replica"
        "/var/lib/maincopy-litestream/active/replica"
        "--socket"
        "/run/maincopy-litestream/private/control.sock"
        "--database"
        "/var/lib/maincopy/database/maincopy.db"
        "--artifacts"
        "/var/lib/maincopy/content-candidates"
        "--status-file"
        "/var/lib/maincopy-backup-status/backup-status.json"
      ]
    );
  restoreCommand =
    "${pkgs.python3}/bin/python3 ${../scripts}/checkpoint-restore.py "
    + pkgs.lib.escapeShellArgs [
      "--config"
      "/var/lib/maincopy-recovery/host.toml"
      "--maincopyd"
      "${package}/bin/maincopyd"
      "--litestream"
      "${litestream}/bin/litestream"
      "--rclone"
      "${uploadProbe}"
      "--key"
      "/var/lib/maincopy-recovery/key"
      "--credentials"
      "/var/lib/maincopy-recovery/b2.conf"
      "--bucket"
      "fixture-bucket"
      "--prefix"
      "maincopy"
      "--directory"
      "/var/lib/maincopy-recovery/download"
    ];
in
pkgs.testers.runNixOSTest {
  name = "maincopy-deployment";
  nodes.machine = { lib, ... }: {
    imports = [ module ];
    virtualisation.memorySize = 3072;
    services.maincopy = {
      enable = true;
      inherit package;
      contentRoot = toString publication;
      initialOwnerPublicKey = "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9";
      public = {
        domain = "site.example.test";
        tls.mode = "internal";
      };
      backup = {
        enable = true;
        keyFile = "/var/lib/fixture-secrets/crypt-key";
        credentialsFile = "/var/lib/fixture-secrets/b2.conf";
        bucket = "fixture-bucket";
      };
      mail = {
        mode = "ses";
        sender = "news@example.test";
        region = "us-east-1";
        configurationSet = "maincopy-newsletter";
        credentialFile = "/var/lib/fixture-secrets/mail-ses.json";
        controlSigningKeyFile = "/var/lib/fixture-secrets/mail-controls";
        subscriptions = {
          # Paused, with no feedback source: no mail network worker starts.
          mode = "paused";
          operatorName = "Deployment fixture";
          postalAddress = "123 Fixture Street, Test City";
          purpose = "Receive published articles.";
          privacyUrl = "https://site.example.test/privacy/";
          contactAddress = "contact@example.test";
        };
      };
    };
    # Only B2 transport is replaced. Native Litestream, Rust manifests/restore,
    # rclone crypt, and systemd isolation all execute in the VM.
    systemd.services.maincopy-backup.serviceConfig.ExecStart = lib.mkForce backupCommand;
    systemd.timers.maincopy-backup.wantedBy = lib.mkForce [ ];
    # Exercise timer activation through daemon Wants without background jobs
    # racing the deliberately interrupted uploads in this VM scenario.
    systemd.timers.maincopy-backup.timerConfig = {
      OnBootSec = lib.mkForce "1h";
      OnUnitInactiveSec = lib.mkForce "1h";
    };
    # Retention has focused native fixtures below the real Nix sandbox; do
    # not let its remote timer use production B2 credentials in this VM.
    systemd.timers.maincopy-backup-expire-local.timerConfig.OnBootSec = lib.mkForce "1h";
    systemd.timers.maincopy-backup-expire-remote.timerConfig.OnBootSec = lib.mkForce "1h";
    # Native fixtures exercise expiration behavior. These invocations exercise
    # the actual unit namespaces while publisher-shaped secret staging exists.
    systemd.services.maincopy-backup-expire-local.serviceConfig.ExecStart =
      lib.mkForce "${expiryIsolationProbe} local";
    systemd.services.maincopy-backup-expire-remote.serviceConfig.ExecStart =
      lib.mkForce "${expiryIsolationProbe} remote";
    environment.systemPackages = [
      pkgs.curl
      pkgs.jq
      pkgs.sqlite
      pkgs.python3
    ];
    system.activationScripts.fixture-secrets.text = ''
      install -d -m 0700 /var/lib/fixture-secrets
      if ! test -e /var/lib/fixture-secrets/crypt-key; then
        ${pkgs.python3}/bin/python3 -c 'import base64,os,pathlib; pathlib.Path("/var/lib/fixture-secrets/crypt-key").write_bytes(base64.b64encode(os.urandom(32)))'
      fi
      cat > /var/lib/fixture-secrets/b2.conf <<'EOF'
      [maincopy-b2]
      type=b2
      account=fixture-account
      key=fixture-secret-key
      EOF
      cat > /var/lib/fixture-secrets/mail-ses.json <<'EOF'
      {"access_key_id":"AKIDEXAMPLE","secret_access_key":"wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"}
      EOF
      if ! test -e /var/lib/fixture-secrets/mail-controls; then
        ${pkgs.python3}/bin/python3 -c 'import os,pathlib; pathlib.Path("/var/lib/fixture-secrets/mail-controls").write_text(os.urandom(32).hex())'
      fi
      chmod 0600 /var/lib/fixture-secrets/*
    '';
  };
  testScript = ''
    import json

    def wait_for_replication():
        # Type=simple admits the restore-marker wrapper before native startup.
        # A live HTTP listener or active unit does not establish replica
        # readiness. Native sync waits for a completed file-replica cutoff.
        confirmation = json.loads(machine.wait_until_succeeds(
            "timeout 15s litestream sync -wait -json -timeout 10 "
            "-socket /run/maincopy-litestream/private/control.sock "
            "/var/lib/maincopy/database/maincopy.db",
            timeout=60,
        ))
        assert confirmation["db_path"] == "/var/lib/maincopy/database/maincopy.db"
        assert type(confirmation["txid"]) is int
        assert type(confirmation["replica_txid"]) is int
        assert 0 < confirmation["txid"] <= confirmation["replica_txid"]

    def assert_late_created_secret_paths_are_hidden():
        # These known, harmless bytes exercise mount visibility without reading
        # a credential. Create the files only after both peer namespaces exist,
        # and repeat after the real uploader has removed its runtime directory.
        for directory in ["/run/maincopy-backup/private", "/run/credentials/maincopy-backup.service"]:
            machine.succeed(f"install -d -o maincopy -g maincopy -m 0700 {directory}")
            machine.succeed(f"install -o maincopy -g maincopy -m 0600 /dev/null {directory}/visibility-fixture")
            machine.succeed(f"su -s /bin/sh maincopy -c 'test -r {directory}/visibility-fixture'")
            for unit in ["maincopy.service", "maincopy-litestream.service"]:
                process = machine.succeed(f"systemctl show {unit} --property=MainPID --value").strip()
                machine.succeed(f"nsenter --target {process} --mount --pid -- su -s /bin/sh maincopy -c 'test ! -r {directory}/visibility-fixture'")
            machine.succeed(f"rm {directory}/visibility-fixture")

    def assert_expiration_namespaces_hide_publisher_staging():
        private = "/run/maincopy-backup/private/.config-visibility"
        secret = private + "/rclone.conf"
        bulk = "/var/lib/maincopy-backup/visibility-plaintext"
        # Harmless bytes model both the private credential config and bulk
        # capture. Native tests verify where the real config is created.
        machine.succeed(f"install -d -o maincopy -g maincopy -m 0700 {private}")
        for path in [secret, bulk]:
            machine.succeed(f"install -o maincopy -g maincopy -m 0600 /dev/null {path}")
        machine.succeed(f"su -s /bin/sh maincopy -c 'test -r {secret} && test -r {bulk}'")
        machine.succeed("systemctl start maincopy-backup-expire-local.service maincopy-backup-expire-remote.service")
        machine.succeed(f"rm {secret} {bulk}; rmdir {private}")

    start_all()
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds("systemctl is-failed maincopy.service")
    machine.fail("curl --fail --max-time 2 http://127.0.0.1:3000/health/live")
    machine.succeed("! journalctl -u maincopy.service --no-pager | grep -q 'Password:'")
    machine.succeed("systemctl stop maincopy.service; systemctl reset-failed maincopy.service")
    machine.succeed("systemctl start maincopy-initialize.service")
    machine.fail("systemctl is-active maincopy-litestream.service")
    machine.fail("systemctl is-active maincopy-backup.timer")
    machine.succeed("systemctl start maincopy.service")
    machine.wait_for_unit("maincopy-backup.timer")
    for port in [3000, 3001, 3002]:
        machine.wait_for_open_port(port, timeout=60)
    machine.wait_for_unit("maincopy-gateway.service")
    machine.wait_for_unit("maincopy-litestream.service")
    public = "curl -ksS --resolve site.example.test:443:127.0.0.1 https://site.example.test"
    admin = "curl -ksS --resolve admin.localhost:8443:127.0.0.1 https://admin.localhost:8443"
    machine.wait_until_succeeds(public + "/health/live -f")
    machine.succeed(public + "/email/subscribe -f | grep -q 'New subscriptions are paused'")
    for name in ["mail-ses", "mail-controls"]:
        path = "/run/maincopy/private/credentials/" + name
        machine.succeed(f"test $(stat -c %a {path}) = 600")
        machine.succeed(f"test $(stat -c %U {path}) = maincopy")
        machine.fail(f"su -s /bin/sh maincopy-gateway -c 'test -r {path}'")
        process = machine.succeed("systemctl show maincopy-litestream.service --property=MainPID --value").strip()
        machine.succeed(f"nsenter --target {process} --mount --pid -- su -s /bin/sh maincopy -c 'test ! -r {path}'")
    machine.succeed(admin + "/admin/login -f | grep -q 'Sign in'")
    for path in ["/admin", "/admin/users", "/api/admin/v1/auth/sessions", "/metrics"]:
        assert machine.succeed(public + path + " -o /dev/null -w '%{http_code}'").strip() == "404"
    assert machine.succeed(admin + "/metrics -o /dev/null -w '%{http_code}'").strip() == "404"
    machine.succeed("curl -fsS http://127.0.0.1:3002/metrics | grep maincopy_")
    assert machine.succeed(public + "/health/live -H 'Host: admin.localhost:8443' -o /dev/null -w '%{http_code}'").strip() == "421"
    bad_origin = admin + "/api/admin/v1/auth/sessions -X POST -H 'Origin: https://evil.example' -H 'Content-Type: application/json' --data '{}'"
    assert machine.succeed(bad_origin + " -o /dev/null -w '%{http_code}'").strip() == "403"
    spoofed = admin + "/api/admin/v1/identity/users -H 'X-Maincopy-Actor: owner' -H 'X-Maincopy-Role: owner' -H 'Forwarded: host=evil.example'"
    assert machine.succeed(spoofed + " -o /dev/null -w '%{http_code}'").strip() == "401"
    machine.succeed("test $(stat -c %a /var/lib/maincopy/database/maincopy.db) = 600")
    machine.fail("su -s /bin/sh maincopy-gateway -c 'cat /var/lib/maincopy/database/maincopy.db'")
    wait_for_replication()
    assert_late_created_secret_paths_are_hidden()
    assert_expiration_namespaces_hide_publisher_staging()
    machine.succeed("systemctl start maincopy-backup.service")
    assert_late_created_secret_paths_are_hidden()
    assert_expiration_namespaces_hide_publisher_staging()
    machine.succeed("curl -fsS http://127.0.0.1:3000/health/live")
    report = json.loads(machine.succeed("cat /var/lib/maincopy-backup-status/backup-status.json"))
    assert report["state"] == "healthy" and report["last_success_at"]
    machine.succeed("test $(stat -c %a /var/lib/maincopy-backup-status/backup-status.json) = 600")
    # Explicit rollover leaves the web process alive and starts a distinct
    # native base instead of keeping a shared historical LTX dependency.
    web_pid = machine.succeed("systemctl show maincopy.service -p MainPID --value").strip()
    epoch = json.loads(machine.succeed("cat /var/lib/maincopy-litestream/active/epoch.json"))["epoch"]
    machine.fail("${pkgs.util-linux}/bin/flock --nonblock /var/lib/maincopy-litestream/.native.lock true")
    machine.succeed("systemctl restart maincopy-litestream.service")
    assert machine.succeed("systemctl show maincopy.service -p MainPID --value").strip() == web_pid
    assert json.loads(machine.succeed("cat /var/lib/maincopy-litestream/active/epoch.json"))["epoch"] != epoch
    machine.succeed("curl -fsS http://127.0.0.1:3000/health/ready")
    wait_for_replication()
    machine.succeed("systemctl start maincopy-backup.service")
    report = json.loads(machine.succeed("cat /var/lib/maincopy-backup-status/backup-status.json"))
    assert report["state"] == "healthy"
    machine.succeed("touch /var/lib/maincopy-backup/interrupt-upload")
    machine.fail("systemctl start maincopy-backup.service")
    failed = json.loads(machine.succeed("cat /var/lib/maincopy-backup-status/backup-status.json"))
    assert failed["state"] == "degraded" and failed["last_success_at"] == report["last_success_at"]
    machine.succeed("rm /var/lib/maincopy-backup/interrupt-upload")
    # The recovery helper must select the preceding complete checkpoint after
    # interruption, decrypt/verify its exact inventory, replay native LTX, then
    # pass that exact SQLite output to Maincopy's offline acceptance boundary.
    machine.succeed("install -d -o maincopy -g maincopy -m 0700 /var/lib/maincopy-recovery")
    machine.succeed("install -o maincopy -g maincopy -m 0600 /var/lib/fixture-secrets/crypt-key /var/lib/maincopy-recovery/key")
    machine.succeed("install -o maincopy -g maincopy -m 0600 /var/lib/fixture-secrets/b2.conf /var/lib/maincopy-recovery/b2.conf")
    machine.succeed("sed -e 's|/var/lib/maincopy|/var/lib/maincopy-recovery/state|g' -e 's|/run/maincopy|/var/lib/maincopy-recovery/runtime|g' /etc/maincopy/maincopy.toml > /var/lib/maincopy-recovery/host.toml; chown maincopy:maincopy /var/lib/maincopy-recovery/host.toml; chmod 0600 /var/lib/maincopy-recovery/host.toml")
    machine.succeed("systemctl stop maincopy.service maincopy-litestream.service maincopy-backup.timer")
    machine.succeed("su -s /bin/sh maincopy -c '${restoreCommand}'")
    machine.succeed("test $(stat -c %a /var/lib/maincopy-recovery/state/database/maincopy.db) = 600")
    # Install this accepted checkpoint as the actual module database, with
    # backups enabled. Its marker must be consumed before native replication
    # writes any database sidecars or bookkeeping. Keep the former local state
    # for inspection and start a fresh local replica for the restored database.
    machine.succeed("mv /var/lib/maincopy /var/lib/maincopy-before-recovery; mv /var/lib/maincopy-recovery/state /var/lib/maincopy; mv /var/lib/maincopy-litestream /var/lib/maincopy-litestream-before-recovery")
    machine.succeed("test -f /var/lib/maincopy/database/maincopy.db.restore.json; test ! -e /var/lib/maincopy/database/maincopy.db-wal; test ! -e /var/lib/maincopy/database/maincopy.db-shm")
    machine.succeed("systemctl start maincopy.service")
    machine.wait_for_open_port(3000, timeout=60)
    machine.wait_for_unit("maincopy-litestream.service")
    machine.wait_for_unit("maincopy-backup.timer")
    machine.succeed("test ! -e /var/lib/maincopy/database/maincopy.db.restore.json; test -f /var/lib/maincopy/database/maincopy.db.restore-consumed")
    machine.succeed("curl -fsS http://127.0.0.1:3000/health/ready")
    wait_for_replication()
    machine.succeed("systemctl start maincopy-backup.service")
    machine.succeed("systemctl stop maincopy.service maincopy-litestream.service maincopy-backup.timer")
    machine.succeed("chmod 0644 /var/lib/maincopy/database/maincopy.db")
    machine.succeed("systemctl start maincopy.service || true")
    machine.wait_until_succeeds("systemctl is-failed maincopy.service")
    machine.fail("curl --fail --max-time 2 http://127.0.0.1:3000/health/live")
    machine.succeed("systemctl stop maincopy.service maincopy-litestream.service; chmod 0600 /var/lib/maincopy/database/maincopy.db; systemctl reset-failed maincopy.service; systemctl start maincopy.service")
    machine.wait_for_open_port(3000)
    machine.succeed("systemctl restart maincopy.service")
    machine.wait_for_open_port(3000)
    machine.succeed("! journalctl -u maincopy.service --no-pager | grep -q 'Password:'")
  '';
}
