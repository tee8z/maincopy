use axum::{
    Json,
    extract::{
        DefaultBodyLimit, Path, Query,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse as _, Response},
};
use maincopy_shared::publication::{
    ChangeReleaseRequest, ListReleasesResponse, ReleaseBlockReason, ReleaseOperationResource,
    ReleaseResource, ReleaseState,
};
use serde::Deserialize;
use time::OffsetDateTime;
use utoipa_axum::{
    router::{UtoipaMethodRouter, UtoipaMethodRouterExt as _},
    routes,
};
use uuid::Uuid;

use super::{
    AvailablePublication, ErrorSpec, MAX_PUBLICATION_REQUEST_BYTES, activation_error,
    idempotency_key, problem, wire_preview_digest,
};
use crate::admin::{problem::AdminProblemEnvelope, request_id::RequestId};
use crate::domain::publication::{
    ActivationBlockReason, CanonicalState,
    activation::{PublicationCoordinatorHandle, ReleaseTransitionError, RetryRelease},
    store::{
        ChangeRelease, ReleaseChange, ReleaseChangeReceipt, ReleaseCommandError, ReleaseLoadError,
        ReleaseMutationError, ReleaseView,
    },
};

pub(crate) fn list_routes() -> UtoipaMethodRouter {
    routes!(list_releases)
}
pub(crate) fn item_routes() -> UtoipaMethodRouter {
    routes!(get_release, change_release).layer(DefaultBodyLimit::max(MAX_PUBLICATION_REQUEST_BYTES))
}
pub(crate) fn operation_routes() -> UtoipaMethodRouter {
    routes!(get_operation)
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseQuery {
    cursor: Option<Uuid>,
}

#[utoipa::path(get, path = "/api/admin/v1/releases",
    params(("cursor" = Option<Uuid>, Query, description = "Publication UUID from next_cursor; pages contain at most 100 releases")),
    responses((status = OK, body = ListReleasesResponse), (status = BAD_REQUEST, body = AdminProblemEnvelope), (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope)), tag = "Releases")]
async fn list_releases(
    request_id: RequestId,
    query: Result<Query<ReleaseQuery>, QueryRejection>,
    AvailablePublication(coordinator): AvailablePublication,
) -> Response {
    let Ok(Query(query)) = query else {
        return invalid_input(request_id);
    };
    match coordinator.releases(query.cursor).await {
        Ok(mut releases) => {
            let next_cursor = if releases.len() > 100 {
                Some(releases[99].publication_id)
            } else {
                None
            };
            releases.truncate(100);
            Json(ListReleasesResponse {
                releases: releases.into_iter().map(release_resource).collect(),
                next_cursor,
            })
            .into_response()
        }
        Err(error) => load_problem(error, request_id),
    }
}

#[utoipa::path(get, path = "/api/admin/v1/releases/{publication_id}",
    params(("publication_id" = Uuid, Path)),
    responses((status = OK, body = ReleaseResource), (status = BAD_REQUEST, body = AdminProblemEnvelope), (status = NOT_FOUND, body = AdminProblemEnvelope), (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope)), tag = "Releases")]
async fn get_release(
    request_id: RequestId,
    id: Result<Path<Uuid>, PathRejection>,
    AvailablePublication(coordinator): AvailablePublication,
) -> Response {
    let Ok(Path(id)) = id else {
        return invalid_input(request_id);
    };
    match coordinator.release(id).await {
        Ok(Some(release)) => Json(release_resource(release)).into_response(),
        Ok(None) => problem(
            ErrorSpec::new(
                StatusCode::NOT_FOUND,
                "release_not_found",
                "the release does not exist",
            ),
            request_id,
        ),
        Err(error) => load_problem(error, request_id),
    }
}

#[utoipa::path(get, path = "/api/admin/v1/release-operations/{operation_id}",
    params(("operation_id" = Uuid, Path)),
    responses((status = OK, description = "Immutable accepted result; inspect the release for current state", body = ReleaseOperationResource), (status = BAD_REQUEST, body = AdminProblemEnvelope), (status = NOT_FOUND, body = AdminProblemEnvelope), (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope)), tag = "Releases")]
