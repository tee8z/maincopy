use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

const VALID_POST: &str = r#"+++
id = "3d8e6b72-d995-4e5b-b45a-b3be9f28f35d"
title = "A valid post"
slug = "a-valid-post"
authored_at = 2026-08-30T12:00:00Z
description = "A valid post used by the CLI process tests."
+++
# A valid post

The document body.
"#;

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let ordinal = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "maincopy-markdowncompiler-cli-{}-{ordinal}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("the isolated CLI test directory is created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path().join(name);
        fs::write(&path, contents).expect("the CLI test input is written");
        path
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_markdowncompiler"))
}

fn run(markdown: &Path, options: &[&str]) -> Output {
    command()
        .args(options)
        .arg(markdown)
        .output()
        .expect("the markdowncompiler test process starts")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

#[test]
fn help_explains_what_single_file_validation_cannot_check() {
    let output = command()
        .arg("--help")
        .output()
        .expect("the markdowncompiler help process starts");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let help = stdout(&output);
    assert!(help.contains("--collection <COLLECTION>"));
    assert!(help.contains("posts"));
    assert!(help.contains("drafts"));
    assert!(help.contains("cross-file duplicate IDs, slugs, aliases, or routes"));
    assert!(help.contains("publication.toml"));
    assert!(help.contains("asset resolution"));
}

#[test]
fn usage_errors_exit_with_code_two() {
    let missing_path = command().output().expect("the missing-path process starts");
    assert_eq!(missing_path.status.code(), Some(2));
    assert!(missing_path.stdout.is_empty());
    assert!(stderr(&missing_path).contains("Usage:"));

    let invalid_collection = command()
        .args(["--collection", "pages"])
        .output()
        .expect("the invalid-collection process starts");
    assert_eq!(invalid_collection.status.code(), Some(2));
    assert!(invalid_collection.stdout.is_empty());
    assert!(stderr(&invalid_collection).contains("invalid value 'pages'"));
}

#[test]
fn a_valid_post_uses_the_human_success_contract() {
    let directory = TempDirectory::new();
    let markdown = directory.write("valid.md", VALID_POST);

    let output = run(&markdown, &[]);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), format!("{}: valid\n", markdown.display()));
    assert!(output.stderr.is_empty());
}

#[test]
fn human_diagnostics_are_sorted_and_end_with_a_summary() {
    let directory = TempDirectory::new();
    let markdown = directory.write(
        "invalid.md",
        r#"+++
description = ""
title = ""
+++
"#,
    );

    let output = run(&markdown, &[]);

    assert_eq!(output.status.code(), Some(65));
    assert!(output.stdout.is_empty());
    let stderr = stderr(&output);
    let lines: Vec<_> = stderr.lines().collect();
    assert_eq!(lines.len(), 6);
    assert!(lines[0].contains(": id [required_field_missing]:"));
    assert!(lines[1].contains(": title [text_empty]:"));
    assert!(lines[2].contains(": slug [required_field_missing]:"));
    assert!(lines[3].contains(": authored_at [required_field_missing]:"));
    assert!(lines[4].contains(": description [text_empty]:"));
    assert_eq!(
        lines[5],
        format!("{}: invalid (5 diagnostics)", markdown.display())
    );
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with(&markdown.display().to_string()))
    );
}

#[test]
fn human_diagnostics_include_related_locations() {
    let directory = TempDirectory::new();
    let markdown = directory.write(
        "related.md",
        VALID_POST.replace(
            "authored_at = 2026-08-30T12:00:00Z",
            "aliases = [\"a-valid-post\"]\nauthored_at = 2026-08-30T12:00:00Z",
        ),
    );

    let output = run(&markdown, &[]);

    assert_eq!(output.status.code(), Some(65));
    assert!(output.stdout.is_empty());
    let stderr = stderr(&output);
    let lines: Vec<_> = stderr.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains(": aliases[0] [alias_matches_slug]:"));
    assert_eq!(lines[1], format!("  related: {}: slug", markdown.display()));
    assert_eq!(
        lines[2],
        format!("{}: invalid (1 diagnostic)", markdown.display())
    );
}

