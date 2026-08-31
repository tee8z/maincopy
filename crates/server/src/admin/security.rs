use std::{collections::BTreeSet, sync::Arc, time::Duration as StdDuration};

use axum::{
    Extension, Json,
    body::{Body, to_bytes},
    extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Request},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode, Uri,
        header::{
            AUTHORIZATION, CACHE_CONTROL, CONTENT_SECURITY_POLICY, COOKIE, HOST, ORIGIN,
            REFERRER_POLICY, SET_COOKIE, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
        },
        request::Parts,
    },
    middleware::Next,
    response::{IntoResponse as _, Response},
};
use base64::{Engine as _, engine::general_purpose};
use maincopy_shared::{
    auth::{
        AdminAuditEventId, AdminScope, AdminSessionId, HumanLoginProvider, LoginChallengeId,
        UserId, UserRole, UserStatus, effective_scopes,
    },
    auth_api::{
        ADMIN_SESSIONS_PATH, AdminSessionResponse, CSRF_COOKIE_NAME, CSRF_HEADER_NAME,
        CreateAdminSessionRequest, CreateLoginChallengeRequest, CreateLoginChallengeResponse,
        RevokeAdminSessionResponse, SESSION_COOKIE_NAME, SecretString,
    },
    publication::IDEMPOTENCY_KEY_HEADER,
};
use serde::de::DeserializeOwned;
use thiserror::Error;
use time::{Duration, OffsetDateTime, UtcOffset};
use tokio::sync::Semaphore;
use utoipa_axum::{
    router::{UtoipaMethodRouter, UtoipaMethodRouterExt as _},
    routes,
};
use uuid::Uuid;

use super::{
    origin::AdminOrigin,
    principal::{AdminAuthentication, AdminPrincipal},
    problem::{AdminProblem, AdminProblemEnvelope, problem_response},
    request_id::RequestId,
};
use crate::{
    database::store::DatabaseAdmissionError,
    domain::auth::{
        Argon2idPolicy, CanonicalUsername, CsrfToken, LoginChallenge, MAX_NIP98_EVENT_BYTES,
        Nip98Payload, Nip98Request, PasswordVerification, PasswordVerificationError, SessionToken,
        TokenGenerationError,
        store::{
            AcceptAgentProof, AdminAuditFailureOutcome, AuditPrincipalReference, AuthCommandError,
            AuthLoadError, AuthMutationError, AuthStore, ConfiguredLoginProviders,
            CreateBrowserSession, CreateLoginChallenge, RecordAdminAuditFailure,
            RevokeBrowserSession, SessionAuditContext, SessionAuthenticationEvidence,
            StoredBrowserSession,
        },
        verify_nip98_event,
    },
    password_executor::{PasswordExecutor, PasswordExecutorError},
};

const AUTH_REQUEST_BODY_LIMIT: usize = 32 * 1024;
const AGENT_REQUEST_BODY_LIMIT: usize = 64 * 1024;
const ADMIN_REQUEST_CONCURRENCY: usize = 64;
const ADMIN_REQUEST_TIMEOUT: StdDuration = StdDuration::from_secs(30);
const PRIVATE_NO_STORE: HeaderValue = HeaderValue::from_static("private, no-store");
const NOSNIFF: HeaderValue = HeaderValue::from_static("nosniff");
const DENY_FRAMING: HeaderValue = HeaderValue::from_static("DENY");
const NO_REFERRER: HeaderValue = HeaderValue::from_static("no-referrer");
const ADMIN_CSP: HeaderValue = HeaderValue::from_static(
    "default-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
);
const RETRY_AFTER_ONE_SECOND: HeaderValue = HeaderValue::from_static("1");
const MAX_ENCODED_NIP98_EVENT_BYTES: usize = MAX_NIP98_EVENT_BYTES.div_ceil(3) * 4;

/// Lifetimes for server-side browser credentials and one-time login proofs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AdminSessionPolicy {
    challenge_lifetime: Duration,
    fresh_lifetime: Duration,
    session_lifetime: Duration,
}

impl Default for AdminSessionPolicy {
    fn default() -> Self {
        Self {
            challenge_lifetime: Duration::minutes(5),
            fresh_lifetime: Duration::minutes(15),
            session_lifetime: Duration::hours(12),
        }
    }
}

/// Required authentication state for the production administration router.
#[derive(Clone)]
pub(crate) struct AdminSecurityState {
    pub(crate) origin: AdminOrigin,
    pub(crate) store: AuthStore,
    pub(crate) providers: ConfiguredLoginProviders,
    pub(crate) sessions: AdminSessionPolicy,
    pub(crate) passwords: PasswordExecutor,
    request_slots: Arc<Semaphore>,
}

impl AdminSecurityState {
    /// Initializes the security boundary only after offline identity bootstrap.
    pub(crate) async fn new(
        origin: AdminOrigin,
        store: AuthStore,
        providers: ConfiguredLoginProviders,
        sessions: AdminSessionPolicy,
        password_policy: Argon2idPolicy,
    ) -> Result<Self, AdminSecurityInitializationError> {
        let identity = store.identity_state().await?;
        if identity.bootstrap_required || identity.instance.is_none() {
            return Err(AdminSecurityInitializationError::BootstrapRequired);
        }
        store.validate_provider_compatibility(providers).await?;
        let passwords = PasswordExecutor::new(password_policy).await?;
        Ok(Self {
            origin,
            store,
            providers,
            sessions,
            passwords,
            request_slots: Arc::new(Semaphore::new(ADMIN_REQUEST_CONCURRENCY)),
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum AdminSecurityInitializationError {
    #[error("admin identity bootstrap must complete before the admin router is built")]
    BootstrapRequired,
    #[error(transparent)]
    Load(#[from] AuthLoadError),
    #[error(transparent)]
    Password(#[from] PasswordExecutorError),
}

#[derive(Clone)]
pub(crate) struct BrowserSessionContext {
    pub(crate) session: StoredBrowserSession,
}

impl BrowserSessionContext {
    pub(crate) fn is_fresh_at(&self, now: OffsetDateTime) -> bool {
        self.session.is_active_at(now) && now < self.session.fresh_until
    }
}

/// Login request context admitted only from the configured browser origin.
///
/// Keeping this check in a parts extractor guarantees it runs before the body
/// extractor while leaving host validation at the outer security boundary.
struct TrustedLoginRequest {
    request_id: RequestId,
    security: AdminSecurityState,
}

impl<S> FromRequestParts<S> for TrustedLoginRequest
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = RequestId::from_request_parts(parts, state)
            .await
            .map_err(|rejection| rejection.into_response())?;
        let Extension(security) = Extension::<AdminSecurityState>::from_request_parts(parts, state)
            .await
            .map_err(|rejection| rejection.into_response())?;
        if !has_exact_origin(&parts.headers, &security.origin) {
            return Err(auth_problem(
                AuthErrorSpec::forbidden(
                    "invalid_request_origin",
                    "the request origin does not match the configured admin origin",
                ),
                request_id,
            ));
        }
        Ok(Self {
            request_id,
            security,
        })
    }
}

/// A trusted login request whose target is exactly the session endpoint.
struct SessionCreationRequest(TrustedLoginRequest);

impl<S> FromRequestParts<S> for SessionCreationRequest
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let trusted = TrustedLoginRequest::from_request_parts(parts, state).await?;
        if parts.uri.query().is_some() {
            return Err(auth_problem(
                AuthErrorSpec::invalid_body(),
                trusted.request_id,
            ));
        }
        Ok(Self(trusted))
    }
}

/// Authentication JSON with the API's stable error envelope.
struct AuthJson<T>(T);

impl<S, T> FromRequest<S> for AuthJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = request_id(&request);
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(|_| auth_problem(AuthErrorSpec::invalid_body(), request_id))
    }
}

/// A browser-backed authenticated request.
///
/// Agent authentication remains valid for shared routes, but routes selecting
/// this extractor reject agents with the existing browser-only error contract.
struct RequiredBrowserSession {
    request_id: RequestId,
    security: AdminSecurityState,
    session: StoredBrowserSession,
}

