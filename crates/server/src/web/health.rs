use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Serialize;
use utoipa::ToSchema;

use super::PublicState;

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

pub(super) async fn live() -> impl IntoResponse {
    Json(Health {
        status: HealthStatus::Live,
    })
}

pub(super) async fn ready(State(state): State<PublicState>) -> impl IntoResponse {
    let (status_code, status) = if state.readiness.is_ready() {
        (StatusCode::OK, HealthStatus::Ready)
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, HealthStatus::NotReady)
    };
    (status_code, Json(Health { status }))
}