#[test]
fn json_mode_emits_exactly_one_object_on_stdout_for_invalid_markdown() {
    let directory = TempDirectory::new();
    let markdown = directory.write("invalid.md", "This has no frontmatter.\n");

    let output = run(&markdown, &["--json"]);

    assert_eq!(output.status.code(), Some(65));
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    assert_eq!(output.stdout.last(), Some(&b'\n'));
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout contains one JSON object");
    assert_eq!(report["path"], markdown.display().to_string());
    assert_eq!(report["valid"], false);
    assert_eq!(report["diagnostics"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        report["diagnostics"][0]["code"],
        "frontmatter_opening_delimiter_missing"
    );
    assert_eq!(report["diagnostics"][0]["field"], "$frontmatter");
}

#[test]
fn json_mode_emits_an_empty_diagnostic_list_for_valid_markdown() {
    let directory = TempDirectory::new();
    let markdown = directory.write("valid.md", VALID_POST);

    let output = run(&markdown, &["--json"]);

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout contains one JSON object");
    assert_eq!(report["path"], markdown.display().to_string());
    assert_eq!(report["valid"], true);
    assert_eq!(report["diagnostics"], serde_json::json!([]));
}

#[test]
fn collection_selects_draft_semantics() {
    let directory = TempDirectory::new();
    let markdown = directory.write(
        "draft.md",
        VALID_POST.replace(
            "description = \"A valid post used by the CLI process tests.\"",
            "description = \"A valid post used by the CLI process tests.\"\ndraft = false",
        ),
    );

    let posts_output = run(&markdown, &[]);
    assert_eq!(posts_output.status.code(), Some(0));

    let drafts_output = run(&markdown, &["--collection", "drafts", "--json"]);
    assert_eq!(drafts_output.status.code(), Some(65));
    assert!(drafts_output.stderr.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&drafts_output.stdout)
        .expect("the draft diagnostic is emitted as JSON");
    assert_eq!(report["diagnostics"][0]["code"], "draft_directory_conflict");
}

#[test]
fn invalid_utf8_is_a_document_diagnostic_instead_of_an_io_failure() {
    let directory = TempDirectory::new();
    let markdown = directory.write("invalid-utf8.md", [0xff, 0xfe, 0xfd]);

    let output = run(&markdown, &["--json"]);

    assert_eq!(output.status.code(), Some(65));
    assert!(output.stderr.is_empty());
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("the UTF-8 diagnostic is emitted as JSON");
    assert_eq!(
        report["diagnostics"][0]["code"],
        "content_text_invalid_utf8"
    );
}

#[test]
fn oversized_inputs_are_bounded_document_diagnostics() {
    let directory = TempDirectory::new();
    let markdown = directory.path().join("oversized.md");
    let file = fs::File::create(&markdown).expect("the oversized test input is created");
    file.set_len(4 * 1024 * 1024 + 1)
        .expect("the oversized test input is extended sparsely");

    let output = run(&markdown, &["--json"]);

    assert_eq!(output.status.code(), Some(65));
    assert!(output.stderr.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("the file-size diagnostic is emitted as JSON");
    assert_eq!(report["diagnostics"][0]["code"], "content_file_too_large");
}

#[test]
fn unavailable_inputs_exit_with_code_sixty_six() {
    let directory = TempDirectory::new();
    let missing = directory.path().join("missing.md");

    let missing_output = run(&missing, &[]);
    assert_eq!(missing_output.status.code(), Some(66));
    assert!(missing_output.stdout.is_empty());
    assert!(stderr(&missing_output).contains("input unavailable"));

    let directory_output = run(directory.path(), &[]);
    assert_eq!(directory_output.status.code(), Some(66));
    assert!(directory_output.stdout.is_empty());
    assert!(stderr(&directory_output).contains("not a regular file"));
}

#[test]
fn json_input_errors_are_one_stdout_object_with_no_stderr() {
    let directory = TempDirectory::new();
    let missing = directory.path().join("missing.md");

    let output = run(&missing, &["--json"]);

    assert_eq!(output.status.code(), Some(66));
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("the unavailable-input error is emitted as JSON");
    assert_eq!(report["error"]["path"], missing.display().to_string());
    assert_eq!(report["error"]["code"], "input_unavailable");
    assert!(
        report["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("input unavailable"))
    );
}

#[cfg(unix)]
#[test]
fn permission_denied_inputs_exit_with_code_seventy_seven() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = TempDirectory::new();
    let markdown = directory.write("unreadable.md", VALID_POST);
    let original_permissions = fs::metadata(&markdown)
        .expect("the test input has metadata")
        .permissions();
    fs::set_permissions(&markdown, fs::Permissions::from_mode(0o000))
        .expect("the test input permissions are restricted");

    // Privileged test runners can bypass mode bits, so only assert the process
    // contract when this process is subject to the restriction too.
    if fs::File::open(&markdown).is_err() {
        let output = run(&markdown, &[]);
        assert_eq!(output.status.code(), Some(77));
        assert!(output.stdout.is_empty());
        assert!(stderr(&output).contains("permission denied"));
    }

    fs::set_permissions(&markdown, original_permissions)
        .expect("the test input permissions are restored");
}

#[cfg(target_os = "linux")]
#[test]
fn output_failures_exit_with_code_seventy() {
    use std::{fs::OpenOptions, process::Stdio};

    let directory = TempDirectory::new();
    let markdown = directory.write("valid.md", VALID_POST);
    let full = OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("Linux exposes /dev/full");
    let output = command()
        .arg(&markdown)
        .stdout(Stdio::from(full))
        .output()
        .expect("the markdowncompiler output-error process starts");

    assert_eq!(output.status.code(), Some(70));
    assert!(stderr(&output).contains("failed to write output"));
}
