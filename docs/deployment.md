# NixOS deployment

Use the exported `nixosModules.default` module for a single Maincopy instance.
The module installs the server, CLI, Mermaid worker, SSH helper, and Caddy.
Optional backups use pinned Litestream 0.5.17 and standard rclone crypt with the
native Backblaze B2 backend. Litestream continuously writes a local file replica.
The checkpoint publisher encrypts complete recovery points before upload, with a
one-minute target interval and seven days of local encrypted checkpoint retention.
See [backup and restore](backup-restore.md) for the manifest and acceptance rules.

## Configure the host

```nix
{
  imports = [ inputs.maincopy.nixosModules.default ];

  services.maincopy = {
    enable = true;
    contentRoot = "/srv/maincopy/content";
    public.domain = "www.example.com";

    # Optional: enables an explicitly invoked offline initialization service.
    # Replace this test public key with the owner's public key.
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
    };
  };
}
```

Set the publication's `site.base_url` to the public HTTPS origin. Provision the
content checkout before service startup. Public TLS uses automatic certificate
issuance by default. Configure DNS and the public HTTPS firewall port separately.
The module does not open an administration firewall port.

The default public gateway listens on port 443. The administration gateway uses
port 8443 and loopback addresses by default. An administration bind outside the
accepted loopback, RFC 1918, carrier-grade NAT, or IPv6 unique-local ranges requires
`admin.allowInternet = true`. Public and administration gateway ports must differ.
Use a VPN and private DNS for remote administration. Install the internal Caddy CA
in the administrator's trust store before browser sign-in.

For externally provisioned TLS, set `tls.mode = "provided"` and supply both
`tls.certificateFile` and `tls.privateKeyFile`. These must be runtime **string**
paths, such as `"/run/secrets/admin-key.pem"`. Do not use Nix path literals or
`builtins.readFile` for secrets: those operations can copy bytes into the store.
The module rejects secret paths under `/nix/store` and parent-directory traversal.

The administration and metrics backends always bind to loopback. The public
backend bind is configurable and defaults to loopback. Caddy removes incoming
forwarding and asserted identity headers. It preserves actual authentication,
Origin, CSRF, and idempotency headers. Proxy retries are disabled. The public
origin blocks administration and metrics paths; the administration origin blocks
metrics. Neither gateway accepts an SNI/Host mismatch.

## Initialize the owner offline

Ordinary service startup sets `[identity] startup_bootstrap = "require_existing"`.
A fresh database causes a clear startup failure before credential generation or
listener binding. It cannot print a generated owner password into the journal.
Development configurations retain the existing generated-owner default.

When `initialOwnerPublicKey` is configured, initialize it explicitly:

```sh
sudo systemctl stop maincopy.service maincopy-litestream.service
sudo systemctl start maincopy-initialize.service
sudo systemctl reset-failed maincopy.service
sudo systemctl start maincopy.service
```

`maincopy-initialize` has no boot target. Run it once against an uninitialized
instance. A repeated attempt fails through the existing identity-bootstrap
boundary; normal daemon restarts do not invoke it.

For a password owner, stop the daemon and run the existing offline command from
a protected interactive terminal as the database owner:

```sh
sudo systemctl stop maincopy.service maincopy-litestream.service
sudo -u maincopy maincopyd --config /etc/maincopy/maincopy.toml \
  identity bootstrap password --username owner
sudo systemctl reset-failed maincopy.service
sudo systemctl start maincopy.service
```

The terminal prompts for and confirms the password. Do not place the password in
an argument, environment variable, Nix expression, or journaled service command.

## Provision encrypted B2 backups

Generate a random 256-bit rclone crypt password on the server. Keep an independent
protected recovery copy; losing the key makes the backups unusable. The file must
contain the canonical base64 encoding of exactly 32 bytes, with an optional final
newline. Empty, malformed, symlinked, or group/world-accessible files are rejected.

