use std::collections::BTreeSet;

use axum::{
    Extension, Json,
    extract::{
        DefaultBodyLimit, Path, Query,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use maincopy_shared::{
    auth::{
        AdminAuditEventId, AdminScope, AgentCredentialId, HumanLoginProvider, UserId, UserRole,
    },
    auth_api::{
        AdminAuditEventResponse, AgentCredentialMutationResponse, AgentCredentialResponse,
        AuditOutcome, AuditPrincipalResponse, CreateUserRequest, DEFAULT_IDENTITY_PAGE_LIMIT,
        ExpectedVersionRequest, HumanCredentialInput, HumanCredentialResponse,
        ListAdminAuditEventsResponse, ListAgentCredentialsResponse, ListUsersResponse,
        MAX_IDENTITY_PAGE_LIMIT, PutHumanCredentialRequest, RegisterAgentCredentialRequest,
        ReplaceAgentScopesRequest, ReplaceUserRolesRequest, SetUserStatusRequest,
        UserMutationResponse, UserResponse, UserSummaryResponse,
    },
    publication::IDEMPOTENCY_KEY_HEADER,
};
use serde::{Deserialize, de::DeserializeOwned};
use time::{OffsetDateTime, UtcOffset};
use utoipa_axum::{
    router::{UtoipaMethodRouter, UtoipaMethodRouterExt as _},
    routes,
};
use uuid::Uuid;

use super::{
    AdminSecurityState, BrowserSessionContext,
    principal::{AdminAuthentication, AdminPrincipal},
    problem::{AdminProblem, AdminProblemEnvelope, problem_response},
    request_id::RequestId,
};
use crate::{
    database::store::DatabaseAdmissionError,
    domain::auth::{
        CanonicalUsername, NostrPublicKey, PasswordHashingError,
        store::{
            AdminAuditOutcome, AdminMutationKey, AuditPrincipalReference, AuthCommandError,
            AuthLoadError, AuthMutationError, CreateUser, HumanCredentialKind,
            MutationAuditContext, NewHumanCredential, PutHumanCredential, RegisterAgentCredential,
            RemoveHumanCredential, ReplaceAgentScopes, ReplaceUserRoles, RevokeAgentCredential,
            SetUserStatus, StoredAdminAuditEvent, StoredAgentCredential, StoredHumanCredential,
            StoredUser,
        },
    },
    domain::publication::activation::{PublicationCoordinatorHandle, UserStatusTransitionError},
    password_executor::PasswordExecutorError,
};

const IDENTITY_REQUEST_BODY_LIMIT: usize = 32 * 1024;
const NO_STORE: HeaderValue = HeaderValue::from_static("no-store");
const NOSNIFF: HeaderValue = HeaderValue::from_static("nosniff");
const RETRY_AFTER_ONE_SECOND: HeaderValue = HeaderValue::from_static("1");

pub(super) fn user_read_routes() -> UtoipaMethodRouter {
    routes!(list_users)
}

pub(super) fn user_item_read_routes() -> UtoipaMethodRouter {
    routes!(get_user)
}

pub(super) fn user_create_routes() -> UtoipaMethodRouter {
    routes!(create_user).layer(DefaultBodyLimit::max(IDENTITY_REQUEST_BODY_LIMIT))
}

pub(super) fn user_status_routes() -> UtoipaMethodRouter {
    routes!(set_user_status).layer(DefaultBodyLimit::max(IDENTITY_REQUEST_BODY_LIMIT))
}

pub(super) fn user_role_routes() -> UtoipaMethodRouter {
    routes!(replace_user_roles).layer(DefaultBodyLimit::max(IDENTITY_REQUEST_BODY_LIMIT))
}

pub(super) fn human_credential_routes() -> UtoipaMethodRouter {
    routes!(put_human_credential, remove_human_credential)
        .layer(DefaultBodyLimit::max(IDENTITY_REQUEST_BODY_LIMIT))
}

pub(super) fn agent_read_routes() -> UtoipaMethodRouter {
    routes!(list_agent_credentials)
}

pub(super) fn agent_item_read_routes() -> UtoipaMethodRouter {
    routes!(get_agent_credential)
}

pub(super) fn agent_registration_routes() -> UtoipaMethodRouter {
    routes!(register_agent_credential).layer(DefaultBodyLimit::max(IDENTITY_REQUEST_BODY_LIMIT))
}

pub(super) fn agent_scope_routes() -> UtoipaMethodRouter {
    routes!(replace_agent_scopes).layer(DefaultBodyLimit::max(IDENTITY_REQUEST_BODY_LIMIT))
}

pub(super) fn agent_revocation_routes() -> UtoipaMethodRouter {
    routes!(revoke_agent_credential).layer(DefaultBodyLimit::max(IDENTITY_REQUEST_BODY_LIMIT))
}

pub(super) fn audit_routes() -> UtoipaMethodRouter {
    routes!(list_audit_events)
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PageQuery {
    cursor: Option<Box<str>>,
    limit: Option<u16>,
}

#[expect(
    clippy::result_large_err,
    reason = "the Axum adapter returns a ready response to preserve the stable error envelope"
)]
fn identity_page(
    query: Result<Query<PageQuery>, QueryRejection>,
    request_id: RequestId,
) -> Result<(Option<Uuid>, u16), Response> {
    let Query(query) = query.map_err(|_| invalid_query(request_id))?;
    let limit = query.limit.unwrap_or(DEFAULT_IDENTITY_PAGE_LIMIT);
    if !(1..=MAX_IDENTITY_PAGE_LIMIT).contains(&limit) {
        return Err(invalid_query(request_id));
    }
    let cursor = query
        .cursor
        .map(|cursor| parse_canonical_uuid(&cursor))
        .transpose()
        .map_err(|()| invalid_query(request_id))?;
    Ok((cursor, limit))
}

#[expect(
    clippy::result_large_err,
    reason = "the Axum adapter returns a ready response to preserve the stable error envelope"
)]
fn user_path(
    path: Result<Path<String>, PathRejection>,
    request_id: RequestId,
) -> Result<UserId, Response> {
    let Path(encoded) = path.map_err(|_| invalid_path(request_id))?;
    parse_canonical_uuid(&encoded)
        .map(UserId::from_uuid)
        .map_err(|()| invalid_path(request_id))
}

