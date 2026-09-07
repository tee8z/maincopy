# Changelog

## Unreleased

Initial release in preparation. See the [implementation plan](docs/implementation.md)
for remaining validation and deployment work.

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
  as a pinned Nix flake.
