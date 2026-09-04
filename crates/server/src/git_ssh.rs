use std::{
    env,
    ffi::{OsStr, OsString},
    path::Path,
    process::{Command, ExitCode, Stdio},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

use crate::git_ssh_contract::{
    EXPECTED_PORT_ENV, EXPECTED_REPOSITORY_ENV, EXPECTED_TARGET_ENV, KNOWN_HOSTS_ENV,
    PRIVATE_KEY_ENV, SSH_EXECUTABLE_ENV,
};

const USAGE_EXIT: u8 = 64;
const EXEC_EXIT: u8 = 70;

pub(crate) fn run() -> ExitCode {
    let Some(invocation) = Invocation::from_process() else {
        return ExitCode::from(USAGE_EXIT);
    };
    invocation.execute()
}

struct Invocation {
    ssh_executable: OsString,
    private_key: OsString,
    known_hosts: OsString,
    target: String,
    port: String,
    repository: String,
}

impl Invocation {
    fn from_process() -> Option<Self> {
        let ssh_executable = absolute_environment_path(SSH_EXECUTABLE_ENV)?;
        let private_key = absolute_environment_path(PRIVATE_KEY_ENV)?;
        let known_hosts = absolute_environment_path(KNOWN_HOSTS_ENV)?;
        let target = environment_ascii(EXPECTED_TARGET_ENV)?;
        let port = environment_ascii(EXPECTED_PORT_ENV)?;
        let repository = environment_ascii(EXPECTED_REPOSITORY_ENV)?;
        let git_arguments = utf8_arguments(env::args_os().skip(1))?;
        if !valid_target(&target)
            || !valid_port(&port)
            || !valid_repository(&repository)
            || !valid_git_arguments(&git_arguments, &target, &port, &repository)
        {
            return None;
        }
        Some(Self {
            ssh_executable,
            private_key,
            known_hosts,
            target,
            port,
            repository,
        })
    }

    #[cfg(unix)]
    fn execute(self) -> ExitCode {
        let _error = Command::new(&self.ssh_executable)
            .env_clear()
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .args([
                OsStr::new("-F"),
                OsStr::new("/dev/null"),
                OsStr::new("-T"),
                OsStr::new("-o"),
                OsStr::new("BatchMode=yes"),
                OsStr::new("-o"),
                OsStr::new("CertificateFile=/dev/null"),
                OsStr::new("-o"),
                OsStr::new("GlobalKnownHostsFile=/dev/null"),
                OsStr::new("-o"),
                OsStr::new("IdentityAgent=none"),
                OsStr::new("-o"),
                OsStr::new("IdentityFile=none"),
                OsStr::new("-o"),
                OsStr::new("IdentitiesOnly=yes"),
                OsStr::new("-o"),
                OsStr::new("LogLevel=ERROR"),
                OsStr::new("-o"),
                OsStr::new("PreferredAuthentications=publickey"),
                OsStr::new("-o"),
                OsStr::new("StrictHostKeyChecking=yes"),
                OsStr::new("-o"),
                OsStr::new("UpdateHostKeys=no"),
                OsStr::new("-o"),
                OsStr::new("VerifyHostKeyDNS=no"),
                OsStr::new("-o"),
                OsStr::new("ConnectTimeout=30"),
                OsStr::new("-o"),
            ])
            .arg(user_known_hosts_argument(&self.known_hosts))
            .arg("-i")
            .arg(&self.private_key)
            .arg("-p")
            .arg(&self.port)
            .arg("--")
            .arg(&self.target)
            .arg(upload_pack_command(&self.repository))
            .exec();
        ExitCode::from(EXEC_EXIT)
    }

    #[cfg(not(unix))]
    fn execute(self) -> ExitCode {
        drop(self);
        ExitCode::from(EXEC_EXIT)
    }
}

fn absolute_environment_path(name: &str) -> Option<OsString> {
    let value = env::var_os(name)?;
    (!value.is_empty() && Path::new(&value).is_absolute()).then_some(value)
}

fn environment_ascii(name: &str) -> Option<String> {
    let value = env::var(name).ok()?;
    (!value.is_empty() && value.is_ascii()).then_some(value)
}

fn utf8_arguments(arguments: impl IntoIterator<Item = OsString>) -> Option<Vec<String>> {
    arguments
        .into_iter()
        .map(OsString::into_string)
        .collect::<Result<Vec<_>, _>>()
        .ok()
}

fn valid_git_arguments(
    git_arguments: &[String],
    expected_target: &str,
    expected_port: &str,
    repository: &str,
) -> bool {
    let mut arguments = git_arguments.iter().map(String::as_str).peekable();
    let mut port_seen = false;
    let mut protocol_seen = false;
    while let Some(argument) = arguments.peek() {
        match *argument {
            "-o" => {
                arguments.next();
                if protocol_seen || arguments.next() != Some("SendEnv=GIT_PROTOCOL") {
                    return false;
                }
                protocol_seen = true;
            }
            "-p" => {
                arguments.next();
                if port_seen || arguments.next() != Some(expected_port) {
                    return false;
                }
                port_seen = true;
            }
            _ => break,
        }
    }
    arguments.next() == Some(expected_target)
        && arguments.next() == Some(upload_pack_command(repository).as_str())
        && arguments.next().is_none()
}

fn valid_target(value: &str) -> bool {
    let Some((user, host)) = value.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && !host.is_empty()
        && !host.contains('@')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'-' | b'_' | b'.'))
}

