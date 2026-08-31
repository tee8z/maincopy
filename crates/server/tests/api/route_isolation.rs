use axum::http::{Method, StatusCode};
use maincopy_server::web::{Readiness, public_router};
use maincopy_shared::{
    ADMIN_CAPABILITIES_PATH,
    posts::POSTS_PATH,
    profile_api::{ACTIVE_TIP_RECIPIENT_PATH, CURRENT_USER_PROFILE_PATH},
    publication::PUBLICATIONS_PATH,
};

use crate::helpers::{get, public_state, request};

#[tokio::test]
async fn public_router_does_not_expose_version_neutral_admin_discovery() {
    let response = get(
        public_router(public_state(Readiness::new(true))),
        ADMIN_CAPABILITIES_PATH,
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_router_does_not_expose_admin_routes() {
    let response = get(
        public_router(public_state(Readiness::new(true))),
        "/api/admin/v1/capabilities",
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_router_does_not_expose_profile_or_tip_recipient_resources() {
    let app = public_router(public_state(Readiness::new(true)));

    for path in [CURRENT_USER_PROFILE_PATH, ACTIVE_TIP_RECIPIENT_PATH] {
        let response = get(app.clone(), path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn public_router_does_not_expose_publication_commands() {
    let response = request(
        public_router(public_state(Readiness::new(true))),
        Method::POST,
        PUBLICATIONS_PATH,
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_router_does_not_expose_loaded_post_revisions() {
    let response = get(
        public_router(public_state(Readiness::new(true))),
        POSTS_PATH,
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_router_does_not_expose_candidate_post_previews() {
    let response = get(
        public_router(public_state(Readiness::new(true))),
        &format!("{POSTS_PATH}/11111111-1111-4111-8111-111111111111/preview"),
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_router_does_not_expose_candidate_preview_assets() {
    let response = get(
        public_router(public_state(Readiness::new(true))),
        "/api/admin/v1/preview-assets/content-b3-v1-4444444444444444444444444444444444444444444444444444444444444444?path=assets/preview.png",
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_router_does_not_expose_admin_openapi() {
    let response = get(
        public_router(public_state(Readiness::new(true))),
        "/api/admin/v1/openapi.json",
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