#[expect(
    clippy::result_large_err,
    reason = "the Axum adapter returns a ready response to preserve the stable error envelope"
)]
fn agent_credential_path(
    path: Result<Path<String>, PathRejection>,
    request_id: RequestId,
) -> Result<AgentCredentialId, Response> {
    let Path(encoded) = path.map_err(|_| invalid_path(request_id))?;
    parse_canonical_uuid(&encoded)
        .map(AgentCredentialId::from_uuid)
        .map_err(|()| invalid_path(request_id))
}

#[expect(
    clippy::result_large_err,
    reason = "the Axum adapter returns a ready response to preserve the stable error envelope"
)]
fn human_credential_path(
    path: Result<Path<(String, String)>, PathRejection>,
    request_id: RequestId,
) -> Result<(UserId, HumanLoginProvider), Response> {
    let Path((encoded_user_id, encoded_provider)) = path.map_err(|_| invalid_path(request_id))?;
    let user_id = parse_canonical_uuid(&encoded_user_id)
        .map(UserId::from_uuid)
        .map_err(|()| invalid_path(request_id))?;
    let provider =
        HumanLoginProvider::parse(&encoded_provider).ok_or_else(|| invalid_path(request_id))?;
    Ok((user_id, provider))
}

#[expect(
    clippy::result_large_err,
    reason = "the Axum adapter returns a ready response to preserve the stable error envelope"
)]
fn identity_body<T: DeserializeOwned>(
    request: Result<Json<T>, JsonRejection>,
    request_id: RequestId,
) -> Result<T, Response> {
    request
        .map(|Json(request)| request)
        .map_err(|_| invalid_body(request_id))
}

#[expect(
    clippy::result_large_err,
    reason = "the Axum adapter returns a ready response to preserve the stable error envelope"
)]
fn require_fresh_authentication(
    principal: &AdminPrincipal,
    browser: Option<&BrowserSessionContext>,
    request_id: RequestId,
) -> Result<(), Response> {
    let fresh = match principal.authentication {
        AdminAuthentication::AgentCredential { .. } => true,
        AdminAuthentication::BrowserSession { session_id } => browser.is_some_and(|context| {
            context.session.session_id == session_id
                && context.is_fresh_at(OffsetDateTime::now_utc())
        }),
    };
    fresh
        .then_some(())
        .ok_or_else(|| fresh_authentication_required(request_id))
}

#[expect(
    clippy::result_large_err,
    reason = "the Axum adapter returns a ready response to preserve the stable error envelope"
)]
fn require_human(principal: &AdminPrincipal, request_id: RequestId) -> Result<(), Response> {
    match principal.authentication {
        AdminAuthentication::BrowserSession { .. } => Ok(()),
        AdminAuthentication::AgentCredential { .. } => {
            Err(fresh_authentication_required(request_id))
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/v1/identity/users",
    params(
        ("cursor" = Option<Uuid>, Query, description = "Last user UUID returned by the previous page"),
        ("limit" = Option<u16>, Query, description = "Page size from 1 through 100")
    ),
    responses(
        (status = OK, body = ListUsersResponse),
        (status = BAD_REQUEST, body = AdminProblemEnvelope),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope)
    ),
    tag = "Identity"
)]
async fn list_users(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let (cursor, limit) = match identity_page(query, request_id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    let cursor = cursor.map(UserId::from_uuid);
    match security.store.users_page(cursor, limit).await {
        Ok(page) => private_json(Json(ListUsersResponse {
            users: page.items.iter().map(user_summary).collect(),
            next_cursor: page.next_cursor,
        }))
        .into_response(),
        Err(error) => load_problem(error, request_id),
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/v1/identity/users/{user_id}",
    params(("user_id" = Uuid, Path)),
    responses(
        (status = OK, body = UserResponse),
        (status = BAD_REQUEST, body = AdminProblemEnvelope),
        (status = NOT_FOUND, body = AdminProblemEnvelope),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope)
    ),
    tag = "Identity"
)]
async fn get_user(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let user_id = match user_path(path, request_id) {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    load_user_response(&security, user_id, request_id).await
}

#[utoipa::path(
    post,
    path = "/api/admin/v1/identity/users",
    params(("Idempotency-Key" = Uuid, Header, description = "Canonical UUID identifying retries of this command")),
    request_body = CreateUserRequest,
    responses(
        (status = CREATED, body = UserMutationResponse),
        (status = BAD_REQUEST, body = AdminProblemEnvelope),
        (status = FORBIDDEN, body = AdminProblemEnvelope),
        (status = CONFLICT, body = AdminProblemEnvelope),
        (status = TOO_MANY_REQUESTS, body = AdminProblemEnvelope),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope)
    ),
    tag = "Identity"
)]
async fn create_user(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    principal: AdminPrincipal,
    browser: Option<Extension<BrowserSessionContext>>,
    headers: HeaderMap,
    request: Result<Json<CreateUserRequest>, JsonRejection>,
) -> Response {
    if let Err(response) = require_fresh_authentication(&principal, browser.as_deref(), request_id)
    {
        return response;
    }
    let request = match identity_body(request, request_id) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if request
        .credentials
        .iter()
        .any(|credential| credential.provider() == HumanLoginProvider::Password)
        && let Err(response) = require_human(&principal, request_id)
    {
        return response;
    }
    let audit = match mutation_audit(&principal, request_id, &headers) {
        Ok(audit) => audit,
        Err(response) => return response,
    };
    let roles = match unique_nonempty(request.roles, "roles", request_id) {
        Ok(roles) => roles,
        Err(response) => return response,
    };
    if roles != BTreeSet::from([UserRole::Publisher])
        && !principal.scopes.contains(&AdminScope::RoleAssign)
    {
        return problem(
            AdminProblem::forbidden(
                "role_assignment_required",
                "creating a non-publisher account requires role-assignment authority",
            ),
            request_id,
        );
    }
    let credentials = match prepare_credentials(request.credentials, &security, request_id).await {
        Ok(credentials) => credentials,
        Err(response) => return response,
    };
    let user_id = UserId::from_uuid(Uuid::new_v4());
    let actor_user_id = principal.user_id;
    let result = security
        .store
        .create_user(CreateUser {
            user_id,
            created_by_user_id: actor_user_id,
            status: request.status,
            roles,
            credentials,
            configured_providers: security.providers,
            occurred_at: OffsetDateTime::now_utc(),
            audit,
        })
        .await;
    match result {
        Ok(result) => private_json((
            StatusCode::CREATED,
            Json(UserMutationResponse {
                user_id: result.user_id,
                version: result.version,
            }),
        )),
        Err(error) => mutation_problem(error, request_id),
    }
}

