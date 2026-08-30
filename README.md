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
the provider-neutral payment domain, the typed content contract, the bounded
Linux content-tree loader, versioned content and snapshot identity primitives,
the embedded core SQLite schema and bootstrap, and a locked Nix development
environment. Most product slices remain under construction.

- [DESIGN.md](DESIGN.md) defines system behavior and trust boundaries.
- [IMPLEMENTATION.md](IMPLEMENTATION.md) defines the delivery order and exit
  criteria.
- [QUALITY.md](QUALITY.md) defines the reproducible Bash-only CRAP measurement
  and the required score budget.

## Development environment

The supported path uses Nix:

```console
nix develop
cargo test
cargo run -p maincopy-server --bin maincopyd --
```

While `maincopyd` runs, use another shell for an operator command:

```console
cargo run -p maincopy-cli --bin maincopy -- capabilities
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

The root manifest defines one Cargo workspace with three crates. `maincopyd`
from `crates/server` owns the listeners, scheduler, and database lifecycle.
`maincopy` from `crates/cli` is a short-lived operator client.
`crates/shared` contains wire contracts and transport defaults used by both.

Exactly one task in `maincopyd` will own one SQLite write connection. All
writers will use a shared bounded channel and receive a reply only after commit.
Query handlers will use a separate, bounded, query-only pool against the same
local write-ahead logging (WAL) database.

The CLI, future admin UI, and other agents will use the same versioned admin
API. They will never open the live SQLite database for writes.

The private API uses HTTP/JSON over a Unix domain socket on Linux and macOS.
On Windows, it uses a local named pipe that rejects remote clients. The pipe
grants access only to its owner and Windows `SYSTEM`. Maincopy has no admin TCP
fallback.

The CLI transport cross-compiles and runs on Windows. This does not make the
complete daemon a supported Windows target. Content discovery remains
Linux-only, and the native frontend build backend supports Linux and macOS.

The private API controls when a pinned article revision first becomes public.
A content reload cannot expose an unpublished or scheduled post. Distribution
jobs become eligible only after the canonical post is active.

Each content compilation pins the configured root once. It loads only
`publication.toml`, `posts/`, `drafts/`, and `assets/` through confined
descriptor-relative lookups. Descendant links, special files, mount crossings,
unsafe names, and resource-limit excesses fail closed. Later compiler stages
use owned bytes and never reopen the mutable source tree.

Revision identities use domain-separated, versioned BLAKE3 transcripts rather
than presentation serialization. Opaque resolved-asset inputs are bound to the
post or publication that produced them. Final post and site calculators also
require typed pre-injection renderer output and the public publication-ledger
projection, so a partial compiler stage cannot mint a final identity.

Maud templates remain Rust modules. A custom build script deterministically
combines and minifies first-party CSS and optional JavaScript, writes typed
generated metadata and bundles under `OUT_DIR`, and embeds those bundles in the
server binary. The bundle digest changes the immutable asset URL, renderer
identity, and site snapshot identity. Favicon, post-image, attachment, and CDN
assets remain part of the separate content pipeline.

`crates/server/src/main.rs` stays as a small process entry point.
`crates/server/src/startup.rs` loads server configuration and owns dependency
wiring, task supervision, and graceful shutdown. The separate CLI constructs a
concrete admin client and does not construct the server. The public and admin
router constructors remain independent. API tests call each router directly
through Tower, and transport tests use real local endpoints only when needed.

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
release workflow will publish the Rust packages to crates.io and the tagged
flake to FlakeHub. A nixpkgs submission can follow when the project has stable
users and a long-term maintainer.

The default branch is `master`.
