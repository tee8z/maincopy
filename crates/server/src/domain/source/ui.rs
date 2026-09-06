//! Browser administration screen for managed source status and synchronization.

use axum::{
    Form, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::StatusCode,
    middleware,
    response::Response,
    routing::{get, post},
};
use maincopy_shared::{
    auth::{AdminAuditEventId, AdminScope},
    auth_api::SecretString,
    source::{
        GitBranchName, ReconfigureSourceRequest, RepositoryContentSubdirectory,
        SourceConfigurationVersion, SourcePollInterval, SourceStatusResponse,
        SourceSyncFailureCode, SourceSyncId, SourceSyncOutcome, SourceSyncResource,
        SshCredentialName, SshRemote, SshRemoteHost, SshRemotePort, SshRemoteUser,
        SshRepositoryPath,
    },
};
use maud::{Markup, html};
use serde::Deserialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    admin::{
        AdminRuntimeState, AdminSecurityState, BrowserFormSession, RequiredBrowserSession,
        browser_scoped_router,
        request_id::RequestId,
        ui::{self as admin_ui, PageKind},
    },
    database::store::{DatabaseCommandError, DatabaseMutationError},
    domain::auth::store::{AdminMutationKey, AuditPrincipalReference, MutationAuditContext},
    source_sync::{SourceControlError, SourceSyncHandle},
};

