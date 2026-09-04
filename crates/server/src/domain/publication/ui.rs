use axum::{
    Form, Router,
    extract::{DefaultBodyLimit, FromRequestParts, Path, Query},
    http::{HeaderMap, StatusCode, request::Parts},
    middleware,
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use maincopy_shared::{auth::AdminScope, auth_api::SecretString};
use markdown_compiler::{
    ContentTreeDigest, DraftStatus, PostId, PostRevisionDigest, PreviewDigest, SiteSnapshotDigest,
};
use maud::html;
use serde::Deserialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    admin::{
        AdminSecurityState, BrowserFormSession, RequiredBrowserSession, browser_scoped_router,
        request_id::RequestId,
        ui::{self as admin_ui, PageKind},
    },
    render::render_bound_post_preview,
};

use super::{
    ActivationBlockReason, CanonicalState, PublicPagePath, PublishedPostRevision,
    activation::{
        PublicationActivationError, PublicationCoordinatorHandle, PublishNow, PublishReviewedNow,
        ReleaseTransitionError, RetryRelease, ReviewedPublicRevision, Schedule, ScheduleReviewed,
        ScheduledApprovalOutcome,
    },
    admin::activation_error,
    store::{
        ChangeRelease, ReleaseChange, ReleaseCommandError, ReleaseMutationError,
        SchedulePublicationLookupError, SiteHead,
    },
    web::application_asset_response,
};

const MAX_OVERVIEW_POSTS: usize = 100;
const MAX_PUBLICATION_FORM_BYTES: usize = 4 * 1024;
const PREVIEW_ASSETS_PATH: &str = "/api/admin/v1/preview-assets";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationState {
    Draft,
    Unpublished,
    Published,
    UnpublishedChanges,
}

impl PublicationState {
    const fn label(self) -> &'static str {
        match self {
            Self::Draft => "Draft",
            Self::Unpublished => "Not published",
            Self::Published => "Published",
            Self::UnpublishedChanges => "Unpublished changes",
        }
    }

    const fn can_publish(self) -> bool {
        matches!(self, Self::Unpublished | Self::UnpublishedChanges)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverviewQuery {
    published: Option<Box<str>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPublicationApproval {
    #[serde(rename = "_csrf")]
    _csrf: SecretString,
    content_digest: Box<str>,
    revision: Box<str>,
    preview_digest: Box<str>,
    expected_site_digest: Box<str>,
    expected_site_version: Box<str>,
    expected_public_revision: Box<str>,
    idempotency_key: Box<str>,
    accept_preview: Box<str>,
    #[serde(default)]
    scheduled_at: Box<str>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReviewConfirmation {
    content_digest: Box<str>,
    revision: Box<str>,
    preview_digest: Box<str>,
    expected_site_digest: Box<str>,
    expected_site_version: Box<str>,
    expected_public_revision: Box<str>,
}

enum PublicationTiming {
    Now,
    Scheduled(OffsetDateTime),
}

struct PublicationApproval {
    timing: PublicationTiming,
    review: ReviewBinding,
    idempotency_key: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewBinding {
    content_digest: ContentTreeDigest,
    revision: PostRevisionDigest,
    preview_digest: PreviewDigest,
    expected_site: SiteHead,
    expected_public_revision: ReviewedPublicRevision,
}

struct UiPublication(PublicationCoordinatorHandle);

impl<S> FromRequestParts<S> for UiPublication
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = RequestId::from_request_parts(parts, state)
            .await
            .map_err(|rejection| rejection.into_response())?;
        parts
            .extensions
            .get::<PublicationCoordinatorHandle>()
            .cloned()
            .map(Self)
            .ok_or_else(|| {
                admin_ui::error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Publication unavailable",
                    "Publication state is temporarily unavailable.",
                    request_id,
                )
            })
    }
}

pub(crate) fn router(security_state: &AdminSecurityState) -> Router {
    let overview = browser_scoped_router(
        Router::new().route("/admin", get(show_overview)),
        security_state,
        AdminScope::ContentRead,
    );
    let review = browser_scoped_router(
        Router::new().route("/admin/posts/{post_id}/review", get(show_review)),
        security_state,
        AdminScope::PreviewRead,
    );
    let confirmation = browser_scoped_router(
        Router::new()
            .route("/admin/posts/{post_id}/confirm", get(show_confirmation))
            .route("/admin/releases/{release_id}", get(show_release))
            .route("/admin/releases", get(show_releases)),
        security_state,
        AdminScope::ReleaseManage,
    );
    let publication = browser_scoped_router(
        Router::new()
            .route("/admin/posts/{post_id}/publish", post(publish))
            .route("/admin/releases/{release_id}", post(change_release))
            .layer(DefaultBodyLimit::max(MAX_PUBLICATION_FORM_BYTES)),
        security_state,
        AdminScope::ReleaseManage,
    );
    let application_assets = browser_scoped_router(
        Router::new().route("/app-assets/{digest}/{name}", get(get_application_asset)),
        security_state,
        AdminScope::PreviewRead,
    );

    overview
        .merge(review)
        .merge(confirmation)
        .merge(publication)
        .merge(application_assets)
        .layer(middleware::from_fn(admin_ui::adapt_security_response))
}

async fn show_overview(
    request_id: RequestId,
    BrowserFormSession {
        csrf_token,
        session,
    }: BrowserFormSession,
    UiPublication(coordinator): UiPublication,
    query: Result<Query<OverviewQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => {
            return admin_ui::error_response(
                StatusCode::BAD_REQUEST,
                "Invalid page request",
                "The administration page query was not valid.",
                request_id,
            );
        }
    };
    let published_request = match query.published.as_deref() {
        Some(value) => match PostId::parse(value) {
            Ok(post_id) => Some(post_id),
            Err(_) => {
                return admin_ui::error_response(
                    StatusCode::BAD_REQUEST,
                    "Invalid page request",
                    "The publication confirmation identifier was not valid.",
                    request_id,
                );
            }
        },
        None => None,
    };

    let projection = coordinator.read();
    let published =
        published_request.filter(|post_id| projection.ledger.published_post(post_id).is_some());
    let effective_scopes = session.scopes();
    let can_preview = effective_scopes.contains(&AdminScope::PreviewRead);
    let roles = session
        .roles
        .iter()
        .map(|role| role.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let scopes = effective_scopes
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let post_count = projection.catalog.rendered_posts().len();
    let posts = projection
        .catalog
        .rendered_posts()
        .take(MAX_OVERVIEW_POSTS)
        .map(|post| {
            let public = projection.ledger.published_post(&post.document.metadata.id);
            let state = publication_state(post.document.metadata.draft, &post.revision, public);
            (post, public, state)
        })
        .collect::<Vec<_>>();
    let csrf = csrf_token.expose_secret();

    admin_ui::page_response(
        StatusCode::OK,
        "Posts",
        PageKind::Authenticated,
        html! {
            div class="row" {
                h1 { "Posts" }
                @if effective_scopes.contains(&AdminScope::ReleaseManage) {
                    a class="button" href="/admin/releases" { "Releases" }
                }
                form method="post" action="/admin/logout" {
                    input type="hidden" name="_csrf" value=(csrf);
                    button type="submit" { "Sign out" }
                }
            }
            @if let Some(post_id) = published {
                section class="notice" role="status" {
                    strong { "Publication completed." }
                    " The public snapshot now includes post " code { (post_id) } "."
                }
            }
            section class="panel" aria-labelledby="loaded-state" {
                h2 id="loaded-state" { "Loaded state" }
                dl {
                    dt { "Signed-in user" }
                    dd { code { (session.user_id) } }
                    dt { "Roles" }
                    dd { (roles) }
                    dt { "Scopes" }
                    dd { (scopes) }
                    dt { "Candidate content" }
                    dd { code { (projection.content_digest) } }
                    dt { "Public site snapshot" }
                    dd { code { (projection.site.digest.as_str()) } " (version " (projection.site.version) ")" }
                }
            }
            @if post_count > MAX_OVERVIEW_POSTS {
                p class="muted" {
                    "Showing the first " (MAX_OVERVIEW_POSTS) " of " (post_count)
                    " posts. Use the administration API for complete pagination."
                }
            }
            @for (post, public, state) in posts {
                article class="panel" {
                    div class="row" {
                        div {
                            h2 { (post.document.metadata.title.as_str()) }
                            p class="muted" { (state.label()) }
                        }
                        @if can_preview && post.document.metadata.draft == DraftStatus::Publishable {
                            a class="button" href=(format!(
                                "/admin/posts/{}/review",
                                post.document.metadata.id
                            )) { "Review exact preview" }
                        }
                    }
                    dl {
                        dt { "Candidate revision" }
                        dd { code { (post.revision.as_str()) } }
                        dt { "Current public revision" }
                        dd {
                            @if let Some(public) = public {
                                code { (public.revision.as_str()) }
                            } @else {
                                "Not published"
                            }
                        }
                        dt { "Canonical path" }
                        dd { code { "/posts/" (post.document.metadata.slug.as_str()) } }
                    }
                }
            }
        },
    )
}

async fn show_review(
    request_id: RequestId,
    RequiredBrowserSession { session, .. }: RequiredBrowserSession,
    UiPublication(coordinator): UiPublication,
    Path(encoded_post_id): Path<String>,
) -> Response {
    let post_id = match PostId::parse(&encoded_post_id) {
        Ok(post_id) => post_id,
        Err(_) => return invalid_post_id(request_id),
    };
    let projection = coordinator.read();
    let Some(post) = projection.catalog.current_post(&post_id) else {
        return admin_ui::error_response(
            StatusCode::NOT_FOUND,
            "Post not found",
            "The selected post is not present in the loaded content.",
            request_id,
        );
    };
    if post.document.metadata.draft == DraftStatus::Draft {
        return admin_ui::error_response(
            StatusCode::CONFLICT,
            "Draft cannot be published",
            "Mark this post publishable in its Markdown metadata, reload it, and review it again.",
            request_id,
        );
    }

    let public = projection.ledger.published_post(&post_id);
    let state = publication_state(post.document.metadata.draft, &post.revision, public);
    let can_release = session.scopes().contains(&AdminScope::ReleaseManage);
    let preview_asset_endpoint = format!("{PREVIEW_ASSETS_PATH}/{}", projection.content_digest);
    let preview = match render_bound_post_preview(
        &projection.catalog,
        projection.frontend,
        &post_id,
        projection.tip_recipient.as_ref(),
        &preview_asset_endpoint,
        public.map(|entry| entry.published_at),
    ) {
        Ok(Some(preview)) => preview,
        Ok(None) => {
            return admin_ui::error_response(
                StatusCode::NOT_FOUND,
                "Post not found",
                "The selected post is not present in the loaded content.",
                request_id,
            );
        }
        Err(error) => {
            tracing::error!(%request_id, %post_id, error = %error, "admin review preview render failed");
            return admin_ui::error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Preview unavailable",
                "The selected preview could not be rendered safely.",
                request_id,
            );
        }
    };
    let preview_path = format!(
        "/api/admin/v1/posts/{post_id}/preview?revision={}&content_digest={}",
        preview.revision.as_str(),
        projection.content_digest
    );
    let binding = ReviewBinding {
        content_digest: projection.content_digest.clone(),
        revision: preview.revision.clone(),
        preview_digest: preview.digest.clone(),
        expected_site: projection.site.clone(),
        expected_public_revision: reviewed_public_revision(public),
    };
    let confirmation_path = confirmation_path(&post_id, &binding);
    let canonical_path = PublicPagePath::post(&post.document.metadata.slug);
    let current_revision = public.map(|entry| entry.revision.as_str());

    admin_ui::page_response(
        StatusCode::OK,
        "Review publication",
        PageKind::Authenticated,
        html! {
            h1 { "Review “" (post.document.metadata.title.as_str()) "”" }
            p { (state.label()) }
            section class="panel" aria-labelledby="revision-details" {
                h2 id="revision-details" { "Exact candidate" }
                dl {
                    dt { "Signed-in user" }
                    dd { code { (session.user_id) } }
                    dt { "Current public revision" }
                    dd {
                        @if let Some(revision) = current_revision {
                            code { (revision) }
                        } @else {
                            "Not published"
                        }
                    }
                    dt { "Candidate revision" }
                    dd { code { (preview.revision.as_str()) } }
                    dt { "Candidate content" }
                    dd { code { (projection.content_digest) } }
                    dt { "Preview digest" }
                    dd { code { (preview.digest.as_str()) } }
                    dt { "Canonical URL" }
                    dd { a href=(preview.canonical_url.as_str()) { (preview.canonical_url.as_str()) } }
                    dt { "Canonical path" }
                    dd { code { (canonical_path.as_str()) } }
                }
            }
            section class="panel" aria-labelledby="rendered-preview" {
                h2 id="rendered-preview" { "Exact rendered preview" }
                iframe class="preview-frame" title="Exact rendered article preview"
                    src=(preview_path.clone()) {}
                nav class="actions" aria-label="Preview actions" {
                    a class="button" href=(preview_path) target="_blank" rel="noopener" {
                        "Open exact rendered preview"
                    }
                }
            }
            @if state.can_publish() && can_release {
                section class="panel" aria-labelledby="continue-heading" {
                    h2 id="continue-heading" { "Continue after review" }
                    p {
                        "After inspecting the complete preview, continue to a separate publication "
                        "confirmation bound to this candidate and public site head."
                    }
                    a class="button" href=(confirmation_path) {
                        "Continue to publication confirmation"
                    }
                }
            } @else if state.can_publish() {
                section class="error" {
                    strong { "Publication permission is required." }
                    " Your account can inspect this preview but cannot publish it."
                }
            } @else {
                section class="notice" {
                    strong { "This exact revision is already public." }
                    " No publication action is needed."
                }
            }
        },
    )
}

