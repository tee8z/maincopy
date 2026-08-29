use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Serialize;
use utoipa::ToSchema;

use super::Readiness;

#[derive(Debug, Serialize, ToSchema)]
struct Health<Status> {
    status: Status,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum LivenessStatus {
    Live,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum ReadinessStatus {
    Ready,
    NotReady,
}

pub(super) async fn live() -> impl IntoResponse {
    Json(Health {
        status: LivenessStatus::Live,
    })
}

pub(super) async fn ready(State(readiness): State<Readiness>) -> impl IntoResponse {
    if readiness.is_ready() {
        (
            StatusCode::OK,
            Json(Health {
                status: ReadinessStatus::Ready,
            }),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(Health {
                status: ReadinessStatus::NotReady,
            }),
        )
    }
}
