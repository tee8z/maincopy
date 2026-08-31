use axum::{
    Json,
    extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Request, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, request::Parts},
    response::{IntoResponse as _, Response},
};
use maincopy_shared::{
    auth::AdminAuditEventId,
    profile_api::{
        ActiveTipRecipientResponse, PutActiveTipRecipientRequest, UpdateUserProfileRequest,
        UserProfileResponse,
    },
    publication::IDEMPOTENCY_KEY_HEADER,
};
use serde::de::DeserializeOwned;
use time::{OffsetDateTime, UtcOffset};
use utoipa_axum::{
    router::{UtoipaMethodRouter, UtoipaMethodRouterExt as _},
    routes,
};
use uuid::Uuid;

use super::{
    principal::{AdminAuthentication, AdminPrincipal},
    problem::{AdminProblem, AdminProblemEnvelope, problem_response},
    request_id::RequestId,
};
use crate::{
    database::store::DatabaseAdmissionError,
    domain::{
        auth::store::{AdminMutationKey, AuditPrincipalReference, MutationAuditContext},
        profile::{
            ProfileCommandError, ProfileLoadError, ProfileMutationError, ProfilePrecondition,
            ProfileStore, SetTipRecipient, StoredTipRecipientSetting, StoredUserProfile,
            UpdateProfile,
        },
        publication::activation::{
            ProfileTransitionError, PublicationCoordinatorHandle, PublicationCoordinatorUnavailable,
        },
    },
};

const PROFILE_REQUEST_BODY_LIMIT: usize = 8 * 1024;
const RETRY_AFTER_ONE_SECOND: HeaderValue = HeaderValue::from_static("1");

pub(super) fn profile_routes() -> UtoipaMethodRouter {
    routes!(get_current_profile, put_current_profile)
        .layer(DefaultBodyLimit::max(PROFILE_REQUEST_BODY_LIMIT))
}

pub(super) fn tip_recipient_routes() -> UtoipaMethodRouter {
    routes!(get_active_tip_recipient, put_active_tip_recipient)
        .layer(DefaultBodyLimit::max(PROFILE_REQUEST_BODY_LIMIT))
}

#[utoipa::path(
    get,
    path = "/api/admin/v1/profile",
    responses(
        (status = OK, body = UserProfileResponse),
        (status = NOT_FOUND, body = AdminProblemEnvelope),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope)
    ),
    tag = "Profiles"
)]
async fn get_current_profile(
    request_id: RequestId,
    principal: AdminPrincipal,
    AvailableProfileStore(store): AvailableProfileStore,
) -> Response {
    match store.profile(principal.user_id).await {
        Ok(Some(profile)) => Json(profile_response(profile)).into_response(),
        Ok(None) => not_found(request_id),
        Err(error) => load_problem(error, request_id),
    }
}

#[utoipa::path(
    put,
    path = "/api/admin/v1/profile",
    request_body = UpdateUserProfileRequest,
    responses(
        (status = OK, body = UserProfileResponse,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = CREATED, body = UserProfileResponse,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = BAD_REQUEST, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = PAYLOAD_TOO_LARGE, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = FORBIDDEN, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = NOT_FOUND, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = CONFLICT, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = PRECONDITION_FAILED, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID")))
    ),
    params(
        ("Idempotency-Key" = Uuid, Header, description = "Canonical UUID identifying retries of this command")
    ),
    tag = "Profiles"
)]
async fn put_current_profile(
    ProfileCommand {
        request_id,
        principal,
        audit,
        coordinator,
        request,
    }: ProfileCommand<UpdateUserProfileRequest>,
) -> Response {
    let creating = request.expected_version.is_none();
    let result = coordinator
        .update_profile(UpdateProfile {
            user_id: principal.user_id,
            precondition: ProfilePrecondition::from(request.expected_version),
            display_name: request.display_name,
            lightning_address: request.lightning_address,
            tips_enabled: request.tips_enabled,
            occurred_at: OffsetDateTime::now_utc(),
            audit,
        })
        .await;
    match result {
        Ok(profile) => {
            let mut response = Json(profile_response(profile)).into_response();
            if creating {
                *response.status_mut() = StatusCode::CREATED;
            }
            response
        }
        Err(error) => transition_problem(error, request_id),
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/v1/lightning/tip-recipient",
    responses(
        (status = OK, body = ActiveTipRecipientResponse),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope)
    ),
    tag = "Lightning"
)]
async fn get_active_tip_recipient(
    request_id: RequestId,
    AvailableProfileStore(store): AvailableProfileStore,
) -> Response {
    match store.active_tip_recipient().await {
        Ok(setting) => Json(recipient_response(setting)).into_response(),
        Err(error) => load_problem(error, request_id),
    }
}

