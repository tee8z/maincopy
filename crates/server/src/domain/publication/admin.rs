use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{DefaultBodyLimit, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse as _, Response},
};
use maincopy_shared::publication::{IDEMPOTENCY_KEY_HEADER, PublishNowRequest, PublishNowResponse};
use serde::Serialize;
use time::UtcOffset;
use tokio::sync::Mutex;
use utoipa::ToSchema;
use utoipa_axum::{
    router::{UtoipaMethodRouter, UtoipaMethodRouterExt as _},
    routes,
};
use uuid::Uuid;

use crate::{
    admin::request_id::RequestId,
    content::{PostId, PostRevisionDigest},
    database::store::{DatabaseAdmissionError, DatabaseCommandError, DatabaseMutationError},
};

use super::{
    activation::{PublicationActivationError, PublicationCoordinator, PublishNow},
    store::PublishNowLookupError,
};

const MAX_PUBLICATION_REQUEST_BYTES: usize = 4 * 1024;

pub(crate) fn routes() -> UtoipaMethodRouter {
    routes!(create_publication).layer(DefaultBodyLimit::max(MAX_PUBLICATION_REQUEST_BYTES))
}

#[utoipa::path(
    post,
    path = "/api/admin/v1/publications",
    request_body = PublishNowRequest,
    params(
        ("Idempotency-Key" = Uuid, Header, description = "Canonical UUID identifying retries of this command")
    ),
    responses(
        (status = OK, description = "Publication completed or an earlier result was replayed", body = PublishNowResponse,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = BAD_REQUEST, description = "The command header or revision is invalid", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = NOT_FOUND, description = "The selected post does not exist", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = CONFLICT, description = "The command conflicts with publication state", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = PRECONDITION_FAILED, description = "The selected post revision is stale", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = PAYLOAD_TOO_LARGE, description = "The request body exceeds 4096 bytes", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = INTERNAL_SERVER_ERROR, description = "Publication snapshot construction failed", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = SERVICE_UNAVAILABLE, description = "Publication state is unavailable or outcome is uncertain", body = PublicationErrorEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID")))
    ),
    tag = "Publications"
)]
pub(crate) async fn create_publication(
    headers: HeaderMap,
    Extension(request_id): Extension<RequestId>,
    Extension(coordinator): Extension<Arc<Mutex<PublicationCoordinator>>>,
    body: Result<Json<PublishNowRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return problem(
                ErrorSpec::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_body_too_large",
                    "the request body must not exceed 4096 bytes",
                ),
                request_id,
            );
        }
        Err(_) => {
            return problem(
                ErrorSpec::bad_request(
                    "invalid_request_body",
                    "the request body must be valid publication JSON",
                ),
                request_id,
            );
        }
    };
    let creation_key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(spec) => return problem(spec, request_id),
    };
    let expected_revision = match request
        .expected_revision
        .as_deref()
        .map(PostRevisionDigest::parse)
        .transpose()
    {
        Ok(revision) => revision,
        Err(_) => {
            return problem(
                ErrorSpec::bad_request(
                    "invalid_expected_revision",
                    "expected_revision must be a complete Maincopy post revision digest",
                ),
                request_id,
            );
        }
    };
    let stable_post_id = PostId::parse(&request.post_id.hyphenated().to_string())
        .expect("a UUID has one canonical lowercase hyphenated representation");
    let result = coordinator
        .lock()
        .await
        .publish_now(PublishNow {
            creation_key,
            publication_id: Uuid::new_v4(),
            stable_post_id,
            expected_revision,
        })
        .await;

    match result {
        Ok(published) => Json(PublishNowResponse {
            publication_id: published.publication_id,
            post_id: published.stable_post_id.as_uuid(),
            revision: published.revision.as_str().into(),
            published_at: published.published_at.to_offset(UtcOffset::UTC),
            site_digest: published.site.digest.as_str().into(),
            site_version: published.site.version,
        })
        .into_response(),
        Err(error) => {
            let spec = activation_error(&error);
            if spec.status.is_server_error() {
                tracing::error!(
                    request_id = %request_id,
                    error = %error,
                    "admin publication request failed"
                );
            }
            problem(spec, request_id)
        }
    }
}

fn idempotency_key(headers: &HeaderMap) -> Result<Uuid, ErrorSpec> {
    let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER).iter();
    let value = values.next().ok_or_else(|| {
        ErrorSpec::bad_request("missing_idempotency_key", "Idempotency-Key is required")
    })?;
    if values.next().is_some() {
        return Err(ErrorSpec::bad_request(
            "invalid_idempotency_key",
            "Idempotency-Key must contain one canonical UUID",
        ));
    }
    let encoded = value.to_str().ok();
    let parsed = encoded.and_then(|value| Uuid::parse_str(value).ok());
    match (encoded, parsed) {
        (Some(encoded), Some(parsed)) if parsed.hyphenated().to_string() == encoded => Ok(parsed),
        _ => Err(ErrorSpec::bad_request(
            "invalid_idempotency_key",
            "Idempotency-Key must contain one canonical UUID",
        )),
    }
}