impl<S> FromRequestParts<S> for RequiredBrowserSession
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let request_id = RequestId::from_request_parts(parts, state)
            .await
            .map_err(|rejection| rejection.into_response())?;
        let Extension(security) = Extension::<AdminSecurityState>::from_request_parts(parts, state)
            .await
            .map_err(|rejection| rejection.into_response())?;
        let Some(browser) = parts.extensions.get::<BrowserSessionContext>() else {
            return Err(auth_problem(
                AuthErrorSpec::forbidden(
                    "browser_session_required",
                    "this resource requires a browser session",
                ),
                request_id,
            ));
        };
        Ok(Self {
            request_id,
            security,
            session: browser.session.clone(),
        })
    }
}

pub(super) fn login_challenge_routes() -> UtoipaMethodRouter {
    routes!(create_login_challenge).layer(DefaultBodyLimit::max(AUTH_REQUEST_BODY_LIMIT))
}

pub(super) fn login_session_routes() -> UtoipaMethodRouter {
    routes!(create_admin_session).layer(DefaultBodyLimit::max(AUTH_REQUEST_BODY_LIMIT))
}

pub(super) fn current_session_routes() -> UtoipaMethodRouter {
    routes!(get_current_admin_session, revoke_current_admin_session)
}

pub(super) fn authenticate_layer(
    routes: UtoipaMethodRouter,
    security: &AdminSecurityState,
) -> UtoipaMethodRouter {
    routes.layer(axum::middleware::from_fn_with_state(
        security.clone(),
        authenticate,
    ))
}

pub(super) fn scoped_layer(
    routes: UtoipaMethodRouter,
    security: &AdminSecurityState,
    scope: AdminScope,
) -> UtoipaMethodRouter {
    routes
        .layer(axum::middleware::from_fn_with_state(scope, authorize_scope))
        .layer(axum::middleware::from_fn_with_state(
            security.clone(),
            authenticate,
        ))
}

pub(super) async fn admit_request(
    axum::extract::State(security): axum::extract::State<AdminSecurityState>,
    request: Request,
    next: Next,
) -> Response {
    let request_id = request_id(&request);
    let Ok(_permit) = Arc::clone(&security.request_slots).try_acquire_owned() else {
        return auth_problem(AuthErrorSpec::request_capacity(), request_id);
    };

    match tokio::time::timeout(ADMIN_REQUEST_TIMEOUT, next.run(request)).await {
        Ok(response) => response,
        Err(_) => auth_problem(AuthErrorSpec::request_timeout(), request_id),
    }
}

pub(super) async fn harden_private_response(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, PRIVATE_NO_STORE);
    headers.insert(X_CONTENT_TYPE_OPTIONS, NOSNIFF);
    headers.insert(X_FRAME_OPTIONS, DENY_FRAMING);
    headers.insert(REFERRER_POLICY, NO_REFERRER);
    headers.entry(CONTENT_SECURITY_POLICY).or_insert(ADMIN_CSP);
    response
}

pub(super) async fn validate_host(
    axum::extract::State(security): axum::extract::State<AdminSecurityState>,
    request: Request,
    next: Next,
) -> Response {
    if single_header(request.headers(), HOST)
        .is_none_or(|host| host.as_bytes() != security.origin.authority().as_bytes())
    {
        return auth_problem(
            AuthErrorSpec::forbidden(
                "invalid_admin_host",
                "the request host does not match the configured admin origin",
            ),
            request_id(&request),
        );
    }
    next.run(request).await
}

async fn authenticate(
    axum::extract::State(security): axum::extract::State<AdminSecurityState>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = request_id(&request);
    let session_cookie = match named_cookie(request.headers(), SESSION_COOKIE_NAME) {
        Ok(value) => value,
        Err(()) => return auth_problem(AuthErrorSpec::authentication_failed(), request_id),
    };
    let authorization = match single_optional_header(request.headers(), AUTHORIZATION) {
        Ok(value) => value,
        Err(()) => return auth_problem(AuthErrorSpec::authentication_failed(), request_id),
    };

    if session_cookie.is_some() && authorization.is_some() {
        return auth_problem(AuthErrorSpec::authentication_failed(), request_id);
    }

    if let Some(encoded_token) = session_cookie {
        let Ok(token) = SessionToken::parse(encoded_token) else {
            return auth_problem(AuthErrorSpec::authentication_failed(), request_id);
        };
        let session = match security.store.browser_session(token.digest()).await {
            Ok(Some(session)) if session.is_active_at(OffsetDateTime::now_utc()) => session,
            Ok(_) => return auth_problem(AuthErrorSpec::authentication_failed(), request_id),
            Err(error) => {
                tracing::error!(%request_id, error = %error, "browser session lookup failed");
                return auth_problem(AuthErrorSpec::unavailable(), request_id);
            }
        };

        if is_mutation(request.method())
            && let Err(spec) = validate_cookie_mutation(&request, &security.origin, &session)
        {
            return auth_problem(spec, request_id);
        }

        let principal = AdminPrincipal {
            user_id: session.user_id,
            scopes: Arc::new(session.scopes()),
            authentication: AdminAuthentication::BrowserSession {
                session_id: session.session_id,
            },
        };
        request
            .extensions_mut()
            .insert(BrowserSessionContext { session });
        request.extensions_mut().insert(principal);
        return next.run(request).await;
    }

    let Some(authorization) = authorization else {
        return auth_problem(AuthErrorSpec::authentication_failed(), request_id);
    };
    let event = match decode_nip98_authorization(authorization) {
        Ok(event) => event,
        Err(()) => return auth_problem(AuthErrorSpec::authentication_failed(), request_id),
    };

    let idempotency_key = match agent_idempotency_key(request.headers(), request.method()) {
        Ok(value) => value.map(str::to_owned),
        Err(()) => return auth_problem(AuthErrorSpec::authentication_failed(), request_id),
    };
    let method = request.method().as_str().to_owned();
    let uri = request.uri().clone();
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, AGENT_REQUEST_BODY_LIMIT).await {
        Ok(body) => body,
        Err(_) => {
            return auth_problem(
                AuthErrorSpec::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_body_too_large",
                    "the authenticated request body exceeds the admin limit",
                ),
                request_id,
            );
        }
    };
    let absolute_url = absolute_request_url(&security.origin, &uri);
    let now = OffsetDateTime::now_utc();
    let verified = match verify_nip98_event(
        &event,
        Nip98Request {
            now,
            url: &absolute_url,
            method: &method,
            payload: Nip98Payload::Exact(&body),
            challenge: None,
            idempotency_key: idempotency_key.as_deref(),
        },
    ) {
        Ok(verified) => verified,
        Err(_) => return auth_problem(AuthErrorSpec::authentication_failed(), request_id),
    };
    let credential = match security.store.agent_credential(&verified.public_key).await {
        Ok(Some(credential)) if credential.is_active_at(now) => credential,
        Ok(_) => return auth_problem(AuthErrorSpec::authentication_failed(), request_id),
        Err(error) => {
            tracing::error!(%request_id, error = %error, "agent credential lookup failed");
            return auth_problem(AuthErrorSpec::unavailable(), request_id);
        }
    };
    match security
        .store
        .accept_agent_proof(AcceptAgentProof {
            credential_id: credential.credential_id,
            expected_credential_version: credential.version,
            expected_owner_version: credential.owner_version,
            event_id: verified.event_id,
            accepted_at: now,
            proof_created_at: verified.created_at,
        })
        .await
    {
        Ok(_) => {}
        Err(AuthMutationError::Command(AuthCommandError::ReplayedProof)) => {
            return auth_problem(AuthErrorSpec::proof_replayed(), request_id);
        }
        Err(AuthMutationError::Command(
            AuthCommandError::NotFound | AuthCommandError::StaleVersion,
        )) => return auth_problem(AuthErrorSpec::authentication_failed(), request_id),
        Err(error) => {
            tracing::error!(%request_id, error = %error, "agent proof admission failed");
            return auth_problem(AuthErrorSpec::unavailable(), request_id);
        }
    }

    let principal = AdminPrincipal {
        user_id: credential.owner_user_id,
        scopes: Arc::new(credential.effective_scopes()),
        authentication: AdminAuthentication::AgentCredential {
            credential_id: credential.credential_id,
        },
    };
    let mut request = Request::from_parts(parts, Body::from(body));
    request.extensions_mut().insert(principal);
    next.run(request).await
}

