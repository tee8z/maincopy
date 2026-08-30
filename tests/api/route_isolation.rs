use axum::http::StatusCode;
use maincopy::{
    admin::admin_router,
    web::{Readiness, public_router},
};

use crate::helpers::{get, public_state};

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
async fn public_router_does_not_expose_admin_openapi() {
    let response = get(
        public_router(public_state(Readiness::new(true))),
        "/api/admin/v1/openapi.json",
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admin_router_does_not_expose_public_routes() {
    let response = get(admin_router(), "/health/live").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
