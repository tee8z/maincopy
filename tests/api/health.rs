use axum::http::StatusCode;
use maincopy::web::{Readiness, public_router};
use serde_json::json;

use crate::helpers::{get, json_body};

#[tokio::test]
async fn liveness_reports_that_the_process_is_running() {
    let response = get(public_router(Readiness::default()), "/health/live").await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await, json!({ "status": "live" }));
}

#[tokio::test]
async fn readiness_is_unavailable_before_startup_completes() {
    let response = get(public_router(Readiness::default()), "/health/ready").await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(response).await, json!({ "status": "not_ready" }));
}

#[tokio::test]
async fn readiness_tracks_application_state() {
    let readiness = Readiness::default();
    let app = public_router(readiness.clone());

    readiness.mark_ready();
    let ready_response = get(app.clone(), "/health/ready").await;
    assert_eq!(ready_response.status(), StatusCode::OK);
    assert_eq!(
        json_body(ready_response).await,
        json!({ "status": "ready" })
    );

    readiness.mark_not_ready();
    let not_ready_response = get(app, "/health/ready").await;
    assert_eq!(not_ready_response.status(), StatusCode::SERVICE_UNAVAILABLE);
}