```sh
sudo install -d -m 0700 /var/lib/maincopy-secrets
sudo sh -c 'set -C; umask 077; head -c 32 /dev/urandom | base64 > /var/lib/maincopy-secrets/backup.cryptkey'
sudo sh -c 'set -C; umask 077; : > /var/lib/maincopy-secrets/b2.conf'
sudoedit /var/lib/maincopy-secrets/b2.conf
```

Creation refuses existing files. Preserve the encryption key when updating credentials.

Use this exact profile shape, replacing the two placeholders:

```ini
[maincopy-b2]
type=b2
account=APPLICATION_KEY_ID
key=APPLICATION_KEY
```

Scope the application key to the dedicated backup bucket. Use a separate prefix
for each site and encryption key. Rclone's native B2 backend uses verified HTTPS.
The closed credential profile rejects alternate backends, endpoint overrides,
extra settings, and empty credentials. Systemd passes credentials as protected
files. The crypt key reaches `rclone obscure` through stdin and is placed only in
a private temporary configuration. Rclone's obscure representation is reversible;
it must receive the same protection as the original key.

The publisher first requests a bounded native `litestream sync -wait` over its
protected Unix socket. It requires confirmation for the configured database and
a replica TXID at least as new as the live synchronized TXID. It then asks native
Litestream for a coherent plan covering that confirmed cutoff and pins finalized
LTX inputs, and captures the retained candidate inventory. It replays the exact
cutoff into a protected temporary SQLite file. Maincopy verifies all required
candidate references and compiles the restored content with the installed package
before generating the complete checkpoint manifest.
This adds local replay and content compilation for each checkpoint; the database itself is never
uploaded. The replay file is removed before ciphertext object processing. Read-only source mounts use file descriptors and
filesystem reflinks when supported, with a bounded copy fallback. Replica garbage
collection cannot invalidate an already pinned input; a file removed before open
causes a bounded replan. Temporary plaintext staging is removed after each run. A stop hook and the next
job startup also remove interrupted staging directories. Completed local
retention contains ciphertext only.

Rclone crypt encrypts both file contents and names before the B2 client sees them.
The publisher reuses immutable ciphertext objects named by their content digest
and verifies both cached and remote ciphertext with `rclone cryptcheck`. It uploads all required
objects before publishing the encrypted versioned manifest. A final encrypted
`latest.json` replacement selects the completed recovery point. B2's completed
object upload is the publication boundary. An interrupted upload leaves the prior
complete checkpoint selected.

Local complete checkpoints retain hardlinks to their exact ciphertext objects.
They remain independently recoverable when the active object cache changes.
Checkpoints older than seven days are removed after a successful publication;
failed backup cycles retain existing recovery points. Native Litestream's local
replica retention is configured separately at seven days. The deployment wrapper accepts at most 17 GiB of combined LTX and candidate
inputs per checkpoint, at most 1 GiB of candidates, and the existing 16 GiB
database limit. This is a smaller operating envelope than the manifest format
can represent. Unit files are limited to 20 GiB each. Provision disk space for
the replica, temporary capture/replay, and the union of retained encrypted objects.
A large database or a slow uplink can increase the effective checkpoint interval.

The module never deletes remote objects. Do not apply a blanket age-based B2
lifecycle rule to shared digest objects: an old object can still be required by a
new checkpoint. Remote garbage collection requires checkpoint-aware inventory
analysis and is deferred. Until that exists, retain objects and budget B2 storage.

The upload timer starts one minute after boot and one minute after the preceding
job finishes. `backup.intervalSeconds` changes that delay (60–3600 seconds). Its recovery-point lag is the capture interval plus upload time.
The maximum job runtime is ten minutes. Each successful job records elapsed
seconds in its journal entry; measure replay, content compilation, and upload cost
with production-sized data before accepting an RPO. A success older than 300 seconds is stale
by default; adjust `backup.staleAfterSeconds` only to an accepted operational RPO.

```sh
sudo systemctl status maincopy-litestream.service
sudo systemctl start maincopy-backup.service
sudo systemctl status maincopy-backup.service
sudo cat /var/lib/maincopy-backup-status/backup-status.json
```

