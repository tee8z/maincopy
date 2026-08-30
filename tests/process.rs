use std::{fs, process::Command};

use maincopy::{error::ProcessExit, frontend_assets::embedded_manifest};

const TIPPED_PUBLICATION: &str = "[site]\n\
title = \"Process Test\"\n\
base_url = \"https://process.example.test\"\n\
description = \"Process configuration contract.\"\n\
[author]\n\
name = \"Process Tester\"\n\
[tips]\n\
enabled = true\n\
minimum_sats = 1\n\
maximum_sats = 2\n";

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[test]
fn help_exits_successfully_from_the_real_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .arg("--help")
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Success.code().into())
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
}

#[test]
fn serve_help_exits_before_any_configuration_io() {
    let working_directory = tempfile::tempdir().unwrap();
    fs::create_dir(working_directory.path().join("maincopy.toml")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .args([
            "serve",
            "--config",
            "explicit-file-that-must-not-be-opened.toml",
            "--help",
        ])
        .current_dir(working_directory.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Success.code().into())
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
}

#[test]
fn version_exits_successfully_from_the_real_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .arg("--version")
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Success.code().into())
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn unknown_command_uses_the_usage_exit_code() {
    let output = Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .arg("unknown")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(ProcessExit::Usage.code().into()));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand"));
}

#[test]
fn unavailable_admin_transport_uses_the_unavailable_exit_code() {
    let output = Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .args(["admin", "capabilities"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Unavailable.code().into())
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("admin API"));
}

#[test]
fn invalid_serve_configuration_fails_before_the_application_runs() {
    let working_directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .arg("serve")
        .current_dir(working_directory.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Configuration.code().into())
    );
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("host_file_unreadable"));
    assert!(!diagnostic.contains(working_directory.path().to_string_lossy().as_ref()));
}

#[test]
#[cfg(target_os = "linux")]
fn serve_uses_maincopy_toml_from_the_process_working_directory_by_default() {
    let working_directory = tempfile::tempdir().unwrap();
    let selected_content = working_directory.path().join("selected-content");
    let decoy_content = working_directory.path().join("content");
    fs::create_dir(&selected_content).unwrap();
    fs::create_dir(&decoy_content).unwrap();
    fs::write(
        working_directory.path().join("maincopy.toml"),
        "[paths]\ncontent_root = \"selected-content\"\n",
    )
    .unwrap();
    fs::write(
        selected_content.join("publication.toml"),
        TIPPED_PUBLICATION,
    )
    .unwrap();
    fs::write(decoy_content.join("publication.toml"), "unknown = true\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .arg("serve")
        .current_dir(working_directory.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Configuration.code().into())
    );
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("tip_provider_required"));
    assert!(!diagnostic.contains("host_file_unreadable"));
}

#[test]
fn missing_explicit_host_file_does_not_fall_back_to_the_default() {
    let working_directory = tempfile::tempdir().unwrap();
    fs::write(
        working_directory.path().join("maincopy.toml"),
        "unknown = true\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .args(["serve", "--config", "host/missing.toml"])
        .current_dir(working_directory.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Configuration.code().into())
    );
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("host_file_unreadable"));
    assert!(!diagnostic.contains("host_toml_invalid"));
    assert!(!diagnostic.contains("host/missing.toml"));
    assert!(!diagnostic.contains(working_directory.path().to_string_lossy().as_ref()));
}

#[test]
#[cfg(target_os = "linux")]
fn selected_host_file_resolves_file_paths_from_its_parent() {
    let working_directory = tempfile::tempdir().unwrap();
    let host_directory = working_directory.path().join("host");
    let content_directory = host_directory.join("file-content");
    let decoy_content_directory = working_directory.path().join("file-content");
    fs::create_dir_all(&content_directory).unwrap();
    fs::create_dir(&decoy_content_directory).unwrap();
    fs::write(
        host_directory.join("selected.toml"),
        "[paths]\ncontent_root = \"file-content\"\n",
    )
    .unwrap();
    fs::write(
        content_directory.join("publication.toml"),
        TIPPED_PUBLICATION,
    )
    .unwrap();
    fs::write(
        decoy_content_directory.join("publication.toml"),
        "unknown = true\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .args(["serve", "--config", "host/selected.toml"])
        .current_dir(working_directory.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Configuration.code().into())
    );
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("tip_provider_required"));
}

#[test]
#[cfg(target_os = "linux")]
fn command_line_paths_resolve_from_the_process_working_directory() {
    let working_directory = tempfile::tempdir().unwrap();
    let host_directory = working_directory.path().join("host");
    let content_directory = working_directory.path().join("cli-content");
    let decoy_content_directory = host_directory.join("cli-content");
    fs::create_dir_all(&host_directory).unwrap();
    fs::create_dir_all(&content_directory).unwrap();
    fs::create_dir(&decoy_content_directory).unwrap();
    fs::write(host_directory.join("selected.toml"), "").unwrap();
    fs::write(
        content_directory.join("publication.toml"),
        TIPPED_PUBLICATION,
    )
    .unwrap();
    fs::write(
        decoy_content_directory.join("publication.toml"),
        "unknown = true\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .args([
            "serve",
            "--config",
            "host/selected.toml",
            "--content-root",
            "cli-content",
        ])
        .current_dir(working_directory.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Configuration.code().into())
    );
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("tip_provider_required"));
}

#[test]
#[cfg(target_os = "linux")]
fn configured_content_limits_reach_production_discovery() {
    let working_directory = tempfile::tempdir().unwrap();
    let content_directory = working_directory.path().join("content");
    fs::create_dir(&content_directory).unwrap();
    fs::write(
        working_directory.path().join("maincopy.toml"),
        "[content]\npublication_file_bytes = 1\n",
    )
    .unwrap();
    fs::write(
        content_directory.join("publication.toml"),
        "more than one byte",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .arg("serve")
        .current_dir(working_directory.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Validation.code().into())
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("content validation failed"));
}

#[test]
fn process_entry_point_stays_a_tiny_runtime_boundary() {
    let source = include_str!("../src/main.rs");
    let non_blank_lines = source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();

    assert!(non_blank_lines < 20);
    assert!(source.contains("use maincopy::{error::ProcessExit, startup::run_until_stop};"));
    assert!(source.trim_end().ends_with("    run_until_stop().await\n}"));

    for forbidden in ["TcpListener", "UnixListener", "Sqlite", "spawn(", "Router"] {
        assert!(!source.contains(forbidden), "main.rs contains {forbidden}");
    }
}

#[test]
fn production_binary_retains_the_complete_embedded_frontend_manifest() {
    let binary = fs::read(env!("CARGO_BIN_EXE_maincopy")).unwrap();
    let manifest = embedded_manifest();
    manifest.validate().unwrap();

    assert!(contains_bytes(&binary, manifest.css().bytes()));
    assert!(contains_bytes(
        &binary,
        manifest.css().public_path().as_str().as_bytes()
    ));
    assert!(contains_bytes(&binary, manifest.bundle_digest().as_bytes()));
    assert!(contains_bytes(&binary, manifest.css().digest().as_bytes()));
}
