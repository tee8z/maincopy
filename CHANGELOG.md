# Changelog

## Unreleased

Changes prepared for the first release. The release version, distribution channels,
and production acceptance remain pending.

### Added

- Browser administration and an HTTPS CLI for human accounts, password and Nostr
  credentials, profiles, roles, account status, and scoped agent grants.
- Human Nostr sign-in through an external signer, with challenge binding and
  protected session storage separate from agent keys.
- Managed Git synchronization through constrained SSH, retained content
  candidates, exact private previews, and source configuration controls.
- Immediate and scheduled publication, versioned rescheduling and cancellation,
  and retry of the originally approved blocked release.
- Snapshot-backed pages, RSS, sitemap, aliases, conditional responses, and eligible
  Lightning tip recipients selected through profile administration.
- An isolated loopback Prometheus endpoint, bounded runtime and database metrics,
  backup health reporting, and a Grafana dashboard.
- A NixOS module with separate public and administration gateways, explicit owner
  initialization, service isolation, and supervised shutdown.
- Continuous local Litestream replication and complete checkpoints encrypted with
  rclone crypt before Backblaze B2 upload. Completed encrypted checkpoints remain
  locally for seven days; automated cleanup does not delete remote objects.
- Offline checkpoint verification and restore, portable snapshot export, exact
  schema and artifact validation, and revocation of restored sessions and agents.
- Deployment, backup, observability, lifecycle evidence, and release preparation
  runbooks.

### Fixed

- Preserve protected password buffers across serialization growth, terminal
  editing, cancellation, and failed request preparation.
- Keep public content pinned until explicit approval, including source changes,
  restarts, blocked activation, and durable operation replay.
- Bound public request input and accepted public and metrics connections; drain
  accepted work before closing the database writer.
- Prevent incomplete checkpoint publication, stale replica uploads from refreshing
  backup health, and interrupted local retention from leaking pending directories.
- Exercise shared Mermaid rendering concurrently within the application's
  renderer admission and process resource limits.
