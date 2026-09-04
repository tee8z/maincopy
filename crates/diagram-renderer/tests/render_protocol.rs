#![cfg(all(feature = "client", feature = "helper", target_os = "linux"))]

use maincopy_diagram_renderer::client::{MermaidRenderErrorCode, MermaidRenderer};

const CORPUS: &str = include_str!("../../server/tests/fixtures/mermaid/selected-corpus.md");
const GOLDEN: &str = include_str!("../../server/tests/fixtures/mermaid/selected-corpus.golden");

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

#[test]
fn supervised_helper_contains_a_deeply_recursive_diagram() {
    let mut source = String::from("flowchart LR\n");
    for index in 0..3_000 {
        source.push_str(&format!("N{index}-->N{}\n", index + 1));
    }

    let error = renderer()
        .render(&source)
        .expect_err("the recursive stress fixture must not escape its helper budget");

    assert_eq!(error.code(), MermaidRenderErrorCode::ResourceLimit);
}

#[test]
fn supervised_helper_matches_the_complete_golden_corpus() {
    let first = render_corpus(&renderer());
    let second = render_corpus(&renderer());

    assert_eq!(first, golden_column(1));
    assert_eq!(second, first);
}

fn render_corpus(renderer: &MermaidRenderer) -> Vec<String> {
    let diagrams = mermaid_fences(CORPUS);
    assert_eq!(diagrams.len(), 10, "update the reviewed corpus count");
    diagrams
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let svg = renderer.render(source).unwrap_or_else(|error| {
                panic!("corpus diagram {} must render: {error}", index + 1)
            });
            blake3::hash(svg.as_str().as_bytes()).to_hex().to_string()
        })
        .collect()
}

fn golden_column(index: usize) -> Vec<String> {
    GOLDEN
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            line.split_ascii_whitespace()
                .nth(index)
                .expect("golden corpus row must contain every digest")
                .to_owned()
        })
        .collect()
}

fn mermaid_fences(markdown: &str) -> Vec<String> {
    let mut diagrams = Vec::new();
    let mut source = None;
    for line in markdown.split_inclusive('\n') {
        match (&mut source, line.trim_end_matches(['\r', '\n'])) {
            (None, "```mermaid") => source = Some(String::new()),
            (Some(_), "```") => diagrams.push(source.take().expect("source exists")),
            (Some(source), _) => source.push_str(line),
            (None, _) => {}
        }
    }
    assert!(source.is_none(), "Mermaid fence must be closed");
    diagrams
}
