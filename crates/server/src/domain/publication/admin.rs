use std::sync::Arc;

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Path, Query, Request},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{
            CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, LINK,
            X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
        },
        request::Parts,
    },
    response::{Html, IntoResponse as _, Response},
};
use maincopy_shared::posts::{ListPostsResponse, PostPublicationState, PostSummary};
use maincopy_shared::publication::{
    CONTENT_DIGEST_HEADER, IDEMPOTENCY_KEY_HEADER, POST_REVISION_HEADER, PREVIEW_DIGEST_HEADER,
    PublicationApprovalState, PublishNowRequest, PublishNowResponse,
};
use markdown_compiler::{
    ContentTreeDigest, DraftStatus, LogicalAssetPath, PostId, PostRevisionDigest, PreviewDigest,
};
use serde::Deserialize;
use time::{OffsetDateTime, UtcOffset};
use utoipa_axum::{
    router::{UtoipaMethodRouter, UtoipaMethodRouterExt as _},
    routes,
};
use uuid::Uuid;

use crate::{
    admin::{
        problem::{AdminProblem, AdminProblemEnvelope, problem_response},
        request_id::RequestId,
    },
    database::store::{DatabaseAdmissionError, DatabaseCommandError, DatabaseMutationError},
    render::{ContentCatalog, PreviewAsset, render_bound_post_preview},
};

use super::{
    PublicLedgerProjection,
    activation::{
        PublicationActivationError, PublicationCoordinatorHandle, PublishNow, Schedule,
        ScheduledApprovalOutcome,
    },
    assets::AssetDelivery,
    store::{
        PublicationRouteOwnershipError, PublishNowLookupError, SchedulePublicationLookupError,
        SiteHead,
    },
};

#[cfg(test)]
use super::activation::PublishReviewError;

const MAX_PUBLICATION_REQUEST_BYTES: usize = 4 * 1024;
const DEFAULT_POST_PAGE_LIMIT: u16 = 50;
const MAX_POST_PAGE_LIMIT: u16 = 100;
const PREVIEW_ASSETS_PATH: &str = "/api/admin/v1/preview-assets";
const PREVIEW_CACHE_POLICY: HeaderValue = HeaderValue::from_static("private, no-store");
const NOSNIFF: HeaderValue = HeaderValue::from_static("nosniff");
// Keep the admin origin so protected styles and media receive the session cookie.
// Scripts and forms stay disabled; an opaque sandbox origin breaks styled previews.
const PREVIEW_DOCUMENT_SANDBOX: HeaderValue = HeaderValue::from_static(
    "sandbox allow-same-origin; default-src 'none'; script-src 'none'; connect-src 'none'; worker-src 'none'; child-src 'none'; frame-src 'none'; object-src 'none'; img-src 'self' data:; style-src 'self'; font-src 'self'; media-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'; navigate-to 'none'",
);
const SAME_ORIGIN_FRAMING: HeaderValue = HeaderValue::from_static("SAMEORIGIN");
const ASSET_SANDBOX: HeaderValue = HeaderValue::from_static("sandbox; default-src 'none'");
const DOWNLOAD_ASSET: HeaderValue =
    HeaderValue::from_static("attachment; filename=\"preview-asset\"");

pub(crate) fn list_routes() -> UtoipaMethodRouter {
    routes!(list_posts)
}

pub(crate) fn preview_routes() -> UtoipaMethodRouter {
    routes!(get_post_preview)
}

pub(crate) fn preview_asset_routes() -> UtoipaMethodRouter {
    routes!(get_preview_asset)
}

pub(crate) fn routes() -> UtoipaMethodRouter {
    routes!(create_publication).layer(DefaultBodyLimit::max(MAX_PUBLICATION_REQUEST_BYTES))
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListPostsQuery {
    cursor: Option<Uuid>,
    limit: Option<u16>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewAssetQuery {
    path: Box<str>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PostPreviewQuery {
    revision: Option<Box<str>>,
    content_digest: Option<Box<str>>,
}

struct PostsPage {
    cursor: Option<Uuid>,
    limit: usize,
}

impl<S> FromRequestParts<S> for PostsPage
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = publication_request_id(parts, state).await?;
        let Query(query) = Query::<ListPostsQuery>::from_request_parts(parts, state)
            .await
            .map_err(|_| {
                problem(
                    ErrorSpec::bad_request(
                        "invalid_posts_query",
                        "cursor and limit must use valid pagination values",
                    ),
                    request_id,
                )
            })?;
        let limit = query.limit.unwrap_or(DEFAULT_POST_PAGE_LIMIT);
        if !(1..=MAX_POST_PAGE_LIMIT).contains(&limit) {
            return Err(problem(
                ErrorSpec::bad_request("invalid_posts_limit", "limit must be between 1 and 100"),
                request_id,
            ));
        }

        Ok(Self {
            cursor: query.cursor,
            limit: usize::from(limit),
        })
    }
}

struct PostPreviewInput {
    post_id: PostId,
    expected_revision: Option<PostRevisionDigest>,
    expected_content_digest: Option<ContentTreeDigest>,
}

impl<S> FromRequestParts<S> for PostPreviewInput
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = publication_request_id(parts, state).await?;
        let Path(encoded_post_id) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| {
                preview_problem(
                    ErrorSpec::bad_request(
                        "invalid_post_id",
                        "post_id must be a canonical lowercase UUID",
                    ),
                    request_id,
                )
            })?;
        let post_id = PostId::parse(&encoded_post_id).map_err(|_| {
            preview_problem(
                ErrorSpec::bad_request(
                    "invalid_post_id",
                    "post_id must be a canonical lowercase UUID",
                ),
                request_id,
            )
        })?;

        let Query(query) = Query::<PostPreviewQuery>::from_request_parts(parts, state)
            .await
            .map_err(|_| {
                preview_problem(
                    ErrorSpec::bad_request(
                        "invalid_preview_query",
                        "revision and content_digest must use valid preview preconditions",
                    ),
                    request_id,
                )
            })?;
        let expected_revision = query
            .revision
            .as_deref()
            .map(PostRevisionDigest::parse)
            .transpose()
            .map_err(|_| {
                preview_problem(
                    ErrorSpec::bad_request(
                        "invalid_preview_revision",
                        "revision must be a complete Maincopy post revision digest",
                    ),
                    request_id,
                )
            })?;
        let expected_content_digest = query
            .content_digest
            .as_deref()
            .map(ContentTreeDigest::parse)
            .transpose()
            .map_err(|_| {
                preview_problem(
                    ErrorSpec::bad_request(
                        "invalid_preview_content_digest",
                        "content_digest must be a complete Maincopy content digest",
                    ),
                    request_id,
                )
            })?;

        Ok(Self {
            post_id,
            expected_revision,
            expected_content_digest,
        })
    }
}

struct PreviewAssetInput {
    content_digest: ContentTreeDigest,
    asset_path: LogicalAssetPath,
}