const MAX_SOURCE_SYNC_FORM_BYTES: usize = 4 * 1024;
const RECENT_SOURCE_SYNC_LIMIT: usize = 20;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePageQuery {
    sync: Option<Box<str>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceSyncForm {
    #[serde(rename = "_csrf")]
    _csrf: SecretString,
    idempotency_key: Box<str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceConfigurationForm {
    #[serde(rename = "_csrf")]
    csrf: SecretString,
    idempotency_key: Box<str>,
    expected_version: SourceConfigurationVersion,
    user: SshRemoteUser,
    host: SshRemoteHost,
    port: SshRemotePort,
    repository_path: SshRepositoryPath,
    branch: GitBranchName,
    content_subdirectory: RepositoryContentSubdirectory,
    credential_name: SshCredentialName,
    poll_interval_seconds: SourcePollInterval,
}

pub(crate) fn router(security: &AdminSecurityState) -> Router<AdminRuntimeState> {
    let page = browser_scoped_router(
        Router::new().route("/admin/source", get(show_source)),
        security,
        AdminScope::StatusRead,
    );
    let sync = browser_scoped_router(
        Router::new()
            .route("/admin/source/sync", post(begin_source_sync))
            .layer(DefaultBodyLimit::max(MAX_SOURCE_SYNC_FORM_BYTES)),
        security,
        AdminScope::SourceSync,
    );
    let configuration = browser_scoped_router(
        Router::new()
            .route("/admin/source/configuration", post(reconfigure_source))
            .layer(DefaultBodyLimit::max(16 * 1024)),
        security,
        AdminScope::SourceManage,
    );
    page.merge(sync)
        .merge(configuration)
        .layer(middleware::from_fn(admin_ui::adapt_security_response))
}

async fn show_source(
    request_id: RequestId,
    BrowserFormSession {
        session,
        csrf_token,
    }: BrowserFormSession,
    State(handle): State<SourceSyncHandle>,
    query: Result<Query<SourcePageQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_page(request_id),
    };
    let accepted_sync = match query.sync.as_deref() {
        Some(encoded) => match canonical_uuid(encoded) {
            Some(id) => Some(SourceSyncId::from_uuid(id)),
            None => return invalid_page(request_id),
        },
        None => None,
    };
    let (status, recent) = match tokio::try_join!(
        handle.status(),
        handle.list(None, RECENT_SOURCE_SYNC_LIMIT)
    ) {
        Ok(state) => state,
        Err(error) => {
            tracing::error!(%request_id, error = %error, "source administration page load failed");
            return source_unavailable(request_id);
        }
    };
    let accepted_sync = accepted_sync.filter(|accepted| {
        recent
            .syncs
            .iter()
            .any(|sync| sync.source_sync_id == *accepted)
    });

    let is_managed = matches!(status, SourceStatusResponse::ManagedGit { .. });
    let deploy_identity = if is_managed && session.scopes().contains(&AdminScope::SourceManage) {
        Some(handle.deploy_public_identity().await)
    } else {
        None
    };
    let can_sync = is_managed && session.scopes().contains(&AdminScope::SourceSync);
    let csrf = csrf_token.expose_secret();
    let idempotency_key = Uuid::new_v4();

    admin_ui::page_response(
        StatusCode::OK,
        "Source",
        PageKind::Authenticated,
        html! {
            div class="row" {
                div {
                    h1 { "Content source" }
                    p class="muted" { "Inspect the candidate source without publishing it." }
                }
                div class="actions" {
                    a class="button" href="/admin" { "Posts" }
                    form method="post" action="/admin/logout" {
                        input type="hidden" name="_csrf" value=(csrf);
                        button type="submit" { "Sign out" }
                    }
                }
            }
            @if let Some(sync_id) = accepted_sync {
                section class="notice" role="status" {
                    strong { "Synchronization request accepted." }
                    " Operation " code { (sync_id) } " can be recovered from the recent history below."
                }
            }
            (source_status_panel(&status))
            @if let Some(identity) = &deploy_identity {
                section class="panel" {
                    h2 { "Selected deploy public key" }
                    @match identity {
                        Ok(identity) => {
                            p { "Credential: " code { (identity.credential_name.as_str()) } }
                            p { "Public key: " code { (&identity.public_key) } }
                            p { "Fingerprint: " code { (&identity.fingerprint) } }
                        }
                        Err(_) => { p class="notice" { "The selected deploy public key is unavailable. Check the protected credential on the host." } }
                    }
                }
            }
            @if session.scopes().contains(&AdminScope::SourceManage) {
                (source_configuration_form(&status, csrf, session.is_active_at(OffsetDateTime::now_utc()) && OffsetDateTime::now_utc() < session.fresh_until))
            }
            @if can_sync {
                section class="panel" aria-labelledby="sync-now" {
                    h2 id="sync-now" { "Sync now" }
                    p {
                        "Fetch the configured branch and prepare a new immutable candidate. "
                        "This does not publish any post."
                    }
                    form method="post" action="/admin/source/sync" {
                        input type="hidden" name="_csrf" value=(csrf);
                        input type="hidden" name="idempotency_key" value=(idempotency_key);
                        button type="submit" { "Start synchronization" }
                    }
                }
            } @else if is_managed {
                section class="panel" {
                    h2 { "Sync now" }
                    p class="muted" { "Your current role can inspect source state but cannot start a synchronization." }
                }
            }
            section class="panel" aria-labelledby="recent-syncs" {
                h2 id="recent-syncs" { "Recent synchronizations" }
                @if recent.syncs.is_empty() {
                    p class="muted" { "No synchronization operations have been recorded." }
                } @else {
                    @for sync in &recent.syncs {
                        (source_sync_panel(sync))
                    }
                }
            }
        },
    )
}

async fn begin_source_sync(
    RequiredBrowserSession {
        request_id,
        session,
        ..
    }: RequiredBrowserSession,
    State(handle): State<SourceSyncHandle>,
    form: Result<Form<SourceSyncForm>, axum::extract::rejection::FormRejection>,
) -> Response {
    let form = match form {
        Ok(Form(form)) => form,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return admin_ui::error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Synchronization not started",
                "The synchronization confirmation was too large. Return to source status and try again.",
                request_id,
            );
        }
        Err(_) => {
            return admin_ui::error_response(
                StatusCode::BAD_REQUEST,
                "Synchronization not started",
                "The synchronization confirmation was not valid. Return to source status and try again.",
                request_id,
            );
        }
    };
    if form._csrf.expose_secret().is_empty() {
        return admin_ui::error_response(
            StatusCode::BAD_REQUEST,
            "Synchronization not started",
            "The synchronization confirmation was not valid. Return to source status and try again.",
            request_id,
        );
    }
    let Some(idempotency_key) = canonical_uuid(&form.idempotency_key) else {
        return admin_ui::error_response(
            StatusCode::BAD_REQUEST,
            "Synchronization not started",
            "The synchronization retry identity was not valid. Return to source status and try again.",
            request_id,
        );
    };
    let audit = MutationAuditContext {
        audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
        principal: AuditPrincipalReference::BrowserSession {
            user_id: session.user_id,
            session_id: session.session_id,
        },
        request_id: Some(request_id.0),
        idempotency_key: AdminMutationKey(idempotency_key),
    };
    match handle.begin_manual(audit).await {
        Ok(accepted) => admin_ui::redirect(&format!(
            "/admin/source?sync={}",
            accepted.sync.source_sync_id
        )),
        Err(error) => source_mutation_error(error, request_id),
    }
}

