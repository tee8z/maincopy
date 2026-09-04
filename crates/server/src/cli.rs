use std::{num::NonZeroU16, path::PathBuf};

#[cfg(test)]
use std::path::Path;

use clap::{Parser, Subcommand};
use maincopy_shared::source::{
    GitBranchName, RepositoryContentSubdirectory, SourceConfigurationVersion, SourcePollInterval,
    SshCredentialName, SshRemote, SshRemoteHost, SshRemotePort, SshRemoteUser, SshRepositoryPath,
};
use uuid::Uuid;

use crate::domain::{
    auth::{CanonicalUsername, NostrPublicKey},
    source::ManagedSourceConfigurationInput,
};

#[derive(Debug, Parser)]
#[command(name = "maincopyd", version, about = "Run or initialize Maincopy")]
struct ServerArguments {
    /// Read all settings from this host configuration file.
    #[arg(long, value_name = "PATH")]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<ServerCommand>,
}

#[derive(Debug, Subcommand)]
enum ServerCommand {
    /// Perform offline identity operations without starting the server.
    Identity {
        #[command(subcommand)]
        command: IdentityCommand,
    },
    /// Perform offline managed-source setup without starting listeners.
    Source {
        #[command(subcommand)]
        command: SourceCommand,
    },
}

#[derive(Debug, Subcommand)]
enum SourceCommand {
    /// Create or repair the durable non-secret managed-source settings.
    Configure {
        #[arg(long, value_name = "USER")]
        user: SshRemoteUser,
        #[arg(long, value_name = "HOST")]
        host: SshRemoteHost,
        #[arg(long, value_name = "PORT", default_value_t = NonZeroU16::new(22).unwrap())]
        port: NonZeroU16,
        #[arg(long, value_name = "PATH")]
        repository_path: SshRepositoryPath,
        #[arg(long, value_name = "BRANCH")]
        branch: GitBranchName,
        #[arg(long, value_name = "PATH", default_value = ".")]
        content_subdirectory: RepositoryContentSubdirectory,
        #[arg(long, value_name = "NAME")]
        credential_name: SshCredentialName,
        #[arg(
            long,
            value_name = "SECONDS",
            default_value = "300",
            value_parser = parse_source_poll_interval
        )]
        poll_interval_seconds: SourcePollInterval,
        /// Require this current source-settings version when repairing settings.
        #[arg(long, value_name = "POSITIVE_INTEGER", value_parser = parse_source_version)]
        expected_version: Option<SourceConfigurationVersion>,
        /// Retry identity for this offline mutation; generated when omitted.
        #[arg(long, value_name = "UUID")]
        idempotency_key: Option<Uuid>,
    },
    /// Generate a dedicated passwordless Ed25519 deploy key at an explicit path.
    GenerateKey {
        #[arg(long, value_name = "PATH")]
        private_key_file: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum IdentityCommand {
    /// Create the instance identity and first owner account.
    Bootstrap {
        #[command(subcommand)]
        credential: BootstrapCredential,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum BootstrapCredential {
    /// Authenticate the owner with a password read from the terminal or standard input.
    Password {
        /// Canonical login name for the first owner.
        #[arg(long, value_name = "USERNAME")]
        username: CanonicalUsername,
    },
    /// Authenticate the owner with a Nostr public key.
    Nostr {
        /// Lowercase hexadecimal x-only secp256k1 public key.
        #[arg(long, value_name = "HEX_PUBLIC_KEY")]
        public_key: NostrPublicKey,
    },
}

#[derive(Debug)]
pub(crate) enum ServerInvocation {
    Serve {
        config_path: PathBuf,
    },
    BootstrapIdentity {
        config_path: PathBuf,
        credential: BootstrapCredential,
    },
    ConfigureSource {
        config_path: PathBuf,
        request: ManagedSourceConfigurationInput,
        idempotency_key: Uuid,
    },
    GenerateSourceKey {
        config_path: PathBuf,
        private_key_file: PathBuf,
    },
}

pub(crate) fn parse_process_invocation() -> Result<ServerInvocation, clap::Error> {
    ServerArguments::try_parse_from(std::env::args_os()).map(Into::into)
}

impl From<ServerArguments> for ServerInvocation {
    fn from(arguments: ServerArguments) -> Self {
        let ServerArguments { config, command } = arguments;
        match command {
            None => Self::Serve {
                config_path: config,
            },
            Some(ServerCommand::Identity {
                command: IdentityCommand::Bootstrap { credential },
            }) => Self::BootstrapIdentity {
                config_path: config,
                credential,
            },
            Some(ServerCommand::Source {
                command:
                    SourceCommand::Configure {
                        user,
                        host,
                        port,
                        repository_path,
                        branch,
                        content_subdirectory,
                        credential_name,
                        poll_interval_seconds,
                        expected_version,
                        idempotency_key,
                    },
            }) => Self::ConfigureSource {
                config_path: config,
                request: ManagedSourceConfigurationInput {
                    remote: SshRemote {
                        user,
                        host,
                        port: SshRemotePort::new(port.get())
                            .expect("a nonzero CLI port is a valid SSH port"),
                        repository_path,
                    },
                    branch,
                    content_subdirectory,
                    credential_name,
                    poll_interval_seconds,
                    expected_version,
                },
                idempotency_key: idempotency_key.unwrap_or_else(Uuid::new_v4),
            },
            Some(ServerCommand::Source {
                command: SourceCommand::GenerateKey { private_key_file },
            }) => Self::GenerateSourceKey {
                config_path: config,
                private_key_file,
            },
        }
    }
}

fn parse_source_poll_interval(value: &str) -> Result<SourcePollInterval, String> {
    value
        .parse::<u64>()
        .ok()
        .and_then(SourcePollInterval::from_seconds)
        .ok_or_else(|| "must be between 30 and 86400 whole seconds".to_owned())
}

fn parse_source_version(value: &str) -> Result<SourceConfigurationVersion, String> {
    value
        .parse::<u64>()
        .ok()
        .and_then(SourceConfigurationVersion::new)
        .ok_or_else(|| "must be a positive source-configuration version".to_owned())
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory as _, error::ErrorKind};

    use super::*;

    const VALID_NOSTR_PUBLIC_KEY: &str =
        "63fe6318dc58583cfe16810f86dd09e18bfd76aabc24a0081ce2856f330504ed";

    #[test]
    fn server_requires_an_explicit_configuration_path() {
        let error = ServerArguments::try_parse_from(["maincopyd"]).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn offline_bootstrap_requires_an_explicit_configuration_path() {
        let error = ServerArguments::try_parse_from([
            "maincopyd",
            "identity",
            "bootstrap",
            "nostr",
            "--public-key",
            VALID_NOSTR_PUBLIC_KEY,
        ])
        .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn no_subcommand_preserves_the_normal_serve_invocation() {
        let arguments =
            ServerArguments::try_parse_from(["maincopyd", "--config", "host/maincopy.toml"])
                .unwrap();

        assert!(matches!(
            ServerInvocation::from(arguments),
            ServerInvocation::Serve { config_path }
                if config_path.as_path() == std::path::Path::new("host/maincopy.toml")
        ));
    }

    #[test]
    fn bootstrap_requires_exactly_one_typed_credential() {
        let password = ServerArguments::try_parse_from([
            "maincopyd",
            "--config",
            "host/maincopy.toml",
            "identity",
            "bootstrap",
            "password",
            "--username",
            "first-owner",
        ])
        .unwrap();
        assert!(matches!(
            ServerInvocation::from(password),
            ServerInvocation::BootstrapIdentity {
                credential: BootstrapCredential::Password { username },
                ..
            } if username.as_str() == "first-owner"
        ));

        let nostr = ServerArguments::try_parse_from([
            "maincopyd",
            "--config",
            "host/maincopy.toml",
            "identity",
            "bootstrap",
            "nostr",
            "--public-key",
            VALID_NOSTR_PUBLIC_KEY,
        ])
        .unwrap();
        assert!(matches!(
            ServerInvocation::from(nostr),
            ServerInvocation::BootstrapIdentity {
                credential: BootstrapCredential::Nostr { public_key },
                ..
            } if public_key.as_str() == VALID_NOSTR_PUBLIC_KEY
        ));

        for arguments in [
            vec![
                "maincopyd",
                "--config",
                "host/maincopy.toml",
                "identity",
                "bootstrap",
            ],
            vec![
                "maincopyd",
                "--config",
                "host/maincopy.toml",
                "identity",
                "bootstrap",
                "password",
                "--username",
                "first-owner",
                "--password",
                "must-not-be-accepted",
            ],
        ] {
            assert!(ServerArguments::try_parse_from(arguments).is_err());
        }
    }

    #[test]
    fn offline_source_configuration_is_fully_typed_and_contains_no_secret() {
        let arguments = ServerArguments::try_parse_from([
            "maincopyd",
            "--config",
            "host/maincopy.toml",
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
            "deploy",
            "--poll-interval-seconds",
            "90",
            "--expected-version",
            "2",
            "--idempotency-key",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        ])
        .unwrap();

        let ServerInvocation::ConfigureSource {
            request,
            idempotency_key,
            ..
        } = ServerInvocation::from(arguments)
        else {
            panic!("source configure must select the offline source mutation");
        };
        assert_eq!(request.remote.user.as_str(), "git");
        assert_eq!(request.remote.host.as_str(), "git.example.test");
        assert_eq!(request.remote.port.get(), 22);
        assert_eq!(
            request.remote.repository_path.as_str(),
            "publisher/site.git"
        );
        assert_eq!(request.branch.as_str(), "main");
        assert_eq!(request.content_subdirectory.as_str(), "publication");
        assert_eq!(request.credential_name.as_str(), "deploy");
        assert_eq!(request.poll_interval_seconds.seconds(), 90);
        assert_eq!(request.expected_version.unwrap().get(), 2);
        assert_eq!(
            idempotency_key,
            Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap()
        );

        for forbidden in [
            "--private-key",
            "--private-key-file",
            "--known-hosts",
            "--known-hosts-file",
            "--passphrase",
            "--password",
        ] {
            assert!(
                ServerArguments::try_parse_from([
                    "maincopyd",
                    "--config",
                    "maincopy.toml",
                    "source",
                    "configure",
                    forbidden,
                    "secret",
                ])
                .is_err(),
                "source configure accepted {forbidden}"
            );
        }
    }

    #[test]
    fn offline_source_key_generation_accepts_only_an_explicit_output_path() {
        let arguments = ServerArguments::try_parse_from([
            "maincopyd",
            "--config",
            "host/maincopy.toml",
            "source",
            "generate-key",
            "--private-key-file",
            "secrets/maincopy-source",
        ])
        .unwrap();
        assert!(matches!(
            ServerInvocation::from(arguments),
            ServerInvocation::GenerateSourceKey { private_key_file, .. }
                if private_key_file == Path::new("secrets/maincopy-source")
        ));
        assert!(
            ServerArguments::try_parse_from([
                "maincopyd",
                "--config",
                "maincopy.toml",
                "source",
                "generate-key",
            ])
            .is_err()
        );
    }

    #[test]
    fn per_setting_command_line_overrides_are_rejected() {
        for (flag, value) in [
            ("--content-root", "publication"),
            ("--state-root", "state"),
            ("--runtime-root", "run"),
            ("--database-path", "maincopy.db"),
            ("--public-bind", "127.0.0.1:4000"),
            ("--admin-socket", "admin.sock"),
            ("--admin-bind", "127.0.0.1:4001"),
            ("--admin-origin", "https://admin.example.test"),
            ("--database-busy-timeout-ms", "7000"),
            ("--database-writer-queue-capacity", "256"),
            ("--database-read-pool-size", "8"),
            ("--content-publication-file-bytes", "131072"),
            ("--content-post-file-bytes", "2097152"),
            ("--content-asset-file-bytes", "16777216"),
            ("--content-total-tree-bytes", "134217728"),
            ("--content-entries", "5000"),
            ("--content-depth", "8"),
            ("--content-path-bytes", "512"),
        ] {
            let error = ServerArguments::try_parse_from([
                "maincopyd",
                "--config",
                "maincopy.toml",
                flag,
                value,
            ])
            .unwrap_err();

            assert_eq!(error.kind(), ErrorKind::UnknownArgument, "accepted {flag}");
        }
    }

    #[test]
    fn server_help_exposes_config_and_identity_without_overrides_or_password_arguments() {
        let mut command = ServerArguments::command();
        let help = command.render_long_help().to_string();

        assert!(help.contains("--config <PATH>"));
        assert!(help.contains("identity"));
        for forbidden in [
            "--password",
            "--passphrase",
            "--content-root",
            "--state-root",
            "--runtime-root",
            "--database-path",
            "--public-bind",
            "--admin-socket",
            "--admin-bind",
            "--admin-origin",
            "--database-busy-timeout-ms",
            "--database-writer-queue-capacity",
            "--database-read-pool-size",
            "--content-publication-file-bytes",
            "--content-post-file-bytes",
            "--content-asset-file-bytes",
            "--content-total-tree-bytes",
            "--content-entries",
            "--content-depth",
            "--content-path-bytes",
        ] {
            assert!(
                !help.contains(forbidden),
                "server help retained {forbidden}"
            );
        }
    }
}