`last_success_at` records the confirmed live-capture time and advances only after
every remote object and the selected complete manifest succeed. A slow upload
therefore cannot make an old cutoff look newly fresh. A stopped, stalled, or
unconfirmed local replica fails before publication. Failures report `degraded` and preserve the preceding successful
timestamp. A healthy local Litestream replica alone cannot report an off-site
success. Public reads continue during replication, encryption, and upload failure.

## Recover a complete checkpoint

Stop the daemon, checkpoint timer, uploader, and Litestream before restoring a
production destination. Keep the original database and state intact until a
separate recovery workspace passes validation. The destination state directory
must satisfy the empty-destination rules in the [restore runbook](backup-restore.md).
Run the helper as the dedicated database UID with a protected host configuration,
protected recovery key, and B2 credential file accessible only to that UID.

```sh
sudo systemctl stop maincopy-backup.timer maincopy-backup.service \
  maincopy.service maincopy-litestream.service
sudo -u maincopy maincopy-restore-checkpoint \
  --config /srv/maincopy-recovery/host.toml \
  --directory /srv/maincopy-recovery/download \
  --key /srv/maincopy-recovery/backup.cryptkey \
  --credentials /srv/maincopy-recovery/b2.conf \
  --bucket my-maincopy-backup --prefix production
```

The download directory must not already exist. The helper decrypts the last
completely published manifest and its exact objects, checks paths and bounds,
and invokes Maincopy's authoritative `verify-checkpoint`. It uses only that
validated maximum TXID for native Litestream replay and passes that exact output
to `restore-replica`. The manifest also binds the expected replayed database
bytes and digest; the SQLite file is not itself an uploaded object. Acceptance verifies the restored database, schema, artifacts,
and first-start marker and revokes restored authentication state. A partial or
rejected download remains in the protected recovery workspace for inspection;
remove it explicitly before retrying.

Use `--checkpoint YYYYMMDDTHHMMSSZ-UUID` to select a recorded older checkpoint.
The local retained directories contain a complete encrypted root, including their
own encrypted `latest.json`; standard rclone crypt can decrypt them with the same
key. The remote helper uses B2 by default. Keep the pinned Litestream and Maincopy
packages with recovery records so a future upgrade does not silently change the
replay or acceptance boundary.

When installing accepted state at the production path, keep all four units
stopped. Preserve the former local Litestream metadata and replica directory
separately, and start with an empty local replica directory for the restored
database. Keep the encrypted checkpoint store and recovery key intact. Starting
`maincopy.service` also starts its replica wrapper and upload timer. The wrapper
waits up to 180 seconds for Maincopy to consume the exact
`maincopy.db.restore.json` marker before it executes native Litestream. An
interrupted or rejected acceptance leaves a `maincopy.db.restore-pending` guard,
which also blocks replication. The gate installs a directory notification watch
before inspecting either path, so it cannot miss a concurrent removal. An
invalid marker, including a broken symlink, causes a visible timeout. The active
wrapper alone is not evidence of replication;
confirm native synchronization and a successful complete checkpoint afterward.

Before relying on production backups, upload to the actual B2 account and run a
recovery on a protected host. Automated fixtures exercise standard cryptography,
publication interruption, replay, and acceptance; they cannot prove access to the
operator's B2 account or the operator's independent recovery key copy.

## Service identities and filesystem boundaries

Maincopy runs as the dedicated `maincopy` user. Caddy runs as
`maincopy-gateway`, with separate state and no access to Maincopy's protected
state or runtime directories. Services use mode 0700 directories and umask 0077.
The live database is `/var/lib/maincopy/database/maincopy.db`, mode 0600.

Litestream and the checkpoint publisher deliberately share the dedicated database
owner's UID. This is a reviewed exception to one UID per service: Maincopy requires
owner access and database mode 0600, and that invariant remains enforced.
Litestream runs separately with access only to the SQLite directory and its own
metadata/replica state. It needs WAL and shared-memory writes for safe checkpoint
coordination. It has no network address family except Unix sockets.

