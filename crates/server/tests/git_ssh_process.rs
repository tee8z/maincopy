#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::Path,
    process::{Command, Output},
};

const SSH_EXECUTABLE_ENV: &str = "MAINCOPY_SSH_EXECUTABLE";
const PRIVATE_KEY_ENV: &str = "MAINCOPY_SSH_PRIVATE_KEY";
const KNOWN_HOSTS_ENV: &str = "MAINCOPY_SSH_KNOWN_HOSTS";
const EXPECTED_TARGET_ENV: &str = "MAINCOPY_SSH_EXPECTED_TARGET";
const EXPECTED_PORT_ENV: &str = "MAINCOPY_SSH_EXPECTED_PORT";
const EXPECTED_REPOSITORY_ENV: &str = "MAINCOPY_SSH_EXPECTED_REPOSITORY";

fn contract_command(ssh_executable: &Path, root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_maincopy-ssh"));
    command
        .env(SSH_EXECUTABLE_ENV, ssh_executable)
        .env(PRIVATE_KEY_ENV, root.join("source-key"))
        .env(KNOWN_HOSTS_ENV, root.join("known-hosts"))
        .env(EXPECTED_TARGET_ENV, "git@example.test")
        .env(EXPECTED_PORT_ENV, "2222")
        .env(EXPECTED_REPOSITORY_ENV, "publisher/site.git");
    command
}

fn run_valid_git_invocation(mut command: Command) -> Output {
    command
        .args([
            "-o",
            "SendEnv=GIT_PROTOCOL",
            "-p",
            "2222",
            "git@example.test",
            "git-upload-pack 'publisher/site.git'",
        ])
        .output()
        .unwrap()
}

#[test]
fn valid_contract_reaches_only_the_configured_ssh_executable() {
    let root = tempfile::tempdir().unwrap();
    let missing_ssh = root.path().join("missing-ssh");

    let output = run_valid_git_invocation(contract_command(&missing_ssh, root.path()));

    assert_eq!(output.status.code(), Some(70));
    assert!(output.stdout.is_empty());
}

#[test]
fn constrained_ssh_execution_clears_contract_secrets_and_forwards_fixed_arguments() {
    let root = tempfile::tempdir().unwrap();
    let fake_ssh = root.path().join("ssh");
    fs::write(
        &fake_ssh,
        "#!/bin/sh\n\
         if [ \"${MAINCOPY_SSH_EXECUTABLE+x}\" = x ] || \
            [ \"${MAINCOPY_SSH_PRIVATE_KEY+x}\" = x ] || \
            [ \"${MAINCOPY_SSH_KNOWN_HOSTS+x}\" = x ] || \
            [ \"${MAINCOPY_SSH_EXPECTED_TARGET+x}\" = x ] || \
            [ \"${MAINCOPY_SSH_EXPECTED_PORT+x}\" = x ] || \
            [ \"${MAINCOPY_SSH_EXPECTED_REPOSITORY+x}\" = x ]; then\n\
           exit 91\n\
         fi\n\
         printf '%s\\n' \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_ssh, fs::Permissions::from_mode(0o700)).unwrap();

    let output = run_valid_git_invocation(contract_command(&fake_ssh, root.path()));

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let arguments = String::from_utf8(output.stdout).unwrap();
    assert!(arguments.contains("BatchMode=yes\n"));
    assert!(arguments.contains("CertificateFile=/dev/null\n"));
    assert!(arguments.contains("GlobalKnownHostsFile=/dev/null\n"));
    assert!(arguments.contains("IdentityAgent=none\n"));
    assert!(arguments.contains("IdentityFile=none\n"));
    assert!(arguments.contains("IdentitiesOnly=yes\n"));
    assert!(arguments.contains("PreferredAuthentications=publickey\n"));
    assert!(arguments.contains("StrictHostKeyChecking=yes\n"));
    assert!(arguments.contains("UpdateHostKeys=no\n"));
    assert!(arguments.contains("VerifyHostKeyDNS=no\n"));
    assert!(!arguments.lines().any(|argument| argument == "-4"));
    assert!(arguments.contains(&format!(
        "UserKnownHostsFile={}\n",
        root.path().join("known-hosts").display()
    )));
    assert!(arguments.contains(&format!("{}\n", root.path().join("source-key").display())));
    assert!(arguments.ends_with("git@example.test\ngit-upload-pack 'publisher/site.git'\n"));
}

#[test]
fn malformed_contract_or_git_arguments_never_start_ssh() {
    let root = tempfile::tempdir().unwrap();
    let fake_ssh = root.path().join("ssh");
    fs::write(&fake_ssh, "#!/bin/sh\nexit 99\n").unwrap();
    fs::set_permissions(&fake_ssh, fs::Permissions::from_mode(0o700)).unwrap();

    let mut missing_repository = contract_command(&fake_ssh, root.path());
    missing_repository.env_remove(EXPECTED_REPOSITORY_ENV);
    let output = run_valid_git_invocation(missing_repository);
    assert_eq!(output.status.code(), Some(64));

    let mut relative_private_key = contract_command(&fake_ssh, root.path());
    relative_private_key.env(PRIVATE_KEY_ENV, "relative-source-key");
    let output = run_valid_git_invocation(relative_private_key);
    assert_eq!(output.status.code(), Some(64));

    let output = contract_command(&fake_ssh, root.path())
        .args([
            "-o",
            "ProxyCommand=touch-owned",
            "git@example.test",
            "git-upload-pack 'publisher/site.git'",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(64));
}
