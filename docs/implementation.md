# Remaining Maincopy v1 work

Last reviewed: 2026-09-07

This is the current status and unfinished work. [Design](design.md) records
architecture; [quality](quality.md) defines engineering rules. Detailed change
and test-run history belongs in Git and CI.

## Current status

Core publishing, administration, managed Git, metrics, encrypted backups, email,
and release automation are implemented and locally validated. Home-server setup,
real-provider acceptance, release credentials, and the first public release remain.

The latest validated code is
[`5cb2a02`](https://github.com/tee8z/maincopy/commit/5cb2a02c759aaaff5d8de30804375ed946750584).
It passed these checks on 2026-09-07:

| Check | Result |
| --- | --- |
| Concurrent instrumented Rust tests | 1,154 passed; one existing ignored test; 16 threads |
| Formatting and Clippy | Passed with warnings denied, including the pinned Nix toolchain |
| Manual CRAP | Maximum 19.749526; zero violations; 93.92% line coverage |
| Canonical Nix checks and build | Passed on x86_64 Linux, including the packaged deployment and restore VM |
| Backup operations | 28 passed, including finite retention and private credential staging |

ARM64, actual SES/B2, Owner signers, and deployed-system acceptance remain.
The final release needs fresh evidence for its exact candidate.

## Work order

1. Prepare the home server and connect SES, DNS, signers, monitoring, and encrypted B2 backups.
2. Complete deployed acceptance and publish the first release.

Release account and ARM64 runner setup can proceed alongside host preparation.
Update this plan when an integrated batch passes its final quality gate.

## Email

Consent and attempts use the existing database. Double opt-in, removal routes,
Owner campaign screens, SES dispatch, feedback recovery, and consent reset are implemented.
Restore retires consent; finite backup epochs bound retained history.
The code simplification pass and README, changelog, and operational documentation cleanup are complete.

Complete actual SES domain/queue permissions, quotas, mailbox tests, and privacy
acceptance using the [email guide](email-delivery.md#provider-acceptance-before-launch).

Use SES; DynamoDB is excluded. Keep capture and sending disabled until all
required acceptance passes. Do not release address capture without removal and delivery controls.

## Home server and recovery

Use [deployment](deployment.md), [backup and restore](backup-restore.md), and
[observability](observability.md) as the operational procedures.

- Provision the reviewed package, content, private administration network, DNS,
  HTTPS, persistent storage, and protected credentials.
- Initialize the Owner offline; verify permissions, restart, and logging without
  exposed credentials or subscriber data.
- Connect monitoring and alerting, including upstream SES/SNS delivery health.
- Publish to the actual B2 bucket and restore with an independently retained key
  and compatible package. Also recover local ciphertext without its active cache.
- Verify actual bucket lifecycle/version deletion, restored-session rejection,
  consent retirement, and acceptance before replication resumes.
- Measure recovery lag, disk needs, and restore duration with representative content.
  Inject backup failure and confirm the public site stays available.

## System and security acceptance

- Exercise the publishing workflow through browser, human CLI, and scoped agent:
  sync, preview, immediate/scheduled publication, updates, cancellation, and blocked retry.
- Test the Owner's actual Nostr signers, credential storage, profiles, and tips.
- Verify gateway isolation, authorization, CSRF/NIP-98 replay protection, request
  bounds, secret handling, content sanitization, and failure/shutdown behavior.
- Review dependency licenses and advisories; close critical or high-risk findings.
- Execute the retained setup/recovery examples on the intended systems and record
  representative runtime, request, compilation, and backup behavior.

Local fixtures cannot complete real-provider, signer, or deployed-system acceptance.

## First release

Follow [release](release.md) for the maintained publication procedure.

- Select the version and finalize the [changelog](../CHANGELOG.md#unreleased).
- Configure crates.io ownership/token, the trusted signer, protected release
  environment, and immutable GitHub Releases.
- Provision the dedicated ARM64 KVM runner and pass both Linux architecture gates.
- Prepare consistent versions, package archives, signed tag, notices, and verified artifacts.
- Complete acceptance for the exact candidate before registry or public release uploads.

## Quality gate

Follow [quality.md](quality.md). Use `cargo check` and Clippy during implementation;
run full workspace tests, CRAP, and canonical Nix checks before a code commit and push.
Keep one current validation baseline above. Detailed batch history belongs in Git and CI.

## Deferred work

Outside v1: browser editing and Git write-back; multiple sites; explicit
retraction; paid access; X/Substack share kits; automatic non-email provider
publishing; replaceable themes and typed widgets; sandboxed article code;
Obsidian Sync/YAML authoring; crawler/archive workers; and database high availability.

These require a design review when scheduled. Historical proposals remain in
Git history. External archival systems can already use canonical links, RSS,
sitemap, structured metadata, and standard HTTP caching.
