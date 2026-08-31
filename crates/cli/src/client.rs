use std::fmt;

use maincopy_shared::{
    CAPABILITIES_PATH, Capabilities,
    auth_api::{
        ADMIN_SESSIONS_PATH, AdminSessionResponse, CSRF_COOKIE_NAME, CSRF_HEADER_NAME,
        CURRENT_ADMIN_SESSION_PATH, CreateAdminSessionRequest, RevokeAdminSessionResponse,
        SESSION_COOKIE_NAME, SecretString,
    },
    posts::{ListPostsResponse, POSTS_PATH},
    publication::{
        CONTENT_DIGEST_HEADER, IDEMPOTENCY_KEY_HEADER, POST_REVISION_HEADER, PREVIEW_DIGEST_HEADER,
        PUBLICATIONS_PATH, PreviewDigest, PublishNowRequest, PublishNowResponse,
    },
};
use reqwest::{
    Method, StatusCode, Url,
    header::{
        ACCEPT, AUTHORIZATION, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, LINK, ORIGIN,
        SET_COOKIE,
    },
};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    credentials::{CredentialKey, CredentialStoreError, PlatformCredentialStore, SecretValue},
    models::AuthenticationContext,
    nip98::{AgentPrivateKey, AgentPrivateKeyError, Nip98SigningError, authorization_proof},
    transport::{HttpRequest, HttpResponse, ReqwestExecutor, TransportError},
};

const REQUEST_ID_HEADER: &str = "x-request-id";
const MAX_ERROR_CODE_BYTES: usize = 64;
const MAX_ERROR_MESSAGE_BYTES: usize = 512;
const POST_REVISION_PREFIX: &str = "post-b3-v1-";
const CONTENT_DIGEST_PREFIX: &str = "content-b3-v1-";
const MAX_JSON_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_PREVIEW_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;
struct AdminOrigin {
    serialized: Box<str>,
}

impl AdminOrigin {
    fn parse(value: &str) -> Result<Self, AdminClientError> {
        let url = Url::parse(value).map_err(|_| AdminClientError::InvalidAdminOrigin)?;
        if url.scheme() != "https"
            || url.cannot_be_a_base()
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(AdminClientError::InvalidAdminOrigin);
        }
        let serialized = url.origin().ascii_serialization();
        if serialized != value {
            return Err(AdminClientError::InvalidAdminOrigin);
        }
        Ok(Self {
            serialized: serialized.into_boxed_str(),
        })
    }

    fn as_str(&self) -> &str {
        &self.serialized
    }

    fn request_url(&self, path: &str) -> Result<Url, AdminClientError> {
        if !path.starts_with('/') || path.starts_with("//") || path.contains('#') {
            return Err(AdminClientError::InvalidRequestTarget);
        }
        let url = Url::parse(&format!("{}{path}", self.as_str()))
            .map_err(|_| AdminClientError::InvalidRequestTarget)?;
        if url.origin().ascii_serialization() != self.as_str()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(AdminClientError::InvalidRequestTarget);
        }
        Ok(url)
    }
}

/// HTTPS administration client with an origin-bound protected credential context.
pub(crate) struct AdminClient {
    origin: AdminOrigin,
    authentication: AuthenticationContext,
    credentials: PlatformCredentialStore,
    executor: ReqwestExecutor,
}

impl fmt::Debug for AdminClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdminClient")
            .field("origin", &self.origin.as_str())
            .field("authentication", &self.authentication)
            .finish_non_exhaustive()
    }
}

impl AdminClient {
    /// Constructs a production client that rejects non-HTTPS requests and redirects.
    pub(crate) fn new(
        admin_origin: &str,
        authentication: AuthenticationContext,
    ) -> Result<Self, AdminClientError> {
        Ok(Self {
            origin: AdminOrigin::parse(admin_origin)?,
            authentication,
            credentials: PlatformCredentialStore,
            executor: ReqwestExecutor::new()?,
        })
    }

    /// Reports whether this origin already has a protected human session.
    pub(crate) fn human_session_is_stored(&self) -> Result<bool, AdminClientError> {
        self.credentials
            .load(&CredentialKey::human(self.origin.as_str()))
            .map(|credential| credential.is_some())
            .map_err(AdminClientError::from)
    }

    /// Fetches the versions advertised by the running server.
    pub(crate) async fn capabilities(&self) -> Result<Capabilities, AdminClientError> {
        let response = self
            .authenticated_request(Method::GET, CAPABILITIES_PATH, Vec::new(), None)
            .await?;
        decode_json(&require_status(response, StatusCode::OK)?)
    }

