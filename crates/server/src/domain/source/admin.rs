//! Versioned administration resources for managed source synchronization.

use axum::{
    Json,
    extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Path, Query, Request},
    http::{HeaderMap, HeaderValue, StatusCode, header::LOCATION, request::Parts},
    response::{IntoResponse as _, Response},
};
use maincopy_shared::{
    auth::AdminAuditEventId,
    publication::IDEMPOTENCY_KEY_HEADER,
    source::{
        BeginSourceSyncResponse, ListSourceSyncsResponse, SourceStatusResponse, SourceSyncId,
        SourceSyncResource,
    },
};
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{
    router::{UtoipaMethodRouter, UtoipaMethodRouterExt as _},
    routes,
};
use uuid::Uuid;

use crate::{
    admin::{
        principal::{AdminAuthentication, AdminPrincipal},
        problem::{AdminProblem, AdminProblemEnvelope, problem_response},
        request_id::RequestId,
    },
    database::store::{DatabaseAdmissionError, DatabaseCommandError, DatabaseMutationError},
    domain::{
        auth::store::{AdminMutationKey, AuditPrincipalReference, MutationAuditContext},
        source::store::SourceLoadError,
    },
    source_sync::{SourceControlError, SourceSyncHandle, accepted_status},
};

const MAX_SOURCE_SYNC_REQUEST_BYTES: usize = 4 * 1024;
const DEFAULT_SOURCE_SYNC_PAGE_LIMIT: u16 = 20;
const MAX_SOURCE_SYNC_PAGE_LIMIT: u16 = 100;
const RETRY_AFTER_ONE_SECOND: HeaderValue = HeaderValue::from_static("1");

pub(crate) fn status_routes() -> UtoipaMethodRouter {
    routes!(get_source_status)
}

pub(crate) fn sync_list_routes() -> UtoipaMethodRouter {
    routes!(list_source_syncs)
}

pub(crate) fn sync_item_routes() -> UtoipaMethodRouter {
    routes!(get_source_sync)
}

pub(crate) fn sync_mutation_routes() -> UtoipaMethodRouter {
    routes!(begin_source_sync).layer(DefaultBodyLimit::max(MAX_SOURCE_SYNC_REQUEST_BYTES))
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListSourceSyncsQuery {
    cursor: Option<Box<str>>,
    limit: Option<u16>,
}

struct SourceSyncPage {
    cursor: Option<SourceSyncId>,
    limit: usize,
}

impl<S> FromRequestParts<S> for SourceSyncPage
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = request_id(parts, state).await?;
        let Query(query) = Query::<ListSourceSyncsQuery>::from_request_parts(parts, state)
            .await
            .map_err(|_| {
                problem(
                    AdminProblem::bad_request(
                        "invalid_source_sync_query",
                        "cursor and limit must use valid source synchronization pagination values",
                    ),
                    request_id,
                )
            })?;
        let limit = query.limit.unwrap_or(DEFAULT_SOURCE_SYNC_PAGE_LIMIT);
        if !(1..=MAX_SOURCE_SYNC_PAGE_LIMIT).contains(&limit) {
            return Err(problem(
                AdminProblem::bad_request(
                    "invalid_source_sync_limit",
                    "limit must be between 1 and 100",
                ),
                request_id,
            ));
        }
        let cursor = match query.cursor.as_deref() {
            Some(encoded) => Some(canonical_uuid(encoded).ok_or_else(|| {
                problem(
                    AdminProblem::bad_request(
                        "invalid_source_sync_cursor",
                        "cursor must be one canonical lowercase hyphenated UUID",
                    ),
                    request_id,
                )
            })?),
            None => None,
        };
        Ok(Self {
            cursor: cursor.map(SourceSyncId::from_uuid),
            limit: usize::from(limit),
        })
    }
}

struct SourceSyncIdentifier(SourceSyncId);

impl<S> FromRequestParts<S> for SourceSyncIdentifier
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = request_id(parts, state).await?;
        let Path(encoded) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| invalid_sync_id(request_id))?;
        canonical_uuid(&encoded)
            .map(SourceSyncId::from_uuid)
            .map(Self)
            .ok_or_else(|| invalid_sync_id(request_id))
    }
}