#[utoipa::path(
    put,
    path = "/api/admin/v1/identity/users/{user_id}/status",
    params(
        ("user_id" = Uuid, Path),
        ("Idempotency-Key" = Uuid, Header, description = "Canonical UUID identifying retries of this command")
    ),
    request_body = SetUserStatusRequest,
    responses(
        (status = OK, body = UserMutationResponse),
        (status = BAD_REQUEST, body = AdminProblemEnvelope),
        (status = FORBIDDEN, body = AdminProblemEnvelope),
        (status = NOT_FOUND, body = AdminProblemEnvelope),
        (status = PRECONDITION_FAILED, body = AdminProblemEnvelope),
        (status = CONFLICT, body = AdminProblemEnvelope),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope)
    ),
    tag = "Identity"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is an independent Axum extractor required by this endpoint"
)]
async fn set_user_status(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    principal: AdminPrincipal,
    browser: Option<Extension<BrowserSessionContext>>,
    coordinator: Option<Extension<PublicationCoordinatorHandle>>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    request: Result<Json<SetUserStatusRequest>, JsonRejection>,
) -> Response {
    if let Err(response) = require_fresh_authentication(&principal, browser.as_deref(), request_id)
    {
        return response;
    }
    let user_id = match user_path(path, request_id) {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let request = match identity_body(request, request_id) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if request.expected_version == 0 {
        return invalid_body(request_id);
    }
    let audit = match mutation_audit(&principal, request_id, &headers) {
        Ok(audit) => audit,
        Err(response) => return response,
    };
    if let Err(response) = require_target_authority(
        &security,
        &principal,
        user_id,
        AdminScope::RoleAssign,
        request_id,
    )
    .await
    {
        return response;
    }
    let Some(Extension(coordinator)) = coordinator else {
        tracing::error!(
            %request_id,
            "user status mutation rejected without an available tip presentation coordinator"
        );
        return problem(identity_unavailable(), request_id);
    };
    match coordinator
        .set_user_status(
            security.store.clone(),
            SetUserStatus {
                user_id,
                changed_by_user_id: principal.user_id,
                expected_version: request.expected_version,
                status: request.status,
                configured_providers: security.providers,
                occurred_at: OffsetDateTime::now_utc(),
                audit,
            },
        )
        .await
    {
        Ok(result) => private_json(Json(UserMutationResponse {
            user_id: result.user_id,
            version: result.version,
        })),
        Err(UserStatusTransitionError::Mutation(error)) => mutation_problem(error, request_id),
        Err(error) => {
            tracing::error!(
                %request_id,
                error = %error,
                "user status transition failed internally"
            );
            problem(identity_unavailable(), request_id)
        }
    }
}

#[utoipa::path(
    put,
    path = "/api/admin/v1/identity/users/{user_id}/roles",
    params(
        ("user_id" = Uuid, Path),
        ("Idempotency-Key" = Uuid, Header, description = "Canonical UUID identifying retries of this command")
    ),
    request_body = ReplaceUserRolesRequest,
    responses(
        (status = OK, body = UserMutationResponse),
        (status = BAD_REQUEST, body = AdminProblemEnvelope),
        (status = NOT_FOUND, body = AdminProblemEnvelope),
        (status = PRECONDITION_FAILED, body = AdminProblemEnvelope),
        (status = CONFLICT, body = AdminProblemEnvelope)
    ),
    tag = "Identity"
)]
async fn replace_user_roles(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    principal: AdminPrincipal,
    browser: Option<Extension<BrowserSessionContext>>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    request: Result<Json<ReplaceUserRolesRequest>, JsonRejection>,
) -> Response {
    if let Err(response) = require_fresh_authentication(&principal, browser.as_deref(), request_id)
    {
        return response;
    }
    let user_id = match user_path(path, request_id) {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let request = match identity_body(request, request_id) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if request.expected_version == 0 {
        return invalid_body(request_id);
    }
    let audit = match mutation_audit(&principal, request_id, &headers) {
        Ok(audit) => audit,
        Err(response) => return response,
    };
    let roles = match unique_nonempty(request.roles, "roles", request_id) {
        Ok(roles) => roles,
        Err(response) => return response,
    };
    match security
        .store
        .replace_user_roles(ReplaceUserRoles {
            user_id,
            expected_version: request.expected_version,
            roles,
            assigned_by_user_id: principal.user_id,
            occurred_at: OffsetDateTime::now_utc(),
            audit,
        })
        .await
    {
        Ok(result) => private_json(Json(UserMutationResponse {
            user_id: result.user_id,
            version: result.version,
        })),
        Err(error) => mutation_problem(error, request_id),
    }
}

#[utoipa::path(
    put,
    path = "/api/admin/v1/identity/users/{user_id}/credentials/{provider}",
    params(
        ("user_id" = Uuid, Path),
        ("provider" = String, Path),
        ("Idempotency-Key" = Uuid, Header, description = "Canonical UUID identifying retries of this command")
    ),
    request_body = PutHumanCredentialRequest,
    responses(
        (status = OK, body = UserMutationResponse),
        (status = BAD_REQUEST, body = AdminProblemEnvelope),
        (status = FORBIDDEN, body = AdminProblemEnvelope),
        (status = NOT_FOUND, body = AdminProblemEnvelope),
        (status = PRECONDITION_FAILED, body = AdminProblemEnvelope),
        (status = CONFLICT, body = AdminProblemEnvelope),
        (status = TOO_MANY_REQUESTS, body = AdminProblemEnvelope)
    ),
    tag = "Identity"
)]
async fn put_human_credential(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    principal: AdminPrincipal,
    browser: Option<Extension<BrowserSessionContext>>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Result<Json<PutHumanCredentialRequest>, JsonRejection>,
) -> Response {
    if let Err(response) = require_fresh_authentication(&principal, browser.as_deref(), request_id)
    {
        return response;
    }
    let (user_id, provider) = match human_credential_path(path, request_id) {
        Ok(path) => path,
        Err(response) => return response,
    };
    let request = match identity_body(request, request_id) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let (expected_version, credential_input) = match request {
        PutHumanCredentialRequest::Create { credential } => (None, credential),
        PutHumanCredentialRequest::Replace {
            expected_version,
            credential,
        } if expected_version > 0 => (Some(expected_version), credential),
        _ => return invalid_body(request_id),
    };
    let audit = match mutation_audit(&principal, request_id, &headers) {
        Ok(audit) => audit,
        Err(response) => return response,
    };
    if credential_input.provider() != provider {
        return invalid_body(request_id);
    }
    if provider == HumanLoginProvider::Password
        && let Err(response) = require_human(&principal, request_id)
    {
        return response;
    }
    if let Err(response) = require_target_authority(
        &security,
        &principal,
        user_id,
        AdminScope::RoleAssign,
        request_id,
    )
    .await
    {
        return response;
    }
    let credential = match prepare_credential(credential_input, &security, request_id).await {
        Ok(credential) => credential,
        Err(response) => return response,
    };
    match security
        .store
        .put_human_credential(PutHumanCredential {
            user_id,
            managed_by_user_id: principal.user_id,
            credential,
            expected_version,
            occurred_at: OffsetDateTime::now_utc(),
            audit,
        })
        .await
    {
        Ok(result) => private_json(Json(UserMutationResponse {
            user_id: result.user_id,
            version: result.version,
        })),
        Err(error) => mutation_problem(error, request_id),
    }
}

