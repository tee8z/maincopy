use std::{
    fs,
    io::Write as _,
    net::TcpListener,
    process::{Command, Output, Stdio},
};

use maincopy_server::{error::ProcessExit, frontend_assets::embedded_manifest};

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn write_offline_host_file(
    root: &tempfile::TempDir,
    public_listener: &TcpListener,
    admin_listener: &TcpListener,
) {
    fs::write(
        root.path().join("maincopy.toml"),
        format!(
            "[paths]\n\
             content_root = \"content-that-does-not-exist\"\n\
             state_root = \"state\"\n\
             runtime_root = \"run\"\n\
             [public]\n\
             bind = \"{}\"\n\
             [admin]\n\
             bind = \"{}\"\n\
             origin = \"https://admin.localhost\"\n",
            public_listener.local_addr().unwrap(),
            admin_listener.local_addr().unwrap(),
        ),
    )
    .unwrap();
}

fn run_password_bootstrap(root: &tempfile::TempDir, password: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args([
            "--config",
            "maincopy.toml",
            "identity",
            "bootstrap",
            "password",
            "--username",
            "first-owner",
        ])
        .current_dir(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{password}\n").as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn write_managed_source_host_file(root: &tempfile::TempDir) {
    fs::write(
        root.path().join("maincopy.toml"),
        "[paths]\n\
         state_root = \"state\"\n\
         runtime_root = \"run\"\n\
         [source]\n\
         mode = \"managed_git\"\n\
         mirror_root = \"state/source-mirror\"\n\
         [source.ssh_credentials.deploy]\n\
         private_key_file = \"keys/source-key\"\n\
         known_hosts_file = \"keys/known-hosts\"\n",
    )
    .unwrap();
}

fn run_source_configuration(
    root: &tempfile::TempDir,
    credential_name: &str,
    expected_version: Option<&str>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_maincopyd"));
    command.args([
        "--config",
        "maincopy.toml",
        "source",
        "configure",
        "--user",
        "git",
        "--host",
        "git.example.test",
        "--repository-path",
        "publisher/site.git",
        "--branch",
        "main",
        "--content-subdirectory",
        "publication",
        "--credential-name",
        credential_name,
        "--poll-interval-seconds",
        "300",
    ]);
    if let Some(expected_version) = expected_version {
        command.args(["--expected-version", expected_version]);
    }
    command.current_dir(root.path()).output().unwrap()
}

#[test]
fn help_exits_successfully_from_the_real_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
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
fn help_with_an_explicit_config_exits_before_any_configuration_io() {
    let working_directory = tempfile::tempdir().unwrap();
    fs::create_dir(working_directory.path().join("maincopy.toml")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args([
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
    let output = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
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
fn offline_password_bootstrap_needs_no_content_or_listener_and_is_single_use() {
    const PASSWORD: &str = "correct horse battery staple";

    let root = tempfile::tempdir().unwrap();
    let public_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let admin_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    write_offline_host_file(&root, &public_listener, &admin_listener);

    let first = run_password_bootstrap(&root, PASSWORD);
    assert_eq!(
        first.status.code(),
        Some(ProcessExit::Success.code().into())
    );
    assert!(!contains_bytes(&first.stdout, PASSWORD.as_bytes()));
    assert!(!contains_bytes(&first.stderr, PASSWORD.as_bytes()));
    assert!(root.path().join("state/maincopy.db").is_file());
    assert!(!root.path().join("content-that-does-not-exist").exists());

    let second = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args([
            "--config",
            "maincopy.toml",
            "identity",
            "bootstrap",
            "password",
            "--username",
            "first-owner",
        ])
        .current_dir(root.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        second.status.code(),
        Some(ProcessExit::Conflict.code().into())
    );
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("identity bootstrap is already complete")
    );
    assert!(!contains_bytes(&second.stdout, PASSWORD.as_bytes()));
    assert!(!contains_bytes(&second.stderr, PASSWORD.as_bytes()));
}

#[test]
fn offline_password_bootstrap_redacts_invalid_password_input() {
    const INVALID_PASSWORD: &str = "too-short";

    let root = tempfile::tempdir().unwrap();
    let public_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let admin_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    write_offline_host_file(&root, &public_listener, &admin_listener);

    let output = run_password_bootstrap(&root, INVALID_PASSWORD);

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Validation.code().into())
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("owner credential input is invalid"));
    assert!(!contains_bytes(&output.stdout, INVALID_PASSWORD.as_bytes()));
    assert!(!contains_bytes(&output.stderr, INVALID_PASSWORD.as_bytes()));
}

#[test]
fn offline_nostr_owner_bootstrap_accepts_the_typed_public_key() {
    const PUBLIC_KEY: &str = "63fe6318dc58583cfe16810f86dd09e18bfd76aabc24a0081ce2856f330504ed";

    let root = tempfile::tempdir().unwrap();
    let public_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let admin_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    write_offline_host_file(&root, &public_listener, &admin_listener);

    let output = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args([
            "--config",
            "maincopy.toml",
            "identity",
            "bootstrap",
            "nostr",
            "--public-key",
            PUBLIC_KEY,
        ])
        .current_dir(root.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Success.code().into())
    );
    assert!(root.path().join("state/maincopy.db").is_file());
    assert!(!root.path().join("content-that-does-not-exist").exists());
}

#[test]
#[cfg(unix)]
fn offline_source_key_generation_is_private_verifiable_and_never_overwrites() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    write_managed_source_host_file(&root);
    let arguments = [
        "--config",
        "maincopy.toml",
        "source",
        "generate-key",
        "--private-key-file",
        "keys/source-key",
    ];
    let first = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args(arguments)
        .current_dir(root.path())
        .output()
        .unwrap();
    assert_eq!(
        first.status.code(),
        Some(ProcessExit::Success.code().into())
    );

    let private_path = root.path().join("keys/source-key");
    let public_path = root.path().join("keys/source-key.pub");
    let private_key = fs::read(&private_path).unwrap();
    let public_key = fs::read_to_string(&public_path).unwrap();
    assert_eq!(
        fs::metadata(&private_path).unwrap().permissions().mode() & 0o077,
        0
    );
    assert!(!contains_bytes(&first.stdout, &private_key));
    assert!(!contains_bytes(&first.stderr, &private_key));

    let derived = Command::new("ssh-keygen")
        .args(["-y", "-f"])
        .arg(&private_path)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(derived.status.success());
    let derived = String::from_utf8(derived.stdout).unwrap();
    let mut derived_fields = derived.split_ascii_whitespace();
    let derived = format!(
        "{} {}",
        derived_fields.next().unwrap(),
        derived_fields.next().unwrap()
    );
    assert!(public_key.starts_with(&derived));
    assert!(
        String::from_utf8_lossy(&first.stdout).contains(&format!("Public key: {derived}")),
        "key-generation output did not contain the derived public key: {}",
        String::from_utf8_lossy(&first.stdout),
    );

    let fingerprint = Command::new("ssh-keygen")
        .args(["-l", "-E", "sha256", "-f"])
        .arg(&public_path)
        .output()
        .unwrap();
    assert!(fingerprint.status.success());
    let fingerprint = String::from_utf8(fingerprint.stdout)
        .unwrap()
        .split_ascii_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    assert!(
        String::from_utf8_lossy(&first.stdout).contains(&format!("Fingerprint: {fingerprint}"))
    );

    let second = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args(arguments)
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert_eq!(fs::read(&private_path).unwrap(), private_key);
    assert_eq!(fs::read_to_string(&public_path).unwrap(), public_key);
}