async fn show_confirmation(
    request_id: RequestId,
    BrowserFormSession {
        csrf_token,
        session,
    }: BrowserFormSession,
    UiPublication(coordinator): UiPublication,
    Path(encoded_post_id): Path<String>,
    query: Result<Query<RawReviewConfirmation>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let post_id = match PostId::parse(&encoded_post_id) {
        Ok(post_id) => post_id,
        Err(_) => return invalid_post_id(request_id),
    };
    let Query(raw) = match query {
        Ok(query) => query,
        Err(_) => return invalid_review_confirmation(request_id),
    };
    let Some(binding) = parse_review_binding(
        &raw.content_digest,
        &raw.revision,
        &raw.preview_digest,
        &raw.expected_site_digest,
        &raw.expected_site_version,
        &raw.expected_public_revision,
    ) else {
        return invalid_review_confirmation(request_id);
    };

    let projection = coordinator.read();
    let Some(post) = projection.catalog.current_post(&post_id) else {
        return stale_review(request_id);
    };
    let public = projection.ledger.published_post(&post_id);
    if projection.content_digest != binding.content_digest
        || projection.site != binding.expected_site
        || post.revision != binding.revision
        || reviewed_public_revision(public) != binding.expected_public_revision
    {
        return stale_review(request_id);
    }
    if post.document.metadata.draft == DraftStatus::Draft {
        return stale_review(request_id);
    }

    let preview_asset_endpoint = format!("{PREVIEW_ASSETS_PATH}/{}", projection.content_digest);
    let preview = match render_bound_post_preview(
        &projection.catalog,
        projection.frontend,
        &post_id,
        projection.tip_recipient.as_ref(),
        &preview_asset_endpoint,
        public.map(|entry| entry.published_at),
    ) {
        Ok(Some(preview)) if preview.digest == binding.preview_digest => preview,
        Ok(Some(_) | None) => return stale_review(request_id),
        Err(error) => {
            tracing::error!(%request_id, %post_id, error = %error, "admin confirmation preview render failed");
            return admin_ui::error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Preview unavailable",
                "The selected preview could not be rendered safely.",
                request_id,
            );
        }
    };
    let state = publication_state(post.document.metadata.draft, &post.revision, public);
    if !state.can_publish() {
        return stale_review(request_id);
    }

    let current_revision = public.map(|entry| entry.revision.as_str());
    let canonical_path = PublicPagePath::post(&post.document.metadata.slug);
    let idempotency_key = Uuid::new_v4().hyphenated().to_string();
    let csrf = csrf_token.expose_secret();
    let expected_public_revision =
        reviewed_public_revision_value(&binding.expected_public_revision);

    admin_ui::page_response(
        StatusCode::OK,
        "Confirm publication",
        PageKind::Authenticated,
        html! {
            h1 { "Confirm publication of “" (post.document.metadata.title.as_str()) "”" }
            section class="panel" aria-labelledby="confirmation-details" {
                h2 id="confirmation-details" { "Reviewed candidate" }
                dl {
                    dt { "Signed-in user" }
                    dd { code { (session.user_id) } }
                    dt { "Current public revision" }
                    dd {
                        @if let Some(revision) = current_revision {
                            code { (revision) }
                        } @else {
                            "Not published"
                        }
                    }
                    dt { "Candidate revision" }
                    dd { code { (binding.revision.as_str()) } }
                    dt { "Candidate content" }
                    dd { code { (binding.content_digest) } }
                    dt { "Preview digest" }
                    dd { code { (binding.preview_digest.as_str()) } }
                    dt { "Reviewed public site" }
                    dd { code { (binding.expected_site.digest.as_str()) }
                        " (version " (binding.expected_site.version) ")" }
                    dt { "Canonical URL" }
                    dd { a href=(preview.canonical_url.as_str()) { (preview.canonical_url.as_str()) } }
                    dt { "Canonical path" }
                    dd { code { (canonical_path.as_str()) } }
                }
                a class="button" href=(format!("/admin/posts/{post_id}/review")) {
                    "Review exact preview again"
                }
            }
            section class="panel" aria-labelledby="publish-heading" {
                h2 id="publish-heading" { "Publish or schedule" }
                p {
                    "Publication is rejected if the candidate, preview, current public revision, "
                    "or public site head changed after review."
                }
                form method="post" action=(format!("/admin/posts/{post_id}/publish")) {
                    input type="hidden" name="_csrf" value=(csrf);
                    input type="hidden" name="content_digest"
                        value=(binding.content_digest.to_string());
                    input type="hidden" name="revision" value=(binding.revision.as_str());
                    input type="hidden" name="preview_digest" value=(binding.preview_digest.as_str());
                    input type="hidden" name="expected_site_digest"
                        value=(binding.expected_site.digest.as_str());
                    input type="hidden" name="expected_site_version"
                        value=(binding.expected_site.version);
                    input type="hidden" name="expected_public_revision"
                        value=(expected_public_revision);
                    input type="hidden" name="idempotency_key" value=(idempotency_key);
                    label for="scheduled-at" { "Scheduled publication time (UTC)" }
                    input id="scheduled-at" type="datetime-local" name="scheduled_at"
                        aria-describedby="schedule-help";
                    p id="schedule-help" {
                        "Leave empty to publish now. To schedule, enter a future date and time in UTC. "
                        "Scheduling reserves this exact revision; later source changes do not replace it."
                    }
                    label {
                        input type="checkbox" name="accept_preview" value="accepted" required;
                        " I reviewed and accept this exact preview."
                    }
                    button type="submit" { "Approve this exact revision" }
                }
            }
        },
    )
}