struct AvailableSourceControl(SourceSyncHandle);

impl<S> FromRequestParts<S> for AvailableSourceControl
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = request_id(parts, state).await?;
        parts
            .extensions
            .get::<SourceSyncHandle>()
            .cloned()
            .map(Self)
            .ok_or_else(|| unavailable(request_id))
    }
}

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct BeginSourceSyncRequest {}

struct SourceSyncCommand {
    request_id: RequestId,
    handle: SourceSyncHandle,
    audit: MutationAuditContext,
}

impl<S> FromRequest<S> for SourceSyncCommand
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let (mut parts, body) = request.into_parts();
        let request_id = request_id(&mut parts, state).await?;
        let principal = AdminPrincipal::from_request_parts(&mut parts, state)
            .await
            .map_err(|error| error.into_response())?;
        let headers = parts.headers.clone();
        let handle = parts.extensions.get::<SourceSyncHandle>().cloned();
        let request = Request::from_parts(parts, body);
        let Json(BeginSourceSyncRequest {}) =
            Json::<BeginSourceSyncRequest>::from_request(request, state)
                .await
                .map_err(|rejection| source_sync_json_rejection(rejection.status(), request_id))?;
        let idempotency_key =
            source_sync_idempotency_key(&headers).map_err(|spec| problem(spec, request_id))?;
        let handle = handle.ok_or_else(|| unavailable(request_id))?;
        Ok(Self {
            request_id,
            handle,
            audit: mutation_audit(&principal, request_id, idempotency_key),
        })
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/v1/source",
    responses(
        (status = OK, description = "Current redacted source configuration and synchronization state", body = SourceStatusResponse,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID")))
    ),
    tag = "Source"
)]
async fn get_source_status(
    request_id: RequestId,
    AvailableSourceControl(handle): AvailableSourceControl,
) -> Response {
    match handle.status().await {
        Ok(status) => Json(status).into_response(),
        Err(error) => source_control_problem(error, request_id),
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/v1/source-syncs",
    params(
        ("cursor" = Option<Uuid>, Query, description = "Stable operation UUID returned as next_cursor by the previous page"),
        ("limit" = Option<u16>, Query, description = "Page size from 1 through 100; defaults to 20")
    ),
    responses(
        (status = OK, description = "Durable source synchronizations in newest-first order", body = ListSourceSyncsResponse,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = BAD_REQUEST, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID")))
    ),
    tag = "Source"
)]
async fn list_source_syncs(
    request_id: RequestId,
    SourceSyncPage { cursor, limit }: SourceSyncPage,
    AvailableSourceControl(handle): AvailableSourceControl,
) -> Response {
    match handle.list(cursor, limit).await {
        Ok(page) => Json(page).into_response(),
        Err(error) => source_control_problem(error, request_id),
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/v1/source-syncs/{source_sync_id}",
    params(
        ("source_sync_id" = Uuid, Path, description = "Canonical durable source synchronization UUID")
    ),
    responses(
        (status = OK, description = "One durable source synchronization", body = SourceSyncResource,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = BAD_REQUEST, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = NOT_FOUND, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID")))
    ),
    tag = "Source"
)]
async fn get_source_sync(
    request_id: RequestId,
    SourceSyncIdentifier(source_sync_id): SourceSyncIdentifier,
    AvailableSourceControl(handle): AvailableSourceControl,
) -> Response {
    match handle.sync(source_sync_id).await {
        Ok(Some(sync)) => Json(sync).into_response(),
        Ok(None) => problem(
            AdminProblem::new(
                StatusCode::NOT_FOUND,
                "source_sync_not_found",
                "the requested source synchronization does not exist",
            ),
            request_id,
        ),
        Err(error) => source_control_problem(error, request_id),
    }
}

#[utoipa::path(
    post,
    path = "/api/admin/v1/source-syncs",
    request_body = BeginSourceSyncRequest,
    params(
        ("Idempotency-Key" = Uuid, Header, description = "Canonical UUID identifying retries of this synchronization request")
    ),
    responses(
        (status = OK, description = "A prior command result was replayed", body = BeginSourceSyncResponse,
            headers(
                ("location" = String, description = "Durable operation resource"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            )),
        (status = ACCEPTED, description = "A synchronization was created or coalesced onto the active operation", body = BeginSourceSyncResponse,
            headers(
                ("location" = String, description = "Durable operation resource"),
                ("x-request-id" = Uuid, description = "Request correlation ID")
            )),
        (status = BAD_REQUEST, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = CONFLICT, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = PAYLOAD_TOO_LARGE, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = INTERNAL_SERVER_ERROR, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID")))
    ),
    tag = "Source"
)]
async fn begin_source_sync(command: SourceSyncCommand) -> Response {
    let SourceSyncCommand {
        request_id,
        handle,
        audit,
    } = command;
    match handle.begin_manual(audit).await {
        Ok(accepted) => {
            let status = accepted_status(accepted.admission);
            let location = format!(
                "/api/admin/v1/source-syncs/{}",
                accepted.sync.source_sync_id
            );
            let mut response = (status, Json(accepted)).into_response();
            match HeaderValue::from_str(&location) {
                Ok(location) => {
                    response.headers_mut().insert(LOCATION, location);
                    response
                }
                Err(error) => {
                    tracing::error!(%request_id, error = %error, "typed source sync location was not a valid header");
                    problem(
                        AdminProblem::internal(
                            "source_sync_response_invalid",
                            "the source synchronization response could not be represented safely",
                        ),
                        request_id,
                    )
                }
            }
        }
        Err(error) => source_control_problem(error, request_id),
    }
}

fn source_sync_json_rejection(status: StatusCode, request_id: RequestId) -> Response {
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        problem(
            AdminProblem::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "source_sync_request_too_large",
                "the source synchronization request body exceeds 4096 bytes",
            ),
            request_id,
        )
    } else {
        problem(
            AdminProblem::bad_request(
                "invalid_source_sync_request",
                "the source synchronization request body must be one empty JSON object",
            ),
            request_id,
        )
    }
}

