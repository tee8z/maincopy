//! Exact rendering output is independent from the helper's wall-clock policy.
//! The process tests cover supervision separately, so instrumentation and
//! concurrent builds do not turn this fixture into a host-speed assertion.

use super::render_source;

const CORPUS: &str = include_str!("../../../server/tests/fixtures/mermaid/selected-corpus.md");
const GOLDEN: &str = include_str!("../../../server/tests/fixtures/mermaid/selected-corpus.golden");

#[test]
fn complete_golden_corpus_is_deterministic() {
    let first = render_corpus();
    let second = render_corpus();

    assert_eq!(first, golden_column(1));
    assert_eq!(second, first);
}

fn render_corpus() -> Vec<String> {
    let diagrams = mermaid_fences(CORPUS);
    assert_eq!(diagrams.len(), 10, "update the reviewed corpus count");
    diagrams
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let svg = render_source(source).unwrap_or_else(|error| {
                panic!("corpus diagram {} must render: {error}", index + 1)
            });
            blake3::hash(svg.as_bytes()).to_hex().to_string()
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