async fn publish(
    request_id: RequestId,
    _browser: RequiredBrowserSession,
    UiPublication(coordinator): UiPublication,
    Path(encoded_post_id): Path<String>,
    form: Result<Form<RawPublicationApproval>, axum::extract::rejection::FormRejection>,
) -> Response {
    let post_id = match PostId::parse(&encoded_post_id) {
        Ok(post_id) => post_id,
        Err(_) => return invalid_post_id(request_id),
    };
    let raw = match form {
        Ok(Form(raw)) => raw,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return admin_ui::error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Publication confirmation is too large",
                "The confirmation exceeded the browser publication limit. Open the current preview and try again.",
                request_id,
            );
        }
        Err(_) => return invalid_publication_form(request_id),
    };
    let approval = match parse_approval(raw) {
        Some(approval) => approval,
        None => return invalid_publication_form(request_id),
    };
    let ReviewBinding {
        content_digest,
        revision,
        preview_digest,
        expected_site,
        expected_public_revision,
    } = approval.review;

    let publication = PublishNow {
        creation_key: approval.idempotency_key,
        publication_id: Uuid::new_v4(),
        stable_post_id: post_id.clone(),
        expected_revision: Some(revision),
        accepted_preview_digest: preview_digest,
    };
    let result = match approval.timing {
        PublicationTiming::Now => coordinator
            .publish_reviewed_now(PublishReviewedNow {
                publication,
                expected_content_digest: content_digest,
                expected_site,
                expected_public_revision,
            })
            .await
            .map(|published| published.publication_id),
        PublicationTiming::Scheduled(scheduled_at) => coordinator
            .schedule_reviewed(ScheduleReviewed {
                publication: Schedule {
                    creation_key: publication.creation_key,
                    publication_id: publication.publication_id,
                    stable_post_id: publication.stable_post_id,
                    expected_revision: publication.expected_revision,
                    accepted_preview_digest: publication.accepted_preview_digest,
                    scheduled_at,
                },
                expected_content_digest: content_digest,
                expected_site,
                expected_public_revision,
            })
            .await
            .map(|outcome| match outcome {
                ScheduledApprovalOutcome::Scheduled(release) => release.publication_id,
                ScheduledApprovalOutcome::Published(release) => release.publication_id,
            }),
    };
    match result {
        Err(PublicationActivationError::ScheduleLookup(
            SchedulePublicationLookupError::Cancelled { publication_id }
            | SchedulePublicationLookupError::Blocked { publication_id },
        )) => admin_ui::redirect(&format!("/admin/releases/{publication_id}")),
        Ok(release_id) => admin_ui::redirect(&format!("/admin/releases/{release_id}")),
        Err(error) => {
            let spec = activation_error(&error);
            if spec.status.is_server_error() {
                tracing::error!(%request_id, %post_id, error = %error, "admin UI publication failed");
            }
            admin_ui::error_response(
                spec.status,
                "Publication did not complete",
                spec.message,
                request_id,
            )
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum RawReleaseChange {
    Retry {
        #[serde(rename = "_csrf")]
        csrf: SecretString,
        operation_id: Uuid,
        expected_version: Box<str>,
    },
    Reschedule {
        #[serde(rename = "_csrf")]
        csrf: SecretString,
        operation_id: Uuid,
        expected_version: Box<str>,
        scheduled_at: Box<str>,
    },
    Cancel {
        #[serde(rename = "_csrf")]
        csrf: SecretString,
        operation_id: Uuid,
        expected_version: Box<str>,
    },
}

enum ReleaseControl {
    Change(ChangeRelease),
    Retry(RetryRelease),
}

impl RawReleaseChange {
    fn into_command(self, publication_id: Uuid) -> Option<ReleaseControl> {
        let (csrf, operation_id, expected_version, change) = match self {
            Self::Retry {
                csrf,
                operation_id,
                expected_version,
            } => {
                let expected_version = expected_version.parse::<u64>().ok()?;
                if csrf.expose_secret().is_empty() || expected_version == 0 {
                    return None;
                }
                return Some(ReleaseControl::Retry(RetryRelease {
                    operation_id,
                    publication_id,
                    expected_version,
                    now: OffsetDateTime::now_utc(),
                }));
            }
            Self::Reschedule {
                csrf,
                operation_id,
                expected_version,
                scheduled_at,
            } => {
                let PublicationTiming::Scheduled(scheduled_at) =
                    parse_publication_timing(&scheduled_at)?
                else {
                    return None;
                };
                (
                    csrf,
                    operation_id,
                    expected_version,
                    ReleaseChange::Reschedule { scheduled_at },
                )
            }
            Self::Cancel {
                csrf,
                operation_id,
                expected_version,
            } => (csrf, operation_id, expected_version, ReleaseChange::Cancel),
        };
        let expected_version = expected_version.parse::<u64>().ok()?;
        if csrf.expose_secret().is_empty() || expected_version == 0 {
            return None;
        }
        Some(ReleaseControl::Change(ChangeRelease {
            operation_id,
            publication_id,
            expected_version,
            change,
            now: OffsetDateTime::now_utc(),
        }))
    }
}

async fn change_release(
    request_id: RequestId,
    _browser: RequiredBrowserSession,
    UiPublication(coordinator): UiPublication,
    Path(encoded_release_id): Path<String>,
    form: Result<Form<RawReleaseChange>, axum::extract::rejection::FormRejection>,
) -> Response {
    let release_id = match Uuid::parse_str(&encoded_release_id) {
        Ok(id) => id,
        Err(_) => return invalid_release_change(request_id),
    };
    let raw = match form {
        Ok(Form(raw)) => raw,
        Err(error) if error.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return admin_ui::error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Release change is too large",
                "Open the release page and try again.",
                request_id,
            );
        }
        Err(_) => return invalid_release_change(request_id),
    };
    let Some(command) = raw.into_command(release_id) else {
        return invalid_release_change(request_id);
    };
    let result = match command {
        ReleaseControl::Change(command) => coordinator.change_release(command).await,
        ReleaseControl::Retry(command) => coordinator.retry_release(command).await,
    };
    match result {
        Ok(receipt) => admin_ui::redirect(&format!(
            "/admin/releases/{release_id}?operation={}",
            receipt.operation_id
        )),
        Err(error) => {
            let (status, message) = release_change_error(&error);
            if status.is_server_error() {
                tracing::error!(%request_id, %release_id, %error, "admin release change failed");
            }
            admin_ui::error_response(
                status,
                "Release change did not complete",
                message,
                request_id,
            )
        }
    }
}

fn invalid_release_change(request_id: RequestId) -> Response {
    admin_ui::error_response(
        StatusCode::BAD_REQUEST,
        "Invalid release change",
        "Use the current release page. Schedule edits require a future UTC date and time.",
        request_id,
    )
}

fn release_change_error(error: &ReleaseTransitionError) -> (StatusCode, &'static str) {
    match error {
        ReleaseTransitionError::Activation(PublicationActivationError::StalePreview { .. }) => (
            StatusCode::CONFLICT,
            "The approved preview cannot be reproduced. This release remains blocked.",
        ),
        ReleaseTransitionError::Activation(error) => {
            let spec = activation_error(error);
            (spec.status, spec.message)
        }
        ReleaseTransitionError::Mutation(ReleaseMutationError::Command(error)) => match error {
            ReleaseCommandError::NotFound => {
                (StatusCode::NOT_FOUND, "The release no longer exists.")
            }
            ReleaseCommandError::StaleVersion => (
                StatusCode::PRECONDITION_FAILED,
                "The release changed. Open its current page before trying again.",
            ),
            ReleaseCommandError::InvalidState => (
                StatusCode::CONFLICT,
                "This release cannot be changed in its current state. Publication may already have started.",
            ),
            ReleaseCommandError::IdempotencyConflict => (
                StatusCode::CONFLICT,
                "This operation identifier belongs to a different change. Open the release page to start a new change.",
            ),
            ReleaseCommandError::InvalidValue => (
                StatusCode::BAD_REQUEST,
                "The schedule must be a future UTC date and time.",
            ),
            ReleaseCommandError::OutcomeUnknown => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The result is not yet known. Repeat the original form to recover the same operation.",
            ),
        },
        ReleaseTransitionError::Mutation(ReleaseMutationError::Admission(_))
        | ReleaseTransitionError::Unavailable(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Release management is unavailable. Try again from the release page.",
        ),
        ReleaseTransitionError::Load(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "The release state could not be loaded safely. Check the release page before trying again.",
        ),
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleasesQuery {
    after: Option<Uuid>,
}

async fn show_releases(
    request_id: RequestId,
    _browser: RequiredBrowserSession,
    UiPublication(coordinator): UiPublication,
    query: Result<Query<ReleasesQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => {
            return admin_ui::error_response(
                StatusCode::BAD_REQUEST,
                "Invalid release page",
                "The release page cursor was not valid.",
                request_id,
            );
        }
    };
    let releases = match coordinator.releases(query.after).await {
        Ok(releases) => releases,
        Err(error) => {
            tracing::error!(%request_id, %error, "admin release list failed");
            return admin_ui::error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Releases unavailable",
                "The releases could not be loaded. Try this page again.",
                request_id,
            );
        }
    };
    admin_ui::page_response(
        StatusCode::OK,
        "Releases",
        PageKind::Authenticated,
        html! {
            h1 { "Releases" }
            a class="button" href="/admin" { "Back to posts" }
            @if releases.is_empty() {
                p { "No releases on this page. Review a post to publish or schedule its exact revision." }
            } @else {
                table {
                    thead { tr { th { "Release" } th { "Post" } th { "Status" } th { "Scheduled time (UTC)" } } }
                    tbody {
                        @for release in releases.iter().take(100) {
                            tr {
                                td { a href=(format!("/admin/releases/{}", release.publication_id)) { (release.publication_id) } }
                                td { (release.publication.stable_post_id) }
                                td { (release_status(release.publication.state).0) }
                                td { (release.publication.scheduled_at) }
                            }
                        }
                    }
                }
            }
            @if releases.len() > 100 {
                a class="button" href=(format!("/admin/releases?after={}", releases[99].publication_id)) { "Next page" }
            }
            @if query.after.is_some() {
                a class="button" href="/admin/releases" { "First page" }
            }
        },
    )
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseDetailQuery {
    operation: Option<Uuid>,
}

