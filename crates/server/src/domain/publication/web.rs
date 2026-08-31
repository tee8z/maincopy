use std::{convert::Infallible, str::FromStr, sync::Arc};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{FromRequestParts, Path},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH},
        request::Parts,
    },
    response::Response,
    routing::get,
};
use markdown_compiler::{PostSlug, PostTag};
use serde::de::DeserializeOwned;

use crate::{
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

/// One coherent view of the active rendering and the request validators.
struct PublicRequest {
    snapshot: Arc<SiteSnapshot>,
    headers: HeaderMap,
}

impl FromRequestParts<SiteSnapshotReader> for PublicRequest {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        snapshots: &SiteSnapshotReader,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self {
            snapshot: snapshots.load_full(),
            headers: parts.headers.clone(),
        })
    }
}

/// Snapshot-bound public path that turns Axum path failures into the site's 404.
struct PublicPath<T> {
    snapshot: Arc<SiteSnapshot>,
    headers: HeaderMap,
    value: T,
}

impl<T> FromRequestParts<SiteSnapshotReader> for PublicPath<T>
where
    T: DeserializeOwned + Send,
{
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        snapshots: &SiteSnapshotReader,
    ) -> Result<Self, Self::Rejection> {
        let snapshot = snapshots.load_full();
        let headers = parts.headers.clone();
        let Path(value) = Path::<T>::from_request_parts(parts, snapshots)
            .await
            .map_err(|_| not_found_response(&snapshot, &headers))?;
        Ok(Self {
            snapshot,
            headers,
            value,
        })
    }
}

async fn index(PublicRequest { snapshot, headers }: PublicRequest) -> Response {
    html_response(StatusCode::OK, snapshot.index_page(), &snapshot, &headers)
}

async fn post(
    PublicPath {
        snapshot,
        headers,
        value,
    }: PublicPath<String>,
) -> Response {
    let Ok(slug) = PostSlug::parse(value) else {
        return not_found_response(&snapshot, &headers);
    };
    snapshot.post_page(&slug).map_or_else(
        || not_found_response(&snapshot, &headers),
        |page| html_response(StatusCode::OK, page, &snapshot, &headers),
    )
}

async fn tag(
    PublicPath {
        snapshot,
        headers,
        value,
    }: PublicPath<String>,
) -> Response {
    let Ok(tag) = PostTag::parse(value.clone()) else {
        return not_found_response(&snapshot, &headers);
    };
    if tag.as_str() != value {
        return not_found_response(&snapshot, &headers);
    }
    snapshot.tag_page(&tag).map_or_else(
        || not_found_response(&snapshot, &headers),
        |page| html_response(StatusCode::OK, page, &snapshot, &headers),
    )
}

async fn archive(PublicRequest { snapshot, headers }: PublicRequest) -> Response {
    html_response(StatusCode::OK, snapshot.archive_page(), &snapshot, &headers)
}

async fn application_asset(
    PublicPath {
        snapshot,
        headers,
        value: (digest, name),
    }: PublicPath<(String, String)>,
) -> Response {
    let Ok(digest) = FrontendBundleDigest::from_str(&digest) else {
        return not_found_response(&snapshot, &headers);
    };
    let Ok(name) = FrontendAssetName::parse(&name) else {
        return not_found_response(&snapshot, &headers);
    };
    let manifest = snapshot.frontend;
    let Some(asset) = manifest.lookup(&digest, name) else {
        return not_found_response(&snapshot, &headers);
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

async fn not_found(PublicRequest { snapshot, headers }: PublicRequest) -> Response {
    not_found_response(&snapshot, &headers)
}

async fn method_not_allowed(PublicRequest { snapshot, headers }: PublicRequest) -> Response {
    html_response(
        StatusCode::METHOD_NOT_ALLOWED,
        snapshot.method_not_allowed_page(),
        &snapshot,
        &headers,
    )
}

fn not_found_response(snapshot: &SiteSnapshot, headers: &HeaderMap) -> Response {
    html_response(
        StatusCode::NOT_FOUND,
        snapshot.not_found_page(),
        snapshot,
        headers,
    )
}

struct HtmlBodyOwner(Arc<str>);

impl AsRef<[u8]> for HtmlBodyOwner {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

fn html_response(
    status: StatusCode,
    html: Arc<str>,
    snapshot: &SiteSnapshot,
    request_headers: &HeaderMap,
) -> Response {
    let Ok(etag) = HeaderValue::from_str(&format!("\"{}\"", snapshot.presentation_digest)) else {
        return internal_error_response();
    };
    if status.is_success() && if_none_match(request_headers, &etag) {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        response.headers_mut().insert(ETAG, etag);
        response
            .headers_mut()
            .insert(CACHE_CONTROL, HTML_CACHE_POLICY);
        return response;
    }

    let mut response = Response::new(Body::from(Bytes::from_owner(HtmlBodyOwner(html))));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HTML_CONTENT_TYPE);
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HTML_CACHE_POLICY);
    response.headers_mut().insert(ETAG, etag);
    response
}

fn if_none_match(headers: &HeaderMap, current: &HeaderValue) -> bool {
    let Some(current) = current.to_str().ok() else {
        return false;
    };
    headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| if_none_match_value(value, current))
}