fn valid_port(value: &str) -> bool {
    value.parse::<u16>().is_ok_and(|port| port != 0)
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_repository(value: &str) -> bool {
    let relative = value.strip_prefix('/').unwrap_or(value);
    !relative.is_empty()
        && !relative.starts_with('-')
        && !value.contains("//")
        && relative
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."))
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
}

fn upload_pack_command(repository: &str) -> String {
    format!("git-upload-pack '{repository}'")
}

fn user_known_hosts_argument(path: &OsStr) -> OsString {
    let mut argument = OsString::from("UserKnownHostsFile=");
    argument.push(path);
    argument
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_target_port_and_repository_grammars_are_closed() {
        assert!(valid_target("git@example.test"));
        assert!(!valid_target("-oProxyCommand=x@example.test"));
        assert!(!valid_target("git@example.test extra"));
        assert!(valid_port("22"));
        assert!(!valid_port("0"));
        assert!(!valid_port("22x"));
        assert!(valid_repository("publisher/site.git"));
        assert!(valid_repository("/publisher/site.git"));
        assert!(!valid_repository("../site.git"));
        assert!(!valid_repository("~/site.git"));
        assert!(!valid_repository("publisher/~archive.git"));
        assert!(!valid_repository("site.git;touch-owned"));
    }

    #[test]
    fn upload_pack_command_is_the_only_remote_command_shape() {
        assert_eq!(
            upload_pack_command("publisher/site.git"),
            "git-upload-pack 'publisher/site.git'"
        );
    }

    #[test]
    fn git_may_only_supply_its_protocol_option_and_the_expected_destination() {
        let valid = [
            "-o",
            "SendEnv=GIT_PROTOCOL",
            "git@example.test",
            "git-upload-pack 'publisher/site.git'",
        ]
        .map(str::to_owned);
        assert!(valid_git_arguments(
            &valid,
            "git@example.test",
            "22",
            "publisher/site.git"
        ));

        for injected in [
            vec![
                "-o",
                "ProxyCommand=touch /tmp/owned",
                "git@example.test",
                "git-upload-pack 'publisher/site.git'",
            ],
            vec!["git@example.test", "git-upload-pack 'other.git'"],
            vec![
                "-p",
                "23",
                "git@example.test",
                "git-upload-pack 'publisher/site.git'",
            ],
        ] {
            let injected = injected.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(!valid_git_arguments(
                &injected,
                "git@example.test",
                "22",
                "publisher/site.git"
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_git_arguments_are_rejected_without_panicking() {
        use std::os::unix::ffi::OsStringExt as _;

        assert!(utf8_arguments([OsString::from_vec(vec![0xff])]).is_none());
    }
}