async fn reconfigure_source(
    RequiredBrowserSession {
        request_id,
        session,
        ..
    }: RequiredBrowserSession,
    State(handle): State<SourceSyncHandle>,
    form: Result<Form<SourceConfigurationForm>, axum::extract::rejection::FormRejection>,
) -> Response {
    if !(session.is_active_at(OffsetDateTime::now_utc())
        && OffsetDateTime::now_utc() < session.fresh_until)
    {
        return admin_ui::error_response(
            StatusCode::FORBIDDEN,
            "Source settings unchanged",
            "Source changes require a recent Owner sign-in. Sign in again, then reload source status.",
            request_id,
        );
    }
    let form = match form {
        Ok(Form(form)) if !form.csrf.expose_secret().is_empty() => form,
        _ => {
            return admin_ui::error_response(
                StatusCode::BAD_REQUEST,
                "Source settings unchanged",
                "The proposed source settings were invalid. Reload source status and try again.",
                request_id,
            );
        }
    };
    let Some(key) = canonical_uuid(&form.idempotency_key) else {
        return admin_ui::error_response(
            StatusCode::BAD_REQUEST,
            "Source settings unchanged",
            "The operation identity was invalid. Reload source status and try again.",
            request_id,
        );
    };
    let request = ReconfigureSourceRequest {
        remote: SshRemote {
            user: form.user,
            host: form.host,
            port: form.port,
            repository_path: form.repository_path,
        },
        branch: form.branch,
        content_subdirectory: form.content_subdirectory,
        credential_name: form.credential_name,
        poll_interval_seconds: form.poll_interval_seconds,
        expected_version: form.expected_version,
    };
    let audit = MutationAuditContext {
        audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
        principal: AuditPrincipalReference::BrowserSession {
            user_id: session.user_id,
            session_id: session.session_id,
        },
        request_id: Some(request_id.0),
        idempotency_key: AdminMutationKey(key),
    };
    match handle.reconfigure(request, audit).await {
        Ok(accepted) => admin_ui::redirect(&format!(
            "/admin/source?sync={}",
            accepted.sync.source_sync_id
        )),
        Err(error) => source_mutation_error(error, request_id),
    }
}

