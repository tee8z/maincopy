# Maincopy

**One canonical copy. Every channel.**

Maincopy is a Git-native publishing engine for technical writers. It compiles
canonical Markdown into a fast, server-rendered site and prepares each article
for feeds and external distribution. Git owns the content. SQLite owns the
publication schedule and delivery history.

Maincopy is in pre-v1 development. The repository is private while the first
usable release is built.

## Why Maincopy

- Keep the authoritative article and its authored metadata in a content repository.
- Publish the complete article on a domain that the author controls.
- Render Markdown, highlighted code, ASCII diagrams, and Mermaid on the server.
- Use content-owned assets or images and files on an allowlisted HTTPS CDN.
- Embed deterministic, content-hashed application CSS and optional JavaScript
  without mixing them with author-owned assets.
- Capture newsletter subscribers with first-party double opt-in.
- Accept optional BOLT11 tips through a provider-neutral receive service. V1
  uses Lexe without making article availability depend on payment health.
- Schedule publication through one private API that works for people and agents.
- Record operational state in SQLite with one serialized writer and WAL readers.
- Replicate the SQLite database with Litestream without placing it on a network
  filesystem.
- Use RSS and target-specific representations without making an external network
  part of the canonical publication path.

## Current status

The repository contains the accepted v1 architecture, the ordered development
plan, the process-composition scaffold, the canonical publication-job domain,
the provider-neutral payment domain, the typed content contract, and a locked
Nix development environment. Most product slices remain under construction.

- [DESIGN.md](DESIGN.md) defines system behavior and trust boundaries.
- [IMPLEMENTATION.md](IMPLEMENTATION.md) defines the delivery order and exit
  criteria.

## Development environment

The supported path uses Nix:

```console
nix develop
cargo test
cargo run -- serve
```

Run the complete local quality gate with:

```console
nix flake check --print-build-logs
nix build
```

The flake currently supports `x86_64-linux` and `aarch64-linux`. Its development
shell includes the pinned Rust toolchain, SQLite tools, Litestream, and the Nix
formatter.

## Architecture in one minute

`maincopy serve` will own the public listener, private Unix-socket admin API,
scheduler, and database lifecycle. Exactly one task will own one SQLite write
connection. All writers will use a shared bounded channel and receive a reply
only after commit. Query handlers will use a separate, bounded, query-only pool
against the same local WAL database.

The CLI, future admin UI, and other agents will use the same versioned admin
API. They will never open the live SQLite database for writes.

The private API controls when a pinned article revision first becomes public.
A content reload cannot expose an unpublished or scheduled post. Distribution
jobs become eligible only after the canonical post is active.

Maud templates remain Rust modules. A custom build script deterministically
combines and minifies first-party CSS and optional JavaScript, writes typed
generated metadata and bundles under `OUT_DIR`, and embeds those bundles in the
binary. The bundle digest changes the immutable asset URL, renderer identity,
and site snapshot identity. Favicon, post-image, attachment, and CDN assets
remain part of the separate content pipeline.

`src/main.rs` stays as a small process entry point. It can initialize bootstrap
logging before its final `run_until_stop().await` call. `src/startup.rs` parses
the typed process command, loads its configuration, and dispatches server or
admin-client behavior without global configuration state. It also owns server
dependency wiring, task supervision, and graceful shutdown so the application
can be built and tested as a library. The public and admin router constructors
remain independent. API tests call each router directly through Tower, and
socket-based tests are reserved for transport behavior.

This startup-owned configuration choice is intentional for the exact
no-argument call boundary; it can change only with an explicit signature
change, not a global configuration singleton.

V1 captures and confirms subscription addresses, but it does not send bulk
newsletter campaigns. Litestream replicas contain that subscriber data, so the
same access, encryption, retention, and deletion rules apply to both the live
database and its replicas.

V1 also exposes provider-neutral tip intent, invoice, and settlement contracts.
A closed, cloneable provider enum delegates to the public crates.io Lexe SDK.
The Lexe provider uses one bounded operation queue whose dispatcher owns a
`JoinSet`; there is no semaphore admission path or second payment-service
queue. When tips are enabled, the typed concurrency limit rejects values below
two, so the one update subscriber cannot occupy the only provider slot.
The operator provisions revocable client credentials with only `Receive`,
`ReadPayments`, and `ReadInfo`, plus no explicit endpoint permissions;
Maincopy has no spend operation. Lexe 0.1.22 does not expose these grants
through the credential blob, so a separate operator audit remains required.
Maincopy commits an intent before invoice creation and stores an opaque intent
marker only in Lexe's payer-private
`personal_note`. Because Lexe does not accept a provider idempotency key for
invoice creation, an uncertain result must reconcile against remote payment
indexes before any later creation decision.

The remote Lexe node is authoritative. An optional SDK cache is disposable and
is not part of Maincopy's Litestream backup. A long-lived payment-update
subscriber uses Lexe's finite-wait tail API for prompt detection, then runs the
same paged, cursor-backed reconciliation path used at startup and after a
disconnect. Maincopy commits each tip decision with its opaque update cursor,
so repeated or unrelated wallet updates cannot duplicate settlement or stall
later updates. An idle wait is a normal heartbeat. A transport failure uses
bounded backoff and degrades only payment health. A future LND adapter can use
the same application contracts. Lexe outages disable tips, but they never gate
article access or core article readiness.

## Release direction

The repository flake is the installation source during pre-v1 development. V1
will use signed Semantic Versioning tags and GitHub Releases. The approved
release workflow will publish the Rust crate to crates.io and the tagged flake
to FlakeHub. A nixpkgs submission can follow when the project has stable users
and a long-term maintainer.

The default branch is `master`.
