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
- Capture newsletter subscribers with first-party double opt-in.
- Schedule publication through one private API that works for people and agents.
- Record operational state in SQLite with one serialized writer and WAL readers.
- Replicate the SQLite database with Litestream without placing it on a network
  filesystem.
- Use RSS and target-specific representations without making an external network
  part of the canonical publication path.

## Current status

The repository contains the accepted v1 architecture, the ordered development
plan, a Rust scaffold, and a locked Nix development environment. The executable
is still a placeholder.

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

`src/main.rs` stays as a small process entry point. It can initialize bootstrap
logging before its final `run_until_stop().await` call. `src/startup.rs` parses
the typed process command, loads its configuration, and dispatches server or
admin-client behavior without global configuration state. It also owns server
dependency wiring, task supervision, and graceful shutdown so the application
can be built and tested as a library. The public and admin router constructors
remain independent. API tests call each router directly through Tower, and
socket-based tests are reserved for transport behavior.

V1 captures and confirms subscription addresses, but it does not send bulk
newsletter campaigns. Litestream replicas contain that subscriber data, so the
same access, encryption, retention, and deletion rules apply to both the live
database and its replicas.

## Release direction

The repository flake is the installation source during pre-v1 development. V1
will use signed Semantic Versioning tags and GitHub Releases. The approved
release workflow will publish the Rust crate to crates.io and the tagged flake
to FlakeHub. A nixpkgs submission can follow when the project has stable users
and a long-term maintainer.

The default branch is `master`.