    /// Lists one bounded page of post revisions loaded by the running server.
    pub(crate) async fn list_posts_page(
        &self,
        cursor: Option<Uuid>,
        limit: u16,
    ) -> Result<ListPostsResponse, AdminClientError> {
        let mut url = self.origin.request_url(POSTS_PATH)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("limit", &limit.to_string());
            if let Some(cursor) = cursor {
                query.append_pair("cursor", &cursor.hyphenated().to_string());
            }
        }
        let response = self
            .authenticated_request_url(Method::GET, url, Vec::new(), None)
            .await?;
        decode_json(&require_status(response, StatusCode::OK)?)
    }

    /// Fetches one exact private preview and its server-authenticated metadata.
    pub(crate) async fn preview_post(
        &self,
        post_id: Uuid,
        revision: Option<&str>,
        content_digest: Option<&str>,
    ) -> Result<PostPreview, AdminClientError> {
        let path = format!("{POSTS_PATH}/{post_id}/preview");
        let mut url = self.origin.request_url(&path)?;
        if revision.is_some() || content_digest.is_some() {
            let mut query = url.query_pairs_mut();
            if let Some(revision) = revision {
                query.append_pair("revision", revision);
            }
            if let Some(content_digest) = content_digest {
                query.append_pair("content_digest", content_digest);
            }
        }
        let response = self
            .authenticated_request_url_with_limit(
                Method::GET,
                url,
                Vec::new(),
                None,
                MAX_PREVIEW_RESPONSE_BYTES,
            )
            .await?;
        let response = require_status(response, StatusCode::OK)?;
        require_content_type(&response.headers, "text/html; charset=utf-8")?;
        decode_preview_response(response)
    }

    /// Approves one exact post revision for immediate or scheduled publication.
    pub(crate) async fn approve_publication(
        &self,
        idempotency_key: Uuid,
        request: &PublishNowRequest,
    ) -> Result<PublishNowResponse, AdminClientError> {
        let body = serde_json::to_vec(request).map_err(AdminClientError::RequestEncoding)?;
        let response = self
            .authenticated_request(Method::POST, PUBLICATIONS_PATH, body, Some(idempotency_key))
            .await?;
        let response = require_status(response, StatusCode::OK)?;
        require_content_type(&response.headers, "application/json")?;
        decode_publication_response(&response.body, &request.preview_digest)
    }

    /// Creates and protects a password-authenticated browser session.
    pub(crate) async fn login_with_password(
        &self,
        username: Box<str>,
        password: SecretString,
    ) -> Result<AdminSessionResponse, AdminClientError> {
        let credential_key = CredentialKey::human(self.origin.as_str());
        if self.human_session_is_stored()? {
            return Err(AdminClientError::HumanSessionAlreadyStored);
        }
        let body = serde_json::to_vec(&CreateAdminSessionRequest::Password { username, password })
            .map_err(AdminClientError::RequestEncoding)?
            .into();
        let url = self.origin.request_url(ADMIN_SESSIONS_PATH)?;
        let mut headers = standard_headers(true);
        headers.insert(ORIGIN, self.origin_header()?);
        let response = self
            .execute(
                HttpRequest {
                    method: Method::POST,
                    url,
                    headers,
                    body,
                },
                MAX_JSON_RESPONSE_BYTES,
            )
            .await?;
        let response = require_status(response, StatusCode::CREATED)?;
        let session: AdminSessionResponse = decode_json(&response)?;
        let credentials = HumanCredentials::from_set_cookie_headers(&response.headers)?;
        if let Err(error) = self
            .credentials
            .save(&credential_key, &credentials.encode())
        {
            let _ = self.revoke_human_credentials(&credentials).await;
            return Err(error.into());
        }
        Ok(session)
    }

    /// Revokes the active human session before deleting its local credentials.
    pub(crate) async fn logout(&self) -> Result<RevokeAdminSessionResponse, AdminClientError> {
        if self.authentication != AuthenticationContext::Human {
            return Err(AdminClientError::HumanContextRequired);
        }
        let credential_key = CredentialKey::human(self.origin.as_str());
        let encoded = self
            .credentials
            .load(&credential_key)?
            .ok_or(AdminClientError::HumanCredentialsMissing)?;
        let credentials = match HumanCredentials::decode(encoded) {
            Ok(credentials) => credentials,
            Err(error) => {
                self.credentials.delete(&credential_key)?;
                return Err(error);
            }
        };
        let response = self.revoke_human_credentials(&credentials).await?;
        let response = require_status(response, StatusCode::OK)?;
        let revoked: RevokeAdminSessionResponse = decode_json(&response)?;
        self.credentials.delete(&credential_key)?;
        Ok(revoked)
    }

    /// Validates and stores an agent private key in the platform credential store.
    pub(crate) fn configure_agent_private_key(
        &self,
        key: SecretString,
    ) -> Result<Box<str>, AdminClientError> {
        let parsed = AgentPrivateKey::parse(key.expose_secret())?;
        let public_key = parsed.public_key_hex().into_boxed_str();
        self.credentials.save(
            &CredentialKey::agent(self.origin.as_str()),
            &SecretValue::new(key.expose_secret()),
        )?;
        Ok(public_key)
    }

    /// Removes the local agent private key without changing its server grant.
    pub(crate) fn remove_agent_private_key(&self) -> Result<(), AdminClientError> {
        self.credentials
            .delete(&CredentialKey::agent(self.origin.as_str()))?;
        Ok(())
    }

    async fn revoke_human_credentials(
        &self,
        credentials: &HumanCredentials,
    ) -> Result<HttpResponse, AdminClientError> {
        let url = self.origin.request_url(CURRENT_ADMIN_SESSION_PATH)?;
        let mut headers = standard_headers(false);
        headers.insert(ORIGIN, self.origin_header()?);
        let cookie = Zeroizing::new(format!(
            "{SESSION_COOKIE_NAME}={}; {CSRF_COOKIE_NAME}={}",
            credentials.session.expose_secret(),
            credentials.csrf.expose_secret()
        ));
        insert_sensitive_header(&mut headers, COOKIE, &cookie)?;
        insert_sensitive_header(
            &mut headers,
            CSRF_HEADER_NAME,
            credentials.csrf.expose_secret(),
        )?;
        self.execute(
            HttpRequest {
                method: Method::DELETE,
                url,
                headers,
                body: Vec::new().into(),
            },
            MAX_JSON_RESPONSE_BYTES,
        )
        .await
    }

    async fn authenticated_request(
        &self,
        method: Method,
        path: &str,
        body: Vec<u8>,
        idempotency_key: Option<Uuid>,
    ) -> Result<HttpResponse, AdminClientError> {
        let url = self.origin.request_url(path)?;
        self.authenticated_request_url(method, url, body, idempotency_key)
            .await
    }

    async fn authenticated_request_url(
        &self,
        method: Method,
        url: Url,
        body: Vec<u8>,
        idempotency_key: Option<Uuid>,
    ) -> Result<HttpResponse, AdminClientError> {
        self.authenticated_request_url_with_limit(
            method,
            url,
            body,
            idempotency_key,
            MAX_JSON_RESPONSE_BYTES,
        )
        .await
    }

    async fn authenticated_request_url_with_limit(
        &self,
        method: Method,
        url: Url,
        body: Vec<u8>,
        idempotency_key: Option<Uuid>,
        maximum_response_bytes: usize,
    ) -> Result<HttpResponse, AdminClientError> {
        if body.len() > MAX_REQUEST_BODY_BYTES {
            return Err(AdminClientError::RequestBodyTooLarge);
        }
        if url.origin().ascii_serialization() != self.origin.as_str()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(AdminClientError::InvalidRequestTarget);
        }
        let mutation = !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS);
        let mut headers = standard_headers(!body.is_empty());

        match self.authentication {
            AuthenticationContext::Human => {
                let credentials = self.load_human_credentials()?;
                authorize_human_request(
                    &mut headers,
                    &self.origin,
                    &credentials,
                    mutation,
                    idempotency_key,
                )?;
            }
            AuthenticationContext::Agent => {
                let private_key = self.load_agent_private_key()?;
                authorize_agent_request(
                    &mut headers,
                    &private_key,
                    OffsetDateTime::now_utc().unix_timestamp(),
                    &url,
                    &method,
                    &body,
                    idempotency_key.unwrap_or_else(Uuid::new_v4),
                )?;
            }
        }

        self.execute(
            HttpRequest {
                method,
                url,
                headers,
                body: body.into(),
            },
            maximum_response_bytes,
        )
        .await
    }

    async fn execute(
        &self,
        request: HttpRequest,
        maximum_response_bytes: usize,
    ) -> Result<HttpResponse, AdminClientError> {
        self.executor
            .execute(request, maximum_response_bytes)
            .await
            .map_err(AdminClientError::from)
    }

    fn load_human_credentials(&self) -> Result<HumanCredentials, AdminClientError> {
        let value = self
            .credentials
            .load(&CredentialKey::human(self.origin.as_str()))?
            .ok_or(AdminClientError::HumanCredentialsMissing)?;
        HumanCredentials::decode(value)
    }

    fn load_agent_private_key(&self) -> Result<AgentPrivateKey, AdminClientError> {
        let value = self
            .credentials
            .load(&CredentialKey::agent(self.origin.as_str()))?
            .ok_or(AdminClientError::AgentCredentialsMissing)?;
        AgentPrivateKey::parse(value.expose_secret()).map_err(AdminClientError::from)
    }

    fn origin_header(&self) -> Result<HeaderValue, AdminClientError> {
        HeaderValue::from_str(self.origin.as_str())
            .map_err(|_| AdminClientError::InvalidAdminOrigin)
    }
}

