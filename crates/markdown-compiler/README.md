# markdown-compiler

Maincopy's content library validates strict TOML frontmatter, resolves local
assets, discovers bounded content trees, and calculates content revision identities.
It also retains immutable candidate archives for publication and recovery.
Whole-tree discovery requires Linux; the single-file validator has a separate path.

The `markdowncompiler` executable validates one Markdown file:

```console
markdowncompiler --json article.md
```

Use `--collection drafts` for a draft document. Single-file validation cannot
detect cross-file identity or route conflicts. It does not load `publication.toml`
or resolve assets across the content tree.

From a repository checkout, build the executable with:

```console
cargo build --locked --release -p markdown-compiler --bin markdowncompiler
```

The server owns HTML rendering, exact previews, release approval, and public
snapshot activation. This crate alone does not publish a website.

Maincopy's current `0.1.0` workspace version is a development version. A registry
release and its compatibility policy have not been announced.

See the [content format](https://github.com/tee8z/maincopy#write-content)
and [Maincopy repository](https://github.com/tee8z/maincopy). The package includes
the project's MIT license.
