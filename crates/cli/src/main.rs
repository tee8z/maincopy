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
    use std::{io, path::PathBuf, process::ExitCode};

    use clap::{Parser, Subcommand};
    use maincopy_cli::{AdminClient, AdminClientError};
    #[cfg(windows)]
    use maincopy_shared::DEFAULT_WINDOWS_ADMIN_PIPE;
    use maincopy_shared::{
        AdminApiVersion, Capabilities, CapabilityContractVersion,
        publication::{PublishNowRequest, PublishNowResponse},
    };
    use serde_json::json;
    use thiserror::Error;
    use uuid::Uuid;

    const SUCCESS: u8 = 0;
    const VALIDATION: u8 = 65;
    const UNAVAILABLE: u8 = 69;
    const INTERNAL: u8 = 70;
    const CONFLICT: u8 = 75;
    const PERMISSION: u8 = 77;
    const CONFIGURATION: u8 = 78;
    #[cfg(unix)]
    const DEFAULT_ADMIN_ENDPOINT: &str = "run/admin.sock";
    #[cfg(windows)]
    const DEFAULT_ADMIN_ENDPOINT: &str = DEFAULT_WINDOWS_ADMIN_PIPE;

    #[derive(Parser)]
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

    #[derive(Debug, Subcommand)]
    enum Command {
        /// Report the API versions supported by the running server.
        Capabilities,

        /// Publish the current eligible revision of one post immediately.
        PublishNow {
            /// Stable post UUID to publish.
            #[arg(value_name = "POST_ID")]
            post_id: Uuid,

            /// Require this exact typed post revision digest.
            #[arg(long, value_name = "DIGEST")]
            revision: Option<String>,

            /// Retry identity for this publication command; generated when omitted.
            #[arg(long, value_name = "UUID")]
            idempotency_key: Option<Uuid>,
        },
    }

    enum CommandOutput {
        Capabilities(Capabilities),
        Publication {
            idempotency_key: Uuid,
            response: PublishNowResponse,
        },
    }

    #[derive(Debug, Error)]
    enum CliError {
        #[error(transparent)]
        Admin(#[from] AdminClientError),

        #[error("publication command {idempotency_key} failed: {source}")]
        Publication {
            idempotency_key: Uuid,
            #[source]
            source: AdminClientError,
        },

        #[error("failed to write command output: {0}")]
        Output(#[from] io::Error),

        #[error("failed to encode command output: {0}")]
        Encode(#[from] serde_json::Error),
    }

    #[tokio::main(flavor = "current_thread")]
    pub async fn run() -> ExitCode {
        let arguments = Arguments::parse();
        let json = arguments.json;
        let result = execute(arguments)
            .await
            .and_then(|output| write_output(std::io::stdout().lock(), output, json));

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

    async fn execute(arguments: Arguments) -> Result<CommandOutput, CliError> {
        let client = AdminClient::new(arguments.socket)?;
        match arguments.command {
            Command::Capabilities => client
                .capabilities()
                .await
                .map(CommandOutput::Capabilities)
                .map_err(CliError::from),
            Command::PublishNow {
                post_id,
                revision,
                idempotency_key,
            } => {
                let idempotency_key = idempotency_key.unwrap_or_else(Uuid::new_v4);
                let response = client
                    .publish_now(
                        idempotency_key,
                        &PublishNowRequest {
                            post_id,
                            expected_revision: revision.map(String::into_boxed_str),
                        },
                    )
                    .await
                    .map_err(|source| CliError::Publication {
                        idempotency_key,
                        source,
                    })?;
                Ok(CommandOutput::Publication {
                    idempotency_key,
                    response,
                })
            }
        }
    }

    fn write_output(
        output: impl io::Write,
        command: CommandOutput,
        json: bool,
    ) -> Result<(), CliError> {
        match command {
            CommandOutput::Capabilities(capabilities) => {
                write_capabilities(output, capabilities, json)
            }
            CommandOutput::Publication {
                idempotency_key,
                response,
            } => write_publication(output, idempotency_key, response, json),
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

    fn write_publication(
        mut output: impl io::Write,
        idempotency_key: Uuid,
        response: PublishNowResponse,
        json: bool,
    ) -> Result<(), CliError> {
        if json {
            let serde_json::Value::Object(mut fields) = serde_json::to_value(&response)? else {
                unreachable!("the shared publication response serializes as an object");
            };
            fields.insert("idempotency_key".into(), json!(idempotency_key));
            serde_json::to_writer(&mut output, &fields)?;
            writeln!(output)?;
            return Ok(());
        }

        writeln!(output, "Publication: {}", response.publication_id)?;
        writeln!(output, "Post: {}", response.post_id)?;
        writeln!(output, "Revision: {}", response.revision)?;
        writeln!(output, "Published at: {}", response.published_at)?;
        writeln!(
            output,
            "Site: {} (version {})",
            response.site_digest, response.site_version
        )?;
        writeln!(output, "Idempotency key: {idempotency_key}")?;
        Ok(())
    }

    fn error_exit(error: &CliError) -> u8 {
        let Some(error) = admin_error(error) else {
            return INTERNAL;
        };

        match error {
            AdminClientError::InvalidSocketPath => CONFIGURATION,
            AdminClientError::Request { .. } => UNAVAILABLE,
            AdminClientError::HttpStatus { status, .. } if matches!(status.as_u16(), 401 | 403) => {
                PERMISSION
            }
            AdminClientError::HttpStatus { status, .. }
                if matches!(status.as_u16(), 400 | 404 | 405 | 413 | 415 | 422) =>
            {
                VALIDATION
            }
            AdminClientError::HttpStatus { status, .. } if matches!(status.as_u16(), 409 | 412) => {
                CONFLICT
            }
            AdminClientError::HttpStatus { status, .. } if matches!(status.as_u16(), 502..=504) => {
                UNAVAILABLE
            }
            AdminClientError::Build(_)
            | AdminClientError::HttpStatus { .. }
            | AdminClientError::ResponseTooLarge { .. }
            | AdminClientError::InvalidResponse(_) => INTERNAL,
        }
    }

    fn report_error(error: &CliError, exit: u8, json_output: bool) -> io::Result<()> {
        if json_output {
            return write_error(std::io::stdout().lock(), error, exit, true);
        }

        write_error(std::io::stderr().lock(), error, exit, false)
    }

    fn write_error(
        mut output: impl io::Write,
        error: &CliError,
        exit: u8,
        json_output: bool,
    ) -> io::Result<()> {
        let (problem, request_id) = match admin_error(error) {
            Some(AdminClientError::HttpStatus {
                problem,
                request_id,
                ..
            }) => (problem.as_ref(), *request_id),
            _ => (None, None),
        };
        if !json_output {
            writeln!(output, "maincopy: {error}")?;
            if let Some(problem) = problem {
                writeln!(output, "maincopy: {}: {}", problem.code, problem.message)?;
            }
            if let Some(request_id) = request_id {
                writeln!(output, "maincopy: request ID: {request_id}")?;
            }
            return Ok(());
        }

        let mut details = serde_json::Map::from_iter([
            ("category".into(), json!(error_category(error, exit))),
            ("message".into(), json!(error.to_string())),
        ]);
        if let CliError::Publication {
            idempotency_key, ..
        } = error
        {
            details.insert("idempotency_key".into(), json!(idempotency_key));
        }
        if let Some(problem) = problem {
            details.insert("code".into(), json!(problem.code));
            details.insert("server_message".into(), json!(problem.message));
        }
        if let Some(request_id) = request_id {
            details.insert("request_id".into(), json!(request_id));
        }
        writeln!(output, "{}", json!({ "error": details }))
    }

    fn error_category(error: &CliError, exit: u8) -> &'static str {
        match admin_error(error) {
            Some(AdminClientError::HttpStatus { status, .. }) if status.as_u16() == 401 => {
                "authentication"
            }
            Some(AdminClientError::HttpStatus { status, .. }) if status.as_u16() == 403 => {
                "authorization"
            }
            _ => match exit {
                VALIDATION => "validation",
                UNAVAILABLE => "availability",
                CONFLICT => "conflict",
                CONFIGURATION => "configuration",
                _ => "internal",
            },
        }
    }

    fn admin_error(error: &CliError) -> Option<&AdminClientError> {
        match error {
            CliError::Admin(error) | CliError::Publication { source: error, .. } => Some(error),
            CliError::Output(_) | CliError::Encode(_) => None,
        }
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

        fn publication_response() -> PublishNowResponse {
            serde_json::from_value(json!({
                "publication_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "post_id": "11111111-1111-4111-8111-111111111111",
                "revision":
                    "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111",
                "published_at": "2026-08-30T12:00:00Z",
                "site_digest":
                    "site-b3-v1-2222222222222222222222222222222222222222222222222222222222222222",
                "site_version": 2
            }))
            .unwrap()
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
        fn publish_now_parses_optional_revision_and_retry_identity() {
            let arguments = Arguments::try_parse_from([
                "maincopy",
                "publish-now",
                "11111111-1111-4111-8111-111111111111",
                "--revision",
                "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111",
                "--idempotency-key",
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            ])
            .unwrap();

            let Command::PublishNow {
                post_id,
                revision,
                idempotency_key,
            } = arguments.command
            else {
                panic!("publish-now must select the publication command");
            };
            assert_eq!(
                post_id,
                Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap()
            );
            assert_eq!(
                revision.as_deref(),
                Some("post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111")
            );
            assert_eq!(
                idempotency_key,
                Some(Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap())
            );
        }

        #[test]
        fn publish_now_generates_the_retry_identity_only_at_execution() {
            let arguments = Arguments::try_parse_from([
                "maincopy",
                "publish-now",
                "11111111-1111-4111-8111-111111111111",
            ])
            .unwrap();

            assert!(matches!(
                arguments.command,
                Command::PublishNow {
                    revision: None,
                    idempotency_key: None,
                    ..
                }
            ));
        }

        #[test]
        fn http_status_failures_use_stable_exit_categories() {
            let authentication = CliError::Admin(AdminClientError::HttpStatus {
                status: reqwest::StatusCode::UNAUTHORIZED,
                problem: None,
                request_id: None,
            });
            let authorization = CliError::Admin(AdminClientError::HttpStatus {
                status: reqwest::StatusCode::FORBIDDEN,
                problem: None,
                request_id: None,
            });

            assert_eq!(error_exit(&authentication), PERMISSION);
            assert_eq!(
                error_category(&authentication, PERMISSION),
                "authentication"
            );
            assert_eq!(error_exit(&authorization), PERMISSION);
            assert_eq!(error_category(&authorization, PERMISSION), "authorization");

            for status in [
                reqwest::StatusCode::PAYLOAD_TOO_LARGE,
                reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ] {
                let invalid_request = CliError::Admin(AdminClientError::HttpStatus {
                    status,
                    problem: None,
                    request_id: None,
                });
                assert_eq!(error_exit(&invalid_request), VALIDATION);
                assert_eq!(error_category(&invalid_request, VALIDATION), "validation");
            }
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

        #[test]
        fn publication_json_output_is_one_direct_machine_document() {
            let mut output = Vec::new();
            let idempotency_key = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();

            write_publication(&mut output, idempotency_key, publication_response(), true).unwrap();

            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
                json!({
                    "idempotency_key": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                    "publication_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                    "post_id": "11111111-1111-4111-8111-111111111111",
                    "revision":
                        "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111",
                    "published_at": "2026-08-30T12:00:00Z",
                    "site_digest":
                        "site-b3-v1-2222222222222222222222222222222222222222222222222222222222222222",
                    "site_version": 2
                })
            );
        }

        #[test]
        fn publication_human_output_reports_every_retryable_identity() {
            let mut output = Vec::new();
            let idempotency_key = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();

            write_publication(&mut output, idempotency_key, publication_response(), false).unwrap();

            assert_eq!(
                String::from_utf8(output).unwrap(),
                "Publication: aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\n\
Post: 11111111-1111-4111-8111-111111111111\n\
Revision: post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111\n\
Published at: 2026-08-30 12:00:00.0 +00:00:00\n\
Site: site-b3-v1-2222222222222222222222222222222222222222222222222222222222222222 (version 2)\n\
Idempotency key: bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\n"
            );
        }

        #[test]
        fn publication_failure_reports_the_retry_identity_in_every_output_mode() {
            let idempotency_key = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();
            let error = CliError::Publication {
                idempotency_key,
                source: AdminClientError::HttpStatus {
                    status: reqwest::StatusCode::GATEWAY_TIMEOUT,
                    problem: Some(maincopy_cli::AdminProblem {
                        code: "publication_unavailable".into(),
                        message: "publication is temporarily unavailable".into(),
                    }),
                    request_id: Some(
                        Uuid::parse_str("cccccccc-cccc-4ccc-8ccc-cccccccccccc").unwrap(),
                    ),
                },
            };

            let exit = error_exit(&error);
            assert_eq!(exit, UNAVAILABLE);
            assert_eq!(error_category(&error, exit), "availability");

            let mut json_output = Vec::new();
            write_error(&mut json_output, &error, exit, true).unwrap();
            let document = serde_json::from_slice::<serde_json::Value>(&json_output).unwrap();
            assert_eq!(
                document["error"]["idempotency_key"],
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
            );
            assert_eq!(document["error"]["code"], "publication_unavailable");
            assert_eq!(
                document["error"]["request_id"],
                "cccccccc-cccc-4ccc-8ccc-cccccccccccc"
            );
            assert!(
                document["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
            );

            let mut human_output = Vec::new();
            write_error(&mut human_output, &error, exit, false).unwrap();
            let human_output = String::from_utf8(human_output).unwrap();
            assert!(human_output.contains("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"));
            assert!(human_output.contains("publication_unavailable"));
            assert!(human_output.contains("cccccccc-cccc-4ccc-8ccc-cccccccccccc"));
        }
    }
}