#[utoipa::path(
    put,
    path = "/api/admin/v1/lightning/tip-recipient",
    request_body = PutActiveTipRecipientRequest,
    responses(
        (status = OK, body = ActiveTipRecipientResponse,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = BAD_REQUEST, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = PAYLOAD_TOO_LARGE, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = FORBIDDEN, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = NOT_FOUND, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = CONFLICT, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = PRECONDITION_FAILED, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID"))),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope,
            headers(("x-request-id" = Uuid, description = "Request correlation ID")))
    ),
    params(
        ("Idempotency-Key" = Uuid, Header, description = "Canonical UUID identifying retries of this command")
    ),
    tag = "Lightning"
)]
async fn put_active_tip_recipient(
    ProfileCommand {
        request_id,
        audit,
        coordinator,
        request,
        ..
    }: ProfileCommand<PutActiveTipRecipientRequest>,
) -> Response {
    match coordinator
        .set_tip_recipient(SetTipRecipient {
            expected_version: request.expected_version,
            recipient_user_id: request.user_id,
            occurred_at: OffsetDateTime::now_utc(),
            audit,
        })
        .await
    {
        Ok(setting) => Json(recipient_response(setting)).into_response(),
        Err(error) => transition_problem(error, request_id),
    }
}

struct AvailableProfileStore(ProfileStore);

impl<S> FromRequestParts<S> for AvailableProfileStore
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = RequestId::from_request_parts(parts, state)
            .await
            .map_err(|error| error.into_response())?;
        parts
            .extensions
            .get::<ProfileStore>()
            .cloned()
            .map(Self)
            .ok_or_else(|| unavailable(request_id))
    }
}

struct ProfileCommand<T> {
    request_id: RequestId,
    principal: AdminPrincipal,
    audit: MutationAuditContext,
    coordinator: PublicationCoordinatorHandle,
    request: T,
}

impl<S, T> FromRequest<S> for ProfileCommand<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let (mut parts, body) = request.into_parts();
        let request_id = RequestId::from_request_parts(&mut parts, state)
            .await
            .map_err(|error| error.into_response())?;
        let principal = AdminPrincipal::from_request_parts(&mut parts, state)
            .await
            .map_err(|error| error.into_response())?;
        let headers = parts.headers.clone();
        let coordinator = parts
            .extensions
            .get::<PublicationCoordinatorHandle>()
            .cloned();
        let request = Request::from_parts(parts, body);
        let Json(request) = Json::<T>::from_request(request, state)
            .await
            .map_err(|rejection| json_rejection(rejection, request_id))?;
        let idempotency_key =
            profile_idempotency_key(&headers).map_err(|spec| problem(spec, request_id))?;
        let coordinator = coordinator.ok_or_else(|| unavailable(request_id))?;
        let audit = mutation_audit(&principal, request_id, idempotency_key);
        Ok(Self {
            request_id,
            principal,
            audit,
            coordinator,
            request,
        })
    }
}

fn profile_idempotency_key(headers: &HeaderMap) -> Result<AdminMutationKey, AdminProblem> {
    let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER).iter();
    let Some(value) = values.next() else {
        return Err(AdminProblem::bad_request(
            "missing_idempotency_key",
            "Idempotency-Key is required for profile mutations",
        ));
    };
    if values.next().is_some() {
        return Err(invalid_idempotency_key());
    }
    let encoded = value.to_str().map_err(|_| invalid_idempotency_key())?;
    let key = Uuid::parse_str(encoded).map_err(|_| invalid_idempotency_key())?;
    if key.hyphenated().to_string() != encoded {
        return Err(invalid_idempotency_key());
    }
    Ok(AdminMutationKey(key))
}

