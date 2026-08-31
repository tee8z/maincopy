# Maincopy

**One canonical copy. Every channel.**

Maincopy is a self-hosted publishing engine for technical writers. Git stores
the canonical Markdown. Maincopy renders and publishes approved revisions on
the author's domain.

Maincopy is in pre-v1 development. It is not ready for a production deployment.

## V1 direction

Maincopy v1 targets one site and canonical domain on one server. That site can
publish any number of Markdown articles from its Git repository.

V1 has these product boundaries:

- Git owns article bodies and authored metadata.
- An administrator previews the exact rendered revision before publication.
- An administrator can publish now or schedule canonical website visibility.
- A Git sync cannot silently replace an already published revision.
- The canonical website and RSS are the only automatic article outputs.
- Maincopy prepares manual X and Substack Note share text after publication.
- A user profile can provide a mutable Lightning Address for static tips.
- Maincopy stores no tip invoice, payment, or settlement state in v1.
- The remote admin site uses authenticated access on a separate origin.
- The admin gateway forwards to a loopback-only HTTP listener in `maincopyd`.
- Owner, Administrator, and Publisher roles map to fixed scopes.
- Publisher access covers content, status, sync, reload, preview, release, and
  share only.
- Browser sessions are opaque server-side cookies with CSRF protection.
- Automation uses scoped public-key credentials and fresh NIP-98 proofs.
- Offline bootstrap and repair are finite process modes that bind no listener.
- These modes create no recovery transport, recovery API, or authentication
  bypass.

Maincopy v1 does not include a browser article editor, Git write-back,
multi-site hosting, paid articles, or automatic social-network publishing.
Git write permission remains external to Maincopy roles and credentials.
V1 uses neither JWT browser sessions nor long-lived bearer API tokens.

## Current status

The repository contains the workspace foundation, content compiler, immutable
snapshot model, SQLite writer, an evolving admin API, and an evolving
publication slice.

Remote authentication, managed Git synchronization, the admin web interface,
manual share kits, and profile-backed tips are incomplete.
Some superseded payment-provider code remains during the pre-v1 transition.
That code is not part of the V1 product contract.

The current server and CLI still use a pre-v1 Unix-socket or Windows named-pipe
transport. The authenticated cutover removes its code, flags, defaults,
service-unit wiring, and tests atomically. Bootstrap and repair create no
recovery transport or recovery API. No supported build has both transports or
an unauthenticated admin TCP listener.

> [!CAUTION]
> The current admin API does not implement the target login and session
> boundary. Do not expose an admin listener until that boundary is complete.

## Documentation

- [System design](docs/design.md) defines the V1 architecture, trust
  boundaries, and data ownership.
- [Implementation plan](docs/implementation.md) defines delivery order,
  dependencies, known transitions, and acceptance gates.
- [Engineering style](docs/quality.md) defines Rust, testing, documentation,
  and quality conventions. It also explains the manual CRAP report.

The design and implementation documents describe target V1 behavior. They do
not claim that every feature is implemented.

## Workspace

The root manifest defines one Cargo workspace with three crates:

```text
crates/
|-- server/    # maincopyd service and application domains
|-- cli/       # short-lived maincopy operator client
`-- shared/    # wire contracts shared by the server and CLI
```

`maincopyd` owns the public service, loopback admin listener, database, and
supervised tasks. `maincopy` sends typed HTTPS admin requests and exits.

## Development

The supported development environment uses Nix on Linux.

```console
nix develop
cargo test --locked --workspace --all-targets --all-features
```

Run the included example in one Nix shell:

```console
cargo run --locked -p maincopy-server --bin maincopyd -- \
  --config /dev/null \
  --content-root crates/server/examples/content \
  --state-root target/maincopy-dev/state \
  --runtime-root target/maincopy-dev/run
```

The example starts the current public service. The authenticated HTTPS admin
development profile is not complete. Do not expose the transitional admin API.
Stop the server with `Ctrl+C`.

Run the Linux continuous integration checks and build with:

```console
nix flake check --print-build-logs
nix build --print-build-logs
```

GitHub Actions also checks the shared contracts, CLI, and server on Windows.
The manual CRAP report is separate from these CI checks.

The flake supports `x86_64-linux` and `aarch64-linux`. The default branch is
`master`.