impl<S> FromRequestParts<S> for PreviewAssetInput
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = publication_request_id(parts, state).await?;
        let Path(content_digest) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| {
                preview_problem(
                    ErrorSpec::bad_request(
                        "invalid_preview_asset_namespace",
                        "the preview asset namespace must include a content digest",
                    ),
                    request_id,
                )
            })?;
        let content_digest = ContentTreeDigest::parse(&content_digest).map_err(|_| {
            preview_problem(
                ErrorSpec::bad_request(
                    "invalid_preview_asset_namespace",
                    "the preview asset namespace must be a complete Maincopy content digest",
                ),
                request_id,
            )
        })?;
        let Query(query) = Query::<PreviewAssetQuery>::from_request_parts(parts, state)
            .await
            .map_err(|_| {
                preview_problem(
                    ErrorSpec::bad_request(
                        "invalid_preview_asset_query",
                        "the preview asset request must contain one logical path",
                    ),
                    request_id,
                )
            })?;
        let asset_path = LogicalAssetPath::parse(&query.path).map_err(|_| {
            preview_problem(
                ErrorSpec::bad_request(
                    "invalid_preview_asset_path",
                    "the preview asset path must be a portable path below assets/",
                ),
                request_id,
            )
        })?;
        Ok(Self {
            content_digest,
            asset_path,
        })
    }
}

struct AvailablePublication(PublicationCoordinatorHandle);

impl<S> FromRequestParts<S> for AvailablePublication
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = publication_request_id(parts, state).await?;
        parts
            .extensions
            .get::<PublicationCoordinatorHandle>()
            .cloned()
            .map(Self)
            .ok_or_else(|| problem(publication_unavailable(), request_id))
    }
}

struct AvailablePreviewPublication(PublicationCoordinatorHandle);

impl<S> FromRequestParts<S> for AvailablePreviewPublication
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = publication_request_id(parts, state).await?;
        parts
            .extensions
            .get::<PublicationCoordinatorHandle>()
            .cloned()
            .map(Self)
            .ok_or_else(|| preview_problem(publication_unavailable(), request_id))
    }
}

pub(crate) struct PublicationCommand {
    request_id: RequestId,
    coordinator: PublicationCoordinatorHandle,
    creation_key: Uuid,
    stable_post_id: PostId,
    expected_revision: Option<PostRevisionDigest>,
    accepted_preview_digest: PreviewDigest,
    scheduled_for: Option<OffsetDateTime>,
}

impl<S> FromRequest<S> for PublicationCommand
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let (mut parts, body) = request.into_parts();
        let request_id = publication_request_id(&mut parts, state).await?;
        let headers = parts.headers.clone();
        let coordinator = parts
            .extensions
            .get::<PublicationCoordinatorHandle>()
            .cloned();
        let request = Request::from_parts(parts, body);
        let Json(request) = Json::<PublishNowRequest>::from_request(request, state)
            .await
            .map_err(|rejection| {
                if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    problem(
                        ErrorSpec::new(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "request_body_too_large",
                            "the request body must not exceed 4096 bytes",
                        ),
                        request_id,
                    )
                } else {
                    problem(
                        ErrorSpec::bad_request(
                            "invalid_request_body",
                            "the request body must be valid publication JSON",
                        ),
                        request_id,
                    )
                }
            })?;
        let creation_key = idempotency_key(&headers).map_err(|spec| problem(spec, request_id))?;
        let expected_revision = request
            .expected_revision
            .as_deref()
            .map(PostRevisionDigest::parse)
            .transpose()
            .map_err(|_| {
                problem(
                    ErrorSpec::bad_request(
                        "invalid_expected_revision",
                        "expected_revision must be a complete Maincopy post revision digest",
                    ),
                    request_id,
                )
            })?;
        let accepted_preview_digest = PreviewDigest::parse(request.preview_digest.as_str())
            .map_err(|_| {
                problem(
                    ErrorSpec::bad_request(
                        "invalid_preview_digest",
                        "preview_digest must be a complete Maincopy preview digest",
                    ),
                    request_id,
                )
            })?;
        let stable_post_id =
            PostId::parse(&request.post_id.hyphenated().to_string()).map_err(|_| {
                problem(
                    ErrorSpec::bad_request(
                        "invalid_post_id",
                        "post_id must be a canonical lowercase UUID",
                    ),
                    request_id,
                )
            })?;
        let coordinator =
            coordinator.ok_or_else(|| problem(publication_unavailable(), request_id))?;

        Ok(Self {
            request_id,
            coordinator,
            creation_key,
            stable_post_id,
            expected_revision,
            accepted_preview_digest,
            scheduled_for: request.scheduled_for,
        })
    }
}

#[allow(
    clippy::result_large_err,
    reason = "the Axum adapter returns a ready response to preserve the stable error envelope"
)]
async fn publication_request_id<S>(parts: &mut Parts, state: &S) -> Result<RequestId, Response>
where
    S: Send + Sync,
{
    RequestId::from_request_parts(parts, state)
        .await
        .map_err(|rejection| rejection.into_response())
}

#[utoipa::path(
    get,
    path = "/api/admin/v1/posts",
    params(
        ("cursor" = Option<Uuid>, Query, description = "Stable post UUID returned as next_cursor by the previous page"),
        ("limit" = Option<u16>, Query, description = "Page size from 1 through 100; defaults to 50")
    ),
    responses(
        (status = OK, description = "Posts loaded in the current immutable content catalog", body = ListPostsResponse,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = BAD_REQUEST, description = "Pagination parameters are invalid", body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = SERVICE_UNAVAILABLE, description = "Publication state is unavailable", body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID")))
    ),
    tag = "Posts"
)]
async fn list_posts(
    PostsPage { cursor, limit }: PostsPage,
    AvailablePublication(coordinator): AvailablePublication,
) -> Response {
    let coordinator = coordinator.read();
    Json(posts_page(
        &coordinator.catalog,
        &coordinator.content_digest,
        &coordinator.ledger,
        &coordinator.site,
        cursor,
        limit,
    ))
    .into_response()
}

