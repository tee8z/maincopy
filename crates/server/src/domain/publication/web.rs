use std::{convert::Infallible, fmt, str::FromStr, sync::Arc};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{FromRequestParts, OriginalUri, Path},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{
            CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_SECURITY_POLICY,
            CONTENT_TYPE, ETAG, IF_NONE_MATCH, LOCATION, X_CONTENT_TYPE_OPTIONS,
        },
        request::Parts,
    },
    response::Response,
    routing::get,
};
use markdown_compiler::{PostAlias, PostSlug, PostTag};
use serde::de::DeserializeOwned;

use super::{
    CanonicalSiteUrl, PublicPagePath, ROBOTS_PATH, RSS_FEED_PATH, SITEMAP_PATH,
    assets::AssetDelivery,
};
use crate::{
    frontend_assets::{FrontendAssetName, FrontendBundleDigest, IMMUTABLE_CACHE_CONTROL},
    render::{SiteSnapshot, SiteSnapshotReader, SnapshotAssetPath, SnapshotPublicAsset},
};

const HTML_CONTENT_TYPE: HeaderValue = HeaderValue::from_static("text/html; charset=utf-8");
const REVALIDATE_CACHE_POLICY: HeaderValue = HeaderValue::from_static("no-cache");
const ROBOTS_CONTENT_TYPE: HeaderValue = HeaderValue::from_static("text/plain; charset=utf-8");
const RSS_CONTENT_TYPE: HeaderValue =
    HeaderValue::from_static("application/rss+xml; charset=utf-8");
const SITEMAP_CONTENT_TYPE: HeaderValue =
    HeaderValue::from_static("application/xml; charset=utf-8");
const NOSNIFF: HeaderValue = HeaderValue::from_static("nosniff");
const ASSET_SANDBOX: HeaderValue = HeaderValue::from_static("sandbox; default-src 'none'");
const DOWNLOAD_ASSET: HeaderValue =
    HeaderValue::from_static("attachment; filename=\"maincopy-asset\"");

/// Builds the publication-specific portion of the public HTTP API.
pub(crate) fn router(snapshots: SiteSnapshotReader) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/posts/{slug}", get(post))
        .route("/tags/{tag}", get(tag))
        .route("/archive", get(archive))
        .route(RSS_FEED_PATH, get(feed))
        .route(ROBOTS_PATH, get(robots))
        .route(SITEMAP_PATH, get(sitemap))
        .route("/assets/{digest}/{*path}", get(content_asset))
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
    OriginalUri(uri): OriginalUri,
    PublicPath {
        snapshot,
        headers,
        value,
    }: PublicPath<String>,
) -> Response {
    let Ok(slug) = PostSlug::parse(value.clone()) else {
        return not_found_response(&snapshot, &headers);
    };
    if let Some(page) = snapshot.post_page(&slug) {
        return html_response(StatusCode::OK, page, &snapshot, &headers);
    }
    let Ok(alias) = PostAlias::parse(value) else {
        return not_found_response(&snapshot, &headers);
    };
    if uri.path() != PublicPagePath::post_alias(&alias).as_str() {
        return not_found_response(&snapshot, &headers);
    }
    snapshot.alias_target(&alias).map_or_else(
        || not_found_response(&snapshot, &headers),
        alias_redirect_response,
    )
}

