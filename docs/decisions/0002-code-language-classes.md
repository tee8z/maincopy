# ADR 0002: Project semantic code language classes without token highlighting

Status: accepted and implemented

Decision date: 2026-09-03

## Purpose

This architecture decision record fixes the V1 fenced-code language policy,
HTML shape, trust boundary, and release evidence. The implementation and
representative corpus now enforce this decision.

## Context

Maincopy turns Markdown into immutable article HTML during content
compilation. Authors benefit from stable language metadata for presentation,
copy tools, and later progressive enhancement. V1 does not need token-level
syntax highlighting to provide that metadata.

An authored fence value is untrusted. Maincopy cannot copy it into an HTML
class, use it to select an arbitrary parser, or let it create unescaped markup.
The public article must remain complete without client-side JavaScript.

## Decision

V1 uses one application-owned, closed alias table and no syntax-highlighting
dependency or embedded syntax-grammar or token-color theme corpus. The
compiler compares the complete CommonMark-decoded fence-info value with that
table using ASCII-case-insensitive equality. It does not trim the value, split
trailing tokens, guess from source, or accept non-ASCII aliases.

The accepted values are:

| Canonical value | Accepted fence aliases |
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

Exact lowercase `mermaid` is reserved before this lookup and enters the
supervised diagram-rendering path. `Mermaid` is not a diagram fence. It is an
unknown code fence and follows the plain-code path.

The rendering flow is:

```text
complete decoded fence value
  -> exact lowercase mermaid
     -> supervised renderer and SVG sanitizer
  -> otherwise, closed ASCII-case-insensitive alias table
     -> known alias: static canonical language class and escaped source
     -> unknown alias: escaped plain code
```

A known alias emits this fixed structure:

```html
<pre class="article-code"><code class="language-CANONICAL">ESCAPED SOURCE</code></pre>
```

`CANONICAL` comes only from the closed application enum. Empty, `text`,
`ascii`, unknown, non-ASCII, and multi-token values emit this structure:

```html
<pre><code>ESCAPED SOURCE</code></pre>
```

Both paths escape the source exactly once through the article writer. V1 emits
no token spans, token classes, inline styles, token-color theme data, or
highlighting JavaScript.

## Identity and limits

The post renderer identity has a distinct code-language-class policy field.
Exact pre-injection article bytes are separate post-revision inputs. Changing
either the alias policy or emitted output therefore changes the post, preview,
and downstream site identities.

The inclusive limit of 256 code blocks per post applies before either the
Mermaid or plain-code path. The shared 32 MiB final article HTML limit applies
to every block. The code-language path performs no grammar parsing and needs no
parser-line, region-count, per-language-source, or aggregate highlighted-source
limit.

## Release evidence

The release corpus covers every canonical language and alias family,
ASCII-case variants, unknown and multi-token fallbacks, `text`, `ascii`,
non-ASCII fence values, hostile source text, the code-block-count boundary,
stable output bytes, and preview/public article equality.

The code-language path adds no direct runtime dependency and ships no embedded
syntax-grammar or token-color theme corpus. The repository root `LICENSE`
records Maincopy's license and retains the Mermaid renderer's MIT notice.

## Rejected alternatives

- Copying an arbitrary authored fence value into `class` would make untrusted
  metadata active output policy.
- Automatic language detection would make output depend on heuristics rather
  than the article's declared language.
- Client-side highlighting would add a JavaScript requirement to public
  reading.
- A server-side grammar corpus would add dependency, package, resource-limit,
  and deterministic-output work that V1 does not need.

## Consequences

Known fences expose predictable semantic hooks while their code remains plain,
escaped text. CSS can style the block and language class, but V1 does not color
individual tokens.

Adding a language or alias changes observable renderer policy and requires a
renderer-identity change plus refreshed corpus evidence. Adding token-level
highlighting after V1 requires a separate dependency, limits, output, license,
and upgrade decision; it is not a compatible implementation detail of this
ADR.