fn source_sync_idempotency_key(headers: &HeaderMap) -> Result<AdminMutationKey, AdminProblem> {
    let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER).iter();
    let Some(value) = values.next() else {
        return Err(AdminProblem::bad_request(
            "missing_idempotency_key",
            "Idempotency-Key is required for source synchronization requests",
        ));
    };
    if values.next().is_some() {
        return Err(invalid_idempotency_key());
    }
    let encoded = value.to_str().map_err(|_| invalid_idempotency_key())?;
    let key = canonical_uuid(encoded).ok_or_else(invalid_idempotency_key)?;
    Ok(AdminMutationKey(key))
}

fn canonical_uuid(encoded: &str) -> Option<Uuid> {
    let uuid = Uuid::parse_str(encoded).ok()?;
    (uuid.hyphenated().to_string() == encoded).then_some(uuid)
}

fn invalid_idempotency_key() -> AdminProblem {
    AdminProblem::bad_request(
        "invalid_idempotency_key",
        "Idempotency-Key must be one canonical lowercase hyphenated UUID",
    )
}

fn invalid_sync_id(request_id: RequestId) -> Response {
    problem(
        AdminProblem::bad_request(
            "invalid_source_sync_id",
            "source_sync_id must be one canonical lowercase hyphenated UUID",
        ),
        request_id,
    )
}

fn mutation_audit(
    principal: &AdminPrincipal,
    request_id: RequestId,
    idempotency_key: AdminMutationKey,
) -> MutationAuditContext {
    let principal = match principal.authentication {
        AdminAuthentication::BrowserSession { session_id } => {
            AuditPrincipalReference::BrowserSession {
                user_id: principal.user_id,
                session_id,
            }
        }
        AdminAuthentication::AgentCredential { credential_id } => {
            AuditPrincipalReference::AgentCredential {
                user_id: principal.user_id,
                credential_id,
            }
        }
    };
    MutationAuditContext {
        audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
        principal,
        request_id: Some(request_id.0),
        idempotency_key,
    }
}