fn alias_redirect_response(target: &CanonicalSiteUrl) -> Response {
    let Ok(location) = HeaderValue::from_str(target.as_str()) else {
        return internal_error_response();
    };
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::PERMANENT_REDIRECT;
    response.headers_mut().insert(LOCATION, location);
    response
        .headers_mut()
        .insert(CACHE_CONTROL, REVALIDATE_CACHE_POLICY);
    response
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

async fn feed(PublicRequest { snapshot, headers }: PublicRequest) -> Response {
    snapshot_document_response(
        Arc::clone(&snapshot.feed.body),
        &snapshot.feed.digest,
        RSS_CONTENT_TYPE,
        &headers,
    )
}

async fn robots(PublicRequest { snapshot, headers }: PublicRequest) -> Response {
    snapshot_document_response(
        Arc::clone(&snapshot.robots.body),
        &snapshot.robots.digest,
        ROBOTS_CONTENT_TYPE,
        &headers,
    )
}

async fn sitemap(PublicRequest { snapshot, headers }: PublicRequest) -> Response {
    snapshot_document_response(
        Arc::clone(&snapshot.sitemap.body),
        &snapshot.sitemap.digest,
        SITEMAP_CONTENT_TYPE,
        &headers,
    )
}

async fn content_asset(
    OriginalUri(uri): OriginalUri,
    PublicRequest { snapshot, headers }: PublicRequest,
) -> Response {
    let Ok(path) = SnapshotAssetPath::parse(uri.path()) else {
        return not_found_response(&snapshot, &headers);
    };
    let Some(asset) = snapshot.public_asset(&path) else {
        return not_found_response(&snapshot, &headers);
    };
    content_asset_response(asset, &headers)
}

fn content_asset_response(asset: &SnapshotPublicAsset, request_headers: &HeaderMap) -> Response {
    let Ok(etag) = HeaderValue::from_str(&format!("\"{}\"", asset.digest)) else {
        return internal_error_response();
    };
    if if_none_match(request_headers, &etag) {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        let headers = response.headers_mut();
        headers.insert(
            CACHE_CONTROL,
            HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL),
        );
        headers.insert(ETAG, etag);
        headers.insert(X_CONTENT_TYPE_OPTIONS, NOSNIFF);
        headers.insert(CONTENT_SECURITY_POLICY, ASSET_SANDBOX);
        return response;
    }

    let Ok(content_length) = HeaderValue::from_str(&asset.bytes.len().to_string()) else {
        return internal_error_response();
    };
    let delivery = asset.delivery;
    let mut response = Response::new(Body::from(Bytes::from_owner(Arc::clone(&asset.bytes))));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static(delivery.content_type()),
    );
    headers.insert(CONTENT_LENGTH, content_length);
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL),
    );
    headers.insert(ETAG, etag);
    headers.insert(X_CONTENT_TYPE_OPTIONS, NOSNIFF);
    headers.insert(CONTENT_SECURITY_POLICY, ASSET_SANDBOX);
    match delivery {
        AssetDelivery::Inline(_) => {}
        AssetDelivery::Attachment => {
            headers.insert(CONTENT_DISPOSITION, DOWNLOAD_ASSET);
        }
    }
    response
}

fn snapshot_document_response(
    body: Arc<str>,
    digest: &impl fmt::Display,
    content_type: HeaderValue,
    request_headers: &HeaderMap,
) -> Response {
    let Ok(etag) = HeaderValue::from_str(&format!("\"{digest}\"")) else {
        return internal_error_response();
    };
    if if_none_match(request_headers, &etag) {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        let headers = response.headers_mut();
        headers.insert(CACHE_CONTROL, REVALIDATE_CACHE_POLICY);
        headers.insert(ETAG, etag);
        headers.insert("x-content-type-options", NOSNIFF);
        return response;
    }

    let mut response = Response::new(Body::from(Bytes::from_owner(ArcStrBodyOwner(body))));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, content_type);
    headers.insert(CACHE_CONTROL, REVALIDATE_CACHE_POLICY);
    headers.insert(ETAG, etag);
    headers.insert("x-content-type-options", NOSNIFF);
    response
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
    if if_none_match(&headers, &etag) {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        let response_headers = response.headers_mut();
        response_headers.insert(
            CACHE_CONTROL,
            HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL),
        );
        response_headers.insert(ETAG, etag);
        response_headers.insert(X_CONTENT_TYPE_OPTIONS, NOSNIFF);
        return response;
    }

    let Ok(content_length) = HeaderValue::from_str(&asset.bytes.len().to_string()) else {
        return internal_error_response();
    };
    let mut response = Response::new(Body::from(asset.bytes));
    *response.status_mut() = StatusCode::OK;
    let response_headers = response.headers_mut();
    response_headers.insert(CONTENT_TYPE, HeaderValue::from_static(asset.mime()));
    response_headers.insert(CONTENT_LENGTH, content_length);
    response_headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL),
    );
    response_headers.insert(ETAG, etag);
    response_headers.insert(X_CONTENT_TYPE_OPTIONS, NOSNIFF);
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

struct ArcStrBodyOwner(Arc<str>);

