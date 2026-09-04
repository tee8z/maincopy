use axum::{
    Json,
    extract::{
        Path, Query,
        rejection::{PathRejection, QueryRejection},
    },
    http::StatusCode,
    response::{IntoResponse as _, Response},
};
use maincopy_shared::publication::{
    ListReleasesResponse, ReleaseBlockReason, ReleaseOperationResource, ReleaseResource,
    ReleaseState,
};
use serde::Deserialize;
use utoipa_axum::{router::UtoipaMethodRouter, routes};
use uuid::Uuid;

use super::{AvailablePublication, ErrorSpec, problem, wire_preview_digest};
use crate::admin::{problem::AdminProblemEnvelope, request_id::RequestId};
use crate::domain::publication::{
    ActivationBlockReason, CanonicalState,
    store::{ReleaseLoadError, ReleaseView},
};

pub(crate) fn list_routes() -> UtoipaMethodRouter {
    routes!(list_releases)
}
pub(crate) fn item_routes() -> UtoipaMethodRouter {
    routes!(get_release)
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
        Ok(Some(receipt)) => Json(ReleaseOperationResource {
            operation_id: receipt.operation_id,
            publication_id: receipt.publication_id,
            version: receipt.version,
            state: release_state(receipt.state),
        })
        .into_response(),
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