async fn show_release(
    request_id: RequestId,
    BrowserFormSession {
        csrf_token,
        session: _,
    }: BrowserFormSession,
    UiPublication(coordinator): UiPublication,
    Path(encoded_release_id): Path<String>,
    query: Result<Query<ReleaseDetailQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_release_change(request_id),
    };
    let release_id = match Uuid::parse_str(&encoded_release_id) {
        Ok(id) => id,
        Err(_) => {
            return admin_ui::error_response(
                StatusCode::BAD_REQUEST,
                "Invalid release",
                "The release identifier was not valid.",
                request_id,
            );
        }
    };
    let release = match coordinator.release(release_id).await {
        Ok(Some(release)) => release,
        Ok(None) => {
            return admin_ui::error_response(
                StatusCode::NOT_FOUND,
                "Release not found",
                "No accepted release has this identifier.",
                request_id,
            );
        }
        Err(error) => {
            tracing::error!(%request_id, %release_id, %error, "admin release read failed");
            return admin_ui::error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Release unavailable",
                "The release could not be loaded. Try this page again.",
                request_id,
            );
        }
    };
    let receipt = match query.operation {
        Some(operation_id) => match coordinator.release_operation(operation_id).await {
            Ok(Some(receipt)) if receipt.publication_id == release_id => Some(receipt),
            Ok(_) => {
                return admin_ui::error_response(
                    StatusCode::NOT_FOUND,
                    "Operation not found",
                    "This release has no accepted operation with that identifier.",
                    request_id,
                );
            }
            Err(error) => {
                tracing::error!(%request_id, %operation_id, %error, "admin release receipt read failed");
                return admin_ui::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Operation unavailable",
                    "The operation receipt could not be loaded. Try this page again.",
                    request_id,
                );
            }
        },
        None => None,
    };
    let view = release.publication;
    let (status, explanation) = release_status(view.state);
    admin_ui::page_response(
        StatusCode::OK,
        "Release",
        PageKind::Authenticated,
        html! {
            h1 { (status) }
        @if let Some(receipt) = receipt {
            p role="status" { "Operation " code { (receipt.operation_id) }
                " accepted at release version " (receipt.version) ": " (release_status(receipt.state).0) ". Current release details appear below." }
        }
            p role="status" { (explanation) }
            section class="panel" {
                h2 { "Release details" }
                dl {
                    dt { "Release identifier" } dd { code { (release.publication_id) } }
                    dt { "Post" } dd { code { (view.stable_post_id) } }
                    dt { "Approved revision" } dd { code { (view.pinned_post_digest.as_str()) } }
                    dt { "Preview digest" } dd { code { (release.accepted_preview_digest.as_str()) } }
                    dt { "Scheduled time (UTC)" } dd { (view.scheduled_at) }
                    dt { "Resource version" } dd { (view.version) }
                @if let Some(reason) = view.block_reason {
                    dt { "Blocked reason" }
                    dd { @match reason {
                        ActivationBlockReason::RevisionUnavailable => { "The approved revision is unavailable." }
                        ActivationBlockReason::PreviewChanged => { "The approved preview cannot currently be reproduced." }
                    } }
                }
                }
                @if view.state == CanonicalState::Scheduled {
                form method="post" action=(format!("/admin/releases/{release_id}")) {
                    input type="hidden" name="_csrf" value=(csrf_token.expose_secret());
                    input type="hidden" name="action" value="reschedule";
                    input type="hidden" name="operation_id" value=(Uuid::new_v4());
                    input type="hidden" name="expected_version" value=(view.version);
                    label for="reschedule-at" { "New scheduled time (UTC)" }
                    input id="reschedule-at" type="datetime-local" name="scheduled_at" required;
                    p { "This changes only the time. The approved revision stays the same." }
                    button type="submit" { "Change scheduled time" }
                }
            }
            @if view.state == CanonicalState::Blocked {
                form method="post" action=(format!("/admin/releases/{release_id}")) {
                    input type="hidden" name="_csrf" value=(csrf_token.expose_secret());
                    input type="hidden" name="action" value="retry";
                    input type="hidden" name="operation_id" value=(Uuid::new_v4());
                    input type="hidden" name="expected_version" value=(view.version);
                    p { "Retry uses the original approved revision and preview." }
                    button type="submit" { "Retry this release" }
                }
            }
            @if matches!(view.state, CanonicalState::Scheduled | CanonicalState::Blocked) {
                form method="post" action=(format!("/admin/releases/{release_id}")) {
                    input type="hidden" name="_csrf" value=(csrf_token.expose_secret());
                    input type="hidden" name="action" value="cancel";
                    input type="hidden" name="operation_id" value=(Uuid::new_v4());
                    input type="hidden" name="expected_version" value=(view.version);
                    p { "Cancellation keeps the current public revision available and retains this release in history." }
                    button type="submit" { "Cancel this release" }
                }
            }
            p { "Bookmark this page to check the durable result of this approval." }
                a class="button" href="/admin" { "Back to posts" }
            }
        },
    )
}

fn release_status(state: CanonicalState) -> (&'static str, &'static str) {
    match state {
        CanonicalState::Scheduled => (
            "Scheduled",
            "This exact revision is approved for publication at the time below.",
        ),
        CanonicalState::Activating => (
            "Publishing",
            "Publication has started. Refresh this page to check its result.",
        ),
        CanonicalState::Blocked => (
            "Blocked",
            "The approved revision could not be activated. The previous public revision remains available.",
        ),
        CanonicalState::Published => (
            "Published",
            "Publication completed. This revision is public.",
        ),
        CanonicalState::Superseded => (
            "Superseded",
            "Publication completed. A later approved revision has replaced this release.",
        ),
        CanonicalState::Cancelled => ("Cancelled", "This release will not publish."),
    }
}