impl AsRef<[u8]> for ArcStrBodyOwner {
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
            .insert(CACHE_CONTROL, REVALIDATE_CACHE_POLICY);
        return response;
    }

    let mut response = Response::new(Body::from(Bytes::from_owner(ArcStrBodyOwner(html))));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HTML_CONTENT_TYPE);
    response
        .headers_mut()
        .insert(CACHE_CONTROL, REVALIDATE_CACHE_POLICY);
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
    use std::{
        fs,
        path::{Path as FilePath, PathBuf},
        sync::Arc,
    };

    use axum::{
        body::{Body, to_bytes},
        http::{HeaderValue, Method, Request, StatusCode},
    };
    use tempfile::tempdir;
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
    use markdown_compiler::{
        ContentTreeLimits, digest_asset, discover_content_tree, resolve_content_assets,
    };

    const TEST_RESPONSE_LIMIT: usize = 64 * 1_024;
    const PUBLISHED_PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, b'I', b'D', b'A', b'T', 0x08, 0xd7, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00,
        0x00, 0x00, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82,
    ];
    const PUBLISHED_HTML: &[u8] =
        b"<!doctype html><title>download fixture</title><script>alert('inert')</script>";

    struct ContentAssetFixture {
        app: Router,
        removed_source_root: PathBuf,
    }

    fn write_content_file(root: &FilePath, relative: &str, bytes: &[u8]) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("content fixture directory must be created");
        }
        fs::write(path, bytes).expect("content fixture file must be written");
    }

    fn content_asset_post(
        id: &str,
        slug: &str,
        aliases: &[&str],
        draft: bool,
        body: &str,
    ) -> String {
        let aliases = aliases
            .iter()
            .map(|alias| format!("{alias:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "+++\n\
             id = {id:?}\n\
             title = {slug:?}\n\
             slug = {slug:?}\n\
             aliases = [{aliases}]\n\
             authored_at = 2026-08-29T12:00:00Z\n\
             description = \"Content asset route fixture.\"\n\
             draft = {draft}\n\
             +++\n\n\
             {body}\n"
        )
    }

    // Placement exception to docs/quality.md: these full public-router contracts need a
    // nonempty exact ledger, whose construction intentionally stays crate-private rather than
    // becoming test-only public API.
    fn content_asset_fixture() -> ContentAssetFixture {
        let root = tempdir().expect("temporary content root must be created");
        write_content_file(
            root.path(),
            "publication.toml",
            b"[site]\n\
              title = \"Content Asset Tests\"\n\
              base_url = \"https://assets.example.test\"\n\
              description = \"Snapshot-owned asset route tests.\"\n\
              [author]\n\
              name = \"Asset Tester\"\n",
        );
        write_content_file(
            root.path(),
            "posts/published.md",
            content_asset_post(
                "11111111-1111-4111-8111-111111111111",
                "published-assets",
                &["old-published-assets", "legacy-published-assets"],
                false,
                "![inline](assets/published.png)\n\n[download](assets/published.html)",
            )
            .as_bytes(),
        );
        write_content_file(
            root.path(),
            "posts/unpublished.md",
            content_asset_post(
                "22222222-2222-4222-8222-222222222222",
                "unpublished-assets",
                &["unpublished-assets-alias"],
                false,
                "![unpublished](assets/unpublished.png)",
            )
            .as_bytes(),
        );
        write_content_file(
            root.path(),
            "drafts/draft.md",
            content_asset_post(
                "33333333-3333-4333-8333-333333333333",
                "draft-assets",
                &["draft-assets-alias"],
                true,
                "![draft](assets/draft.png)",
            )
            .as_bytes(),
        );
        write_content_file(root.path(), "assets/published.png", PUBLISHED_PNG);
        write_content_file(root.path(), "assets/published.html", PUBLISHED_HTML);
        write_content_file(
            root.path(),
            "assets/unpublished.png",
            b"unpublished asset bytes",
        );
        write_content_file(root.path(), "assets/draft.png", b"draft asset bytes");
        write_content_file(
            root.path(),
            "assets/unreferenced.bin",
            b"unreferenced asset bytes",
        );

        let tree = discover_content_tree(root.path(), ContentTreeLimits::default())
            .expect("content asset fixture must be discovered");
        let content = tree
            .validate()
            .expect("content asset fixture must validate");
        let assets = resolve_content_assets(&tree, &content)
            .expect("content asset fixture references must resolve");
        let published = content
            .posts
            .iter()
            .find(|document| document.metadata.slug.as_str() == "published-assets")
            .expect("published fixture post must exist");
        let rendered = render_markdown(
            published,
            assets
                .assets_for(published)
                .expect("published fixture assets must exist"),
            assets
                .site_assets_for(&content.publication)
                .expect("fixture site assets must exist"),
        )
        .expect("published fixture post must render");
        let ledger = PublicLedgerProjection::try_from_exact_entries([PublishedPostRevision::new(
            rendered.document.metadata.id.clone(),
            rendered.revision,
            OffsetDateTime::from_unix_timestamp(2_000)
                .expect("fixture publication time must be valid"),
        )])
        .expect("fixture ledger must be valid");
        let catalog = Arc::new(
            compile_content_catalog(&content, &assets).expect("content asset fixture must compile"),
        );
        let shell = render_site_shell(catalog, embedded_manifest(), &ledger)
            .expect("content asset fixture shell must render");
        let snapshot =
            build_site_snapshot(shell, &ledger).expect("content asset fixture snapshot must build");

        let removed_source_root = root.path().to_path_buf();
        root.close()
            .expect("temporary content source must be removed after snapshot compilation");
        assert!(!removed_source_root.exists());

        ContentAssetFixture {
            app: public_router(PublicState {
                snapshots: SiteSnapshotReader::from_snapshot(snapshot),
                readiness: Readiness::new(true),
            }),
            removed_source_root,
        }
    }

    async fn content_asset_request(
        app: &Router,
        method: Method,
        path: &str,
        validators: &[HeaderValue],
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .expect("content asset request must be valid");
        for validator in validators {
            request
                .headers_mut()
                .append(IF_NONE_MATCH, validator.clone());
        }
        app.clone().oneshot(request).await.unwrap()
    }

    async fn content_asset_urls(app: &Router) -> (String, String) {
        let response =
            content_asset_request(app, Method::GET, "/posts/published-assets", &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = String::from_utf8(
            to_bytes(response.into_body(), TEST_RESPONSE_LIMIT)
                .await
                .expect("fixture post body must stay within the test limit")
                .to_vec(),
        )
        .expect("fixture post must be UTF-8");
        let find = |suffix: &str| {
            html.split('"')
                .find(|value| value.starts_with("/assets/") && value.ends_with(suffix))
                .unwrap_or_else(|| panic!("rendered post must contain an asset ending in {suffix}"))
                .to_owned()
        };
        (find("/published.png"), find("/published.html"))
    }

    fn assert_asset_headers(
        response: &Response,
        content_type: &str,
        content_length: usize,
        expected_etag: &HeaderValue,
        attachment: bool,
    ) {
        assert_eq!(response.headers().get(CONTENT_TYPE).unwrap(), content_type);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_LENGTH)
                .unwrap()
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            content_length
        );
        assert_eq!(
            response.headers().get(CACHE_CONTROL),
            Some(&HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL))
        );
        assert_eq!(response.headers().get(ETAG), Some(expected_etag));
        assert_eq!(
            response.headers().get(X_CONTENT_TYPE_OPTIONS),
            Some(&NOSNIFF)
        );
        assert_eq!(
            response.headers().get(CONTENT_SECURITY_POLICY),
            Some(&ASSET_SANDBOX)
        );
        if attachment {
            assert_eq!(
                response.headers().get(CONTENT_DISPOSITION),
                Some(&DOWNLOAD_ASSET)
            );
        } else {
            assert!(response.headers().get(CONTENT_DISPOSITION).is_none());
        }
    }

    fn expected_asset_etag(bytes: &[u8]) -> HeaderValue {
        HeaderValue::from_str(&format!("\"{}\"", digest_asset(bytes)))
            .expect("fixture asset digest must form an HTTP entity tag")
    }

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
            .expect("snapshot response must carry an ETag")
            .clone()
    }

    #[tokio::test]
    async fn selected_content_assets_survive_source_removal_with_fixed_delivery_headers() {
        let fixture = content_asset_fixture();
        assert!(!fixture.removed_source_root.exists());
        let (png_path, html_path) = content_asset_urls(&fixture.app).await;
        assert_ne!(png_path, html_path);

        let png_etag = expected_asset_etag(PUBLISHED_PNG);
        for method in [Method::GET, Method::HEAD] {
            let response =
                content_asset_request(&fixture.app, method.clone(), &png_path, &[]).await;
            assert_eq!(response.status(), StatusCode::OK, "{method}");
            assert_asset_headers(
                &response,
                "image/png",
                PUBLISHED_PNG.len(),
                &png_etag,
                false,
            );
            let body = to_bytes(response.into_body(), TEST_RESPONSE_LIMIT)
                .await
                .expect("PNG response must stay within the test limit");
            if method == Method::GET {
                assert_eq!(body.as_ref(), PUBLISHED_PNG);
            } else {
                assert!(body.is_empty());
            }
        }

        let html_etag = expected_asset_etag(PUBLISHED_HTML);
        for method in [Method::GET, Method::HEAD] {
            let response =
                content_asset_request(&fixture.app, method.clone(), &html_path, &[]).await;
            assert_eq!(response.status(), StatusCode::OK, "{method}");
            assert_asset_headers(
                &response,
                "application/octet-stream",
                PUBLISHED_HTML.len(),
                &html_etag,
                true,
            );
            let body = to_bytes(response.into_body(), TEST_RESPONSE_LIMIT)
                .await
                .expect("HTML asset response must stay within the test limit");
            if method == Method::GET {
                assert_eq!(body.as_ref(), PUBLISHED_HTML);
            } else {
                assert!(body.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn content_asset_validators_apply_to_get_and_head_without_weakening_headers() {
        let fixture = content_asset_fixture();
        let (png_path, _) = content_asset_urls(&fixture.app).await;
        let current = expected_asset_etag(PUBLISHED_PNG);
        let weak = HeaderValue::from_str(&format!("W/{}", current.to_str().unwrap()))
            .expect("fixture weak entity tag must be valid");

        for method in [Method::GET, Method::HEAD] {
            for validator in [current.clone(), weak.clone(), HeaderValue::from_static("*")] {
                let response =
                    content_asset_request(&fixture.app, method.clone(), &png_path, &[validator])
                        .await;
                assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{method}");
                assert_eq!(response.headers().get(ETAG), Some(&current));
                assert_eq!(
                    response.headers().get(CACHE_CONTROL),
                    Some(&HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL))
                );
                assert_eq!(
                    response.headers().get(X_CONTENT_TYPE_OPTIONS),
                    Some(&NOSNIFF)
                );
                assert_eq!(
                    response.headers().get(CONTENT_SECURITY_POLICY),
                    Some(&ASSET_SANDBOX)
                );
                assert!(response.headers().get(CONTENT_TYPE).is_none());
                assert!(response.headers().get(CONTENT_DISPOSITION).is_none());
                assert!(
                    to_bytes(response.into_body(), TEST_RESPONSE_LIMIT)
                        .await
                        .expect("conditional response must stay within the test limit")
                        .is_empty()
                );
            }

            let response = content_asset_request(
                &fixture.app,
                method.clone(),
                &png_path,
                &[HeaderValue::from_static("\"not-the-current-asset\"")],
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{method}");
            assert_asset_headers(&response, "image/png", PUBLISHED_PNG.len(), &current, false);
            let body = to_bytes(response.into_body(), TEST_RESPONSE_LIMIT)
                .await
                .expect("nonmatching response must stay within the test limit");
            if method == Method::GET {
                assert_eq!(body.as_ref(), PUBLISHED_PNG);
            } else {
                assert!(body.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn content_asset_route_hides_unselected_noncanonical_and_wrong_snapshot_paths() {
        let fixture = content_asset_fixture();
        let (png_path, _) = content_asset_urls(&fixture.app).await;
        let snapshot_prefix = png_path
            .strip_suffix("/published.png")
            .expect("fixture PNG path must end in its logical asset name");
        let oversized_path = [
            "a".repeat(255),
            "b".repeat(255),
            "c".repeat(255),
            "d".repeat(250),
        ]
        .join("/");
        let overdeep_path = std::iter::repeat_n("a", 16).collect::<Vec<_>>().join("/");
        let hidden_paths = [
            format!("{snapshot_prefix}/unpublished.png"),
            format!("{snapshot_prefix}/draft.png"),
            format!("{snapshot_prefix}/unreferenced.bin"),
            format!("/assets/site-b3-v1-{}/published.png", "aa".repeat(32)),
            "/assets/not-a-snapshot/published.png".to_owned(),
            format!("{snapshot_prefix}/%2e%2e/published.png"),
            format!("{snapshot_prefix}/%70ublished.png"),
            format!("{snapshot_prefix}//published.png"),
            format!("{snapshot_prefix}/%FF"),
            format!("{snapshot_prefix}/{overdeep_path}"),
            format!("{snapshot_prefix}/{oversized_path}"),
        ];
        let removed_source = fixture.removed_source_root.to_string_lossy();

        for path in hidden_paths {
            let response = content_asset_request(&fixture.app, Method::GET, &path, &[]).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            let body = to_bytes(response.into_body(), TEST_RESPONSE_LIMIT)
                .await
                .expect("not-found response must stay within the test limit");
            assert!(
                !String::from_utf8_lossy(&body).contains(removed_source.as_ref()),
                "response disclosed source root for {path}"
            );
        }

        let response = content_asset_request(&fixture.app, Method::POST, &png_path, &[]).await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let body = to_bytes(response.into_body(), TEST_RESPONSE_LIMIT)
            .await
            .expect("method-not-allowed response must stay within the test limit");
        assert!(!String::from_utf8_lossy(&body).contains(removed_source.as_ref()));
        assert_ne!(body.as_ref(), PUBLISHED_PNG);
    }

    #[tokio::test]
    async fn exact_published_aliases_redirect_directly_to_the_configured_canonical_url() {
        let fixture = content_asset_fixture();
        assert!(!fixture.removed_source_root.exists());

        for method in [Method::GET, Method::HEAD] {
            for alias in ["old-published-assets", "legacy-published-assets"] {
                let path = format!("/posts/{alias}?ignored=request-query");
                let request = Request::builder()
                    .method(method.clone())
                    .uri(path)
                    .header(axum::http::header::HOST, "attacker.example")
                    .header("forwarded", "host=attacker.example;proto=http")
                    .header("x-forwarded-host", "attacker.example")
                    .header("x-forwarded-proto", "http")
                    .body(Body::empty())
                    .expect("alias request must be valid");
                let response = fixture.app.clone().oneshot(request).await.unwrap();

                assert_eq!(
                    response.status(),
                    StatusCode::PERMANENT_REDIRECT,
                    "{method}"
                );
                assert_eq!(
                    response.headers().get(LOCATION).unwrap(),
                    "https://assets.example.test/posts/published-assets"
                );
                assert_eq!(
                    response.headers().get(CACHE_CONTROL),
                    Some(&REVALIDATE_CACHE_POLICY)
                );
                assert!(
                    to_bytes(response.into_body(), TEST_RESPONSE_LIMIT)
                        .await
                        .expect("alias response must stay within the test limit")
                        .is_empty()
                );
            }
        }

        assert_eq!(
            content_asset_request(&fixture.app, Method::GET, "/posts/published-assets", &[],)
                .await
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn aliases_outside_the_active_published_revision_are_not_public_routes() {
        let fixture = content_asset_fixture();
        for path in [
            "/posts/unpublished-assets-alias",
            "/posts/draft-assets-alias",
            "/posts/OLD-published-assets",
            "/posts/old-published-assets/",
            "/posts/%6Fld-published-assets",
            "/posts/old%2Dpublished-assets",
            "/posts/not-an-authored-alias",
        ] {
            assert_eq!(
                content_asset_request(&fixture.app, Method::GET, path, &[])
                    .await
                    .status(),
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
        assert_eq!(
            content_asset_request(
                &fixture.app,
                Method::POST,
                "/posts/old-published-assets",
                &[],
            )
            .await
            .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );

        for path in ["/feed.xml", "/sitemap.xml"] {
            let response = content_asset_request(&fixture.app, Method::GET, path, &[]).await;
            let body = to_bytes(response.into_body(), TEST_RESPONSE_LIMIT)
                .await
                .expect("discovery document must stay within the test limit");
            assert!(!String::from_utf8_lossy(&body).contains("old-published-assets"));
        }
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
    async fn canonical_metadata_ignores_request_authority_headers() {
        let request = Request::builder()
            .method(Method::GET)
            .uri("/posts/hello-maincopy")
            .header(axum::http::header::HOST, "attacker.example")
            .header("forwarded", "host=attacker.example;proto=http")
            .header("x-forwarded-host", "attacker.example")
            .header("x-forwarded-proto", "http")
            .body(Body::empty())
            .unwrap();
        let response = public_router(published_example_state())
            .oneshot(request)
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        let canonical_url = "https://example.test/posts/hello-maincopy";
        assert!(body.contains(&format!(
            "<link rel=\"canonical\" href=\"{canonical_url}\">"
        )));
        assert!(body.contains(&format!(
            "<meta property=\"og:url\" content=\"{canonical_url}\">"
        )));
        assert!(body.contains(&format!("\"url\":\"{canonical_url}\"")));
        assert!(body.contains(&format!("\"mainEntityOfPage\":\"{canonical_url}\"")));
        assert!(!body.contains("attacker.example"));
    }

    #[tokio::test]
    async fn rss_feed_is_snapshot_backed_and_discoverable() {
        let response = get("/feed.xml").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE),
            Some(&RSS_CONTENT_TYPE)
        );
        assert_eq!(
            response.headers().get(CACHE_CONTROL),
            Some(&REVALIDATE_CACHE_POLICY)
        );
        assert_eq!(
            response.headers().get("x-content-type-options"),
            Some(&NOSNIFF)
        );
        let current = etag(&response);
        assert!(current.to_str().unwrap().starts_with("\"feed-b3-v1-"));
        let body = String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(body.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(body.contains("<title>Maincopy Example</title>"));
        assert!(body.contains("<link>https://example.test/</link>"));
        assert!(body.contains("href=\"https://example.test/feed.xml\""));
        assert!(body.contains("<link>https://example.test/posts/hello-maincopy</link>"));
        assert!(
            body.contains(
                "<guid isPermaLink=\"false\">1dd7559b-90a9-4c5b-a13c-70bf6ec01e92</guid>"
            )
        );
        assert!(body.contains("<pubDate>Thu, 01 Jan 1970 00:33:20 +0000</pubDate>"));
        assert!(!body.contains("not-published"));
        assert!(!body.contains("This repository is the canonical source"));

        let index = String::from_utf8(
            to_bytes(get("/").await.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(index.contains(
            "<link rel=\"alternate\" type=\"application/rss+xml\" title=\"Maincopy Example RSS feed\" href=\"https://example.test/feed.xml\">"
        ));
    }

    #[tokio::test]
    async fn rss_head_and_conditional_reads_preserve_feed_headers() {
        let current = etag(&get("/feed.xml").await);

        let head = request(Method::HEAD, "/feed.xml", &[]).await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers().get(CONTENT_TYPE), Some(&RSS_CONTENT_TYPE));
        assert_eq!(
            head.headers().get(CACHE_CONTROL),
            Some(&REVALIDATE_CACHE_POLICY)
        );
        assert_eq!(head.headers().get(ETAG), Some(&current));
        assert_eq!(head.headers().get("x-content-type-options"), Some(&NOSNIFF));
        assert!(
            to_bytes(head.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );

        let conditional = request(Method::GET, "/feed.xml", std::slice::from_ref(&current)).await;
        assert_eq!(conditional.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            conditional.headers().get(CACHE_CONTROL),
            Some(&REVALIDATE_CACHE_POLICY)
        );
        assert_eq!(conditional.headers().get(ETAG), Some(&current));
        assert_eq!(
            conditional.headers().get("x-content-type-options"),
            Some(&NOSNIFF)
        );
        assert!(conditional.headers().get(CONTENT_TYPE).is_none());
        assert!(
            to_bytes(conditional.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn robots_is_snapshot_backed_and_ignores_request_authority_headers() {
        let request = Request::builder()
            .method(Method::GET)
            .uri("/robots.txt")
            .header(axum::http::header::HOST, "attacker.example")
            .header("forwarded", "host=attacker.example;proto=http")
            .header("x-forwarded-host", "attacker.example")
            .header("x-forwarded-proto", "http")
            .body(Body::empty())
            .unwrap();
        let response = public_router(published_example_state())
            .oneshot(request)
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE),
            Some(&ROBOTS_CONTENT_TYPE)
        );
        assert_eq!(
            response.headers().get(CACHE_CONTROL),
            Some(&REVALIDATE_CACHE_POLICY)
        );
        assert_eq!(
            response.headers().get("x-content-type-options"),
            Some(&NOSNIFF)
        );
        let current = etag(&response);
        assert!(current.to_str().unwrap().starts_with("\"robots-b3-v1-"));
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            body.as_ref(),
            concat!(
                "User-agent: *\n",
                "Allow: /\n",
                "\n",
                "Sitemap: https://example.test/sitemap.xml\n",
            )
            .as_bytes()
        );
        assert!(!body.starts_with(&[0xef, 0xbb, 0xbf]));
        assert!(!body.contains(&b'\r'));
        for private_name in ["admin", "preview", "metrics"] {
            assert!(!String::from_utf8_lossy(&body).contains(private_name));
        }
    }

    #[tokio::test]
    async fn robots_head_and_conditional_reads_preserve_document_headers() {
        let current = etag(&get("/robots.txt").await);

        let head = request(Method::HEAD, "/robots.txt", &[]).await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers().get(CONTENT_TYPE), Some(&ROBOTS_CONTENT_TYPE));
        assert_eq!(
            head.headers().get(CACHE_CONTROL),
            Some(&REVALIDATE_CACHE_POLICY)
        );
        assert_eq!(head.headers().get(ETAG), Some(&current));
        assert_eq!(head.headers().get("x-content-type-options"), Some(&NOSNIFF));
        assert!(
            to_bytes(head.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );

        let weak = HeaderValue::from_str(&format!("W/{}", current.to_str().unwrap())).unwrap();
        for method in [Method::GET, Method::HEAD] {
            for validator in [current.clone(), weak.clone(), HeaderValue::from_static("*")] {
                let conditional = request(method.clone(), "/robots.txt", &[validator]).await;
                assert_eq!(conditional.status(), StatusCode::NOT_MODIFIED);
                assert_eq!(
                    conditional.headers().get(CACHE_CONTROL),
                    Some(&REVALIDATE_CACHE_POLICY)
                );
                assert_eq!(conditional.headers().get(ETAG), Some(&current));
                assert_eq!(
                    conditional.headers().get("x-content-type-options"),
                    Some(&NOSNIFF)
                );
                assert!(conditional.headers().get(CONTENT_TYPE).is_none());
                assert!(
                    to_bytes(conditional.into_body(), usize::MAX)
                        .await
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }

    #[tokio::test]
    async fn sitemap_is_snapshot_backed_and_contains_only_canonical_html_routes() {
        let response = get("/sitemap.xml").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE),
            Some(&SITEMAP_CONTENT_TYPE)
        );
        assert_eq!(
            response.headers().get(CACHE_CONTROL),
            Some(&REVALIDATE_CACHE_POLICY)
        );
        assert_eq!(
            response.headers().get("x-content-type-options"),
            Some(&NOSNIFF)
        );
        let current = etag(&response);
        assert!(current.to_str().unwrap().starts_with("\"sitemap-b3-v1-"));
        let body = String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        let expected = [
            "https://example.test/",
            "https://example.test/archive",
            "https://example.test/posts/hello-maincopy",
            "https://example.test/tags/rust",
        ];
        let mut prior = 0;
        for location in expected {
            let position = body.find(&format!("<loc>{location}</loc>")).unwrap();
            assert!(position > prior, "sitemap locations must be sorted");
            prior = position;
        }
        assert!(!body.contains("feed.xml"));
        assert!(!body.contains("robots.txt"));
        assert!(!body.contains("not-published"));
        assert!(!body.contains("<lastmod>"));
    }

    #[tokio::test]
    async fn sitemap_head_and_conditional_reads_preserve_document_headers() {
        let current = etag(&get("/sitemap.xml").await);

        let head = request(Method::HEAD, "/sitemap.xml", &[]).await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(
            head.headers().get(CONTENT_TYPE),
            Some(&SITEMAP_CONTENT_TYPE)
        );
        assert_eq!(
            head.headers().get(CACHE_CONTROL),
            Some(&REVALIDATE_CACHE_POLICY)
        );
        assert_eq!(head.headers().get(ETAG), Some(&current));
        assert_eq!(head.headers().get("x-content-type-options"), Some(&NOSNIFF));
        assert!(
            to_bytes(head.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );

        let conditional =
            request(Method::GET, "/sitemap.xml", std::slice::from_ref(&current)).await;
        assert_eq!(conditional.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            conditional.headers().get(CACHE_CONTROL),
            Some(&REVALIDATE_CACHE_POLICY)
        );
        assert_eq!(conditional.headers().get(ETAG), Some(&current));
        assert_eq!(
            conditional.headers().get("x-content-type-options"),
            Some(&NOSNIFF)
        );
        assert!(conditional.headers().get(CONTENT_TYPE).is_none());
        assert!(
            to_bytes(conditional.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
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
                Some(&REVALIDATE_CACHE_POLICY)
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
                Some(&REVALIDATE_CACHE_POLICY)
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
                    Some(&REVALIDATE_CACHE_POLICY)
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
            Some(&REVALIDATE_CACHE_POLICY)
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
            Some(&REVALIDATE_CACHE_POLICY)
        );
        assert_eq!(
            method_not_allowed.headers().get(CONTENT_TYPE),
            Some(&HTML_CONTENT_TYPE)
        );
    }
}
