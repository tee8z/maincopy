# maincopy-diagram-renderer

An isolated Mermaid rendering helper and its supervised client for Maincopy.
The `maincopy-mermaid` executable uses a bounded file protocol. The client
controls execution time, process termination, input size, and output size.

The `client` feature exposes `MermaidRenderer`. The `helper` feature builds
`maincopy-mermaid`. Both features are enabled by default.

From a repository checkout, build the helper with:

```console
cargo build --locked --release -p maincopy-diagram-renderer --bin maincopy-mermaid
```

Install the helper beside `maincopyd`, or configure its absolute path through
`MAINCOPY_MERMAID_HELPER`. The production server package uses the client feature;
Cargo does not install dependency executables automatically.

Rendering returns untrusted SVG bytes. Callers must validate those bytes before
serving them. Maincopy applies its own SVG policy after rendering.

Linux is the validated production platform. Maincopy's current `0.1.0` workspace
version is a development version, without an announced registry release.

See the [Maincopy repository](https://github.com/tee8z/maincopy) for the complete
runtime package. This package includes the project's MIT license.