#[utoipa::path(
    delete,
    path = "/api/admin/v1/identity/users/{user_id}/credentials/{provider}",
    params(
        ("user_id" = Uuid, Path),
        ("provider" = String, Path),
        ("Idempotency-Key" = Uuid, Header, description = "Canonical UUID identifying retries of this command")
    ),
    request_body = ExpectedVersionRequest,
    responses(
        (status = OK, body = UserMutationResponse),
        (status = BAD_REQUEST, body = AdminProblemEnvelope),
        (status = FORBIDDEN, body = AdminProblemEnvelope),
        (status = NOT_FOUND, body = AdminProblemEnvelope),
        (status = PRECONDITION_FAILED, body = AdminProblemEnvelope),
        (status = CONFLICT, body = AdminProblemEnvelope)
    ),
    tag = "Identity"
)]
async fn remove_human_credential(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    principal: AdminPrincipal,
    browser: Option<Extension<BrowserSessionContext>>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Result<Json<ExpectedVersionRequest>, JsonRejection>,
) -> Response {
    if let Err(response) = require_fresh_authentication(&principal, browser.as_deref(), request_id)
    {
        return response;
    }
    let (user_id, provider) = match human_credential_path(path, request_id) {
        Ok(path) => path,
        Err(response) => return response,
    };
    let request = match identity_body(request, request_id) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if request.expected_version == 0 {
        return invalid_body(request_id);
    }
    let audit = match mutation_audit(&principal, request_id, &headers) {
        Ok(audit) => audit,
        Err(response) => return response,
    };
    if provider == HumanLoginProvider::Password
        && let Err(response) = require_human(&principal, request_id)
    {
        return response;
    }
    if let Err(response) = require_target_authority(
        &security,
        &principal,
        user_id,
        AdminScope::RoleAssign,
        request_id,
    )
    .await
    {
        return response;
    }
    let kind = match provider {
        HumanLoginProvider::Password => HumanCredentialKind::Password,
        HumanLoginProvider::Nostr => HumanCredentialKind::Nostr,
    };
    match security
        .store
        .remove_human_credential(RemoveHumanCredential {
            user_id,
            managed_by_user_id: principal.user_id,
            kind,
            expected_version: request.expected_version,
            configured_providers: security.providers,
            occurred_at: OffsetDateTime::now_utc(),
            audit,
        })
        .await
    {
        Ok(result) => private_json(Json(UserMutationResponse {
            user_id: result.user_id,
            version: result.version,
        })),
        Err(error) => mutation_problem(error, request_id),
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/v1/identity/agents",
    params(
        ("cursor" = Option<Uuid>, Query),
        ("limit" = Option<u16>, Query)
    ),
    responses(
        (status = OK, body = ListAgentCredentialsResponse),
        (status = BAD_REQUEST, body = AdminProblemEnvelope),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope)
    ),
    tag = "Identity"
)]
async fn list_agent_credentials(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let (cursor, limit) = match identity_page(query, request_id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    let cursor = cursor.map(AgentCredentialId::from_uuid);
    match security.store.agent_credentials_page(cursor, limit).await {
        Ok(page) => private_json(Json(ListAgentCredentialsResponse {
            agent_credentials: page.items.iter().map(agent_response).collect(),
            next_cursor: page.next_cursor,
        }))
        .into_response(),
        Err(error) => load_problem(error, request_id),
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/v1/identity/agents/{agent_credential_id}",
    params(("agent_credential_id" = Uuid, Path)),
    responses(
        (status = OK, body = AgentCredentialResponse),
        (status = BAD_REQUEST, body = AdminProblemEnvelope),
        (status = NOT_FOUND, body = AdminProblemEnvelope),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope)
    ),
    tag = "Identity"
)]
async fn get_agent_credential(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let credential_id = match agent_credential_path(path, request_id) {
        Ok(credential_id) => credential_id,
        Err(response) => return response,
    };
    load_agent_response(&security, credential_id, request_id).await
}

