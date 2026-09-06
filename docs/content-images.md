# Configure content images

Maincopy resolves image references before it installs a candidate.
Use files below `assets/` or HTTPS URLs from an explicit origin allowlist.

Add site metadata to `publication.toml`:

```toml
[site]
title = "Example"
base_url = "https://example.com/"
description = "An example publication."
favicon = "assets/favicon.png"
image = "assets/site-cover.webp"

[author]
name = "Example Author"

[assets]
allowed_https_origins = ["https://images.example.com"]
```

Add an optional `image` field to a post's TOML frontmatter:

```toml
image = "https://images.example.com/article-cover.webp"
```

Keep the post's required frontmatter fields. A local post image can use
`image = "assets/article-cover.webp"` instead.

The site image appears in Open Graph metadata for the index, archive, and tag
pages. A post image appears in article Open Graph and `BlogPosting` JSON-LD.
Missing images remain absent; the favicon is never an image fallback.

Public local URLs contain the active snapshot digest. Preview-only files remain
unavailable from public routes. Private previews use an authenticated favicon
URL and omit public metadata URLs for local images until release.

Use PNG, JPEG, GIF, WebP, AVIF, or ICO files for browser image display.
Authored SVG and unrecognized file types retain attachment delivery.
Maincopy never relaxes asset delivery rules because a file is used in metadata.

External origins must be exact HTTPS origins without paths, credentials, queries,
fragments, wildcards, or CSP directive separators. The generated policy permits
these origins for images and media only. Keep the generated policy below 16 KiB.

External URLs participate in revision identity, but their remote bytes can change
independently. Use local assets when a release must retain the exact image bytes.

Related: [system design](design.md), [local development](local-development.md),
and [engineering style](quality.md).
