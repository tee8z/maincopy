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
fn non_https_admin_origins_fail_before_any_transport_attempt() {
    let output = maincopy(&[
        "--admin-origin",
        "http://admin.example.test",
        "capabilities",
    ]);
    assert_eq!(output.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&output.stderr).contains("canonical HTTPS origin"));
}