async fn get_application_asset(
    _browser: RequiredBrowserSession,
    UiPublication(coordinator): UiPublication,
    Path((digest, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    application_asset_response(coordinator.read().frontend, &headers, &digest, &name)
        .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

fn publication_state(
    draft: DraftStatus,
    candidate: &PostRevisionDigest,
    public: Option<&PublishedPostRevision>,
) -> PublicationState {
    match (public, draft) {
        (Some(public), _) if &public.revision == candidate => PublicationState::Published,
        (Some(_), _) => PublicationState::UnpublishedChanges,
        (None, DraftStatus::Draft) => PublicationState::Draft,
        (None, DraftStatus::Publishable) => PublicationState::Unpublished,
    }
}

fn parse_approval(raw: RawPublicationApproval) -> Option<PublicationApproval> {
    if raw.accept_preview.as_ref() != "accepted" || raw._csrf.expose_secret().is_empty() {
        return None;
    }
    let idempotency_key = Uuid::parse_str(&raw.idempotency_key).ok()?;
    if idempotency_key.hyphenated().to_string() != raw.idempotency_key.as_ref() {
        return None;
    }
    let review = parse_review_binding(
        &raw.content_digest,
        &raw.revision,
        &raw.preview_digest,
        &raw.expected_site_digest,
        &raw.expected_site_version,
        &raw.expected_public_revision,
    )?;
    let timing = parse_publication_timing(&raw.scheduled_at)?;
    Some(PublicationApproval {
        timing,
        review,
        idempotency_key,
    })
}

fn parse_publication_timing(value: &str) -> Option<PublicationTiming> {
    if value.is_empty() {
        return Some(PublicationTiming::Now);
    }
    // The browser control is explicitly labelled UTC and has minute precision.
    if value.len() != 16 {
        return None;
    }
    let scheduled_at = OffsetDateTime::parse(&format!("{value}:00Z"), &Rfc3339).ok()?;
    i64::try_from(scheduled_at.unix_timestamp_nanos()).ok()?;
    Some(PublicationTiming::Scheduled(scheduled_at))
}

fn parse_review_binding(
    content_digest: &str,
    revision: &str,
    preview_digest: &str,
    expected_site_digest: &str,
    expected_site_version: &str,
    expected_public_revision: &str,
) -> Option<ReviewBinding> {
    let site_version = expected_site_version.parse::<u64>().ok()?;
    if site_version.to_string() != expected_site_version {
        return None;
    }
    let expected_public_revision = if expected_public_revision == "unpublished" {
        ReviewedPublicRevision::Unpublished
    } else {
        ReviewedPublicRevision::Published {
            revision: PostRevisionDigest::parse(expected_public_revision).ok()?,
        }
    };
    Some(ReviewBinding {
        content_digest: ContentTreeDigest::parse(content_digest).ok()?,
        revision: PostRevisionDigest::parse(revision).ok()?,
        preview_digest: PreviewDigest::parse(preview_digest).ok()?,
        expected_site: SiteHead {
            digest: SiteSnapshotDigest::parse(expected_site_digest).ok()?,
            version: site_version,
        },
        expected_public_revision,
    })
}

fn reviewed_public_revision(public: Option<&PublishedPostRevision>) -> ReviewedPublicRevision {
    public.map_or(ReviewedPublicRevision::Unpublished, |public| {
        ReviewedPublicRevision::Published {
            revision: public.revision.clone(),
        }
    })
}

fn reviewed_public_revision_value(revision: &ReviewedPublicRevision) -> &str {
    match revision {
        ReviewedPublicRevision::Unpublished => "unpublished",
        ReviewedPublicRevision::Published { revision } => revision.as_str(),
    }
}

fn confirmation_path(post_id: &PostId, binding: &ReviewBinding) -> String {
    let site_version = binding.expected_site.version.to_string();
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query
        .append_pair("content_digest", &binding.content_digest.to_string())
        .append_pair("revision", binding.revision.as_str())
        .append_pair("preview_digest", binding.preview_digest.as_str())
        .append_pair(
            "expected_site_digest",
            binding.expected_site.digest.as_str(),
        )
        .append_pair("expected_site_version", &site_version)
        .append_pair(
            "expected_public_revision",
            reviewed_public_revision_value(&binding.expected_public_revision),
        );
    format!("/admin/posts/{post_id}/confirm?{}", query.finish())
}

fn invalid_post_id(request_id: RequestId) -> Response {
    admin_ui::error_response(
        StatusCode::BAD_REQUEST,
        "Invalid post",
        "The post identifier must be a canonical lowercase UUID.",
        request_id,
    )
}

fn invalid_publication_form(request_id: RequestId) -> Response {
    admin_ui::error_response(
        StatusCode::BAD_REQUEST,
        "Invalid publication confirmation",
        "The confirmation was incomplete or invalid. Open the current preview and try again.",
        request_id,
    )
}

fn invalid_review_confirmation(request_id: RequestId) -> Response {
    admin_ui::error_response(
        StatusCode::BAD_REQUEST,
        "Invalid review confirmation",
        "The review confirmation was incomplete or invalid. Return to posts and review the current preview.",
        request_id,
    )
}

fn stale_review(request_id: RequestId) -> Response {
    admin_ui::error_response(
        StatusCode::PRECONDITION_FAILED,
        "Review is stale",
        "The candidate or public site changed after review. Return to posts and review the current preview.",
        request_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_approval_rejects_noncanonical_or_unaccepted_input() {
        let raw = |accept_preview: &str, idempotency_key: &str| RawPublicationApproval {
            _csrf: SecretString::new("mcc1_token"),
            content_digest: format!("content-b3-v1-{}", "11".repeat(32)).into(),
            revision: format!("post-b3-v1-{}", "22".repeat(32)).into(),
            preview_digest: format!("preview-b3-v1-{}", "33".repeat(32)).into(),
            expected_site_digest: format!("site-b3-v1-{}", "44".repeat(32)).into(),
            expected_site_version: "7".into(),
            expected_public_revision: "unpublished".into(),
            idempotency_key: idempotency_key.into(),
            accept_preview: accept_preview.into(),
            scheduled_at: "".into(),
        };
        let canonical = "67e55044-10b1-426f-9247-bb680e5fe0c8";

        assert!(parse_approval(raw("accepted", canonical)).is_some());
        assert!(parse_approval(raw("", canonical)).is_none());
        assert!(parse_approval(raw("accepted", "67E55044-10B1-426F-9247-BB680E5FE0C8")).is_none());
    }
}

#[cfg(test)]
mod workflow_tests {
    use std::{collections::BTreeMap, path::Path, sync::Arc};

    use axum::{
        Router,
        body::{Body, Bytes, to_bytes},
        http::{
            Method, Request, StatusCode,
            header::{HOST, ORIGIN},
        },
        response::Response,
    };
    use maincopy_shared::auth_api::{CSRF_COOKIE_NAME, SESSION_COOKIE_NAME};
    use markdown_compiler::{
        ContentTreeDigest, PostCollection, PostId, PostRevisionDigest, PostSlug, PreviewDigest,
        resolve_content_assets,
    };
    use time::OffsetDateTime;
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt as _;
    use uuid::Uuid;

    use crate::{
        admin::test_support::{ADMIN_AUTHORITY, BrowserSession, ProtectedAdminHarness},
        config::{
            DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
            DatabaseWriterQueueCapacity,
        },
        content_fixtures::{content_tree, post, publication},
        database,
        domain::{
            profile::ProfileStore,
            publication::{
                PublicLedgerProjection,
                activation::{PublicationCoordinator, PublicationCoordinatorHandle},
                store::{InstallStartupSnapshot, ObservedPostRevision, PublicationStore},
                web as publication_web,
            },
        },
        frontend_assets::embedded_manifest,
        render::{
            ContentCatalog, SiteSnapshotReader, build_site_snapshot, compile_content_catalog,
            render_bound_post_preview, render_site_shell, snapshot_store,
        },
        web::Readiness,
    };

    use super::{
        CanonicalState, ChangeRelease, PREVIEW_ASSETS_PATH, PublicationActivationError,
        ReleaseChange, ReviewBinding, Schedule, ScheduledApprovalOutcome, confirmation_path,
        reviewed_public_revision, reviewed_public_revision_value,
    };

    const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const POST_SLUG: &str = "hello-maincopy";

    struct WorkflowRuntime {
        _root: tempfile::TempDir,
        router: Router,
        auth: ProtectedAdminHarness,
        coordinator: PublicationCoordinatorHandle,
        snapshots: SiteSnapshotReader,
        actor_shutdown: CancellationToken,
        actor_task: JoinHandle<()>,
        writer_shutdown: CancellationToken,
        writer_task: JoinHandle<()>,
    }

    impl WorkflowRuntime {
        async fn stop(self) {
            drop(self.close().await);
        }

        async fn close(self) -> tempfile::TempDir {
            let Self {
                _root,
                router,
                auth,
                coordinator,
                snapshots: _,
                actor_shutdown,
                actor_task,
                writer_shutdown,
                writer_task,
            } = self;
            writer_shutdown.cancel();
            actor_shutdown.cancel();
            drop(router);
            drop(coordinator);
            actor_task
                .await
                .expect("the publication actor task must join");
            writer_task
                .await
                .expect("the publication writer task must join");
            auth.stop().await;
            _root
        }
    }

    struct ApprovalFixture {
        body: Bytes,
        confirmation_path: String,
        preview_path: String,
        revision: PostRevisionDigest,
        preview_digest: PreviewDigest,
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn browser_workflow_publishes_rejects_stale_review_and_updates_exactly() {
        let (initial_catalog, content_digest) = catalog("Initial body.");
        let runtime = workflow_runtime(Arc::clone(&initial_catalog), content_digest).await;

        let login = runtime
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/login")
                    .header(HOST, ADMIN_AUTHORITY)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let login_html = response_text(login).await;
        assert!(login_html.contains("Sign in to Maincopy"));
        assert!(login_html.contains("action=\"/admin/login\""));

        let unauthenticated = runtime
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin")
                    .header(HOST, ADMIN_AUTHORITY)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            unauthenticated.headers()[axum::http::header::LOCATION],
            "/admin/login"
        );

        let browser = runtime.auth.password_login(&runtime.router).await;

        let missing_logout_csrf = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, "/admin/logout", Bytes::new()))
            .await
            .unwrap();
        assert_eq!(missing_logout_csrf.status(), StatusCode::FORBIDDEN);

        let mut wrong_origin_logout = browser.request(
            Method::POST,
            "/admin/logout",
            Bytes::from(format!("_csrf={}", browser.as_csrf_token())),
        );
        wrong_origin_logout
            .headers_mut()
            .insert(ORIGIN, "https://untrusted.example".parse().unwrap());
        let wrong_origin_logout = runtime
            .router
            .clone()
            .oneshot(wrong_origin_logout)
            .await
            .unwrap();
        assert_eq!(wrong_origin_logout.status(), StatusCode::FORBIDDEN);

        let overview = browser_get(&runtime, &browser, "/admin").await;
        assert_eq!(overview.status(), StatusCode::OK);
        let overview_html = response_text(overview).await;
        assert!(overview_html.contains("Hello, Maincopy"));
        assert!(overview_html.contains("Not published"));
        assert!(overview_html.contains("action=\"/admin/logout\""));

        let initial = approval_fixture(&runtime.coordinator, &browser, Uuid::new_v4());
        let review_path = format!("/admin/posts/{POST_ID}/review");
        let review = browser_get(&runtime, &browser, &review_path).await;
        assert_eq!(review.status(), StatusCode::OK);
        let review_html = response_text(review).await;
        assert!(review_html.contains(initial.revision.as_str()));
        assert!(review_html.contains(initial.preview_digest.as_str()));
        assert!(review_html.contains(&initial.preview_path.replace('&', "&amp;")));
        assert!(review_html.contains(&initial.confirmation_path.replace('&', "&amp;")));
        assert!(review_html.contains("<iframe"));
        assert!(!review_html.contains(&format!("action=\"/admin/posts/{POST_ID}/publish\"")));

        let preview = browser_get(&runtime, &browser, &initial.preview_path).await;
        assert_eq!(preview.status(), StatusCode::OK);
        assert_eq!(
            preview.headers()[axum::http::header::X_FRAME_OPTIONS],
            "SAMEORIGIN"
        );
        assert!(
            preview.headers()[axum::http::header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'self'")
        );
        assert_eq!(
            preview.headers()[axum::http::header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .split(';')
                .next(),
            Some("sandbox allow-same-origin")
        );
        assert!(
            response_text(preview)
                .await
                .contains(embedded_manifest().css.public_path)
        );
        let stylesheet = browser_get(&runtime, &browser, embedded_manifest().css.public_path).await;
        assert_eq!(stylesheet.status(), StatusCode::OK);
        assert_eq!(
            stylesheet.headers()[axum::http::header::CONTENT_TYPE],
            "text/css; charset=utf-8"
        );
        assert_eq!(stylesheet.headers()["cache-control"], "private, no-store");
        let public_stylesheet = publication_web::router(runtime.snapshots.clone())
            .oneshot(
                Request::builder()
                    .uri(embedded_manifest().css.public_path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(public_stylesheet.status(), StatusCode::OK);
        let preview_css = to_bytes(stylesheet.into_body(), 1024 * 1024).await.unwrap();
        let public_css = to_bytes(public_stylesheet.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(preview_css, public_css);
        assert_eq!(preview_css.as_ref(), embedded_manifest().css.bytes);

        let confirmation = browser_get(&runtime, &browser, &initial.confirmation_path).await;
        assert_eq!(confirmation.status(), StatusCode::OK);
        let confirmation_html = response_text(confirmation).await;
        assert!(confirmation_html.contains("Confirm publication"));
        assert!(confirmation_html.contains(&format!("action=\"/admin/posts/{POST_ID}/publish\"")));
        for field in [
            "_csrf",
            "content_digest",
            "revision",
            "preview_digest",
            "expected_site_digest",
            "expected_site_version",
            "expected_public_revision",
            "idempotency_key",
            "accept_preview",
        ] {
            assert!(confirmation_html.contains(&format!("name=\"{field}\"")));
        }

        let publish_path = format!("/admin/posts/{POST_ID}/publish");
        let missing_publish_csrf = runtime
            .router
            .clone()
            .oneshot(browser.request(
                Method::POST,
                &publish_path,
                Bytes::from_static(b"accept_preview=accepted"),
            ))
            .await
            .unwrap();
        assert_eq!(missing_publish_csrf.status(), StatusCode::FORBIDDEN);
        assert!(runtime.coordinator.read().ledger.is_empty());

        let published = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &publish_path, initial.body.clone()))
            .await
            .unwrap();
        assert_eq!(published.status(), StatusCode::SEE_OTHER);
        let release_path = published.headers()[axum::http::header::LOCATION]
            .to_str()
            .unwrap();
        assert!(release_path.starts_with("/admin/releases/"));
        let release_page = browser_get(&runtime, &browser, release_path).await;
        assert_eq!(release_page.status(), StatusCode::OK);
        assert!(
            response_text(release_page)
                .await
                .contains("Publication completed.")
        );
        for scheduled_at in [
            None,
            Some(OffsetDateTime::now_utc() + time::Duration::days(1)),
        ] {
            let body = release_change_form(&browser, Uuid::new_v4(), 3, scheduled_at);
            let rejected = runtime
                .router
                .clone()
                .oneshot(browser.request(Method::POST, release_path, body))
                .await
                .unwrap();
            assert_eq!(rejected.status(), StatusCode::CONFLICT);
        }
        let first_publication = runtime
            .coordinator
            .read()
            .ledger
            .published_post(&PostId::parse(POST_ID).unwrap())
            .cloned()
            .expect("the initial browser publication must enter the public ledger");
        assert_eq!(first_publication.revision, initial.revision);
        let public_page = runtime
            .snapshots
            .load_full()
            .post_page(&PostSlug::parse(POST_SLUG).unwrap())
            .expect("the reviewed initial revision must be public");
        assert!(public_page.contains("Initial body."));

        let (stale_catalog, stale_content_digest) = catalog("First update.");
        runtime
            .coordinator
            .apply_content_catalog(stale_catalog, stale_content_digest, None)
            .await
            .unwrap();
        let replay = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &publish_path, initial.body))
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::SEE_OTHER);
        let stale_approval = approval_fixture(&runtime.coordinator, &browser, Uuid::new_v4());
        let (current_catalog, current_content_digest) = catalog("Reviewed update.");
        runtime
            .coordinator
            .apply_content_catalog(current_catalog, current_content_digest, None)
            .await
            .unwrap();

        let stale_confirmation =
            browser_get(&runtime, &browser, &stale_approval.confirmation_path).await;
        assert_eq!(stale_confirmation.status(), StatusCode::PRECONDITION_FAILED);

        let stale = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &publish_path, stale_approval.body))
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(
            runtime
                .coordinator
                .read()
                .ledger
                .published_post(&PostId::parse(POST_ID).unwrap())
                .unwrap()
                .revision,
            first_publication.revision
        );

        let changed = browser_get(&runtime, &browser, "/admin").await;
        assert!(response_text(changed).await.contains("Unpublished changes"));
        let update = approval_fixture(&runtime.coordinator, &browser, Uuid::new_v4());
        let update_confirmation = browser_get(&runtime, &browser, &update.confirmation_path).await;
        assert_eq!(update_confirmation.status(), StatusCode::OK);
        let updated = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &publish_path, update.body))
            .await
            .unwrap();
        assert_eq!(updated.status(), StatusCode::SEE_OTHER);
        let current_publication = runtime
            .coordinator
            .read()
            .ledger
            .published_post(&PostId::parse(POST_ID).unwrap())
            .cloned()
            .unwrap();
        assert_eq!(current_publication.revision, update.revision);
        assert_eq!(
            current_publication.published_at,
            first_publication.published_at
        );
        let public_page = runtime
            .snapshots
            .load_full()
            .post_page(&PostSlug::parse(POST_SLUG).unwrap())
            .unwrap();
        assert!(public_page.contains("Reviewed update."));

        let logout_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("_csrf", browser.as_csrf_token())
            .finish();
        let logout = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, "/admin/logout", logout_body.into()))
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            logout.headers()[axum::http::header::LOCATION],
            "/admin/login"
        );
        let cleared_cookies = logout
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .map(|header| header.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(cleared_cookies.len(), 2);
        for name in [SESSION_COOKIE_NAME, CSRF_COOKIE_NAME] {
            assert!(cleared_cookies.iter().any(|cookie| {
                cookie.starts_with(&format!("{name}=;")) && cookie.contains("Max-Age=0")
            }));
        }

        let revoked = browser_get(&runtime, &browser, "/admin").await;
        assert_eq!(revoked.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            revoked.headers()[axum::http::header::LOCATION],
            "/admin/login"
        );

        runtime.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn browser_schedule_preserves_reviewed_revision_and_recovers_lost_response() {
        let (initial, digest) = catalog("Scheduled body.");
        let runtime = workflow_runtime(initial, digest).await;
        let browser = runtime.auth.password_login(&runtime.router).await;
        let approval = approval_fixture(&runtime.coordinator, &browser, Uuid::new_v4());
        let now = OffsetDateTime::now_utc();
        let scheduled_at = now
            .replace_second(0)
            .unwrap()
            .replace_nanosecond(0)
            .unwrap()
            + time::Duration::days(1);
        let timestamp = scheduled_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let suffix = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("scheduled_at", &timestamp[..16])
            .finish();
        let body = Bytes::from(format!(
            "{}&{suffix}",
            std::str::from_utf8(&approval.body).unwrap()
        ));
        let path = format!("/admin/posts/{POST_ID}/publish");
        let response = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &path, body.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response.headers()[axum::http::header::LOCATION]
            .to_str()
            .unwrap()
            .to_owned();
        let release_id =
            Uuid::parse_str(location.strip_prefix("/admin/releases/").unwrap()).unwrap();
        assert!(runtime.coordinator.read().ledger.is_empty());
        assert!(
            runtime
                .snapshots
                .load_full()
                .post_page(&PostSlug::parse(POST_SLUG).unwrap())
                .is_none()
        );
        let details = response_text(browser_get(&runtime, &browser, &location).await).await;
        assert!(details.contains("Scheduled"));
        assert!(details.contains(approval.revision.as_str()));
        let list = response_text(browser_get(&runtime, &browser, "/admin/releases").await).await;
        assert!(list.contains(&location));

        for path in [
            "/admin/releases",
            location.as_str(),
            approval.preview_path.as_str(),
        ] {
            let response = publication_web::router(runtime.snapshots.clone())
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        let private = runtime
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&location)
                    .header(HOST, ADMIN_AUTHORITY)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(private.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            private.headers()[axum::http::header::LOCATION],
            "/admin/login"
        );

        let (changed, digest) = catalog("Later unapproved body.");
        runtime
            .coordinator
            .apply_content_catalog(changed, digest, None)
            .await
            .unwrap();
        // Repeat the original form as if the first mutation response was lost.
        let replay = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &path, body.clone()))
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::SEE_OTHER);
        assert_eq!(replay.headers()[axum::http::header::LOCATION], location);
        assert_eq!(runtime.coordinator.releases(None).await.unwrap().len(), 1);
        let changed_time = Bytes::from(
            std::str::from_utf8(&body).unwrap().replace(
                &suffix,
                &url::form_urlencoded::Serializer::new(String::new())
                    .append_pair(
                        "scheduled_at",
                        &(scheduled_at + time::Duration::days(1))
                            .format(&time::format_description::well_known::Rfc3339)
                            .unwrap()[..16],
                    )
                    .finish(),
            ),
        );
        let conflict = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &path, changed_time))
            .await
            .unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        runtime
            .coordinator
            .activate_scheduled(release_id, scheduled_at)
            .await
            .unwrap();
        let page = runtime
            .snapshots
            .load_full()
            .post_page(&PostSlug::parse(POST_SLUG).unwrap())
            .unwrap();
        assert!(page.contains("Scheduled body."));
        assert!(!page.contains("Later unapproved body."));
        let replay = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &path, body))
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::SEE_OTHER);
        assert_eq!(replay.headers()[axum::http::header::LOCATION], location);
        let details = response_text(browser_get(&runtime, &browser, &location).await).await;
        assert!(details.contains("Publication completed."));
        runtime.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn browser_schedule_rejects_invalid_time_and_stale_review_without_approval() {
        let (initial, digest) = catalog("Initial body.");
        let runtime = workflow_runtime(initial, digest).await;
        let browser = runtime.auth.password_login(&runtime.router).await;
        let approval = approval_fixture(&runtime.coordinator, &browser, Uuid::new_v4());
        let path = format!("/admin/posts/{POST_ID}/publish");
        for (time, expected) in [
            ("garbage", StatusCode::BAD_REQUEST),
            ("2000-01-01T12:00", StatusCode::BAD_REQUEST),
            ("9999-01-01T12:00", StatusCode::BAD_REQUEST),
        ] {
            let suffix = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("scheduled_at", time)
                .finish();
            let body = Bytes::from(format!(
                "{}&{suffix}",
                std::str::from_utf8(&approval.body).unwrap()
            ));
            let response = runtime
                .router
                .clone()
                .oneshot(browser.request(Method::POST, &path, body))
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        let (changed, digest) = catalog("New body.");
        runtime
            .coordinator
            .apply_content_catalog(changed, digest, None)
            .await
            .unwrap();
        let timestamp = (OffsetDateTime::now_utc() + time::Duration::days(1))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let suffix = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("scheduled_at", &timestamp[..16])
            .finish();
        let body = Bytes::from(format!(
            "{}&{suffix}",
            std::str::from_utf8(&approval.body).unwrap()
        ));
        let response = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &path, body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        assert!(runtime.coordinator.releases(None).await.unwrap().is_empty());
        assert!(runtime.coordinator.read().ledger.is_empty());
        runtime.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn browser_retries_the_original_blocked_release_and_replays_its_receipt() {
        let (initial, digest) = catalog("Originally approved body.");
        let runtime = workflow_runtime_with_blocked(initial, digest, true).await;
        let browser = runtime.auth.password_login(&runtime.router).await;
        let blocked = runtime.coordinator.releases(None).await.unwrap().remove(0);
        assert_eq!(blocked.publication.state, CanonicalState::Blocked);
        let path = format!("/admin/releases/{}", blocked.publication_id);
        let page = response_text(browser_get(&runtime, &browser, &path).await).await;
        assert!(page.contains("Retry this release"));
        assert!(page.contains("The approved revision is unavailable."));
        let operation_id = Uuid::new_v4();
        let retry = Bytes::from(
            std::str::from_utf8(&release_change_form(
                &browser,
                operation_id,
                blocked.publication.version,
                None,
            ))
            .unwrap()
            .replace("action=cancel", "action=retry"),
        );
        let response = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &path, retry.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let receipt_path = response.headers()[axum::http::header::LOCATION]
            .to_str()
            .unwrap()
            .to_owned();
        let published = runtime
            .coordinator
            .release(blocked.publication_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(published.publication.state, CanonicalState::Published);
        assert_eq!(
            published.publication.pinned_post_digest,
            blocked.publication.pinned_post_digest
        );
        assert_eq!(
            published.accepted_preview_digest,
            blocked.accepted_preview_digest
        );
        assert_eq!(runtime.coordinator.releases(None).await.unwrap().len(), 1);
        let replay = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &path, retry))
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::SEE_OTHER);
        assert_eq!(replay.headers()[axum::http::header::LOCATION], receipt_path);
        let page = response_text(browser_get(&runtime, &browser, &receipt_path).await).await;
        assert!(page.contains("Publication completed."));
        assert!(!page.contains("Retry this release"));
        let conflict =
            release_change_form(&browser, operation_id, blocked.publication.version, None);
        let response = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &path, conflict))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        runtime.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn browser_cancels_a_blocked_release_without_publishing_it() {
        let (initial, digest) = catalog("Private body.");
        let runtime = workflow_runtime_with_blocked(initial, digest, true).await;
        let browser = runtime.auth.password_login(&runtime.router).await;
        let blocked = runtime.coordinator.releases(None).await.unwrap().remove(0);
        let path = format!("/admin/releases/{}", blocked.publication_id);
        let cancel =
            release_change_form(&browser, Uuid::new_v4(), blocked.publication.version, None);
        let response = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &path, cancel))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            runtime
                .coordinator
                .release(blocked.publication_id)
                .await
                .unwrap()
                .unwrap()
                .publication
                .state,
            CanonicalState::Cancelled
        );
        assert!(runtime.coordinator.read().ledger.is_empty());
        runtime.stop().await;
    }

    fn schedule_form(approval: &ApprovalFixture, scheduled_at: OffsetDateTime) -> Bytes {
        let timestamp = scheduled_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let suffix = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("scheduled_at", &timestamp[..16])
            .finish();
        Bytes::from(format!(
            "{}&{suffix}",
            std::str::from_utf8(&approval.body).unwrap()
        ))
    }

    fn release_change_form(
        browser: &BrowserSession,
        operation_id: Uuid,
        version: u64,
        scheduled_at: Option<OffsetDateTime>,
    ) -> Bytes {
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        form.append_pair("_csrf", browser.as_csrf_token())
            .append_pair("operation_id", &operation_id.to_string())
            .append_pair("expected_version", &version.to_string());
        if let Some(scheduled_at) = scheduled_at {
            let timestamp = scheduled_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap();
            form.append_pair("action", "reschedule")
                .append_pair("scheduled_at", &timestamp[..16]);
        } else {
            form.append_pair("action", "cancel");
        }
        Bytes::from(form.finish())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn browser_edits_and_cancels_exact_release_versions_with_durable_replay() {
        let (initial, digest) = catalog("Approved body.");
        let runtime = workflow_runtime(initial, digest).await;
        let browser = runtime.auth.password_login(&runtime.router).await;
        let approval = approval_fixture(&runtime.coordinator, &browser, Uuid::new_v4());
        let scheduled_at = OffsetDateTime::now_utc()
            .replace_second(0)
            .unwrap()
            .replace_nanosecond(0)
            .unwrap()
            + time::Duration::days(1);
        let original = schedule_form(&approval, scheduled_at);
        let publish_path = format!("/admin/posts/{POST_ID}/publish");
        let created = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &publish_path, original.clone()))
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::SEE_OTHER);
        let location = created.headers()[axum::http::header::LOCATION]
            .to_str()
            .unwrap()
            .to_owned();
        let release_id =
            Uuid::parse_str(location.strip_prefix("/admin/releases/").unwrap()).unwrap();
        let page = response_text(browser_get(&runtime, &browser, &location).await).await;
        assert!(page.contains("Change scheduled time"));
        assert!(page.contains("Cancel this release"));

        let operation_id = Uuid::new_v4();
        let changed_time = scheduled_at + time::Duration::days(1);
        let edit = release_change_form(&browser, operation_id, 1, Some(changed_time));
        let response = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &location, edit.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let receipt_path = response.headers()[axum::http::header::LOCATION]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(receipt_path.contains(&operation_id.to_string()));
        let edited = runtime
            .coordinator
            .release(release_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(edited.publication.version, 2);
        assert_eq!(edited.publication.scheduled_at, changed_time);
        assert_eq!(edited.publication.pinned_post_digest, approval.revision);
        assert_eq!(edited.accepted_preview_digest, approval.preview_digest);

        let replay = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &publish_path, original.clone()))
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::SEE_OTHER);
        assert_eq!(replay.headers()[axum::http::header::LOCATION], location);
        let conflict = release_change_form(
            &browser,
            operation_id,
            1,
            Some(changed_time + time::Duration::days(1)),
        );
        let response = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &location, conflict))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let stale = release_change_form(&browser, Uuid::new_v4(), 1, None);
        let response = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &location, stale))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);

        let cancel = release_change_form(&browser, Uuid::new_v4(), 2, None);
        let response = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &location, cancel.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cancelled = runtime
            .coordinator
            .release(release_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cancelled.publication.state, CanonicalState::Cancelled);
        assert_eq!(cancelled.publication.version, 3);
        assert!(runtime.coordinator.read().ledger.is_empty());
        // The scheduler may already have selected this release before cancellation.
        assert!(matches!(
            runtime
                .coordinator
                .activate_scheduled(release_id, changed_time)
                .await,
            Err(PublicationActivationError::Database(_))
        ));
        for body in [edit, cancel] {
            let replay = runtime
                .router
                .clone()
                .oneshot(browser.request(Method::POST, &location, body))
                .await
                .unwrap();
            assert_eq!(replay.status(), StatusCode::SEE_OTHER);
        }
        let receipt = response_text(browser_get(&runtime, &browser, &receipt_path).await).await;
        assert!(receipt.contains("accepted at release version 2: Scheduled"));
        assert!(receipt.contains("Cancelled"));
        assert!(!receipt.contains("Cancel this release"));
        let replay = runtime
            .router
            .clone()
            .oneshot(browser.request(Method::POST, &publish_path, original))
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::SEE_OTHER);
        assert_eq!(replay.headers()[axum::http::header::LOCATION], location);

        // Cancellation keeps history but permits a fresh approval of the same revision.
        let next = approval_fixture(&runtime.coordinator, &browser, Uuid::new_v4());
        let response = runtime
            .router
            .clone()
            .oneshot(browser.request(
                Method::POST,
                &publish_path,
                schedule_form(&next, changed_time),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_ne!(response.headers()[axum::http::header::LOCATION], location);
        assert_eq!(runtime.coordinator.releases(None).await.unwrap().len(), 2);
        let root = runtime.close().await;
        let database_path = root.path().join("state/maincopy.db");
        let database = database::bootstrap(database_configuration(&database_path))
            .await
            .unwrap();
        let (store, writer) = database.into_store(8);
        let shutdown = CancellationToken::new();
        let writer_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            writer.run(writer_shutdown).await.unwrap();
        });
        let restored = store.publications.startup_snapshot_state().await.unwrap();
        assert!(restored.ledger.is_empty());
        assert_eq!(restored.scheduled.len(), 1);
        assert_eq!(
            store
                .publications
                .release(release_id)
                .await
                .unwrap()
                .unwrap()
                .publication
                .state,
            CanonicalState::Cancelled
        );
        let receipt = store
            .publications
            .release_operation(operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.version, 2);
        let replay = store
            .publications
            .change_release(ChangeRelease {
                operation_id,
                publication_id: release_id,
                expected_version: 1,
                change: ReleaseChange::Reschedule {
                    scheduled_at: changed_time,
                },
                now: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();
        assert_eq!(receipt, replay);
        shutdown.cancel();
        task.await.unwrap();
        drop(store);
        drop(root);
    }

    fn catalog(body: &str) -> (Arc<ContentCatalog>, ContentTreeDigest) {
        let tree = content_tree(
            publication(
                "publication.toml",
                "[site]\n\
                 title = \"Maincopy UI test\"\n\
                 base_url = \"https://example.test/\"\n\
                 description = \"Browser publication workflow.\"\n\
                 [author]\n\
                 name = \"Example Author\"\n"
                    .to_owned(),
            ),
            vec![post(
                "posts/hello-maincopy.md",
                PostCollection::Posts,
                format!(
                    "+++\n\
                     id = \"{POST_ID}\"\n\
                     title = \"Hello, Maincopy\"\n\
                     slug = \"{POST_SLUG}\"\n\
                     authored_at = 2026-09-04T12:00:00Z\n\
                     description = \"Browser workflow fixture.\"\n\
                     +++\n\
                     # Hello, Maincopy\n\n{body}\n"
                ),
            )],
            Vec::new(),
            0,
        );
        let content_digest = tree.digest();
        let content = tree.validate().unwrap();
        let assets = resolve_content_assets(&tree, &content).unwrap();
        (
            Arc::new(compile_content_catalog(&content, &assets).unwrap()),
            content_digest,
        )
    }

    async fn workflow_runtime(
        catalog: Arc<ContentCatalog>,
        content_digest: ContentTreeDigest,
    ) -> WorkflowRuntime {
        workflow_runtime_with_blocked(catalog, content_digest, false).await
    }

    async fn workflow_runtime_with_blocked(
        catalog: Arc<ContentCatalog>,
        content_digest: ContentTreeDigest,
        seed_blocked: bool,
    ) -> WorkflowRuntime {
        let ledger = PublicLedgerProjection::empty();
        let shell = render_site_shell(Arc::clone(&catalog), embedded_manifest(), &ledger).unwrap();
        let initial_snapshot = build_site_snapshot(shell, &ledger).unwrap();
        let initial_digest = initial_snapshot.digest.clone();
        let (snapshots, activator) = snapshot_store(initial_snapshot);

        let root = tempfile::tempdir().unwrap();
        let database_path = root.path().join("state/maincopy.db");
        let database = database::bootstrap(database_configuration(&database_path))
            .await
            .unwrap();
        let (store, writer) = database.into_store(16);
        let writer_shutdown = CancellationToken::new();
        let writer_cancellation = writer_shutdown.clone();
        let writer_task = tokio::spawn(async move {
            writer
                .run(writer_cancellation)
                .await
                .expect("the publication writer must stop cleanly");
        });
        let publication_store: PublicationStore = store.publications.clone();
        let profile_store: ProfileStore = store.profiles.clone();
        let site = publication_store
            .install_startup_snapshot(InstallStartupSnapshot {
                expected: None,
                candidate_digest: initial_digest,
                activated_at: OffsetDateTime::from_unix_timestamp(1_000).unwrap(),
                source_commit: None,
                posts: observed_posts(&catalog),
            })
            .await
            .unwrap();
        drop(store);

        let mut coordinator = PublicationCoordinator {
            catalog: Arc::clone(&catalog),
            content_digest: content_digest.clone(),
            candidates: Arc::new(BTreeMap::from([(content_digest, catalog)])),
            ledger,
            site,
            activator,
            store: publication_store,
            profiles: profile_store,
            tip_recipient: None,
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(tokio::sync::Notify::new()),
            readiness: Readiness::new(true),
            cancellation: CancellationToken::new(),
        };
        if seed_blocked {
            let post_id = PostId::parse(POST_ID).unwrap();
            let preview = render_bound_post_preview(
                &coordinator.catalog,
                coordinator.frontend,
                &post_id,
                None,
                PREVIEW_ASSETS_PATH,
                None,
            )
            .unwrap()
            .unwrap();
            let scheduled_at = OffsetDateTime::now_utc() + time::Duration::days(1);
            let scheduled = coordinator
                .schedule(Schedule {
                    creation_key: Uuid::new_v4(),
                    publication_id: Uuid::new_v4(),
                    stable_post_id: post_id,
                    expected_revision: Some(preview.revision),
                    accepted_preview_digest: preview.digest,
                    scheduled_at,
                })
                .await
                .unwrap();
            let ScheduledApprovalOutcome::Scheduled(scheduled) = scheduled else {
                panic!("expected scheduled approval");
            };
            let retained =
                std::mem::replace(&mut coordinator.candidates, Arc::new(BTreeMap::new()));
            assert!(matches!(
                coordinator
                    .activate_scheduled(scheduled.publication_id, scheduled_at)
                    .await,
                Err(PublicationActivationError::ReleaseBlocked { .. })
            ));
            coordinator.candidates = retained;
        }
        let (coordinator, actor) = coordinator.into_actor(8);
        let actor_shutdown = CancellationToken::new();
        let actor_cancellation = actor_shutdown.clone();
        let actor_task = tokio::spawn(async move {
            actor
                .run(actor_cancellation)
                .await
                .expect("the publication actor must stop cleanly");
        });
        let auth = ProtectedAdminHarness::start_with_password().await;
        let router = auth.runtime_router(coordinator.clone());

        WorkflowRuntime {
            _root: root,
            router,
            auth,
            coordinator,
            snapshots,
            actor_shutdown,
            actor_task,
            writer_shutdown,
            writer_task,
        }
    }

    fn observed_posts(catalog: &ContentCatalog) -> Vec<ObservedPostRevision> {
        catalog
            .rendered_posts()
            .map(|post| ObservedPostRevision {
                stable_post_id: post.document.metadata.id.clone(),
                revision_digest: post.revision.clone(),
                publication_status: post.document.metadata.draft,
                slug: post.document.metadata.slug.clone(),
            })
            .collect()
    }

    fn database_configuration(path: &Path) -> DatabaseConfigurationView<'_> {
        DatabaseConfigurationView {
            path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1_000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(16).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        }
    }

    fn approval_fixture(
        coordinator: &PublicationCoordinatorHandle,
        browser: &BrowserSession,
        idempotency_key: Uuid,
    ) -> ApprovalFixture {
        let projection = coordinator.read();
        let post_id = PostId::parse(POST_ID).unwrap();
        let public = projection.ledger.published_post(&post_id);
        let preview_asset_endpoint = format!("{PREVIEW_ASSETS_PATH}/{}", projection.content_digest);
        let preview = render_bound_post_preview(
            &projection.catalog,
            projection.frontend,
            &post_id,
            projection.tip_recipient.as_ref(),
            &preview_asset_endpoint,
            public.map(|entry| entry.published_at),
        )
        .unwrap()
        .unwrap();
        let binding = ReviewBinding {
            content_digest: projection.content_digest.clone(),
            revision: preview.revision.clone(),
            preview_digest: preview.digest.clone(),
            expected_site: projection.site.clone(),
            expected_public_revision: reviewed_public_revision(public),
        };
        let preview_path = format!(
            "/api/admin/v1/posts/{POST_ID}/preview?revision={}&content_digest={}",
            preview.revision.as_str(),
            projection.content_digest
        );
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("_csrf", browser.as_csrf_token())
            .append_pair("content_digest", &projection.content_digest.to_string())
            .append_pair("revision", preview.revision.as_str())
            .append_pair("preview_digest", preview.digest.as_str())
            .append_pair("expected_site_digest", projection.site.digest.as_str())
            .append_pair(
                "expected_site_version",
                &projection.site.version.to_string(),
            )
            .append_pair(
                "expected_public_revision",
                reviewed_public_revision_value(&binding.expected_public_revision),
            )
            .append_pair("idempotency_key", &idempotency_key.hyphenated().to_string())
            .append_pair("accept_preview", "accepted")
            .finish();
        ApprovalFixture {
            body: Bytes::from(body),
            confirmation_path: confirmation_path(&post_id, &binding),
            preview_path,
            revision: preview.revision,
            preview_digest: preview.digest,
        }
    }

    async fn browser_get(
        runtime: &WorkflowRuntime,
        browser: &BrowserSession,
        path: &str,
    ) -> Response {
        runtime
            .router
            .clone()
            .oneshot(browser.request(Method::GET, path, Bytes::new()))
            .await
            .unwrap()
    }

    async fn response_text(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), 2 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }
}
