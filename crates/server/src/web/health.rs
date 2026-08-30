use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use serde::Serialize;
use utoipa::ToSchema;

use super::Readiness;

#[derive(Debug, Serialize, ToSchema)]
struct Health {
    status: HealthStatus,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum HealthStatus {
    Live,
    Ready,
    NotReady,
}

pub(super) fn router(readiness: Readiness) -> Router {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .with_state(readiness)
}

async fn live() -> impl IntoResponse {
    Json(Health {
        status: HealthStatus::Live,
    })
}

async fn ready(State(readiness): State<Readiness>) -> impl IntoResponse {
    let (status_code, status) = if readiness.is_ready() {
        (StatusCode::OK, HealthStatus::Ready)
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, HealthStatus::NotReady)
    };
    (status_code, Json(Health { status }))
}