#[utoipa::path(
    post,
    path = "/api/admin/v1/identity/agents",
    params(("Idempotency-Key" = Uuid, Header, description = "Canonical UUID identifying retries of this command")),
    request_body = RegisterAgentCredentialRequest,
    responses(
        (status = CREATED, body = AgentCredentialMutationResponse),
        (status = BAD_REQUEST, body = AdminProblemEnvelope),
        (status = FORBIDDEN, body = AdminProblemEnvelope),
        (status = CONFLICT, body = AdminProblemEnvelope),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope)
    ),
    tag = "Identity"
)]
async fn register_agent_credential(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    principal: AdminPrincipal,
    browser: Option<Extension<BrowserSessionContext>>,
    headers: HeaderMap,
    request: Result<Json<RegisterAgentCredentialRequest>, JsonRejection>,
) -> Response {
    if let Err(response) = require_fresh_authentication(&principal, browser.as_deref(), request_id)
    {
        return response;
    }
    let request = match identity_body(request, request_id) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let audit = match mutation_audit(&principal, request_id, &headers) {
        Ok(audit) => audit,
        Err(response) => return response,
    };
    if let Err(response) = require_target_authority(
        &security,
        &principal,
        request.owner_user_id,
        AdminScope::RoleAssign,
        request_id,
    )
    .await
    {
        return response;
    }
    let public_key = match NostrPublicKey::parse(&request.public_key) {
        Ok(public_key) => public_key,
        Err(_) => return invalid_body(request_id),
    };
    let scopes = match unique_nonempty(request.scopes, "scopes", request_id) {
        Ok(scopes) if scopes.is_subset(&principal.scopes) => scopes,
        Ok(_) => {
            return problem(
                AdminProblem::forbidden(
                    "scope_escalation",
                    "agent scopes must be a subset of the authenticated authority",
                ),
                request_id,
            );
        }
        Err(response) => return response,
    };
    let credential_id = AgentCredentialId::from_uuid(Uuid::new_v4());
    match security
        .store
        .register_agent_credential(RegisterAgentCredential {
            credential_id,
            owner_user_id: request.owner_user_id,
            issuer_user_id: principal.user_id,
            public_key,
            label: request.label,
            scopes,
            created_at: OffsetDateTime::now_utc(),
            expires_at: request.expires_at,
            audit,
        })
        .await
    {
        Ok(result) => private_json((
            StatusCode::CREATED,
            Json(AgentCredentialMutationResponse {
                agent_credential_id: result.credential_id,
                version: result.version,
            }),
        )),
        Err(error) => mutation_problem(error, request_id),
    }
}

#[utoipa::path(
    put,
    path = "/api/admin/v1/identity/agents/{agent_credential_id}/scopes",
    params(
        ("agent_credential_id" = Uuid, Path),
        ("Idempotency-Key" = Uuid, Header, description = "Canonical UUID identifying retries of this command")
    ),
    request_body = ReplaceAgentScopesRequest,
    responses(
        (status = OK, body = AgentCredentialMutationResponse),
        (status = BAD_REQUEST, body = AdminProblemEnvelope),
        (status = FORBIDDEN, body = AdminProblemEnvelope),
        (status = NOT_FOUND, body = AdminProblemEnvelope),
        (status = PRECONDITION_FAILED, body = AdminProblemEnvelope),
        (status = CONFLICT, body = AdminProblemEnvelope)
    ),
    tag = "Identity"
)]
async fn replace_agent_scopes(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    principal: AdminPrincipal,
    browser: Option<Extension<BrowserSessionContext>>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    request: Result<Json<ReplaceAgentScopesRequest>, JsonRejection>,
) -> Response {
    if let Err(response) = require_fresh_authentication(&principal, browser.as_deref(), request_id)
    {
        return response;
    }
    let credential_id = match agent_credential_path(path, request_id) {
        Ok(credential_id) => credential_id,
        Err(response) => return response,
    };
    let request = match identity_body(request, request_id) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if request.expected_version == 0 {
        return invalid_body(request_id);
    }
    let audit = match mutation_audit(&principal, request_id, &headers) {
        Ok(audit) => audit,
        Err(response) => return response,
    };
    if let Err(response) =
        require_agent_owner_authority(&security, &principal, credential_id, request_id).await
    {
        return response;
    }
    let scopes = match unique_nonempty(request.scopes, "scopes", request_id) {
        Ok(scopes) if scopes.is_subset(&principal.scopes) => scopes,
        Ok(_) => {
            return problem(
                AdminProblem::forbidden(
                    "scope_escalation",
                    "agent scopes must be a subset of the authenticated authority",
                ),
                request_id,
            );
        }
        Err(response) => return response,
    };
    match security
        .store
        .replace_agent_scopes(ReplaceAgentScopes {
            credential_id,
            expected_version: request.expected_version,
            issuer_user_id: principal.user_id,
            scopes,
            occurred_at: OffsetDateTime::now_utc(),
            audit,
        })
        .await
    {
        Ok(result) => private_json(Json(AgentCredentialMutationResponse {
            agent_credential_id: result.credential_id,
            version: result.version,
        })),
        Err(error) => mutation_problem(error, request_id),
    }
}

