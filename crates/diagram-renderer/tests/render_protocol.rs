#![cfg(all(feature = "client", feature = "helper", target_os = "linux"))]

use maincopy_diagram_renderer::client::{MermaidRenderErrorCode, MermaidRenderer};
use std::{sync::mpsc, thread};

#[test]
fn shared_renderer_handles_concurrent_protocol_jobs() {
    let renderer = MermaidRenderer::from_executable(env!("CARGO_BIN_EXE_maincopy-mermaid"))
        .expect("Cargo must provide an absolute helper path");
    let cases = [
        (
            "unicode",
            assert_unicode_is_deterministic as fn(&MermaidRenderer),
        ),
        ("author-errors", assert_typed_author_errors),
        ("oversized-source", assert_oversized_source_is_rejected),
        ("recursive-source", assert_recursive_source_is_contained),
    ];

    // One application shares one renderer. Submit concurrent jobs through its
    // real admission queue, so waiting for another job consumes no helper deadline.
    thread::scope(|scope| {
        let jobs: Vec<_> = cases
            .into_iter()
            .map(|(name, check)| {
                let renderer = &renderer;
                let (start, ready) = mpsc::channel();
                let job = thread::Builder::new()
                    .name(name.into())
                    .spawn_scoped(scope, move || {
                        ready.recv().expect("protocol fixture must be released");
                        check(renderer);
                    })
                    .expect("protocol fixture thread must start");
                (job, start)
            })
            .collect();
        for (_, start) in &jobs {
            start.send(()).expect("protocol fixture must be waiting");
        }
        for (job, _) in jobs {
            job.join().expect("concurrent protocol case must pass");
        }
    });
}

fn assert_unicode_is_deterministic(renderer: &MermaidRenderer) {
    let source = "flowchart LR\n    A[Résumé 日本語 🚀] --> B[Published]\n";

    let first = renderer.render(source).expect("first render must succeed");
    let second = renderer.render(source).expect("second render must succeed");

    assert_eq!(first.as_str(), second.as_str());
    assert!(first.as_str().starts_with("<svg "));
    assert!(first.as_str().ends_with("</svg>"));
}

fn assert_typed_author_errors(renderer: &MermaidRenderer) {
    for source in [
        "notMermaid\nA --> B\n",
        "flowchart LR\nA -->\n",
        "%%{init: { 'theme': 'dark' }}%%\nflowchart LR\nA-->B\n",
    ] {
        let error = renderer.render(source).unwrap_err();
        assert_eq!(error.code(), MermaidRenderErrorCode::InvalidDiagram);
    }
}

fn assert_oversized_source_is_rejected(renderer: &MermaidRenderer) {
    let source = "x".repeat(256 * 1024 + 1);
    let error = renderer.render(&source).unwrap_err();

    assert_eq!(error.code(), MermaidRenderErrorCode::ResourceLimit);
}

fn assert_recursive_source_is_contained(renderer: &MermaidRenderer) {
    let mut source = String::from("flowchart LR\n");
    for index in 0..3_000 {
        source.push_str(&format!("N{index}-->N{}\n", index + 1));
    }

    let error = renderer
        .render(&source)
        .expect_err("the recursive stress fixture must not escape its helper budget");

    assert_eq!(error.code(), MermaidRenderErrorCode::ResourceLimit);
}
