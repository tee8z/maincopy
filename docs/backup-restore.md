# Backup and offline restore

Recover Maincopy from complete encrypted Litestream checkpoints, including the database and retained content artifacts.
Use [deployment](deployment.md#provision-encrypted-b2-backups) to configure B2 credentials, services, encryption, and retention.
Subscriber tables enter these checkpoints; restore must apply the [mail recovery safeguards](email-delivery.md#recover-feedback-or-restored-data).

## Complete checkpoint boundary

A database replica alone cannot recover content artifacts. A complete checkpoint contains:

```text
checkpoint.json
ltx/<level>/<minimum-transaction>-<maximum-transaction>.ltx
content-candidates/<content-digest>.candidate
```

The publisher confirms native replication, pins a coherent LTX plan, and captures immutable candidate archives.
It replays and validates the database and content locally before encryption. Missing or corrupt required archives prevent publication.
Only after all encrypted objects and the versioned manifest succeed does the encrypted `latest.json` selector advance.
An interrupted upload preserves the preceding complete recovery point.

The `maincopy-litestream-checkpoint-v1` manifest binds exact package, schema, database replay bytes, and artifact identities.
Keep its referenced objects together. Epochs share no objects; [expiration](deployment.md#finite-epochs-and-expiration) preserves their dependency windows.

The deployment helper accepts at most 17 GiB of combined LTX and candidate inputs, including at most 1 GiB of candidates.
The database limit is 16 GiB and individual service files are limited to 20 GiB.
Provision disk space for active and retired native replicas, temporary replay, and retained ciphertext.
Automatic artifact collection remains disabled.

## Recover a complete checkpoint

Keep the original database and ciphertext until a separate recovery workspace passes verification.
Use the matching Maincopy and Litestream packages and independently recovered encryption credentials.
Run recovery as the dedicated `maincopy` UID; its configuration, key, and B2 profile must be readable only by that UID.

Prepare `/srv/maincopy-recovery` as a mode 0700 directory owned by `maincopy`.
Copy the host configuration and set `paths.state_root`, `paths.runtime_root`, and `database.path` to a new recovery destination.
The state destination must be empty and contain the configured database path. The download directory must not exist.

[Stop all services and backup jobs](deployment.md#stop-the-services), then run:

```sh
sudo -u maincopy maincopy-restore-checkpoint \
  --config /srv/maincopy-recovery/host.toml \
  --directory /srv/maincopy-recovery/download \
  --key /srv/maincopy-recovery/backup.cryptkey \
  --credentials /srv/maincopy-recovery/b2.conf \
  --bucket my-maincopy-backup --prefix production
```

To select an older checkpoint, supply both `--epoch YYYYMMDDTHHMMSSZ-UUID` and `--checkpoint YYYYMMDDTHHMMSSZ-UUID`.
The helper decrypts and verifies the exact inventory, replays native LTX, then passes that exact database to offline acceptance.
It refuses to merge into existing state. Failed downloads remain in protected staging; remove them before retrying.

Local retained checkpoints are also complete encrypted roots, with their own encrypted selector and `epochs/UTC-UUID` directory.
The packaged helper supports B2; it does not automate local ciphertext recovery.
That manual path requires selector and epoch decryption, manifest verification, native LTX replay, and acceptance of the exact replay output.
A complete local-only operator recovery drill remains [acceptance work](implementation.md).

### Install accepted state

Keep services stopped while replacing production state. Preserve the former database and native replica separately, with recorded deletion deadlines.
Install the accepted state at the configured production path and start with an empty local Litestream replica directory.
Keep the encrypted checkpoint store and recovery key intact.

```sh
sudo systemctl start maincopy.service
```

The replica wrapper waits up to 180 seconds for Maincopy to consume its acceptance marker before native writes begin.
Follow [replication verification](deployment.md#verify-backup-operation), then publish and confirm a new complete checkpoint.
Verify pages, RSS, profiles, and Lightning tips. Old browser sessions and agent credentials must fail; register replacements through normal administration.
Restored subscribers must not receive mail; follow the mail recovery procedure before enabling new enrollment.
Remove decrypted staging after verification.

Record the package, checkpoint, transaction cutoff, off-site object names, recovery duration, and verification date.
Local fixtures do not replace a recovery drill from the actual B2 account.

## Offline acceptance

Acceptance verifies exact package and schema, input hashes, SQLite integrity, authentication state, and retained content before authorizing normal startup.
Unknown schema objects fail acceptance; only pinned Litestream bookkeeping extends the migration-generated schema.
It revokes restored authentication and subscriber eligibility, rotates the mail epoch, and quarantines unfinished campaigns in one transaction.

The `<database>.restore.json` marker binds accepted database bytes, artifacts, schema, and executable bytes.
Start the same executable with the accepted configuration. Before ordinary writes, startup verifies and renames the marker to `<database>.restore-consumed`.
Changed bytes fail this check. Offline identity and source commands cannot consume the marker.

A remaining `<database>.restore-pending` guard means acceptance did not finish.
A rejected or broken marker also blocks startup and replication. Preserve diagnostics and retry from the original checkpoint into a new empty destination.
Never edit the marker or database to bypass acceptance.

## Secrets and health

Keep protected recovery copies of encryption and subscriber-control keys, B2 credentials, SSH trust anchors, and the matching server package.
Keep an encryption key copy offline. Missing or incorrect keys prevent recovery.

The Nix module configures the backup report automatically. For direct server configuration:

```toml
[backup]
status_file = "/var/lib/maincopy-backup-status/backup-status.json"
stale_after_seconds = 300
```

The freshness setting accepts 60 seconds through seven days; Nix exposes it as `backup.staleAfterSeconds`.
Set it to the accepted checkpoint interval plus publication delay.
`last_success_at` records the native synchronization confirmation before capture and upload, and advances only after complete remote publication.
Slow uploads cannot make old data look newly captured.

| Signal | Action |
| --- | --- |
| Report is `healthy` and fresh | Confirm scheduled publication continues; a healthy local replica alone is insufficient. |
| Report is `degraded` | Inspect `maincopy-backup.service` and its journal; publication can preserve the previous successful timestamp. |
| Report is stale or missing | Check timers, credentials, mounts, and failed unit setup. Setup failures occur before the publisher can update its report. |
| Report is malformed, unsafe, or future-dated | Correct the protected report path, file ownership, and host clock; do not manufacture a successful timestamp. |

Unconfigured backups also report degraded health. Backup failure does not stop public reads.
[Metrics](observability.md) expose `maincopy_backup_healthy` and `maincopy_backup_last_success_timestamp_seconds`.

## Portable full-snapshot export

For a manual encrypted bundle, provision an age recipient file and retain its private key offline.
Install the optional SQLite CLI and age tools; the continuous backup module does not provide this workflow's credentials or tools.
Run in Bash as the database owner:

```bash
set -euo pipefail
umask 077
BACKUP_STAGE="$(mktemp -d /var/lib/maincopy-backup/manual.XXXXXXXX)"
sqlite3 'file:/var/lib/maincopy/database/maincopy.db?mode=ro' \
  ".backup '$BACKUP_STAGE/database.sqlite3'"
maincopyd --config /etc/maincopy/maincopy.toml export-backup \
  --database-file "$BACKUP_STAGE/database.sqlite3" \
  | age -R /run/maincopy-manual-backup/age-recipients \
      -o "$BACKUP_STAGE/recovery.tar.age.partial"
mv -- "$BACKUP_STAGE/recovery.tar.age.partial" "$BACKUP_STAGE/recovery.tar.age"
```

SQLite's online backup is consistent while the daemon runs. Never substitute a copy of the live SQLite main file.
Only promote ciphertext after export and encryption succeed. After validation, remove the temporary plaintext snapshot.
Manual exports and failed staging require explicit cleanup; continuous checkpoint expiration does not manage them.

Decrypt the trusted bundle into protected staging. Its `maincopy-backup-v1` manifest pairs `database.sqlite3` with exact `content-candidates` archives.
Use `maincopyd --config ... restore --database-file ... --artifact-root ... --manifest-file ...`; the same offline acceptance rules apply.
