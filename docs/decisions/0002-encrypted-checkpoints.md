# Continuous replication with encrypted recovery checkpoints

Status: implemented; production acceptance remains in the [backlog](../implementation.md).

## Decision

Use pinned Litestream 0.5.17 for continuous replication into a protected local
file replica. Use standard rclone crypt and its native B2 backend for encrypted
off-site storage. Keep complete encrypted checkpoints locally for seven days.

The owner selected continuous replication to establish a reusable operating
pattern. Current Litestream releases removed native age encryption. The standard
rclone crypt layer encrypts bytes on the source server before B2 receives them.
Maincopy implements no cryptographic format.

## Complete recovery points

A replicated ledger alone cannot reconstruct publication. Its retained candidate
artifacts must match the selected transaction cutoff.

The publisher confirms native sync, pins an immutable restore plan, and replays
that plan into a private database. Maincopy verifies schema, identity, and retained
content before producing a manifest. The manifest also binds the replayed database
digest and the exact Maincopy binary. The private database is then discarded.

Only encrypted LTX files and content artifacts become upload objects. The publisher
sends the encrypted manifest and updates the latest selection after those objects
reach B2. Failed uploads preserve the previous complete recovery point.

Health records the confirmed capture time only after successful publication and
durable local retention. The timer waits one minute after each job by default.
Actual recovery lag includes replay, validation, encryption, and upload time.

## Connection and process ownership

Maincopy domain mutations retain one SQLx writer and one bounded command channel.
Litestream owns separate SQLite connections, its two exact bookkeeping tables,
and WAL checkpoint coordination. This exception is required by native replication;
Litestream cannot submit its internal operations through Maincopy's typed commands.
Restore schema validation permits only the pinned bookkeeping table definitions.

The daemon, Litestream, and publisher run in separate systemd mount namespaces.
They share the dedicated database owner where mode `0600` requires it. Litestream
sees the database but no remote credentials. The publisher sees a read-only replica,
content artifacts, and the native control socket, but cannot open the live database.
Only the publisher receives B2 credentials and the crypt key.

The Python process adapter uses stock rclone configuration and command interfaces.
Python cannot guarantee in-memory zeroization of immutable secret strings. Keep
that exception confined to the short-lived publisher: bounded credential reads,
protected runtime files, suppressed child diagnostics, and no secret arguments.
The Rust daemon and CLI retain their dedicated zeroizing secret types.

## Dependencies and operating limits

`prometheus` supplies explicit registries with default features disabled.
`tar` supplies the existing portable recovery archive format with default features
disabled. Their pinned licenses are Apache-2.0 and MIT OR Apache-2.0 respectively.

The deployment bounds a checkpoint to 17 GiB. The Rust format admits larger
bounded inventories for offline tools. Remote objects remain immutable; the local
seven-day cleanup does not apply a remote deletion policy. Operators must preserve
every object referenced by a retained remote manifest.

The [deployment](../deployment.md) and [recovery](../backup-restore.md) runbooks define
provisioning, key retention, restore acceptance, and measured recovery evidence.

## Upstream references

- [Litestream encryption migration](https://litestream.io/docs/migration/#age-encryption-migration)
- [Litestream 0.5.17](https://github.com/benbjohnson/litestream/releases/tag/v0.5.17)
- [rclone crypt](https://rclone.org/crypt/)
