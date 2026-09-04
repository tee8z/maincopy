# Remaining Maincopy v1 work

Status: active backlog

Last reviewed: 2026-09-04

Related: [project overview](../README.md), [system design](design.md),
[managed Git runbook](managed-source.md),
[local development runbook](local-development.md), and
[engineering style](quality.md).

## Purpose

This document lists only unfinished work. Git history and tests provide the
record of completed implementation.

The [system design](design.md) remains the authority for V1 behavior, data
ownership, and trust boundaries. Each change must also follow the
[engineering style guide](quality.md).

## Execution order

```mermaid
flowchart LR
    Product[1. Product closure] --> Operations[2. Deployment and recovery]
    Operations --> Review[3. Security and system evidence]
    Review --> Release[4. Release candidate]
```

1. Complete update, schedule, cancel, retry, account, and profile workflows.
2. Add public-response metadata, Content Security Policy, and lifecycle polish.
3. Add metrics, NixOS, Caddy, Litestream, backup, and restore support.
4. Complete the security review, system matrix, documentation, and release dry run.

Do not begin automatic provider delivery, subscription capture, multi-site
hosting, or Git write-back as part of V1.

## 1. Product closure

### 1.1 Complete release management

Expose the remaining release transitions through the typed writer and
activation coordinator.

Deliverables:

- Expose durable release listing and inspection through the API and CLI.
- Edit or cancel a scheduled release with an exact resource version.
- Retry or cancel blocked activation without creating a second release.
- Add schedule edit, cancel, and retry flows to the browser interface.
- Add release inspection, edit, cancel, and retry commands to the CLI.
- Reject release edits after activation starts.
- Return stable conflicts for stale resource versions and idempotency reuse.
- Show a clear terminal result after a lost or repeated request.

Required evidence:

- Complete initial, update, and scheduled publication through the browser.
- Edit and cancel schedules with exact resource-version checks through the API
  and CLI.
- Retry and cancel a blocked release without changing its approved revision.
- Lose a mutation response and recover through its operation identifier.
- Keep draft and preview assets unreachable from the public router.

### 1.2 Complete account and profile workflows

Complete the browser and CLI operations needed for ordinary administration.
Preserve the fixed Owner, Administrator, and Publisher scope boundaries.

Deliverables:

- Rotate a password without exposing it in logs, arguments, or diagnostics.
- Create, disable, and inspect users under the existing role rules.
- Manage login and agent public-key credentials with fresh authentication.
- Show the selected credential public key and fingerprint without exposing its
  private path or bytes.
- Update the display profile, Lightning Address, and active tip recipient.
- Provide clear empty, conflict, expired-session, and forbidden states.

Required evidence:

- Exercise password-only, Nostr-only, and combined-provider configurations.
- Preserve one usable credential for every enabled user.
- Reject Publisher access to users, roles, profiles, credentials, and source
  configuration.
- Revoke sessions and agent credentials after user disablement.
- Reject stale profile and role mutations without partial state.

### 1.3 Complete managed-source administration

The normal push-to-preview loop is complete. Add safe online operations for
the source settings that still require daemon shutdown.

Deliverables:

- Reconfigure the remote, branch, subdirectory, credential, and poll interval
  through an Owner-only, fresh-authenticated operation.
- Preserve the last installed candidate until new settings fetch, validate,
  compile, and commit successfully.
- Display the selected deploy public key and fingerprint.
- Define reachability-aware candidate retention before automatic garbage
  collection removes any retained artifact.

Required evidence:

- Reject stale versions and unknown credential names before network access.
- Keep the current private catalog and public snapshot after any failed change.
- Restart during reconfiguration and recover one durable terminal result.
- Prove that no source response exposes a private-key or `known_hosts` path.

### 1.4 Add image metadata and response policy

Complete public image output and apply one least-privilege response policy.

Deliverables:

- Render local or allowlisted external favicons.
- Render site and article image metadata.
- Emit canonical image URLs in Open Graph and `BlogPosting` JSON-LD.
- Derive Content Security Policy (CSP) origins from validated configuration.
- Disallow scripts, objects, frames, and unconfigured connections by default.
- Add `Referrer-Policy: no-referrer` to public responses.
- Keep `unsafe-inline` and `unsafe-eval` out of the CSP.
- Document that external asset bytes can change independently.