async fn get_operation(
    request_id: RequestId,
    id: Result<Path<Uuid>, PathRejection>,
    AvailablePublication(coordinator): AvailablePublication,
) -> Response {
    let Ok(Path(id)) = id else {
        return invalid_input(request_id);
    };
    match coordinator.release_operation(id).await {
        Ok(Some(receipt)) => Json(operation_resource(receipt)).into_response(),
        Ok(None) => problem(
            ErrorSpec::new(
                StatusCode::NOT_FOUND,
                "release_operation_not_found",
                "no accepted release operation exists for this identifier",
            ),
            request_id,
        ),
        Err(error) => load_problem(error, request_id),
    }
}

#[utoipa::path(post, path = "/api/admin/v1/releases/{publication_id}",
    params(("publication_id" = Uuid, Path), ("Idempotency-Key" = Uuid, Header, description = "Stable operation UUID; reuse only with the identical command")),
    request_body = ChangeReleaseRequest,
    responses(
        (status = OK, description = "Immutable accepted result; retry acceptance can precede final publication", body = ReleaseOperationResource),
        (status = BAD_REQUEST, body = AdminProblemEnvelope),
        (status = NOT_FOUND, body = AdminProblemEnvelope),
        (status = CONFLICT, body = AdminProblemEnvelope),
        (status = PRECONDITION_FAILED, body = AdminProblemEnvelope),
        (status = PAYLOAD_TOO_LARGE, body = AdminProblemEnvelope),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope),
        (status = INTERNAL_SERVER_ERROR, body = AdminProblemEnvelope)
    ), tag = "Releases")]
async fn change_release(
    request_id: RequestId,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    AvailablePublication(coordinator): AvailablePublication,
    body: Result<Json<ChangeReleaseRequest>, JsonRejection>,
) -> Response {
    let Ok(Path(publication_id)) = id else {
        return invalid_input(request_id);
    };
    let operation_id = match idempotency_key(&headers) {
        Ok(id) => id,
        Err(spec) => return problem(spec, request_id),
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(error) => return invalid_change_body(error, request_id),
    };
    match apply_control(&coordinator, publication_id, operation_id, request).await {
        Ok(receipt) => Json(operation_resource(receipt)).into_response(),
        Err(error) => {
            let spec = transition_error(&error);
            if spec.status.is_server_error() {
                tracing::error!(%request_id, %publication_id, %operation_id, %error, "admin release control failed");
            }
            problem(spec, request_id)
        }
    }
}

async fn apply_control(
    coordinator: &PublicationCoordinatorHandle,
    publication_id: Uuid,
    operation_id: Uuid,
    request: ChangeReleaseRequest,
) -> Result<ReleaseChangeReceipt, ReleaseTransitionError> {
    let now = OffsetDateTime::now_utc();
    let (expected_version, change) = match request {
        ChangeReleaseRequest::Retry { expected_version } => {
            return coordinator
                .retry_release(RetryRelease {
                    operation_id,
                    publication_id,
                    expected_version,
                    now,
                })
                .await;
        }
        ChangeReleaseRequest::Reschedule {
            expected_version,
            scheduled_for,
        } => (
            expected_version,
            ReleaseChange::Reschedule {
                scheduled_at: scheduled_for,
            },
        ),
        ChangeReleaseRequest::Cancel { expected_version } => {
            (expected_version, ReleaseChange::Cancel)
        }
    };
    coordinator
        .change_release(ChangeRelease {
            operation_id,
            publication_id,
            expected_version,
            change,
            now,
        })
        .await
}

fn invalid_change_body(error: JsonRejection, request_id: RequestId) -> Response {
    let spec = if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ErrorSpec::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_body_too_large",
            "the request body must not exceed 4096 bytes",
        )
    } else {
        ErrorSpec::bad_request(
            "invalid_release_change",
            "provide a known action, an exact positive version, and a UTC time when rescheduling",
        )
    };
    problem(spec, request_id)
}

