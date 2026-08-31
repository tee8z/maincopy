# Maincopy engineering style guide

Status: active project convention

Related: [README](../README.md), [design](design.md), and
[implementation plan](implementation.md).

These rules are normative for new code. Existing deviations are cleanup debt,
not precedent. Reviews must approve and document exceptions.

## Design

- Start with a concrete type. Put behavior in its inherent `impl` blocks.
- Use a closed enum when Maincopy knows all implementations.
- Use a closure or function parameter to replace one operation.
- Add a project trait only for a real, open production substitution or
  extension boundary. Testing and possible future use are not sufficient.
- Use standard and framework traits for Rust interoperability.
- Pass dependencies through constructors. Do not use mutable global state or a
  service locator.

## Types and APIs

- Expose fields directly on records, commands, wire values, and read views.
  Keep invariant-bearing fields private behind validated operations.
- Do not add trivial getters, setters, or convenience `Deref` implementations.
- Use `as_*` for borrowed projections, `into_*` for consuming conversions, and
  `view()` only for a real read or persistence boundary.
- Use enums for domain alternatives and typed payloads for variant data. Do not
  encode alternatives as primitives or mutually exclusive `Option`s. Use
  `bool` only for an independent yes/no fact.
- Flat wire or storage records can use optional fields only when conversion
  immediately validates them into the typed domain form.
- Match enums exhaustively. Convert them to primitives only at external
  boundaries and test stable encodings. Use `Unknown` only when a protocol
  requires safe forward compatibility.
- Use a newtype for validation, units, ambiguity, or security meaning. Use
  typestate only when it prevents a materially invalid transition.

## Modules and imports

- Organize business code by capability under `domain`. Keep wire contracts in
  `maincopy-shared` and operator CLI behavior in `maincopy-cli`.
- Keep authored content types, validation, and compilation in
  `markdown-compiler`. Keep server composition and publication policy in
  `maincopy-server`.
- Name each root mechanism for the operation that it performs. Do not give a
  root mechanism the same capability name as a domain module. For example,
  keep Git provenance discovery in `source_provenance` and presentation
  mechanics in `render`.
- Keep items private by default. Export only the required caller boundary and
  re-export the intentional module surface.
- Do not expose an implementation only for a test. Keep `main.rs` small and
  process composition in `startup`.
- Import project items at module scope and use their short names at call sites.
  Import `crate::domain::publication::admin::preview_asset_routes`, then call
  `preview_asset_routes()`.
- Never call a project item through a `crate::...` path inside a function.
  Resolve collisions with a clear module-scope `as` alias.

## Validation and errors

- Parse and validate external input before domain logic. Reject unknown fields
  when ignoring them could change behavior.
- Validate database rows during rehydration. Fail closed on unknown states,
  invalid widths, or impossible relationships.
- Bound untrusted bytes, counts, depths, queues, requests, and responses.
- Aggregate document errors deterministically. Return one error for one
  operational failure.
- Use typed errors at every Maincopy-owned fallible boundary. Give variants
  actionable classes, typed context, and sources.
- Do not use `String`, maps, or `anyhow` as domain errors. Convert to
  framework-required text only in adapters.
- Map errors exhaustively to stable API and CLI categories. Return a safe
  message and request ID, never raw internal text.
- Restrict type erasure to top-level aggregation. Do not panic on external
  input.

## Persistence and HTTP

- Keep domain SQL in its concrete `store.rs` and connection ownership in
  `database`. Do not expose SQLx connections through application stores.
- Use concrete mutation commands with direct precondition, version, identity,
  and idempotency fields. Dispatch their closed enum through the sole writer.
- Use a bounded, read-only, query-only pool for reads.
- Put structural integrity in strict migrations and transition rules in domain
  code.
- Never hold a transaction across network, filesystem, renderer, or other
  unbounded work.
- Keep public and admin routers separate. Prove route isolation.
- Build admin routes and OpenAPI from one registry.
- Use direct wire fields and explicit Serde names. Use `deny_unknown_fields`
  when omission could remove a precondition, but not on forward-compatible
  responses.
- Never expose arbitrary SQL, shell commands, host paths, or secret reads
  through an admin route.

## Lifecycle and security

- Let `Application` own, supervise, cancel, and await long-lived tasks. Treat
  unexpected critical-task completion as an application failure.
- Put cancellation first when shutdown must win. Use `spawn_blocking` for
  blocking work.
- Close ingress before workers and the writer. Drain accepted work before
  closing resources.
- Do not coordinate with sleeps. Use channels, notifications, paused time, or
  explicit state transitions.
- Give secrets dedicated types and narrow lifetimes. Never expose plaintext.
  Avoid `Clone`, `Serialize`, and ordinary `Debug`; bound reads and zeroize
  owned buffers.
- Use safe Rust by default. Use `unsafe` only at a required external boundary.
  Keep each block small and put a `SAFETY:` explanation immediately before it.
- Explain validity, ownership, lifetime, and release. Give owned raw resources
  a tested safe wrapper with `Drop`.

## Tests and documentation

- Name tests after observable behavior.
- Keep rule tests beside code. Put router, wire, process, and platform behavior
  in integration tests.
- Prefer concrete components and domain-owned substitutes over mock traits.
- Test success, rejection, bounds, transitions, stable encodings, restarts,
  and trust-boundary isolation when applicable.
- Use fixtures for exact bytes and temporary directories with port `0`. Never
  depend on developer state or real services.
- Call routers directly when a socket adds no evidence. Use paused Tokio time
  for timers and timeouts for real waits.
- Cancel and await spawned tasks. Do not add line-hit tests only for coverage.
- Document invariants, lifecycle, trust boundaries, and surprising decisions.
  Do not restate syntax.
- Use `#[expect(lint, reason = "...")]` for a narrow lint exception.
- Do not leave unowned placeholders. Label target and implemented behavior.

## Dependencies and checks

- Declare shared versions in `[workspace.dependencies]`. Enable features at the
  consumer and add dependencies only with their first production use.
- Pin Rust dependencies in `Cargo.lock` and Nix inputs in `flake.lock`.

Run the canonical Linux gate:

```console
nix flake check --print-build-logs
nix build --print-build-logs
```

Do not claim another documentation, audit, or security gate until the
repository configures it.

### CRAP risk check

Change Risk Anti-Patterns (CRAP) is manual. Every measured function must remain
below 20; a score of 20 fails.

```bash
KNOTS_BIN=/path/to/knots scripts/crap-report.sh
```

The script writes below `target/crap/`. Nix and GitHub Actions do not run it.
Add meaningful tests or simplify risky code. Never game the score with line-hit
tests or unnecessary function splits.

## Review exceptions

A pull request must explain each project trait, trivial getter, unsafe block,
lint exception, dependency, or public item. State the production need and why
an existing simpler pattern cannot satisfy it.