fn source_control_problem(error: SourceControlError, request_id: RequestId) -> Response {
    let spec = match error {
        SourceControlError::Unsupported => AdminProblem::conflict(
            "source_sync_unsupported",
            "manual synchronization is unavailable in external-checkout source mode",
        ),
        SourceControlError::ShuttingDown => source_unavailable(),
        SourceControlError::ConfigurationUnavailable => source_unavailable(),
        SourceControlError::Load(SourceLoadError::CursorNotFound) => AdminProblem::bad_request(
            "invalid_source_sync_cursor",
            "the source synchronization cursor does not exist",
        ),
        SourceControlError::Load(SourceLoadError::InvalidPageLimit) => AdminProblem::internal(
            "source_sync_pagination_invalid",
            "the source synchronization page could not be represented safely",
        ),
        SourceControlError::Load(SourceLoadError::Query(_) | SourceLoadError::Corrupt { .. }) => {
            source_unavailable()
        }
        SourceControlError::Mutation(DatabaseMutationError::Admission(
            DatabaseAdmissionError::QueueFull | DatabaseAdmissionError::WriterClosed,
        ))
        | SourceControlError::Mutation(DatabaseMutationError::Command(
            DatabaseCommandError::OutcomeUnknown,
        )) => source_unavailable(),
        SourceControlError::Mutation(DatabaseMutationError::Command(
            DatabaseCommandError::IdempotencyConflict,
        )) => AdminProblem::conflict(
            "idempotency_key_conflict",
            "Idempotency-Key is already bound to a different source synchronization command",
        ),
        SourceControlError::Mutation(DatabaseMutationError::Command(
            DatabaseCommandError::Rejected,
        )) => AdminProblem::conflict(
            "source_sync_conflict",
            "the source synchronization request conflicts with current durable state",
        ),
        SourceControlError::Mutation(DatabaseMutationError::Command(
            DatabaseCommandError::InvalidValue,
        )) => AdminProblem::internal(
            "source_sync_state_invalid",
            "the source synchronization command could not be represented safely",
        ),
    };
    if spec.status.is_server_error() {
        tracing::error!(%request_id, error = %error, "source synchronization administration failed");
    }
    problem(spec, request_id)
}

const fn source_unavailable() -> AdminProblem {
    AdminProblem::unavailable(
        "source_unavailable",
        "source synchronization state is temporarily unavailable",
    )
}

fn unavailable(request_id: RequestId) -> Response {
    problem(source_unavailable(), request_id)
}

fn problem(spec: AdminProblem, request_id: RequestId) -> Response {
    let mut response = problem_response(spec, request_id);
    if spec.status == StatusCode::SERVICE_UNAVAILABLE {
        response
            .headers_mut()
            .insert("retry-after", RETRY_AFTER_ONE_SECOND);
    }
    response
}

async fn request_id<S>(parts: &mut Parts, state: &S) -> Result<RequestId, Response>
where
    S: Send + Sync,
{
    RequestId::from_request_parts(parts, state)
        .await
        .map_err(|error| error.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_keys_are_single_canonical_uuids() {
        let canonical = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let mut headers = HeaderMap::new();
        assert_eq!(
            source_sync_idempotency_key(&headers).unwrap_err().code,
            "missing_idempotency_key"
        );
        headers.insert(IDEMPOTENCY_KEY_HEADER, HeaderValue::from_static(canonical));
        assert_eq!(
            source_sync_idempotency_key(&headers),
            Ok(AdminMutationKey(Uuid::parse_str(canonical).unwrap()))
        );
        for invalid in [
            "67E55044-10B1-426F-9247-BB680E5FE0C8",
            "67e5504410b1426f9247bb680e5fe0c8",
            "not-a-uuid",
        ] {
            headers.insert(
                IDEMPOTENCY_KEY_HEADER,
                HeaderValue::from_str(invalid).unwrap(),
            );
            assert_eq!(
                source_sync_idempotency_key(&headers).unwrap_err().code,
                "invalid_idempotency_key"
            );
        }
        headers.insert(IDEMPOTENCY_KEY_HEADER, HeaderValue::from_static(canonical));
        headers.append(IDEMPOTENCY_KEY_HEADER, HeaderValue::from_static(canonical));
        assert_eq!(
            source_sync_idempotency_key(&headers).unwrap_err().code,
            "invalid_idempotency_key"
        );
    }
}
