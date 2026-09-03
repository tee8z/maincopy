//! CLI process startup, command execution, and output handling.

use std::{
    collections::HashSet,
    fs::OpenOptions,
    future::Future,
    io::{self, Write as _},
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::Parser;
use maincopy_shared::{
    AdminApiVersion, Capabilities, CapabilityContractVersion,
    auth_api::{AdminSessionResponse, RevokeAdminSessionResponse, SecretString},
    posts::{ListPostsResponse, PostPublicationState, PostSummary},
    publication::{PreviewDigest, PublicationApprovalState, PublishNowRequest, PublishNowResponse},
};
use serde::Serialize;
use serde_json::json;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    client::{AdminClient, AdminClientError, PostPreview},
    models::{AgentKeyCommand, Arguments, Command},
    transport::AdditionalRootCertificateError,
};

const SUCCESS: u8 = 0;
const VALIDATION: u8 = 65;
const UNAVAILABLE: u8 = 69;
const INTERNAL: u8 = 70;
const CONFLICT: u8 = 75;
const PERMISSION: u8 = 77;
const POSTS_PAGE_LIMIT: u16 = 100;
const MAX_POSTS_PAGES: usize = 10_001;
const POST_REVISION_PREFIX: &str = "post-b3-v1-";
const CONTENT_DIGEST_PREFIX: &str = "content-b3-v1-";

enum CommandOutput {
    Login(AdminSessionResponse),
    Logout(RevokeAdminSessionResponse),
    AgentKeyConfigured {
        public_key: Box<str>,
    },
    AgentKeyRemoved,
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

struct PreviewSelection {
    post_id: Uuid,
    output: PathBuf,
    revision: Option<String>,
    content_digest: Option<String>,
}

impl PreviewSelection {
    fn new(
        post_id: Uuid,
        output: PathBuf,
        revision: Option<String>,
        content_digest: Option<String>,
    ) -> Result<Self, CliError> {
        validate_optional_digest(revision.as_deref(), POST_REVISION_PREFIX, "revision")?;
        validate_optional_digest(
            content_digest.as_deref(),
            CONTENT_DIGEST_PREFIX,
            "content_digest",
        )?;
        Ok(Self {
            post_id,
            output,
            revision,
            content_digest,
        })
    }

