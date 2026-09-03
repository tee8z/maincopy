use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use maincopy_server::{
    frontend_assets::{FrontendAssetName, IMMUTABLE_CACHE_CONTROL, embedded_manifest},
    web::{Readiness, public_router},
};
use quick_xml::{Reader, escape::unescape, events::Event};

use crate::helpers::{body_bytes, get, public_state, request, request_with_headers};

fn if_none_match(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::IF_NONE_MATCH,
        HeaderValue::from_bytes(value.as_bytes()).expect("fixture ETag header must be valid"),
    );
    headers
}

#[tokio::test]
async fn empty_public_snapshot_serves_semantic_index_and_archive_pages() {
    let app = public_router(public_state(Readiness::new(true)));

    for path in ["/", "/archive"] {
        let response = get(app.clone(), path).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        let body = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
        assert!(body.starts_with("<!DOCTYPE html>"));
        assert!(body.contains("<main"));
    }
}

#[tokio::test]
async fn empty_public_snapshot_serves_a_valid_rss_channel() {
    let app = public_router(public_state(Readiness::new(true)));
    let response = get(app, "/feed.xml").await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/rss+xml; charset=utf-8"
    );
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-cache"
    );
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert!(
        response
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("\"feed-b3-v1-")
    );
    let body = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
    assert!(body.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
    assert!(body.contains("<channel>"));
    assert!(!body.contains("<item>"));
}

#[tokio::test]
async fn empty_public_snapshot_serves_a_valid_sitemap() {
    let app = public_router(public_state(Readiness::new(true)));
    let response = get(app, "/sitemap.xml").await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/xml; charset=utf-8"
    );
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-cache"
    );
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    let etag = response
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap();
    let digest = etag
        .strip_prefix("\"sitemap-b3-v1-")
        .and_then(|value| value.strip_suffix('"'))
        .expect("sitemap ETag must be strong, quoted, and domain-separated");
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );

    let body = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
    let mut reader = Reader::from_str(&body);
    let mut saw_urlset = false;
    let mut url_count = 0;
    let mut locations = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) => match start.name().as_ref() {
                "urlset" => {
                    assert!(!saw_urlset, "sitemap must have one urlset root");
                    saw_urlset = true;
                    let namespace = start
                        .try_get_attribute("xmlns")
                        .unwrap()
                        .expect("urlset must declare the sitemap namespace");
                    assert_eq!(
                        namespace.value.as_ref(),
                        "http://www.sitemaps.org/schemas/sitemap/0.9"
                    );
                }
                "url" => url_count += 1,
                "loc" => {
                    let raw = reader.read_text(start.name()).unwrap();
                    locations.push(unescape(raw.as_ref()).unwrap().into_owned());
                }
                "lastmod" | "changefreq" | "priority" => {
                    panic!("empty sitemap must omit optional URL metadata")
                }
                _ => {}
            },
            Ok(Event::Empty(empty))
                if matches!(empty.name().as_ref(), "lastmod" | "changefreq" | "priority") =>
            {
                panic!("empty sitemap must omit optional URL metadata")
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => panic!(
                "sitemap must be well-formed XML at byte {}: {error}",
                reader.error_position()
            ),
        }
    }

    assert!(saw_urlset);
    assert_eq!(url_count, 2);
    assert_eq!(
        locations,
        ["https://example.test/", "https://example.test/archive"]
    );
}

#[tokio::test]
async fn empty_public_snapshot_serves_the_allow_all_robots_policy() {
    let app = public_router(public_state(Readiness::new(true)));
    let response = get(app, "/robots.txt").await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/plain; charset=utf-8"
    );
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-cache"
    );
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert!(
        response
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("\"robots-b3-v1-")
    );
    assert_eq!(
        body_bytes(response).await.as_ref(),
        concat!(
            "User-agent: *\n",
            "Allow: /\n",
            "\n",
            "Sitemap: https://example.test/sitemap.xml\n",
        )
        .as_bytes()
    );
}