async fn authorize_scope(
    axum::extract::State(scope): axum::extract::State<AdminScope>,
    request: Request,
    next: Next,
) -> Response {
    let request_id = request_id(&request);
    match request.extensions().get::<AdminPrincipal>().cloned() {
        Some(principal) if principal.allows(scope) => next.run(request).await,
        Some(principal) => {
            let Some(security) = request.extensions().get::<AdminSecurityState>().cloned() else {
                tracing::error!(%request_id, "admin security state is missing during authorization");
                return auth_problem(AuthErrorSpec::unavailable(), request_id);
            };
            let denial = RecordAdminAuditFailure {
                audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
                principal: audit_principal(&principal),
                request_id: Some(request_id.0),
                action: format!("authorization.{}", scope.as_str()).into_boxed_str(),
                outcome: AdminAuditFailureOutcome::Denied,
                reason_code: "insufficient_scope".into(),
                occurred_at: OffsetDateTime::now_utc(),
            };
            if let Err(error) = security.store.record_admin_audit_failure(denial).await {
                tracing::error!(%request_id, error = %error, "authorization denial audit failed");
                return auth_problem(AuthErrorSpec::unavailable(), request_id);
            }
            auth_problem(
                AuthErrorSpec::forbidden(
                    "insufficient_scope",
                    "the authenticated principal does not have the required scope",
                ),
                request_id,
            )
        }
        None => auth_problem(AuthErrorSpec::authentication_failed(), request_id),
    }
}

fn audit_principal(principal: &AdminPrincipal) -> AuditPrincipalReference {
    match principal.authentication {
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
    }
}

#[utoipa::path(
    post,
    path = "/api/admin/v1/auth/challenges",
    request_body = CreateLoginChallengeRequest,
    responses(
        (status = CREATED, description = "One short-lived Nostr login challenge", body = CreateLoginChallengeResponse),
        (status = FORBIDDEN, description = "The request origin or provider is not allowed", body = AdminProblemEnvelope),
        (status = CONFLICT, description = "The challenge identifier conflicts with durable state", body = AdminProblemEnvelope),
        (status = SERVICE_UNAVAILABLE, description = "Authentication persistence is unavailable", body = AdminProblemEnvelope)
    ),
    tag = "Authentication"
)]
async fn create_login_challenge(
    TrustedLoginRequest {
        request_id,
        security,
    }: TrustedLoginRequest,
    AuthJson(request): AuthJson<CreateLoginChallengeRequest>,
) -> Response {
    if request.provider != HumanLoginProvider::Nostr
        || !security.providers.accepts(HumanLoginProvider::Nostr)
    {
        return auth_problem(
            AuthErrorSpec::forbidden(
                "login_provider_unavailable",
                "the requested login provider is not enabled",
            ),
            request_id,
        );
    }

    let challenge = match LoginChallenge::generate() {
        Ok(challenge) => challenge,
        Err(error) => return token_generation_problem(error, request_id),
    };
    let now = OffsetDateTime::now_utc();
    let expires_at = now + security.sessions.challenge_lifetime;
    let challenge_id = LoginChallengeId::from_uuid(Uuid::new_v4());
    let stored = security
        .store
        .create_login_challenge(CreateLoginChallenge {
            challenge_id,
            provider: HumanLoginProvider::Nostr,
            challenge_digest: challenge.digest(),
            created_at: now,
            expires_at,
        })
        .await;
    match stored {
        Ok(stored) => (
            StatusCode::CREATED,
            Json(CreateLoginChallengeResponse {
                challenge_id: stored.challenge_id,
                provider: stored.provider,
                challenge: SecretString::new(challenge.expose_secret()),
                expires_at: stored.expires_at.to_offset(UtcOffset::UTC),
            }),
        )
            .into_response(),
        Err(error) => mutation_problem(error, request_id, false),
    }
}

#[utoipa::path(
    post,
    path = "/api/admin/v1/auth/sessions",
    request_body = CreateAdminSessionRequest,
    responses(
        (status = CREATED, description = "A new opaque browser session", body = AdminSessionResponse),
        (status = UNAUTHORIZED, description = "The human login proof is invalid", body = AdminProblemEnvelope),
        (status = FORBIDDEN, description = "The request origin is not allowed", body = AdminProblemEnvelope),
        (status = TOO_MANY_REQUESTS, description = "Password verification capacity is exhausted", body = AdminProblemEnvelope),
        (status = SERVICE_UNAVAILABLE, description = "Authentication is temporarily unavailable", body = AdminProblemEnvelope)
    ),
    tag = "Authentication"
)]
async fn create_admin_session(
    SessionCreationRequest(TrustedLoginRequest {
        request_id,
        security,
    }): SessionCreationRequest,
    AuthJson(request): AuthJson<CreateAdminSessionRequest>,
) -> Response {
    match request {
        CreateAdminSessionRequest::Password { username, password } => {
            create_password_session(&security, &username, password, request_id).await
        }
        CreateAdminSessionRequest::Nostr {
            challenge_id,
            challenge,
            event,
        } => {
            create_nostr_session(
                &security,
                challenge_id,
                challenge.expose_secret(),
                &event,
                request_id,
            )
            .await
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/v1/auth/session",
    responses(
        (status = OK, description = "Current browser session metadata", body = AdminSessionResponse),
        (status = UNAUTHORIZED, description = "No active browser session is present", body = AdminProblemEnvelope),
        (status = FORBIDDEN, description = "The principal is not a browser session", body = AdminProblemEnvelope)
    ),
    tag = "Authentication"
)]
async fn get_current_admin_session(
    RequiredBrowserSession { session, .. }: RequiredBrowserSession,
) -> Response {
    Json(session_response(&session)).into_response()
}

#[utoipa::path(
    delete,
    path = "/api/admin/v1/auth/session",
    responses(
        (status = OK, description = "The current browser session was revoked", body = RevokeAdminSessionResponse),
        (status = UNAUTHORIZED, description = "No active browser session is present", body = AdminProblemEnvelope),
        (status = FORBIDDEN, description = "Origin or CSRF verification failed", body = AdminProblemEnvelope),
        (status = CONFLICT, description = "The session changed before revocation", body = AdminProblemEnvelope),
        (status = SERVICE_UNAVAILABLE, description = "Authentication persistence is unavailable", body = AdminProblemEnvelope)
    ),
    tag = "Authentication"
)]
async fn revoke_current_admin_session(
    RequiredBrowserSession {
        request_id,
        security,
        session,
    }: RequiredBrowserSession,
) -> Response {
    match security
        .store
        .revoke_browser_session(RevokeBrowserSession {
            session_id: session.session_id,
            expected_version: session.version,
            revoked_at: OffsetDateTime::now_utc(),
            audit: SessionAuditContext {
                audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
                request_id: Some(request_id.0),
            },
        })
        .await
    {
        Ok(_) => {
            let mut response = Json(RevokeAdminSessionResponse {
                session_id: session.session_id,
            })
            .into_response();
            clear_auth_cookies(response.headers_mut());
            response
        }
        Err(error) => mutation_problem(error, request_id, false),
    }
}