#[test]
fn offline_source_configuration_rejects_an_unregistered_credential_before_database_io() {
    let root = tempfile::tempdir().unwrap();
    write_managed_source_host_file(&root);

    let output = run_source_configuration(&root, "missing", None);

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Validation.code().into())
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("selected source credential is not registered")
    );
    assert!(!root.path().join("state/maincopy.db").exists());
}

#[test]
fn offline_source_repair_reports_conflicts_and_preserves_the_current_version() {
    const PASSWORD: &str = "correct horse battery staple";

    let root = tempfile::tempdir().unwrap();
    write_managed_source_host_file(&root);
    assert!(run_password_bootstrap(&root, PASSWORD).status.success());
    assert!(
        run_source_configuration(&root, "deploy", None)
            .status
            .success()
    );

    for expected_version in [None, Some("99")] {
        let conflict = run_source_configuration(&root, "deploy", expected_version);
        assert_eq!(
            conflict.status.code(),
            Some(ProcessExit::Conflict.code().into())
        );
        assert!(
            String::from_utf8_lossy(&conflict.stderr)
                .contains("source configuration conflicts with durable state")
        );
    }

    assert!(
        run_source_configuration(&root, "deploy", Some("1"))
            .status
            .success()
    );
}

#[test]
fn unknown_server_subcommand_uses_the_usage_exit_code() {
    let output = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .arg("unknown")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(ProcessExit::Usage.code().into()));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand 'unknown'"));
}

