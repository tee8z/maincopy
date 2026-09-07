# Deploy Maincopy on NixOS

Use `nixosModules.default` for a single instance with Caddy, private administration, and optional encrypted Backblaze B2 backups.
See [email setup](email-delivery.md) for SES and [monitoring](observability.md) for Prometheus and Grafana.

## Configure the host

Pin Maincopy in the host flake. Pass its inputs through `nixpkgs.lib.nixosSystem` using `specialArgs = { inherit inputs; };`.

```nix
{ inputs, ... }:
{
  imports = [ inputs.maincopy.nixosModules.default ];

  services.maincopy = {
    enable = true;
    contentRoot = "/srv/maincopy/content";
    public.domain = "www.example.com";

    # Replace this test key with the owner's public key, or omit for password bootstrap.
    initialOwnerPublicKey =
      "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9";

    admin = {
      domain = "admin.example.internal";
      listenAddresses = [ "10.20.30.40" ];
      port = 8443;
      tls.mode = "internal";
    };

    backup = {
      enable = true;
      keyFile = "/var/lib/maincopy-secrets/backup.cryptkey";
      credentialsFile = "/var/lib/maincopy-secrets/b2.conf";
      bucket = "my-maincopy-backup";
      prefix = "production";
      epochSeconds = 86400;
      localRetentionDays = 7;
      remoteRetentionDays = 9;
    };
  };
}
```

Provision the content checkout and set publication `site.base_url` to the public HTTPS origin.
Configure DNS and the public HTTPS firewall port. Public TLS uses automatic certificates by default.

Administration defaults to loopback port 8443; the module opens no administration firewall port.
Use a VPN and private DNS for remote access. Trust Caddy's internal CA before browser sign-in.
An administration bind outside accepted private ranges requires `admin.allowInternet = true`; public and administration gateway ports must differ.
Public traffic cannot reach administration or metrics, and Caddy rejects SNI/Host mismatches and asserted identity headers.

For existing TLS certificates, set `tls.mode = "provided"`, `tls.certificateFile`, and `tls.privateKeyFile` on the relevant gateway.
All secret paths must be runtime strings, such as `"/run/secrets/admin-key.pem"`.
Never use Nix path literals or `builtins.readFile` for secrets. The module rejects `/nix/store` paths and parent traversal.

For managed Git, set `source.managed = true` and configure `source.credentials.<name>.privateKeyFile` and `knownHostsFile`.
See the [managed Git runbook](managed-source.md).

## Stop the services

Before bootstrap, restore, or replacement of database state, stop all writers and backup jobs:

```sh
sudo systemctl stop maincopy-backup.timer maincopy-backup.service \
  maincopy-backup-expire-local.timer maincopy-backup-expire-remote.timer \
  maincopy-backup-expire-local.service maincopy-backup-expire-remote.service \
  maincopy.service maincopy-litestream.service
```

Omit backup unit names when backups are disabled.
Ordinary Maincopy shutdown drains accepted requests and workers before closing SQLite; Litestream then flushes its final WAL.

## Initialize the owner offline