async fn create_password_session(
    security: &AdminSecurityState,
    username: &str,
    password: SecretString,
    request_id: RequestId,
) -> Response {
    let record = match CanonicalUsername::parse(username) {
        Ok(username) if security.providers.accepts(HumanLoginProvider::Password) => {
            match security.store.password_login(&username).await {
                Ok(record) => record,
                Err(error) => {
                    tracing::error!(%request_id, error = %error, "password credential lookup failed");
                    return auth_problem(AuthErrorSpec::unavailable(), request_id);
                }
            }
        }
        _ => None,
    };
    let (stored, eligible) = match record {
        Some(record) if record.user_status == UserStatus::Enabled => (
            Some(record.password_hash),
            Some((
                record.user_id,
                record.user_version,
                record.roles,
                record.credential_version,
            )),
        ),
        Some(_) | None => (None, None),
    };
    let verification = match security.passwords.verify_password(password, stored).await {
        Ok(verification) => verification,
        Err(PasswordExecutorError::Busy) => {
            return auth_problem(AuthErrorSpec::rate_limited(), request_id);
        }
        Err(PasswordExecutorError::Verification(PasswordVerificationError::InvalidPassword(_))) => {
            return auth_problem(AuthErrorSpec::authentication_failed(), request_id);
        }
        Err(error) => {
            tracing::error!(%request_id, error = %error, "password verification failed internally");
            return auth_problem(AuthErrorSpec::unavailable(), request_id);
        }
    };
    let Some((user_id, user_version, roles, credential_version)) = eligible else {
        return auth_problem(AuthErrorSpec::authentication_failed(), request_id);
    };
    if !matches!(verification, PasswordVerification::Verified { .. }) {
        return auth_problem(AuthErrorSpec::authentication_failed(), request_id);
    }
    finish_session_creation(
        security,
        user_id,
        user_version,
        roles,
        HumanLoginProvider::Password,
        SessionAuthenticationEvidence::Password {
            expected_credential_version: credential_version,
        },
        request_id,
    )
    .await
}

async fn create_nostr_session(
    security: &AdminSecurityState,
    challenge_id: LoginChallengeId,
    challenge: &str,
    event: &str,
    request_id: RequestId,
) -> Response {
    if !security.providers.accepts(HumanLoginProvider::Nostr) {
        return auth_problem(AuthErrorSpec::authentication_failed(), request_id);
    }
    let Ok(challenge_token) = LoginChallenge::parse(challenge) else {
        return auth_problem(AuthErrorSpec::authentication_failed(), request_id);
    };
    let now = OffsetDateTime::now_utc();
    let absolute_url = format!("{}{}", security.origin.as_str(), ADMIN_SESSIONS_PATH);
    let verified = match verify_nip98_event(
        event.as_bytes(),
        Nip98Request {
            now,
            url: &absolute_url,
            method: Method::POST.as_str(),
            payload: Nip98Payload::Absent,
            challenge: Some(challenge),
            idempotency_key: None,
        },
    ) {
        Ok(verified) => verified,
        Err(_) => return auth_problem(AuthErrorSpec::authentication_failed(), request_id),
    };
    let stored_challenge = match security.store.login_challenge(challenge_id).await {
        Ok(Some(stored))
            if stored.provider == HumanLoginProvider::Nostr
                && stored.is_usable_at(now)
                && stored.challenge_digest.ct_eq(&challenge_token.digest()) =>
        {
            stored
        }
        Ok(_) => return auth_problem(AuthErrorSpec::authentication_failed(), request_id),
        Err(error) => {
            tracing::error!(%request_id, error = %error, "login challenge lookup failed");
            return auth_problem(AuthErrorSpec::unavailable(), request_id);
        }
    };
    let record = match security.store.nostr_login(&verified.public_key).await {
        Ok(Some(record)) if record.user_status == UserStatus::Enabled => record,
        Ok(_) => return auth_problem(AuthErrorSpec::authentication_failed(), request_id),
        Err(error) => {
            tracing::error!(%request_id, error = %error, "Nostr credential lookup failed");
            return auth_problem(AuthErrorSpec::unavailable(), request_id);
        }
    };
    finish_session_creation(
        security,
        record.user_id,
        record.user_version,
        record.roles,
        HumanLoginProvider::Nostr,
        SessionAuthenticationEvidence::Nostr {
            expected_credential_version: record.credential_version,
            challenge_id,
            challenge_digest: stored_challenge.challenge_digest,
            event_id: verified.event_id,
            proof_created_at: verified.created_at,
        },
        request_id,
    )
    .await
}

async fn finish_session_creation(
    security: &AdminSecurityState,
    user_id: UserId,
    expected_user_version: u64,
    roles: BTreeSet<UserRole>,
    provider: HumanLoginProvider,
    evidence: SessionAuthenticationEvidence,
    request_id: RequestId,
) -> Response {
    let session_token = match SessionToken::generate() {
        Ok(token) => token,
        Err(error) => return token_generation_problem(error, request_id),
    };
    let csrf_token = match CsrfToken::generate() {
        Ok(token) => token,
        Err(error) => return token_generation_problem(error, request_id),
    };
    let now = OffsetDateTime::now_utc();
    let policy = security.sessions;
    let fresh_until = now + policy.fresh_lifetime;
    let expires_at = now + policy.session_lifetime;
    let session_id = AdminSessionId::from_uuid(Uuid::new_v4());
    match security
        .store
        .create_browser_session(CreateBrowserSession {
            session_id,
            user_id,
            expected_user_version,
            session_token_digest: session_token.digest(),
            csrf_token_digest: csrf_token.digest(),
            evidence,
            authenticated_at: now,
            fresh_until,
            expires_at,
            audit: SessionAuditContext {
                audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
                request_id: Some(request_id.0),
            },
        })
        .await
    {
        Ok(_) => {
            let scopes = effective_scopes(roles.iter().copied());
            let mut response = (
                StatusCode::CREATED,
                Json(AdminSessionResponse {
                    session_id,
                    user_id,
                    provider,
                    roles: roles.into_iter().collect(),
                    scopes: scopes.into_iter().collect(),
                    fresh_until: fresh_until.to_offset(UtcOffset::UTC),
                    expires_at: expires_at.to_offset(UtcOffset::UTC),
                }),
            )
                .into_response();
            set_auth_cookies(
                response.headers_mut(),
                session_token.expose_secret(),
                csrf_token.expose_secret(),
                policy.session_lifetime,
            );
            response
        }
        Err(error) => mutation_problem(error, request_id, true),
    }
}

fn session_response(session: &StoredBrowserSession) -> AdminSessionResponse {
    AdminSessionResponse {
        session_id: session.session_id,
        user_id: session.user_id,
        provider: session.provider,
        roles: session.roles.iter().copied().collect(),
        scopes: session.scopes().into_iter().collect(),
        fresh_until: session.fresh_until.to_offset(UtcOffset::UTC),
        expires_at: session.expires_at.to_offset(UtcOffset::UTC),
    }
}

fn validate_cookie_mutation(
    request: &Request,
    origin: &AdminOrigin,
    session: &StoredBrowserSession,
) -> Result<(), AuthErrorSpec> {
    if !has_exact_origin(request.headers(), origin) {
        return Err(AuthErrorSpec::forbidden(
            "invalid_request_origin",
            "the request origin does not match the configured admin origin",
        ));
    }
    let encoded = single_optional_header(request.headers(), CSRF_HEADER_NAME)
        .map_err(|()| AuthErrorSpec::csrf_failed())?
        .ok_or_else(AuthErrorSpec::csrf_failed)?
        .to_str()
        .map_err(|_| AuthErrorSpec::csrf_failed())?;
    let cookie = named_cookie(request.headers(), CSRF_COOKIE_NAME)
        .map_err(|()| AuthErrorSpec::csrf_failed())?
        .ok_or_else(AuthErrorSpec::csrf_failed)?;
    let header_token = CsrfToken::parse(encoded).map_err(|_| AuthErrorSpec::csrf_failed())?;
    let cookie_token = CsrfToken::parse(cookie).map_err(|_| AuthErrorSpec::csrf_failed())?;
    let header_digest = header_token.digest();
    let cookie_digest = cookie_token.digest();
    if !(header_digest.ct_eq(&cookie_digest) & session.csrf_token_digest.ct_eq(&header_digest)) {
        return Err(AuthErrorSpec::csrf_failed());
    }
    Ok(())
}

fn has_exact_origin(headers: &HeaderMap, origin: &AdminOrigin) -> bool {
    single_header(headers, ORIGIN)
        .is_some_and(|value| value.as_bytes() == origin.as_str().as_bytes())
}