#[utoipa::path(
    get,
    path = "/api/admin/v1/posts/{post_id}/preview",
    params(
        ("post_id" = Uuid, Path, description = "Stable post UUID from the selected candidate catalog"),
        ("revision" = Option<String>, Query, description = "Exact post revision precondition"),
        ("content_digest" = Option<String>, Query, description = "Exact retained content candidate precondition")
    ),
    responses(
        (status = OK, description = "Production-shell HTML for the current synchronized candidate revision", body = String,
            content_type = "text/html",
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-maincopy-preview-digest" = String, description = "Exact presentation binding accepted by publication commands"),
                ("x-maincopy-post-revision" = String, description = "Exact rendered post revision"),
                ("x-maincopy-content-digest" = String, description = "Retained candidate used to reproduce this preview"),
                ("link" = String, description = "Exact reviewed canonical URL with rel=canonical"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            )),
        (status = BAD_REQUEST, description = "The post UUID is invalid", body = AdminProblemEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            )),
        (status = NOT_FOUND, description = "The post is not present in the current candidate catalog", body = AdminProblemEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            )),
        (status = INTERNAL_SERVER_ERROR, description = "The candidate preview could not be rendered", body = AdminProblemEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            )),
        (status = SERVICE_UNAVAILABLE, description = "Candidate publication state is unavailable", body = AdminProblemEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            ))
    ),
    tag = "Posts"
)]
async fn get_post_preview(
    request_id: RequestId,
    PostPreviewInput {
        post_id,
        expected_revision,
        expected_content_digest,
    }: PostPreviewInput,
    AvailablePreviewPublication(coordinator): AvailablePreviewPublication,
) -> Response {
    let coordinator = coordinator.read();
    let (content_digest, catalog) = match expected_content_digest {
        Some(digest) => {
            let Some(catalog) = coordinator.candidates.get(&digest) else {
                return preview_problem(
                    ErrorSpec::new(
                        StatusCode::NOT_FOUND,
                        "preview_candidate_unavailable",
                        "the selected retained content candidate is unavailable",
                    ),
                    request_id,
                );
            };
            (digest, Arc::clone(catalog))
        }
        None => (
            coordinator.content_digest.clone(),
            Arc::clone(&coordinator.catalog),
        ),
    };
    if let Some(expected) = expected_revision.as_ref() {
        let actual = catalog.current_post(&post_id).map(|post| &post.revision);
        if actual != Some(expected) {
            return preview_problem(
                ErrorSpec::new(
                    StatusCode::PRECONDITION_FAILED,
                    "stale_preview_revision",
                    "the selected candidate does not contain the requested current post revision",
                ),
                request_id,
            );
        }
    }
    let frontend = coordinator.frontend;
    let published_at = coordinator
        .ledger
        .published_post(&post_id)
        .map(|published| published.published_at);
    let preview_asset_endpoint = format!("{PREVIEW_ASSETS_PATH}/{content_digest}");

    match render_bound_post_preview(
        &catalog,
        frontend,
        &post_id,
        coordinator.tip_recipient.as_ref(),
        &preview_asset_endpoint,
        published_at,
    ) {
        Ok(Some(preview)) => {
            let preview_digest = HeaderValue::from_str(preview.digest.as_str());
            let revision = HeaderValue::from_str(preview.revision.as_str());
            let content_digest = HeaderValue::from_str(&content_digest.to_string());
            let canonical =
                HeaderValue::from_str(&format!("<{}>; rel=\"canonical\"", preview.canonical_url));
            let (Ok(preview_digest), Ok(revision), Ok(content_digest), Ok(canonical)) =
                (preview_digest, revision, content_digest, canonical)
            else {
                tracing::error!(
                    request_id = %request_id,
                    post_id = %post_id,
                    "typed preview metadata could not be represented as HTTP headers"
                );
                return preview_problem(
                    ErrorSpec::internal(
                        "preview_metadata_invalid",
                        "the selected post preview could not be represented safely",
                    ),
                    request_id,
                );
            };
            let mut response = Html(preview.html).into_response();
            response
                .headers_mut()
                .insert(CACHE_CONTROL, PREVIEW_CACHE_POLICY);
            response
                .headers_mut()
                .insert(PREVIEW_DIGEST_HEADER, preview_digest);
            response
                .headers_mut()
                .insert(POST_REVISION_HEADER, revision);
            response
                .headers_mut()
                .insert(CONTENT_DIGEST_HEADER, content_digest);
            response.headers_mut().insert(LINK, canonical);
            response
                .headers_mut()
                .insert("x-content-type-options", NOSNIFF);
            response
                .headers_mut()
                .insert(CONTENT_SECURITY_POLICY, PREVIEW_DOCUMENT_SANDBOX);
            // The reviewed document is frameable only by the authenticated
            // admin origin; its own sandbox still disables scripts and forms.
            response
                .headers_mut()
                .insert(X_FRAME_OPTIONS, SAME_ORIGIN_FRAMING);
            response
        }
        Ok(None) => preview_problem(
            ErrorSpec::new(
                StatusCode::NOT_FOUND,
                "post_not_found",
                "the selected post is not present in the current content catalog",
            ),
            request_id,
        ),
        Err(error) => {
            tracing::error!(
                request_id = %request_id,
                post_id = %post_id,
                error = %error,
                "admin post preview render failed"
            );
            preview_problem(
                ErrorSpec::internal(
                    "preview_render_failed",
                    "the selected post preview could not be rendered",
                ),
                request_id,
            )
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/v1/preview-assets/{content_digest}",
    params(
        ("content_digest" = String, Path, description = "Exact current candidate content digest from a preview URL"),
        ("path" = String, Query, description = "Portable logical asset path emitted by the preview renderer")
    ),
    responses(
        (status = OK, description = "Exact referenced bytes from the current candidate; passive authored media uses its allowlisted type, while active, opaque, and unsanitized generated bytes use application/octet-stream",
            body = [u8], content_type = "*/*",
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID"),
                ("x-content-type-options" = String, description = "Always nosniff"),
                ("content-security-policy" = String, description = "Always sandboxed with no default source"),
                ("content-disposition" = String, description = "Present only for active, opaque, or unsanitized generated downloads")
            )),
        (status = BAD_REQUEST, description = "The asset query is invalid", body = AdminProblemEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            )),
        (status = NOT_FOUND, description = "The candidate namespace is stale or the asset is absent", body = AdminProblemEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            )),
        (status = INTERNAL_SERVER_ERROR, description = "The retained asset fails its integrity check", body = AdminProblemEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            )),
        (status = SERVICE_UNAVAILABLE, description = "Candidate publication state is unavailable", body = AdminProblemEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            ))
    ),
    tag = "Posts"
)]
async fn get_preview_asset(
    request_id: RequestId,
    PreviewAssetInput {
        content_digest,
        asset_path,
    }: PreviewAssetInput,
    AvailablePreviewPublication(coordinator): AvailablePreviewPublication,
) -> Response {
    let asset = {
        let coordinator = coordinator.read();
        let Some(catalog) = coordinator.candidates.get(&content_digest) else {
            return preview_problem(
                ErrorSpec::new(
                    StatusCode::NOT_FOUND,
                    "preview_candidate_unavailable",
                    "the retained preview candidate is unavailable",
                ),
                request_id,
            );
        };
        match catalog.current_preview_asset(&asset_path) {
            Ok(Some(asset)) => asset,
            Ok(None) => {
                return preview_problem(
                    ErrorSpec::new(
                        StatusCode::NOT_FOUND,
                        "preview_asset_not_found",
                        "the preview asset is not referenced by the current content candidate",
                    ),
                    request_id,
                );
            }
            Err(error) => {
                tracing::error!(
                    request_id = %request_id,
                    asset_path = %asset_path,
                    error = %error,
                    "preview asset integrity check failed"
                );
                return preview_problem(
                    ErrorSpec::internal(
                        "preview_asset_unavailable",
                        "the preview asset could not be loaded",
                    ),
                    request_id,
                );
            }
        }
    };

    let (bytes, delivery) = match asset {
        PreviewAsset::Authored(bytes) => (bytes, AssetDelivery::for_authored(&asset_path)),
        PreviewAsset::RendererGenerated(bytes) => (bytes, AssetDelivery::for_untrusted_generated()),
    };

    let mut response = Response::new(Body::from(Bytes::from_owner(bytes)));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(delivery.content_type()),
    );
    match delivery {
        AssetDelivery::Inline(_) => {}
        AssetDelivery::Attachment => {
            response
                .headers_mut()
                .insert(CONTENT_DISPOSITION, DOWNLOAD_ASSET);
        }
    }
    response
        .headers_mut()
        .insert(CACHE_CONTROL, PREVIEW_CACHE_POLICY);
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, NOSNIFF);
    response
        .headers_mut()
        .insert(CONTENT_SECURITY_POLICY, ASSET_SANDBOX);
    response
}