fn insert_idempotency_header(headers: &mut HeaderMap, idempotency_key: Uuid) -> String {
    let encoded = idempotency_key.hyphenated().to_string();
    headers.insert(
        IDEMPOTENCY_KEY_HEADER,
        HeaderValue::from_str(&encoded).expect("a UUID is always a valid HTTP header value"),
    );
    encoded
}

fn authorize_human_request(
    headers: &mut HeaderMap,
    origin: &AdminOrigin,
    credentials: &HumanCredentials,
    mutation: bool,
    idempotency_key: Option<Uuid>,
) -> Result<(), AdminClientError> {
    let cookie = Zeroizing::new(if mutation {
        format!(
            "{SESSION_COOKIE_NAME}={}; {CSRF_COOKIE_NAME}={}",
            credentials.session.expose_secret(),
            credentials.csrf.expose_secret()
        )
    } else {
        format!(
            "{SESSION_COOKIE_NAME}={}",
            credentials.session.expose_secret()
        )
    });
    insert_sensitive_header(headers, COOKIE, &cookie)?;
    if mutation {
        headers.insert(
            ORIGIN,
            HeaderValue::from_str(origin.as_str())
                .map_err(|_| AdminClientError::InvalidAdminOrigin)?,
        );
        insert_sensitive_header(headers, CSRF_HEADER_NAME, credentials.csrf.expose_secret())?;
    }
    if let Some(idempotency_key) = idempotency_key {
        insert_idempotency_header(headers, idempotency_key);
    }
    Ok(())
}

fn authorize_agent_request(
    headers: &mut HeaderMap,
    private_key: &AgentPrivateKey,
    created_at: i64,
    url: &Url,
    method: &Method,
    body: &[u8],
    idempotency_key: Uuid,
) -> Result<(), AdminClientError> {
    let idempotency_key = insert_idempotency_header(headers, idempotency_key);
    let proof = authorization_proof(
        private_key,
        created_at,
        url.as_str(),
        method.as_str(),
        body,
        &idempotency_key,
    )?;
    let authorization = Zeroizing::new(format!("Nostr {}", proof.as_str()));
    insert_sensitive_header(headers, AUTHORIZATION, &authorization)
}