#[test]
fn missing_explicit_config_uses_the_usage_exit_without_configuration_io() {
    let working_directory = tempfile::tempdir().unwrap();
    fs::write(
        working_directory.path().join("maincopy.toml"),
        "unknown = true\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .current_dir(working_directory.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(ProcessExit::Usage.code().into()));
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("--config <PATH>"));
    assert!(!diagnostic.contains("host_toml_invalid"));
}

#[test]
#[cfg(target_os = "linux")]
fn removed_payment_provider_in_the_selected_host_file_is_rejected_before_discovery() {
    let working_directory = tempfile::tempdir().unwrap();
    let selected_content = working_directory.path().join("selected-content");
    let decoy_content = working_directory.path().join("content");
    fs::create_dir(&selected_content).unwrap();
    fs::create_dir(&decoy_content).unwrap();
    fs::write(
        working_directory.path().join("maincopy.toml"),
        "[paths]\ncontent_root = \"selected-content\"\n\
         [lightning]\nprovider = \"lexe\"\n",
    )
    .unwrap();
    fs::write(
        selected_content.join("publication.toml"),
        "unknown = true\n",
    )
    .unwrap();
    fs::write(decoy_content.join("publication.toml"), "unknown = true\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args(["--config", "maincopy.toml"])
        .current_dir(working_directory.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Configuration.code().into())
    );
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("host_toml_invalid"));
    assert!(!diagnostic.contains("selected-content"));
    assert!(!diagnostic.contains("lexe"));
}

#[test]
fn missing_selected_host_file_does_not_open_an_unselected_file() {
    let working_directory = tempfile::tempdir().unwrap();
    fs::write(
        working_directory.path().join("maincopy.toml"),
        "unknown = true\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args(["--config", "host/missing.toml"])
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
        "unknown = true\n",
    )
    .unwrap();
    fs::write(
        decoy_content_directory.join("publication.toml"),
        "unknown = true\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args(["--config", "host/selected.toml"])
        .current_dir(working_directory.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(ProcessExit::Validation.code().into())
    );
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("content validation failed"));
}

#[test]
#[cfg(target_os = "linux")]
fn per_setting_override_is_rejected_before_configuration_io() {
    let working_directory = tempfile::tempdir().unwrap();
    fs::create_dir(working_directory.path().join("selected.toml")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args(["--config", "selected.toml", "--content-root", "cli-content"])
        .current_dir(working_directory.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(ProcessExit::Usage.code().into()));
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("unexpected argument '--content-root'"));
    assert!(!diagnostic.contains("host_file_unreadable"));
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

    let output = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args(["--config", "maincopy.toml"])
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
    assert!(source.contains("use maincopy_server::{error::ProcessExit, startup::run_until_stop};"));
    assert!(source.trim_end().ends_with("    run_until_stop().await\n}"));

    for forbidden in ["TcpListener", "UnixListener", "Sqlite", "spawn(", "Router"] {
        assert!(!source.contains(forbidden), "main.rs contains {forbidden}");
    }
}

#[test]
fn production_binary_retains_the_complete_embedded_frontend_manifest() {
    let binary = fs::read(env!("CARGO_BIN_EXE_maincopyd")).unwrap();
    let manifest = embedded_manifest();
    manifest.validate().unwrap();

    assert!(contains_bytes(&binary, manifest.css.bytes));
    assert!(contains_bytes(&binary, manifest.css.public_path.as_bytes()));
    assert!(contains_bytes(&binary, manifest.bundle_digest.as_bytes()));
    assert!(contains_bytes(&binary, manifest.css.digest.as_bytes()));
}
