use std::{str::FromStr, sync::Arc};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, State, rejection::PathRejection},
    http::{
        HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE, ETAG},
    },
    response::Response,
    routing::get,
};

use crate::{
    content::{PostSlug, PostTag},
    frontend_assets::{FrontendAssetName, FrontendBundleDigest, IMMUTABLE_CACHE_CONTROL},
    render::{SiteSnapshot, SiteSnapshotReader},
};

const HTML_CONTENT_TYPE: HeaderValue = HeaderValue::from_static("text/html; charset=utf-8");
const HTML_CACHE_POLICY: HeaderValue = HeaderValue::from_static("no-cache");
const NOSNIFF: HeaderValue = HeaderValue::from_static("nosniff");

/// Builds the publication-specific portion of the public HTTP API.
pub(crate) fn router(snapshots: SiteSnapshotReader) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/posts/{slug}", get(post))
        .route("/tags/{tag}", get(tag))
        .route("/archive", get(archive))
        .route("/app-assets/{digest}/{name}", get(application_asset))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(snapshots)
}

async fn index(State(snapshots): State<SiteSnapshotReader>) -> Response {
    let snapshot = snapshots.load_full();
    html_response(StatusCode::OK, snapshot.index_page())
}

async fn post(
    State(snapshots): State<SiteSnapshotReader>,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let snapshot = snapshots.load_full();
    let Ok(Path(value)) = path else {
        return not_found_response(&snapshot);
    };
    let Ok(slug) = PostSlug::parse(value) else {
        return not_found_response(&snapshot);
    };
    snapshot.post_page(&slug).map_or_else(
        || not_found_response(&snapshot),
        |page| html_response(StatusCode::OK, page),
    )
}

async fn tag(
    State(snapshots): State<SiteSnapshotReader>,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let snapshot = snapshots.load_full();
    let Ok(Path(value)) = path else {
        return not_found_response(&snapshot);
    };
    let Ok(tag) = PostTag::parse(value.clone()) else {
        return not_found_response(&snapshot);
    };
    if tag.as_str() != value {
        return not_found_response(&snapshot);
    }
    snapshot.tag_page(&tag).map_or_else(
        || not_found_response(&snapshot),
        |page| html_response(StatusCode::OK, page),
    )
}

async fn archive(State(snapshots): State<SiteSnapshotReader>) -> Response {
    let snapshot = snapshots.load_full();
    html_response(StatusCode::OK, snapshot.archive_page())
}

async fn application_asset(
    State(snapshots): State<SiteSnapshotReader>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let snapshot = snapshots.load_full();
    let Ok(Path((digest, name))) = path else {
        return not_found_response(&snapshot);
    };
    let Ok(digest) = FrontendBundleDigest::from_str(&digest) else {
        return not_found_response(&snapshot);
    };
    let Ok(name) = FrontendAssetName::parse(&name) else {
        return not_found_response(&snapshot);
    };
    let manifest = snapshot.frontend;
    let Some(asset) = manifest.lookup(&digest, name) else {
        return not_found_response(&snapshot);
    };

    let Ok(etag) = HeaderValue::from_str(&asset.etag()) else {
        return internal_error_response();
    };
    let mut response = Response::new(Body::from(asset.bytes));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(asset.mime()));
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL),
    );
    headers.insert(ETAG, etag);
    headers.insert("x-content-type-options", NOSNIFF);
    response
}

async fn not_found(State(snapshots): State<SiteSnapshotReader>) -> Response {
    let snapshot = snapshots.load_full();
    not_found_response(&snapshot)
}

async fn method_not_allowed(State(snapshots): State<SiteSnapshotReader>) -> Response {
    let snapshot = snapshots.load_full();
    html_response(
        StatusCode::METHOD_NOT_ALLOWED,
        snapshot.method_not_allowed_page(),
    )
}

fn not_found_response(snapshot: &SiteSnapshot) -> Response {
    html_response(StatusCode::NOT_FOUND, snapshot.not_found_page())
}

struct HtmlBodyOwner(Arc<str>);

impl AsRef<[u8]> for HtmlBodyOwner {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

fn html_response(status: StatusCode, html: Arc<str>) -> Response {
    let mut response = Response::new(Body::from(Bytes::from_owner(HtmlBodyOwner(html))));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HTML_CONTENT_TYPE);
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HTML_CACHE_POLICY);
    response
}

fn internal_error_response() -> Response {
    let mut response = Response::new(Body::from("internal server error"));
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

#[cfg(test)]
mod tests {
    use std::{path::Path as FilePath, sync::Arc};

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use time::OffsetDateTime;
    use tower::ServiceExt;

    use super::*;
    use crate::{
        content::{
            ContentTreeLimits, PublishedPostRevision, discover_content_tree, resolve_content_assets,
        },
        frontend_assets::embedded_manifest,
        render::{
            PublicLedgerProjection, SiteSnapshotReader, build_site_snapshot,
            compile_content_catalog, render_markdown, render_site_shell,
        },
        web::{PublicState, Readiness, public_router},
    };

    fn published_example_state() -> PublicState {
        let root = FilePath::new(env!("CARGO_MANIFEST_DIR")).join("examples/content");
        let tree = discover_content_tree(&root, ContentTreeLimits::default()).unwrap();
        let content = tree.validate().unwrap();
        let assets = resolve_content_assets(&tree, &content).unwrap();
        let document = content.posts.first().unwrap();
        let rendered = render_markdown(
            document,
            assets.assets_for(document).unwrap(),
            assets.site_assets_for(&content.publication).unwrap(),
        )
        .unwrap();
        let ledger = PublicLedgerProjection::try_from_exact_entries([PublishedPostRevision::new(
            rendered.document.metadata.id.clone(),
            rendered.revision.clone(),
            OffsetDateTime::from_unix_timestamp(2_000).unwrap(),
        )])
        .unwrap();
        let catalog = Arc::new(compile_content_catalog(&content, &assets).unwrap());
        let shell = render_site_shell(catalog, embedded_manifest(), &ledger).unwrap();
        let snapshot = build_site_snapshot(shell, &ledger).unwrap();
        PublicState {
            snapshots: SiteSnapshotReader::from_snapshot(snapshot),
            readiness: Readiness::new(true),
        }
    }

    async fn get(path: &str) -> Response {
        public_router(published_example_state())
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn selected_post_route_uses_the_injected_ledger_projection() {
        let response = get("/posts/hello-maincopy").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("Hello, Maincopy"));
        assert!(body.contains("1970-01-01 0:33:20.0 +00:00:00"));

        assert_eq!(
            get("/posts/not-published").await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get("/tags/not-published").await.status(),
            StatusCode::NOT_FOUND
        );
    }
}
