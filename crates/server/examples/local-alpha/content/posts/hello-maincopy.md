+++
id = "1dd7559b-90a9-4c5b-a13c-70bf6ec01e92"
title = "Hello, Maincopy"
slug = "hello-maincopy"
aliases = ["welcome"]
authored_at = 2026-08-29T10:15:00-04:00
description = "How Maincopy separates Git content, private previews, release state, and immutable public snapshots."
+++

Maincopy is a self-hosted publishing engine for one site and one canonical
domain. Git owns each article body and its authored metadata. Maincopy controls
when an approved revision becomes public.

This local-alpha article passes through the same compiler, preview, approval,
and publication path as other content. It is also a compact design document for
that path.

## One content model, two source modes

Git is the authority for article content. SQLite is the authority for users,
sessions, release decisions, route ownership, and the current public revision.

Maincopy does not store an editable article body in SQLite. A content reload can
index a new candidate, but it cannot publish that candidate.

```mermaid
flowchart LR
    Git[SSH Git repository] -->|Managed Git mode| Sync[Durable source sync]
    Checkout[Operator checkout] -->|External checkout mode| Candidate[Immutable content candidate]
    Sync -->|Exact commit and subtree| Candidate
    Candidate --> Compiler[Bounded validator and compiler]
    Compiler --> Catalog[Private candidate catalog]
    Catalog --> Preview[Exact private preview]
    Operator[Authenticated operator] --> Preview
    Preview -->|PreviewDigest| Approval[Explicit approval]
    Approval --> Coordinator[Publication coordinator]
    Coordinator --> Writer[Single SQLite writer]
    Coordinator --> Snapshot[Immutable public snapshot]
    Snapshot --> Public[Canonical site and RSS]
```

The compiler confines reads to the configured content root. It rejects unsafe
paths, special files, invalid metadata, and configured limit excesses.

Compilation produces deterministic owned bytes. Public request handlers read
the active snapshot instead of reopening Markdown or querying the source tree.

The checked-in local alpha uses external checkout mode. A managed installation
uses a structured SSH endpoint, one branch, and one selected repository
subdirectory.

## Managed synchronization is durable and read-only

Normal startup, periodic polling, and `Sync now` use one synchronization
coordinator. Concurrent triggers share one durable operation identifier.

```mermaid
sequenceDiagram
    participant T as Startup, poll, or administrator
    participant S as Sync coordinator
    participant D as SQLite sync ledger
    participant G as Git through restricted SSH
    participant C as Candidate pipeline
    participant P as Private catalog

    T->>S: Request sync
    S->>D: Begin or coalesce operation
    D-->>S: SourceSyncId
    S-->>T: SourceSyncId
    S->>G: Fetch configured branch
    G-->>S: Full commit identity
    alt Commit already installed
        opt Startup requires serving state
            S->>C: Load and compile retained candidate
        end
        S->>D: Finish no_change
    else New commit
        S->>C: Materialize selected subtree
        C->>C: Validate and compile
        S->>D: Install candidate and finish applied
        S->>P: Swap private catalog
    end
```

The SSH helper accepts only the configured target, port, and `git-upload-pack`
command. Git fetches only the configured branch and follows no tags or
submodules.

Maincopy shallow-fetches the configured branch head, prunes unreachable Git
objects, and checks the mirror's byte and entry bounds. It does not commit,
merge, push, create a remote branch, or edit Markdown.

The mirror is a bounded transport cache, not an archive. Maincopy reads blobs
from the resolved commit and creates an immutable candidate before compilation;
those content artifacts and database identities preserve review and release
continuity after the mirror moves on.

Candidate retention is itself bounded: Maincopy admits at most 4,096 archive
or staging entries and 1 GiB of archive bytes. Capacity exhaustion rejects a
new preview without deleting a revision that a current or pending publication
may still need.

A live unchanged result skips compilation. Startup still compiles the retained
candidate because the new process needs an in-memory catalog.

