use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::domain::auth::{CanonicalUsername, NostrPublicKey};

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
        }
    }
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