fn posts_page(
    catalog: &ContentCatalog,
    content_digest: &ContentTreeDigest,
    ledger: &PublicLedgerProjection,
    site: &SiteHead,
    cursor: Option<Uuid>,
    limit: usize,
) -> ListPostsResponse {
    let mut selected = catalog
        .rendered_posts()
        .filter(|post| cursor.is_none_or(|cursor| post.document.metadata.id.as_uuid() > cursor))
        .take(limit + 1)
        .map(|post| {
            let published = ledger.published_post(&post.document.metadata.id);
            let publication_state = match (published, post.document.metadata.draft) {
                (Some(published), _) if published.revision == post.revision => {
                    PostPublicationState::Published
                }
                (Some(_), _) => PostPublicationState::UnpublishedChange,
                (None, DraftStatus::Draft) => PostPublicationState::Draft,
                (None, DraftStatus::Publishable) => PostPublicationState::Unpublished,
            };
            PostSummary {
                post_id: post.document.metadata.id.as_uuid(),
                source_path: post.document.path.as_str().into(),
                title: post.document.metadata.title.as_str().into(),
                slug: post.document.metadata.slug.as_str().into(),
                revision: post.revision.as_str().into(),
                publication_state,
                published_at: published.map(|entry| entry.published_at.to_offset(UtcOffset::UTC)),
            }
        })
        .collect::<Vec<_>>();
    let has_more = selected.len() > limit;
    selected.truncate(limit);
    let next_cursor = has_more.then(|| {
        selected
            .last()
            .expect("a non-empty bounded page must precede another post")
            .post_id
    });

    ListPostsResponse {
        content_digest: content_digest.to_string().into_boxed_str(),
        site_digest: site.digest.as_str().into(),
        site_version: site.version,
        posts: selected,
        next_cursor,
    }
}

#[utoipa::path(
    post,
    path = "/api/admin/v1/publications",
    request_body = PublishNowRequest,
    params(
        ("Idempotency-Key" = Uuid, Header, description = "Canonical UUID identifying retries of this command")
    ),
    responses(
        (status = OK, description = "Publication completed or an earlier result was replayed", body = PublishNowResponse,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = BAD_REQUEST, description = "The command header or revision is invalid", body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = NOT_FOUND, description = "The selected post does not exist", body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = CONFLICT, description = "The command conflicts with publication state", body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = PRECONDITION_FAILED, description = "The selected post revision is stale", body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = PAYLOAD_TOO_LARGE, description = "The request body exceeds 4096 bytes", body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = INTERNAL_SERVER_ERROR, description = "Publication snapshot construction failed", body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = SERVICE_UNAVAILABLE, description = "Publication state is unavailable or outcome is uncertain", body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID")))
    ),
    tag = "Publications"
)]
async fn create_publication(
    PublicationCommand {
        request_id,
        coordinator,
        creation_key,
        stable_post_id,
        expected_revision,
        accepted_preview_digest,
        scheduled_for,
    }: PublicationCommand,
) -> Response {
    let publication_id = Uuid::new_v4();
    if let Some(scheduled_at) = scheduled_for {
        let result = coordinator
            .schedule(Schedule {
                creation_key,
                publication_id,
                stable_post_id,
                expected_revision,
                accepted_preview_digest,
                scheduled_at,
            })
            .await;
        return match result {
            Ok(ScheduledApprovalOutcome::Scheduled(scheduled)) => Json(PublishNowResponse {
                publication_id: scheduled.publication_id,
                post_id: scheduled.stable_post_id.as_uuid(),
                preview_digest: wire_preview_digest(&scheduled.accepted_preview_digest),
                revision: scheduled.revision.as_str().into(),
                state: PublicationApprovalState::Scheduled,
                scheduled_for: Some(scheduled.scheduled_at.to_offset(UtcOffset::UTC)),
                published_at: None,
                site_digest: scheduled.site.digest.as_str().into(),
                site_version: scheduled.site.version,
            })
            .into_response(),
            Ok(ScheduledApprovalOutcome::Published(published)) => Json(PublishNowResponse {
                publication_id: published.publication_id,
                post_id: published.stable_post_id.as_uuid(),
                preview_digest: wire_preview_digest(&published.accepted_preview_digest),
                revision: published.revision.as_str().into(),
                state: PublicationApprovalState::Published,
                scheduled_for: Some(scheduled_at.to_offset(UtcOffset::UTC)),
                published_at: Some(published.published_at.to_offset(UtcOffset::UTC)),
                site_digest: published.site.digest.as_str().into(),
                site_version: published.site.version,
            })
            .into_response(),
            Err(error) => publication_problem(error, request_id),
        };
    }
    let result = coordinator
        .publish_now(PublishNow {
            creation_key,
            publication_id,
            stable_post_id,
            expected_revision,
            accepted_preview_digest,
        })
        .await;

    match result {
        Ok(published) => Json(PublishNowResponse {
            publication_id: published.publication_id,
            post_id: published.stable_post_id.as_uuid(),
            preview_digest: wire_preview_digest(&published.accepted_preview_digest),
            revision: published.revision.as_str().into(),
            state: PublicationApprovalState::Published,
            scheduled_for: None,
            published_at: Some(published.published_at.to_offset(UtcOffset::UTC)),
            site_digest: published.site.digest.as_str().into(),
            site_version: published.site.version,
        })
        .into_response(),
        Err(error) => publication_problem(error, request_id),
    }
}

fn wire_preview_digest(digest: &PreviewDigest) -> maincopy_shared::publication::PreviewDigest {
    maincopy_shared::publication::PreviewDigest::parse(digest.as_str())
        .expect("the domain and shared preview digest formats are identical")
}

fn publication_problem(error: PublicationActivationError, request_id: RequestId) -> Response {
    let spec = activation_error(&error);
    if spec.status.is_server_error() {
        tracing::error!(
            request_id = %request_id,
            error = %error,
            "admin publication request failed"
        );
    }
    problem(spec, request_id)
}

fn idempotency_key(headers: &HeaderMap) -> Result<Uuid, ErrorSpec> {
    let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER).iter();
    let value = values.next().ok_or_else(|| {
        ErrorSpec::bad_request("missing_idempotency_key", "Idempotency-Key is required")
    })?;
    if values.next().is_some() {
        return Err(ErrorSpec::bad_request(
            "invalid_idempotency_key",
            "Idempotency-Key must contain one canonical UUID",
        ));
    }
    let encoded = value.to_str().ok();
    let parsed = encoded.and_then(|value| Uuid::parse_str(value).ok());
    match (encoded, parsed) {
        (Some(encoded), Some(parsed)) if parsed.hyphenated().to_string() == encoded => Ok(parsed),
        _ => Err(ErrorSpec::bad_request(
            "invalid_idempotency_key",
            "Idempotency-Key must contain one canonical UUID",
        )),
    }
}

