#![cfg(all(feature = "client", feature = "helper", target_os = "linux"))]

use maincopy_diagram_renderer::client::{MermaidRenderErrorCode, MermaidRenderer};

fn renderer() -> MermaidRenderer {
    MermaidRenderer::from_executable(env!("CARGO_BIN_EXE_maincopy-mermaid"))
        .expect("Cargo must provide an absolute helper path")
}

#[test]
fn supervised_helper_renders_unicode_deterministically() {
    let renderer = renderer();
    let source = "flowchart LR\n    A[Résumé 日本語 🚀] --> B[Published]\n";

    let first = renderer.render(source).expect("first render must succeed");
    let second = renderer.render(source).expect("second render must succeed");

    assert_eq!(first.as_str(), second.as_str());
    assert!(first.as_str().starts_with("<svg "));
    assert!(first.as_str().ends_with("</svg>"));
}

#[test]
fn supervised_helper_returns_typed_author_errors() {
    for source in [
        "notMermaid\nA --> B\n",
        "flowchart LR\nA -->\n",
        "%%{init: { 'theme': 'dark' }}%%\nflowchart LR\nA-->B\n",
    ] {
        let error = renderer().render(source).unwrap_err();
        assert_eq!(error.code(), MermaidRenderErrorCode::InvalidDiagram);
    }
}

#[test]
fn client_rejects_source_before_creating_an_oversized_helper_job() {
    let source = "x".repeat(256 * 1024 + 1);
    let error = renderer().render(&source).unwrap_err();

    assert_eq!(error.code(), MermaidRenderErrorCode::ResourceLimit);
}