The module requires an existing owner. A fresh database fails startup without generating a password or binding listeners.
After [stopping the services](#stop-the-services), use one bootstrap method.

For the configured `initialOwnerPublicKey`:

```sh
sudo systemctl start maincopy-initialize.service
```

For a password owner, use a protected interactive terminal:

```sh
sudo -u maincopy maincopyd --config /etc/maincopy/maincopy.toml \
  identity bootstrap password --username owner
```

Enter and confirm the password at the prompt. Never place it in arguments, environment variables, Nix expressions, or journals.
Bootstrap refuses an initialized instance and does not run on ordinary restarts.

```sh
sudo systemctl reset-failed maincopy.service
sudo systemctl start maincopy.service
```

Starting Maincopy also starts configured replication and backup timers.

## Provision encrypted B2 backups

Generate the encryption key once and keep an independently protected recovery copy. Losing it makes backups unusable.

```sh
sudo install -d -m 0700 /var/lib/maincopy-secrets
sudo sh -c 'set -C; umask 077; head -c 32 /dev/urandom | base64 > /var/lib/maincopy-secrets/backup.cryptkey'
sudo sh -c 'set -C; umask 077; : > /var/lib/maincopy-secrets/b2.conf'
sudoedit /var/lib/maincopy-secrets/b2.conf
```

Creation refuses existing files. Preserve the encryption key when replacing B2 credentials.
The key is canonical base64 for exactly 32 random bytes, optionally followed by a newline.
Keys and credential files must be private regular files; malformed, symlinked, or group/world-accessible inputs are rejected.

Use this exact profile, replacing its placeholders:

```ini
[maincopy-b2]
type=b2
account=APPLICATION_KEY_ID
key=APPLICATION_KEY
```

Use a dedicated bucket-scoped application key with file listing, reading, writing, deletion, and bucket lifecycle read permissions.
Use a separate prefix for each site and encryption key. Alternate backends, endpoints, and additional credential fields are rejected.

Configure one whole-bucket B2 lifecycle rule before activation:

| Field | Default |
| --- | ---: |
| `fileNamePrefix` | Empty string |
| `daysFromUploadingToHiding` | 9 |
| `daysFromHidingToDeleting` | 1 |
| `daysFromStartingToCancelingUnfinishedLargeFiles` | 1 |

The publisher reads this rule and refuses a mismatch; it never changes bucket policy.
Verify that Object Lock or legal holds cannot prevent deletion. Lifecycle validation alone cannot establish this.
See [B2 lifecycle rules](https://www.backblaze.com/docs/cloud-storage-lifecycle-rules) and [Object Lock](https://www.backblaze.com/docs/cloud-storage-object-lock).

### Finite epochs and expiration

Each Litestream start creates a fresh replica under `/var/lib/maincopy-litestream/active`.
Systemd restarts only Litestream before the 24-hour upload window ends, reserving ten minutes for an admitted publisher.
The website stays running. Old and new epochs share no checkpoint objects.

Native replica state is plaintext in private storage. Rclone crypt encrypts checkpoint contents and file names before B2 receives them.
Remote epochs use `PREFIX/epochs/UTC-UUID`; an encrypted `latest.json` selector identifies the last complete checkpoint.
Interrupted publication leaves the previous selector intact.

| Retained copy | Default expiration |
| --- | --- |
| Complete local encrypted checkpoint | Seven days; hourly cleanup |
| Epoch cache and retired native state | Eight days from epoch start, preserving the final checkpoint's dependency window |
| Remote epoch, including historical versions and unfinished uploads | Same eight-day boundary; separate hourly B2 cleanup |
| B2 lifecycle backstop | Nine days from upload to hiding, then one day to deletion |

Local expiration needs no credentials and continues during upload failure. An OS lock protects running native state from cleanup after a clock jump.
Remote expiration uses [B2 purge and cleanup](https://rclone.org/b2/#versions); it requires credentials but no encryption key or database access.
Provider lifecycle continues when the host is unavailable. An old page can be uploaded until its epoch closes.
The default provider limit therefore includes one epoch plus ten days, followed by provider processing delays.

Host downtime delays local cleanup. Provider outages, holds, snapshots, and unmanaged copies prevent promises of immediate physical erasure.
Assign separate deletion deadlines to manual exports and operator copies.
Before subscriber capture, accept actual retention settings and complete an off-site recovery drill.

`backup.epochSeconds` accepts 3600–86400 seconds; `localRetentionDays` accepts 1–30 days.
`remoteRetentionDays` must be at least two days longer than local retention and cannot exceed 90 days.

For a legacy unscoped replica, [stop services](#stop-the-services) before migrating `metadata` and `replica` directories.
Inventory old copies and remove or relocate them with explicit deletion deadlines.
Use a fresh remote prefix and configure finite lifecycle deletion for legacy objects; epoch cleanup does not manage them.

Epoch expiration does not erase historical live WAL, filesystem snapshots, or storage media.
SQLite `secure_delete` clears current deleted database content; native PASSIVE checkpoints do not guarantee a physical WAL wipe.
Do not run external `PRAGMA wal_checkpoint(TRUNCATE)` during replication; Litestream's coordination protects incremental history.
See [Litestream checkpoint coordination](https://github.com/benbjohnson/litestream/blob/v0.5.17/db.go) and [SQLite secure deletion](https://www.sqlite.org/pragma.html#pragma_secure_delete).

## Verify backup operation

Before an immediate backup after startup or restore, confirm replication:

```sh
sudo systemctl status maincopy-litestream.service
sudo timeout 15s litestream sync -wait -json -timeout 10 \
  -socket /run/maincopy-litestream/private/control.sock \
  /var/lib/maincopy/database/maincopy.db
```

Require successful synchronization, the expected `db_path`, a positive `txid`, and `replica_txid >= txid`.
An active service or ready website alone does not prove replication. If synchronization fails, inspect the Litestream journal before publishing.

```sh
sudo systemctl start maincopy-backup.service
sudo systemctl status maincopy-backup.service
sudo cat /var/lib/maincopy-backup-status/backup-status.json
```

The upload timer runs one minute after boot and after each completed job; `backup.intervalSeconds` accepts 60–3600 seconds.
Each job has a ten-minute deadline. Measure replay, compilation, encryption, and upload with production-sized data before accepting recovery-point lag.
See [backup health](backup-restore.md#secrets-and-health) for freshness and failure diagnosis.

## Recover a complete checkpoint

Follow [the recovery procedure](backup-restore.md#recover-a-complete-checkpoint) with the matching package and an independent key copy.
Before relying on production backups, restore a checkpoint from the actual B2 account on a protected host.
Local fixtures cannot prove account access or key recoverability.

## Service identities and filesystem boundaries

Maincopy uses the `maincopy` UID, private mode 0700 directories, and mode 0600 database files.
Caddy uses a separate `maincopy-gateway` UID. Other units share the database owner UID but have isolated mounts and PID namespaces:

| Unit | Allowed data |
| --- | --- |
| Maincopy | Application state, its own runtime credentials, read-only backup health |
| Litestream | Database directory, native replica state, protected synchronization socket |
| Checkpoint publisher | Read-only replica and content artifacts; encrypted backup state and temporary credentials |
| Local expiration | Local checkpoint and replica state; no credentials or network |
| Remote expiration | Encrypted backup state and B2 credentials; no live database or encryption key |
| Caddy | Gateway state and TLS credentials; no application state |

Litestream's SQLite bookkeeping and checkpoint connections cannot use the application writer; this separate native connection boundary is intentional.
Peer mounts hide credentials even when another unit creates or replaces its runtime directory later.
Systemd credentials are copied into private service-owned files, then removed with the service's runtime directory.

Python backup helpers cannot guarantee zeroization of immutable buffers. They use bounded reads, private temporary configuration, no secret arguments, and suppressed child diagnostics.
Rclone's obscure representation is reversible and remains protected as a secret.

Maincopy and checkpoint validation disable `RestrictSUIDSGID` because pinned systemd otherwise blocks the required `openat2` resolver.
Other namespace, privilege, and syscall restrictions remain enabled; native ownership-copy calls return `EPERM` instead of killing Litestream.
Keep these module settings when adapting the deployment. See the [systemd filter implementation](https://github.com/systemd/systemd/blob/v261.2/src/shared/seccomp-util.c#L2241).