pub(super) fn activation_error(error: &PublicationActivationError) -> ErrorSpec {
    match error {
        PublicationActivationError::Coordinator(_) => publication_unavailable(),
        PublicationActivationError::PostNotFound { .. } => ErrorSpec::new(
            StatusCode::NOT_FOUND,
            "post_not_found",
            "the selected post is not present in the current content catalog",
        ),
        PublicationActivationError::DraftPost { .. } => ErrorSpec::conflict(
            "post_is_draft",
            "the selected post must be publishable before publication",
        ),
        PublicationActivationError::UpdateRevisionRequired { .. } => ErrorSpec::new(
            StatusCode::PRECONDITION_REQUIRED,
            "expected_revision_required",
            "publishing an update requires the exact current preview revision",
        ),
        PublicationActivationError::StaleRevision { .. } => ErrorSpec::new(
            StatusCode::PRECONDITION_FAILED,
            "stale_revision",
            "the current post revision does not match expected_revision",
        ),
        PublicationActivationError::StalePreview { .. } => ErrorSpec::new(
            StatusCode::PRECONDITION_FAILED,
            "stale_preview",
            "the current rendered preview does not match preview_digest",
        ),
        PublicationActivationError::StaleReview(_) => ErrorSpec::new(
            StatusCode::PRECONDITION_FAILED,
            "stale_review",
            "the reviewed candidate or public site changed before publication",
        ),
        PublicationActivationError::AlreadyPublished { .. } => ErrorSpec::conflict(
            "already_published",
            "the selected post is already published",
        ),
        PublicationActivationError::ApprovedRevisionUnavailable => ErrorSpec::conflict(
            "approved_revision_unavailable",
            "The approved revision is unavailable; this release remains blocked",
        ),
        PublicationActivationError::ReleaseBlocked { .. }
        | PublicationActivationError::ScheduleLookup(SchedulePublicationLookupError::Blocked {
            ..
        }) => ErrorSpec::conflict(
            "release_blocked",
            "This release is blocked and requires an explicit retry or cancellation",
        ),
        PublicationActivationError::ReleaseLoad(_) => publication_unavailable(),
        PublicationActivationError::ScheduleLookup(SchedulePublicationLookupError::Cancelled {
            ..
        }) => ErrorSpec::conflict(
            "release_cancelled",
            "This release was cancelled and will not publish",
        ),
        PublicationActivationError::ScheduleNotFuture { .. } => ErrorSpec::bad_request(
            "schedule_not_future",
            "scheduled_for must be later than the current server time",
        ),
        PublicationActivationError::Database(DatabaseMutationError::Admission(
            DatabaseAdmissionError::QueueFull | DatabaseAdmissionError::WriterClosed,
        ))
        | PublicationActivationError::Database(DatabaseMutationError::Command(
            DatabaseCommandError::OutcomeUnknown,
        ))
        | PublicationActivationError::DurableStateMismatch
        | PublicationActivationError::SnapshotActivationConflict
        | PublicationActivationError::CandidateDigestMismatch { .. }
        | PublicationActivationError::RouteOwnership(PublicationRouteOwnershipError::Query(_)) => {
            publication_unavailable()
        }
        PublicationActivationError::Database(DatabaseMutationError::Command(
            DatabaseCommandError::IdempotencyConflict,
        ))
        | PublicationActivationError::Lookup(PublishNowLookupError::IdempotencyConflict)
        | PublicationActivationError::ScheduleLookup(
            SchedulePublicationLookupError::IdempotencyConflict,
        ) => ErrorSpec::conflict(
            "idempotency_conflict",
            "Idempotency-Key is already bound to a different publication command",
        ),
        PublicationActivationError::Lookup(
            PublishNowLookupError::Query(_) | PublishNowLookupError::InvalidStoredState,
        )
        | PublicationActivationError::ScheduleLookup(
            SchedulePublicationLookupError::Query(_)
            | SchedulePublicationLookupError::InvalidStoredState
            | SchedulePublicationLookupError::ActivationInProgress,
        ) => publication_unavailable(),
        PublicationActivationError::Database(DatabaseMutationError::Command(
            DatabaseCommandError::Rejected,
        ))
        | PublicationActivationError::RouteOwnership(PublicationRouteOwnershipError::Conflict {
            ..
        }) => ErrorSpec::conflict(
            "publication_conflict",
            "the command conflicts with current publication state",
        ),
        PublicationActivationError::Database(DatabaseMutationError::Command(
            DatabaseCommandError::InvalidValue,
        ))
        | PublicationActivationError::RouteOwnership(
            PublicationRouteOwnershipError::InvalidRouteSet(_),
        ) => ErrorSpec::internal(
            "invalid_publication_state",
            "the publication command could not be represented safely",
        ),
        PublicationActivationError::SnapshotBuild(_) => ErrorSpec::internal(
            "snapshot_build_failed",
            "the publication snapshot could not be built",
        ),
    }
}

const fn publication_unavailable() -> ErrorSpec {
    ErrorSpec::unavailable(
        "publication_unavailable",
        "publication is temporarily unavailable",
    )
}

type ErrorSpec = AdminProblem;

fn problem(spec: ErrorSpec, request_id: RequestId) -> Response {
    problem_response(spec, request_id)
}

fn preview_problem(spec: ErrorSpec, request_id: RequestId) -> Response {
    let mut response = problem(spec, request_id);
    response
        .headers_mut()
        .insert(CACHE_CONTROL, PREVIEW_CACHE_POLICY);
    response
}

#[cfg(test)]
mod tests {
    use axum::{body::to_bytes, http::header::CONTENT_TYPE};
    use sqlx::sqlite::SqlitePoolOptions;
    use time::OffsetDateTime;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt as _;

    use super::*;
    use crate::{
        admin::test_support::ProtectedAdminHarness,
        domain::{
            profile::ProfileStore,
            publication::{
                MAX_PUBLIC_ROUTES, PublishedPostRevision,
                activation::PublicationCoordinator,
                store::{PublicationRoute, PublicationRouteSetError, PublicationStore},
            },
        },
        frontend_assets::embedded_manifest,
        render::{build_site_snapshot, compile_content_catalog, render_site_shell, snapshot_store},
        web::Readiness,
    };
    use markdown_compiler::{
        DiscoveredAsset, LogicalAssetPath, PostAlias, PostCollection, SiteSnapshotDigest,
        resolve_content_assets,
    };

    use crate::content_fixtures::{asset, content_tree, post, publication};

    const KEY: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";
    const PREVIEW_ASSET_PATH: &str = "assets/preview.png";
    const CURRENT_PREVIEW_ASSET: &[u8] = b"current preview image";

    fn publication_command_router() -> axum::Router {
        axum::Router::new()
            .route("/", axum::routing::post(create_publication))
            .layer(DefaultBodyLimit::max(MAX_PUBLICATION_REQUEST_BYTES))
    }

    fn publication_command_request(body: Body) -> Request {
        let mut request = Request::builder()
            .method(axum::http::Method::POST)
            .uri("/")
            .header(CONTENT_TYPE, "application/json")
            .header(IDEMPOTENCY_KEY_HEADER, KEY)
            .body(body)
            .unwrap();
        request
            .extensions_mut()
            .insert(RequestId(Uuid::parse_str(KEY).unwrap()));
        request
    }

    async fn response_problem_code(response: Response) -> String {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        body["error"]["code"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn malformed_publication_json_precedes_missing_coordinator() {
        let response = publication_command_router()
            .oneshot(publication_command_request(Body::from("{")))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_problem_code(response).await,
            "invalid_request_body"
        );
    }

    #[tokio::test]
    async fn oversized_publication_json_precedes_missing_coordinator() {
        let response = publication_command_router()
            .oneshot(publication_command_request(Body::from(vec![
                b' ';
                MAX_PUBLICATION_REQUEST_BYTES
                    + 1
            ])))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            response_problem_code(response).await,
            "request_body_too_large"
        );
    }