fn single_header(
    headers: &HeaderMap,
    name: impl axum::http::header::AsHeaderName,
) -> Option<&HeaderValue> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn single_optional_header(
    headers: &HeaderMap,
    name: impl axum::http::header::AsHeaderName,
) -> Result<Option<&HeaderValue>, ()> {
    let mut values = headers.get_all(name).iter();
    let value = values.next();
    if values.next().is_some() {
        return Err(());
    }
    Ok(value)
}

fn named_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ()> {
    let mut found = None;
    for header in headers.get_all(COOKIE) {
        let header = header.to_str().map_err(|_| ())?;
        for pair in header.split(';') {
            let pair = pair.trim_matches(|character| character == ' ' || character == '\t');
            let Some((candidate, value)) = pair.split_once('=') else {
                return Err(());
            };
            if candidate == name && (found.replace(value).is_some() || value.is_empty()) {
                return Err(());
            }
        }
    }
    Ok(found)
}

fn decode_nip98_authorization(value: &HeaderValue) -> Result<Vec<u8>, ()> {
    let value = value.to_str().map_err(|_| ())?;
    let (scheme, encoded) = value.split_once(' ').ok_or(())?;
    if !scheme.eq_ignore_ascii_case("Nostr")
        || encoded.is_empty()
        || encoded.len() > MAX_ENCODED_NIP98_EVENT_BYTES
        || encoded.contains(char::is_whitespace)
    {
        return Err(());
    }
    let (engine, canonical): (&general_purpose::GeneralPurpose, fn(&[u8]) -> String) =
        if encoded.ends_with('=') {
            (&general_purpose::STANDARD, |bytes| {
                general_purpose::STANDARD.encode(bytes)
            })
        } else {
            (&general_purpose::STANDARD_NO_PAD, |bytes| {
                general_purpose::STANDARD_NO_PAD.encode(bytes)
            })
        };
    let decoded = engine.decode(encoded).map_err(|_| ())?;
    if decoded.len() > MAX_NIP98_EVENT_BYTES || canonical(&decoded) != encoded {
        return Err(());
    }
    Ok(decoded)
}

fn agent_idempotency_key<'a>(
    headers: &'a HeaderMap,
    method: &Method,
) -> Result<Option<&'a str>, ()> {
    let value = single_optional_header(headers, IDEMPOTENCY_KEY_HEADER)?
        .map(HeaderValue::to_str)
        .transpose()
        .map_err(|_| ())?;
    if is_mutation(method) && value.is_none_or(str::is_empty) {
        return Err(());
    }
    Ok(value)
}

fn absolute_request_url(origin: &AdminOrigin, uri: &Uri) -> String {
    uri.path_and_query().map_or_else(
        || format!("{}/", origin.as_str()),
        |target| origin.absolute_request_url(target),
    )
}

fn is_mutation(method: &Method) -> bool {
    !matches!(method, &Method::GET | &Method::HEAD | &Method::OPTIONS)
}

fn set_auth_cookies(headers: &mut HeaderMap, session: &str, csrf: &str, lifetime: Duration) {
    let max_age = lifetime.whole_seconds();
    append_set_cookie(
        headers,
        &format!(
            "{SESSION_COOKIE_NAME}={session}; Path=/; Max-Age={max_age}; Secure; HttpOnly; SameSite=Strict"
        ),
    );
    append_set_cookie(
        headers,
        &format!("{CSRF_COOKIE_NAME}={csrf}; Path=/; Max-Age={max_age}; Secure; SameSite=Strict"),
    );
}

fn clear_auth_cookies(headers: &mut HeaderMap) {
    append_set_cookie(
        headers,
        &format!("{SESSION_COOKIE_NAME}=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Strict"),
    );
    append_set_cookie(
        headers,
        &format!("{CSRF_COOKIE_NAME}=; Path=/; Max-Age=0; Secure; SameSite=Strict"),
    );
}

fn append_set_cookie(headers: &mut HeaderMap, value: &str) {
    headers.append(
        SET_COOKIE,
        HeaderValue::from_str(value).expect("typed opaque tokens produce safe cookie values"),
    );
}

fn token_generation_problem(error: TokenGenerationError, request_id: RequestId) -> Response {
    tracing::error!(%request_id, error = %error, "authentication token generation failed");
    auth_problem(AuthErrorSpec::unavailable(), request_id)
}

fn mutation_problem(
    error: AuthMutationError,
    request_id: RequestId,
    hide_authentication_conflict: bool,
) -> Response {
    let spec = match error {
        AuthMutationError::Admission(
            DatabaseAdmissionError::QueueFull | DatabaseAdmissionError::WriterClosed,
        )
        | AuthMutationError::Command(
            AuthCommandError::OutcomeUnknown
            | AuthCommandError::BootstrapRequired
            | AuthCommandError::IdempotencyConflict
            | AuthCommandError::InvalidValue,
        ) => AuthErrorSpec::unavailable(),
        AuthMutationError::Command(
            AuthCommandError::InvalidChallenge
            | AuthCommandError::NotFound
            | AuthCommandError::StaleVersion
            | AuthCommandError::ReplayedProof,
        ) if hide_authentication_conflict => AuthErrorSpec::authentication_failed(),
        AuthMutationError::Command(AuthCommandError::ReplayedProof) => {
            AuthErrorSpec::proof_replayed()
        }
        AuthMutationError::Command(
            AuthCommandError::ChallengeCapacity
            | AuthCommandError::SessionCapacity
            | AuthCommandError::ReplayCapacity
            | AuthCommandError::AgentCredentialCapacity,
        ) => AuthErrorSpec::unavailable(),
        AuthMutationError::Command(
            AuthCommandError::AlreadyBootstrapped
            | AuthCommandError::Conflict
            | AuthCommandError::StaleVersion
            | AuthCommandError::InvalidChallenge
            | AuthCommandError::NotFound
            | AuthCommandError::NoLoginProvider
            | AuthCommandError::EnabledUserRequiresCredential
            | AuthCommandError::LastEnabledOwner
            | AuthCommandError::ScopeEscalation,
        ) => AuthErrorSpec::conflict(
            "authentication_conflict",
            "the authentication resource changed before the command completed",
        ),
    };
    if spec.status.is_server_error() {
        tracing::error!(%request_id, error = %error, "authentication mutation failed");
    }
    auth_problem(spec, request_id)
}

fn request_id(request: &Request) -> RequestId {
    request
        .extensions()
        .get::<RequestId>()
        .copied()
        .unwrap_or_else(|| {
            tracing::error!("request ID middleware is missing from the admin security boundary");
            RequestId(Uuid::new_v4())
        })
}

fn auth_problem(spec: AuthErrorSpec, request_id: RequestId) -> Response {
    let mut response = problem_response(
        AdminProblem {
            status: spec.status,
            code: spec.code,
            message: spec.message,
        },
        request_id,
    );
    if spec.status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            "www-authenticate",
            HeaderValue::from_static("Nostr realm=\"maincopy-admin\""),
        );
    }
    if matches!(
        spec.status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    ) {
        response
            .headers_mut()
            .insert("retry-after", RETRY_AFTER_ONE_SECOND);
    }
    response
}

#[derive(Clone, Copy)]
struct AuthErrorSpec {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl AuthErrorSpec {
    const fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }

    const fn authentication_failed() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "authentication_required",
            "a valid admin session or NIP-98 agent proof is required",
        )
    }

    const fn forbidden(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, message)
    }

    const fn conflict(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    const fn proof_replayed() -> Self {
        Self::conflict(
            "nip98_proof_replayed",
            "the NIP-98 event has already been admitted",
        )
    }

    const fn csrf_failed() -> Self {
        Self::forbidden(
            "csrf_verification_failed",
            "the CSRF cookie and request proof are invalid",
        )
    }

    const fn rate_limited() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "password_verification_busy",
            "password verification capacity is temporarily exhausted",
        )
    }

    const fn request_capacity() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "admin_request_capacity",
            "admin request capacity is temporarily exhausted",
        )
    }

    const fn request_timeout() -> Self {
        Self::new(
            StatusCode::REQUEST_TIMEOUT,
            "admin_request_timeout",
            "the admin request did not complete within the allowed time",
        )
    }

    const fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "authentication is temporarily unavailable",
        )
    }

    const fn invalid_body() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_authentication_request",
            "the authentication request body is invalid",
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::{Router, body::Bytes, routing::post};
    use k256::schnorr::SigningKey;
    use serde::Serialize;
    use serde_json::json;
    use sha2::{Digest as _, Sha256};
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt as _;

    use super::*;
    use crate::{
        admin::{admin_router, origin::AdminOrigin},
        config::{
            DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
            DatabaseWriterQueueCapacity,
        },
        database::{self, store::DatabaseStore},
        domain::auth::{
            NIP98_EVENT_KIND, NostrPublicKey,
            store::{
                AdminAuditOutcome, AdminMutationKey, AuditPrincipalReference, BootstrapIdentity,
                CreateUser, MutationAuditContext, NewHumanCredential, RegisterAgentCredential,
            },
        },
    };
    use maincopy_shared::{
        CAPABILITIES_PATH,
        auth::{AdminAuditEventId, AgentCredentialId, InstanceId},
        auth_api::{CURRENT_ADMIN_SESSION_PATH, LOGIN_CHALLENGES_PATH},
    };

    const ADMIN_ORIGIN: &str = "https://admin.example.test";
    const ADMIN_AUTHORITY: &str = "admin.example.test";
    const OWNER_USERNAME: &str = "owner";
    const OWNER_PASSWORD: &str = "correct horse battery staple";

    struct SecurityHarness {
        _root: tempfile::TempDir,
        state: AdminSecurityState,
        store: DatabaseStore,
        publisher_human_key: SigningKey,
        publisher_agent_key: SigningKey,
        shutdown: CancellationToken,
        writer: JoinHandle<()>,
    }

    impl SecurityHarness {
        async fn start() -> Self {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("state/maincopy.db");
            let database = database::bootstrap(database_configuration(&path))
                .await
                .unwrap();
            let (store, writer) = database.into_store(64);
            let shutdown = CancellationToken::new();
            let writer_shutdown = shutdown.clone();
            let writer = tokio::spawn(async move {
                writer.run(writer_shutdown).await.unwrap();
            });

            let providers = ConfiguredLoginProviders::new(true, true).unwrap();
            let owner_user_id = UserId::from_uuid(Uuid::new_v4());
            store
                .auth
                .bootstrap_identity(BootstrapIdentity {
                    instance_id: InstanceId::from_uuid(Uuid::new_v4()),
                    owner_user_id,
                    credential: NewHumanCredential::Password {
                        username: CanonicalUsername::parse(OWNER_USERNAME).unwrap(),
                        password_hash: Argon2idPolicy::v1().hash_password(OWNER_PASSWORD).unwrap(),
                        policy_version: 1,
                    },
                    configured_providers: providers,
                    occurred_at: OffsetDateTime::now_utc(),
                    audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
                })
                .await
                .unwrap();

            let publisher_user_id = UserId::from_uuid(Uuid::new_v4());
            let publisher_human_key = SigningKey::from_bytes(&[4_u8; 32]).unwrap();
            store
                .auth
                .create_user(CreateUser {
                    user_id: publisher_user_id,
                    created_by_user_id: owner_user_id,
                    status: UserStatus::Enabled,
                    roles: BTreeSet::from([UserRole::Publisher]),
                    credentials: vec![NewHumanCredential::Nostr {
                        public_key: public_key(&publisher_human_key),
                    }],
                    configured_providers: providers,
                    occurred_at: OffsetDateTime::now_utc(),
                    audit: mutation_audit(owner_user_id),
                })
                .await
                .unwrap();

            let publisher_agent_key = SigningKey::from_bytes(&[3_u8; 32]).unwrap();
            store
                .auth
                .register_agent_credential(RegisterAgentCredential {
                    credential_id: AgentCredentialId::from_uuid(Uuid::new_v4()),
                    owner_user_id: publisher_user_id,
                    issuer_user_id: owner_user_id,
                    public_key: public_key(&publisher_agent_key),
                    label: "publisher test agent".into(),
                    scopes: AdminScope::PUBLISHER.into_iter().collect(),
                    created_at: OffsetDateTime::now_utc(),
                    expires_at: None,
                    audit: mutation_audit(owner_user_id),
                })
                .await
                .unwrap();

            let state = AdminSecurityState::new(
                AdminOrigin::parse(ADMIN_ORIGIN).unwrap(),
                store.auth.clone(),
                providers,
                AdminSessionPolicy::default(),
                Argon2idPolicy::v1(),
            )
            .await
            .unwrap();
            Self {
                _root: root,
                state,
                store,
                publisher_human_key,
                publisher_agent_key,
                shutdown,
                writer,
            }
        }

        async fn stop(self) {
            drop(self.state);
            drop(self.store);
            self.shutdown.cancel();
            self.writer.await.unwrap();
        }
    }

    fn database_configuration(path: &Path) -> DatabaseConfigurationView<'_> {
        DatabaseConfigurationView {
            path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1_000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(64).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        }
    }

    fn public_key(key: &SigningKey) -> NostrPublicKey {
        NostrPublicKey::from_bytes(key.verifying_key().to_bytes().into()).unwrap()
    }

    fn mutation_audit(user_id: UserId) -> MutationAuditContext {
        MutationAuditContext {
            audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
            principal: AuditPrincipalReference::Offline {
                user_id: Some(user_id),
            },
            request_id: None,
            idempotency_key: AdminMutationKey(Uuid::new_v4()),
        }
    }

    fn lower_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[derive(Serialize)]
    struct SignedEvent {
        id: String,
        pubkey: String,
        created_at: i64,
        kind: u64,
        tags: Vec<Vec<String>>,
        content: String,
        sig: String,
    }

    fn agent_authorization(
        signing_key: &SigningKey,
        method: &Method,
        path: &str,
        body: &[u8],
        idempotency_key: Option<&str>,
    ) -> HeaderValue {
        let created_at = OffsetDateTime::now_utc().unix_timestamp();
        let mut tags = vec![
            vec!["u".into(), format!("{ADMIN_ORIGIN}{path}")],
            vec!["method".into(), method.as_str().into()],
            vec!["payload".into(), lower_hex(&Sha256::digest(body))],
        ];
        if let Some(key) = idempotency_key {
            tags.push(vec!["idempotency".into(), key.into()]);
        }
        let pubkey = lower_hex(&signing_key.verifying_key().to_bytes());
        let content = String::new();
        let encoded =
            serde_json::to_vec(&(0, &pubkey, created_at, NIP98_EVENT_KIND, &tags, &content))
                .unwrap();
        let event_id: [u8; 32] = Sha256::digest(encoded).into();
        let signature = signing_key
            .sign_raw(&event_id, &[7_u8; 32])
            .unwrap()
            .to_bytes();
        let event = SignedEvent {
            id: lower_hex(&event_id),
            pubkey,
            created_at,
            kind: NIP98_EVENT_KIND,
            tags,
            content,
            sig: lower_hex(&signature),
        };
        let encoded = general_purpose::STANDARD.encode(serde_json::to_vec(&event).unwrap());
        HeaderValue::from_str(&format!("Nostr {encoded}")).unwrap()
    }

    fn nostr_login_event(signing_key: &SigningKey, challenge: &str) -> String {
        let created_at = OffsetDateTime::now_utc().unix_timestamp();
        let tags = vec![
            vec!["u".into(), format!("{ADMIN_ORIGIN}{ADMIN_SESSIONS_PATH}")],
            vec!["method".into(), Method::POST.as_str().into()],
            vec!["challenge".into(), challenge.into()],
        ];
        let pubkey = lower_hex(&signing_key.verifying_key().to_bytes());
        let content = String::new();
        let encoded =
            serde_json::to_vec(&(0, &pubkey, created_at, NIP98_EVENT_KIND, &tags, &content))
                .unwrap();
        let event_id: [u8; 32] = Sha256::digest(encoded).into();
        let signature = signing_key
            .sign_raw(&event_id, &[8_u8; 32])
            .unwrap()
            .to_bytes();
        serde_json::to_string(&SignedEvent {
            id: lower_hex(&event_id),
            pubkey,
            created_at,
            kind: NIP98_EVENT_KIND,
            tags,
            content,
            sig: lower_hex(&signature),
        })
        .unwrap()
    }

    fn request(method: Method, path: &str, body: Body) -> Request {
        Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, ADMIN_AUTHORITY)
            .header(ORIGIN, ADMIN_ORIGIN)
            .header("content-type", "application/json")
            .body(body)
            .expect("the test request is valid")
    }

    async fn error_code(response: Response) -> String {
        let body = to_bytes(response.into_body(), AUTH_REQUEST_BODY_LIMIT)
            .await
            .unwrap();
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"]["code"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn authorization_accepts_only_canonical_padded_or_unpadded_base64() {
        let event = br#"{"id":"test"}"#;
        let padded = general_purpose::STANDARD.encode(event);
        let unpadded = general_purpose::STANDARD_NO_PAD.encode(event);
        for encoded in [padded, unpadded] {
            let header = HeaderValue::from_str(&format!("Nostr {encoded}")).unwrap();
            assert_eq!(decode_nip98_authorization(&header).unwrap(), event);
        }

        for invalid in [
            "Nostr ",
            "Bearer abc",
            "Nostr YQ===",
            "Nostr YQ=",
            "Nostr YQ== trailing",
            "Nostr YQ==\t",
        ] {
            let header = HeaderValue::from_str(invalid).unwrap();
            assert!(decode_nip98_authorization(&header).is_err(), "{invalid}");
        }
    }

    #[test]
    fn cookie_parser_rejects_duplicate_or_malformed_security_cookies() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static("other=x; __Host-maincopy_session=mcs1_value"),
        );
        assert_eq!(
            named_cookie(&headers, SESSION_COOKIE_NAME).unwrap(),
            Some("mcs1_value")
        );

        headers.append(
            COOKIE,
            HeaderValue::from_static("__Host-maincopy_session=second"),
        );
        assert!(named_cookie(&headers, SESSION_COOKIE_NAME).is_err());

        let mut malformed = HeaderMap::new();
        malformed.insert(COOKIE, HeaderValue::from_static("not-a-cookie"));
        assert!(named_cookie(&malformed, SESSION_COOKIE_NAME).is_err());
    }

    #[test]
    fn cookie_attributes_are_host_only_secure_and_role_separated() {
        let mut headers = HeaderMap::new();
        set_auth_cookies(
            &mut headers,
            "mcs1_session",
            "mcc1_csrf",
            Duration::hours(1),
        );
        let values = headers
            .get_all(SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 2);
        assert!(values[0].starts_with("__Host-maincopy_session=mcs1_session;"));
        assert!(values[0].contains("; Secure; HttpOnly; SameSite=Strict"));
        assert!(values[1].starts_with("__Host-maincopy_csrf=mcc1_csrf;"));
        assert!(values[1].contains("; Secure; SameSite=Strict"));
        assert!(!values.iter().any(|value| value.contains("Domain=")));
        assert!(!values[1].contains("HttpOnly"));
    }

    #[test]
    fn externally_stable_security_categories_keep_their_status_and_code() {
        let cases = [
            (
                AuthErrorSpec::authentication_failed(),
                StatusCode::UNAUTHORIZED,
                "authentication_required",
            ),
            (
                AuthErrorSpec::forbidden("insufficient_scope", "denied"),
                StatusCode::FORBIDDEN,
                "insufficient_scope",
            ),
            (
                AuthErrorSpec::proof_replayed(),
                StatusCode::CONFLICT,
                "nip98_proof_replayed",
            ),
            (
                AuthErrorSpec::rate_limited(),
                StatusCode::TOO_MANY_REQUESTS,
                "password_verification_busy",
            ),
            (
                AuthErrorSpec::unavailable(),
                StatusCode::SERVICE_UNAVAILABLE,
                "authentication_unavailable",
            ),
        ];
        for (spec, status, code) in cases {
            assert_eq!(spec.status, status);
            assert_eq!(spec.code, code);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn protected_router_enforces_host_origin_cookie_csrf_and_logout() {
        let harness = SecurityHarness::start().await;
        let app = admin_router(harness.state.clone());
        let challenge_body = serde_json::to_vec(&CreateLoginChallengeRequest {
            provider: HumanLoginProvider::Nostr,
        })
        .unwrap();

        let mut wrong_host = request(
            Method::POST,
            LOGIN_CHALLENGES_PATH,
            Body::from(challenge_body.clone()),
        );
        wrong_host
            .headers_mut()
            .insert(HOST, HeaderValue::from_static("loopback.invalid"));
        let wrong_host = app.clone().oneshot(wrong_host).await.unwrap();
        assert_eq!(wrong_host.status(), StatusCode::FORBIDDEN);
        assert_private_hardening(&wrong_host);

        let mut wrong_origin = request(
            Method::POST,
            LOGIN_CHALLENGES_PATH,
            Body::from(challenge_body.clone()),
        );
        wrong_origin
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static("https://evil.example"));
        assert_eq!(
            app.clone().oneshot(wrong_origin).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );

        let mut malformed_from_wrong_origin =
            request(Method::POST, LOGIN_CHALLENGES_PATH, Body::from("{"));
        malformed_from_wrong_origin
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static("https://evil.example"));
        let malformed_from_wrong_origin = app
            .clone()
            .oneshot(malformed_from_wrong_origin)
            .await
            .unwrap();
        assert_eq!(malformed_from_wrong_origin.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            error_code(malformed_from_wrong_origin).await,
            "invalid_request_origin"
        );

        let malformed = request(Method::POST, LOGIN_CHALLENGES_PATH, Body::from("{"));
        let malformed = app.clone().oneshot(malformed).await.unwrap();
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            error_code(malformed).await,
            "invalid_authentication_request"
        );

        let challenge = request(
            Method::POST,
            LOGIN_CHALLENGES_PATH,
            Body::from(challenge_body),
        );
        assert_eq!(
            app.clone().oneshot(challenge).await.unwrap().status(),
            StatusCode::CREATED
        );

        let login_body = serde_json::to_vec(&json!({
            "provider": "password",
            "username": OWNER_USERNAME,
            "password": OWNER_PASSWORD,
        }))
        .unwrap();
        let query_target = request(
            Method::POST,
            &format!("{ADMIN_SESSIONS_PATH}?unexpected=true"),
            Body::from(login_body.clone()),
        );
        let query_target = app.clone().oneshot(query_target).await.unwrap();
        assert_eq!(query_target.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            error_code(query_target).await,
            "invalid_authentication_request"
        );

        let login = request(Method::POST, ADMIN_SESSIONS_PATH, Body::from(login_body));
        let login = app.clone().oneshot(login).await.unwrap();
        assert_eq!(login.status(), StatusCode::CREATED);
        assert_private_hardening(&login);
        let cookie_pairs = login
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .map(|value| {
                value
                    .to_str()
                    .unwrap()
                    .split(';')
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(cookie_pairs.len(), 2);
        let cookie_header = cookie_pairs.join("; ");
        let csrf = cookie_pairs
            .iter()
            .find_map(|cookie| cookie.strip_prefix(&format!("{CSRF_COOKIE_NAME}=")))
            .unwrap()
            .to_owned();

        let current = Request::builder()
            .uri(CURRENT_ADMIN_SESSION_PATH)
            .header(HOST, ADMIN_AUTHORITY)
            .header(COOKIE, &cookie_header)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(current).await.unwrap().status(),
            StatusCode::OK
        );

        let ambiguous = Request::builder()
            .uri(CAPABILITIES_PATH)
            .header(HOST, ADMIN_AUTHORITY)
            .header(COOKIE, &cookie_header)
            .header(AUTHORIZATION, "Nostr invalid")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(ambiguous).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let missing_csrf = Request::builder()
            .method(Method::DELETE)
            .uri(CURRENT_ADMIN_SESSION_PATH)
            .header(HOST, ADMIN_AUTHORITY)
            .header(ORIGIN, ADMIN_ORIGIN)
            .header(COOKIE, &cookie_header)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(missing_csrf).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );

        let logout = Request::builder()
            .method(Method::DELETE)
            .uri(CURRENT_ADMIN_SESSION_PATH)
            .header(HOST, ADMIN_AUTHORITY)
            .header(ORIGIN, ADMIN_ORIGIN)
            .header(COOKIE, &cookie_header)
            .header(CSRF_HEADER_NAME, &csrf)
            .body(Body::empty())
            .unwrap();
        let logout = app.clone().oneshot(logout).await.unwrap();
        assert_eq!(logout.status(), StatusCode::OK);
        assert_eq!(logout.headers().get_all(SET_COOKIE).iter().count(), 2);

        let revoked = Request::builder()
            .uri(CURRENT_ADMIN_SESSION_PATH)
            .header(HOST, ADMIN_AUTHORITY)
            .header(COOKIE, cookie_header)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(revoked).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        harness.stop().await;
    }

    fn assert_private_hardening(response: &Response) {
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            PRIVATE_NO_STORE
        );
        assert_eq!(
            response.headers().get(X_CONTENT_TYPE_OPTIONS).unwrap(),
            NOSNIFF
        );
        assert_eq!(
            response.headers().get(X_FRAME_OPTIONS).unwrap(),
            DENY_FRAMING
        );
        assert_eq!(
            response.headers().get(REFERRER_POLICY).unwrap(),
            NO_REFERRER
        );
        assert_eq!(
            response.headers().get(CONTENT_SECURITY_POLICY).unwrap(),
            ADMIN_CSP
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nostr_login_consumes_one_signed_challenge_exactly_once() {
        let harness = SecurityHarness::start().await;
        let app = admin_router(harness.state.clone());
        let challenge_request = request(
            Method::POST,
            LOGIN_CHALLENGES_PATH,
            Body::from(
                serde_json::to_vec(&CreateLoginChallengeRequest {
                    provider: HumanLoginProvider::Nostr,
                })
                .unwrap(),
            ),
        );
        let challenge_response = app.clone().oneshot(challenge_request).await.unwrap();
        assert_eq!(challenge_response.status(), StatusCode::CREATED);
        let challenge: CreateLoginChallengeResponse = serde_json::from_slice(
            &to_bytes(challenge_response.into_body(), AUTH_REQUEST_BODY_LIMIT)
                .await
                .unwrap(),
        )
        .unwrap();
        let event = nostr_login_event(
            &harness.publisher_human_key,
            challenge.challenge.expose_secret(),
        );
        let login_body = serde_json::to_vec(&json!({
            "provider": "nostr",
            "challenge_id": challenge.challenge_id,
            "challenge": challenge.challenge.expose_secret(),
            "event": event,
        }))
        .unwrap();
        let login_request = || {
            request(
                Method::POST,
                ADMIN_SESSIONS_PATH,
                Body::from(login_body.clone()),
            )
        };
        let accepted = app.clone().oneshot(login_request()).await.unwrap();
        assert_eq!(accepted.status(), StatusCode::CREATED);
        assert_eq!(accepted.headers().get_all(SET_COOKIE).iter().count(), 2);

        let replayed = app.oneshot(login_request()).await.unwrap();
        assert_eq!(replayed.status(), StatusCode::UNAUTHORIZED);
        harness.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_proofs_are_exact_replay_protected_scoped_and_restore_the_body() {
        let harness = SecurityHarness::start().await;
        let app = admin_router(harness.state.clone());
        let path = CAPABILITIES_PATH;
        let authorization =
            agent_authorization(&harness.publisher_agent_key, &Method::GET, path, &[], None);
        let agent_request = || {
            Request::builder()
                .uri(path)
                .header(HOST, ADMIN_AUTHORITY)
                .header(AUTHORIZATION, authorization.clone())
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            app.clone().oneshot(agent_request()).await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(
            app.clone().oneshot(agent_request()).await.unwrap().status(),
            StatusCode::CONFLICT
        );

        let profile_path = "/test/profile";
        let profile_authorization = agent_authorization(
            &harness.publisher_agent_key,
            &Method::GET,
            profile_path,
            &[],
            None,
        );
        let profile_app = Router::new()
            .route(
                profile_path,
                axum::routing::get(|| async { StatusCode::OK }),
            )
            .layer(axum::middleware::from_fn_with_state(
                AdminScope::ProfileManage,
                authorize_scope,
            ))
            .layer(axum::middleware::from_fn_with_state(
                harness.state.clone(),
                authenticate,
            ))
            .layer(axum::middleware::from_fn_with_state(
                harness.state.clone(),
                validate_host,
            ))
            .layer(Extension(harness.state.clone()))
            .layer(axum::middleware::from_fn(super::super::request_id::assign));
        let profile_request = Request::builder()
            .uri(profile_path)
            .header(HOST, ADMIN_AUTHORITY)
            .header(AUTHORIZATION, profile_authorization)
            .body(Body::empty())
            .unwrap();
        let profile_response = profile_app.oneshot(profile_request).await.unwrap();
        assert_eq!(profile_response.status(), StatusCode::FORBIDDEN);
        let audit = harness
            .store
            .auth
            .audit_events_page(None, 32)
            .await
            .unwrap();
        let denial = audit
            .items
            .iter()
            .find(|event| event.action.as_ref() == "authorization.profile_manage")
            .expect("the scope denial must be durably audited");
        assert_eq!(denial.outcome, AdminAuditOutcome::Denied);
        assert_eq!(denial.reason_code.as_deref(), Some("insufficient_scope"));
        assert!(denial.request_id.is_some());
        assert!(matches!(
            denial.principal,
            AuditPrincipalReference::AgentCredential { .. }
        ));

        let session_path = CURRENT_ADMIN_SESSION_PATH;
        let session_authorization = agent_authorization(
            &harness.publisher_agent_key,
            &Method::GET,
            session_path,
            &[],
            None,
        );
        let session_request = Request::builder()
            .uri(session_path)
            .header(HOST, ADMIN_AUTHORITY)
            .header(AUTHORIZATION, session_authorization)
            .body(Body::empty())
            .unwrap();
        let session_response = app.clone().oneshot(session_request).await.unwrap();
        assert_eq!(session_response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            error_code(session_response).await,
            "browser_session_required"
        );

        async fn echo(body: axum::body::Bytes) -> axum::body::Bytes {
            body
        }
        let echo_path = "/test/echo";
        let echo_body = Bytes::from_static(br#"{"exact":"bytes"}"#);
        let idempotency_key = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let echo_authorization = agent_authorization(
            &harness.publisher_agent_key,
            &Method::POST,
            echo_path,
            &echo_body,
            Some(idempotency_key),
        );
        let echo_app = Router::new()
            .route(echo_path, post(echo))
            .layer(axum::middleware::from_fn_with_state(
                AdminScope::ContentRead,
                authorize_scope,
            ))
            .layer(axum::middleware::from_fn_with_state(
                harness.state.clone(),
                authenticate,
            ))
            .layer(axum::middleware::from_fn_with_state(
                harness.state.clone(),
                validate_host,
            ))
            .layer(axum::middleware::from_fn(super::super::request_id::assign));
        let echo_request = Request::builder()
            .method(Method::POST)
            .uri(echo_path)
            .header(HOST, ADMIN_AUTHORITY)
            .header(AUTHORIZATION, echo_authorization)
            .header(IDEMPOTENCY_KEY_HEADER, idempotency_key)
            .body(Body::from(echo_body.clone()))
            .unwrap();
        let echo_response = echo_app.oneshot(echo_request).await.unwrap();
        assert_eq!(echo_response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(echo_response.into_body(), usize::MAX)
                .await
                .unwrap(),
            echo_body
        );
        harness.stop().await;
    }
}
