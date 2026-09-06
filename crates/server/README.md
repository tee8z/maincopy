# maincopy-server

Maincopy is a self-hosted publishing server. Git updates produce private content
candidates. An explicit release approval activates the exact reviewed public snapshot.

This package builds two executables:

- `maincopyd`: the daemon and offline administration commands.
- `maincopy-ssh`: the restricted SSH adapter used for managed Git fetches.

The server also requires the separately built `maincopy-mermaid` helper.
Install matching helpers beside the daemon. Managed Git mode needs Git and
OpenSSH; deploy-key generation needs `ssh-keygen`.

The [NixOS deployment runbook](https://github.com/tee8z/maincopy/blob/master/docs/deployment.md)
provides the complete Linux runtime, gateway, protected state, and encrypted backups.
A standalone Cargo installation does not configure those services.

From a repository checkout, build the runtime executables together:

```console
cargo build --locked --release -p maincopy-server -p maincopy-diagram-renderer
```

Frontend assets and SQLite migrations are included in this crate and embedded
during the build. The Rust modules are server implementation capabilities;
no stable embedding API has been announced.

Maincopy's current `0.1.0` workspace version is a development version. A registry
release and its compatibility policy have not been announced.

The package includes the project's MIT license.