fn if_none_match_value(value: &str, current: &str) -> bool {
    let mut quoted = false;
    let mut token_start = 0;
    let mut tokens = Vec::new();
    for (index, byte) in value.bytes().enumerate() {
        match byte {
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                tokens.push(&value[token_start..index]);
                token_start = index + 1;
            }
            _ => {}
        }
    }
    if quoted {
        return false;
    }
    tokens.push(&value[token_start..]);

    tokens.into_iter().map(str::trim).any(|candidate| {
        candidate == "*"
            || entity_tag(candidate)
                .is_some_and(|candidate| candidate.as_bytes() == current.as_bytes())
    })
}

fn entity_tag(candidate: &str) -> Option<&str> {
    let candidate = candidate.strip_prefix("W/").unwrap_or(candidate);
    let opaque = candidate.strip_prefix('"')?.strip_suffix('"')?;
    opaque
        .bytes()
        .all(|byte| byte == 0x21 || (0x23..=0x7e).contains(&byte) || byte >= 0x80)
        .then_some(candidate)
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
        http::{HeaderValue, Method, Request, StatusCode},
    };
    use time::OffsetDateTime;
    use tower::ServiceExt;

    use super::*;
    use crate::{
        domain::publication::{PublicLedgerProjection, PublishedPostRevision},
        frontend_assets::embedded_manifest,
        render::{
            SiteSnapshotReader, build_site_snapshot, compile_content_catalog, render_markdown,
            render_site_shell,
        },
        web::{PublicState, Readiness, public_router},
    };
    use markdown_compiler::{ContentTreeLimits, discover_content_tree, resolve_content_assets};

    fn published_example_state() -> PublicState {
        let root = FilePath::new(env!("CARGO_MANIFEST_DIR")).join("examples/content");
        let mut tree = discover_content_tree(&root, ContentTreeLimits::default()).unwrap();
        let source = tree.posts[0].source.replace(
            "\n+++\n\n# Hello, Maincopy",
            "\ntags = [\"rust\"]\n+++\n\n# Hello, Maincopy",
        );
        assert_ne!(source.as_str(), tree.posts[0].source.as_ref());
        tree.posts[0].source = source.into_boxed_str();
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
        request(Method::GET, path, &[]).await
    }

    async fn request(method: Method, path: &str, if_none_match: &[HeaderValue]) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        for value in if_none_match {
            request.headers_mut().append(IF_NONE_MATCH, value.clone());
        }
        public_router(published_example_state())
            .oneshot(request)
            .await
            .unwrap()
    }

    fn etag(response: &Response) -> HeaderValue {
        response
            .headers()
            .get(ETAG)
            .expect("rendered HTML must carry an ETag")
            .clone()
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

    #[tokio::test]
    async fn malformed_path_parameters_use_the_snapshot_backed_not_found_page() {
        let expected = get("/not-found").await;
        let expected_etag = etag(&expected);
        let expected_body = to_bytes(expected.into_body(), usize::MAX).await.unwrap();

        for path in [
            "/posts/%FF",
            "/tags/%FF",
            "/app-assets/%FF/site.css",
            "/app-assets/frontend-b3-v1-deadbeef/%FF",
        ] {
            let response = get(path).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(
                response.headers().get(CONTENT_TYPE),
                Some(&HTML_CONTENT_TYPE)
            );
            assert_eq!(
                response.headers().get(CACHE_CONTROL),
                Some(&HTML_CACHE_POLICY)
            );
            assert_eq!(response.headers().get(ETAG), Some(&expected_etag));
            assert_eq!(
                to_bytes(response.into_body(), usize::MAX).await.unwrap(),
                expected_body,
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn rendered_html_uses_the_active_snapshot_as_a_strong_etag() {
        let mut expected = None;
        for path in ["/", "/posts/hello-maincopy", "/tags/rust", "/archive"] {
            let response = get(path).await;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                response.headers().get(CACHE_CONTROL),
                Some(&HTML_CACHE_POLICY)
            );
            assert_eq!(
                response.headers().get(CONTENT_TYPE),
                Some(&HTML_CONTENT_TYPE)
            );
            let actual = etag(&response);
            let encoded = actual.to_str().unwrap();
            assert!(encoded.starts_with("\"presentation-b3-v1-"), "{encoded}");
            assert!(encoded.ends_with('"'), "{encoded}");
            assert!(!encoded.starts_with("W/"), "{encoded}");
            if let Some(expected) = &expected {
                assert_eq!(&actual, expected, "{path}");
            } else {
                expected = Some(actual);
            }
        }
    }

    #[tokio::test]
    async fn matching_if_none_match_returns_an_empty_304() {
        for path in ["/", "/posts/hello-maincopy", "/tags/rust", "/archive"] {
            let current = etag(&get(path).await);
            let encoded = current.to_str().unwrap();
            let candidates = [
                vec![current.clone()],
                vec![HeaderValue::from_str(&format!("W/{encoded}")).unwrap()],
                vec![HeaderValue::from_static("*")],
                vec![HeaderValue::from_str(&format!("\"another\", {encoded}")).unwrap()],
                vec![HeaderValue::from_static("\"another\""), current.clone()],
            ];
            for candidate in candidates {
                let response = request(Method::GET, path, &candidate).await;

                assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{path}");
                assert_eq!(response.headers().get(ETAG), Some(&current));
                assert_eq!(
                    response.headers().get(CACHE_CONTROL),
                    Some(&HTML_CACHE_POLICY)
                );
                assert!(response.headers().get(CONTENT_TYPE).is_none());
                assert!(
                    to_bytes(response.into_body(), usize::MAX)
                        .await
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }

    #[tokio::test]
    async fn malformed_or_nonmatching_if_none_match_does_not_suppress_html() {
        let current = etag(&get("/").await);
        let candidates = [
            HeaderValue::from_static("site-b3-v1-not-quoted"),
            HeaderValue::from_static("\"another\""),
            HeaderValue::from_static("W/\"another\""),
            HeaderValue::from_static("\"unterminated"),
            HeaderValue::from_static("\"invalid tag with spaces\""),
        ];

        for candidate in candidates {
            let response = request(Method::GET, "/", std::slice::from_ref(&candidate)).await;
            assert_eq!(response.status(), StatusCode::OK, "{candidate:?}");
            assert_eq!(etag(&response), current);
            assert!(
                !to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn html_error_pages_carry_validators_without_changing_error_semantics() {
        let not_found = get("/not-found").await;
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            not_found.headers().get(CACHE_CONTROL),
            Some(&HTML_CACHE_POLICY)
        );
        assert_eq!(
            not_found.headers().get(CONTENT_TYPE),
            Some(&HTML_CONTENT_TYPE)
        );
        let current = etag(&not_found);

        let conditional = request(Method::GET, "/not-found", std::slice::from_ref(&current)).await;
        assert_eq!(conditional.status(), StatusCode::NOT_FOUND);
        assert_eq!(etag(&conditional), current);
        assert!(
            !to_bytes(conditional.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );

        let method_not_allowed = request(Method::POST, "/", &[]).await;
        assert_eq!(method_not_allowed.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(etag(&method_not_allowed), current);
        assert_eq!(
            method_not_allowed.headers().get(CACHE_CONTROL),
            Some(&HTML_CACHE_POLICY)
        );
        assert_eq!(
            method_not_allowed.headers().get(CONTENT_TYPE),
            Some(&HTML_CONTENT_TYPE)
        );
    }
}