fn activation_error(error: &PublicationActivationError) -> ErrorSpec {
    match error {
        PublicationActivationError::PostNotFound { .. } => ErrorSpec::new(
            StatusCode::NOT_FOUND,
            "post_not_found",
            "the selected post is not present in the current content catalog",
        ),
        PublicationActivationError::DraftPost { .. } => ErrorSpec::conflict(
            "post_is_draft",
            "the selected post must be publishable before publication",
        ),
        PublicationActivationError::StaleRevision { .. } => ErrorSpec::new(
            StatusCode::PRECONDITION_FAILED,
            "stale_revision",
            "the current post revision does not match expected_revision",
        ),
        PublicationActivationError::AlreadyPublished { .. } => ErrorSpec::conflict(
            "already_published",
            "the selected post is already published",
        ),
        PublicationActivationError::Database(DatabaseMutationError::Admission(
            DatabaseAdmissionError::QueueFull | DatabaseAdmissionError::WriterClosed,
        ))
        | PublicationActivationError::Database(DatabaseMutationError::Command(
            DatabaseCommandError::OutcomeUnknown,
        ))
        | PublicationActivationError::DurableStateMismatch
        | PublicationActivationError::SnapshotActivationConflict
        | PublicationActivationError::CandidateDigestMismatch { .. } => ErrorSpec::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "publication_unavailable",
            "publication is temporarily unavailable",
        ),
        PublicationActivationError::Database(DatabaseMutationError::Command(
            DatabaseCommandError::IdempotencyConflict,
        ))
        | PublicationActivationError::Lookup(PublishNowLookupError::IdempotencyConflict) => {
            ErrorSpec::conflict(
                "idempotency_conflict",
                "Idempotency-Key is already bound to a different publication command",
            )
        }
        PublicationActivationError::Lookup(
            PublishNowLookupError::Query(_) | PublishNowLookupError::InvalidStoredState,
        ) => ErrorSpec::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "publication_unavailable",
            "publication is temporarily unavailable",
        ),
        PublicationActivationError::Database(DatabaseMutationError::Command(
            DatabaseCommandError::Rejected,
        )) => ErrorSpec::conflict(
            "publication_conflict",
            "the command conflicts with current publication state",
        ),
        PublicationActivationError::Database(DatabaseMutationError::Command(
            DatabaseCommandError::InvalidValue,
        )) => ErrorSpec::internal(
            "invalid_publication_state",
            "the publication command could not be represented safely",
        ),
        PublicationActivationError::SnapshotBuild(_) => ErrorSpec::internal(
            "snapshot_build_failed",
            "the publication snapshot could not be built",
        ),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ErrorSpec {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl ErrorSpec {
    const fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }

    const fn bad_request(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    const fn conflict(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    const fn internal(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, code, message)
    }
}

fn problem(spec: ErrorSpec, request_id: RequestId) -> Response {
    (
        spec.status,
        Json(PublicationErrorEnvelope {
            error: PublicationErrorBody {
                code: spec.code,
                message: spec.message,
                request_id: request_id.to_string().into_boxed_str(),
            },
        }),
    )
        .into_response()
}

#[derive(Serialize, ToSchema)]
struct PublicationErrorEnvelope {
    error: PublicationErrorBody,
}

#[derive(Serialize, ToSchema)]
struct PublicationErrorBody {
    code: &'static str,
    message: &'static str,
    request_id: Box<str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";

    #[test]
    fn idempotency_key_requires_one_canonical_uuid() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            idempotency_key(&headers).unwrap_err().code,
            "missing_idempotency_key"
        );

        headers.insert(IDEMPOTENCY_KEY_HEADER, KEY.parse().unwrap());
        assert_eq!(idempotency_key(&headers).unwrap().to_string(), KEY);

        for invalid in [
            "67E55044-10B1-426F-9247-BB680E5FE0C8",
            "67e5504410b1426f9247bb680e5fe0c8",
            "not-a-uuid",
        ] {
            headers.insert(IDEMPOTENCY_KEY_HEADER, invalid.parse().unwrap());
            assert_eq!(
                idempotency_key(&headers).unwrap_err().code,
                "invalid_idempotency_key"
            );
        }

        headers.insert(IDEMPOTENCY_KEY_HEADER, KEY.parse().unwrap());
        headers.append(
            IDEMPOTENCY_KEY_HEADER,
            Uuid::new_v4().to_string().parse().unwrap(),
        );
        assert_eq!(
            idempotency_key(&headers).unwrap_err().code,
            "invalid_idempotency_key"
        );
    }

    #[test]
    fn typed_activation_failures_have_stable_http_statuses_and_codes() {
        let post_id = PostId::parse("11111111-1111-4111-8111-111111111111").unwrap();
        let digest = PostRevisionDigest::from_bytes([0x11; 32]);
        let cases = [
            (
                PublicationActivationError::PostNotFound {
                    post_id: post_id.clone(),
                },
                StatusCode::NOT_FOUND,
                "post_not_found",
            ),
            (
                PublicationActivationError::DraftPost {
                    post_id: post_id.clone(),
                },
                StatusCode::CONFLICT,
                "post_is_draft",
            ),
            (
                PublicationActivationError::StaleRevision {
                    post_id,
                    expected: Box::new(digest.clone()),
                    current: Box::new(digest),
                },
                StatusCode::PRECONDITION_FAILED,
                "stale_revision",
            ),
            (
                PublicationActivationError::Database(DatabaseMutationError::Admission(
                    DatabaseAdmissionError::QueueFull,
                )),
                StatusCode::SERVICE_UNAVAILABLE,
                "publication_unavailable",
            ),
            (
                PublicationActivationError::Database(DatabaseMutationError::Command(
                    DatabaseCommandError::IdempotencyConflict,
                )),
                StatusCode::CONFLICT,
                "idempotency_conflict",
            ),
            (
                PublicationActivationError::Lookup(PublishNowLookupError::IdempotencyConflict),
                StatusCode::CONFLICT,
                "idempotency_conflict",
            ),
            (
                PublicationActivationError::Lookup(PublishNowLookupError::InvalidStoredState),
                StatusCode::SERVICE_UNAVAILABLE,
                "publication_unavailable",
            ),
        ];

        for (error, status, code) in cases {
            let spec = activation_error(&error);
            assert_eq!(spec.status, status);
            assert_eq!(spec.code, code);
        }
    }
}
