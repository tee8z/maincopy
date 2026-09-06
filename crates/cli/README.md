# maincopy-cli

The `maincopy` executable operates Maincopy's authenticated administration API.
It supports source synchronization, exact publication previews, release controls,
profiles, and identity administration.

This package installs `maincopy`. It does not install the server or rendering
helper. Its Rust library exposes the command runner, not a general API client.

From a repository checkout, build the client with:

```console
cargo build --locked --release -p maincopy-cli --bin maincopy
```

Use `maincopy --help` to inspect the command interface. Follow the
[CLI walkthrough](https://github.com/tee8z/maincopy/blob/master/docs/local-development.md#cli-reference-and-diagnostics)
for configuration, trust, and credential storage.

Maincopy's current `0.1.0` workspace version is a development version. A registry
release and its compatibility policy have not been announced.

The package includes the project's MIT license.
