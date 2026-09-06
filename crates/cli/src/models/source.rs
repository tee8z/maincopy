use super::{SourceSyncArguments, SourceSyncInvocation};
use clap::Args;
use maincopy_shared::source::{
    GitBranchName, ReconfigureSourceRequest, RepositoryContentSubdirectory,
    SourceConfigurationVersion, SourcePollInterval, SshCredentialName, SshRemote, SshRemoteHost,
    SshRemotePort, SshRemoteUser, SshRepositoryPath,
};

#[derive(Debug, Args)]
pub(crate) struct SourceConfigurationArguments {
    #[arg(long)]
    user: SshRemoteUser,
    #[arg(long)]
    host: SshRemoteHost,
    #[arg(long, default_value = "22", value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,
    #[arg(long)]
    repository_path: SshRepositoryPath,
    #[arg(long)]
    branch: GitBranchName,
    #[arg(long)]
    content_subdirectory: RepositoryContentSubdirectory,
    #[arg(long)]
    credential_name: SshCredentialName,
    #[arg(long, value_parser = parse_interval)]
    poll_interval_seconds: SourcePollInterval,
    /// Installed version from source status; failed proposals can leave gaps.
    #[arg(long, value_parser = parse_version)]
    expected_version: SourceConfigurationVersion,
    #[command(flatten)]
    completion: SourceSyncArguments,
}

impl SourceConfigurationArguments {
    pub(crate) fn into_invocation(self) -> (ReconfigureSourceRequest, SourceSyncInvocation) {
        (
            ReconfigureSourceRequest {
                remote: SshRemote {
                    user: self.user,
                    host: self.host,
                    port: SshRemotePort::new(self.port)
                        .expect("Clap accepts only a positive SSH port"),
                    repository_path: self.repository_path,
                },
                branch: self.branch,
                content_subdirectory: self.content_subdirectory,
                credential_name: self.credential_name,
                poll_interval_seconds: self.poll_interval_seconds,
                expected_version: self.expected_version,
            },
            self.completion.into_invocation(),
        )
    }
}

fn parse_interval(value: &str) -> Result<SourcePollInterval, String> {
    value
        .parse::<u64>()
        .ok()
        .and_then(SourcePollInterval::from_seconds)
        .ok_or_else(|| "must be between 30 and 86400 seconds".to_owned())
}

fn parse_version(value: &str) -> Result<SourceConfigurationVersion, String> {
    value
        .parse::<u64>()
        .ok()
        .and_then(SourceConfigurationVersion::new)
        .ok_or_else(|| "must be a positive source configuration version".to_owned())
}

#[cfg(test)]
mod tests {
    use crate::models::{Arguments, Command, SourceCommand, SourceSyncDisposition};
    use clap::Parser as _;
    const INPUT: [&str; 20] = [
        "maincopy",
        "source",
        "configure",
        "--user",
        "git",
        "--host",
        "git.example.test",
        "--repository-path",
        "site.git",
        "--branch",
        "main",
        "--content-subdirectory",
        ".",
        "--credential-name",
        "deploy",
        "--poll-interval-seconds",
        "300",
        "--expected-version",
        "3",
        "--wait",
    ];
    #[test]
    fn source_configuration_requires_valid_complete_settings_and_explicit_completion() {
        let parsed = Arguments::try_parse_from(INPUT).unwrap();
        let Command::Source {
            command: SourceCommand::Configure(arguments),
        } = parsed.command
        else {
            panic!("configuration command");
        };
        let (request, completion) = arguments.into_invocation();
        assert_eq!(request.expected_version.get(), 3);
        assert_eq!(request.remote.port.get(), 22);
        assert_eq!(completion.disposition, SourceSyncDisposition::Wait);
        assert!(Arguments::try_parse_from(&INPUT[..19]).is_err());
        for (flag, value) in [
            ("--port", "0"),
            ("--host", "-attacker"),
            ("--poll-interval-seconds", "29"),
            ("--expected-version", "0"),
        ] {
            let mut input = INPUT.to_vec();
            if let Some(index) = input.iter().position(|part| *part == flag) {
                input[index + 1] = value;
            } else {
                input.extend([flag, value]);
            }
            assert!(
                Arguments::try_parse_from(input).is_err(),
                "accepted {flag} {value}"
            );
        }
    }
}
