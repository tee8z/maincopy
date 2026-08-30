#[cfg(any(unix, windows))]
fn main() -> std::process::ExitCode {
    local::run()
}

#[cfg(not(any(unix, windows)))]
fn main() -> std::process::ExitCode {
    use std::io::Write as _;

    let _ = writeln!(
        std::io::stderr().lock(),
        "maincopy: the private admin client requires a supported local transport"
    );
    std::process::ExitCode::from(69)
}

#[cfg(any(unix, windows))]
mod local {
    use std::{
        io::{self, Write as _},
        path::PathBuf,
        process::ExitCode,
    };

    use clap::{Parser, Subcommand};
    use maincopy_cli::{AdminClient, AdminClientError};
    #[cfg(windows)]
    use maincopy_shared::DEFAULT_WINDOWS_ADMIN_PIPE;
    use maincopy_shared::{AdminApiVersion, Capabilities, CapabilityContractVersion};
    use serde_json::json;
    use thiserror::Error;

    const SUCCESS: u8 = 0;
    const VALIDATION: u8 = 65;
    const UNAVAILABLE: u8 = 69;
    const INTERNAL: u8 = 70;
    const CONFLICT: u8 = 75;
    const CONFIGURATION: u8 = 78;
    #[cfg(unix)]
    const DEFAULT_ADMIN_ENDPOINT: &str = "run/admin.sock";
    #[cfg(windows)]
    const DEFAULT_ADMIN_ENDPOINT: &str = DEFAULT_WINDOWS_ADMIN_PIPE;

    #[derive(Debug, Parser)]
    #[command(
        name = "maincopy",
        version,
        about = "Operate a running Maincopy server."
    )]
    struct Arguments {
        /// Connect through this private local admin socket or named pipe.
        #[arg(
            long,
            global = true,
            value_name = "PATH",
            default_value = DEFAULT_ADMIN_ENDPOINT
        )]
        socket: PathBuf,

        /// Write one machine-readable JSON document.
        #[arg(long, global = true)]
        json: bool,

        #[command(subcommand)]
        command: Command,
    }

    #[derive(Clone, Copy, Debug, Subcommand)]
    enum Command {
        /// Report the API versions supported by the running server.
        Capabilities,
    }

    #[derive(Debug, Error)]
    enum CliError {
        #[error(transparent)]
        Admin(#[from] AdminClientError),

        #[error("failed to write command output: {0}")]
        Output(#[from] io::Error),

        #[error("failed to encode command output: {0}")]
        Encode(#[from] serde_json::Error),
    }

    #[tokio::main(flavor = "current_thread")]
    pub async fn run() -> ExitCode {
        let arguments = Arguments::parse();
        let json = arguments.json;
        let result = execute(arguments).await.and_then(|capabilities| {
            write_capabilities(std::io::stdout().lock(), capabilities, json)
        });

        match result {
            Ok(()) => ExitCode::from(SUCCESS),
            Err(error) => {
                let exit = error_exit(&error);
                if report_error(&error, exit, json).is_err() {
                    return ExitCode::from(INTERNAL);
                }
                ExitCode::from(exit)
            }
        }
    }

    async fn execute(arguments: Arguments) -> Result<Capabilities, CliError> {
        let client = AdminClient::new(arguments.socket)?;
        match arguments.command {
            Command::Capabilities => client.capabilities().await.map_err(CliError::from),
        }
    }

    fn write_capabilities(
        mut output: impl io::Write,
        capabilities: Capabilities,
        json: bool,
    ) -> Result<(), CliError> {
        if json {
            serde_json::to_writer(&mut output, &capabilities)?;
            writeln!(output)?;
            return Ok(());
        }

        let api_version = match capabilities.api_version {
            AdminApiVersion::V1 => "v1",
        };
        let capability_version = match capabilities.features.capabilities {
            CapabilityContractVersion::V1 => "v1",
        };
        writeln!(output, "Admin API: {api_version}")?;
        writeln!(output, "Capabilities contract: {capability_version}")?;
        Ok(())
    }

    fn error_exit(error: &CliError) -> u8 {
        let CliError::Admin(error) = error else {
            return INTERNAL;
        };

        match error {
            AdminClientError::InvalidSocketPath => CONFIGURATION,
            AdminClientError::Request { .. } => UNAVAILABLE,
            AdminClientError::HttpStatus { status }
                if matches!(status.as_u16(), 400 | 404 | 405 | 422) =>
            {
                VALIDATION
            }
            AdminClientError::HttpStatus { status } if matches!(status.as_u16(), 409 | 412) => {
                CONFLICT
            }
            AdminClientError::HttpStatus { status } if matches!(status.as_u16(), 502..=504) => {
                UNAVAILABLE
            }
            AdminClientError::Build(_)
            | AdminClientError::HttpStatus { .. }
            | AdminClientError::ResponseTooLarge { .. }
            | AdminClientError::InvalidResponse(_) => INTERNAL,
        }
    }

    fn report_error(error: &CliError, exit: u8, json_output: bool) -> io::Result<()> {
        let category = match exit {
            VALIDATION => "validation",
            UNAVAILABLE => "availability",
            CONFLICT => "conflict",
            CONFIGURATION => "configuration",
            _ => "internal",
        };

        if json_output {
            return writeln!(
                std::io::stdout().lock(),
                "{}",
                json!({
                    "error": {
                        "category": category,
                        "message": error.to_string()
                    }
                })
            );
        }

        writeln!(std::io::stderr().lock(), "maincopy: {error}")
    }

    #[cfg(test)]
    mod tests {
        use maincopy_shared::FeatureVersions;
        use serde_json::json;

        use super::*;

        fn capabilities() -> Capabilities {
            Capabilities {
                api_version: AdminApiVersion::V1,
                features: FeatureVersions {
                    capabilities: CapabilityContractVersion::V1,
                },
            }
        }

        #[test]
        #[cfg(unix)]
        fn unix_client_arguments_have_a_local_development_default() {
            let arguments = Arguments::try_parse_from(["maincopy", "capabilities"]).unwrap();

            assert_eq!(arguments.socket, PathBuf::from("run/admin.sock"));
            assert!(!arguments.json);
            assert!(matches!(arguments.command, Command::Capabilities));
        }

        #[test]
        #[cfg(windows)]
        fn windows_client_arguments_use_the_shared_named_pipe_default() {
            let arguments = Arguments::try_parse_from(["maincopy", "capabilities"]).unwrap();

            assert_eq!(
                arguments.socket,
                PathBuf::from(maincopy_shared::DEFAULT_WINDOWS_ADMIN_PIPE)
            );
            assert!(!arguments.json);
            assert!(matches!(arguments.command, Command::Capabilities));
        }

        #[test]
        fn global_options_are_accepted_after_the_command() {
            let arguments = Arguments::try_parse_from([
                "maincopy",
                "capabilities",
                "--socket",
                "custom.sock",
                "--json",
            ])
            .unwrap();

            assert_eq!(arguments.socket, PathBuf::from("custom.sock"));
            assert!(arguments.json);
        }

        #[test]
        fn json_output_is_the_shared_wire_contract() {
            let mut output = Vec::new();

            write_capabilities(&mut output, capabilities(), true).unwrap();

            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
                json!({
                    "api_version": "v1",
                    "features": { "capabilities": "v1" }
                })
            );
        }

        #[test]
        fn human_output_names_each_version() {
            let mut output = Vec::new();

            write_capabilities(&mut output, capabilities(), false).unwrap();

            assert_eq!(
                String::from_utf8(output).unwrap(),
                "Admin API: v1\nCapabilities contract: v1\n"
            );
        }
    }
}
