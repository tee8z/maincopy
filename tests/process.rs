use std::process::Command;

use maincopy::error::ProcessExit;

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
