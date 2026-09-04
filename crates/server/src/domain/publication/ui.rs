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
    PublicPagePath, PublishedPostRevision,
    activation::{
        PublicationCoordinatorHandle, PublishNow, PublishReviewedNow, ReviewedPublicRevision,
    },
    admin::activation_error,
    store::SiteHead,
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

struct PublicationApproval {
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
        Router::new().route("/admin/posts/{post_id}/confirm", get(show_confirmation)),
        security_state,
        AdminScope::ReleaseManage,
    );
    let publication = browser_scoped_router(
        Router::new()
            .route("/admin/posts/{post_id}/publish", post(publish))
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
                h2 id="publish-heading" { "Publish now" }
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
                    label {
                        input type="checkbox" name="accept_preview" value="accepted" required;
                        " I reviewed and accept this exact preview."
                    }
                    button type="submit" { "Publish this exact revision" }
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

    let result = coordinator
        .publish_reviewed_now(PublishReviewedNow {
            publication: PublishNow {
                creation_key: approval.idempotency_key,
                publication_id: Uuid::new_v4(),
                stable_post_id: post_id.clone(),
                expected_revision: Some(revision),
                accepted_preview_digest: preview_digest,
            },
            expected_content_digest: content_digest,
            expected_site,
            expected_public_revision,
        })
        .await;
    match result {
        Ok(_) => redirect_to_overview(&post_id),
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
    Some(PublicationApproval {
        review,
        idempotency_key,
    })
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

fn redirect_to_overview(post_id: &PostId) -> Response {
    admin_ui::redirect(&format!("/admin?published={post_id}"))
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
        PREVIEW_ASSETS_PATH, ReviewBinding, confirmation_path, reviewed_public_revision,
        reviewed_public_revision_value,
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
            drop(_root);
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
        assert_eq!(
            published.headers()[axum::http::header::LOCATION],
            format!("/admin?published={POST_ID}")
        );
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

        let coordinator = PublicationCoordinator {
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
