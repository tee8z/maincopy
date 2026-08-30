use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request},
    response::Response,
};
use serde_json::Value;
use tower::ServiceExt;

use maincopy::{
    content::{ContentTreeLimits, discover_content_tree, resolve_content_assets},
    frontend_assets::embedded_manifest,
    render::{
        PublicLedgerProjection, SiteSnapshotBuilder, SiteSnapshotReader, compile_content_catalog,
        render_site_shell,
    },
    web::{PublicState, Readiness},
};

pub fn public_state(readiness: Readiness) -> PublicState {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/content");
    let tree = discover_content_tree(&root, ContentTreeLimits::default())
        .expect("example content tree must be discoverable");
    let content = tree.validate().expect("example content must validate");
    let assets = resolve_content_assets(&tree, &content).expect("example assets must resolve");
    let catalog = std::sync::Arc::new(
        compile_content_catalog(&content, &assets).expect("example catalog must compile"),
    );
    let ledger = PublicLedgerProjection::empty();
    let shell = render_site_shell(catalog, embedded_manifest(), &ledger)
        .expect("empty public shell must render");
    let snapshot = SiteSnapshotBuilder::new()
        .build(shell, &ledger)
        .expect("empty public snapshot must build");
    PublicState::new(SiteSnapshotReader::from_snapshot(snapshot), readiness)
}

pub async fn get(app: Router, path: &str) -> Response {
    request(app, Method::GET, path).await
}

pub async fn request(app: Router, method: Method, path: &str) -> Response {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .expect("test request must be valid");

    app.oneshot(request)
        .await
        .expect("router must produce a response")
}

pub async fn body_bytes(response: Response) -> axum::body::Bytes {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body must be readable")
}

pub async fn json_body(response: Response) -> Value {
    let body = body_bytes(response).await;

    serde_json::from_slice(&body).expect("response body must be valid JSON")
}
