use std::sync::Arc;

use axum::{
    Extension, Json,
    body::{Body, Bytes},
    extract::{
        DefaultBodyLimit, Path, Query,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE, LINK},
    },
    response::{Html, IntoResponse as _, Response},
};
use maincopy_shared::posts::{ListPostsResponse, PostPublicationState, PostSummary};
use maincopy_shared::publication::{
    CONTENT_DIGEST_HEADER, IDEMPOTENCY_KEY_HEADER, POST_REVISION_HEADER, PREVIEW_DIGEST_HEADER,
    PublicationApprovalState, PublishNowRequest, PublishNowResponse,
};
use serde::{Deserialize, Serialize};
use time::UtcOffset;
use utoipa::ToSchema;
use utoipa_axum::{
    router::{UtoipaMethodRouter, UtoipaMethodRouterExt as _},
    routes,
};
use uuid::Uuid;

use crate::{
    admin::request_id::RequestId,
    content::{
        ContentTreeDigest, DraftStatus, LogicalAssetPath, PostId, PostRevisionDigest, PreviewDigest,
    },
    database::store::{DatabaseAdmissionError, DatabaseCommandError, DatabaseMutationError},
    render::{ContentCatalog, render_bound_post_preview},
};

use super::{
    PublicLedgerProjection,
    activation::{
        PublicationActivationError, PublicationCoordinatorHandle, PublishNow, Schedule,
        ScheduledApprovalOutcome,
    },
    store::{PublishNowLookupError, SchedulePublicationLookupError, SiteHead},
};

