# Remaining Maincopy v1 work

Last reviewed: 2026-09-06

This is the current status and unfinished work. [Design](design.md) records
architecture; [quality](quality.md) defines engineering rules. Detailed change
and test-run history belongs in Git and CI.

## Current status

Core publishing, administration, managed Git, metrics, encrypted backup tooling,
and release automation are implemented. Email is in progress. Home-server setup,
real-provider acceptance, release credentials, and the first public release remain.

The last fully validated code batch is
[`459573a`](https://github.com/tee8z/maincopy/commit/459573a58d07ff99f5989eeae4da0bd7e3382faa).
It passed these checks on 2026-09-06:

| Check | Result |
| --- | --- |
| Concurrent Rust tests | 994 passed; one existing ignored test; 16 threads |
| Formatting and Clippy | Passed with warnings denied |
| Manual CRAP | Maximum 19.662785; zero violations; 93.69% line coverage |
| Canonical Nix checks and build | Passed on x86_64 Linux, including the packaged deployment VM |
| Release helper fixtures | 13 passed with real GPG and a loopback registry |
| Hosted CI | [Linux Nix and Windows client jobs passed](https://github.com/tee8z/maincopy/actions/runs/34059358517) |

Local runbook and package rehearsals also passed. These results exclude ongoing
email code and do not establish ARM64, actual B2, owner-signer, or deployed-system acceptance.
The final release needs fresh evidence for its exact candidate.

## Work order

1. Complete email privacy, unsubscribe, removal, and durable dispatch.
2. Configure release accounts, protected environments, and the ARM64 runner in parallel.
3. Prepare the home server, networking, storage, and protected credentials.
4. Connect SES, DNS, signers, monitoring, and encrypted Backblaze B2 recovery.
5. Complete deployed acceptance and publish the first release.

Use isolated workstreams where they do not share implementation boundaries.
Update this plan after each integrated batch passes its final quality checks.

## Email

Use SES; DynamoDB is excluded. [Email delivery](email-delivery.md) is the single
source for consent, storage, dispatch, and deliverability requirements.

- Finish the concrete SES adapter, owner-reviewed campaign UI, and supervised dispatcher.
- Resolve subscriber storage and its encrypted backup/deletion recovery policy.
  Separate local SQLite outside the current site checkpoints is proposed; owner direction remains pending.
- Finish double opt-in, scanner-safe controls, RFC 8058 one-click unsubscribe,
  address removal, suppression, bounded retention, and protected credentials.
- Verify removal during dispatch, re-enrollment, reordered events, provider failure,
  cancellation, restart, and old-backup restoration. Never automatically retry an unknown send.
- Prove redaction and that site checkpoints contain no subscriber records or controls.
- Complete SES domain authentication, feedback handling, quotas, budgets, and authorized mailbox tests during deployment.

Keep capture and sending disabled until privacy, recovery, dispatch, and
real-provider deliverability acceptance pass together. If unfinished, release
core v1 with email disabled; do not ship capture alone.

## Home server and recovery

Follow [deployment](deployment.md), [backup and restore](backup-restore.md), and
[observability](observability.md) on the selected host.

- Deploy the reviewed package with the owner's content and protected credentials.
- Verify public HTTPS, private administration, origin checks, forwarded-header removal,
  offline owner initialization, service permissions, and restart without journaled credentials.
- Connect the intended Prometheus scraper and verify storage, queue, and backup health.
- Publish a complete encrypted checkpoint to the actual B2 bucket and restore it
  with an independently retained key and compatible package.
- Recover retained local ciphertext without the active object cache.
- Verify pages, RSS, sitemap, profiles, tips, rejected pre-restore sessions and agent grants,
  and acceptance consumption before replication or normal writes.
- Inject backup failure and confirm public reading remains available.
- Measure capture, replay, content verification, encryption, upload, and restore
  with representative data. Record recovery lag, duration, disk needs, and key-recovery steps.
- Set and accept a remote retention policy and storage budget. Remote immutable
  objects have no automatic deletion policy yet; local ciphertext retention is seven days.

The checkpoint timer targets one minute after the previous job finishes.
Replay, validation, encryption, and upload add to recovery lag.
Local replicas and fixture B2 transport do not complete off-site acceptance.

## System and security acceptance

- Exercise one managed Git site through browser, human CLI, and agent API:
  login, sync, preview, immediate/scheduled release, update, cancellation, blocked
  retry, profiles/tips, metrics, backup, restore, and shutdown.
- Verify successful and cancelled Nostr login with the owner's browser and CLI
  signers. Record versions, platforms, and protected session storage.
- Inject startup, writer, activation, Git, renderer, gateway, and restore failures.
  Confirm the last committed public snapshot survives wherever required by design.
- Review passwords and enumeration, session lifecycle, cookies/CSRF, NIP-98
  replay and request binding, roles/scopes, route isolation, and gateway TLS.
- Review Git credentials, content traversal, HTML/SVG/CSP, secret handling,
  corruption, queue saturation, backup/restore, dependency licenses, and advisories.
- Record representative request/compile latency, queue depth, WAL size, backup
  lag, runtime use, and shutdown duration. Close every critical or high-risk finding.
- Execute remaining authenticated CLI examples, browser trust setup, managed Git,
  deployment, B2 recovery, and release instructions on the intended systems.
  Help parsing and local fixtures do not replace these checks.
- Recheck examples and internal links after final release metadata is fixed.

## First release

The [release workflow](../.github/workflows/release.yml) prepares all five crates
from one reviewed version and signed tag. Follow [release](release.md) for exact
configuration, artifacts, publication order, retries, and version-pinned Nix use.

- Select the version and finish the [Unreleased changelog](../CHANGELOG.md#unreleased).
- Configure the crates.io publisher, trusted signer, protected release environment,
  and immutable GitHub Releases. Confirm ordinary CI cannot access release secrets.
- Provision the dedicated ARM64 runner with KVM and execute both Linux architecture gates.
- Freeze workspace and path-dependency versions; prepare clean package archives,
  signed tag, checksum manifest, and verified candidate artifacts.
- Review the packaged CLI, server, renderer, content examples, Nix outputs, and install instructions.
- Include required third-party notices in the distribution artifacts.
- Complete final acceptance for that candidate before registry uploads or public release publication.

## Quality gate

Use `cargo check` and Clippy during implementation. Defer full workspace tests
and CRAP measurement until the final pre-commit gate. Run canonical Nix checks
before code commits and pushes, as required by [quality](quality.md).

Completion requires meaningful transition, rejection, limit, restart, and
isolation tests; typed/redacted errors; matching schema and domain invariants;
supervised shutdown; documented limits and dependencies; and current runbooks.
Record the production need for new public APIs, traits, trivial getters, unsafe
blocks, and lint exceptions. Do not mark a feature complete from fixtures alone
when its acceptance requires an external system.

## Deferred work

Outside v1: browser editing and Git write-back; multiple sites; explicit
retraction; paid access; X/Substack share kits; automatic non-email provider
publishing; replaceable themes and typed widgets; sandboxed article code;
Obsidian Sync/YAML authoring; crawler/archive workers; and database high availability.

These require a design review when scheduled. Historical proposals remain in
Git history. External archival systems can already use canonical links, RSS,
sitemap, structured metadata, and standard HTTP caching.
