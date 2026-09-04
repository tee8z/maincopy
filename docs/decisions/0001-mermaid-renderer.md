# ADR 0001: Use mermaid-rs-renderer 0.3.1 in a supervised helper

Status: accepted and implemented; release verification is in progress

Decision date: 2026-09-03

## Purpose

This architecture decision record (ADR) selects the local Mermaid engine for
Maincopy v1. It also fixes the process boundary and initial resource targets.

The decision resolves the renderer selection in
[work package 6.2](../implementation.md#work-package-62-mermaid-implementation-spike).
It does not complete the wider Slice 6 rendering corpus and release gate.

## Context

Maincopy renders Mermaid during content compilation. Public pages must not need
JavaScript, Node.js, Chromium, or a network renderer.

Mermaid source and renderer output are untrusted. A panic, stack overflow, or
infinite loop must not take down or indefinitely block `maincopyd`; hostile SVG
must not cross the inline-markup trust boundary. The helper is crash and
availability isolation, not a separate operating-system privilege sandbox.

The renderer must also produce revision-bound output. The same accepted input
must produce the same sanitized bytes in preview and public snapshots.

## Evidence

### Adoption and maturity

The evaluation used primary project and registry records on 2026-09-03.
Download counts cover all published versions unless a row says otherwise.

| Candidate | Registry adoption | Repository adoption | Coverage and maturity | Result |
| --- | --- | --- | --- | --- |
| [`mermaid-rs-renderer` 0.3.1](https://crates.io/crates/mermaid-rs-renderer/0.3.1) | 103,174 total downloads, 87,585 `recent_downloads`, and [26 reverse dependencies](https://crates.io/api/v1/crates/mermaid-rs-renderer/reverse_dependencies?page=1&per_page=100) | [1,690 stars and 96 forks](https://api.github.com/repos/1jehuang/mermaid-rs-renderer) | Stable release from 2026-07-06. Upstream documents [23 native diagram types](https://github.com/1jehuang/mermaid-rs-renderer/tree/v0.3.1#diagram-types). The license is MIT. | Select exact 0.3.1. |
| [`merman` 0.8.0-alpha.6](https://crates.io/crates/merman/0.8.0-alpha.6) | 60,429 total downloads, 56,002 `recent_downloads`, and [12 reverse dependencies](https://crates.io/api/v1/crates/merman/reverse_dependencies?page=1&per_page=100) | [537 stars and 29 forks](https://api.github.com/repos/Latias94/merman) | Prerelease from 2026-09-02. Upstream documents [35 families, deterministic metrics, limits, cancellation, and sanitization](https://github.com/Latias94/merman/tree/v0.8.0-alpha.6#determinism-cancellation-and-limits). The license is MIT OR Apache-2.0. | Defer until a stable release proves its current API. |

The crates.io API supplies the recorded
[`mermaid-rs-renderer` counts](https://crates.io/api/v1/crates/mermaid-rs-renderer)
and [`merman` counts](https://crates.io/api/v1/crates/merman). The counts are a
dated selection input, not a live release requirement.

`merman` is the strongest alternative. Its current resource model fits
Maincopy well. [Zed uses it as a Rust Mermaid
backend](https://github.com/zed-industries/zed/pull/57644). However, the
evaluated API was a one-day-old prerelease. Its tagged documentation also
records recent API movement. Maincopy will reassess it after a stable release.

Browser and WebView wrappers did not fit the local runtime boundary. Narrower
native crates had materially less adoption or Mermaid coverage.

### Repository corpus

The spike built the exact 0.3.1 release with SVG-only CLI features:

```console
cargo build --release --locked --no-default-features --features cli
```

The spike extracted all ten Mermaid fences from `docs/design.md` and
`docs/implementation.md`. The corpus contains six flowcharts, two sequence
diagrams, and two state diagrams.

The renderer completed 10 of 10 diagrams. Two fresh processes produced the
same bytes for each diagram under one fixed host environment.

Each measurement selected SVG output and `--fastText`. The SVG-only release
build excluded the PNG renderer and its dependencies.

On an AMD Ryzen 7 PRO 6850U Linux host, process start through SVG completion
took 6–1,038 ms. Raw SVG output ranged from 4,604 to 31,503 bytes. The largest
observed peak resident set size was 8,964 KiB.

These measurements screen the engine only. They do not replace the required
release corpus, Nix checks, or target-platform measurements.

### Determinism and hostile-input findings

[`fast_text_metrics`](https://github.com/1jehuang/mermaid-rs-renderer/blob/v0.3.1/src/layout/text.rs#L417-L421)
avoids font lookup only for ASCII labels. A Unicode fixture produced different
SVG geometry with different Fontconfig inputs. The root SVG width changed from
`372.27106` to `366.992`, and the byte digests changed.

The bundled `mmdr` CLI [merges Mermaid init
configuration](https://github.com/1jehuang/mermaid-rs-renderer/blob/v0.3.1/src/cli.rs#L197-L199).
A five-node fixture used the maximum unsigned `flowchart.orderPasses` value. It
exceeded one second without completion.

The layout code contains [recursive graph
walks](https://github.com/1jehuang/mermaid-rs-renderer/blob/v0.3.1/src/layout/ranking.rs#L527-L541)
and recursive tree walks. A 3,000-node chain aborted with stack overflow under
a 512 KiB child stack.

The renderer can emit active or remotely referencing SVG. Local spike fixtures
produced a `javascript:` anchor and a remote `url(...)` paint value. The
renderer also [emits C4 data-URL
images](https://github.com/1jehuang/mermaid-rs-renderer/blob/v0.3.1/src/render.rs#L4704-L4723).

These results require process isolation and SVG sanitization. Engine popularity
does not grant trust to its output.

## Decision

Maincopy pins `mermaid-rs-renderer` 0.3.1 with default features disabled. The
production helper links the library and calls
[`render_strict`](https://github.com/1jehuang/mermaid-rs-renderer/blob/v0.3.1/src/lib.rs#L274-L279)
with fixed options. Maincopy does not invoke the bundled `mmdr` CLI.

For each post containing diagrams, `maincopyd` first starts one short-lived
`maincopy-mermaid` helper to verify the fixed protocol version. It then starts
one fresh helper for each diagram. The parent owns admission, the wall
deadline, termination, and reaping. Each child owns operating-system resource
limits and one protocol or render operation.

The trust flow is:

```text
validated Mermaid source
  -> supervised maincopy-mermaid helper
  -> untrusted raw SVG
  -> WP6.3 sanitizer
  -> sanitized inline-SVG capability

any failure
  -> reject candidate
  -> retain the active snapshot
```

Raw SVG never receives inline delivery capability. The WP6.3 sanitizer parses
and canonicalizes accepted SVG through an explicit element, attribute, and
value allowlist. It rejects scripts, event attributes, foreign objects, and
remote resource references. HTTPS and root-relative anchors remain inert
navigation; local SVG references are rewritten into a post-and-block-specific
ID namespace.

Maincopy intentionally uses a renderer-specific `quick-xml` policy instead of
a general-purpose markup sanitizer. The v1 boundary must validate
element-specific value grammars, rewrite every local ID and reference, accept
only two digest-pinned embedded images, canonicalize a closed style subset,
enforce structural and byte budgets during parsing and emission, and produce
deterministic bytes. A broader sanitizer would still require this complete
post-validation layer. Reconsider that choice if a maintained SVG sanitizer
can express the entire closed policy and materially reduce the audited code.

Sanitized output contains no inline `style` attributes. The sanitizer converts
the renderer's approved presentation declarations to SVG attributes and maps
`mix-blend-mode: multiply` to one scoped class in Maincopy's own stylesheet.
This keeps the preview CSP and public rendering behavior aligned without an
inline-style exception.

The only accepted embedded PNGs are the two exact C4 person icons shipped by
the pinned renderer, identified by fixed SHA-256 digests. Arbitrary PNG data
URLs remain outside the inline-SVG capability.

### Initial resource targets

These values are enforced implementation limits. Inclusive byte limits accept
a value equal to the stated limit; the wider release stress matrix remains
part of WP6.4.

| Boundary | Selected target | Owner |
| --- | --- | --- |
| Mermaid source | 256 KiB per block | Markdown compiler and helper |
| Mermaid block count | 64 blocks per post | Markdown compiler |
| Accepted raw SVG bytes | 2 MiB | Helper result check and parent reader |
| Output file size | 2 MiB through `RLIMIT_FSIZE` | Helper |
| Address space | 512 MiB through `RLIMIT_AS` | Helper |
| Stack | 16 MiB through `RLIMIT_STACK` | Helper |
| CPU time | 5 seconds through `RLIMIT_CPU` | Helper |
| Core dump | 0 bytes through `RLIMIT_CORE` | Helper |
| Wall time | 6 seconds | Parent supervisor |
| Concurrent helpers | 1 within each renderer instance; the current catalog loop is serial | Parent supervisor |

The 512 MiB address-space limit, not the 2 MiB accepted-output limit, bounds
memory while the engine constructs its result. The concurrency row describes
the current serial catalog path; a process-wide bound under simultaneous
compiler calls remains a WP6.4 release gate.

The helper uses a fixed, versioned file protocol. It must return bounded,
machine-readable failure classes. The parent classifies timeout, signal,
protocol, renderer, and sanitizer failures without parsing diagnostic prose.

### Font environment

The parent gives each helper an empty Fontconfig directory, an isolated cache,
and a versioned `maincopy-fontless-v1` environment marker. With
`fast_text_metrics`, ASCII uses the renderer's fixed metric and Unicode uses its
deterministic no-font fallback. A non-UTF-8 environment path is rejected before
startup so the renderer cannot silently fall back to host Fontconfig data.

Tests must cover ASCII and Unicode labels in fresh processes. They must also
cover empty, populated, and hostile ambient cache directories.

### Init directives

Maincopy v1 rejects every Mermaid `%%{...}%%` directive before helper startup.
It does not silently ignore or merge an init object. Ordinary `%%` comments
remain part of the selected Mermaid syntax.

This policy keeps render options host-owned. It also prevents the bundled CLI's
unbounded `orderPasses` behavior from entering the helper contract.

### Version identity

The opaque renderer identity must bind all output-affecting selections:

- engine name and exact version;
- helper protocol and fixed render options;
- init-directive policy;
- fontless metric policy, Fontconfig input, and cache policy;
- renderer and sanitizer limits;
- sanitizer policy and version.

The post revision includes the exact sanitized inline SVG in the identity-bound
article bytes. A renderer, metric, sanitizer, or policy change must change the
relevant frozen identity tag.

## Consequences

The selected path keeps Node.js and browsers out of the runtime closure. The
helper contains engine crashes, but process isolation does not make SVG safe.

The selection adds a helper binary, process supervision, and license records to
the release closure. Serialized rendering can increase catalog compilation
time for posts with many diagrams.

The two accepted C4 icon payloads are embedded in the renderer's `render.rs`
source and ship under the package's MIT license. The 0.3.1 crate declares no
separate asset license for those byte strings; their exact digests remain part
of Maincopy's renderer-version-specific sanitizer policy.

WP6.2 remains in progress until the full corpus and every limit test pass.
WP6.3 now provides the distinct sanitized-inline-SVG capability and removes
the known inline-style CSP mismatch. Its remaining release work is the full
preview/public equivalence and hostile-candidate retention evidence in WP6.4.

Reconsider `merman` when it publishes a stable release with the evaluated
resource, cancellation, sanitizer, and diagram-family contracts.