#[tokio::test]
async fn public_router_uses_snapshot_backed_error_pages() {
    let app = public_router(public_state(Readiness::new(true)));

    let missing = get(app.clone(), "/does-not-exist").await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert!(
        String::from_utf8(body_bytes(missing).await.to_vec())
            .unwrap()
            .contains("Page not found")
    );

    for path in ["/", "/feed.xml", "/robots.txt", "/sitemap.xml"] {
        let method = request(app.clone(), Method::POST, path).await;
        assert_eq!(method.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(
            String::from_utf8(body_bytes(method).await.to_vec())
                .unwrap()
                .contains("Method not allowed")
        );
    }
}

#[tokio::test]
async fn rss_feed_has_no_implicit_aliases() {
    let app = public_router(public_state(Readiness::new(true)));

    for path in ["/feed", "/rss", "/rss.xml"] {
        assert_eq!(get(app.clone(), path).await.status(), StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn sitemap_has_no_implicit_aliases() {
    let app = public_router(public_state(Readiness::new(true)));

    for path in [
        "/sitemap",
        "/sitemap.xml/",
        "/sitemap_index.xml",
        "/SITEMAP.XML",
    ] {
        assert_eq!(get(app.clone(), path).await.status(), StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn robots_policy_has_no_implicit_aliases() {
    let app = public_router(public_state(Readiness::new(true)));

    for path in ["/robots", "/robots.txt/", "/ROBOTS.TXT", "/Robots.txt"] {
        assert_eq!(get(app.clone(), path).await.status(), StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn application_assets_require_an_exact_typed_manifest_lookup() {
    let app = public_router(public_state(Readiness::new(true)));
    let manifest = embedded_manifest();
    let stylesheet = &manifest.css;

    let response = get(app.clone(), stylesheet.public_path).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        stylesheet.mime()
    );
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        IMMUTABLE_CACHE_CONTROL
    );
    assert_eq!(
        response.headers().get(header::CONTENT_LENGTH).unwrap(),
        stylesheet.bytes.len().to_string().as_str()
    );
    assert_eq!(
        response
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap(),
        stylesheet.etag()
    );
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(body_bytes(response).await.as_ref(), stylesheet.bytes);

    let response = request(app.clone(), Method::HEAD, stylesheet.public_path).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        stylesheet.mime()
    );
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        IMMUTABLE_CACHE_CONTROL
    );
    assert_eq!(
        response.headers().get(header::CONTENT_LENGTH).unwrap(),
        stylesheet.bytes.len().to_string().as_str()
    );
    assert_eq!(
        response
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap(),
        stylesheet.etag()
    );
    assert!(body_bytes(response).await.is_empty());

    for method in [Method::GET, Method::HEAD] {
        let response = request_with_headers(
            app.clone(),
            method,
            stylesheet.public_path,
            if_none_match(&stylesheet.etag()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            IMMUTABLE_CACHE_CONTROL
        );
        assert_eq!(
            response
                .headers()
                .get(header::ETAG)
                .unwrap()
                .to_str()
                .unwrap(),
            stylesheet.etag()
        );
        assert_eq!(
            response
                .headers()
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .unwrap(),
            "nosniff"
        );
        assert!(body_bytes(response).await.is_empty());
    }

    let response = request_with_headers(
        app.clone(),
        Method::GET,
        stylesheet.public_path,
        if_none_match("\"frontend-asset-b3-v1-not-the-current-digest\""),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await.as_ref(), stylesheet.bytes);

    let javascript = manifest.javascript.as_ref().unwrap();
    let response = get(app.clone(), javascript.public_path).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        javascript.mime()
    );
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        IMMUTABLE_CACHE_CONTROL
    );
    assert_eq!(body_bytes(response).await.as_ref(), javascript.bytes);

    let digest = &manifest.bundle_digest;
    for path in [
        format!("/app-assets/{digest}/SITE.CSS"),
        format!("/app-assets/{digest}/unknown.css"),
        format!(
            "/app-assets/{}/{name}",
            "frontend-b3-v1-0000000000000000000000000000000000000000000000000000000000000000",
            name = FrontendAssetName::Stylesheet.as_str()
        ),
        format!(
            "/app-assets/not-a-digest/{}",
            FrontendAssetName::Stylesheet.as_str()
        ),
        format!("/app-assets/{digest}/%2E%2E"),
        format!("/app-assets/{digest}/%2E%2E%2Fsite.css"),
    ] {
        assert_eq!(
            get(app.clone(), &path).await.status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn malformed_public_path_parameters_are_not_found() {
    let app = public_router(public_state(Readiness::new(true)));

    for path in [
        "/posts/%FF",
        "/tags/%FF",
        "/app-assets/%FF/site.css",
        "/app-assets/frontend-b3-v1-deadbeef/%FF",
    ] {
        assert_eq!(get(app.clone(), path).await.status(), StatusCode::NOT_FOUND);
    }
}