fn invalid_idempotency_key() -> AdminProblem {
    AdminProblem::bad_request(
        "invalid_idempotency_key",
        "Idempotency-Key must be one canonical lowercase hyphenated UUID",
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

fn profile_response(profile: StoredUserProfile) -> UserProfileResponse {
    UserProfileResponse {
        user_id: profile.user_id,
        display_name: profile.display_name,
        lightning_address: profile.lightning_address,
        tips_enabled: profile.tips_enabled,
        version: profile.version,
        updated_at: profile.updated_at.to_offset(UtcOffset::UTC),
    }
}

fn recipient_response(setting: StoredTipRecipientSetting) -> ActiveTipRecipientResponse {
    ActiveTipRecipientResponse {
        user_id: setting.recipient_user_id,
        version: setting.version,
        updated_at: setting.updated_at.to_offset(UtcOffset::UTC),
    }
}

fn invalid_body(request_id: RequestId) -> Response {
    problem(
        AdminProblem::bad_request(
            "invalid_profile_request",
            "the profile request body is invalid",
        ),
        request_id,
    )
}

fn json_rejection(rejection: JsonRejection, request_id: RequestId) -> Response {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return problem(
            AdminProblem::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "profile_request_too_large",
                "the profile request body exceeds 8192 bytes",
            ),
            request_id,
        );
    }
    invalid_body(request_id)
}

fn not_found(request_id: RequestId) -> Response {
    problem(
        AdminProblem::new(
            StatusCode::NOT_FOUND,
            "profile_not_found",
            "the requested profile resource does not exist",
        ),
        request_id,
    )
}

fn load_problem(error: ProfileLoadError, request_id: RequestId) -> Response {
    tracing::error!(%request_id, error = %error, "profile state lookup failed");
    unavailable(request_id)
}

fn transition_problem(error: ProfileTransitionError, request_id: RequestId) -> Response {
    let spec = match error {
        ProfileTransitionError::Mutation(ProfileMutationError::Command(
            ProfileCommandError::NotFound,
        )) => return not_found(request_id),
        ProfileTransitionError::Mutation(ProfileMutationError::Command(
            ProfileCommandError::StaleVersion,
        )) => AdminProblem::new(
            StatusCode::PRECONDITION_FAILED,
            "stale_profile_version",
            "the expected profile resource version is stale",
        ),
        ProfileTransitionError::Mutation(ProfileMutationError::Command(
            ProfileCommandError::Conflict,
        )) => AdminProblem::new(
            StatusCode::CONFLICT,
            "profile_already_exists",
            "the profile already exists; replace it with its expected version",
        ),
        ProfileTransitionError::Mutation(ProfileMutationError::Command(
            ProfileCommandError::Forbidden,
        )) => AdminProblem::forbidden(
            "profile_actor_forbidden",
            "the authenticated principal cannot change this profile resource",
        ),
        ProfileTransitionError::Mutation(ProfileMutationError::Command(
            ProfileCommandError::IdempotencyConflict,
        )) => AdminProblem::new(
            StatusCode::CONFLICT,
            "idempotency_key_conflict",
            "Idempotency-Key is already bound to a different profile command",
        ),
        ProfileTransitionError::Mutation(ProfileMutationError::Command(
            ProfileCommandError::InvalidValue,
        )) => return invalid_body(request_id),
        ProfileTransitionError::Coordinator(
            PublicationCoordinatorUnavailable::Closed
            | PublicationCoordinatorUnavailable::OutcomeUnknown,
        )
        | ProfileTransitionError::Mutation(ProfileMutationError::Admission(
            DatabaseAdmissionError::QueueFull | DatabaseAdmissionError::WriterClosed,
        ))
        | ProfileTransitionError::Mutation(ProfileMutationError::Command(
            ProfileCommandError::OutcomeUnknown,
        ))
        | ProfileTransitionError::Load(_)
        | ProfileTransitionError::Snapshot(_)
        | ProfileTransitionError::SnapshotActivationConflict => profile_unavailable(),
    };
    if spec.status.is_server_error() {
        tracing::error!(%request_id, error = %error, "profile presentation transition failed");
    }
    problem(spec, request_id)
}