fn source_configuration_form(status: &SourceStatusResponse, csrf: &str, fresh: bool) -> Markup {
    let SourceStatusResponse::ManagedGit {
        configuration,
        active_sync,
        ..
    } = status
    else {
        return html! {};
    };
    let remote = &configuration.remote;
    html! {
        section class="panel" aria-labelledby="source-configuration" {
            h2 id="source-configuration" { "Source settings" }
            p { "Maincopy validates and compiles the proposed source before installing these settings. A failed change preserves the installed settings and public site." }
            @if !fresh { p class="notice" { "Source changes require a recent sign-in. " a href="/admin/login" { "Sign in again" } } }
            @if active_sync.is_some() { p class="notice" { "Wait for the active synchronization to finish, then reload this page before changing settings." } }
            form method="post" action="/admin/source/configuration" {
                input type="hidden" name="_csrf" value=(csrf);
                input type="hidden" name="idempotency_key" value=(Uuid::new_v4());
                input type="hidden" name="expected_version" value=(configuration.version.get());
                fieldset disabled[!fresh || active_sync.is_some()] {
                    label for="source-user" { "SSH user" }
                    input id="source-user" name="user" value=(remote.user.as_str()) maxlength="64" required;
                    label for="source-host" { "SSH host" }
                    input id="source-host" name="host" value=(remote.host.as_str()) maxlength="253" required;
                    label for="source-port" { "SSH port" }
                    input id="source-port" name="port" type="number" value=(remote.port.get()) min="1" max="65535" required;
                    label for="source-repository" { "Repository path" }
                    input id="source-repository" name="repository_path" value=(remote.repository_path.as_str()) maxlength="1024" required;
                    label for="source-branch" { "Branch" }
                    input id="source-branch" name="branch" value=(configuration.branch.as_str()) maxlength="255" required;
                    label for="source-subdirectory" { "Content subdirectory" }
                    input id="source-subdirectory" name="content_subdirectory" value=(configuration.content_subdirectory.as_str()) maxlength="1024" required;
                    label for="source-credential" { "Registered deploy credential" }
                    input id="source-credential" name="credential_name" value=(configuration.credential_name.as_str()) maxlength="64" required;
                    label for="source-poll" { "Poll interval in seconds" }
                    input id="source-poll" name="poll_interval_seconds" type="number" value=(configuration.poll_interval_seconds.seconds()) min="30" max="86400" required;
                    button type="submit" { "Validate and apply settings" }
                }
            }
        }
    }
}

fn source_status_panel(status: &SourceStatusResponse) -> Markup {
    match status {
        SourceStatusResponse::ExternalCheckout => html! {
            section class="panel" aria-labelledby="source-mode" {
                h2 id="source-mode" { "External checkout" }
                p {
                    "Maincopy reads the service-managed content directory. Git synchronization is managed outside this process."
                }
                p class="muted" { "The Sync now control is intentionally unavailable in this mode." }
            }
        },
        SourceStatusResponse::ManagedGit {
            configuration,
            installed_commit,
            content_digest,
            active_sync,
            latest_sync,
            next_poll_at,
        } => {
            let remote = &configuration.remote;
            html! {
                section class="panel" aria-labelledby="source-mode" {
                    h2 id="source-mode" { "Managed Git" }
                    p {
                        "Maincopy checks this branch in the background. Pushes can become private previews without restarting the service."
                    }
                    dl {
                        dt { "Remote" }
                        dd { code { (remote.user) "@" (remote.host) ":" (remote.port.get()) "/" (remote.repository_path) } }
                        dt { "Branch" }
                        dd { code { (configuration.branch) } }
                        dt { "Content directory" }
                        dd { code { (configuration.content_subdirectory) } }
                        dt { "Credential reference" }
                        dd { code { (configuration.credential_name) } }
                        dt { "Poll interval" }
                        dd { (configuration.poll_interval_seconds.seconds()) " seconds" }
                        dt { "Configuration version" }
                        dd { (configuration.version.get()) }
                        dt { "Installed commit" }
                        dd { (optional_code(installed_commit.as_deref(), "No commit installed")) }
                        dt { "Candidate digest" }
                        dd { (optional_code(content_digest.as_deref(), "No candidate installed")) }
                        dt { "Next poll" }
                        dd { (optional_time(*next_poll_at, "Not scheduled")) }
                    }
                }
                @if let Some(active) = active_sync {
                    section class="notice" role="status" {
                        strong { "Synchronization in progress: " }
                        code { (active.source_sync_id) }
                        " — " (sync_state(active))
                    }
                } @else if let Some(latest) = latest_sync {
                    @if latest.outcome == Some(SourceSyncOutcome::Failed) {
                        section class="error" role="alert" {
                            strong { "Latest synchronization failed." }
                            " " (recovery_guidance(latest.failure_code))
                        }
                    }
                }
            }
        }
    }
}

