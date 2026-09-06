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
    import configparser, os, pathlib, sys
    args = sys.argv[1:]
    assert "fixture-secret-key" not in " ".join(args)
    offsite = "/var/lib/maincopy-backup/offsite-fixture"
    if any(value.startswith("maincopy-b2:") for value in args):
        if pathlib.Path("/var/lib/maincopy-backup/interrupt-upload").exists():
            sys.exit(41)
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
        config["offsitecrypt"]["remote"] = offsite
        with path.open("w") as output:
            config.write(output)
    os.execv("${pkgs.rclone}/bin/rclone", ["${pkgs.rclone}/bin/rclone", *args])
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
        "--directory"
        "/var/lib/maincopy-backup"
        "--replica"
        "/var/lib/maincopy-litestream/replica"
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
      chmod 0600 /var/lib/fixture-secrets/*
    '';
  };
  testScript = ''
    import json

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
    machine.wait_until_succeeds("find /var/lib/maincopy-litestream/replica -name '*.ltx' | grep .")
    assert_late_created_secret_paths_are_hidden()
    machine.succeed("systemctl start maincopy-backup.service")
    assert_late_created_secret_paths_are_hidden()
    machine.succeed("curl -fsS http://127.0.0.1:3000/health/live")
    report = json.loads(machine.succeed("cat /var/lib/maincopy-backup-status/backup-status.json"))
    assert report["state"] == "healthy" and report["last_success_at"]
    machine.succeed("test $(stat -c %a /var/lib/maincopy-backup-status/backup-status.json) = 600")
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
