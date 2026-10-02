# Changelog

## Unreleased

## 0.1.1

- Replace the large article tip panel with a muted link and collapsed Lightning
  address, copy control, and QR code.
- Enable tips when an administrator saves a Lightning address. Select that user
  automatically when the site has no recipient; clearing the address disables tips.
- Control site tips from the admin profile instead of `publication.toml`.
  Existing publication-level tip flags no longer control visibility; article
  frontmatter can still use `tips = false` to hide tips on that article.

## 0.1.0

Initial release. See the [implementation plan](docs/implementation.md) for remaining
validation work.

- Publish Markdown from Git with private previews, immediate or scheduled releases,
  and control over when updated articles become public.
- Serve article pages, archives, RSS, sitemaps, aliases, code blocks, Mermaid diagrams,
  and optional Lightning tips.
- Manage publishing and accounts through browser administration, the CLI, and scoped
  agent access. Human login supports passwords and Nostr signers.
- Send reviewed article announcements through SES with double opt-in, one-click
  unsubscribe, address removal, suppression, and recovery controls. Email remains
  disabled until provider and privacy acceptance pass.
- Deploy through a NixOS module with HTTPS, private administration, and Prometheus metrics.
- Back up continuously with Litestream and server-encrypted Backblaze B2 checkpoints,
  bounded retention, and verified restore that cannot reactivate old subscriber consent.
- Publish versioned crates and a GitHub release backed by a signed tag, usable
  as a pinned Nix flake, with Linux binary archives for x86_64 and arm64.
