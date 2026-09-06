# Backup and offline restore

Maincopy recovers from complete encrypted Litestream checkpoints.
Litestream writes its continuous replica to protected local storage.
The host uses standard rclone crypt to encrypt checkpoint objects before Backblaze B2 receives them.

The checkpoint upload scheduling target is one minute.
Backup health becomes stale when the latest complete off-site checkpoint's confirmed capture time is more than five minutes old.
Every cycle performs local SQLite replay and content compilation. Large histories can exceed one minute; measure cycle duration and adjust scheduling and freshness settings together.
Keep completed encrypted checkpoints locally for seven days. Local cleanup does not delete B2 objects.
See [deployment](deployment.md) for service configuration and protected credential paths.

## Complete checkpoint boundary

A database replica alone cannot recover private content artifacts.
Each complete checkpoint includes a pinned native Litestream restore plan and every retained candidate available after that plan was selected.

Maincopy retains candidate archives before committing database references to them.
Automatic artifact collection remains disabled. Existing candidate archives are immutable.
These rules make artifact capture after the selected transaction cutoff safe during normal operation.
Checkpoint validation also reconstructs the pinned database and its public content before publication.
A missing or corrupt required archive prevents the manifest from being created.

The host performs these operations in order:

1. Confirm the configured database's replica with bounded native `sync -wait`. Record the confirmation time.
2. Ask native Litestream for a bounded `restore -dry-run -json` plan covering the confirmed transaction cutoff.
3. Pin the exact completed LTX files named by that plan.
4. Capture the immutable candidate inventory after selecting the plan.
5. Replay the pinned plan with native Litestream into a private temporary database at the selected cutoff.
6. Run `checkpoint-manifest --database-file ...` to validate that database, compile its retained content, and bind exact file hashes.
7. Delete the temporary database. Encrypt and upload every referenced object through rclone crypt, then verify remote ciphertext.
8. Upload the encrypted versioned manifest, then replace encrypted `latest.json` to select the complete checkpoint.
9. Complete local encrypted retention. Record the confirmed capture time in the successful health report.

Only a complete checkpoint can replace the previous recovery point.
A recently uploaded LTX file does not by itself advance complete backup health.

The decrypted checkpoint layout is:

```text
checkpoint.json
ltx/
  <level>/<minimum-transaction>-<maximum-transaction>.ltx
content-candidates/
  <content-digest>.candidate
```

The manifest format is `maincopy-litestream-checkpoint-v1`.
It records the exact package and schema identities, replayed database identity, transaction range, selected LTX files, and complete candidate inventory.
The database identity binds byte count and digest of the verified native replay output; the database itself is not an uploaded object.
File identities contain a byte count and a 64-character lowercase hexadecimal BLAKE3 digest.

LTX transaction identifiers contain 16 lowercase hexadecimal digits.
The plan must start at transaction one and progress without gaps to its declared maximum.
Native compaction overlap is permitted when each file advances the recovered transaction range.

Limits are 4,096 LTX files, 32 GiB per LTX file, and 64 GiB across the plan.
Candidates are limited to 4,096 archives and 1 GiB combined. Manifests are limited to 2 MiB.
The recovered SQLite database limit is 16 GiB.
These are format limits. The [NixOS deployment helper](deployment.md) limits combined LTX and candidate inputs to 17 GiB.

The host stores encrypted objects under content-derived identifiers and publishes immutable checkpoint manifests.
Retain the checkpoint manifest and all referenced encrypted objects as one recovery unit.
Do not delete shared objects while any retained checkpoint references them.

## Checkpoint commands

The host stages the exact native plan files before creating a manifest:

```console
maincopyd --config /etc/maincopy/maincopy.toml checkpoint-manifest \
  --database-file /var/lib/maincopy-backup/staging/database.sqlite3 \
  --plan-file /var/lib/maincopy-backup/staging/plan.json \
  --ltx-root /var/lib/maincopy-backup/staging/ltx \
  --artifact-root /var/lib/maincopy-backup/staging/content-candidates \
  --output /var/lib/maincopy-backup/staging/checkpoint.json
```