struct HumanCredentials {
    session: SecretValue,
    csrf: SecretValue,
}

impl HumanCredentials {
    fn from_set_cookie_headers(headers: &HeaderMap) -> Result<Self, AdminClientError> {
        let values = headers
            .get_all(SET_COOKIE)
            .iter()
            .map(|value| {
                value
                    .to_str()
                    .map_err(|_| AdminClientError::InvalidAuthenticationResponse {
                        message: "Set-Cookie is not visible ASCII",
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if values.len() != 2 {
            return Err(AdminClientError::InvalidAuthenticationResponse {
                message: "login must return exactly two authentication cookies",
            });
        }
        let mut session = None;
        let mut csrf = None;
        let mut session_max_age = None;
        let mut csrf_max_age = None;
        for value in values {
            if let Some((token, max_age)) = parse_auth_cookie(value, SESSION_COOKIE_NAME, true) {
                if session.replace(SecretValue::new(token)).is_some() {
                    return Err(invalid_auth_cookies());
                }
                session_max_age = Some(max_age);
            } else if let Some((token, max_age)) = parse_auth_cookie(value, CSRF_COOKIE_NAME, false)
            {
                if csrf.replace(SecretValue::new(token)).is_some() {
                    return Err(invalid_auth_cookies());
                }
                csrf_max_age = Some(max_age);
            } else {
                return Err(invalid_auth_cookies());
            }
        }
        if session_max_age != csrf_max_age {
            return Err(invalid_auth_cookies());
        }
        Ok(Self {
            session: session.ok_or_else(invalid_auth_cookies)?,
            csrf: csrf.ok_or_else(invalid_auth_cookies)?,
        })
    }

    fn encode(&self) -> SecretValue {
        SecretValue::new(
            format!(
                "{}\n{}",
                self.session.expose_secret(),
                self.csrf.expose_secret()
            )
            .into_boxed_str(),
        )
    }

    fn decode(value: SecretValue) -> Result<Self, AdminClientError> {
        let Some((session, csrf)) = value.expose_secret().split_once('\n') else {
            return Err(AdminClientError::StoredCredentialsInvalid);
        };
        if csrf.contains('\n') || !valid_token(session, "mcs1_") || !valid_token(csrf, "mcc1_") {
            return Err(AdminClientError::StoredCredentialsInvalid);
        }
        Ok(Self {
            session: SecretValue::new(session),
            csrf: SecretValue::new(csrf),
        })
    }
}

fn parse_auth_cookie<'a>(value: &'a str, name: &str, http_only: bool) -> Option<(&'a str, u64)> {
    let prefix = format!("{name}=");
    let value = value.strip_prefix(&prefix)?;
    let (token, attributes) = value.split_once("; ")?;
    let token_prefix = if name == SESSION_COOKIE_NAME {
        "mcs1_"
    } else {
        "mcc1_"
    };
    if !valid_token(token, token_prefix) {
        return None;
    }
    let attributes = attributes.split("; ").collect::<Vec<_>>();
    let expected_count = if http_only { 5 } else { 4 };
    if attributes.len() != expected_count
        || attributes[0] != "Path=/"
        || !attributes[1].starts_with("Max-Age=")
        || attributes[2] != "Secure"
        || (http_only && attributes[3] != "HttpOnly")
        || attributes[expected_count - 1] != "SameSite=Strict"
    {
        return None;
    }
    let encoded_age = attributes[1].strip_prefix("Max-Age=")?;
    if encoded_age.is_empty()
        || !encoded_age.bytes().all(|byte| byte.is_ascii_digit())
        || (encoded_age.len() > 1 && encoded_age.starts_with('0'))
    {
        return None;
    }
    let max_age = encoded_age.parse::<u64>().ok()?;
    (max_age > 0).then_some((token, max_age))
}

fn valid_token(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|encoded| {
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

const fn invalid_auth_cookies() -> AdminClientError {
    AdminClientError::InvalidAuthenticationResponse {
        message: "login returned invalid authentication cookies",
    }
}

fn standard_headers(has_json_body: bool) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    if has_json_body {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    headers
}

fn insert_sensitive_header(
    headers: &mut HeaderMap,
    name: impl reqwest::header::IntoHeaderName,
    value: &str,
) -> Result<(), AdminClientError> {
    let mut value =
        HeaderValue::from_str(value).map_err(|_| AdminClientError::StoredCredentialsInvalid)?;
    value.set_sensitive(true);
    headers.insert(name, value);
    Ok(())
}

fn require_success(response: HttpResponse) -> Result<HttpResponse, AdminClientError> {
    if response.status.is_success() {
        return Ok(response);
    }
    let header_request_id = response_request_id(&response.headers);
    let (problem, body_request_id) = decode_problem(&response.body);
    Err(AdminClientError::HttpStatus {
        status: response.status,
        problem,
        request_id: consistent_request_id(header_request_id, body_request_id),
    })
}

fn require_status(
    response: HttpResponse,
    expected: StatusCode,
) -> Result<HttpResponse, AdminClientError> {
    if response.status == expected {
        return Ok(response);
    }
    if !response.status.is_success() {
        return require_success(response);
    }
    Err(AdminClientError::UnexpectedSuccessStatus {
        actual: response.status,
        expected,
    })
}

fn decode_json<Value>(response: &HttpResponse) -> Result<Value, AdminClientError>
where
    Value: serde::de::DeserializeOwned,
{
    require_content_type(&response.headers, "application/json")?;
    serde_json::from_slice(&response.body).map_err(AdminClientError::InvalidResponse)
}

fn require_content_type(
    headers: &HeaderMap,
    expected: &'static str,
) -> Result<(), AdminClientError> {
    let mut values = headers.get_all(CONTENT_TYPE).iter();
    let matches = values
        .next()
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == expected);
    if !matches || values.next().is_some() {
        return Err(AdminClientError::InvalidContentType { expected });
    }
    Ok(())
}

/// Exact private preview representation returned by the administration API.
pub(crate) struct PostPreview {
    pub(crate) html: Box<str>,
    pub(crate) preview_digest: PreviewDigest,
    pub(crate) revision: Box<str>,
    pub(crate) content_digest: Box<str>,
    pub(crate) canonical_url: Box<str>,
}

fn decode_preview_response(response: HttpResponse) -> Result<PostPreview, AdminClientError> {
    let preview_digest =
        required_header(&response.headers, PREVIEW_DIGEST_HEADER).and_then(|value| {
            PreviewDigest::parse(value)
                .map_err(|_| invalid_preview("invalid preview digest header"))
        })?;
    let revision = required_header(&response.headers, POST_REVISION_HEADER).and_then(|value| {
        typed_digest(value, POST_REVISION_PREFIX, "invalid post revision header")
    })?;
    let content_digest =
        required_header(&response.headers, CONTENT_DIGEST_HEADER).and_then(|value| {
            typed_digest(
                value,
                CONTENT_DIGEST_PREFIX,
                "invalid content digest header",
            )
        })?;
    let canonical_url = canonical_link(&response.headers)?;
    let html = std::str::from_utf8(&response.body)
        .map_err(|_| invalid_preview("preview body is not valid UTF-8"))?
        .into();
    Ok(PostPreview {
        html,
        preview_digest,
        revision,
        content_digest,
        canonical_url,
    })
}

fn decode_publication_response(
    body: &[u8],
    expected_preview: &PreviewDigest,
) -> Result<PublishNowResponse, AdminClientError> {
    let response: PublishNowResponse =
        serde_json::from_slice(body).map_err(AdminClientError::InvalidResponse)?;
    if response.preview_digest != *expected_preview {
        return Err(AdminClientError::InvalidPublicationResponse {
            message: "preview_digest does not echo the approved preview",
        });
    }
    Ok(response)
}

fn required_header<'headers>(
    headers: &'headers HeaderMap,
    name: &'static str,
) -> Result<&'headers str, AdminClientError> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .ok_or_else(|| invalid_preview("required preview metadata header is missing"))?;
    if values.next().is_some() {
        return Err(invalid_preview("preview metadata header is repeated"));
    }
    value
        .to_str()
        .map_err(|_| invalid_preview("preview metadata header is not visible ASCII"))
}

fn typed_digest(
    value: &str,
    prefix: &str,
    message: &'static str,
) -> Result<Box<str>, AdminClientError> {
    let valid = value.strip_prefix(prefix).is_some_and(|encoded| {
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if valid {
        Ok(value.into())
    } else {
        Err(invalid_preview(message))
    }
}

fn canonical_link(headers: &HeaderMap) -> Result<Box<str>, AdminClientError> {
    let value = required_header(headers, LINK.as_str())?;
    let Some(value) = value.strip_prefix('<') else {
        return Err(invalid_preview("invalid canonical Link header"));
    };
    let Some((target, parameters)) = value.split_once('>') else {
        return Err(invalid_preview("invalid canonical Link header"));
    };
    if parameters != "; rel=\"canonical\"" {
        return Err(invalid_preview("invalid canonical Link header"));
    }
    let url = Url::parse(target).map_err(|_| invalid_preview("invalid canonical Link target"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.as_str() != target
    {
        return Err(invalid_preview("invalid canonical Link target"));
    }
    Ok(target.into())
}

const fn invalid_preview(message: &'static str) -> AdminClientError {
    AdminClientError::InvalidPreviewResponse { message }
}

fn response_request_id(headers: &HeaderMap) -> Option<Uuid> {
    let mut values = headers.get_all(REQUEST_ID_HEADER).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    canonical_uuid(value)
}

fn consistent_request_id(header: Option<Uuid>, body: Option<Uuid>) -> Option<Uuid> {
    match (header, body) {
        (Some(header), Some(body)) if header == body => Some(header),
        (Some(_), Some(_)) => None,
        (Some(request_id), None) | (None, Some(request_id)) => Some(request_id),
        (None, None) => None,
    }
}

fn decode_problem(body: &[u8]) -> (Option<AdminProblem>, Option<Uuid>) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (None, None);
    };
    let Some(error) = value.get("error").and_then(serde_json::Value::as_object) else {
        return (None, None);
    };
    let request_id = error
        .get("request_id")
        .and_then(serde_json::Value::as_str)
        .and_then(canonical_uuid);
    let problem = error
        .get("code")
        .and_then(serde_json::Value::as_str)
        .zip(error.get("message").and_then(serde_json::Value::as_str))
        .filter(|(code, message)| safe_error_code(code) && safe_error_message(message))
        .map(|(code, message)| AdminProblem {
            code: code.into(),
            message: message.into(),
        });
    (problem, request_id)
}

fn canonical_uuid(value: &str) -> Option<Uuid> {
    let uuid = Uuid::parse_str(value).ok()?;
    (uuid.hyphenated().to_string() == value).then_some(uuid)
}

fn safe_error_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= MAX_ERROR_CODE_BYTES
        && code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn safe_error_message(message: &str) -> bool {
    !message.is_empty()
        && message.len() <= MAX_ERROR_MESSAGE_BYTES
        && message.bytes().all(|byte| (b' '..=b'~').contains(&byte))
}

/// Safe diagnostic details decoded from an admin error response.
#[derive(Debug)]
pub(crate) struct AdminProblem {
    pub(crate) code: Box<str>,
    pub(crate) message: Box<str>,
}

/// Failures produced while preparing or decoding an admin request.
#[derive(Debug, Error)]
pub(crate) enum AdminClientError {
    #[error(
        "the admin origin must be one canonical HTTPS origin without credentials, path, query, or fragment"
    )]
    InvalidAdminOrigin,

    #[error("the administration request target is not on the configured origin")]
    InvalidRequestTarget,

    #[error("the administration request body exceeds the client safety limit")]
    RequestBodyTooLarge,

    #[error("no human session is stored for this admin origin; run `maincopy login`")]
    HumanCredentialsMissing,

    #[error(
        "a human session is already stored for this admin origin; log out before logging in again"
    )]
    HumanSessionAlreadyStored,