    #[tokio::test]
    async fn valid_publication_command_reports_missing_coordinator() {
        let body = serde_json::json!({
            "post_id": "11111111-1111-4111-8111-111111111111",
            "preview_digest": format!("preview-b3-v1-{}", "22".repeat(32)),
            "expected_revision": null
        })
        .to_string();
        let response = publication_command_router()
            .oneshot(publication_command_request(Body::from(body)))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response_problem_code(response).await,
            "publication_unavailable"
        );
    }

    fn posts_catalog() -> ContentCatalog {
        posts_catalog_with_asset("First post", CURRENT_PREVIEW_ASSET)
    }

    fn posts_catalog_with_first_title(first_title: &str) -> ContentCatalog {
        posts_catalog_with_asset(first_title, CURRENT_PREVIEW_ASSET)
    }

    fn posts_catalog_with_asset(first_title: &str, asset_bytes: &[u8]) -> ContentCatalog {
        posts_catalog_with_named_asset(first_title, PREVIEW_ASSET_PATH, asset_bytes)
    }

    fn posts_catalog_with_named_asset(
        first_title: &str,
        asset_path: &str,
        asset_bytes: &[u8],
    ) -> ContentCatalog {
        posts_catalog_with_assets(
            first_title,
            asset_path,
            vec![asset(
                LogicalAssetPath::parse(asset_path).unwrap(),
                asset_bytes.to_vec(),
            )],
        )
    }

    fn posts_catalog_with_unreferenced_asset(
        first_title: &str,
        asset_path: &str,
        asset_bytes: &[u8],
    ) -> ContentCatalog {
        posts_catalog_with_assets(
            first_title,
            asset_path,
            vec![
                asset(
                    LogicalAssetPath::parse(asset_path).unwrap(),
                    asset_bytes.to_vec(),
                ),
                asset(
                    LogicalAssetPath::parse("assets/unreferenced.bin").unwrap(),
                    b"must remain private".to_vec(),
                ),
            ],
        )
    }

    fn posts_catalog_with_assets(
        first_title: &str,
        asset_path: &str,
        assets: Vec<DiscoveredAsset>,
    ) -> ContentCatalog {
        let publication = publication(
            "publication.toml",
            "[site]\n\
             title = \"Admin posts\"\n\
             base_url = \"https://example.test/\"\n\
             description = \"Admin post tests.\"\n\
             [author]\n\
             name = \"Example Author\"\n"
                .to_owned(),
        );
        let posts = [
            (
                "11111111-1111-4111-8111-111111111111",
                "posts/first.md",
                PostCollection::Posts,
                first_title,
                "first-post",
            ),
            (
                "22222222-2222-4222-8222-222222222222",
                "posts/second.md",
                PostCollection::Posts,
                "Second post",
                "second-post",
            ),
            (
                "33333333-3333-4333-8333-333333333333",
                "drafts/third.md",
                PostCollection::Drafts,
                "Third post",
                "third-post",
            ),
        ]
        .into_iter()
        .map(|(id, path, collection, title, slug)| {
            post(
                path,
                collection,
                format!(
                    "+++\nid = \"{id}\"\ntitle = \"{title}\"\nslug = \"{slug}\"\n\
                     authored_at = 2026-08-30T12:00:00Z\n\
                     description = \"Post summary fixture.\"\n+++\n# {title}\n\n\
                     ![preview]({asset_path})\n"
                ),
            )
        })
        .collect();
        let tree = content_tree(publication, posts, assets, 0);
        let content = tree.validate().unwrap();
        let assets = resolve_content_assets(&tree, &content).unwrap();
        compile_content_catalog(&content, &assets).unwrap()
    }

    struct PreviewRuntime {
        router: axum::Router,
        auth: ProtectedAdminHarness,
        cancellation: CancellationToken,
        actor_task: tokio::task::JoinHandle<()>,
    }

    impl PreviewRuntime {
        async fn stop(self) {
            self.cancellation.cancel();
            self.actor_task.await.unwrap();
            self.auth.stop().await;
        }
    }

    async fn preview_router(ledger: PublicLedgerProjection) -> PreviewRuntime {
        let catalog = Arc::new(posts_catalog());
        preview_router_with_catalog(Arc::clone(&catalog), catalog, ledger).await
    }

    async fn preview_router_with_catalog(
        candidate: Arc<ContentCatalog>,
        public: Arc<ContentCatalog>,
        ledger: PublicLedgerProjection,
    ) -> PreviewRuntime {
        preview_runtime_with_catalog(candidate, public, ledger).await
    }

    async fn preview_runtime_with_catalog(
        candidate: Arc<ContentCatalog>,
        public: Arc<ContentCatalog>,
        ledger: PublicLedgerProjection,
    ) -> PreviewRuntime {
        preview_runtime_with_catalog_and_digest(
            candidate,
            public,
            ledger,
            ContentTreeDigest::from_bytes([0x44; 32]),
        )
        .await
    }

    async fn preview_runtime_with_catalog_and_digest(
        candidate: Arc<ContentCatalog>,
        public: Arc<ContentCatalog>,
        ledger: PublicLedgerProjection,
        content_digest: ContentTreeDigest,
    ) -> PreviewRuntime {
        let shell = render_site_shell(Arc::clone(&public), embedded_manifest(), &ledger).unwrap();
        let snapshot = build_site_snapshot(shell, &ledger).unwrap();
        let site = SiteHead {
            digest: snapshot.digest.clone(),
            version: 1,
        };
        let (_, activator) = snapshot_store(snapshot);
        let readers = SqlitePoolOptions::new()
            .connect_lazy("sqlite::memory:")
            .unwrap();
        let (mutations, _receiver) = tokio::sync::mpsc::channel(1);
        let mut candidates =
            std::collections::BTreeMap::from([(content_digest.clone(), Arc::clone(&candidate))]);
        candidates
            .entry(ContentTreeDigest::from_bytes([0x44; 32]))
            .or_insert(public);
        let candidates = Arc::new(candidates);
        let coordinator = PublicationCoordinator {
            catalog: candidate,
            content_digest,
            candidates,
            ledger,
            site,
            activator,
            store: PublicationStore::new(readers.clone(), mutations.clone()),
            profiles: ProfileStore::new(readers, mutations),
            tip_recipient: None,
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: std::collections::BTreeMap::new(),
            scheduler_wakeup: Arc::new(tokio::sync::Notify::new()),
            readiness: Readiness::new(true),
            cancellation: CancellationToken::new(),
        };
        let (coordinator, actor) = coordinator.into_actor(1);
        let cancellation = CancellationToken::new();
        let actor_cancellation = cancellation.clone();
        let actor_task = tokio::spawn(async move {
            actor.run(actor_cancellation).await.unwrap();
        });
        let auth = ProtectedAdminHarness::start().await;
        PreviewRuntime {
            router: auth.runtime_router(coordinator),
            auth,
            cancellation,
            actor_task,
        }
    }

    async fn get_preview(runtime: &PreviewRuntime, post_id: &str) -> Response {
        let path = format!("/api/admin/v1/posts/{post_id}/preview");
        runtime
            .router
            .clone()
            .oneshot(
                runtime
                    .auth
                    .request(axum::http::Method::GET, &path, Bytes::new(), None),
            )
            .await
            .unwrap()
    }

    async fn get_preview_asset(
        runtime: &PreviewRuntime,
        digest: &str,
        asset_path: &str,
    ) -> Response {
        let path = format!("{PREVIEW_ASSETS_PATH}/{digest}?path={asset_path}");
        runtime
            .router
            .clone()
            .oneshot(
                runtime
                    .auth
                    .request(axum::http::Method::GET, &path, Bytes::new(), None),
            )
            .await
            .unwrap()
    }

    #[test]
    fn post_pages_use_stable_id_cursors_and_public_ledger_state() {
        let catalog = posts_catalog();
        let published = catalog.rendered_posts().next().unwrap();
        let ledger = PublicLedgerProjection::try_from_exact_entries([PublishedPostRevision::new(
            published.document.metadata.id.clone(),
            published.revision.clone(),
            OffsetDateTime::UNIX_EPOCH,
        )])
        .unwrap();
        let site = SiteHead {
            digest: SiteSnapshotDigest::from_bytes([0x55; 32]),
            version: 7,
        };
        let content_digest = ContentTreeDigest::from_bytes([0x44; 32]);

        let first = posts_page(&catalog, &content_digest, &ledger, &site, None, 2);
        assert_eq!(first.content_digest.as_ref(), content_digest.to_string());
        assert_eq!(first.site_digest.as_ref(), site.digest.as_str());
        assert_eq!(first.site_version, 7);
        assert_eq!(first.posts.len(), 2);
        assert_eq!(
            first.posts[0].publication_state,
            PostPublicationState::Published
        );
        assert_eq!(
            first.posts[0].published_at,
            Some(OffsetDateTime::UNIX_EPOCH)
        );
        assert_eq!(
            first.posts[1].publication_state,
            PostPublicationState::Unpublished
        );
        assert_eq!(first.posts[1].published_at, None);
        assert_eq!(first.next_cursor, Some(first.posts[1].post_id));

        let second = posts_page(
            &catalog,
            &content_digest,
            &ledger,
            &site,
            first.next_cursor,
            2,
        );
        assert_eq!(second.posts.len(), 1);
        assert_eq!(
            second.posts[0].publication_state,
            PostPublicationState::Draft
        );
        assert_eq!(second.posts[0].source_path.as_ref(), "drafts/third.md");
        assert_eq!(second.next_cursor, None);
    }

    #[tokio::test]
    async fn preview_api_renders_current_published_unpublished_and_draft_candidates() {
        let catalog = posts_catalog();
        let published = catalog.rendered_posts().next().unwrap();
        let ledger = PublicLedgerProjection::try_from_exact_entries([PublishedPostRevision::new(
            published.document.metadata.id.clone(),
            published.revision.clone(),
            OffsetDateTime::UNIX_EPOCH,
        )])
        .unwrap();
        let runtime = preview_router(ledger).await;

        for (post_id, title, is_published) in [
            ("11111111-1111-4111-8111-111111111111", "First post", true),
            ("22222222-2222-4222-8222-222222222222", "Second post", false),
            ("33333333-3333-4333-8333-333333333333", "Third post", false),
        ] {
            let response = get_preview(&runtime, post_id).await;
            assert_eq!(response.status(), StatusCode::OK, "{post_id}");
            assert_eq!(
                response.headers().get(CACHE_CONTROL).unwrap(),
                &PREVIEW_CACHE_POLICY
            );
            assert_eq!(
                response.headers().get(CONTENT_TYPE).unwrap(),
                "text/html; charset=utf-8"
            );
            assert_eq!(
                response.headers().get("x-content-type-options").unwrap(),
                &NOSNIFF
            );
            let sandbox = response
                .headers()
                .get(CONTENT_SECURITY_POLICY)
                .unwrap()
                .to_str()
                .unwrap();
            assert_eq!(sandbox.split(';').next(), Some("sandbox allow-same-origin"));
            assert!(sandbox.contains("default-src 'none'"));
            assert!(sandbox.contains("script-src 'none'"));
            assert!(sandbox.contains("connect-src 'none'"));
            assert!(sandbox.contains("worker-src 'none'"));
            assert!(sandbox.contains("frame-src 'none'"));
            assert!(sandbox.contains("form-action 'none'"));
            assert!(sandbox.contains("navigate-to 'none'"));
            assert!(response.headers().get("set-cookie").is_none());
            assert!(response.headers().get("x-request-id").is_some());
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let html = std::str::from_utf8(&body).unwrap();
            assert!(html.starts_with("<!DOCTYPE html>"), "{post_id}");
            assert!(html.contains("maincopy-site-header"), "{post_id}");
            assert!(html.contains(&format!("<h1>{title}</h1>")), "{post_id}");
            assert!(
                html.contains(&format!(
                    "{PREVIEW_ASSETS_PATH}/{}?path={PREVIEW_ASSET_PATH}",
                    ContentTreeDigest::from_bytes([0x44; 32])
                )),
                "{post_id}"
            );
            assert!(!html.contains("/assets/site-b3-v1-"), "{post_id}");
            assert_eq!(
                html.contains("class=\"publication-time\""),
                is_published,
                "{post_id}"
            );
        }

        let response = get_preview(&runtime, "44444444-4444-4444-8444-444444444444").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            &PREVIEW_CACHE_POLICY
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "post_not_found");
        runtime.stop().await;
    }

    #[tokio::test]
    async fn preview_api_uses_current_candidate_when_public_revision_is_older() {
        let public = Arc::new(posts_catalog());
        let published = public.rendered_posts().next().unwrap();
        let ledger = PublicLedgerProjection::try_from_exact_entries([PublishedPostRevision::new(
            published.document.metadata.id.clone(),
            published.revision.clone(),
            OffsetDateTime::UNIX_EPOCH,
        )])
        .unwrap();
        let candidate = Arc::new(posts_catalog_with_first_title("First post revised"));
        assert_ne!(
            public.rendered_posts().next().unwrap().revision,
            candidate.rendered_posts().next().unwrap().revision
        );
        let runtime = preview_router_with_catalog(candidate, public, ledger).await;

        let response = get_preview(&runtime, "11111111-1111-4111-8111-111111111111").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("<h1>First post revised</h1>"));
        assert!(html.contains("class=\"publication-time\""));
        assert!(!html.contains("<h1>First post</h1>"));
        runtime.stop().await;
    }

    #[tokio::test]
    async fn preview_assets_serve_exact_retained_candidate_bytes() {
        let public = Arc::new(posts_catalog_with_asset("First post", b"old public image"));
        let candidate = Arc::new(posts_catalog_with_asset(
            "First post candidate",
            CURRENT_PREVIEW_ASSET,
        ));
        let runtime = preview_runtime_with_catalog(
            candidate,
            Arc::clone(&public),
            PublicLedgerProjection::empty(),
        )
        .await;
        let old_digest = ContentTreeDigest::from_bytes([0x44; 32]).to_string();

        let response = get_preview_asset(&runtime, &old_digest, PREVIEW_ASSET_PATH).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get(CONTENT_TYPE).unwrap(), "image/png");
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            &PREVIEW_CACHE_POLICY
        );
        assert_eq!(
            response.headers().get("x-content-type-options").unwrap(),
            &NOSNIFF
        );
        assert!(response.headers().get("x-request-id").is_some());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), CURRENT_PREVIEW_ASSET);

        let new_asset = b"new synchronized preview image";
        let new_digest = ContentTreeDigest::from_bytes([0x55; 32]);
        let new_digest_string = new_digest.to_string();
        runtime.stop().await;
        let runtime = preview_runtime_with_catalog_and_digest(
            Arc::new(posts_catalog_with_asset(
                "First post synchronized",
                new_asset,
            )),
            public,
            PublicLedgerProjection::empty(),
            new_digest,
        )
        .await;

        let response = get_preview_asset(&runtime, &old_digest, PREVIEW_ASSET_PATH).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            &PREVIEW_CACHE_POLICY
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), b"old public image");

        let response = get_preview_asset(&runtime, &new_digest_string, PREVIEW_ASSET_PATH).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), new_asset);

        let missing = ContentTreeDigest::from_bytes([0x66; 32]).to_string();
        let response = get_preview_asset(&runtime, &missing, PREVIEW_ASSET_PATH).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "preview_candidate_unavailable");
        runtime.stop().await;
    }

    #[tokio::test]
    async fn preview_asset_route_forces_active_and_unknown_formats_to_download() {
        let digest = ContentTreeDigest::from_bytes([0x44; 32]).to_string();

        for (path, bytes) in [
            ("assets/active.html", b"<script>active</script>".as_slice()),
            ("assets/archive.bin", b"unclassified bytes".as_slice()),
        ] {
            let catalog = Arc::new(posts_catalog_with_named_asset("Asset preview", path, bytes));
            let runtime = preview_runtime_with_catalog(
                Arc::clone(&catalog),
                catalog,
                PublicLedgerProjection::empty(),
            )
            .await;
            let downloaded = get_preview_asset(&runtime, &digest, path).await;
            assert_eq!(downloaded.status(), StatusCode::OK, "{path}");
            assert_eq!(
                downloaded.headers().get(CONTENT_TYPE).unwrap(),
                "application/octet-stream",
                "{path}"
            );
            assert_eq!(
                downloaded.headers().get(CONTENT_DISPOSITION).unwrap(),
                &DOWNLOAD_ASSET,
                "{path}"
            );
            assert_eq!(
                downloaded.headers().get(CONTENT_SECURITY_POLICY).unwrap(),
                &ASSET_SANDBOX,
                "{path}"
            );
            assert_eq!(
                to_bytes(downloaded.into_body(), 16 * 1024)
                    .await
                    .unwrap()
                    .as_ref(),
                bytes,
                "{path}"
            );
            runtime.stop().await;
        }
    }

    #[tokio::test]
    async fn preview_asset_route_hides_unreferenced_candidate_files() {
        let archive_path = "assets/archive.bin";
        let catalog = Arc::new(posts_catalog_with_unreferenced_asset(
            "Archive preview",
            archive_path,
            b"unclassified bytes",
        ));
        let runtime = preview_runtime_with_catalog(
            Arc::clone(&catalog),
            catalog,
            PublicLedgerProjection::empty(),
        )
        .await;
        let digest = ContentTreeDigest::from_bytes([0x44; 32]).to_string();

        let missing = get_preview_asset(&runtime, &digest, "assets/unreferenced.bin").await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response_problem_code(missing).await,
            "preview_asset_not_found"
        );
        runtime.stop().await;
    }

    #[test]
    fn idempotency_key_requires_one_canonical_uuid() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            idempotency_key(&headers).unwrap_err().code,
            "missing_idempotency_key"
        );
        headers.insert(IDEMPOTENCY_KEY_HEADER, KEY.parse().unwrap());
        assert_eq!(idempotency_key(&headers).unwrap().to_string(), KEY);
        for invalid in [
            "67E55044-10B1-426F-9247-BB680E5FE0C8",
            "67e5504410b1426f9247bb680e5fe0c8",
            "not-a-uuid",
        ] {
            headers.insert(IDEMPOTENCY_KEY_HEADER, invalid.parse().unwrap());
            assert_eq!(
                idempotency_key(&headers).unwrap_err().code,
                "invalid_idempotency_key"
            );
        }
        headers.insert(IDEMPOTENCY_KEY_HEADER, KEY.parse().unwrap());
        headers.append(
            IDEMPOTENCY_KEY_HEADER,
            Uuid::new_v4().to_string().parse().unwrap(),
        );
        assert_eq!(
            idempotency_key(&headers).unwrap_err().code,
            "invalid_idempotency_key"
        );
    }

    #[test]
    fn typed_activation_failures_have_stable_http_statuses_and_codes() {
        let post_id = PostId::parse("11111111-1111-4111-8111-111111111111").unwrap();
        let digest = PostRevisionDigest::from_bytes([0x11; 32]);
        let cases = [
            (
                PublicationActivationError::PostNotFound {
                    post_id: post_id.clone(),
                },
                StatusCode::NOT_FOUND,
                "post_not_found",
            ),
            (
                PublicationActivationError::DraftPost {
                    post_id: post_id.clone(),
                },
                StatusCode::CONFLICT,
                "post_is_draft",
            ),
            (
                PublicationActivationError::StaleRevision {
                    post_id,
                    expected: Box::new(digest.clone()),
                    current: Box::new(digest),
                },
                StatusCode::PRECONDITION_FAILED,
                "stale_revision",
            ),
            (
                PublicationActivationError::StalePreview {
                    accepted: PreviewDigest::from_bytes([0x22; 32]),
                    current: PreviewDigest::from_bytes([0x33; 32]),
                },
                StatusCode::PRECONDITION_FAILED,
                "stale_preview",
            ),
            (
                PublicationActivationError::StaleReview(PublishReviewError::Content {
                    reviewed: Box::new(ContentTreeDigest::from_bytes([0x44; 32])),
                    current: Box::new(ContentTreeDigest::from_bytes([0x55; 32])),
                }),
                StatusCode::PRECONDITION_FAILED,
                "stale_review",
            ),
            (
                PublicationActivationError::Database(DatabaseMutationError::Admission(
                    DatabaseAdmissionError::QueueFull,
                )),
                StatusCode::SERVICE_UNAVAILABLE,
                "publication_unavailable",
            ),
            (
                PublicationActivationError::Database(DatabaseMutationError::Command(
                    DatabaseCommandError::IdempotencyConflict,
                )),
                StatusCode::CONFLICT,
                "idempotency_conflict",
            ),
            (
                PublicationActivationError::Lookup(PublishNowLookupError::IdempotencyConflict),
                StatusCode::CONFLICT,
                "idempotency_conflict",
            ),
            (
                PublicationActivationError::Lookup(PublishNowLookupError::InvalidStoredState),
                StatusCode::SERVICE_UNAVAILABLE,
                "publication_unavailable",
            ),
            (
                PublicationActivationError::ScheduleLookup(
                    SchedulePublicationLookupError::IdempotencyConflict,
                ),
                StatusCode::CONFLICT,
                "idempotency_conflict",
            ),
            (
                PublicationActivationError::ScheduleLookup(
                    SchedulePublicationLookupError::InvalidStoredState,
                ),
                StatusCode::SERVICE_UNAVAILABLE,
                "publication_unavailable",
            ),
            (
                PublicationActivationError::RouteOwnership(
                    PublicationRouteOwnershipError::Conflict {
                        route: PublicationRoute::Alias(PostAlias::parse("claimed-route").unwrap()),
                    },
                ),
                StatusCode::CONFLICT,
                "publication_conflict",
            ),
            (
                PublicationActivationError::RouteOwnership(
                    PublicationRouteOwnershipError::InvalidRouteSet(
                        PublicationRouteSetError::TooManyAliases {
                            count: MAX_PUBLIC_ROUTES,
                            maximum: MAX_PUBLIC_ROUTES - 1,
                        },
                    ),
                ),
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid_publication_state",
            ),
        ];

        for (error, status, code) in cases {
            let spec = activation_error(&error);
            assert_eq!(spec.status, status);
            assert_eq!(spec.code, code);
        }
    }
}