The command refuses an existing output file.
It verifies the direct native replay output and retained content without binding listeners or acquiring the running daemon's runtime lock.
The temporary database is never uploaded. Compilation uses the staged candidate directory and the renderer's private temporary workspace.
Standard error receives diagnostics. The generated manifest contains no source host paths or credentials.

After downloading and decrypting one complete checkpoint, verify its exact inventory:

```console
maincopyd --config /etc/maincopy/maincopy.toml verify-checkpoint \
  --manifest-file /var/lib/maincopy-recovery/checkpoint.json \
  --ltx-root /var/lib/maincopy-recovery/ltx \
  --artifact-root /var/lib/maincopy-recovery/content-candidates
```

Successful standard output contains only the validated replay cutoff:

```json
{"max_txid":"0000000000000004"}
```

Pass this value to the pinned native `litestream restore -txid` command.
Use the directory containing the verified `ltx/` subtree as the file replica source.
Use a new owner-only output database and require native restore to succeed.

`restore-replica` must receive that direct native output, before any application writes:

```console
maincopyd --config /etc/maincopy/maincopy.toml restore-replica \
  --database-file /var/lib/maincopy-recovery/database.sqlite3 \
  --artifact-root /var/lib/maincopy-recovery/content-candidates \
  --manifest-file /var/lib/maincopy-recovery/checkpoint.json \
  --ltx-root /var/lib/maincopy-recovery/ltx
```

Maincopy rechecks the checkpoint inventory and requires supplied SQLite bytes to match the manifest's verified replay output, then validates the ledger and referenced content.
Native Litestream replay establishes the relationship between the selected LTX files and the output database.
Maincopy does not independently replay LTX. The recorded replay digest also rejects an otherwise valid database from a different checkpoint.

## Offline acceptance

Stop the daemon and backup services before restoring into deployment paths.
Keep the original data and ciphertext until the restored service passes verification.
Use an empty destination state directory and protected temporary decryption storage.

The configured database must be inside the configured state directory.
Nested database directories are supported, including `/var/lib/maincopy/database/maincopy.db`.
Acceptance refuses a nonempty destination and never overwrites existing state.

Acceptance proceeds in this order:

1. Verify the supplied manifest, exact package, and complete input inventory.
2. Create a durable pending guard before copying into empty state.
3. Recheck copied database bytes and artifacts against the verified input identities.
4. Verify SQLite integrity, foreign keys, migration checksums, and every actual schema object.
5. Validate identities, profiles, release history, route ownership, and retained release inputs.
6. Rebuild public output and recoverable activations with the eligible tip projection.
7. Revoke restored browser sessions and agent credentials in one accepted transaction.
8. Checkpoint and close SQLite, then write the one-use acceptance marker.

Schema comparison includes SQLite internal names and generated indexes.
Only the exact pinned Litestream bookkeeping tables extend the migration-generated schema.
Their committed lock table must be empty; sequence rows must have the native valid shape.
Unknown tables, columns, indexes, and triggers fail acceptance.

The marker is `<database>.restore.json`.
It binds accepted database bytes, the complete artifact inventory, schema, and executable bytes.
Its database identity includes credential revocation.

Start the same executable with the accepted host configuration.
Before ordinary database writes, startup verifies and renames the marker to `<database>.restore-consumed`.
Changed database, binary, schema, or artifact bytes fail this first-start check.
Offline identity and source commands cannot consume the marker.

A remaining `<database>.restore-pending` guard means acceptance did not finish.
Startup refuses that candidate. Preserve diagnostics and retry into a new empty destination from the original checkpoint.

Verify published pages, RSS, profiles, and Lightning tips after startup.
Old browser sessions and agent credentials must fail.
Sign in again and register replacement agent credentials through normal administration.
Remove decrypted recovery staging after verification.

## Secrets and health

