# Maincopy

**A self-hosted blog you publish from Git.**

Write articles in Markdown with your preferred editor, keep them in Git, and
publish them on your own domain. Maincopy provides the website and a private
administration screen for reviewing, publishing, and scheduling your writing.
You can also manage the site through the `maincopy` CLI or a scoped agent account.

A Git push prepares an update for review. You choose when the reviewed article
goes live, immediately or at a scheduled time. Readers get article pages,
an archive, RSS, and a sitemap. Code blocks, Mermaid diagrams, images, and
optional Lightning tips are supported.

Maincopy runs one site per instance. It uses SQLite for accounts and publication
state, with optional encrypted backups to Backblaze B2. The NixOS module supplies
the server, HTTPS gateway, and background services.

**Status:** preparing the first release. Email announcements through SES include
double opt-in and unsubscribe controls, but remain disabled until deployment
acceptance. See [what remains for v1](docs/implementation.md).

## Try it locally

On Linux with Nix installed:

```sh
git clone https://github.com/tee8z/maincopy.git
cd maincopy
nix develop -c just start
```

The launcher builds Maincopy, starts an example site, and installs a local
certificate authority in supported browser stores. Keep the terminal open.
On first startup, save the generated `owner` password shown there.
Wait for `Maincopy development environment is ready` before signing in.

1. Open [the administration screen](https://admin.localhost:8443/admin/login) and sign in as `owner`.
2. Select **Review exact preview** for **Hello, Maincopy**, then open the preview.
3. Continue to publication confirmation, accept the reviewed preview, and approve the revision.
4. Open [the published article](https://maincopy.localhost:8443/posts/hello-maincopy).

Press **Ctrl+C** to stop. Running `just start` again preserves your local state.
The [local guide](docs/local-development.md) covers CLI login, agents, certificate
trust, troubleshooting, and resetting the example.

![An article published with Maincopy](docs/images/published-article.png)

## Use it for your own site

Create a content repository with `publication.toml` for your site settings and
Markdown articles under `posts/`. Each article has TOML frontmatter for its stable
ID, title, slug, date, and description. Follow the [content guide](docs/content-rendering.md)
and the [included example](crates/server/examples/development/content).

Deploy Maincopy, connect the content repository, and use the same review-and-publish
workflow. Managed Git mode fetches your selected branch with a read-only deploy
key. You can also supply a local checkout maintained by your own tools.

| Task | Guide |
| --- | --- |
| Set up the home server and HTTPS | [Deployment](docs/deployment.md) |
| Connect a Git repository | [Managed Git](docs/managed-source.md) |
| Configure email announcements | [Email](docs/email-delivery.md) |
| Back up or restore the site | [Backup and restore](docs/backup-restore.md) |
| Monitor the running service | [Observability](docs/observability.md) |
| Install or publish a versioned release | [Releases](docs/release.md) |

Keep administration on a private network or VPN. The deployment guide configures
separate public, administration, and metrics access.

## Develop Maincopy

Maincopy is written in Rust. `maincopyd` runs the server; `maincopy` is its
administration client. Start with the [design](docs/design.md) and
[quality rules](docs/quality.md), then check the [implementation plan](docs/implementation.md).

```sh
nix develop -c cargo test --locked --workspace --all-targets --all-features
nix flake check --print-build-logs
nix build --print-build-logs
```

The flake supports Linux x86_64 and ARM64. Release preparation and installation
instructions are in the [release guide](docs/release.md).