#[utoipa::path(
    delete,
    path = "/api/admin/v1/identity/agents/{agent_credential_id}",
    params(
        ("agent_credential_id" = Uuid, Path),
        ("Idempotency-Key" = Uuid, Header, description = "Canonical UUID identifying retries of this command")
    ),
    request_body = ExpectedVersionRequest,
    responses(
        (status = OK, body = AgentCredentialMutationResponse),
        (status = BAD_REQUEST, body = AdminProblemEnvelope),
        (status = FORBIDDEN, body = AdminProblemEnvelope),
        (status = NOT_FOUND, body = AdminProblemEnvelope),
        (status = PRECONDITION_FAILED, body = AdminProblemEnvelope)
    ),
    tag = "Identity"
)]
async fn revoke_agent_credential(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    principal: AdminPrincipal,
    browser: Option<Extension<BrowserSessionContext>>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    request: Result<Json<ExpectedVersionRequest>, JsonRejection>,
) -> Response {
    if let Err(response) = require_fresh_authentication(&principal, browser.as_deref(), request_id)
    {
        return response;
    }
    let credential_id = match agent_credential_path(path, request_id) {
        Ok(credential_id) => credential_id,
        Err(response) => return response,
    };
    let request = match identity_body(request, request_id) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if request.expected_version == 0 {
        return invalid_body(request_id);
    }
    let audit = match mutation_audit(&principal, request_id, &headers) {
        Ok(audit) => audit,
        Err(response) => return response,
    };
    if let Err(response) =
        require_agent_owner_authority(&security, &principal, credential_id, request_id).await
    {
        return response;
    }
    match security
        .store
        .revoke_agent_credential(RevokeAgentCredential {
            credential_id,
            revoked_by_user_id: principal.user_id,
            expected_version: request.expected_version,
            revoked_at: OffsetDateTime::now_utc(),
            audit,
        })
        .await
    {
        Ok(result) => private_json(Json(AgentCredentialMutationResponse {
            agent_credential_id: result.credential_id,
            version: result.version,
        })),
        Err(error) => mutation_problem(error, request_id),
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/v1/audit/events",
    params(
        ("cursor" = Option<Uuid>, Query),
        ("limit" = Option<u16>, Query)
    ),
    responses(
        (status = OK, body = ListAdminAuditEventsResponse),
        (status = BAD_REQUEST, body = AdminProblemEnvelope),
        (status = SERVICE_UNAVAILABLE, body = AdminProblemEnvelope)
    ),
    tag = "Audit"
)]
async fn list_audit_events(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let (cursor, limit) = match identity_page(query, request_id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    let cursor = cursor.map(AdminAuditEventId::from_uuid);
    match security.store.audit_events_page(cursor, limit).await {
        Ok(page) => private_json(Json(ListAdminAuditEventsResponse {
            audit_events: page.items.into_iter().map(audit_response).collect(),
            next_cursor: page.next_cursor,
        }))
        .into_response(),
        Err(AuthLoadError::CursorNotFound) => problem(
            AdminProblem::bad_request(
                "invalid_audit_cursor",
                "the audit cursor does not identify a retained event",
            ),
            request_id,
        ),
        Err(error) => load_problem(error, request_id),
    }
}

#[allow(
    clippy::result_large_err,
    reason = "the Axum adapter returns a ready response to preserve the stable error envelope"
)]
async fn prepare_credentials(
    credentials: Vec<HumanCredentialInput>,
    security: &AdminSecurityState,
    request_id: RequestId,
) -> Result<Vec<NewHumanCredential>, Response> {
    if credentials.len() > 2 {
        return Err(invalid_body(request_id));
    }
    let mut providers = BTreeSet::new();
    for credential in &credentials {
        if !providers.insert(credential.provider()) {
            return Err(invalid_body(request_id));
        }
    }
    let mut prepared = Vec::with_capacity(credentials.len());
    for credential in credentials {
        prepared.push(prepare_credential(credential, security, request_id).await?);
    }
    Ok(prepared)
}

#[allow(
    clippy::result_large_err,
    reason = "the Axum adapter returns a ready response to preserve the stable error envelope"
)]
async fn prepare_credential(
    credential: HumanCredentialInput,
    security: &AdminSecurityState,
    request_id: RequestId,
) -> Result<NewHumanCredential, Response> {
    if !security.providers.accepts(credential.provider()) {
        return Err(problem(
            AdminProblem::bad_request(
                "login_provider_unavailable",
                "the requested human login provider is not enabled",
            ),
            request_id,
        ));
    }
    match credential {
        HumanCredentialInput::Password { username, password } => {
            let username =
                CanonicalUsername::parse(&username).map_err(|_| invalid_body(request_id))?;
            let password_hash = security
                .passwords
                .hash_password(password)
                .await
                .map_err(|error| password_problem(error, request_id))?;
            Ok(NewHumanCredential::Password {
                username,
                password_hash,
                policy_version: 1,
            })
        }
        HumanCredentialInput::Nostr { public_key } => {
            let public_key =
                NostrPublicKey::parse(&public_key).map_err(|_| invalid_body(request_id))?;
            Ok(NewHumanCredential::Nostr { public_key })
        }
    }
}

async fn load_user_response(
    security: &AdminSecurityState,
    user_id: UserId,
    request_id: RequestId,
) -> Response {
    let user = match security.store.user(user_id).await {
        Ok(Some(user)) => user,
        Ok(None) => return not_found(request_id),
        Err(error) => return load_problem(error, request_id),
    };
    let credentials = match security.store.user_credentials(user_id).await {
        Ok(Some(credentials)) => credentials,
        Ok(None) => return not_found(request_id),
        Err(error) => return load_problem(error, request_id),
    };
    private_json(Json(user_response(&user, credentials))).into_response()
}

async fn load_agent_response(
    security: &AdminSecurityState,
    credential_id: AgentCredentialId,
    request_id: RequestId,
) -> Response {
    match security.store.agent_credential_by_id(credential_id).await {
        Ok(Some(credential)) => private_json(Json(agent_response(&credential))).into_response(),
        Ok(None) => not_found(request_id),
        Err(error) => load_problem(error, request_id),
    }
}

#[allow(
    clippy::result_large_err,
    reason = "the Axum adapter returns a ready response to preserve the stable error envelope"
)]
async fn require_target_authority(
    security: &AdminSecurityState,
    principal: &AdminPrincipal,
    target_user_id: UserId,
    elevated_scope: AdminScope,
    request_id: RequestId,
) -> Result<(), Response> {
    let target = match security.store.user(target_user_id).await {
        Ok(Some(target)) => target,
        Ok(None) => return Err(not_found(request_id)),
        Err(error) => return Err(load_problem(error, request_id)),
    };
    if target.scopes().is_subset(&principal.scopes) {
        return Ok(());
    }
    if principal.scopes.contains(&elevated_scope) {
        return Ok(());
    }
    Err(problem(
        AdminProblem::forbidden(
            "target_authority_exceeded",
            "the target account has authority beyond the authenticated principal",
        ),
        request_id,
    ))
}

