# Maincopy

**Markdown in Git. Live on your domain.**

Maincopy is a self-hosted publishing engine for writers. Git stores
the canonical Markdown. Maincopy renders and publishes approved revisions on
the author's domain.

Maincopy is in pre-v1 development. It is not ready for a production deployment.

## V1 direction

Maincopy v1 targets one site and canonical domain on one server. That site can
publish any number of Markdown articles from its Git repository.

V1 has these product boundaries:

- Git owns article bodies and authored metadata.
- Maincopy synchronizes one configured branch through read-only Git access.
- An administrator previews the exact rendered revision before publication.
- An administrator can publish now or schedule canonical website visibility.
- A Git sync cannot silently replace an already published revision.
- The canonical website and RSS are the only article outputs.
- Technical Markdown includes syntax highlighting, Mermaid diagrams, and
  sanitized Scalable Vector Graphics (SVG) output.
- A user profile can provide a mutable Lightning Address for static tips.
- Maincopy stores no tip invoice, payment, or settlement state in v1.
- The remote admin site uses authenticated access on a separate origin.
- The admin gateway forwards to a loopback-only HTTP listener in `maincopyd`.
- `maincopyd` exports Prometheus process, Tokio runtime, and database metrics
  from a dedicated loopback-only `/metrics` endpoint.
- Owner, Administrator, and Publisher roles map to fixed scopes.
- Publisher access covers content, status, sync, reload, preview, and release
  only.
- Browser sessions are opaque server-side cookies with CSRF protection.
- Automation uses scoped public-key credentials and fresh NIP-98 proofs.
- The NixOS module deploys `maincopyd`, the admin gateway, and Litestream.
- A tested restore combines the database replica and revision artifacts.
- Offline bootstrap and repair are finite process modes that bind no listener.
- These modes create no recovery transport, recovery API, or authentication
  bypass.

Maincopy v1 does not include a browser article editor, Git write-back,
multi-site hosting, paid articles, subscription capture, email delivery, share
kits, or social-network publishing. These outbound features are post-v1 work.
Git write permission remains external to Maincopy roles and credentials.
V1 uses neither JWT browser sessions nor long-lived bearer API tokens.

## Current status

The repository contains the content compiler, immutable snapshot model,
single-writer SQLite core, authenticated admin API, remote CLI, publication
scheduler, and static tip foundation.

Password and Nostr login, server-side sessions, role scopes, NIP-98 agent
proofs, exact previews, and immediate or scheduled release foundations are
present. The admin backend now uses a loopback-only HTTP listener. The CLI
connects through the configured HTTPS admin origin.

Managed Git synchronization, RSS and discovery documents, the admin web
interface, release-quality technical rendering, the NixOS module, Litestream
wiring, Prometheus metrics and dashboard, and complete restore evidence remain
incomplete. V1 rejects authored subscription and outbound-distribution
configuration and stores no target-job state.

> [!CAUTION]
> Do not expose the loopback admin listener directly. Use the reviewed HTTPS
> gateway and keep the public origin isolated from every admin route. Keep the
> metrics listener loopback-only.

## Post-v1 roadmap

Post-v1 mailing-list work starts with first-party double opt-in, unsubscribe,
export, and deletion. It must store token digests and keep raw addresses out of
logs, metrics, and audit events. The design must select an email transport,
address-comparison rule, and retention policy before implementation. Bulk
newsletter campaigns remain a separate increment.

Subscriber mutations and transactional email work must commit together. The
email worker must perform network delivery outside the database transaction.

Assisted X and Substack distribution remains credential-free. A future share
kit can use only a committed canonical revision and its canonical URL. Copy and
Open actions cannot claim delivery. X uses its supported Web Intent. Substack
uses copyable text plus an ordinary link so the user can select `Create` and
then `Note`; Maincopy does not depend on an undocumented prefilled composer.
X support must select and pin a weighted-text implementation against official
fixtures before release.

Automatic provider delivery, including Nostr article distribution, requires a
separate credential and job design. That design must define signer custody,
idempotency, retries, audit data, and delivery states. No outbound provider can
block or roll back canonical publication.

Post-v1 authoring can add Obsidian Sync as an optional source. The official
Obsidian Headless client mirrors a dedicated publishing vault to the server.
Maincopy then creates an immutable content snapshot before validation and
preview. A completed sync never publishes an article without the existing
preview and release approval.

The first Obsidian increment includes a Maincopy article template, strict YAML
Properties support, deterministic wiki links, and local attachment embeds. It
does not execute community plugins or use Obsidian Publish. Obsidian Sync
protects remote synchronization, but it does not replace Maincopy database and
revision-artifact backups.

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

The root manifest defines one Cargo workspace with four crates:

```text
crates/
|-- cli/                 # short-lived maincopy operator client
|-- markdown-compiler/   # content discovery, validation, and identity
|-- server/              # maincopyd service and application domains
`-- shared/              # wire contracts shared by the server and CLI
```

`maincopyd` owns the public service, loopback admin listener, database, and
supervised tasks. `maincopy` sends typed HTTPS admin requests and exits.

## Development

The supported development environment uses Nix on Linux.

```console
nix develop
cargo test --locked --workspace --all-targets --all-features
```

Bootstrap the included example in one Nix shell. The command reads the owner
password from the terminal.

```console
cargo run --locked -p maincopy-server --bin maincopyd -- \
  --config crates/server/examples/maincopy.toml \
  identity bootstrap password --username owner
```

Then start the example service:

```console
cargo run --locked -p maincopy-server --bin maincopyd -- \
  --config crates/server/examples/maincopy.toml
```

The example starts the public service and loopback admin backend. The HTTPS
admin development gateway is not complete. Do not expose the backend directly.
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