Keep independent protected recovery copies of SSH keys, pinned host keys, B2 credentials, encryption credentials, and the matching server package.
Keep encryption recovery credentials offline as well as on the server. Losing them prevents decryption.
Missing or incorrect decryption credentials must stop recovery before Maincopy accepts any state.

The health report is an owner-only regular file, bounded to 4 KiB:

```toml
[backup]
status_file = "/var/lib/maincopy-backup-status/backup-status.json"
stale_after_seconds = 300
```

```json
{"format":"maincopy-backup-status-v1","state":"healthy","last_success_at":"2026-09-06T03:00:00Z"}
```

Only a complete encrypted off-site checkpoint advances `last_success_at`.
The recorded time is the successful native `sync -wait` confirmation for the selected database, before capture and upload.
A slow upload does not reset the data-freshness clock to its later completion time.
The daemon receives read-only access to the separate health directory; backup credentials remain outside that directory.
A failed job reports `degraded` and can preserve the last successful timestamp.
Missing, malformed, unsafe, stale, or future-dated reports produce degraded health.
Valid freshness allowances range from 60 seconds through seven days.

Metrics expose `maincopy_backup_healthy` and `maincopy_backup_last_success_timestamp_seconds`.
Backup degradation does not change public readiness or terminate the server.

## Portable full-snapshot export

A separate manual export pairs a consistent SQLite snapshot with exact retained archives.
Provision an operator-owned age recipient file for this manual workflow and retain its private recovery key offline.
The rclone crypt deployment does not create age credentials or install the optional SQLite CLI and age tools.
Provide those tools before starting this workflow.
Run these commands in Bash as the dedicated database owner on the daemon host:

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

SQLite's online backup produces a consistent snapshot while the daemon can remain running.
The read-only source URI prevents this command from creating a missing live database.
Only promote ciphertext after both export and encryption succeed.
The tar stream contains `database.sqlite3`, `manifest.json`, and `content-candidates/*.candidate`, with mode `0600`.
The `maincopy-backup-v1` manifest also binds the original snapshot bytes.
Never substitute a copy of the live SQLite main file for a consistent online snapshot.
After validating the encrypted backup, remove its temporary plaintext SQLite snapshot.
Remove failed staging directories explicitly; the continuous checkpoint cleanup does not manage manual exports.

Decrypt and extract the trusted bundle into protected staging.
Use `restore --database-file ... --artifact-root ... --manifest-file ...` for this paired format.
Acceptance rechecks both copied database bytes and copied artifacts against the original manifest.
The paired format is independent of the continuous checkpoint upload path.

## Recovery drill evidence

Run the portable export process fixture with the packaged renderer and SSH helper available:

```console
cargo test -p maincopy-server --test api \
  an_exported_backup_restores_released_pages_feed_profiles_tips_and_revokes_old_sessions \
  -- --nocapture
```

The fixture compares released page, feed, and sitemap bytes and ETags after restore.
It preserves the profile and eligible tip recipient and rejects the old browser session.

A local drill on 2026-09-06 passed with zero acknowledged fixture changes lost and 1.171 seconds from acceptance through listener startup.
The development test binary completed the fixture in 5.07 seconds. Download and decryption were excluded.
These measurements cover the portable bundle path, not an off-site continuous-checkpoint drill.

Record the package, selected checkpoint, restored transaction cutoff, elapsed recovery time, off-site object names, and verification date for deployment drills.
The one-minute upload interval is a target. Actual recovery point and recovery time follow the latest complete checkpoint and measured procedure.

Automatic artifact collection remains disabled.
Collection must account for installed source, private previews, public output, nonterminal releases, active source proposals, and every retained recovery manifest.

The checkpoint publication fixture also rejects both missing and corrupt required candidate archives before any manifest exists.
With intact inputs, schema, identity, retained-content compilation, and hashing completed in 245 ms for that small fixture.
This measurement excludes native SQLite replay, encryption, and upload and is not a production throughput guarantee.