Required evidence:

- Test local and external favicon and article-image fixtures.
- Reject an unconfigured asset origin and CSP directive injection.
- Snapshot the exact CSP and referrer headers.
- Serve local files with safe content types and disposition rules.
- Keep preview-only assets unreachable from public routes.

### 1.5 Finish listener and tip administration

Complete the remaining lifecycle behavior on the public and profile surfaces.

Deliverables:

- Apply bounded request limits and structured access logs.
- Drain active public requests during orderly shutdown.
- Keep liveness independent from snapshot readiness.
- Add browser forms for profile and active-recipient changes.
- Explain ineligible and unconfigured recipient states without blocking pages.
- Include the active tip projection in restart and restore evidence.

Required evidence:

- Drain an active request before the writer closes.
- Fail readiness after a required supervised task exits.
- Update and remove the tip recipient through the browser.
- Keep articles readable when the selected tip recipient becomes invalid.
- Reconstruct the same eligible tip projection after restart and restore.

### Product-closure gate

- Browser and CLI users can complete every supported release transition.
- No sync, reload, profile edit, or restart grants publication approval.
- Every public response has the accepted metadata and security headers.
- Public routes expose no admin, metrics, draft, or preview capability.

## 2. Deployment and recovery

### 2.1 Add metrics and database health

Create one application-owned Prometheus registry. Do not use the default
registry or user-controlled metric labels.

Deliverables:

- Serve `GET` and `HEAD /metrics` from a dedicated loopback listener.
- Record bounded writer queue, pool, transaction, WAL, and checkpoint metrics.
- Record stable Tokio runtime and Linux process metrics.
- Supervise the metrics listener and runtime collector with the application.
- Add a checked-in Grafana dashboard whose queries match emitted metrics.
- Convert corruption, disk-full, and checkpoint failures into typed health
  and shutdown behavior.

Required evidence:

- Keep `/metrics` absent from public and admin routers.
- Construct multiple isolated registries in one test process.
- Verify metric names, types, labels, content type, and cardinality.
- Fail the listener or collector and start controlled shutdown.
- Prove that labels contain no path, URL, identifier, slug, or secret.

### 2.2 Package the production topology

Add a NixOS module that owns the complete service boundary.

Deliverables:

- Package `maincopyd`, `maincopy`, `maincopy-mermaid`, Caddy, and Litestream.
- Run each service under a dedicated identity with protected state paths.
- Bind public traffic according to configuration.
- Keep admin and metrics upstreams on loopback.
- Make private-network admin exposure the default.
- Require explicit configuration for an Internet-reachable admin origin.
- Remove untrusted identity and forwarding headers at the gateway.
- Disable automatic retries for admin mutations.
- Keep SSH, TLS, and replica credentials outside Git and the Nix store.

Required evidence:

- Evaluate minimal and complete module configurations.
- Boot the topology in a NixOS virtual machine.
- Prove route and origin isolation through Caddy.
- Reject unsafe ownership, permissions, paths, and listener addresses.

### 2.3 Add Litestream backup and offline restore

Back up the operational SQLite ledger and retain compatible revision artifacts.

Deliverables:

- Configure a local development replica and secret-backed production replica.
- Expose degraded backup health without blocking public reads.
- Document recovery point objective and recovery time objective measurements.
- Restore into an empty destination only.
- Verify schema, database digest, artifact digest, and restore acceptance offline.
- Consume one typed restore marker before normal startup.
- Invalidate restored browser sessions and agent credentials.
- Refuse migration or mutation before the restored candidate is accepted.

Required evidence:

- Interrupt replication and recover without corrupting the live database.
- Restore after removing every local SQLite sidecar file.
- Reject a marker for different bytes, schema, artifacts, or binary.
- Reproduce released pages, RSS, routes, profiles, and tip projection.
- Measure the documented recovery targets.

### Operations gate

