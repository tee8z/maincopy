//! CLI process startup, command execution, and output handling.

use std::{
    collections::HashSet,
    fs::OpenOptions,
    future::Future,
    io::{self, Write as _},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use clap::Parser;
use maincopy_shared::{
    AdminApiVersion, Capabilities, CapabilityContractVersion,
    auth_api::{AdminSessionResponse, RevokeAdminSessionResponse, SecretString},
    posts::{ListPostsResponse, PostPublicationState, PostSummary},
    publication::{
        ChangeReleaseRequest, ListReleasesResponse, PreviewDigest, PublicationApprovalState,
        PublishNowRequest, PublishNowResponse, ReleaseOperationResource, ReleaseResource,
        ReleaseState,
    },
    source::{
        BeginSourceSyncResponse, SourceStatusResponse, SourceSyncAdmission, SourceSyncFailureCode,
        SourceSyncId, SourceSyncOutcome, SourceSyncResource, valid_source_commit,
        valid_source_content_digest,
    },
};
use serde::Serialize;
use serde_json::json;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    client::{AdminClient, AdminClientError, AdminProblem, PostPreview},
    models::{
        AgentKeyCommand, Arguments, Command, ReleaseCommand, ReleaseTarget, SourceCommand,
        SourceSyncDisposition, SourceSyncInvocation,
    },
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
const SOURCE_SYNC_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SOURCE_SYNC_WAIT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_SOURCE_SYNC_POLLS: usize = 600;

enum CommandOutput {
    Login(AdminSessionResponse),
    Logout(RevokeAdminSessionResponse),
    AgentKeyConfigured {
        public_key: Box<str>,
    },
    AgentKeyRemoved,
    Capabilities(Capabilities),
    Posts(ListPostsResponse),
    Releases(ListReleasesResponse),
    Release(ReleaseResource),
    ReleaseOperation(ReleaseOperationResource),
    SourceStatus(Box<SourceStatusResponse>),
    SourceSync {
        idempotency_key: Uuid,
        admission: SourceSyncAdmission,
        sync: SourceSyncResource,
    },
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

    #[error("release {publication_id} operation {operation_id} failed: {source}")]
    ReleaseChange {
        publication_id: Uuid,
        operation_id: Uuid,
        #[source]
        source: AdminClientError,
    },

    #[error("source synchronization command {idempotency_key} could not be started: {source}")]
    SourceSyncStart {
        idempotency_key: Uuid,
        #[source]
        source: AdminClientError,
    },

    #[error(
        "source synchronization {source_sync_id} (command {idempotency_key}) could not be followed: {source}"
    )]
    SourceSyncFollow {
        idempotency_key: Uuid,
        source_sync_id: SourceSyncId,
        #[source]
        source: AdminClientError,
    },

    #[error(
        "source synchronization {source_sync_id} (command {idempotency_key}) did not finish within the client wait limit"
    )]
    SourceSyncTimedOut {
        idempotency_key: Uuid,
        source_sync_id: SourceSyncId,
    },

    #[error(
        "source synchronization {source_sync_id} (command {idempotency_key}) finished with {outcome}"
    )]
    SourceSyncTerminalFailure {
        idempotency_key: Uuid,
        source_sync_id: SourceSyncId,
        outcome: &'static str,
        failure_code: Option<SourceSyncFailureCode>,
    },

    #[error(
        "the admin server returned inconsistent source synchronization {source_sync_id} (command {idempotency_key}): {message}"
    )]
    InvalidSourceSyncResponse {
        idempotency_key: Uuid,
        source_sync_id: SourceSyncId,
        message: &'static str,
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

async fn execute_releases(
    client: &AdminClient,
    command: ReleaseCommand,
) -> Result<CommandOutput, CliError> {
    match command {
        ReleaseCommand::List { cursor } => client
            .releases(cursor)
            .await
            .map(CommandOutput::Releases)
            .map_err(CliError::from),
        ReleaseCommand::Inspect { publication_id } => client
            .release(publication_id)
            .await
            .map(CommandOutput::Release)
            .map_err(CliError::from),
        ReleaseCommand::Operation { operation_id } => client
            .release_operation(operation_id)
            .await
            .map(CommandOutput::ReleaseOperation)
            .map_err(CliError::from),
        ReleaseCommand::Reschedule { target, at } => {
            let request = ChangeReleaseRequest::Reschedule {
                expected_version: target.expected_version,
                scheduled_for: at,
            };
            change_release(client, target, request).await
        }
        ReleaseCommand::Cancel(target) => {
            let request = ChangeReleaseRequest::Cancel {
                expected_version: target.expected_version,
            };
            change_release(client, target, request).await
        }
        ReleaseCommand::Retry(target) => {
            let request = ChangeReleaseRequest::Retry {
                expected_version: target.expected_version,
            };
            change_release(client, target, request).await
        }
    }
}

async fn change_release(
    client: &AdminClient,
    target: ReleaseTarget,
    request: ChangeReleaseRequest,
) -> Result<CommandOutput, CliError> {
    let publication_id = target.publication_id;
    let operation_id = target.idempotency_key.unwrap_or_else(Uuid::new_v4);
    client
        .change_release(publication_id, operation_id, &request)
        .await
        .map(CommandOutput::ReleaseOperation)
        .map_err(|source| CliError::ReleaseChange {
            publication_id,
            operation_id,
            source,
        })
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
        Command::Releases { command } => execute_releases(&client, command).await,
        Command::Source {
            command: SourceCommand::Status,
        } => client
            .source_status()
            .await
            .map(Box::new)
            .map(CommandOutput::SourceStatus)
            .map_err(CliError::from),
        Command::Source {
            command: SourceCommand::Sync(arguments),
        } => {
            let SourceSyncInvocation {
                disposition,
                idempotency_key,
            } = arguments.into_invocation();
            source_sync(&client, disposition, idempotency_key).await
        }
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

async fn source_sync(
    client: &AdminClient,
    disposition: SourceSyncDisposition,
    idempotency_key: Option<Uuid>,
) -> Result<CommandOutput, CliError> {
    let idempotency_key = idempotency_key.unwrap_or_else(Uuid::new_v4);
    let BeginSourceSyncResponse { admission, sync } = client
        .begin_source_sync(idempotency_key)
        .await
        .map_err(|source| CliError::SourceSyncStart {
            idempotency_key,
            source,
        })?;

    let sync = complete_source_sync(client, disposition, sync, idempotency_key).await?;

    Ok(CommandOutput::SourceSync {
        idempotency_key,
        admission,
        sync,
    })
}

async fn complete_source_sync(
    client: &AdminClient,
    disposition: SourceSyncDisposition,
    sync: SourceSyncResource,
    idempotency_key: Uuid,
) -> Result<SourceSyncResource, CliError> {
    match disposition {
        SourceSyncDisposition::Async => {
            validate_source_sync(&sync, idempotency_key)?;
            Ok(sync)
        }
        SourceSyncDisposition::Wait => wait_for_source_sync(client, sync, idempotency_key).await,
    }
}

async fn wait_for_source_sync(
    client: &AdminClient,
    initial: SourceSyncResource,
    idempotency_key: Uuid,
) -> Result<SourceSyncResource, CliError> {
    let source_sync_id = initial.source_sync_id;
    tokio::time::timeout(
        SOURCE_SYNC_WAIT_TIMEOUT,
        poll_source_sync(
            initial,
            idempotency_key,
            |source_sync_id| client.source_sync(source_sync_id),
            source_sync_poll_pause,
            MAX_SOURCE_SYNC_POLLS,
        ),
    )
    .await
    .map_err(|_| CliError::SourceSyncTimedOut {
        idempotency_key,
        source_sync_id,
    })?
}

async fn source_sync_poll_pause() {
    tokio::time::sleep(SOURCE_SYNC_POLL_INTERVAL).await;
}

async fn poll_source_sync<Fetch, FetchFuture, Pause, PauseFuture>(
    initial: SourceSyncResource,
    idempotency_key: Uuid,
    mut fetch: Fetch,
    mut pause: Pause,
    maximum_polls: usize,
) -> Result<SourceSyncResource, CliError>
where
    Fetch: FnMut(SourceSyncId) -> FetchFuture,
    FetchFuture: Future<Output = Result<SourceSyncResource, AdminClientError>>,
    Pause: FnMut() -> PauseFuture,
    PauseFuture: Future<Output = ()>,
{
    let source_sync_id = initial.source_sync_id;
    let configuration_version = initial.configuration_version;
    let request_origin = initial.request_origin;
    let requested_at = initial.requested_at;
    let mut previous_version = initial.version;
    let mut previous_updated_at = initial.updated_at;
    if validate_source_sync(&initial, idempotency_key)? {
        return Ok(initial);
    }

    for _ in 0..maximum_polls {
        pause().await;
        let current = fetch(source_sync_id)
            .await
            .map_err(|source| CliError::SourceSyncFollow {
                idempotency_key,
                source_sync_id,
                source,
            })?;
        if current.source_sync_id != source_sync_id
            || current.configuration_version != configuration_version
            || current.request_origin != request_origin
            || current.requested_at != requested_at
        {
            return Err(invalid_source_sync(
                idempotency_key,
                source_sync_id,
                "operation identity changed while polling",
            ));
        }
        if current.version < previous_version || current.updated_at < previous_updated_at {
            return Err(invalid_source_sync(
                idempotency_key,
                source_sync_id,
                "operation version or update time moved backwards",
            ));
        }
        previous_version = current.version;
        previous_updated_at = current.updated_at;
        if validate_source_sync(&current, idempotency_key)? {
            return Ok(current);
        }
    }

    Err(CliError::SourceSyncTimedOut {
        idempotency_key,
        source_sync_id,
    })
}

fn validate_source_sync(
    sync: &SourceSyncResource,
    idempotency_key: Uuid,
) -> Result<bool, CliError> {
    let invalid = |message| invalid_source_sync(idempotency_key, sync.source_sync_id, message);
    if sync.version == 0 {
        return Err(invalid("operation version must be positive"));
    }
    if sync.updated_at < sync.requested_at {
        return Err(invalid("updated_at precedes requested_at"));
    }
    if sync
        .source_commit
        .as_deref()
        .is_some_and(|source_commit| !valid_source_commit(source_commit))
    {
        return Err(invalid("source_commit is not a canonical Git object ID"));
    }
    if sync
        .content_digest
        .as_deref()
        .is_some_and(|digest| !valid_source_content_digest(digest))
    {
        return Err(invalid("content_digest is not a typed content digest"));
    }

    match (sync.outcome, sync.finished_at) {
        (None, None) => {
            if sync.failure_code.is_some() {
                return Err(invalid(
                    "non-terminal operation contains a terminal failure code",
                ));
            }
            Ok(false)
        }
        (None, Some(_)) | (Some(_), None) => Err(invalid(
            "outcome and finished_at must either both be present or both be absent",
        )),
        (Some(outcome), Some(finished_at)) => {
            if finished_at < sync.updated_at {
                return Err(invalid("finished_at precedes updated_at"));
            }
            match outcome {
                SourceSyncOutcome::Applied | SourceSyncOutcome::NoChange => {
                    if sync.failure_code.is_some() {
                        return Err(invalid("successful operation contains a failure code"));
                    }
                    if sync.source_commit.is_none() || sync.content_digest.is_none() {
                        return Err(invalid(
                            "successful operation is missing its commit or content digest",
                        ));
                    }
                    Ok(true)
                }
                SourceSyncOutcome::Failed => {
                    if sync.failure_code.is_none() {
                        return Err(invalid("failed operation does not contain a failure code"));
                    }
                    Err(terminal_source_sync_failure(sync, idempotency_key, outcome))
                }
                SourceSyncOutcome::Cancelled => {
                    if sync.failure_code.is_some() {
                        return Err(invalid("cancelled operation contains a failure code"));
                    }
                    Err(terminal_source_sync_failure(sync, idempotency_key, outcome))
                }
            }
        }
    }
}

fn terminal_source_sync_failure(
    sync: &SourceSyncResource,
    idempotency_key: Uuid,
    outcome: SourceSyncOutcome,
) -> CliError {
    CliError::SourceSyncTerminalFailure {
        idempotency_key,
        source_sync_id: sync.source_sync_id,
        outcome: outcome.as_str(),
        failure_code: sync.failure_code,
    }
}

const fn invalid_source_sync(
    idempotency_key: Uuid,
    source_sync_id: SourceSyncId,
    message: &'static str,
) -> CliError {
    CliError::InvalidSourceSyncResponse {
        idempotency_key,
        source_sync_id,
        message,
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
        CommandOutput::Releases(releases) => write_releases(output, releases, json),
        CommandOutput::Release(release) => write_release(output, release, json),
        CommandOutput::ReleaseOperation(operation) => {
            write_release_operation(output, operation, json)
        }
        CommandOutput::SourceStatus(status) => write_source_status(output, *status, json),
        CommandOutput::SourceSync {
            idempotency_key,
            admission,
            sync,
        } => write_source_sync(output, idempotency_key, admission, sync, json),
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

fn write_releases(
    mut output: impl io::Write,
    page: ListReleasesResponse,
    json: bool,
) -> Result<(), CliError> {
    if json {
        serde_json::to_writer(&mut output, &page)?;
        writeln!(output)?;
        return Ok(());
    }
    for release in page.releases {
        writeln!(
            output,
            "{}  {}  version {}  post {}",
            release.publication_id,
            release_state_name(release.state),
            release.version,
            release.post_id
        )?;
    }
    if let Some(cursor) = page.next_cursor {
        writeln!(
            output,
            "Next page: maincopy releases list --cursor {cursor}"
        )?;
    }
    Ok(())
}

fn write_release(
    mut output: impl io::Write,
    release: ReleaseResource,
    json: bool,
) -> Result<(), CliError> {
    if json {
        serde_json::to_writer(&mut output, &release)?;
        writeln!(output)?;
        return Ok(());
    }
    writeln!(output, "Release: {}", release.publication_id)?;
    writeln!(output, "Post: {}", release.post_id)?;
    writeln!(output, "State: {}", release_state_name(release.state))?;
    writeln!(output, "Version: {}", release.version)?;
    writeln!(output, "Revision: {}", release.revision)?;
    writeln!(output, "Preview digest: {}", release.preview_digest)?;
    writeln!(output, "Scheduled for: {}", release.scheduled_for)?;
    if let Some(published_at) = release.published_at {
        writeln!(output, "Published at: {published_at}")?;
    }
    if let Some(reason) = release.block_reason {
        writeln!(output, "Block reason: {}", serde_json::to_value(reason)?)?;
    }
    Ok(())
}

fn write_release_operation(
    mut output: impl io::Write,
    operation: ReleaseOperationResource,
    json: bool,
) -> Result<(), CliError> {
    if json {
        serde_json::to_writer(&mut output, &operation)?;
        writeln!(output)?;
        return Ok(());
    }
    writeln!(output, "Operation: {}", operation.operation_id)?;
    writeln!(output, "Release: {}", operation.publication_id)?;
    writeln!(output, "Accepted version: {}", operation.version)?;
    writeln!(
        output,
        "Accepted state: {}",
        release_state_name(operation.state)
    )?;
    writeln!(
        output,
        "Inspect current state: maincopy releases inspect {}",
        operation.publication_id
    )?;
    Ok(())
}

const fn release_state_name(state: ReleaseState) -> &'static str {
    match state {
        ReleaseState::Scheduled => "scheduled",
        ReleaseState::Activating => "activating",
        ReleaseState::Blocked => "blocked",
        ReleaseState::Published => "published",
        ReleaseState::Superseded => "superseded",
        ReleaseState::Cancelled => "cancelled",
    }
}

fn write_source_status(
    mut output: impl io::Write,
    status: SourceStatusResponse,
    json: bool,
) -> Result<(), CliError> {
    if json {
        serde_json::to_writer(&mut output, &status)?;
        writeln!(output)?;
        return Ok(());
    }

    match status {
        SourceStatusResponse::ExternalCheckout => {
            writeln!(output, "Source mode: external_checkout")?;
        }
        SourceStatusResponse::ManagedGit {
            configuration,
            installed_commit,
            content_digest,
            active_sync,
            latest_sync,
            next_poll_at,
        } => {
            writeln!(output, "Source mode: managed_git")?;
            writeln!(
                output,
                "Remote: {}@{}:{}/{}",
                configuration.remote.user,
                configuration.remote.host,
                configuration.remote.port.get(),
                configuration.remote.repository_path
            )?;
            writeln!(output, "Branch: {}", configuration.branch)?;
            writeln!(
                output,
                "Content subdirectory: {}",
                configuration.content_subdirectory
            )?;
            writeln!(output, "Credential: {}", configuration.credential_name)?;
            writeln!(
                output,
                "Poll interval: {} seconds",
                configuration.poll_interval_seconds.seconds()
            )?;
            writeln!(
                output,
                "Configuration version: {}",
                configuration.version.get()
            )?;
            writeln!(
                output,
                "Configuration updated at: {}",
                configuration.updated_at
            )?;
            write_optional_line(&mut output, "Installed commit", installed_commit.as_deref())?;
            write_optional_line(&mut output, "Content", content_digest.as_deref())?;
            write_optional_line(
                &mut output,
                "Active sync",
                active_sync
                    .as_ref()
                    .map(|sync| sync.source_sync_id.to_string())
                    .as_deref(),
            )?;
            write_optional_line(
                &mut output,
                "Latest sync",
                latest_sync
                    .as_ref()
                    .map(|sync| sync.source_sync_id.to_string())
                    .as_deref(),
            )?;
            writeln!(
                output,
                "Next poll at: {}",
                next_poll_at
                    .map(|timestamp| timestamp.to_string())
                    .as_deref()
                    .unwrap_or("none")
            )?;
        }
    }
    Ok(())
}

fn write_source_sync(
    mut output: impl io::Write,
    idempotency_key: Uuid,
    admission: SourceSyncAdmission,
    sync: SourceSyncResource,
    json: bool,
) -> Result<(), CliError> {
    validate_source_sync(&sync, idempotency_key)?;
    if json {
        #[derive(Serialize)]
        struct SourceSyncOutput<'sync> {
            idempotency_key: Uuid,
            admission: SourceSyncAdmission,
            sync: &'sync SourceSyncResource,
        }

        serde_json::to_writer(
            &mut output,
            &SourceSyncOutput {
                idempotency_key,
                admission,
                sync: &sync,
            },
        )?;
        writeln!(output)?;
        return Ok(());
    }

    writeln!(output, "Source sync: {}", sync.source_sync_id)?;
    writeln!(
        output,
        "Admission: {}",
        source_sync_admission_name(admission)
    )?;
    writeln!(
        output,
        "Status: {}",
        sync.outcome
            .map(SourceSyncOutcome::as_str)
            .unwrap_or_else(|| sync.stage.as_str())
    )?;
    writeln!(
        output,
        "Configuration version: {}",
        sync.configuration_version.get()
    )?;
    writeln!(output, "Requested by: {}", sync.request_origin.as_str())?;
    writeln!(output, "Requested at: {}", sync.requested_at)?;
    writeln!(output, "Updated at: {}", sync.updated_at)?;
    if let Some(finished_at) = sync.finished_at {
        writeln!(output, "Finished at: {finished_at}")?;
    }
    write_optional_line(&mut output, "Source commit", sync.source_commit.as_deref())?;
    write_optional_line(&mut output, "Content", sync.content_digest.as_deref())?;
    if let Some(code) = sync.failure_code {
        writeln!(output, "Failure code: {}", code.as_str())?;
    }
    writeln!(output, "Idempotency key: {idempotency_key}")?;
    Ok(())
}

fn write_optional_line(
    mut output: impl io::Write,
    label: &str,
    value: Option<&str>,
) -> io::Result<()> {
    writeln!(output, "{label}: {}", value.unwrap_or("none"))
}

const fn source_sync_admission_name(admission: SourceSyncAdmission) -> &'static str {
    match admission {
        SourceSyncAdmission::Created => "created",
        SourceSyncAdmission::Coalesced => "coalesced",
        SourceSyncAdmission::Replayed => "replayed",
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
        | CliError::InvalidSourceSyncResponse { .. }
        | CliError::SecretInput(_)
        | CliError::PreviewOutput { .. }
        | CliError::Output(_)
        | CliError::Encode(_) => {
            return INTERNAL;
        }
        CliError::SourceSyncTimedOut { .. } | CliError::SourceSyncTerminalFailure { .. } => {
            return UNAVAILABLE;
        }
        CliError::Admin(_)
        | CliError::Publication { .. }
        | CliError::ReleaseChange { .. }
        | CliError::SourceSyncStart { .. }
        | CliError::SourceSyncFollow { .. } => {}
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
        | AdminClientError::InvalidPublicationResponse { .. }
        | AdminClientError::InvalidSourceSyncResponse { .. } => INTERNAL,
    }
}

fn report_error(error: &CliError, exit: u8, json_output: bool) -> io::Result<()> {
    if json_output {
        return write_error(std::io::stdout().lock(), error, exit, true);
    }

    write_error(std::io::stderr().lock(), error, exit, false)
}

#[derive(Clone, Copy)]
enum ErrorRecovery {
    None,
    Publication(Uuid),
    ReleaseChange {
        publication_id: Uuid,
        operation_id: Uuid,
    },
    SourceSyncStart(Uuid),
    SourceSync {
        idempotency_key: Uuid,
        source_sync_id: SourceSyncId,
        outcome: Option<&'static str>,
        failure_code: Option<SourceSyncFailureCode>,
    },
}

fn write_error(
    output: impl io::Write,
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
    let recovery = error_recovery(error);
    if json_output {
        return write_json_error(output, error, exit, problem, request_id, recovery);
    }

    write_human_error(output, error, problem, request_id, recovery)
}

fn write_human_error(
    mut output: impl io::Write,
    error: &CliError,
    problem: Option<&AdminProblem>,
    request_id: Option<Uuid>,
    recovery: ErrorRecovery,
) -> io::Result<()> {
    writeln!(output, "maincopy: {error}")?;
    if let Some(problem) = problem {
        writeln!(output, "maincopy: {}: {}", problem.code, problem.message)?;
    }
    if let Some(request_id) = request_id {
        writeln!(output, "maincopy: request ID: {request_id}")?;
    }
    match recovery {
        ErrorRecovery::None | ErrorRecovery::Publication(_) => {}
        ErrorRecovery::ReleaseChange { operation_id, .. } => {
            writeln!(
                output,
                "maincopy: recover accepted result: maincopy releases operation {operation_id}"
            )?;
        }
        ErrorRecovery::SourceSyncStart(idempotency_key) => {
            writeln!(output, "maincopy: idempotency key: {idempotency_key}")?;
        }
        ErrorRecovery::SourceSync {
            idempotency_key,
            source_sync_id,
            failure_code,
            ..
        } => {
            writeln!(output, "maincopy: source sync: {source_sync_id}")?;
            writeln!(output, "maincopy: idempotency key: {idempotency_key}")?;
            if let Some(failure_code) = failure_code {
                writeln!(output, "maincopy: failure code: {}", failure_code.as_str())?;
            }
        }
    }
    Ok(())
}

fn write_json_error(
    mut output: impl io::Write,
    error: &CliError,
    exit: u8,
    problem: Option<&AdminProblem>,
    request_id: Option<Uuid>,
    recovery: ErrorRecovery,
) -> io::Result<()> {
    let mut details = serde_json::Map::from_iter([
        ("category".into(), json!(error_category(error, exit))),
        ("message".into(), json!(error.to_string())),
    ]);
    match recovery {
        ErrorRecovery::None => {}
        ErrorRecovery::ReleaseChange {
            publication_id,
            operation_id,
        } => {
            details.insert("publication_id".into(), json!(publication_id));
            details.insert("operation_id".into(), json!(operation_id));
        }
        ErrorRecovery::Publication(idempotency_key)
        | ErrorRecovery::SourceSyncStart(idempotency_key) => {
            details.insert("idempotency_key".into(), json!(idempotency_key));
        }
        ErrorRecovery::SourceSync {
            idempotency_key,
            source_sync_id,
            outcome,
            failure_code,
        } => {
            details.insert("idempotency_key".into(), json!(idempotency_key));
            details.insert("source_sync_id".into(), json!(source_sync_id));
            if let Some(outcome) = outcome {
                details.insert("outcome".into(), json!(outcome));
            }
            if let Some(failure_code) = failure_code {
                details.insert("failure_code".into(), json!(failure_code.as_str()));
            }
        }
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

const fn error_recovery(error: &CliError) -> ErrorRecovery {
    match error {
        CliError::Publication {
            idempotency_key, ..
        } => ErrorRecovery::Publication(*idempotency_key),
        CliError::ReleaseChange {
            publication_id,
            operation_id,
            ..
        } => ErrorRecovery::ReleaseChange {
            publication_id: *publication_id,
            operation_id: *operation_id,
        },
        CliError::SourceSyncStart {
            idempotency_key, ..
        } => ErrorRecovery::SourceSyncStart(*idempotency_key),
        CliError::SourceSyncFollow {
            idempotency_key,
            source_sync_id,
            ..
        }
        | CliError::SourceSyncTimedOut {
            idempotency_key,
            source_sync_id,
        }
        | CliError::InvalidSourceSyncResponse {
            idempotency_key,
            source_sync_id,
            ..
        } => ErrorRecovery::SourceSync {
            idempotency_key: *idempotency_key,
            source_sync_id: *source_sync_id,
            outcome: None,
            failure_code: None,
        },
        CliError::SourceSyncTerminalFailure {
            idempotency_key,
            source_sync_id,
            outcome,
            failure_code,
        } => ErrorRecovery::SourceSync {
            idempotency_key: *idempotency_key,
            source_sync_id: *source_sync_id,
            outcome: Some(outcome),
            failure_code: *failure_code,
        },
        CliError::Admin(_)
        | CliError::SecretInput(_)
        | CliError::PostsSnapshotChanged { .. }
        | CliError::InvalidPostsPagination { .. }
        | CliError::InvalidPublicationResponse { .. }
        | CliError::InvalidPreviewSelector { .. }
        | CliError::PreviewRevisionMismatch { .. }
        | CliError::PreviewContentDigestMismatch { .. }
        | CliError::PreviewOutputExists { .. }
        | CliError::PreviewOutput { .. }
        | CliError::Output(_)
        | CliError::Encode(_) => ErrorRecovery::None,
    }
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
        CliError::Admin(error)
        | CliError::Publication { source: error, .. }
        | CliError::ReleaseChange { source: error, .. }
        | CliError::SourceSyncStart { source: error, .. }
        | CliError::SourceSyncFollow { source: error, .. } => Some(error),
        CliError::PostsSnapshotChanged { .. }
        | CliError::SecretInput(_)
        | CliError::InvalidPostsPagination { .. }
        | CliError::InvalidPublicationResponse { .. }
        | CliError::SourceSyncTimedOut { .. }
        | CliError::SourceSyncTerminalFailure { .. }
        | CliError::InvalidSourceSyncResponse { .. }
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
    use std::{cell::Cell, collections::VecDeque, future::ready};

    use maincopy_shared::FeatureVersions;
    use serde_json::json;

    use super::*;
    use crate::client::AdminProblem;

    const PREVIEW_DIGEST: &str =
        "preview-b3-v1-4444444444444444444444444444444444444444444444444444444444444444";
    const SOURCE_SYNC_ID: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    const SOURCE_COMMIT: &str = "git-sha1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SOURCE_CONTENT_DIGEST: &str =
        "content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333";

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

    fn source_sync_resource(
        stage: &str,
        outcome: Option<SourceSyncOutcome>,
        version: u64,
    ) -> SourceSyncResource {
        let (source_commit, content_digest, failure_code) = match outcome {
            Some(SourceSyncOutcome::Applied) => {
                (Some(SOURCE_COMMIT), Some(SOURCE_CONTENT_DIGEST), None)
            }
            Some(SourceSyncOutcome::NoChange) => {
                (Some(SOURCE_COMMIT), Some(SOURCE_CONTENT_DIGEST), None)
            }
            Some(SourceSyncOutcome::Failed) => (None, None, Some("remote_unavailable")),
            Some(SourceSyncOutcome::Cancelled) | None => (None, None, None),
        };
        serde_json::from_value(json!({
            "source_sync_id": SOURCE_SYNC_ID,
            "configuration_version": 3,
            "request_origin": "manual",
            "stage": stage,
            "outcome": outcome,
            "source_commit": source_commit,
            "content_digest": content_digest,
            "failure_code": failure_code,
            "version": version,
            "requested_at": "2026-09-04T12:00:00Z",
            "updated_at": "2026-09-04T12:00:01Z",
            "finished_at": outcome.map(|_| "2026-09-04T12:00:01Z")
        }))
        .unwrap()
    }

    fn managed_source_status() -> SourceStatusResponse {
        serde_json::from_value(json!({
            "mode": "managed_git",
            "configuration": {
                "remote": {
                    "user": "git",
                    "host": "git.example.test",
                    "port": 22,
                    "repository_path": "publisher/site.git"
                },
                "branch": "main",
                "content_subdirectory": "publication",
                "credential_name": "deploy-key-1",
                "poll_interval_seconds": 300,
                "version": 3,
                "updated_at": "2026-09-04T11:55:00Z"
            },
            "installed_commit": SOURCE_COMMIT,
            "content_digest": SOURCE_CONTENT_DIGEST,
            "active_sync": null,
            "latest_sync": source_sync_resource(
                "reloading",
                Some(SourceSyncOutcome::Applied),
                4
            ),
            "next_poll_at": "2026-09-04T12:05:00Z"
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

    use maincopy_shared::publication::ReleaseBlockReason;

    #[test]
    fn release_failures_preserve_recovery_identifiers_and_exit_categories() {
        let publication_id = Uuid::from_u128(1);
        let operation_id = Uuid::from_u128(2);
        for (status, expected_exit) in [
            (412, CONFLICT),
            (409, CONFLICT),
            (400, VALIDATION),
            (503, UNAVAILABLE),
        ] {
            let error = CliError::ReleaseChange {
                publication_id,
                operation_id,
                source: AdminClientError::HttpStatus {
                    status: reqwest::StatusCode::from_u16(status).unwrap(),
                    problem: None,
                    request_id: None,
                },
            };
            assert_eq!(error_exit(&error), expected_exit);
            let mut output = Vec::new();
            write_error(&mut output, &error, expected_exit, true).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(value["error"]["publication_id"], publication_id.to_string());
            assert_eq!(value["error"]["operation_id"], operation_id.to_string());
            output.clear();
            write_error(&mut output, &error, expected_exit, false).unwrap();
            assert!(
                String::from_utf8(output)
                    .unwrap()
                    .contains(&format!("maincopy releases operation {operation_id}"))
            );
        }
    }

    #[test]
    fn release_output_distinguishes_current_state_from_accepted_receipts() {
        let release = ReleaseResource {
            publication_id: Uuid::from_u128(1),
            post_id: Uuid::from_u128(2),
            preview_digest: PreviewDigest::parse(&format!("preview-b3-v1-{}", "11".repeat(32)))
                .unwrap(),
            revision: format!("post-b3-v1-{}", "22".repeat(32)).into_boxed_str(),
            state: ReleaseState::Blocked,
            version: 3,
            scheduled_for: OffsetDateTime::UNIX_EPOCH,
            published_at: None,
            block_reason: Some(ReleaseBlockReason::RevisionUnavailable),
        };
        let operation = ReleaseOperationResource {
            operation_id: Uuid::from_u128(3),
            publication_id: release.publication_id,
            version: 2,
            state: ReleaseState::Activating,
        };
        let mut output = Vec::new();
        write_release(&mut output, release.clone(), true).unwrap();
        assert_eq!(
            serde_json::from_slice::<ReleaseResource>(&output).unwrap(),
            release
        );
        output.clear();
        write_release(&mut output, release.clone(), false).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("State: blocked\nVersion: 3"));
        assert!(text.contains("revision_unavailable"));
        let mut output = Vec::new();
        write_release_operation(&mut output, operation.clone(), true).unwrap();
        assert_eq!(
            serde_json::from_slice::<ReleaseOperationResource>(&output).unwrap(),
            operation
        );
        output.clear();
        write_release_operation(&mut output, operation, false).unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Accepted version: 2\nAccepted state: activating")
        );
        let page = ListReleasesResponse {
            releases: vec![release],
            next_cursor: Some(Uuid::from_u128(1)),
        };
        let mut output = Vec::new();
        write_releases(&mut output, page.clone(), true).unwrap();
        assert_eq!(
            serde_json::from_slice::<ListReleasesResponse>(&output).unwrap(),
            page
        );
        output.clear();
        write_releases(&mut output, page, false).unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Next page: maincopy releases list --cursor")
        );
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
    fn source_status_has_direct_machine_output_and_bounded_operator_fields() {
        let status = managed_source_status();
        let mut json_output = Vec::new();
        write_source_status(&mut json_output, status.clone(), true).unwrap();
        assert_eq!(
            serde_json::from_slice::<SourceStatusResponse>(&json_output).unwrap(),
            status
        );

        let mut human_output = Vec::new();
        write_source_status(&mut human_output, status, false).unwrap();
        let human_output = String::from_utf8(human_output).unwrap();
        for expected in [
            "Source mode: managed_git\n",
            "Remote: git@git.example.test:22/publisher/site.git\n",
            "Branch: main\n",
            "Content subdirectory: publication\n",
            "Credential: deploy-key-1\n",
            "Poll interval: 300 seconds\n",
            "Configuration version: 3\n",
            "Installed commit: git-sha1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
            "Content: content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333\n",
            "Latest sync: dddddddd-dddd-4ddd-8ddd-dddddddddddd\n",
        ] {
            assert!(human_output.contains(expected), "missing {expected:?}");
        }
        assert!(!human_output.contains("private_key"));

        let mut external = Vec::new();
        write_source_status(&mut external, SourceStatusResponse::ExternalCheckout, false).unwrap();
        assert_eq!(
            String::from_utf8(external).unwrap(),
            "Source mode: external_checkout\n"
        );
    }

    #[test]
    fn asynchronous_source_sync_output_preserves_both_recovery_identities() {
        let idempotency_key = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();
        let queued = source_sync_resource("queued", None, 1);

        let mut json_output = Vec::new();
        write_source_sync(
            &mut json_output,
            idempotency_key,
            SourceSyncAdmission::Created,
            queued.clone(),
            true,
        )
        .unwrap();
        let document = serde_json::from_slice::<serde_json::Value>(&json_output).unwrap();
        assert_eq!(document["idempotency_key"], idempotency_key.to_string());
        assert_eq!(document["admission"], "created");
        assert_eq!(document["sync"]["source_sync_id"], SOURCE_SYNC_ID);
        assert_eq!(document["sync"]["stage"], "queued");

        let mut human_output = Vec::new();
        write_source_sync(
            &mut human_output,
            idempotency_key,
            SourceSyncAdmission::Coalesced,
            queued,
            false,
        )
        .unwrap();
        let human_output = String::from_utf8(human_output).unwrap();
        assert!(human_output.contains("Source sync: dddddddd-dddd-4ddd-8ddd-dddddddddddd\n"));
        assert!(human_output.contains("Admission: coalesced\n"));
        assert!(human_output.contains("Status: queued\n"));
        assert!(human_output.contains("Idempotency key: bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\n"));
    }

    #[tokio::test]
    async fn source_sync_wait_is_bounded_and_stops_on_success() {
        let idempotency_key = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();
        let initial = source_sync_resource("queued", None, 1);
        let fetching = source_sync_resource("fetching", None, 2);
        let applied = source_sync_resource("reloading", Some(SourceSyncOutcome::Applied), 3);
        let mut responses = VecDeque::from([Ok(fetching), Ok(applied.clone())]);
        let polls = Cell::new(0);
        let pauses = Cell::new(0);

        let completed = poll_source_sync(
            initial,
            idempotency_key,
            |source_sync_id| {
                assert_eq!(source_sync_id.to_string(), SOURCE_SYNC_ID);
                polls.set(polls.get() + 1);
                ready(responses.pop_front().unwrap())
            },
            || {
                pauses.set(pauses.get() + 1);
                ready(())
            },
            3,
        )
        .await
        .unwrap();

        assert_eq!(completed, applied);
        assert_eq!(polls.get(), 2);
        assert_eq!(pauses.get(), 2);

        let polls = Cell::new(0);
        let no_change =
            source_sync_resource("resolving_commit", Some(SourceSyncOutcome::NoChange), 2);
        let completed = poll_source_sync(
            no_change.clone(),
            idempotency_key,
            |_| {
                polls.set(polls.get() + 1);
                ready(Ok(no_change.clone()))
            },
            || ready(()),
            3,
        )
        .await
        .unwrap();
        assert_eq!(completed, no_change);
        assert_eq!(polls.get(), 0);
    }

    #[tokio::test]
    async fn source_sync_wait_reports_terminal_failure_and_poll_limit() {
        let idempotency_key = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();
        let initial = source_sync_resource("queued", None, 1);
        let failed = source_sync_resource("fetching", Some(SourceSyncOutcome::Failed), 2);
        let mut responses = VecDeque::from([Ok(failed)]);
        let error = poll_source_sync(
            initial.clone(),
            idempotency_key,
            |_| ready(responses.pop_front().unwrap()),
            || ready(()),
            2,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            CliError::SourceSyncTerminalFailure {
                failure_code: Some(SourceSyncFailureCode::RemoteUnavailable),
                ..
            }
        ));
        assert_eq!(error_exit(&error), UNAVAILABLE);

        let polls = Cell::new(0);
        let error = poll_source_sync(
            initial.clone(),
            idempotency_key,
            |_| {
                polls.set(polls.get() + 1);
                ready(Ok(initial.clone()))
            },
            || ready(()),
            2,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, CliError::SourceSyncTimedOut { .. }));
        assert_eq!(polls.get(), 2);
    }

    #[test]
    fn source_sync_failures_report_safe_details_and_recovery_identities() {
        let idempotency_key = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();
        let failed = source_sync_resource("fetching", Some(SourceSyncOutcome::Failed), 2);
        let error = validate_source_sync(&failed, idempotency_key).unwrap_err();
        let exit = error_exit(&error);

        let mut json_output = Vec::new();
        write_error(&mut json_output, &error, exit, true).unwrap();
        let document = serde_json::from_slice::<serde_json::Value>(&json_output).unwrap();
        assert_eq!(document["error"]["source_sync_id"], SOURCE_SYNC_ID);
        assert_eq!(
            document["error"]["idempotency_key"],
            idempotency_key.to_string()
        );
        assert_eq!(document["error"]["failure_code"], "remote_unavailable");
        assert!(document["error"].get("diagnostic").is_none());

        let mut human_output = Vec::new();
        write_error(&mut human_output, &error, exit, false).unwrap();
        let human_output = String::from_utf8(human_output).unwrap();
        assert!(human_output.contains("source sync: dddddddd-dddd-4ddd-8ddd-dddddddddddd\n"));
        assert!(human_output.contains("idempotency key: bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\n"));
        assert!(human_output.contains("failure code: remote_unavailable\n"));
        assert!(!human_output.contains("diagnostic"));
    }

    #[test]
    fn source_sync_validation_rejects_untrusted_or_incomplete_terminal_metadata() {
        let idempotency_key = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();

        let mut invalid_commit =
            source_sync_resource("reloading", Some(SourceSyncOutcome::Applied), 2);
        invalid_commit.source_commit = Some("not-a-commit\n".into());
        assert!(matches!(
            validate_source_sync(&invalid_commit, idempotency_key),
            Err(CliError::InvalidSourceSyncResponse { .. })
        ));

        let mut missing_digest =
            source_sync_resource("reloading", Some(SourceSyncOutcome::Applied), 2);
        missing_digest.content_digest = None;
        assert!(matches!(
            validate_source_sync(&missing_digest, idempotency_key),
            Err(CliError::InvalidSourceSyncResponse { .. })
        ));

        let mut incomplete_no_change =
            source_sync_resource("resolving_commit", Some(SourceSyncOutcome::NoChange), 2);
        incomplete_no_change.content_digest = None;
        assert!(matches!(
            validate_source_sync(&incomplete_no_change, idempotency_key),
            Err(CliError::InvalidSourceSyncResponse { .. })
        ));

        let mut cancelled = source_sync_resource("fetching", Some(SourceSyncOutcome::Cancelled), 2);
        cancelled.failure_code = Some(SourceSyncFailureCode::Internal);
        assert!(matches!(
            validate_source_sync(&cancelled, idempotency_key),
            Err(CliError::InvalidSourceSyncResponse { .. })
        ));
    }

    #[test]
    fn source_sync_validation_rejects_impossible_lifecycle_metadata() {
        let idempotency_key = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();

        let mut zero_version = source_sync_resource("queued", None, 1);
        zero_version.version = 0;
        assert!(matches!(
            validate_source_sync(&zero_version, idempotency_key),
            Err(CliError::InvalidSourceSyncResponse {
                message: "operation version must be positive",
                ..
            })
        ));

        let mut backwards_update = source_sync_resource("queued", None, 1);
        backwards_update.updated_at = backwards_update.requested_at - time::Duration::SECOND;
        assert!(matches!(
            validate_source_sync(&backwards_update, idempotency_key),
            Err(CliError::InvalidSourceSyncResponse {
                message: "updated_at precedes requested_at",
                ..
            })
        ));

        let mut invalid_digest = source_sync_resource("queued", None, 1);
        invalid_digest.content_digest = Some("content-b3-v1-not-a-digest".into());
        assert!(matches!(
            validate_source_sync(&invalid_digest, idempotency_key),
            Err(CliError::InvalidSourceSyncResponse {
                message: "content_digest is not a typed content digest",
                ..
            })
        ));

        let mut unfinished_with_outcome =
            source_sync_resource("reloading", Some(SourceSyncOutcome::Applied), 2);
        unfinished_with_outcome.finished_at = None;
        assert!(matches!(
            validate_source_sync(&unfinished_with_outcome, idempotency_key),
            Err(CliError::InvalidSourceSyncResponse {
                message: "outcome and finished_at must either both be present or both be absent",
                ..
            })
        ));

        let mut unfinished_with_failure = source_sync_resource("fetching", None, 2);
        unfinished_with_failure.failure_code = Some(SourceSyncFailureCode::Internal);
        assert!(matches!(
            validate_source_sync(&unfinished_with_failure, idempotency_key),
            Err(CliError::InvalidSourceSyncResponse {
                message: "non-terminal operation contains a terminal failure code",
                ..
            })
        ));

        let mut backwards_finish =
            source_sync_resource("reloading", Some(SourceSyncOutcome::Applied), 2);
        backwards_finish.finished_at = Some(backwards_finish.updated_at - time::Duration::SECOND);
        assert!(matches!(
            validate_source_sync(&backwards_finish, idempotency_key),
            Err(CliError::InvalidSourceSyncResponse {
                message: "finished_at precedes updated_at",
                ..
            })
        ));

        let mut successful_with_failure =
            source_sync_resource("reloading", Some(SourceSyncOutcome::Applied), 2);
        successful_with_failure.failure_code = Some(SourceSyncFailureCode::Internal);
        assert!(matches!(
            validate_source_sync(&successful_with_failure, idempotency_key),
            Err(CliError::InvalidSourceSyncResponse {
                message: "successful operation contains a failure code",
                ..
            })
        ));

        let mut failed_without_code =
            source_sync_resource("fetching", Some(SourceSyncOutcome::Failed), 2);
        failed_without_code.failure_code = None;
        assert!(matches!(
            validate_source_sync(&failed_without_code, idempotency_key),
            Err(CliError::InvalidSourceSyncResponse {
                message: "failed operation does not contain a failure code",
                ..
            })
        ));

        let cancelled = source_sync_resource("fetching", Some(SourceSyncOutcome::Cancelled), 2);
        assert!(matches!(
            validate_source_sync(&cancelled, idempotency_key),
            Err(CliError::SourceSyncTerminalFailure {
                outcome: "cancelled",
                failure_code: None,
                ..
            })
        ));
    }

    #[test]
    fn source_sync_errors_preserve_available_recovery_identities() {
        let idempotency_key = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();
        let source_sync_id = SOURCE_SYNC_ID.parse().unwrap();
        let errors = [
            (
                CliError::SourceSyncStart {
                    idempotency_key,
                    source: AdminClientError::InvalidAdminOrigin,
                },
                false,
            ),
            (
                CliError::SourceSyncFollow {
                    idempotency_key,
                    source_sync_id,
                    source: AdminClientError::InvalidAdminOrigin,
                },
                true,
            ),
            (
                CliError::SourceSyncTimedOut {
                    idempotency_key,
                    source_sync_id,
                },
                true,
            ),
            (
                CliError::InvalidSourceSyncResponse {
                    idempotency_key,
                    source_sync_id,
                    message: "operation identity changed while polling",
                },
                true,
            ),
        ];

        for (error, has_source_sync_id) in &errors {
            let exit = error_exit(error);
            let mut json_output = Vec::new();
            write_error(&mut json_output, error, exit, true).unwrap();
            let document = serde_json::from_slice::<serde_json::Value>(&json_output).unwrap();
            assert_eq!(
                document["error"]["idempotency_key"],
                idempotency_key.to_string()
            );
            if *has_source_sync_id {
                assert_eq!(document["error"]["source_sync_id"], SOURCE_SYNC_ID);
            } else {
                assert!(document["error"].get("source_sync_id").is_none());
            }

            let mut human_output = Vec::new();
            write_error(&mut human_output, error, exit, false).unwrap();
            let human_output = String::from_utf8(human_output).unwrap();
            assert!(
                human_output
                    .contains("maincopy: idempotency key: bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\n")
            );
            assert_eq!(
                human_output
                    .contains("maincopy: source sync: dddddddd-dddd-4ddd-8ddd-dddddddddddd\n"),
                *has_source_sync_id
            );
        }
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
