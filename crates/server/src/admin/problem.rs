use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse as _, Response},
};
use serde::Serialize;
use utoipa::ToSchema;

use super::request_id::RequestId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AdminProblem {
    pub(crate) status: StatusCode,
    pub(crate) code: &'static str,
    pub(crate) message: &'static str,
}

impl AdminProblem {
    pub(crate) const fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }

    pub(crate) const fn bad_request(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub(crate) const fn forbidden(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, message)
    }

    pub(crate) const fn conflict(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    pub(crate) const fn internal(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, code, message)
    }

    pub(crate) const fn unavailable(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, code, message)
    }
}

pub(crate) fn problem_response(problem: AdminProblem, request_id: RequestId) -> Response {
    (
        problem.status,
        Json(AdminProblemEnvelope {
            error: AdminProblemBody {
                code: problem.code,
                message: problem.message,
                request_id: request_id.to_string().into_boxed_str(),
            },
        }),
    )
        .into_response()
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AdminProblemEnvelope {
    error: AdminProblemBody,
}

#[derive(Serialize, ToSchema)]
struct AdminProblemBody {
    code: &'static str,
    message: &'static str,
    request_id: Box<str>,
}

#[cfg(test)]
mod tests {
    use axum::{body::to_bytes, http::StatusCode};
    use serde_json::Value;
    use uuid::Uuid;

    use super::*;

    #[tokio::test]
    async fn response_preserves_the_stable_problem_contract() {
        let request_id = RequestId(Uuid::new_v4());
        let response = problem_response(
            AdminProblem {
                status: StatusCode::CONFLICT,
                code: "resource_conflict",
                message: "the resource changed",
            },
            request_id,
        );

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 1024)
                .await
                .expect("the problem body must be bounded"),
        )
        .expect("the problem body must be JSON");
        assert_eq!(body["error"]["code"], "resource_conflict");
        assert_eq!(body["error"]["message"], "the resource changed");
        assert_eq!(body["error"]["request_id"], request_id.to_string());
    }
}