- The NixOS virtual machine runs Maincopy, Caddy, and Litestream.
- Local Prometheus can scrape the loopback metrics listener.
- The live database remains on local storage.
- The restore drill preserves the operational ledger and required artifacts.
- Restart and restore complete before public or admin readiness.

## 3. Security and system evidence

### 3.1 Run the end-to-end matrix

Repair the canonical Nix test environment before release acceptance. The current
sandbox omits Git and `ssh-keygen` from the test PATH. Its root directory belongs
to UID 65534, which the credential ancestor policy rejects. Preserve credential
validation while providing a representative test environment. Local workspace
tests and CRAP currently pass; the Nix gate remains failing.

Exercise one representative managed Git site through browser, human CLI, and
agent API workflows.

The matrix must cover startup, login, sync, preview, immediate release,
scheduled release, update, cancellation, blocked retry, tips, metrics, backup,
restore, and shutdown.

Inject failures at each startup stage, writer boundary, activation boundary,
Git phase, renderer phase, gateway route, and restore gate. Public readers must
retain the last committed snapshot whenever the design requires continuity.

### 3.2 Complete the security review

Review these boundaries before release:

- password hashing, enumeration resistance, and password-worker limits;
- session fixation, expiry, rotation, revocation, cookies, and CSRF;
- Nostr login and NIP-98 freshness, replay, URL, method, and payload binding;
- role, scope, actor, host, origin, and route isolation;
- gateway header removal and TLS termination;
- Git host verification and private-key handling;
- content traversal, HTML, SVG, asset-origin, and CSP policy;
- database corruption, queue saturation, backup failure, and restore acceptance;
- dependency licenses, advisories, and reproducible inputs.

Record representative latency, compilation, queue, WAL, backup-lag, runtime,
and shutdown measurements. Close every critical or high-risk finding.

### 3.3 Verify operator documentation

- Run every documented command from a clean environment.
- Validate each TOML and frontmatter example.
- Check internal Markdown links and generated OpenAPI output.
- Execute the deployment and restore runbooks without hidden steps.
- Verify that documentation distinguishes current behavior from target design.

## 4. Release candidate

Prepare a candidate without publishing an artifact until the owner approves it.

Deliverables:

- Select a semantic version and write the changelog.
- Verify crate metadata, included files, README, and license.
- Build the source archive and Nix outputs from clean inputs.
- Generate checksums and a dependency inventory.
- Define a signed annotated tag policy and trusted signing keys.
- Pin each third-party release action to an immutable commit.
- Protect crates.io and release credentials behind owner approval.
- Test idempotent recovery after each publication step.

Required evidence:

- Run `cargo publish --dry-run --locked` on the exact candidate.
- Run `nix flake check` and `nix build` from the release archive.
- Reject an unsigned tag, version mismatch, or untrusted signing key.
- Create a draft GitHub Release without making it public.
- Confirm that ordinary continuous integration cannot access release secrets.

## Definition of done

A work item is complete only when all applicable statements are true:

- Tests cover success, rejection, limits, transitions, restarts, and isolation.
- External failures map to stable typed codes without secret detail.
- Database structure and domain transitions enforce the same invariants.
- Long-running work is supervised, cancelled, and awaited.
- New limits are configured or documented as safe fixed constants.
- New dependencies have minimal features and recorded licenses.
- New project traits, trivial getters, unsafe blocks, lint exceptions, and
  public items have a documented production need.
- Operator behavior and configuration changes update their runbooks.
- Formatting, Clippy, workspace tests, Nix checks, and the CRAP budget pass.

## Deferred work

The following work remains outside V1:

- browser article editing and Git write-back;
- multiple sites or tenants;
- mailing-list capture and email delivery;
- automatic Nostr or other provider delivery;
- X and Substack share kits;
- paid articles and access entitlements;
- Obsidian Sync as a managed source;
- replaceable themes and typed article widgets;
- sandboxed article code execution; and
- crawler or archive workers.

External archive systems can continue to use canonical links, sitemap,
`BlogPosting` metadata, RSS, and ordinary HTTP caching metadata.