fn source_sync_panel(sync: &SourceSyncResource) -> Markup {
    html! {
        article aria-label=(format!("Source synchronization {}", sync.source_sync_id)) {
            h3 { code { (sync.source_sync_id) } }
            dl {
                dt { "State" }
                dd { (sync_state(sync)) }
                dt { "Requested by" }
                dd { (sync.request_origin.as_str()) }
                dt { "Requested" }
                dd { (timestamp(sync.requested_at)) }
                dt { "Updated" }
                dd { (timestamp(sync.updated_at)) }
                @if let Some(commit) = sync.source_commit.as_deref() {
                    dt { "Commit" }
                    dd { code { (commit) } }
                }
                @if let Some(digest) = sync.content_digest.as_deref() {
                    dt { "Candidate digest" }
                    dd { code { (digest) } }
                }
                @if let Some(code) = sync.failure_code {
                    dt { "Failure" }
                    dd { code { (code.as_str()) } }
                    dt { "Recovery" }
                    dd { (recovery_guidance(Some(code))) }
                }
            }
        }
    }
}

fn sync_state(sync: &SourceSyncResource) -> &'static str {
    match sync.outcome {
        Some(SourceSyncOutcome::Applied) => "applied",
        Some(SourceSyncOutcome::NoChange) => "no change",
        Some(SourceSyncOutcome::Failed) => "failed",
        Some(SourceSyncOutcome::Cancelled) => "cancelled",
        None => sync.stage.as_str(),
    }
}

fn recovery_guidance(code: Option<SourceSyncFailureCode>) -> &'static str {
    match code {
        Some(SourceSyncFailureCode::CredentialUnavailable) => {
            "Verify that the configured credential reference is installed and readable, then retry."
        }
        Some(SourceSyncFailureCode::UnknownHost) => {
            "Verify the configured SSH host key in the service known-hosts file, then retry."
        }
        Some(SourceSyncFailureCode::AuthenticationFailed) => {
            "Verify the deploy key has read access to the repository, then retry."
        }
        Some(
            SourceSyncFailureCode::RemoteUnavailable
            | SourceSyncFailureCode::FetchFailed
            | SourceSyncFailureCode::TimedOut,
        ) => "Verify repository connectivity and retry. The installed candidate remains active.",
        Some(SourceSyncFailureCode::BranchUnavailable) => {
            "Verify that the configured branch exists and the deploy key can read it, then retry."
        }
        Some(SourceSyncFailureCode::CommitInvalid) => {
            "Verify the remote branch resolves to a complete commit, then retry."
        }
        Some(SourceSyncFailureCode::CandidateFailed) => {
            "Inspect service logs using the operation ID and check candidate-store integrity and capacity. The installed candidate remains active."
        }
        Some(SourceSyncFailureCode::ValidationFailed | SourceSyncFailureCode::CompileFailed) => {
            "Correct the content at a new commit and retry. The installed candidate remains active."
        }
        Some(SourceSyncFailureCode::ConfigurationChanged) => {
            "Refresh this page and retry against the current source configuration."
        }
        Some(SourceSyncFailureCode::ReloadFailed | SourceSyncFailureCode::Internal) => {
            "Inspect service logs using the operation ID, correct the cause, and retry."
        }
        Some(SourceSyncFailureCode::Interrupted) => {
            "Retry the synchronization. The installed candidate remains active."
        }
        None => "Inspect service logs using the operation ID, correct the cause, and retry.",
    }
}

fn optional_code(value: Option<&str>, absent: &'static str) -> Markup {
    value.map_or_else(
        || html! { span class="muted" { (absent) } },
        |value| html! { code { (value) } },
    )
}

fn optional_time(value: Option<OffsetDateTime>, absent: &'static str) -> Markup {
    value.map_or_else(
        || html! { span class="muted" { (absent) } },
        |value| html! { (timestamp(value)) },
    )
}

fn timestamp(value: OffsetDateTime) -> String {
    value
        .format(&Rfc3339)
        .unwrap_or_else(|_| "timestamp unavailable".to_owned())
}

fn canonical_uuid(encoded: &str) -> Option<Uuid> {
    let uuid = Uuid::parse_str(encoded).ok()?;
    (uuid.hyphenated().to_string() == encoded).then_some(uuid)
}