const MAX_PUBLICATION_REQUEST_BYTES: usize = 4 * 1024;
const DEFAULT_POST_PAGE_LIMIT: u16 = 50;
const MAX_POST_PAGE_LIMIT: u16 = 100;
const PREVIEW_ASSETS_PATH: &str = "/api/admin/v1/preview-assets";
const PREVIEW_CACHE_POLICY: HeaderValue = HeaderValue::from_static("private, no-store");
const NOSNIFF: HeaderValue = HeaderValue::from_static("nosniff");

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
        (status = BAD_REQUEST, description = "Pagination parameters are invalid", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = SERVICE_UNAVAILABLE, description = "Publication state is unavailable", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID")))
    ),
    tag = "Posts"
)]
async fn list_posts(
    Extension(request_id): Extension<RequestId>,
    coordinator: Option<Extension<PublicationCoordinatorHandle>>,
    query: Result<Query<ListPostsQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => {
            return problem(
                ErrorSpec::bad_request(
                    "invalid_posts_query",
                    "cursor and limit must use valid pagination values",
                ),
                request_id,
            );
        }
    };
    let limit = query.limit.unwrap_or(DEFAULT_POST_PAGE_LIMIT);
    if !(1..=MAX_POST_PAGE_LIMIT).contains(&limit) {
        return problem(
            ErrorSpec::bad_request("invalid_posts_limit", "limit must be between 1 and 100"),
            request_id,
        );
    }
    let Some(Extension(coordinator)) = coordinator else {
        return problem(publication_unavailable(), request_id);
    };
    let coordinator = coordinator.read();
    Json(posts_page(
        &coordinator.catalog,
        &coordinator.content_digest,
        &coordinator.ledger,
        &coordinator.site,
        query.cursor,
        usize::from(limit),
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
        (status = BAD_REQUEST, description = "The post UUID is invalid", body = PublicationErrorEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            )),
        (status = NOT_FOUND, description = "The post is not present in the current candidate catalog", body = PublicationErrorEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            )),
        (status = INTERNAL_SERVER_ERROR, description = "The candidate preview could not be rendered", body = PublicationErrorEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            )),
        (status = SERVICE_UNAVAILABLE, description = "Candidate publication state is unavailable", body = PublicationErrorEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            ))
    ),
    tag = "Posts"
)]
async fn get_post_preview(
    Extension(request_id): Extension<RequestId>,
    coordinator: Option<Extension<PublicationCoordinatorHandle>>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<PostPreviewQuery>, QueryRejection>,
) -> Response {
    let Path(encoded_post_id) = match path {
        Ok(path) => path,
        Err(_) => {
            return preview_problem(
                ErrorSpec::bad_request(
                    "invalid_post_id",
                    "post_id must be a canonical lowercase UUID",
                ),
                request_id,
            );
        }
    };
    let parsed = Uuid::parse_str(&encoded_post_id).ok();
    let Some(post_id) =
        parsed.filter(|post_id| post_id.hyphenated().to_string() == encoded_post_id)
    else {
        return preview_problem(
            ErrorSpec::bad_request(
                "invalid_post_id",
                "post_id must be a canonical lowercase UUID",
            ),
            request_id,
        );
    };
    let post_id = PostId::parse(&post_id.hyphenated().to_string())
        .expect("a UUID has one canonical lowercase hyphenated representation");
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => {
            return preview_problem(
                ErrorSpec::bad_request(
                    "invalid_preview_query",
                    "revision and content_digest must use valid preview preconditions",
                ),
                request_id,
            );
        }
    };
    let expected_revision = match query
        .revision
        .as_deref()
        .map(PostRevisionDigest::parse)
        .transpose()
    {
        Ok(revision) => revision,
        Err(_) => {
            return preview_problem(
                ErrorSpec::bad_request(
                    "invalid_preview_revision",
                    "revision must be a complete Maincopy post revision digest",
                ),
                request_id,
            );
        }
    };
    let expected_content_digest = match query
        .content_digest
        .as_deref()
        .map(ContentTreeDigest::parse)
        .transpose()
    {
        Ok(digest) => digest,
        Err(_) => {
            return preview_problem(
                ErrorSpec::bad_request(
                    "invalid_preview_content_digest",
                    "content_digest must be a complete Maincopy content digest",
                ),
                request_id,
            );
        }
    };
    let Some(Extension(coordinator)) = coordinator else {
        return preview_problem(publication_unavailable(), request_id);
    };
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
        &preview_asset_endpoint,
        published_at,
    ) {
        Ok(Some(preview)) => {
            let preview_digest = HeaderValue::from_str(preview.digest.as_str())
                .expect("a typed preview digest is a valid header value");
            let revision = HeaderValue::from_str(preview.revision.as_str())
                .expect("a typed post revision is a valid header value");
            let content_digest = HeaderValue::from_str(&content_digest.to_string())
                .expect("a typed content digest is a valid header value");
            let canonical =
                HeaderValue::from_str(&format!("<{}>; rel=\"canonical\"", preview.canonical_url))
                    .expect("a validated canonical URL is a valid link header");
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
        (status = OK, description = "Exact authored or renderer-generated bytes from the current candidate", body = [u8],
            content_type = "application/octet-stream",
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID"),
                ("x-content-type-options" = String, description = "Always nosniff")
            )),
        (status = BAD_REQUEST, description = "The asset query is invalid", body = PublicationErrorEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            )),
        (status = NOT_FOUND, description = "The candidate namespace is stale or the asset is absent", body = PublicationErrorEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            )),
        (status = SERVICE_UNAVAILABLE, description = "Candidate publication state is unavailable", body = PublicationErrorEnvelope,
            headers(
                ("cache-control" = String, description = "Always private, no-store"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            ))
    ),
    tag = "Posts"
)]
async fn get_preview_asset(
    Extension(request_id): Extension<RequestId>,
    coordinator: Option<Extension<PublicationCoordinatorHandle>>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<PreviewAssetQuery>, QueryRejection>,
) -> Response {
    let Path(content_digest) = match path {
        Ok(path) => path,
        Err(_) => {
            return preview_problem(
                ErrorSpec::bad_request(
                    "invalid_preview_asset_namespace",
                    "the preview asset namespace must include a content digest",
                ),
                request_id,
            );
        }
    };
    let Ok(content_digest) = ContentTreeDigest::parse(&content_digest) else {
        return preview_problem(
            ErrorSpec::bad_request(
                "invalid_preview_asset_namespace",
                "the preview asset namespace must be a complete Maincopy content digest",
            ),
            request_id,
        );
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => {
            return preview_problem(
                ErrorSpec::bad_request(
                    "invalid_preview_asset_query",
                    "the preview asset request must contain one logical path",
                ),
                request_id,
            );
        }
    };
    let Ok(asset_path) = LogicalAssetPath::parse(&query.path) else {
        return preview_problem(
            ErrorSpec::bad_request(
                "invalid_preview_asset_path",
                "the preview asset path must be a portable path below assets/",
            ),
            request_id,
        );
    };
    let Some(Extension(coordinator)) = coordinator else {
        return preview_problem(publication_unavailable(), request_id);
    };
    let bytes = {
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
        let Some(bytes) = catalog.current_preview_asset(&asset_path) else {
            return preview_problem(
                ErrorSpec::new(
                    StatusCode::NOT_FOUND,
                    "preview_asset_not_found",
                    "the preview asset is not present in the current content candidate",
                ),
                request_id,
            );
        };
        bytes
    };

    let mut response = Response::new(Body::from(Bytes::from_owner(bytes)));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(preview_asset_content_type(&asset_path)),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, PREVIEW_CACHE_POLICY);
    response
        .headers_mut()
        .insert("x-content-type-options", NOSNIFF);
    response
}

