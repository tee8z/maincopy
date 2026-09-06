# Content rendering

Maincopy compiles Markdown into immutable article HTML before publication.
Preview and public output use the same compiled article bytes for the same bound inputs.
Public reading needs no highlighting script, browser renderer, or external rendering service.

## Code fences

The compiler compares the complete decoded fence-info value with a closed alias table.
Comparison is ASCII-case-insensitive. It does not trim whitespace, split trailing tokens, or infer a language from source.

| Canonical language | Accepted aliases |
| --- | --- |
| `bash` | `bash`, `sh`, `shell` |
| `c` | `c` |
| `cpp` | `cpp`, `c++` |
| `csharp` | `csharp`, `cs` |
| `css` | `css` |
| `diff` | `diff`, `patch` |
| `dockerfile` | `dockerfile` |
| `go` | `go` |
| `html` | `html` |
| `java` | `java` |
| `javascript` | `javascript`, `js` |
| `json` | `json` |
| `nix` | `nix` |
| `python` | `python`, `py` |
| `ruby` | `ruby`, `rb` |
| `rust` | `rust`, `rs` |
| `sql` | `sql` |
| `toml` | `toml` |
| `typescript` | `typescript`, `ts` |
| `tsx` | `tsx` |
| `xml` | `xml` |
| `yaml` | `yaml`, `yml` |

Known aliases produce escaped source inside this application-owned structure:

```html
<pre class="article-code"><code class="language-CANONICAL">ESCAPED SOURCE</code></pre>
```

Empty, `text`, `ascii`, unknown, non-ASCII, and multi-token values produce plain escaped code:

```html
<pre><code>ESCAPED SOURCE</code></pre>
```

V1 emits no token spans, inline styles, syntax grammar corpus, or highlighting JavaScript.
The language class comes from application code, never directly from authored text.
Adding an alias changes observable renderer policy and requires updated renderer identity and corpus evidence.

## Mermaid diagrams

Only exact lowercase `mermaid` selects diagram rendering. `Mermaid` remains plain code.
Maincopy rejects every `%%{...}%%` directive before helper startup; ordinary `%%` comments remain valid syntax.
Render options remain application-owned.

The packaged `maincopy-mermaid` helper links pinned `mermaid-rs-renderer` 0.3.1 with default features disabled.
It uses the library's strict renderer with fixed options, not the upstream `mmdr` command.
Keep the helper beside `maincopyd` when installing the package.

One application-owned content compiler shares one helper admission slot across startup, retained recovery, and live synchronization.
It verifies the helper protocol once per renderer instance, then starts a fresh process for each diagram.
The parent owns deadlines, process-group termination, and reaping.
The child installs resource limits before rendering.
This is crash and availability isolation, not a separate operating-system privilege sandbox.

The parent clears inherited environment and creates private Fontconfig input and cache paths for each job.
ASCII uses fixed text metrics; Unicode uses the deterministic no-font fallback.
Host fonts and ambient caches must not change diagram geometry.

## SVG trust boundary

```text
validated Mermaid source -> supervised helper -> untrusted SVG -> sanitizer -> inline SVG
failure -> reject candidate -> preserve active public snapshot
```

The renderer can emit unsafe markup. Its output never receives inline delivery directly.
The sanitizer uses a closed element, attribute, and value policy backed by `quick-xml`.
It rejects scripts, event attributes, foreign objects, and remote resource references.
HTTPS and root-relative anchors remain navigation links. Local IDs and references receive a post-and-block-specific namespace.

Approved presentation declarations become SVG attributes.
The sole supported blend mode maps to an application-owned CSS class, preserving the strict content security policy.
Only the renderer's two digest-pinned C4 person PNGs may appear as embedded images.
Arbitrary image data URLs remain rejected.
The pinned renderer includes these byte strings in its MIT-licensed source.

The dedicated sanitizer must validate element-specific grammars, rewrite references, enforce parsing and emission budgets, and produce canonical bytes.
A general markup sanitizer would still need these checks.
Replace this policy only when another maintained implementation can enforce the complete contract and reduce the code requiring review.

## Enforced limits

Limits are inclusive. Accepted output bytes and helper address space constrain different stages of rendering.

| Boundary | Limit |
| --- | --- |
| All code blocks | 256 per post |
| Final article HTML | 32 MiB per post |
| Mermaid source | 256 KiB per block |
| Mermaid blocks | 64 per post |
| Raw SVG | 2 MiB per block |
| Sanitized SVG | 2 MiB per block; 16 MiB per post |
| SVG structure | 20,000 elements; depth 64; 200,000 attributes; 32 attributes per element |
| SVG references | 20,000 IDs; 100,000 local references |
| SVG text | 1 MiB total; 256 KiB per text node |
| SVG values | 256-byte IDs; 256 KiB paths; 16 KiB embedded PNGs; 2 KiB navigation URLs |
| Absolute coordinates | 10,000,000 |
| Helper output file | 2 MiB |
| Helper address space | 512 MiB |
| Helper stack | 16 MiB |
| Helper CPU | 5 seconds |
| Helper wall time | 10 seconds |
| Helper core dumps | Disabled |
| Concurrent helpers | One per application-owned compiler |

The parent classifies timeout, signal, protocol, renderer, and sanitizer failures without parsing diagnostic prose.
The renderer identity binds the engine, protocol, options, font policy, limits, and sanitizer policy.
Sanitized SVG bytes also participate in the post revision.
Output-affecting changes require updated identity tags and corpus evidence.

The [renderer tests](../crates/diagram-renderer/tests) cover native protocol and concurrent submissions.
The [Markdown renderer](../crates/server/src/render/markdown.rs) and
[SVG sanitizer](../crates/server/src/render/svg.rs) keep their boundary tests beside the policy.
Corpus checks fix output bytes; separate supervisor checks exercise deadlines and reaping.
Passing a small corpus does not establish production throughput or the full release acceptance matrix.

Related: [content images](content-images.md), [system design](design.md#rendering-and-assets),
and [system evidence](system-evidence.md).