fn unavailable(request_id: RequestId) -> Response {
    problem(profile_unavailable(), request_id)
}

const fn profile_unavailable() -> AdminProblem {
    AdminProblem::unavailable(
        "profile_unavailable",
        "profile management is temporarily unavailable",
    )
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

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use axum::{
        Extension, Router,
        body::{Body, Bytes, to_bytes},
        http::{Method, Request as HttpRequest, StatusCode, header::CONTENT_TYPE},
        routing::put,
    };
    use maincopy_shared::auth::{AdminScope, AdminSessionId, UserId};
    use serde_json::Value;
    use tower::ServiceExt as _;

    use super::*;

    const IDEMPOTENCY_KEY: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";
    const VALID_PROFILE_BODY: &[u8] =
        br#"{"display_name":null,"lightning_address":null,"tips_enabled":false}"#;

    async fn extract_profile_command(_: ProfileCommand<UpdateUserProfileRequest>) -> StatusCode {
        StatusCode::NO_CONTENT
    }

    fn extractor_router() -> Router {
        let principal = AdminPrincipal {
            user_id: UserId::from_uuid(Uuid::new_v4()),
            scopes: Arc::new(BTreeSet::<AdminScope>::new()),
            authentication: AdminAuthentication::BrowserSession {
                session_id: AdminSessionId::from_uuid(Uuid::new_v4()),
            },
        };
        Router::new()
            .route("/", put(extract_profile_command))
            .layer(DefaultBodyLimit::max(PROFILE_REQUEST_BODY_LIMIT))
            .layer(Extension(principal))
            .layer(Extension(RequestId(Uuid::new_v4())))
    }

    fn profile_request(body: Bytes, idempotency_key: Option<&str>) -> HttpRequest<Body> {
        let mut request = HttpRequest::builder()
            .method(Method::PUT)
            .uri("/")
            .header(CONTENT_TYPE, "application/json");
        if let Some(idempotency_key) = idempotency_key {
            request = request.header(IDEMPOTENCY_KEY_HEADER, idempotency_key);
        }
        request.body(Body::from(body)).unwrap()
    }

    async fn error_code(response: Response) -> Value {
        let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        serde_json::from_slice::<Value>(&body).unwrap()["error"]["code"].clone()
    }

    #[test]
    fn profile_mutations_require_one_canonical_idempotency_key() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            profile_idempotency_key(&headers).unwrap_err().code,
            "missing_idempotency_key"
        );

        headers.insert(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_static(IDEMPOTENCY_KEY),
        );
        assert!(matches!(
            profile_idempotency_key(&headers),
            Ok(key) if key == AdminMutationKey(Uuid::parse_str(IDEMPOTENCY_KEY).unwrap())
        ));

        headers.append(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_static(IDEMPOTENCY_KEY),
        );
        assert_eq!(
            profile_idempotency_key(&headers).unwrap_err().code,
            "invalid_idempotency_key"
        );
    }

    #[tokio::test]
    async fn malformed_profile_body_precedes_missing_idempotency_key_and_runtime_state() {
        let response = extractor_router()
            .oneshot(profile_request(Bytes::from_static(b"{"), None))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error_code(response).await, "invalid_profile_request");
    }

    #[tokio::test]
    async fn valid_profile_body_reports_missing_idempotency_key_before_runtime_state() {
        let response = extractor_router()
            .oneshot(profile_request(
                Bytes::from_static(VALID_PROFILE_BODY),
                None,
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error_code(response).await, "missing_idempotency_key");
    }

    #[tokio::test]
    async fn valid_profile_command_reports_missing_runtime_state() {
        let response = extractor_router()
            .oneshot(profile_request(
                Bytes::from_static(VALID_PROFILE_BODY),
                Some(IDEMPOTENCY_KEY),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error_code(response).await, "profile_unavailable");
    }

    #[tokio::test]
    async fn oversized_profile_body_preserves_payload_too_large() {
        let response = extractor_router()
            .oneshot(profile_request(
                Bytes::from(vec![b' '; PROFILE_REQUEST_BODY_LIMIT + 1]),
                None,
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error_code(response).await, "profile_request_too_large");
    }
}
