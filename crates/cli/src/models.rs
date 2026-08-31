//! Command-line input models.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use maincopy_shared::publication::PreviewDigest;
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(
    name = "maincopy",
    version,
    about = "Operate a running Maincopy server."
)]
pub(crate) struct Arguments {
    /// Write one machine-readable JSON document.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    /// Canonical HTTPS origin of the private administration API.
    #[arg(
        long,
        global = true,
        value_name = "HTTPS_ORIGIN",
        default_value = "https://admin.localhost"
    )]
    pub(crate) admin_origin: Box<str>,

    /// Protected credential context used for administration requests.
    #[arg(long, global = true, value_enum, default_value_t = AuthenticationContext::Human)]
    pub(crate) auth_context: AuthenticationContext,

    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Create and protect a password-authenticated human session.
    Login {
        /// Canonical account username. The password is read from the terminal.
        #[arg(long, value_name = "USERNAME")]
        username: Box<str>,
    },

    /// Revoke the active human session and delete its protected local credentials.
    Logout,

    /// Manage the protected local Nostr private key used by the agent context.
    AgentKey {
        #[command(subcommand)]
        command: AgentKeyCommand,
    },

    /// Report the API versions supported by the running server.
    Capabilities,

    /// List post revisions loaded by the running server.
    Posts,

    /// Download one exact private post preview without overwriting a file.
    Preview {
        /// Stable post UUID to preview.
        #[arg(value_name = "POST_ID")]
        post_id: Uuid,

        /// Create this new HTML file; an existing path is never overwritten.
        #[arg(long, value_name = "PATH")]
        output: PathBuf,

        /// Require this exact typed post revision digest.
        #[arg(long, value_name = "DIGEST")]
        revision: Option<String>,

        /// Require this exact typed managed content-tree digest.
        #[arg(long, value_name = "DIGEST")]
        content_digest: Option<String>,
    },

    /// Publish the current eligible revision of one post immediately.
    PublishNow {
        /// Stable post UUID to publish.
        #[arg(value_name = "POST_ID")]
        post_id: Uuid,

        /// Exact private preview digest reviewed for this approval.
        #[arg(long, value_name = "DIGEST")]
        preview_digest: PreviewDigest,

        /// Require this exact typed post revision digest.
        #[arg(long, value_name = "DIGEST")]
        revision: Option<String>,

        /// Retry identity for this publication command; generated when omitted.
        #[arg(long, value_name = "UUID")]
        idempotency_key: Option<Uuid>,
    },

    /// Approve an exact post revision for publication at a UTC time.
    Schedule {
        /// Stable post UUID to schedule.
        #[arg(value_name = "POST_ID")]
        post_id: Uuid,

        /// Exact private preview digest reviewed for this approval.
        #[arg(long, value_name = "DIGEST")]
        preview_digest: PreviewDigest,

        /// UTC RFC3339 publication time, for example 2026-09-01T12:30:00Z.
        #[arg(long, value_name = "UTC_RFC3339", value_parser = parse_utc_rfc3339)]
        at: OffsetDateTime,

        /// Pin this exact typed post revision digest.
        #[arg(long, value_name = "DIGEST")]
        revision: Option<String>,

        /// Retry identity for this scheduling command; generated when omitted.
        #[arg(long, value_name = "UUID")]
        idempotency_key: Option<Uuid>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum AuthenticationContext {
    Human,
    Agent,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AgentKeyCommand {
    /// Read a lowercase-hex Nostr private key from the terminal and protect it locally.
    Set,
    /// Delete the protected local agent key.
    Remove,
}

fn parse_utc_rfc3339(value: &str) -> Result<OffsetDateTime, String> {
    let timestamp = OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| "must be a valid RFC3339 timestamp".to_owned())?;
    if timestamp.offset() != UtcOffset::UTC {
        return Err("must use the UTC offset (Z or +00:00)".to_owned());
    }
    Ok(timestamp)
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;

    const PREVIEW_DIGEST: &str =
        "preview-b3-v1-4444444444444444444444444444444444444444444444444444444444444444";

    #[test]
    fn client_arguments_select_capabilities_without_a_transport_flag() {
        let arguments = Arguments::try_parse_from(["maincopy", "capabilities"]).unwrap();

        assert!(!arguments.json);
        assert!(matches!(arguments.command, Command::Capabilities));
    }

    #[test]
    fn removed_socket_option_is_rejected() {
        assert!(
            Arguments::try_parse_from(["maincopy", "--socket", "admin.sock", "capabilities",])
                .is_err()
        );
    }

    #[test]
    fn global_options_are_accepted_after_the_command() {
        let arguments = Arguments::try_parse_from([
            "maincopy",
            "capabilities",
            "--json",
            "--admin-origin",
            "https://admin.example.test",
            "--auth-context",
            "agent",
        ])
        .unwrap();

        assert!(arguments.json);
        assert_eq!(
            arguments.admin_origin.as_ref(),
            "https://admin.example.test"
        );
        assert_eq!(arguments.auth_context, AuthenticationContext::Agent);
    }

    #[test]
    fn login_and_agent_key_commands_never_accept_secrets_on_argv() {
        let login =
            Arguments::try_parse_from(["maincopy", "login", "--username", "publisher"]).unwrap();
        assert!(matches!(
            login.command,
            Command::Login { ref username } if username.as_ref() == "publisher"
        ));
        assert!(
            Arguments::try_parse_from([
                "maincopy",
                "login",
                "--username",
                "publisher",
                "--password",
                "secret"
            ])
            .is_err()
        );
        assert!(
            Arguments::try_parse_from(["maincopy", "agent-key", "set", "--private-key", "secret"])
                .is_err()
        );
    }

    #[test]
    fn posts_selects_the_loaded_post_listing_command() {
        let arguments = Arguments::try_parse_from(["maincopy", "posts"]).unwrap();

        assert!(matches!(arguments.command, Command::Posts));
    }

    #[test]
    fn preview_parses_required_output_and_optional_exact_selectors() {
        let arguments = Arguments::try_parse_from([
            "maincopy",
            "preview",
            "11111111-1111-4111-8111-111111111111",
            "--output",
            "ready.html",
            "--revision",
            "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111",
            "--content-digest",
            "content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333",
        ])
        .unwrap();

        let Command::Preview {
            post_id,
            output,
            revision,
            content_digest,
        } = arguments.command
        else {
            panic!("preview must select the private preview command");
        };
        assert_eq!(
            post_id,
            Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap()
        );
        assert_eq!(output, PathBuf::from("ready.html"));
        assert_eq!(
            revision.as_deref(),
            Some("post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111")
        );
        assert_eq!(
            content_digest.as_deref(),
            Some("content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333")
        );
    }

    #[test]
    fn preview_requires_an_explicit_output_path() {
        let error = Arguments::try_parse_from([
            "maincopy",
            "preview",
            "11111111-1111-4111-8111-111111111111",
        ])
        .unwrap_err();

        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn publication_commands_require_a_typed_reviewed_preview() {
        for command in ["publish-now", "schedule"] {
            let mut arguments = vec!["maincopy", command, "11111111-1111-4111-8111-111111111111"];
            if command == "schedule" {
                arguments.extend(["--at", "2026-09-01T12:30:00Z"]);
            }
            let error = Arguments::try_parse_from(arguments).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::MissingRequiredArgument
            );
        }
    }

    #[test]
    fn publish_now_parses_optional_revision_and_retry_identity() {
        let arguments = Arguments::try_parse_from([
            "maincopy",
            "publish-now",
            "11111111-1111-4111-8111-111111111111",
            "--preview-digest",
            PREVIEW_DIGEST,
            "--revision",
            "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111",
            "--idempotency-key",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        ])
        .unwrap();

        let Command::PublishNow {
            post_id,
            preview_digest,
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
            preview_digest,
            PreviewDigest::parse(PREVIEW_DIGEST).unwrap()
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
            "--preview-digest",
            PREVIEW_DIGEST,
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
    fn schedule_parses_an_exact_utc_time_revision_and_retry_identity() {
        let arguments = Arguments::try_parse_from([
            "maincopy",
            "schedule",
            "11111111-1111-4111-8111-111111111111",
            "--preview-digest",
            PREVIEW_DIGEST,
            "--at",
            "2026-09-01T12:30:00Z",
            "--revision",
            "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111",
            "--idempotency-key",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        ])
        .unwrap();

        let Command::Schedule {
            post_id,
            preview_digest,
            at,
            revision,
            idempotency_key,
        } = arguments.command
        else {
            panic!("schedule must select the scheduled approval command");
        };
        assert_eq!(
            post_id,
            Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap()
        );
        assert_eq!(at.offset(), UtcOffset::UTC);
        assert_eq!(
            preview_digest,
            PreviewDigest::parse(PREVIEW_DIGEST).unwrap()
        );
        assert_eq!(
            at,
            OffsetDateTime::parse("2026-09-01T12:30:00Z", &Rfc3339).unwrap()
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
    fn schedule_rejects_non_utc_and_malformed_times() {
        for at in ["2026-09-01T14:30:00+02:00", "tomorrow"] {
            let error = Arguments::try_parse_from([
                "maincopy",
                "schedule",
                "11111111-1111-4111-8111-111111111111",
                "--preview-digest",
                PREVIEW_DIGEST,
                "--at",
                at,
            ])
            .unwrap_err();

            assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        }
    }
}
