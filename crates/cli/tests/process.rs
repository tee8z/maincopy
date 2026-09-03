use std::process::Command;

fn maincopy(arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .args(arguments)
        .output()
        .expect("the CLI test binary starts")
}

#[test]
fn removed_local_socket_flags_remain_rejected_by_the_process() {
    for arguments in [
        ["--socket", "admin.sock", "capabilities"].as_slice(),
        ["capabilities", "--socket", "admin.sock"].as_slice(),
    ] {
        let output = maincopy(arguments);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--socket'"));
    }
}

#[test]
fn process_rejects_secret_bearing_arguments_without_echoing_the_secret() {
    for (arguments, secret) in [
        (
            [
                "login",
                "--username",
                "publisher",
                "--password",
                "raw-password-must-not-print",
            ]
            .as_slice(),
            "raw-password-must-not-print",
        ),
        (
            [
                "agent-key",
                "set",
                "--private-key",
                "raw-private-key-must-not-print",
            ]
            .as_slice(),
            "raw-private-key-must-not-print",
        ),
    ] {
        let output = maincopy(arguments);
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains(secret));
    }
}

#[test]
fn removed_insecure_transport_flag_remains_rejected() {
    let output = maincopy(&["--insecure", "capabilities"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--insecure'"));
}

#[test]
fn non_https_admin_origins_fail_before_any_transport_attempt() {
    let output = maincopy(&[
        "--admin-origin",
        "http://admin.example.test",
        "capabilities",
    ]);
    assert_eq!(output.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&output.stderr).contains("canonical HTTPS origin"));
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn malformed_ca_file_fails_as_validation_before_transport() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("invalid.pem");
    std::fs::write(&path, b"not a certificate").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .args([
            "--admin-origin",
            "https://admin.example.test",
            "--admin-ca-file",
        ])
        .arg(&path)
        .arg("capabilities")
        .output()
        .expect("the CLI test binary starts");

    assert_eq!(output.status.code(), Some(65));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("contains 0 certificates"));
    assert!(stderr.contains(path.to_string_lossy().as_ref()));
}

#[test]
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn additional_ca_files_fail_closed_on_unsupported_platforms() {
    let output = maincopy(&[
        "--admin-origin",
        "https://admin.example.test",
        "--admin-ca-file",
        "development-ca.pem",
        "capabilities",
    ]);

    assert_eq!(output.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&output.stderr).contains("supported only on Linux and macOS"));
}

#[test]
fn invalid_origin_is_rejected_before_opening_the_ca_file() {
    let output = maincopy(&[
        "--admin-origin",
        "http://admin.example.test",
        "--admin-ca-file",
        "this-file-does-not-exist.pem",
        "capabilities",
    ]);

    assert_eq!(output.status.code(), Some(65));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("canonical HTTPS origin"));
    assert!(!stderr.contains("certificate"));
}
