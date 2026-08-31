use axum::http::{Method, StatusCode, header};
use maincopy_server::{
    frontend_assets::{FrontendAssetName, IMMUTABLE_CACHE_CONTROL, embedded_manifest},
    web::{Readiness, public_router},
};

use crate::helpers::{body_bytes, get, public_state, request};

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
async fn public_router_uses_snapshot_backed_error_pages() {
    let app = public_router(public_state(Readiness::new(true)));

    let missing = get(app.clone(), "/does-not-exist").await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert!(
        String::from_utf8(body_bytes(missing).await.to_vec())
            .unwrap()
            .contains("Page not found")
    );

    let method = request(app, Method::POST, "/").await;
    assert_eq!(method.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(
        String::from_utf8(body_bytes(method).await.to_vec())
            .unwrap()
            .contains("Method not allowed")
    );
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
