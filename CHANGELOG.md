# Changelog

## Unreleased

## 0.1.6

- Share each newly published article as one short teaser on Substack and X.
  Manage both channels and their credentials in the Owner admin portal, without
  host configuration or a restart. The teaser fits a single X post and is listed
  for copying elsewhere. Existing articles are not shared on upgrade.

## 0.1.5

- Send each newly published article once to its active newsletter subscribers.
  Queue multiple articles, preserve delivery progress across restarts, and exclude
  existing articles from upgrade notifications.
- Remove recipient and daily message caps from newsletter settings. Retain
  delivery pacing and signup abuse protection, and share delivery time fairly
  between article updates and confirmation emails.
- Retry explicit provider throttling with a delay. Preserve uncertain delivery
  outcomes to avoid duplicate messages.
- Provide a built-in privacy notice at `/email/privacy`. Leave the optional
  privacy notice URL blank to use the built-in page.

## 0.1.4

- Recover v0.1.0 published snapshots when upgrading the frontend. Verify the
  retained content against the frozen v0.1.0 frontend before installing the new
  presentation; keep approved article revisions and publication settings pinned.

## 0.1.2

- Manage newsletter details, signup mode, and sending limits in the Owner admin
  portal, with durable settings and live updates. Keep provider credentials in
  protected host configuration.
- Make the public postal address optional for mailing lists. Omitted addresses
  leave no empty fields in signup information, campaign review, or message footers.

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