    #[error("no agent key is stored for this admin origin; run `maincopy agent-key set`")]
    AgentCredentialsMissing,

    #[error("the stored administration credential is invalid")]
    StoredCredentialsInvalid,

    #[error("this command requires the human authentication context")]
    HumanContextRequired,

    #[error("the admin server response Content-Type must be {expected}")]
    InvalidContentType { expected: &'static str },

    #[error("the admin server returned HTTP {actual}; this response requires HTTP {expected}")]
    UnexpectedSuccessStatus {
        actual: StatusCode,
        expected: StatusCode,
    },

    #[error("the admin server returned invalid authentication metadata: {message}")]
    InvalidAuthenticationResponse { message: &'static str },

    #[error("the administration request could not be encoded: {0}")]
    RequestEncoding(#[source] serde_json::Error),

    #[error("the operating system credential store is unavailable")]
    CredentialStore(#[source] CredentialStoreError),

    #[error("the administration HTTPS request failed")]
    Transport(#[source] TransportError),

    #[error("the Nostr private key is invalid")]
    AgentPrivateKey(#[source] AgentPrivateKeyError),

    #[error("the NIP-98 proof could not be created")]
    Nip98Signing(#[source] Nip98SigningError),

    #[error("the admin server returned HTTP {status}")]
    HttpStatus {
        status: StatusCode,
        problem: Option<AdminProblem>,
        request_id: Option<Uuid>,
    },

    #[error("the admin server returned an invalid response: {0}")]
    InvalidResponse(#[source] serde_json::Error),

    #[error("the admin server returned invalid preview metadata: {message}")]
    InvalidPreviewResponse { message: &'static str },

    #[error("the admin server returned an inconsistent publication response: {message}")]
    InvalidPublicationResponse { message: &'static str },
}

impl From<CredentialStoreError> for AdminClientError {
    fn from(error: CredentialStoreError) -> Self {
        Self::CredentialStore(error)
    }
}

impl From<TransportError> for AdminClientError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl From<AgentPrivateKeyError> for AdminClientError {
    fn from(error: AgentPrivateKeyError) -> Self {
        Self::AgentPrivateKey(error)
    }
}

impl From<Nip98SigningError> for AdminClientError {
    fn from(error: Nip98SigningError) -> Self {
        Self::Nip98Signing(error)
    }
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose};
    use reqwest::header::HeaderValue;
    use serde_json::json;
    use sha2::Digest as _;

    use super::*;

    const REQUEST_ID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    const PREVIEW_DIGEST: &str =
        "preview-b3-v1-1111111111111111111111111111111111111111111111111111111111111111";
    const REVISION: &str =
        "post-b3-v1-2222222222222222222222222222222222222222222222222222222222222222";
    const CONTENT_DIGEST: &str =
        "content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333";

    fn preview_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            PREVIEW_DIGEST_HEADER,
            HeaderValue::from_static(PREVIEW_DIGEST),
        );
        headers.insert(POST_REVISION_HEADER, HeaderValue::from_static(REVISION));
        headers.insert(
            CONTENT_DIGEST_HEADER,
            HeaderValue::from_static(CONTENT_DIGEST),
        );
        headers.insert(
            LINK,
            HeaderValue::from_static("<https://example.test/posts/ready>; rel=\"canonical\""),
        );
        headers
    }

    fn preview_response(headers: HeaderMap, body: Vec<u8>) -> HttpResponse {
        HttpResponse {
            status: StatusCode::OK,
            headers,
            body,
        }
    }

    #[test]
    fn bounded_problem_decoder_keeps_only_safe_operator_details() {
        let body = serde_json::to_vec(&json!({
            "error": {
                "code": "idempotency_conflict",
                "message": "Idempotency-Key is already bound to another command",
                "request_id": REQUEST_ID
            }
        }))
        .unwrap();

        let (problem, request_id) = decode_problem(&body);
        let problem = problem.unwrap();
        assert_eq!(&*problem.code, "idempotency_conflict");
        assert_eq!(
            &*problem.message,
            "Idempotency-Key is already bound to another command"
        );
        assert_eq!(request_id.unwrap().hyphenated().to_string(), REQUEST_ID);

        let unsafe_body = serde_json::to_vec(&json!({
            "error": {
                "code": "not safe",
                "message": "contains\na newline",
                "request_id": REQUEST_ID
            }
        }))
        .unwrap();
        assert!(decode_problem(&unsafe_body).0.is_none());
    }

    #[test]
    fn conflicting_header_and_body_request_ids_are_discarded() {
        let header = Uuid::parse_str(REQUEST_ID).unwrap();
        let body = Uuid::parse_str("dddddddd-dddd-4ddd-8ddd-dddddddddddd").unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static(REQUEST_ID));
        assert_eq!(response_request_id(&headers), Some(header));

        assert_eq!(
            consistent_request_id(Some(header), Some(header)),
            Some(header)
        );
        assert_eq!(consistent_request_id(Some(header), Some(body)), None);
    }

    #[test]
    fn preview_decoder_requires_exact_typed_metadata_and_utf8_html() {
        let preview = decode_preview_response(preview_response(
            preview_headers(),
            b"<!doctype html><title>Ready</title>".to_vec(),
        ))
        .unwrap();

        assert_eq!(preview.preview_digest.as_str(), PREVIEW_DIGEST);
        assert_eq!(preview.revision.as_ref(), REVISION);
        assert_eq!(preview.content_digest.as_ref(), CONTENT_DIGEST);
        assert_eq!(
            preview.canonical_url.as_ref(),
            "https://example.test/posts/ready"
        );
        assert_eq!(preview.html.as_ref(), "<!doctype html><title>Ready</title>");
    }

    #[test]
    fn preview_decoder_rejects_missing_repeated_or_malformed_metadata() {
        for (name, malformed) in [
            (PREVIEW_DIGEST_HEADER, REVISION),
            (POST_REVISION_HEADER, CONTENT_DIGEST),
            (CONTENT_DIGEST_HEADER, REVISION),
        ] {
            let mut headers = preview_headers();
            headers.insert(name, HeaderValue::from_static(malformed));
            assert!(
                decode_preview_response(preview_response(headers, b"<html></html>".to_vec()))
                    .is_err(),
                "{name}"
            );
        }

        let mut missing = preview_headers();
        missing.remove(PREVIEW_DIGEST_HEADER);
        assert!(
            decode_preview_response(preview_response(missing, b"<html></html>".to_vec())).is_err()
        );

        let mut repeated = preview_headers();
        repeated.append(
            PREVIEW_DIGEST_HEADER,
            HeaderValue::from_static(PREVIEW_DIGEST),
        );
        assert!(
            decode_preview_response(preview_response(repeated, b"<html></html>".to_vec())).is_err()
        );

        for link in [
            "https://example.test/posts/ready",
            "<https://example.test/posts/ready>; rel=canonical",
            "<https://user@example.test/posts/ready>; rel=\"canonical\"",
            "<https://example.test/posts/ready#fragment>; rel=\"canonical\"",
            "<https://example.test/posts/ready>; rel=\"canonical\", <https://example.test/other>; rel=\"alternate\"",
        ] {
            let mut headers = preview_headers();
            headers.insert(LINK, HeaderValue::from_str(link).unwrap());
            assert!(
                decode_preview_response(preview_response(headers, b"<html></html>".to_vec()))
                    .is_err(),
                "{link}"
            );
        }

        assert!(decode_preview_response(preview_response(preview_headers(), vec![0xff])).is_err());
    }

    #[test]
    fn publication_decoder_requires_the_server_to_echo_the_approved_preview() {
        let expected = PreviewDigest::parse(PREVIEW_DIGEST).unwrap();
        let matching = serde_json::to_vec(&json!({
            "publication_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "post_id": "11111111-1111-4111-8111-111111111111",
            "preview_digest": PREVIEW_DIGEST,
            "revision": REVISION,
            "state": "published",
            "published_at": "2026-08-30T12:00:00Z",
            "site_digest":
                "site-b3-v1-5555555555555555555555555555555555555555555555555555555555555555",
            "site_version": 2
        }))
        .unwrap();
        assert!(decode_publication_response(&matching, &expected).is_ok());

        let mismatched = matching
            .windows(PREVIEW_DIGEST.len())
            .position(|window| window == PREVIEW_DIGEST.as_bytes())
            .map(|offset| {
                let mut mismatched = matching.clone();
                let last = offset + PREVIEW_DIGEST.len() - 1;
                mismatched[last] = b'5';
                mismatched
            })
            .unwrap();
        assert!(matches!(
            decode_publication_response(&mismatched, &expected),
            Err(AdminClientError::InvalidPublicationResponse { .. })
        ));
    }

    const SESSION_TOKEN: &str =
        "mcs1_1111111111111111111111111111111111111111111111111111111111111111";
    const CSRF_TOKEN: &str =
        "mcc1_2222222222222222222222222222222222222222222222222222222222222222";
    const AGENT_KEY: &str = "0303030303030303030303030303030303030303030303030303030303030303";

    fn authentication_cookie_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_str(&format!(
                "{SESSION_COOKIE_NAME}={SESSION_TOKEN}; Path=/; Max-Age=43200; Secure; HttpOnly; SameSite=Strict"
            ))
            .unwrap(),
        );
        headers.append(
            SET_COOKIE,
            HeaderValue::from_str(&format!(
                "{CSRF_COOKIE_NAME}={CSRF_TOKEN}; Path=/; Max-Age=43200; Secure; SameSite=Strict"
            ))
            .unwrap(),
        );
        headers
    }

    #[test]
    fn human_credentials_apply_exact_read_and_mutation_boundaries() {
        let credentials =
            HumanCredentials::from_set_cookie_headers(&authentication_cookie_headers()).unwrap();
        let decoded = HumanCredentials::decode(credentials.encode()).unwrap();
        let origin = AdminOrigin::parse("https://admin.example.test").unwrap();

        let mut read = standard_headers(false);
        authorize_human_request(&mut read, &origin, &decoded, false, None).unwrap();
        assert_eq!(
            read[COOKIE],
            format!("{SESSION_COOKIE_NAME}={SESSION_TOKEN}")
        );
        assert!(read[COOKIE].is_sensitive());
        assert!(!read.contains_key(CSRF_HEADER_NAME));
        assert!(!read.contains_key(ORIGIN));
        assert!(!read.contains_key(IDEMPOTENCY_KEY_HEADER));

        let idempotency_key = Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap();
        let mut mutation = standard_headers(true);
        authorize_human_request(
            &mut mutation,
            &origin,
            &decoded,
            true,
            Some(idempotency_key),
        )
        .unwrap();
        assert_eq!(
            mutation[COOKIE],
            format!("{SESSION_COOKIE_NAME}={SESSION_TOKEN}; {CSRF_COOKIE_NAME}={CSRF_TOKEN}")
        );
        assert_eq!(mutation[CSRF_HEADER_NAME], CSRF_TOKEN);
        assert!(mutation[CSRF_HEADER_NAME].is_sensitive());
        assert_eq!(mutation[ORIGIN], "https://admin.example.test");
        assert_eq!(
            mutation[IDEMPOTENCY_KEY_HEADER],
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        );
    }

    fn decode_authorization(headers: &HeaderMap) -> serde_json::Value {
        let header = headers[AUTHORIZATION].to_str().unwrap();
        let encoded = header.strip_prefix("Nostr ").unwrap();
        assert!(!encoded.ends_with('='));
        let event = general_purpose::STANDARD_NO_PAD.decode(encoded).unwrap();
        serde_json::from_slice(&event).unwrap()
    }

    #[test]
    fn agent_requests_bind_unique_or_caller_supplied_idempotency_keys() {
        let private_key = AgentPrivateKey::parse(AGENT_KEY).unwrap();
        let url = Url::parse("https://admin.example.test/api/admin/v1/posts").unwrap();
        let first_key = Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap();
        let second_key = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();

        let mut first_headers = standard_headers(false);
        authorize_agent_request(
            &mut first_headers,
            &private_key,
            1_800_000_000,
            &url,
            &Method::GET,
            b"",
            first_key,
        )
        .unwrap();
        let mut second_headers = standard_headers(false);
        authorize_agent_request(
            &mut second_headers,
            &private_key,
            1_800_000_000,
            &url,
            &Method::GET,
            b"",
            second_key,
        )
        .unwrap();

        let first = decode_authorization(&first_headers);
        let second = decode_authorization(&second_headers);
        assert_ne!(first["id"], second["id"]);
        for (headers, event, expected_key) in [
            (&first_headers, &first, first_key),
            (&second_headers, &second, second_key),
        ] {
            assert_eq!(event["kind"], 27_235);
            assert_eq!(event["content"], "");
            assert_eq!(event["tags"][0], json!(["u", url.as_str()]));
            assert_eq!(event["tags"][1], json!(["method", "GET"]));
            assert_eq!(
                event["tags"][2],
                json!(["payload", format!("{:x}", sha2::Sha256::digest(b""))])
            );
            assert_eq!(
                event["tags"][3],
                json!(["idempotency", expected_key.hyphenated().to_string()])
            );
            assert_eq!(
                headers[IDEMPOTENCY_KEY_HEADER],
                expected_key.hyphenated().to_string()
            );
            assert!(headers[AUTHORIZATION].is_sensitive());
        }

        let publication_url =
            Url::parse("https://admin.example.test/api/admin/v1/publications").unwrap();
        let body = br#"{"preserve":[1,2,3]}"#;
        let caller_key = Uuid::parse_str("cccccccc-cccc-4ccc-8ccc-cccccccccccc").unwrap();
        let mut publication_headers = standard_headers(true);
        authorize_agent_request(
            &mut publication_headers,
            &private_key,
            1_800_000_001,
            &publication_url,
            &Method::POST,
            body,
            caller_key,
        )
        .unwrap();
        let publication = decode_authorization(&publication_headers);
        assert_eq!(
            publication_headers[IDEMPOTENCY_KEY_HEADER],
            caller_key.hyphenated().to_string()
        );
        assert_eq!(
            publication["tags"][2],
            json!(["payload", format!("{:x}", sha2::Sha256::digest(body))])
        );
        assert_eq!(
            publication["tags"][3],
            json!(["idempotency", caller_key.hyphenated().to_string()])
        );
    }

    #[test]
    fn origin_cookie_and_diagnostics_boundaries_reject_unsafe_inputs() {
        for value in [
            "http://admin.example.test",
            "https://admin.example.test/",
            "https://user@admin.example.test",
            "https://admin.example.test/path",
            "https://admin.example.test?query",
            "https://admin.example.test#fragment",
        ] {
            assert!(matches!(
                AdminOrigin::parse(value),
                Err(AdminClientError::InvalidAdminOrigin)
            ));
        }
        assert!(HumanCredentials::from_set_cookie_headers(&HeaderMap::new()).is_err());

        let client =
            AdminClient::new("https://admin.example.test", AuthenticationContext::Agent).unwrap();
        let diagnostics = format!("{client:?}");
        assert!(!diagnostics.contains(AGENT_KEY));
        assert!(!format!("{:?}", AdminClientError::StoredCredentialsInvalid).contains(AGENT_KEY));
    }
}