    fn accept(self, preview: PostPreview) -> Result<CommandOutput, CliError> {
        if let Some(expected) = self.revision
            && expected.as_str() != preview.revision.as_ref()
        {
            return Err(CliError::PreviewRevisionMismatch {
                expected: expected.into_boxed_str(),
                actual: preview.revision.clone(),
            });
        }
        if let Some(expected) = self.content_digest
            && expected.as_str() != preview.content_digest.as_ref()
        {
            return Err(CliError::PreviewContentDigestMismatch {
                expected: expected.into_boxed_str(),
                actual: preview.content_digest.clone(),
            });
        }
        write_preview_file(&self.output, &preview.html)?;
        Ok(CommandOutput::Preview {
            post_id: self.post_id,
            output: self.output,
            preview,
        })
    }
}

#[derive(Debug, Error)]
enum CliError {
    #[error(transparent)]
    Admin(#[from] AdminClientError),

    #[error("failed to read a secret from the protected terminal: {0}")]
    SecretInput(#[source] io::Error),

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
    let client = AdminClient::new(
        &arguments.admin_origin,
        arguments.auth_context,
        arguments.admin_ca_file.as_deref(),
    )?;
    match arguments.command {
        Command::Login { username } => login(&client, username).await,
        Command::Logout => client
            .logout()
            .await
            .map(CommandOutput::Logout)
            .map_err(CliError::from),
        Command::AgentKey {
            command: AgentKeyCommand::Set,
        } => configure_agent_key(&client),
        Command::AgentKey {
            command: AgentKeyCommand::Remove,
        } => client
            .remove_agent_private_key()
            .map(|()| CommandOutput::AgentKeyRemoved)
            .map_err(CliError::from),
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

async fn login(client: &AdminClient, username: Box<str>) -> Result<CommandOutput, CliError> {
    client.ensure_human_session_absent()?;
    let password = rpassword::prompt_password("Password: ").map_err(CliError::SecretInput)?;
    client
        .login_with_password(username, SecretString::new(password.into_boxed_str()))
        .await
        .map(CommandOutput::Login)
        .map_err(CliError::from)
}

fn configure_agent_key(client: &AdminClient) -> Result<CommandOutput, CliError> {
    let key = rpassword::prompt_password("Nostr private key (lowercase hex): ")
        .map_err(CliError::SecretInput)?;
    client
        .configure_agent_private_key(SecretString::new(key.into_boxed_str()))
        .map(|public_key| CommandOutput::AgentKeyConfigured { public_key })
        .map_err(CliError::from)
}

async fn preview_post(
    client: &AdminClient,
    post_id: Uuid,
    output: PathBuf,
    revision: Option<String>,
    content_digest: Option<String>,
) -> Result<CommandOutput, CliError> {
    let selection = PreviewSelection::new(post_id, output, revision, content_digest)?;
    let preview = client
        .preview_post(
            selection.post_id,
            selection.revision.as_deref(),
            selection.content_digest.as_deref(),
        )
        .await?;
    selection.accept(preview)
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

async fn list_all_posts(client: &AdminClient) -> Result<ListPostsResponse, CliError> {
    collect_post_pages(|cursor| client.list_posts_page(cursor, POSTS_PAGE_LIMIT)).await
}

async fn collect_post_pages<Fetch, FetchFuture>(
    mut fetch: Fetch,
) -> Result<ListPostsResponse, CliError>
where
    Fetch: FnMut(Option<Uuid>) -> FetchFuture,
    FetchFuture: Future<Output = Result<ListPostsResponse, AdminClientError>>,
{
    let first = fetch(None).await?;
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

        let page = fetch(Some(cursor)).await?;
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
        CommandOutput::Login(session) => write_login(output, session, json),
        CommandOutput::Logout(revoked) => write_logout(output, revoked, json),
        CommandOutput::AgentKeyConfigured { public_key } => {
            write_agent_key_configured(output, &public_key, json)
        }
        CommandOutput::AgentKeyRemoved => write_agent_key_removed(output, json),
        CommandOutput::Capabilities(capabilities) => write_capabilities(output, capabilities, json),
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

fn write_login(
    mut output: impl io::Write,
    session: AdminSessionResponse,
    json: bool,
) -> Result<(), CliError> {
    if json {
        serde_json::to_writer(&mut output, &session)?;
        writeln!(output)?;
        return Ok(());
    }
    writeln!(output, "Session: {}", session.session_id)?;
    writeln!(output, "User: {}", session.user_id)?;
    writeln!(output, "Provider: {}", session.provider.as_str())?;
    writeln!(
        output,
        "Roles: {}",
        session
            .roles
            .iter()
            .map(|role| role.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    writeln!(output, "Expires at: {}", session.expires_at)?;
    Ok(())
}

fn write_logout(
    mut output: impl io::Write,
    revoked: RevokeAdminSessionResponse,
    json: bool,
) -> Result<(), CliError> {
    if json {
        serde_json::to_writer(&mut output, &revoked)?;
        writeln!(output)?;
    } else {
        writeln!(output, "Revoked session: {}", revoked.session_id)?;
    }
    Ok(())
}

fn write_agent_key_configured(
    mut output: impl io::Write,
    public_key: &str,
    json: bool,
) -> Result<(), CliError> {
    if json {
        writeln!(
            output,
            "{}",
            json!({ "public_key": public_key, "configured": true })
        )?;
    } else {
        writeln!(output, "Agent public key: {public_key}")?;
    }
    Ok(())
}

fn write_agent_key_removed(mut output: impl io::Write, json: bool) -> Result<(), CliError> {
    if json {
        writeln!(output, "{}", json!({ "removed": true }))?;
    } else {
        writeln!(output, "Agent key removed")?;
    }
    Ok(())
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
                PostPublicationState::Draft | PostPublicationState::Unpublished => "Publication at",
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
        #[derive(Serialize)]
        struct PublicationOutput<'response> {
            #[serde(flatten)]
            response: &'response PublishNowResponse,
            idempotency_key: Uuid,
        }

        serde_json::to_writer(
            &mut output,
            &PublicationOutput {
                response: &response,
                idempotency_key,
            },
        )?;
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
        CliError::SecretInput(source) if source.kind() == io::ErrorKind::PermissionDenied => {
            return PERMISSION;
        }
        CliError::PreviewOutput { source, .. }
            if source.kind() == io::ErrorKind::PermissionDenied =>
        {
            return PERMISSION;
        }
        CliError::InvalidPostsPagination { .. }
        | CliError::InvalidPublicationResponse { .. }
        | CliError::SecretInput(_)
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
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        AdminClientError::AdditionalRootCertificates(
            AdditionalRootCertificateError::UnsupportedPlatform { .. },
        ) => VALIDATION,
        AdminClientError::AdditionalRootCertificates(
            AdditionalRootCertificateError::Open { source, .. }
            | AdditionalRootCertificateError::Read { source, .. },
        ) if source.kind() == io::ErrorKind::PermissionDenied => PERMISSION,
        AdminClientError::AdditionalRootCertificates(AdditionalRootCertificateError::Open {
            source,
            ..
        }) if source.kind() == io::ErrorKind::NotFound => VALIDATION,
        AdminClientError::AdditionalRootCertificates(
            AdditionalRootCertificateError::Open { .. }
            | AdditionalRootCertificateError::Read { .. },
        ) => UNAVAILABLE,
        AdminClientError::AdditionalRootCertificates(
            AdditionalRootCertificateError::NotRegularFile { .. }
            | AdditionalRootCertificateError::ChangedDuringOpen { .. }
            | AdditionalRootCertificateError::TooLarge { .. }
            | AdditionalRootCertificateError::UnexpectedPemSection { .. }
            | AdditionalRootCertificateError::InvalidBundle { .. }
            | AdditionalRootCertificateError::InvalidCount { .. },
        ) => VALIDATION,
        AdminClientError::CredentialStore(_) | AdminClientError::Transport(_) => UNAVAILABLE,
        AdminClientError::InvalidAdminOrigin
        | AdminClientError::InvalidRequestTarget
        | AdminClientError::RequestBodyTooLarge
        | AdminClientError::AgentPrivateKey(_) => VALIDATION,
        AdminClientError::HumanCredentialsMissing
        | AdminClientError::AgentCredentialsMissing
        | AdminClientError::StoredCredentialsInvalid
        | AdminClientError::HumanContextRequired => PERMISSION,
        AdminClientError::HumanSessionAlreadyStored => CONFLICT,
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
        AdminClientError::HttpStatus { status, .. } if status.as_u16() == 429 => UNAVAILABLE,
        AdminClientError::HttpStatus { .. }
        | AdminClientError::UnexpectedSuccessStatus { .. }
        | AdminClientError::InvalidContentType { .. }
        | AdminClientError::InvalidAuthenticationResponse { .. }
        | AdminClientError::RequestEncoding(_)
        | AdminClientError::Nip98Signing(_)
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
            _ => "internal",
        },
    }
}

fn admin_error(error: &CliError) -> Option<&AdminClientError> {
    match error {
        CliError::Admin(error) | CliError::Publication { source: error, .. } => Some(error),
        CliError::PostsSnapshotChanged { .. }
        | CliError::SecretInput(_)
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
    use std::{collections::VecDeque, future::ready};

    use maincopy_shared::FeatureVersions;
    use serde_json::json;

    use super::*;
    use crate::client::AdminProblem;

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
            revision: "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            content_digest:
                "content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333"
                    .into(),
            canonical_url: "https://example.test/posts/ready".into(),
        }
    }

    fn session_response() -> AdminSessionResponse {
        serde_json::from_value(json!({
            "session_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "user_id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            "provider": "password",
            "roles": ["owner", "publisher"],
            "scopes": ["content_read"],
            "fresh_until": "2026-09-03T13:00:00Z",
            "expires_at": "2026-09-04T12:00:00Z"
        }))
        .unwrap()
    }

    fn post_page(posts: Vec<PostSummary>, next_cursor: Option<Uuid>) -> ListPostsResponse {
        ListPostsResponse {
            content_digest:
                "content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333"
                    .into(),
            site_digest:
                "site-b3-v1-2222222222222222222222222222222222222222222222222222222222222222".into(),
            site_version: 2,
            posts,
            next_cursor,
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
    fn certificate_authority_failures_use_stable_exit_categories() {
        let path = PathBuf::from("development-ca.pem");
        let error = |source| CliError::Admin(AdminClientError::AdditionalRootCertificates(source));

        let permission = error(AdditionalRootCertificateError::Open {
            path: path.clone(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        });
        assert_eq!(error_exit(&permission), PERMISSION);

        let missing = error(AdditionalRootCertificateError::Open {
            path: path.clone(),
            source: io::Error::from(io::ErrorKind::NotFound),
        });
        assert_eq!(error_exit(&missing), VALIDATION);

        let unavailable = error(AdditionalRootCertificateError::Read {
            path: path.clone(),
            source: io::Error::from(io::ErrorKind::BrokenPipe),
        });
        assert_eq!(error_exit(&unavailable), UNAVAILABLE);

        for validation in [
            AdditionalRootCertificateError::NotRegularFile { path: path.clone() },
            AdditionalRootCertificateError::ChangedDuringOpen { path: path.clone() },
            AdditionalRootCertificateError::TooLarge { path: path.clone() },
            AdditionalRootCertificateError::UnexpectedPemSection { path: path.clone() },
            AdditionalRootCertificateError::InvalidCount {
                path: path.clone(),
                count: 0,
            },
        ] {
            assert_eq!(error_exit(&error(validation)), VALIDATION);
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert_eq!(
            error_exit(&error(
                AdditionalRootCertificateError::UnsupportedPlatform { path }
            )),
            VALIDATION
        );
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
    fn preview_selection_validates_server_identity_before_creating_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let post_id = Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
        let revision = preview_response().revision.into_string();
        let content_digest = preview_response().content_digest.into_string();

        let revision_path = directory.path().join("revision-mismatch.html");
        let selection = PreviewSelection::new(
            post_id,
            revision_path.clone(),
            Some(format!("{POST_REVISION_PREFIX}{}", "9".repeat(64))),
            Some(content_digest.clone()),
        )
        .unwrap();
        assert!(matches!(
            selection.accept(preview_response()),
            Err(CliError::PreviewRevisionMismatch { .. })
        ));
        assert!(!revision_path.exists());

        let content_path = directory.path().join("content-mismatch.html");
        let selection = PreviewSelection::new(
            post_id,
            content_path.clone(),
            Some(revision.clone()),
            Some(format!("{CONTENT_DIGEST_PREFIX}{}", "9".repeat(64))),
        )
        .unwrap();
        assert!(matches!(
            selection.accept(preview_response()),
            Err(CliError::PreviewContentDigestMismatch { .. })
        ));
        assert!(!content_path.exists());

        let output = directory.path().join("accepted.html");
        let selection = PreviewSelection::new(
            post_id,
            output.clone(),
            Some(revision),
            Some(content_digest),
        )
        .unwrap();
        assert!(matches!(
            selection.accept(preview_response()).unwrap(),
            CommandOutput::Preview { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "<!doctype html><title>Ready</title>"
        );
    }

    #[tokio::test]
    async fn pagination_combines_stable_pages_in_request_order() {
        let mut expected = posts_response();
        let second_post = expected.posts.pop().unwrap();
        let first_post = expected.posts.pop().unwrap();
        expected.posts = vec![first_post.clone(), second_post.clone()];
        let cursor = second_post.post_id;
        let mut pages = VecDeque::from([
            Ok(post_page(vec![first_post], Some(cursor))),
            Ok(post_page(vec![second_post], None)),
        ]);
        let mut requested = Vec::new();

        let combined = collect_post_pages(|cursor| {
            requested.push(cursor);
            ready(pages.pop_front().unwrap())
        })
        .await
        .unwrap();

        assert_eq!(requested, [None, Some(cursor)]);
        assert_eq!(combined, expected);
    }

    #[tokio::test]
    async fn pagination_rejects_snapshot_changes_repeated_cursors_and_posts() {
        let cursor = Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap();
        for change in ["content", "site", "version"] {
            let first = post_page(Vec::new(), Some(cursor));
            let mut changed = post_page(Vec::new(), None);
            match change {
                "content" => {
                    changed.content_digest =
                        format!("{CONTENT_DIGEST_PREFIX}{}", "4".repeat(64)).into()
                }
                "site" => changed.site_digest = format!("site-b3-v1-{}", "4".repeat(64)).into(),
                "version" => changed.site_version += 1,
                _ => unreachable!(),
            }
            let mut pages = VecDeque::from([Ok(first), Ok(changed)]);
            let error = collect_post_pages(|_| ready(pages.pop_front().unwrap()))
                .await
                .unwrap_err();
            assert!(
                matches!(error, CliError::PostsSnapshotChanged { .. }),
                "{change}"
            );
        }

        let repeated = post_page(Vec::new(), Some(cursor));
        let mut pages = VecDeque::from([Ok(repeated.clone()), Ok(repeated)]);
        let error = collect_post_pages(|_| ready(pages.pop_front().unwrap()))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            CliError::InvalidPostsPagination {
                message: "next_cursor repeated an earlier cursor"
            }
        ));

        let post = posts_response().posts.remove(0);
        let mut pages = VecDeque::from([
            Ok(post_page(vec![post.clone()], Some(cursor))),
            Ok(post_page(vec![post], None)),
        ]);
        let error = collect_post_pages(|_| ready(pages.pop_front().unwrap()))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            CliError::InvalidPostsPagination {
                message: "a post UUID appeared more than once"
            }
        ));
    }

    #[tokio::test]
    async fn pagination_enforces_the_page_count_safety_limit() {
        let mut next = 1_u128;
        let error = collect_post_pages(|_| {
            let cursor = Uuid::from_u128(next);
            next += 1;
            ready(Ok(post_page(Vec::new(), Some(cursor))))
        })
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            CliError::InvalidPostsPagination {
                message: "the page count exceeded the client safety limit"
            }
        ));
        assert_eq!(next, MAX_POSTS_PAGES as u128 + 1);
    }

    #[test]
    fn authentication_and_agent_key_outputs_have_stable_human_and_json_forms() {
        let session = session_response();
        let mut json_output = Vec::new();
        write_login(&mut json_output, session.clone(), true).unwrap();
        assert_eq!(
            serde_json::from_slice::<AdminSessionResponse>(&json_output).unwrap(),
            session
        );

        let mut human_output = Vec::new();
        write_login(&mut human_output, session, false).unwrap();
        assert_eq!(
            String::from_utf8(human_output).unwrap(),
            concat!(
                "Session: aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\n",
                "User: bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\n",
                "Provider: password\n",
                "Roles: owner, publisher\n",
                "Expires at: 2026-09-04 12:00:00.0 +00:00:00\n",
            )
        );

        let revoked: RevokeAdminSessionResponse = serde_json::from_value(json!({
            "session_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        }))
        .unwrap();
        let mut json_output = Vec::new();
        write_logout(&mut json_output, revoked, true).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&json_output).unwrap(),
            json!({"session_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"})
        );
        let mut human_output = Vec::new();
        write_logout(&mut human_output, revoked, false).unwrap();
        assert_eq!(
            String::from_utf8(human_output).unwrap(),
            "Revoked session: aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\n"
        );

        for json in [false, true] {
            let mut configured = Vec::new();
            write_agent_key_configured(&mut configured, "public-key", json).unwrap();
            let configured = String::from_utf8(configured).unwrap();
            if json {
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&configured).unwrap(),
                    json!({"public_key": "public-key", "configured": true})
                );
            } else {
                assert_eq!(configured, "Agent public key: public-key\n");
            }

            let mut removed = Vec::new();
            write_agent_key_removed(&mut removed, json).unwrap();
            let removed = String::from_utf8(removed).unwrap();
            if json {
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&removed).unwrap(),
                    json!({"removed": true})
                );
            } else {
                assert_eq!(removed, "Agent key removed\n");
            }
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
                problem: Some(AdminProblem {
                    code: "publication_unavailable".into(),
                    message: "publication is temporarily unavailable".into(),
                }),
                request_id: Some(Uuid::parse_str("cccccccc-cccc-4ccc-8ccc-cccccccccccc").unwrap()),
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