A failed fetch or compile keeps the previous candidate and public snapshot.
An interrupted operation remains diagnosable after restart.

## Publication is an explicit state change

An operator reviews one production-faithful preview before publication. The
approval binds the selected revision and the exact rendered preview.

```mermaid
sequenceDiagram
    participant O as Operator
    participant A as Admin UI
    participant P as Preview builder
    participant C as Publication coordinator
    participant D as SQLite writer
    participant S as Public snapshot

    O->>A: Select candidate revision
    A->>P: Build exact preview
    P-->>A: Preview HTML and PreviewDigest
    A-->>O: Show protected preview
    O->>A: Accept exact revision
    A->>C: Publish now with bound digests
    C->>D: Begin activating release
    C->>S: Install candidate snapshot
    C->>D: Commit public revision
```

The `PreviewDigest` covers the post revision and presentation inputs. In this
immediate browser flow, Maincopy rejects publication when the reviewed
candidate, preview, or public site head becomes stale.

An update preserves the original publication time. A reload alone never
replaces the live revision.

The admin contract represents loaded post state with a closed enum:

```rust
pub enum PostPublicationState {
    Draft,
    Unpublished,
    UnpublishedChange,
    Published,
}
```

The browser labels `UnpublishedChange` as `Unpublished changes`. The previous
revision remains public until an approved update activates.

## Trust boundaries

- Host configuration owns source mode, process bounds, and named credential
  file references. Protected host files own key and host-verification bytes.
- SQLite owns non-secret source settings and the durable sync ledger.
- The immutable candidate contains authored Markdown and metadata. Compilation
  cannot escape the selected tree.
- SQLite contains operational and publication state. Article bodies do not
  become editable database records.
- The admin origin serves login, previews, and mutations. Authentication and
  scopes protect private operations; CSRF and exact Origin checks protect
  cookie-backed mutations.
- The public origin serves the canonical reading surface. Drafts, private
  previews, and admin routes return not found.
- The Mermaid helper converts diagrams. Bounded local rendering precedes strict
  SVG sanitization.

The local Caddy gateway terminates HTTPS for separate public and admin virtual
hosts. Both upstream listeners bind to loopback. The local gateway and its
development certificate authority are not a production deployment.

The opaque session token uses a host-only `Secure`, `HttpOnly`, and `SameSite`
cookie. SQLite stores the token digest instead of the raw token.

## Technical content is compiled, not executed

Known code fences receive application-owned language classes. Maincopy escapes
the source and performs no token-level syntax highlighting.

Exact lowercase `mermaid` fences use the supervised local renderer. Raw SVG
cannot enter an article directly from that renderer. A strict sanitizer grants
the capability to embed accepted SVG.

Public article reading does not require JavaScript. The built-in shell provides
server-rendered navigation and content-hashed application assets.

## What the local alpha implements

The current local alpha provides these foundations:

- An operator-maintained local checkout with automatic candidate reloads.
- Generated first-start owner credentials and password browser sign-in.
- Current and candidate revision inspection with exact private previews.
- Immediate initial publication and preview-gated updates.
- An immutable public snapshot with RSS, sitemap, robots policy, and aliases.
- A single SQLite writer with typed commands and bounded query readers.
- Semantic code classes, supervised Mermaid rendering, and sanitized SVG.
- A loopback-only HTTPS development gateway with separate origins.

The local fixture intentionally exercises external checkout mode. The managed
source runbook covers SSH setup, startup synchronization, polling, and
operator-triggered synchronization.

## What remains before V1

The publication backend has scheduling foundations. The browser still needs the
complete schedule, edit, cancel, blocked-retry, profile, and account flows.

Favicon output, image metadata, and public-page Content Security Policy remain
planned. Production also needs NixOS, Caddy, Litestream, metrics, restore
evidence, security review, and release evidence.

Maincopy keeps the two authorities separate: Git defines what an article says,
and SQLite records when one exact revision is public.