fn transition_error(error: &ReleaseTransitionError) -> ErrorSpec {
    match error {
        ReleaseTransitionError::Activation(error) => activation_error(error),
        ReleaseTransitionError::Mutation(ReleaseMutationError::Command(error)) => {
            command_error(*error)
        }
        ReleaseTransitionError::Mutation(ReleaseMutationError::Admission(_))
        | ReleaseTransitionError::Unavailable(_) => ErrorSpec::unavailable(
            "release_unavailable",
            "release management is temporarily unavailable",
        ),
        ReleaseTransitionError::Load(_) => ErrorSpec::internal(
            "invalid_release_state",
            "stored release state could not be validated",
        ),
    }
}

fn command_error(error: ReleaseCommandError) -> ErrorSpec {
    match error {
        ReleaseCommandError::NotFound => ErrorSpec::new(
            StatusCode::NOT_FOUND,
            "release_not_found",
            "the release does not exist",
        ),
        ReleaseCommandError::StaleVersion => ErrorSpec::new(
            StatusCode::PRECONDITION_FAILED,
            "stale_release_version",
            "the release changed; inspect its current version before starting a new operation",
        ),
        ReleaseCommandError::InvalidState => ErrorSpec::conflict(
            "release_state_conflict",
            "this action is unavailable in the current release state",
        ),
        ReleaseCommandError::IdempotencyConflict => ErrorSpec::conflict(
            "idempotency_conflict",
            "Idempotency-Key is already bound to a different release command",
        ),
        ReleaseCommandError::InvalidValue => ErrorSpec::bad_request(
            "invalid_release_change",
            "rescheduling requires a future UTC time and a valid resource version",
        ),
        ReleaseCommandError::OutcomeUnknown => ErrorSpec::unavailable(
            "release_outcome_unknown",
            "repeat the identical command or inspect its operation identifier to recover the accepted result",
        ),
    }
}

fn operation_resource(receipt: ReleaseChangeReceipt) -> ReleaseOperationResource {
    ReleaseOperationResource {
        operation_id: receipt.operation_id,
        publication_id: receipt.publication_id,
        version: receipt.version,
        state: release_state(receipt.state),
    }
}

fn release_resource(release: ReleaseView) -> ReleaseResource {
    let publication = release.publication;
    ReleaseResource {
        publication_id: release.publication_id,
        post_id: publication.stable_post_id.as_uuid(),
        preview_digest: wire_preview_digest(&release.accepted_preview_digest),
        revision: publication.pinned_post_digest.to_string().into_boxed_str(),
        state: release_state(publication.state),
        version: publication.version,
        scheduled_for: publication.scheduled_at,
        published_at: publication.published_at,
        block_reason: publication.block_reason.map(|reason| match reason {
            ActivationBlockReason::RevisionUnavailable => ReleaseBlockReason::RevisionUnavailable,
            ActivationBlockReason::PreviewChanged => ReleaseBlockReason::PreviewChanged,
        }),
    }
}

fn release_state(state: CanonicalState) -> ReleaseState {
    match state {
        CanonicalState::Scheduled => ReleaseState::Scheduled,
        CanonicalState::Activating => ReleaseState::Activating,
        CanonicalState::Blocked => ReleaseState::Blocked,
        CanonicalState::Published => ReleaseState::Published,
        CanonicalState::Superseded => ReleaseState::Superseded,
        CanonicalState::Cancelled => ReleaseState::Cancelled,
    }
}

fn invalid_input(request_id: RequestId) -> Response {
    problem(
        ErrorSpec::bad_request(
            "invalid_release_query",
            "release identifiers and cursors must be UUIDs; unknown query fields are rejected",
        ),
        request_id,
    )
}

fn load_problem(error: ReleaseLoadError, request_id: RequestId) -> Response {
    let spec = match error {
        ReleaseLoadError::Database(_) => ErrorSpec::unavailable(
            "release_unavailable",
            "release inspection is temporarily unavailable",
        ),
        ReleaseLoadError::InvalidOperation | ReleaseLoadError::InvalidStoredRelease(_) => {
            ErrorSpec::internal(
                "invalid_release_state",
                "stored release state could not be validated",
            )
        }
    };
    problem(spec, request_id)
}