fn preview_asset_content_type(path: &LogicalAssetPath) -> &'static str {
    let extension = path
        .as_str()
        .rsplit_once('.')
        .map_or("", |(_, extension)| extension);
    if extension.eq_ignore_ascii_case("png") {
        "image/png"
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        "image/jpeg"
    } else if extension.eq_ignore_ascii_case("gif") {
        "image/gif"
    } else if extension.eq_ignore_ascii_case("webp") {
        "image/webp"
    } else if extension.eq_ignore_ascii_case("avif") {
        "image/avif"
    } else if extension.eq_ignore_ascii_case("ico") {
        "image/x-icon"
    } else if extension.eq_ignore_ascii_case("svg") {
        "image/svg+xml"
    } else if extension.eq_ignore_ascii_case("pdf") {
        "application/pdf"
    } else if extension.eq_ignore_ascii_case("mp4") {
        "video/mp4"
    } else if extension.eq_ignore_ascii_case("webm") {
        "video/webm"
    } else if extension.eq_ignore_ascii_case("mp3") {
        "audio/mpeg"
    } else if extension.eq_ignore_ascii_case("wav") {
        "audio/wav"
    } else if extension.eq_ignore_ascii_case("ogg") {
        "audio/ogg"
    } else if extension.eq_ignore_ascii_case("woff") {
        "font/woff"
    } else if extension.eq_ignore_ascii_case("woff2") {
        "font/woff2"
    } else {
        "application/octet-stream"
    }
}