#[allow(
    clippy::result_large_err,
    reason = "the Axum adapter returns a ready response to preserve the stable error envelope"
)]
async fn require_agent_owner_authority(
    security: &AdminSecurityState,
    principal: &AdminPrincipal,
    credential_id: AgentCredentialId,
    request_id: RequestId,
) -> Result<(), Response> {
    let credential = match security.store.agent_credential_by_id(credential_id).await {
        Ok(Some(credential)) => credential,
        Ok(None) => return Err(not_found(request_id)),
        Err(error) => return Err(load_problem(error, request_id)),
    };
    require_target_authority(
        security,
        principal,
        credential.owner_user_id,
        AdminScope::RoleAssign,
        request_id,
    )
    .await
}

fn fresh_authentication_required(request_id: RequestId) -> Response {
    problem(
        AdminProblem::forbidden(
            "fresh_authentication_required",
            "this identity mutation requires a recently authenticated browser session",
        ),
        request_id,
    )
}

#[expect(
    clippy::result_large_err,
    reason = "the Axum adapter returns a ready response to preserve the stable error envelope"
)]
fn mutation_audit(
    principal: &AdminPrincipal,
    request_id: RequestId,
    headers: &HeaderMap,
) -> Result<MutationAuditContext, Response> {
    let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER).iter();
    let value = values.next().ok_or_else(|| {
        problem(
            AdminProblem::bad_request(
                "missing_idempotency_key",
                "Idempotency-Key is required for identity mutations",
            ),
            request_id,
        )
    })?;
    if values.next().is_some() {
        return Err(invalid_idempotency_key(request_id));
    }
    let encoded = value
        .to_str()
        .map_err(|_| invalid_idempotency_key(request_id))?;
    let key = parse_canonical_uuid(encoded).map_err(|()| invalid_idempotency_key(request_id))?;
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
    Ok(MutationAuditContext {
        audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
        principal,
        request_id: Some(request_id.0),
        idempotency_key: AdminMutationKey(key),
    })
}

fn invalid_idempotency_key(request_id: RequestId) -> Response {
    problem(
        AdminProblem::bad_request(
            "invalid_idempotency_key",
            "Idempotency-Key must contain one canonical UUID",
        ),
        request_id,
    )
}

fn user_summary(user: &StoredUser) -> UserSummaryResponse {
    let mut credential_providers = Vec::with_capacity(2);
    if user.has_password {
        credential_providers.push(HumanLoginProvider::Password);
    }
    if user.has_nostr {
        credential_providers.push(HumanLoginProvider::Nostr);
    }
    UserSummaryResponse {
        user_id: user.user_id,
        status: user.status,
        version: user.version,
        roles: user.roles.iter().copied().collect(),
        scopes: user.scopes().into_iter().collect(),
        credential_providers,
        created_at: user.created_at.to_offset(UtcOffset::UTC),
        updated_at: user.updated_at.to_offset(UtcOffset::UTC),
    }
}

fn user_response(user: &StoredUser, credentials: Vec<StoredHumanCredential>) -> UserResponse {
    UserResponse {
        user_id: user.user_id,
        status: user.status,
        version: user.version,
        roles: user.roles.iter().copied().collect(),
        scopes: user.scopes().into_iter().collect(),
        credentials: credentials.into_iter().map(credential_response).collect(),
        created_at: user.created_at.to_offset(UtcOffset::UTC),
        updated_at: user.updated_at.to_offset(UtcOffset::UTC),
    }
}

fn credential_response(credential: StoredHumanCredential) -> HumanCredentialResponse {
    match credential {
        StoredHumanCredential::Password {
            username,
            version,
            created_at,
            updated_at,
        } => HumanCredentialResponse::Password {
            username: username.as_str().into(),
            version,
            created_at: created_at.to_offset(UtcOffset::UTC),
            updated_at: updated_at.to_offset(UtcOffset::UTC),
        },
        StoredHumanCredential::Nostr {
            public_key,
            version,
            created_at,
            updated_at,
        } => HumanCredentialResponse::Nostr {
            public_key: public_key.as_str().into(),
            version,
            created_at: created_at.to_offset(UtcOffset::UTC),
            updated_at: updated_at.to_offset(UtcOffset::UTC),
        },
    }
}

fn agent_response(credential: &StoredAgentCredential) -> AgentCredentialResponse {
    AgentCredentialResponse {
        agent_credential_id: credential.credential_id,
        owner_user_id: credential.owner_user_id,
        issuer_user_id: credential.issuer_user_id,
        public_key: credential.public_key.as_str().into(),
        label: credential.label.clone(),
        scopes: credential.scopes.iter().copied().collect(),
        effective_scopes: credential.effective_scopes().into_iter().collect(),
        version: credential.version,
        created_at: credential.created_at.to_offset(UtcOffset::UTC),
        expires_at: credential
            .expires_at
            .map(|value| value.to_offset(UtcOffset::UTC)),
        last_used_at: credential
            .last_used_at
            .map(|value| value.to_offset(UtcOffset::UTC)),
        revoked_at: credential
            .revoked_at
            .map(|value| value.to_offset(UtcOffset::UTC)),
    }
}

fn audit_response(event: StoredAdminAuditEvent) -> AdminAuditEventResponse {
    let principal = match event.principal {
        AuditPrincipalReference::BrowserSession {
            user_id,
            session_id,
        } => AuditPrincipalResponse::BrowserSession {
            user_id,
            session_id,
        },
        AuditPrincipalReference::AgentCredential {
            user_id,
            credential_id,
        } => AuditPrincipalResponse::AgentCredential {
            user_id,
            agent_credential_id: credential_id,
        },
        AuditPrincipalReference::Offline { user_id } => AuditPrincipalResponse::Offline { user_id },
        AuditPrincipalReference::Unauthenticated => AuditPrincipalResponse::Unauthenticated,
    };
    let outcome = match event.outcome {
        AdminAuditOutcome::Succeeded => AuditOutcome::Succeeded,
        AdminAuditOutcome::Denied => AuditOutcome::Denied,
        AdminAuditOutcome::Failed => AuditOutcome::Failed,
    };
    AdminAuditEventResponse {
        audit_event_id: event.audit_event_id,
        occurred_at: event.occurred_at.to_offset(UtcOffset::UTC),
        principal,
        request_id: event.request_id,
        idempotency_key: event.idempotency_key,
        action: event.action,
        outcome,
        reason_code: event.reason_code,
    }
}