fn source_mutation_error(error: SourceControlError, request_id: RequestId) -> Response {
    let (status, title, message) = match error {
        SourceControlError::Unsupported => (
            StatusCode::CONFLICT,
            "Synchronization not available",
            "This server uses an external checkout. Synchronize that checkout through its service-managed workflow.",
        ),
        SourceControlError::ShuttingDown => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Synchronization not started",
            "Maincopy is shutting down. Start the service before requesting another synchronization.",
        ),
        SourceControlError::Mutation(DatabaseMutationError::Command(
            DatabaseCommandError::IdempotencyConflict | DatabaseCommandError::Rejected,
        )) => (
            StatusCode::CONFLICT,
            "Synchronization not started",
            "The request conflicts with current source state. Refresh source status and try again.",
        ),
        SourceControlError::Mutation(DatabaseMutationError::Command(
            DatabaseCommandError::InvalidValue,
        )) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Synchronization not started",
            "The synchronization request could not be represented safely.",
        ),
        SourceControlError::ConfigurationUnavailable
        | SourceControlError::CredentialUnavailable
        | SourceControlError::Load(_)
        | SourceControlError::Mutation(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Synchronization not started",
            "Source synchronization is temporarily unavailable. Wait a moment, refresh source status, and try again.",
        ),
    };
    if status.is_server_error() {
        tracing::error!(%request_id, error = %error, "source administration mutation failed");
    }
    admin_ui::error_response(status, title, message, request_id)
}

fn invalid_page(request_id: RequestId) -> Response {
    admin_ui::error_response(
        StatusCode::BAD_REQUEST,
        "Invalid source page request",
        "The source page query was not valid. Return to source status and try again.",
        request_id,
    )
}

fn source_unavailable(request_id: RequestId) -> Response {
    admin_ui::error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "Source status unavailable",
        "Source synchronization state is temporarily unavailable. Wait a moment and refresh this page.",
        request_id,
    )
}

#[cfg(test)]
mod tests {
    use maincopy_shared::source::{
        SourceConfigurationVersion, SourceSyncRequestOrigin, SourceSyncStage,
    };

    use super::*;

    #[test]
    fn operation_markup_uses_safe_failure_codes() {
        let sync = SourceSyncResource {
            source_sync_id: SourceSyncId::from_uuid(Uuid::new_v4()),
            configuration_version: SourceConfigurationVersion::new(1).unwrap(),
            request_origin: SourceSyncRequestOrigin::Manual,
            stage: SourceSyncStage::Fetching,
            outcome: Some(SourceSyncOutcome::Failed),
            source_commit: None,
            content_digest: None,
            failure_code: Some(SourceSyncFailureCode::CredentialUnavailable),
            version: 2,
            requested_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            finished_at: Some(OffsetDateTime::UNIX_EPOCH),
        };

        let rendered = source_sync_panel(&sync).into_string();
        assert!(rendered.contains("credential_unavailable"));
        assert!(rendered.contains("configured credential reference"));
    }

    #[test]
    fn every_failure_code_has_operator_recovery_guidance() {
        for code in [
            SourceSyncFailureCode::ConfigurationChanged,
            SourceSyncFailureCode::CredentialUnavailable,
            SourceSyncFailureCode::UnknownHost,
            SourceSyncFailureCode::AuthenticationFailed,
            SourceSyncFailureCode::RemoteUnavailable,
            SourceSyncFailureCode::BranchUnavailable,
            SourceSyncFailureCode::FetchFailed,
            SourceSyncFailureCode::CommitInvalid,
            SourceSyncFailureCode::CandidateFailed,
            SourceSyncFailureCode::ValidationFailed,
            SourceSyncFailureCode::CompileFailed,
            SourceSyncFailureCode::ReloadFailed,
            SourceSyncFailureCode::TimedOut,
            SourceSyncFailureCode::Interrupted,
            SourceSyncFailureCode::Internal,
        ] {
            assert!(!recovery_guidance(Some(code)).is_empty(), "{code:?}");
        }
    }

    #[test]
    fn external_checkout_explains_why_managed_sync_is_absent() {
        let rendered = source_status_panel(&SourceStatusResponse::ExternalCheckout).into_string();
        assert!(rendered.contains("External checkout"));
        assert!(rendered.contains("Sync now control is intentionally unavailable"));
    }
}