fn posts_page(
    catalog: &ContentCatalog,
    content_digest: &crate::content::ContentTreeDigest,
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
        (status = BAD_REQUEST, description = "The command header or revision is invalid", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = NOT_FOUND, description = "The selected post does not exist", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = CONFLICT, description = "The command conflicts with publication state", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = PRECONDITION_FAILED, description = "The selected post revision is stale", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = PAYLOAD_TOO_LARGE, description = "The request body exceeds 4096 bytes", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = INTERNAL_SERVER_ERROR, description = "Publication snapshot construction failed", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = SERVICE_UNAVAILABLE, description = "Publication state is unavailable or outcome is uncertain", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID")))
    ),
    tag = "Publications"
)]
pub(crate) async fn create_publication(
    headers: HeaderMap,
    Extension(request_id): Extension<RequestId>,
    Extension(coordinator): Extension<PublicationCoordinatorHandle>,
    body: Result<Json<PublishNowRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return problem(
                ErrorSpec::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_body_too_large",
                    "the request body must not exceed 4096 bytes",
                ),
                request_id,
            );
        }
        Err(_) => {
            return problem(
                ErrorSpec::bad_request(
                    "invalid_request_body",
                    "the request body must be valid publication JSON",
                ),
                request_id,
            );
        }
    };
    let creation_key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(spec) => return problem(spec, request_id),
    };
    let expected_revision = match request
        .expected_revision
        .as_deref()
        .map(PostRevisionDigest::parse)
        .transpose()
    {
        Ok(revision) => revision,
        Err(_) => {
            return problem(
                ErrorSpec::bad_request(
                    "invalid_expected_revision",
                    "expected_revision must be a complete Maincopy post revision digest",
                ),
                request_id,
            );
        }
    };
    let accepted_preview_digest = PreviewDigest::parse(request.preview_digest.as_str())
        .expect("the shared publication contract validates preview digests during JSON parsing");
    let stable_post_id = PostId::parse(&request.post_id.hyphenated().to_string())
        .expect("a UUID has one canonical lowercase hyphenated representation");
    let publication_id = Uuid::new_v4();
    if let Some(scheduled_at) = request.scheduled_for {
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

fn activation_error(error: &PublicationActivationError) -> ErrorSpec {
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
        PublicationActivationError::AlreadyPublished { .. } => ErrorSpec::conflict(
            "already_published",
            "the selected post is already published",
        ),
        PublicationActivationError::ScheduleNotFuture { .. } => ErrorSpec::bad_request(
            "schedule_not_future",
            "scheduled_for must be later than the current server time",
        ),
        PublicationActivationError::ScheduledPublicationUnavailable { .. } => {
            publication_unavailable()
        }
        PublicationActivationError::Database(DatabaseMutationError::Admission(
            DatabaseAdmissionError::QueueFull | DatabaseAdmissionError::WriterClosed,
        ))
        | PublicationActivationError::Database(DatabaseMutationError::Command(
            DatabaseCommandError::OutcomeUnknown,
        ))
        | PublicationActivationError::DurableStateMismatch
        | PublicationActivationError::SnapshotActivationConflict
        | PublicationActivationError::CandidateDigestMismatch { .. } => publication_unavailable(),
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
        ) => ErrorSpec::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "publication_unavailable",
            "publication is temporarily unavailable",
        ),
        PublicationActivationError::Database(DatabaseMutationError::Command(
            DatabaseCommandError::Rejected,
        )) => ErrorSpec::conflict(
            "publication_conflict",
            "the command conflicts with current publication state",
        ),
        PublicationActivationError::Database(DatabaseMutationError::Command(
            DatabaseCommandError::InvalidValue,
        )) => ErrorSpec::internal(
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
    ErrorSpec::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "publication_unavailable",
        "publication is temporarily unavailable",
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ErrorSpec {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl ErrorSpec {
    const fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }

    const fn bad_request(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    const fn conflict(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    const fn internal(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, code, message)
    }
}

fn problem(spec: ErrorSpec, request_id: RequestId) -> Response {
    (
        spec.status,
        Json(PublicationErrorEnvelope {
            error: PublicationErrorBody {
                code: spec.code,
                message: spec.message,
                request_id: request_id.to_string().into_boxed_str(),
            },
        }),
    )
        .into_response()
}

fn preview_problem(spec: ErrorSpec, request_id: RequestId) -> Response {
    let mut response = problem(spec, request_id);
    response
        .headers_mut()
        .insert(CACHE_CONTROL, PREVIEW_CACHE_POLICY);
    response
}

#[derive(Serialize, ToSchema)]
struct PublicationErrorEnvelope {
    error: PublicationErrorBody,
}

#[derive(Serialize, ToSchema)]
struct PublicationErrorBody {
    code: &'static str,
    message: &'static str,
    request_id: Box<str>,
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, header::CONTENT_TYPE},
    };
    use sqlx::sqlite::SqlitePoolOptions;
    use time::OffsetDateTime;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt as _;

    use super::*;
    use crate::{
        admin::runtime_admin_router,
        content::{
            ContentTreeDigest, DiscoveredContentTree, LogicalAssetPath, PostCollection,
            PublishedPostRevision, SiteSnapshotDigest, resolve_content_assets,
            tree::{asset, post, publication},
        },
        domain::publication::{activation::PublicationCoordinator, store::PublicationStore},
        frontend_assets::embedded_manifest,
        render::{build_site_snapshot, compile_content_catalog, render_site_shell, snapshot_store},
        web::Readiness,
    };

    const KEY: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";
    const PREVIEW_ASSET_PATH: &str = "assets/preview.png";
    const CURRENT_PREVIEW_ASSET: &[u8] = b"current preview image";

    fn posts_catalog() -> ContentCatalog {
        posts_catalog_with_asset("First post", CURRENT_PREVIEW_ASSET)
    }

    fn posts_catalog_with_first_title(first_title: &str) -> ContentCatalog {
        posts_catalog_with_asset(first_title, CURRENT_PREVIEW_ASSET)
    }

    fn posts_catalog_with_asset(first_title: &str, asset_bytes: &[u8]) -> ContentCatalog {
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
                     ![preview]({PREVIEW_ASSET_PATH})\n"
                ),
            )
        })
        .collect();
        let tree = DiscoveredContentTree::new(
            publication,
            posts,
            vec![asset(
                LogicalAssetPath::parse(PREVIEW_ASSET_PATH).unwrap(),
                asset_bytes.to_vec(),
            )],
            0,
        );
        let content = tree.validate().unwrap();
        let assets = resolve_content_assets(&tree, &content).unwrap();
        compile_content_catalog(&content, &assets).unwrap()
    }

    fn preview_router(ledger: PublicLedgerProjection) -> axum::Router {
        let catalog = Arc::new(posts_catalog());
        preview_router_with_catalog(Arc::clone(&catalog), catalog, ledger)
    }

    fn preview_router_with_catalog(
        candidate: Arc<ContentCatalog>,
        public: Arc<ContentCatalog>,
        ledger: PublicLedgerProjection,
    ) -> axum::Router {
        let (router, _coordinator, _actor_task) =
            preview_runtime_with_catalog(candidate, public, ledger);
        router
    }

    fn preview_runtime_with_catalog(
        candidate: Arc<ContentCatalog>,
        public: Arc<ContentCatalog>,
        ledger: PublicLedgerProjection,
    ) -> (
        axum::Router,
        PublicationCoordinatorHandle,
        tokio::task::JoinHandle<()>,
    ) {
        preview_runtime_with_catalog_and_digest(
            candidate,
            public,
            ledger,
            ContentTreeDigest::from_bytes([0x44; 32]),
        )
    }

    fn preview_runtime_with_catalog_and_digest(
        candidate: Arc<ContentCatalog>,
        public: Arc<ContentCatalog>,
        ledger: PublicLedgerProjection,
        content_digest: ContentTreeDigest,
    ) -> (
        axum::Router,
        PublicationCoordinatorHandle,
        tokio::task::JoinHandle<()>,
    ) {
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
            store: PublicationStore::new(readers, mutations),
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: std::collections::BTreeMap::new(),
            scheduler_wakeup: Arc::new(tokio::sync::Notify::new()),
            readiness: Readiness::new(true),
            cancellation: CancellationToken::new(),
        };
        let (coordinator, actor) = coordinator.into_actor(1);
        let actor_task = tokio::spawn(async move {
            let _ = actor.run(CancellationToken::new()).await;
        });
        (
            runtime_admin_router(coordinator.clone()),
            coordinator,
            actor_task,
        )
    }

    async fn get_preview(router: axum::Router, post_id: &str) -> Response {
        router
            .oneshot(
                Request::builder()
                    .uri(format!("/api/admin/v1/posts/{post_id}/preview"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn get_preview_asset(router: axum::Router, digest: &str, path: &str) -> Response {
        router
            .oneshot(
                Request::builder()
                    .uri(format!("{PREVIEW_ASSETS_PATH}/{digest}?path={path}"))
                    .body(Body::empty())
                    .unwrap(),
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
        let router = preview_router(ledger);

        for (post_id, title, is_published) in [
            ("11111111-1111-4111-8111-111111111111", "First post", true),
            ("22222222-2222-4222-8222-222222222222", "Second post", false),
            ("33333333-3333-4333-8333-333333333333", "Third post", false),
        ] {
            let response = get_preview(router.clone(), post_id).await;
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
            assert!(response.headers().get("x-request-id").is_some());
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let html = std::str::from_utf8(&body).unwrap();
            assert!(html.starts_with("<!DOCTYPE html>"), "{post_id}");
            assert!(html.contains("class=\"site-header\""), "{post_id}");
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

        let response = get_preview(router, "44444444-4444-4444-8444-444444444444").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            &PREVIEW_CACHE_POLICY
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "post_not_found");
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
        let router = preview_router_with_catalog(candidate, public, ledger);

        let response = get_preview(router, "11111111-1111-4111-8111-111111111111").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("<h1>First post revised</h1>"));
        assert!(html.contains("class=\"publication-time\""));
        assert!(!html.contains("<h1>First post</h1>"));
    }

    #[tokio::test]
    async fn preview_assets_serve_exact_retained_candidate_bytes() {
        let public = Arc::new(posts_catalog_with_asset("First post", b"old public image"));
        let candidate = Arc::new(posts_catalog_with_asset(
            "First post candidate",
            CURRENT_PREVIEW_ASSET,
        ));
        let (router, _coordinator, _actor_task) = preview_runtime_with_catalog(
            candidate,
            Arc::clone(&public),
            PublicLedgerProjection::empty(),
        );
        let old_digest = ContentTreeDigest::from_bytes([0x44; 32]).to_string();

        let response = get_preview_asset(router.clone(), &old_digest, PREVIEW_ASSET_PATH).await;
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
        let (router, _coordinator, _actor_task) = preview_runtime_with_catalog_and_digest(
            Arc::new(posts_catalog_with_asset(
                "First post synchronized",
                new_asset,
            )),
            public,
            PublicLedgerProjection::empty(),
            new_digest,
        );

        let response = get_preview_asset(router.clone(), &old_digest, PREVIEW_ASSET_PATH).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            &PREVIEW_CACHE_POLICY
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), b"old public image");

        let response =
            get_preview_asset(router.clone(), &new_digest_string, PREVIEW_ASSET_PATH).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), new_asset);

        let missing = ContentTreeDigest::from_bytes([0x66; 32]).to_string();
        let response = get_preview_asset(router, &missing, PREVIEW_ASSET_PATH).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "preview_candidate_unavailable");
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
        ];

        for (error, status, code) in cases {
            let spec = activation_error(&error);
            assert_eq!(spec.status, status);
            assert_eq!(spec.code, code);
        }
    }
}