#[expect(
    clippy::result_large_err,
    reason = "the Axum adapter returns a ready response to preserve the stable error envelope"
)]
fn unique_nonempty<Value: Ord>(
    values: Vec<Value>,
    _name: &'static str,
    request_id: RequestId,
) -> Result<BTreeSet<Value>, Response> {
    if values.is_empty() {
        return Err(invalid_body(request_id));
    }
    let count = values.len();
    let values: BTreeSet<_> = values.into_iter().collect();
    if values.len() != count {
        return Err(invalid_body(request_id));
    }
    Ok(values)
}

fn parse_canonical_uuid(value: &str) -> Result<Uuid, ()> {
    let parsed = Uuid::parse_str(value).map_err(|_| ())?;
    (parsed.hyphenated().to_string() == value)
        .then_some(parsed)
        .ok_or(())
}

fn private_json<T: IntoResponse>(response: T) -> Response {
    let mut response = response.into_response();
    response.headers_mut().insert(CACHE_CONTROL, NO_STORE);
    response
        .headers_mut()
        .insert("x-content-type-options", NOSNIFF);
    response
}

fn invalid_body(request_id: RequestId) -> Response {
    problem(
        AdminProblem::bad_request(
            "invalid_identity_request",
            "the identity request body is invalid",
        ),
        request_id,
    )
}

fn invalid_query(request_id: RequestId) -> Response {
    problem(
        AdminProblem::bad_request(
            "invalid_identity_query",
            "identity pagination parameters are invalid",
        ),
        request_id,
    )
}

fn invalid_path(request_id: RequestId) -> Response {
    problem(
        AdminProblem::bad_request(
            "invalid_identity_identifier",
            "the identity identifier is not a canonical UUID or provider",
        ),
        request_id,
    )
}

fn not_found(request_id: RequestId) -> Response {
    problem(
        AdminProblem::new(
            StatusCode::NOT_FOUND,
            "identity_not_found",
            "the requested identity resource does not exist",
        ),
        request_id,
    )
}

fn password_problem(error: PasswordExecutorError, request_id: RequestId) -> Response {
    let spec = match error {
        PasswordExecutorError::Busy => AdminProblem::new(
            StatusCode::TOO_MANY_REQUESTS,
            "password_hashing_busy",
            "password hashing capacity is temporarily exhausted",
        ),
        PasswordExecutorError::Hashing(PasswordHashingError::InvalidPassword(_)) => {
            return invalid_body(request_id);
        }
        PasswordExecutorError::Closed
        | PasswordExecutorError::InvalidLimits
        | PasswordExecutorError::WorkerFailed(_)
        | PasswordExecutorError::Hashing(PasswordHashingError::CryptographicFailure)
        | PasswordExecutorError::Verification(_) => identity_unavailable(),
    };
    if spec.status.is_server_error() {
        tracing::error!(%request_id, error = %error, "password hashing failed internally");
    }
    problem(spec, request_id)
}

fn load_problem(error: AuthLoadError, request_id: RequestId) -> Response {
    tracing::error!(%request_id, error = %error, "identity state lookup failed");
    problem(identity_unavailable(), request_id)
}

fn mutation_problem(error: AuthMutationError, request_id: RequestId) -> Response {
    let spec = match error {
        AuthMutationError::Admission(
            DatabaseAdmissionError::QueueFull | DatabaseAdmissionError::WriterClosed,
        )
        | AuthMutationError::Command(
            AuthCommandError::OutcomeUnknown
            | AuthCommandError::BootstrapRequired
            | AuthCommandError::NoLoginProvider
            | AuthCommandError::ChallengeCapacity
            | AuthCommandError::SessionCapacity
            | AuthCommandError::ReplayCapacity
            | AuthCommandError::AgentCredentialCapacity
            | AuthCommandError::InvalidChallenge
            | AuthCommandError::ReplayedProof,
        ) => identity_unavailable(),
        AuthMutationError::Command(AuthCommandError::NotFound) => {
            return not_found(request_id);
        }
        AuthMutationError::Command(AuthCommandError::StaleVersion) => AdminProblem::new(
            StatusCode::PRECONDITION_FAILED,
            "stale_identity_version",
            "the expected identity resource version is stale",
        ),
        AuthMutationError::Command(AuthCommandError::IdempotencyConflict) => AdminProblem::new(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key is already bound to another identity command",
        ),
        AuthMutationError::Command(AuthCommandError::ScopeEscalation) => AdminProblem::forbidden(
            "scope_escalation",
            "the identity mutation exceeds the authenticated authority",
        ),
        AuthMutationError::Command(AuthCommandError::InvalidValue) => {
            return invalid_body(request_id);
        }
        AuthMutationError::Command(
            AuthCommandError::AlreadyBootstrapped
            | AuthCommandError::Conflict
            | AuthCommandError::EnabledUserRequiresCredential
            | AuthCommandError::LastEnabledOwner,
        ) => AdminProblem::new(
            StatusCode::CONFLICT,
            "identity_conflict",
            "the identity mutation conflicts with durable account invariants",
        ),
    };
    if spec.status.is_server_error() {
        tracing::error!(%request_id, error = %error, "identity mutation failed");
    }
    problem(spec, request_id)
}

fn problem(spec: AdminProblem, request_id: RequestId) -> Response {
    let status = spec.status;
    let mut response = problem_response(spec, request_id);
    response.headers_mut().insert(CACHE_CONTROL, NO_STORE);
    response
        .headers_mut()
        .insert("x-content-type-options", NOSNIFF);
    if matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    ) {
        response
            .headers_mut()
            .insert("retry-after", RETRY_AFTER_ONE_SECOND);
    }
    response
}

const fn identity_unavailable() -> AdminProblem {
    AdminProblem::unavailable(
        "identity_unavailable",
        "identity management is temporarily unavailable",
    )
}