The uploader sees the file replica and retained candidates through read-only
mounts, plus the protected native Litestream control socket needed for synchronous
replication confirmation. That socket exposes Litestream control operations and
is available only inside the uploader and Litestream namespaces under their
dedicated shared UID; it is not a public HTTP endpoint. It cannot access the live database, SSH credentials, or daemon runtime.
The daemon cannot access protected uploader state or runtime crypt credentials;
it reads only the separate backup-status directory. Caddy has a different UID.
Private PID namespaces prevent same-UID access through another service's `/proc`
entries; debug syscalls and core dumps are disabled. Resource and runtime bounds
apply independently to the Litestream and upload units.

Peer state, runtime, and credential directories are hidden by empty read-only
mounts. These mounts also hide directories that a peer creates after the service
starts, including recreated uploader credentials on later checkpoint cycles.
Runtime data lives in `private` children below stable mode 0700 parents. Systemd
removes the children at stop; their parents remain in place so peer mounts cannot
detach during that cleanup. The stable `/run/credentials` parent is hidden as a
whole, with only the current unit's own systemd credentials bound back in.

Systemd supplies the backup secrets through `LoadCredential`. The uploader's
unprivileged preparation step copies them into its private runtime directory as
mode 0600 files. Systemd removes that directory when the unit stops; the daemon
and Litestream cannot see it. This preserves the tools' owner-only file checks
when systemd uses a service-UID ACL to grant credential access. See the
[pinned credential implementation](https://github.com/systemd/systemd/blob/v261.2/src/core/exec-credential.c#L406).

Maincopy, its offline initializer, and the checkpoint validator set
`RestrictSUIDSGID=false`. The pinned systemd implementation otherwise returns
`ENOSYS` for every `openat2` call because seccomp cannot inspect its pointed-to
flags. Maincopy requires that syscall for its safe content resolver; it does not
fall back to weaker path checks. The services retain `NoNewPrivileges`, an empty
capability bounding set, dedicated UID, private writable roots, mount isolation,
and syscall restrictions. Caddy and native Litestream retain
`RestrictSUIDSGID=true`.
See the [systemd 261.2 filter implementation](https://github.com/systemd/systemd/blob/v261.2/src/shared/seccomp-util.c#L2241).
Litestream's optional ownership-copy calls are still denied, but return `EPERM`
so that its existing same-UID replica and replay operations can continue.

The daemon drains accepted public requests and supervised workers before closing
its SQLite writer. Systemd then stops Litestream and permits a final WAL flush.
The publisher never stops or restarts the live daemon. The offline initialization
unit stops the daemon, Litestream, uploader, and upload timer before bootstrap.
Starting the daemon afterward automatically starts Litestream and resumes the
configured upload timer; no separate Litestream restart is required.

For managed Git, set `source.managed = true` and configure named
`source.credentials.<name>.privateKeyFile` and `knownHostsFile` runtime paths.
Systemd delivers their contents through credentials. The daemon receives copies
owned by its service UID with mode 0600; the backup and gateway namespaces hide
those files. See the [managed Git runbook](managed-source.md).

## Deployment checks

The `deployment-module` flake check evaluates valid and unsafe configurations,
validates the generated Caddyfile, and runs real Litestream replication/replay and
rclone crypt. Its B2 fixture checks ciphertext at the transport boundary. Tests
cover interrupted publication preserving the previous recovery point, corrupted
cached ciphertext, protected key/profile validation, and bounded control output.

The `deployment-vm` check boots the actual daemon, Caddy, Litestream, and uploader.
It checks explicit owner initialization, fail-closed startup, gateway boundaries,
database permissions, encrypted publication, interrupted upload, native replay,
Maincopy restore acceptance, and a successful first start with the accepted
database installed at the module's actual path and backups enabled.
Only the external B2 transport is replaced. Run the real B2 acceptance above
before relying on off-site recovery.
