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
        collections::HashSet,
        fs::OpenOptions,
        io::{self, Write as _},
        path::{Path, PathBuf},
        process::ExitCode,
    };

    use clap::{Parser, Subcommand};
    use maincopy_cli::{AdminClient, AdminClientError, PostPreview};
    #[cfg(windows)]
    use maincopy_shared::DEFAULT_WINDOWS_ADMIN_PIPE;
    use maincopy_shared::{
        AdminApiVersion, Capabilities, CapabilityContractVersion,
        posts::{ListPostsResponse, PostPublicationState, PostSummary},
        publication::{
            PreviewDigest, PublicationApprovalState, PublishNowRequest, PublishNowResponse,
        },
    };
    use serde_json::json;
    use thiserror::Error;
    use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
    use uuid::Uuid;

    const SUCCESS: u8 = 0;
    const VALIDATION: u8 = 65;
    const UNAVAILABLE: u8 = 69;
    const INTERNAL: u8 = 70;
    const CONFLICT: u8 = 75;
    const PERMISSION: u8 = 77;
    const CONFIGURATION: u8 = 78;
    const POSTS_PAGE_LIMIT: u16 = 100;
    const MAX_POSTS_PAGES: usize = 10_001;
    const POST_REVISION_PREFIX: &str = "post-b3-v1-";
    const CONTENT_DIGEST_PREFIX: &str = "content-b3-v1-";
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

    #[derive(Debug, Subcommand)]
    enum Command {
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

    enum CommandOutput {
        Capabilities(Capabilities),
        Posts(ListPostsResponse),
        Preview {
            post_id: Uuid,
            output: PathBuf,
            preview: PostPreview,
        },
        Publication {
            idempotency_key: Uuid,
            response: PublishNowResponse,
        },
    }

    #[derive(Debug, Error)]
    enum CliError {
        #[error(transparent)]
        Admin(#[from] AdminClientError),

        #[error(
            "the loaded-post snapshot changed during pagination from content {expected_content_digest}, site {expected_site_digest} (version {expected_site_version}) to content {actual_content_digest}, site {actual_site_digest} (version {actual_site_version}); retry the command"
        )]
        PostsSnapshotChanged {
            expected_content_digest: Box<str>,
            expected_site_digest: Box<str>,
            expected_site_version: u64,
            actual_content_digest: Box<str>,
            actual_site_digest: Box<str>,
            actual_site_version: u64,
        },

        #[error("the admin server returned invalid loaded-post pagination: {message}")]
        InvalidPostsPagination { message: &'static str },

        #[error("the admin server returned inconsistent publication approval state: {message}")]
        InvalidPublicationResponse { message: &'static str },

        #[error("{field} must be {prefix} followed by 64 lowercase hexadecimal characters")]
        InvalidPreviewSelector {
            field: &'static str,
            prefix: &'static str,
        },

        #[error(
            "the preview revision {actual} does not match requested revision {expected}; no file was created"
        )]
        PreviewRevisionMismatch {
            expected: Box<str>,
            actual: Box<str>,
        },

        #[error(
            "the preview content digest {actual} does not match requested content digest {expected}; no file was created"
        )]
        PreviewContentDigestMismatch {
            expected: Box<str>,
            actual: Box<str>,
        },

        #[error("preview output {path:?} already exists; refusing to overwrite it")]
        PreviewOutputExists { path: PathBuf },

        #[error("failed to create or write preview output {path:?}: {source}")]
        PreviewOutput {
            path: PathBuf,
            #[source]
            source: io::Error,
        },

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
            Command::Posts => list_all_posts(&client).await.map(CommandOutput::Posts),
            Command::Preview {
                post_id,
                output,
                revision,
                content_digest,
            } => preview_post(&client, post_id, output, revision, content_digest).await,
            Command::PublishNow {
                post_id,
                preview_digest,
                revision,
                idempotency_key,
            } => {
                approve_publication(
                    &client,
                    post_id,
                    preview_digest,
                    revision,
                    None,
                    idempotency_key,
                )
                .await
            }
            Command::Schedule {
                post_id,
                preview_digest,
                at,
                revision,
                idempotency_key,
            } => {
                approve_publication(
                    &client,
                    post_id,
                    preview_digest,
                    revision,
                    Some(at),
                    idempotency_key,
                )
                .await
            }
        }
    }

    async fn preview_post(
        client: &AdminClient,
        post_id: Uuid,
        output: PathBuf,
        revision: Option<String>,
        content_digest: Option<String>,
    ) -> Result<CommandOutput, CliError> {
        validate_optional_digest(revision.as_deref(), POST_REVISION_PREFIX, "revision")?;
        validate_optional_digest(
            content_digest.as_deref(),
            CONTENT_DIGEST_PREFIX,
            "content_digest",
        )?;
        let preview = client
            .preview_post(post_id, revision.as_deref(), content_digest.as_deref())
            .await?;
        if let Some(expected) = revision
            && expected.as_str() != preview.revision.as_ref()
        {
            return Err(CliError::PreviewRevisionMismatch {
                expected: expected.into_boxed_str(),
                actual: preview.revision.clone(),
            });
        }
        if let Some(expected) = content_digest
            && expected.as_str() != preview.content_digest.as_ref()
        {
            return Err(CliError::PreviewContentDigestMismatch {
                expected: expected.into_boxed_str(),
                actual: preview.content_digest.clone(),
            });
        }
        write_preview_file(&output, &preview.html)?;
        Ok(CommandOutput::Preview {
            post_id,
            output,
            preview,
        })
    }

    fn validate_optional_digest(
        value: Option<&str>,
        prefix: &'static str,
        field: &'static str,
    ) -> Result<(), CliError> {
        let Some(value) = value else {
            return Ok(());
        };
        let valid = value.strip_prefix(prefix).is_some_and(|encoded| {
            encoded.len() == 64
                && encoded
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if valid {
            Ok(())
        } else {
            Err(CliError::InvalidPreviewSelector { field, prefix })
        }
    }

    fn write_preview_file(path: &Path, html: &str) -> Result<(), CliError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|source| {
                if source.kind() == io::ErrorKind::AlreadyExists {
                    CliError::PreviewOutputExists {
                        path: path.to_path_buf(),
                    }
                } else {
                    CliError::PreviewOutput {
                        path: path.to_path_buf(),
                        source,
                    }
                }
            })?;
        file.write_all(html.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|source| CliError::PreviewOutput {
                path: path.to_path_buf(),
                source,
            })
    }

    async fn approve_publication(
        client: &AdminClient,
        post_id: Uuid,
        preview_digest: PreviewDigest,
        revision: Option<String>,
        scheduled_for: Option<OffsetDateTime>,
        idempotency_key: Option<Uuid>,
    ) -> Result<CommandOutput, CliError> {
        let idempotency_key = idempotency_key.unwrap_or_else(Uuid::new_v4);
        let response = client
            .approve_publication(
                idempotency_key,
                &PublishNowRequest {
                    post_id,
                    preview_digest,
                    expected_revision: revision.map(String::into_boxed_str),
                    scheduled_for,
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

    fn parse_utc_rfc3339(value: &str) -> Result<OffsetDateTime, String> {
        let timestamp = OffsetDateTime::parse(value, &Rfc3339)
            .map_err(|_| "must be a valid RFC3339 timestamp".to_owned())?;
        if timestamp.offset() != UtcOffset::UTC {
            return Err("must use the UTC offset (Z or +00:00)".to_owned());
        }
        Ok(timestamp)
    }

    async fn list_all_posts(client: &AdminClient) -> Result<ListPostsResponse, CliError> {
        let first = client.list_posts_page(None, POSTS_PAGE_LIMIT).await?;
        let content_digest = first.content_digest.clone();
        let site_digest = first.site_digest.clone();
        let site_version = first.site_version;
        let mut posts = Vec::new();
        let mut seen_posts = HashSet::new();
        append_post_page(&mut posts, &mut seen_posts, first.posts)?;

        let mut next_cursor = first.next_cursor;
        let mut seen_cursors = HashSet::new();
        let mut page_count = 1_usize;
        while let Some(cursor) = next_cursor {
            if page_count >= MAX_POSTS_PAGES {
                return Err(CliError::InvalidPostsPagination {
                    message: "the page count exceeded the client safety limit",
                });
            }
            if !seen_cursors.insert(cursor) {
                return Err(CliError::InvalidPostsPagination {
                    message: "next_cursor repeated an earlier cursor",
                });
            }

            let page = client
                .list_posts_page(Some(cursor), POSTS_PAGE_LIMIT)
                .await?;
            if page.content_digest != content_digest
                || page.site_digest != site_digest
                || page.site_version != site_version
            {
                return Err(CliError::PostsSnapshotChanged {
                    expected_content_digest: content_digest,
                    expected_site_digest: site_digest,
                    expected_site_version: site_version,
                    actual_content_digest: page.content_digest,
                    actual_site_digest: page.site_digest,
                    actual_site_version: page.site_version,
                });
            }
            append_post_page(&mut posts, &mut seen_posts, page.posts)?;
            next_cursor = page.next_cursor;
            page_count += 1;
        }

        Ok(ListPostsResponse {
            content_digest,
            site_digest,
            site_version,
            posts,
            next_cursor: None,
        })
    }

    fn append_post_page(
        posts: &mut Vec<PostSummary>,
        seen: &mut HashSet<Uuid>,
        page: Vec<PostSummary>,
    ) -> Result<(), CliError> {
        for post in page {
            if !seen.insert(post.post_id) {
                return Err(CliError::InvalidPostsPagination {
                    message: "a post UUID appeared more than once",
                });
            }
            posts.push(post);
        }
        Ok(())
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
            CommandOutput::Posts(posts) => write_posts(output, posts, json),
            CommandOutput::Preview {
                post_id,
                output: path,
                preview,
            } => write_preview(output, post_id, &path, preview, json),
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

    fn write_posts(
        mut output: impl io::Write,
        response: ListPostsResponse,
        json: bool,
    ) -> Result<(), CliError> {
        if json {
            serde_json::to_writer(&mut output, &response)?;
            writeln!(output)?;
            return Ok(());
        }

        writeln!(
            output,
            "Site: {} (version {})",
            response.site_digest, response.site_version
        )?;
        writeln!(output, "Content: {}", response.content_digest)?;
        writeln!(output, "Posts: {}", response.posts.len())?;
        for post in response.posts {
            let publication_state = post.publication_state;
            writeln!(output)?;
            writeln!(
                output,
                "[{}] {}",
                publication_state_name(publication_state),
                post.title
            )?;
            writeln!(output, "  ID: {}", post.post_id)?;
            writeln!(output, "  Revision: {}", post.revision)?;
            writeln!(output, "  Source: {}", post.source_path)?;
            writeln!(output, "  Slug: {}", post.slug)?;
            if let Some(published_at) = post.published_at {
                let label = match publication_state {
                    PostPublicationState::UnpublishedChange => "Current publication at",
                    PostPublicationState::Published => "Published at",
                    PostPublicationState::Draft | PostPublicationState::Unpublished => {
                        "Publication at"
                    }
                };
                writeln!(output, "  {label}: {published_at}")?;
            }
        }
        Ok(())
    }

    fn write_preview(
        mut output: impl io::Write,
        post_id: Uuid,
        path: &Path,
        preview: PostPreview,
        json: bool,
    ) -> Result<(), CliError> {
        if json {
            serde_json::to_writer(
                &mut output,
                &json!({
                    "post_id": post_id,
                    "preview_digest": preview.preview_digest,
                    "revision": preview.revision,
                    "content_digest": preview.content_digest,
                    "canonical_url": preview.canonical_url,
                    "output": path.display().to_string(),
                }),
            )?;
            writeln!(output)?;
            return Ok(());
        }

        writeln!(output, "Preview: {}", preview.preview_digest)?;
        writeln!(output, "Post: {post_id}")?;
        writeln!(output, "Revision: {}", preview.revision)?;
        writeln!(output, "Content: {}", preview.content_digest)?;
        writeln!(output, "Canonical: {}", preview.canonical_url)?;
        writeln!(output, "Output: {}", path.display())?;
        Ok(())
    }

    const fn publication_state_name(state: PostPublicationState) -> &'static str {
        match state {
            PostPublicationState::Draft => "draft",
            PostPublicationState::Unpublished => "unpublished",
            PostPublicationState::UnpublishedChange => "unpublished_change",
            PostPublicationState::Published => "published",
        }
    }

    fn write_publication(
        mut output: impl io::Write,
        idempotency_key: Uuid,
        response: PublishNowResponse,
        json: bool,
    ) -> Result<(), CliError> {
        validate_publication_response(&response)?;
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
        writeln!(output, "Status: {}", approval_state_name(response.state))?;
        writeln!(output, "Post: {}", response.post_id)?;
        writeln!(output, "Preview: {}", response.preview_digest)?;
        writeln!(output, "Pinned revision: {}", response.revision)?;
        if let Some(scheduled_for) = response.scheduled_for {
            writeln!(output, "Scheduled for: {scheduled_for}")?;
        }
        if let Some(published_at) = response.published_at {
            writeln!(output, "Published at: {published_at}")?;
        }
        writeln!(
            output,
            "Site: {} (version {})",
            response.site_digest, response.site_version
        )?;
        writeln!(output, "Idempotency key: {idempotency_key}")?;
        Ok(())
    }

    fn validate_publication_response(response: &PublishNowResponse) -> Result<(), CliError> {
        match (
            response.state,
            response.scheduled_for,
            response.published_at,
        ) {
            (PublicationApprovalState::Scheduled, Some(_), None)
            | (PublicationApprovalState::Published, _, Some(_)) => Ok(()),
            (PublicationApprovalState::Scheduled, None, _) => {
                Err(CliError::InvalidPublicationResponse {
                    message: "scheduled state requires scheduled_for",
                })
            }
            (PublicationApprovalState::Scheduled, Some(_), Some(_)) => {
                Err(CliError::InvalidPublicationResponse {
                    message: "scheduled state must not contain published_at",
                })
            }
            (PublicationApprovalState::Published, _, None) => {
                Err(CliError::InvalidPublicationResponse {
                    message: "published state requires published_at",
                })
            }
        }
    }

    const fn approval_state_name(state: PublicationApprovalState) -> &'static str {
        match state {
            PublicationApprovalState::Scheduled => "scheduled",
            PublicationApprovalState::Published => "published",
        }
    }

    fn error_exit(error: &CliError) -> u8 {
        match error {
            CliError::PostsSnapshotChanged { .. }
            | CliError::PreviewRevisionMismatch { .. }
            | CliError::PreviewContentDigestMismatch { .. }
            | CliError::PreviewOutputExists { .. } => return CONFLICT,
            CliError::InvalidPreviewSelector { .. } => return VALIDATION,
            CliError::PreviewOutput { source, .. }
                if source.kind() == io::ErrorKind::PermissionDenied =>
            {
                return PERMISSION;
            }
            CliError::InvalidPostsPagination { .. }
            | CliError::InvalidPublicationResponse { .. }
            | CliError::PreviewOutput { .. }
            | CliError::Output(_)
            | CliError::Encode(_) => {
                return INTERNAL;
            }
            CliError::Admin(_) | CliError::Publication { .. } => {}
        }
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
            | AdminClientError::InvalidResponse(_)
            | AdminClientError::InvalidPreviewResponse { .. }
            | AdminClientError::InvalidPublicationResponse { .. } => INTERNAL,
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
                PERMISSION => "permission",
                CONFIGURATION => "configuration",
                _ => "internal",
            },
        }
    }

    fn admin_error(error: &CliError) -> Option<&AdminClientError> {
        match error {
            CliError::Admin(error) | CliError::Publication { source: error, .. } => Some(error),
            CliError::PostsSnapshotChanged { .. }
            | CliError::InvalidPostsPagination { .. }
            | CliError::InvalidPublicationResponse { .. }
            | CliError::InvalidPreviewSelector { .. }
            | CliError::PreviewRevisionMismatch { .. }
            | CliError::PreviewContentDigestMismatch { .. }
            | CliError::PreviewOutputExists { .. }
            | CliError::PreviewOutput { .. }
            | CliError::Output(_)
            | CliError::Encode(_) => None,
        }
    }

    #[cfg(test)]
    mod tests {
        use maincopy_shared::FeatureVersions;
        use serde_json::json;

        use super::*;

        const PREVIEW_DIGEST: &str =
            "preview-b3-v1-4444444444444444444444444444444444444444444444444444444444444444";

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
                "preview_digest": PREVIEW_DIGEST,
                "revision":
                    "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111",
                "state": "published",
                "published_at": "2026-08-30T12:00:00Z",
                "site_digest":
                    "site-b3-v1-2222222222222222222222222222222222222222222222222222222222222222",
                "site_version": 2
            }))
            .unwrap()
        }

        fn scheduled_response() -> PublishNowResponse {
            serde_json::from_value(json!({
                "publication_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "post_id": "11111111-1111-4111-8111-111111111111",
                "preview_digest": PREVIEW_DIGEST,
                "revision":
                    "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111",
                "state": "scheduled",
                "scheduled_for": "2026-09-01T12:30:00Z",
                "published_at": null,
                "site_digest":
                    "site-b3-v1-2222222222222222222222222222222222222222222222222222222222222222",
                "site_version": 2
            }))
            .unwrap()
        }

        fn posts_response() -> ListPostsResponse {
            serde_json::from_value(json!({
                "content_digest":
                    "content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333",
                "site_digest":
                    "site-b3-v1-2222222222222222222222222222222222222222222222222222222222222222",
                "site_version": 2,
                "posts": [
                    {
                        "post_id": "11111111-1111-4111-8111-111111111111",
                        "source_path": "posts/ready.md",
                        "title": "Ready to publish",
                        "slug": "ready-to-publish",
                        "revision":
                            "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111",
                        "publication_state": "unpublished_change",
                        "published_at": "2026-08-29T12:00:00Z"
                    },
                    {
                        "post_id": "22222222-2222-4222-8222-222222222222",
                        "source_path": "posts/already-live.md",
                        "title": "Already live",
                        "slug": "already-live",
                        "revision":
                            "post-b3-v1-2222222222222222222222222222222222222222222222222222222222222222",
                        "publication_state": "published",
                        "published_at": "2026-08-30T12:00:00Z"
                    }
                ],
                "next_cursor": null
            }))
            .unwrap()
        }

        fn preview_response() -> PostPreview {
            PostPreview {
                html: "<!doctype html><title>Ready</title>".into(),
                preview_digest: PreviewDigest::parse(PREVIEW_DIGEST).unwrap(),
                revision:
                    "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111"
                        .into(),
                content_digest:
                    "content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333"
                        .into(),
                canonical_url: "https://example.test/posts/ready".into(),
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
                Some(
                    "content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333"
                )
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
                let mut arguments =
                    vec!["maincopy", command, "11111111-1111-4111-8111-111111111111"];
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
        fn posts_json_output_is_the_combined_shared_response() {
            let mut output = Vec::new();

            write_posts(&mut output, posts_response(), true).unwrap();

            let value = serde_json::from_slice::<serde_json::Value>(&output).unwrap();
            assert_eq!(value["site_version"], 2);
            assert_eq!(value["posts"].as_array().unwrap().len(), 2);
            assert!(value["next_cursor"].is_null());
            assert_eq!(value["posts"][0]["source_path"], "posts/ready.md");
            assert_eq!(value["posts"][0]["publication_state"], "unpublished_change");
        }

        #[test]
        fn posts_human_output_exposes_operator_selection_fields() {
            let mut output = Vec::new();

            write_posts(&mut output, posts_response(), false).unwrap();

            assert_eq!(
                String::from_utf8(output).unwrap(),
                concat!(
                    "Site: site-b3-v1-2222222222222222222222222222222222222222222222222222222222222222 (version 2)\n",
                    "Content: content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333\n",
                    "Posts: 2\n",
                    "\n",
                    "[unpublished_change] Ready to publish\n",
                    "  ID: 11111111-1111-4111-8111-111111111111\n",
                    "  Revision: post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111\n",
                    "  Source: posts/ready.md\n",
                    "  Slug: ready-to-publish\n",
                    "  Current publication at: 2026-08-29 12:00:00.0 +00:00:00\n",
                    "\n",
                    "[published] Already live\n",
                    "  ID: 22222222-2222-4222-8222-222222222222\n",
                    "  Revision: post-b3-v1-2222222222222222222222222222222222222222222222222222222222222222\n",
                    "  Source: posts/already-live.md\n",
                    "  Slug: already-live\n",
                    "  Published at: 2026-08-30 12:00:00.0 +00:00:00\n"
                )
            );
        }

        #[test]
        fn preview_output_reports_only_metadata_in_human_and_json_modes() {
            let path = PathBuf::from("ready.html");
            let post_id = Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
            let mut json_output = Vec::new();
            write_preview(&mut json_output, post_id, &path, preview_response(), true).unwrap();
            let document = serde_json::from_slice::<serde_json::Value>(&json_output).unwrap();
            assert_eq!(document["post_id"], post_id.to_string());
            assert_eq!(document["preview_digest"], PREVIEW_DIGEST);
            assert_eq!(
                document["revision"],
                "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111"
            );
            assert_eq!(
                document["content_digest"],
                "content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333"
            );
            assert_eq!(
                document["canonical_url"],
                "https://example.test/posts/ready"
            );
            assert_eq!(document["output"], "ready.html");
            assert!(
                !String::from_utf8(json_output)
                    .unwrap()
                    .contains("<!doctype")
            );

            let mut human_output = Vec::new();
            write_preview(&mut human_output, post_id, &path, preview_response(), false).unwrap();
            assert_eq!(
                String::from_utf8(human_output).unwrap(),
                concat!(
                    "Preview: preview-b3-v1-4444444444444444444444444444444444444444444444444444444444444444\n",
                    "Post: 11111111-1111-4111-8111-111111111111\n",
                    "Revision: post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111\n",
                    "Content: content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333\n",
                    "Canonical: https://example.test/posts/ready\n",
                    "Output: ready.html\n",
                )
            );
        }

        #[test]
        fn preview_file_creation_never_overwrites_an_existing_path() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("preview.html");
            write_preview_file(&path, "first").unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");

            let error = write_preview_file(&path, "second").unwrap_err();
            assert!(matches!(error, CliError::PreviewOutputExists { .. }));
            assert_eq!(error_exit(&error), CONFLICT);
            assert_eq!(std::fs::read_to_string(path).unwrap(), "first");
        }

        #[test]
        fn malformed_preview_selectors_are_local_validation_errors() {
            for (value, prefix, field) in [
                ("post-b3-v1-UPPER", POST_REVISION_PREFIX, "revision"),
                (
                    "content-b3-v1-short",
                    CONTENT_DIGEST_PREFIX,
                    "content_digest",
                ),
            ] {
                let error = validate_optional_digest(Some(value), prefix, field).unwrap_err();
                assert!(matches!(error, CliError::InvalidPreviewSelector { .. }));
                assert_eq!(error_exit(&error), VALIDATION);
                assert_eq!(error_category(&error, VALIDATION), "validation");
            }
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
                    "preview_digest": PREVIEW_DIGEST,
                    "revision":
                        "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111",
                    "state": "published",
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
Status: published\n\
Post: 11111111-1111-4111-8111-111111111111\n\
Preview: preview-b3-v1-4444444444444444444444444444444444444444444444444444444444444444\n\
Pinned revision: post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111\n\
Published at: 2026-08-30 12:00:00.0 +00:00:00\n\
Site: site-b3-v1-2222222222222222222222222222222222222222222222222222222222222222 (version 2)\n\
Idempotency key: bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\n"
            );
        }

        #[test]
        fn scheduled_human_output_never_claims_the_revision_is_published() {
            let mut output = Vec::new();
            let idempotency_key = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();

            write_publication(&mut output, idempotency_key, scheduled_response(), false).unwrap();

            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("Status: scheduled\n"));
            assert!(output.contains(
                "Pinned revision: post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111\n"
            ));
            assert!(output.contains("Scheduled for: 2026-09-01 12:30:00.0 +00:00:00\n"));
            assert!(!output.contains("Published at:"));
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
