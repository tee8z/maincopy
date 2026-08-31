//! Stable wire contracts for admin login and browser sessions.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use time::OffsetDateTime;
use zeroize::Zeroize as _;

use uuid::Uuid;

use crate::auth::{
    AdminAuditEventId, AdminScope, AdminSessionId, AgentCredentialId, HumanLoginProvider,
    LoginChallengeId, UserId, UserRole, UserStatus,
};

pub const LOGIN_CHALLENGES_PATH: &str = "/api/admin/v1/auth/challenges";
pub const ADMIN_SESSIONS_PATH: &str = "/api/admin/v1/auth/sessions";
pub const CURRENT_ADMIN_SESSION_PATH: &str = "/api/admin/v1/auth/session";
pub const SESSION_COOKIE_NAME: &str = "__Host-maincopy_session";
pub const CSRF_COOKIE_NAME: &str = "__Host-maincopy_csrf";
pub const CSRF_HEADER_NAME: &str = "x-maincopy-csrf";
pub const ADMIN_USERS_PATH: &str = "/api/admin/v1/identity/users";
pub const ADMIN_USER_PATH: &str = "/api/admin/v1/identity/users/{user_id}";
pub const ADMIN_USER_STATUS_PATH: &str = "/api/admin/v1/identity/users/{user_id}/status";
pub const ADMIN_USER_ROLES_PATH: &str = "/api/admin/v1/identity/users/{user_id}/roles";
pub const ADMIN_USER_CREDENTIAL_PATH: &str =
    "/api/admin/v1/identity/users/{user_id}/credentials/{provider}";
pub const ADMIN_AGENT_CREDENTIALS_PATH: &str = "/api/admin/v1/identity/agents";
pub const ADMIN_AGENT_CREDENTIAL_PATH: &str = "/api/admin/v1/identity/agents/{agent_credential_id}";
pub const ADMIN_AGENT_SCOPES_PATH: &str =
    "/api/admin/v1/identity/agents/{agent_credential_id}/scopes";
pub const ADMIN_AUDIT_EVENTS_PATH: &str = "/api/admin/v1/audit/events";
pub const DEFAULT_IDENTITY_PAGE_LIMIT: u16 = 50;
pub const MAX_IDENTITY_PAGE_LIMIT: u16 = 100;

/// A serialized secret that is redacted from diagnostics and erased on drop.
pub struct SecretString(Box<str>);

impl SecretString {
    pub fn new(value: impl Into<Box<str>>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString(<redacted>)")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl Serialize for SecretString {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        serializer.serialize_str(self.expose_secret())
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        struct SecretVisitor;

        impl de::Visitor<'_> for SecretVisitor {
            type Value = SecretString;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a string")
            }

            fn visit_str<ErrorType>(self, value: &str) -> Result<Self::Value, ErrorType>
            where
                ErrorType: de::Error,
            {
                Ok(SecretString::new(value))
            }

            fn visit_string<ErrorType>(self, value: String) -> Result<Self::Value, ErrorType>
            where
                ErrorType: de::Error,
            {
                Ok(SecretString::new(value.into_boxed_str()))
            }
        }

        deserializer.deserialize_string(SecretVisitor)
    }
}

#[cfg(feature = "schema")]
impl utoipa::PartialSchema for SecretString {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        <String as utoipa::PartialSchema>::schema()
    }
}

#[cfg(feature = "schema")]
impl utoipa::ToSchema for SecretString {}

/// Requests one short-lived proof challenge for a human login provider.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateLoginChallengeRequest {
    pub provider: HumanLoginProvider,
}

/// One short-lived challenge. The raw value is returned once and stored only as a digest.
#[derive(Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct CreateLoginChallengeResponse {
    pub challenge_id: LoginChallengeId,
    pub provider: HumanLoginProvider,
    pub challenge: SecretString,
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
    pub expires_at: OffsetDateTime,
}

/// A human proof that can create one opaque server-side browser session.
///
/// This type intentionally has no `Debug` implementation because the password
/// variant contains a raw secret.
#[derive(Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum CreateAdminSessionRequest {
    Password {
        username: Box<str>,
        password: SecretString,
    },
    Nostr {
        challenge_id: LoginChallengeId,
        challenge: SecretString,
        /// The exact signed NIP-98 event JSON produced by the user's signer.
        event: Box<str>,
    },
}

/// Public metadata for one newly created or currently authenticated session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct AdminSessionResponse {
    pub session_id: AdminSessionId,
    pub user_id: UserId,
    pub provider: HumanLoginProvider,
    pub roles: Vec<UserRole>,
    pub scopes: Vec<AdminScope>,
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
    pub fresh_until: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
    pub expires_at: OffsetDateTime,
}

/// Confirmation that the current browser session was revoked.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct RevokeAdminSessionResponse {
    pub session_id: AdminSessionId,
}

/// A human credential supplied while creating or updating an account.
///
/// This type intentionally has no `Debug` or `Clone` implementation because
/// the password variant owns an unredacted secret.
#[derive(Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum HumanCredentialInput {
    Password {
        username: Box<str>,
        password: SecretString,
    },
    Nostr {
        public_key: Box<str>,
    },
}

impl HumanCredentialInput {
    pub const fn provider(&self) -> HumanLoginProvider {
        match self {
            Self::Password { .. } => HumanLoginProvider::Password,
            Self::Nostr { .. } => HumanLoginProvider::Nostr,
        }
    }
}

/// Non-secret metadata for one human login credential.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum HumanCredentialResponse {
    Password {
        username: Box<str>,
        version: u64,
        #[serde(with = "time::serde::rfc3339")]
        #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
        created_at: OffsetDateTime,
        #[serde(with = "time::serde::rfc3339")]
        #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
        updated_at: OffsetDateTime,
    },
    Nostr {
        public_key: Box<str>,
        version: u64,
        #[serde(with = "time::serde::rfc3339")]
        #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
        created_at: OffsetDateTime,
        #[serde(with = "time::serde::rfc3339")]
        #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
        updated_at: OffsetDateTime,
    },
}

/// One account summary returned by the bounded identity listing.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct UserSummaryResponse {
    pub user_id: UserId,
    pub status: UserStatus,
    pub version: u64,
    pub roles: Vec<UserRole>,
    pub scopes: Vec<AdminScope>,
    pub credential_providers: Vec<HumanLoginProvider>,
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
    pub updated_at: OffsetDateTime,
}

/// Full non-secret account state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct UserResponse {
    pub user_id: UserId,
    pub status: UserStatus,
    pub version: u64,
    pub roles: Vec<UserRole>,
    pub scopes: Vec<AdminScope>,
    pub credentials: Vec<HumanCredentialResponse>,
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
    pub updated_at: OffsetDateTime,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ListUsersResponse {
    pub users: Vec<UserSummaryResponse>,
    pub next_cursor: Option<UserId>,
}

/// Creates one account and its initial credentials atomically.
///
/// This type intentionally has no `Debug` or `Clone` implementation because
/// a nested credential can own an unredacted password.
#[derive(Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateUserRequest {
    pub status: UserStatus,
    pub roles: Vec<UserRole>,
    pub credentials: Vec<HumanCredentialInput>,
}

/// The exact durable outcome of a user mutation.
///
/// Clients can fetch the user resource when they need its current projection.
/// Retrying the command with the same `Idempotency-Key` returns this same
/// identifier and version without applying the mutation again.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct UserMutationResponse {
    pub user_id: UserId,
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct SetUserStatusRequest {
    pub expected_version: u64,
    pub status: UserStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ReplaceUserRolesRequest {
    pub expected_version: u64,
    pub roles: Vec<UserRole>,
}

/// Creates or replaces one provider-specific human credential.
#[derive(Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum PutHumanCredentialRequest {
    Create {
        credential: HumanCredentialInput,
    },
    Replace {
        expected_version: u64,
        credential: HumanCredentialInput,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ExpectedVersionRequest {
    pub expected_version: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct AgentCredentialResponse {
    pub agent_credential_id: AgentCredentialId,
    pub owner_user_id: UserId,
    pub issuer_user_id: UserId,
    pub public_key: Box<str>,
    pub label: Box<str>,
    pub scopes: Vec<AdminScope>,
    pub effective_scopes: Vec<AdminScope>,
    pub version: u64,
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
    pub created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[cfg_attr(feature = "schema", schema(value_type = Option<String>, format = DateTime))]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[cfg_attr(feature = "schema", schema(value_type = Option<String>, format = DateTime))]
    pub last_used_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[cfg_attr(feature = "schema", schema(value_type = Option<String>, format = DateTime))]
    pub revoked_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ListAgentCredentialsResponse {
    pub agent_credentials: Vec<AgentCredentialResponse>,
    pub next_cursor: Option<AgentCredentialId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct RegisterAgentCredentialRequest {
    pub owner_user_id: UserId,
    pub public_key: Box<str>,
    pub label: Box<str>,
    pub scopes: Vec<AdminScope>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[cfg_attr(feature = "schema", schema(value_type = Option<String>, format = DateTime))]
    pub expires_at: Option<OffsetDateTime>,
}

/// The exact durable outcome of an agent-credential mutation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct AgentCredentialMutationResponse {
    pub agent_credential_id: AgentCredentialId,
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ReplaceAgentScopesRequest {
    pub expected_version: u64,
    pub scopes: Vec<AdminScope>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Succeeded,
    Denied,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditPrincipalResponse {
    BrowserSession {
        user_id: UserId,
        session_id: AdminSessionId,
    },
    AgentCredential {
        user_id: UserId,
        agent_credential_id: AgentCredentialId,
    },
    Offline {
        user_id: Option<UserId>,
    },
    Unauthenticated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct AdminAuditEventResponse {
    pub audit_event_id: AdminAuditEventId,
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
    pub occurred_at: OffsetDateTime,
    pub principal: AuditPrincipalResponse,
    pub request_id: Option<Uuid>,
    pub idempotency_key: Option<Uuid>,
    pub action: Box<str>,
    pub outcome: AuditOutcome,
    pub reason_code: Option<Box<str>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ListAdminAuditEventsResponse {
    pub audit_events: Vec<AdminAuditEventResponse>,
    pub next_cursor: Option<AdminAuditEventId>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    fn uuid(value: &str) -> Uuid {
        Uuid::parse_str(value).unwrap()
    }

    #[test]
    fn password_and_nostr_proofs_have_closed_tagged_shapes() {
        let password: CreateAdminSessionRequest = serde_json::from_value(json!({
            "provider": "password",
            "username": "publisher",
            "password": "correct horse battery staple"
        }))
        .unwrap();
        assert!(matches!(
            password,
            CreateAdminSessionRequest::Password { .. }
        ));

        let challenge_id =
            LoginChallengeId::from_uuid(uuid("11111111-1111-4111-8111-111111111111"));
        let nostr: CreateAdminSessionRequest = serde_json::from_value(json!({
            "provider": "nostr",
            "challenge_id": challenge_id,
            "challenge": "challenge",
            "event": "{}"
        }))
        .unwrap();
        assert!(matches!(nostr, CreateAdminSessionRequest::Nostr { .. }));
        assert!(
            serde_json::from_value::<CreateAdminSessionRequest>(json!({
                "provider": "jwt",
                "token": "no"
            }))
            .is_err()
        );
    }

    #[test]
    fn session_metadata_never_contains_raw_session_or_csrf_tokens() {
        let response = AdminSessionResponse {
            session_id: AdminSessionId::from_uuid(uuid("22222222-2222-4222-8222-222222222222")),
            user_id: UserId::from_uuid(uuid("33333333-3333-4333-8333-333333333333")),
            provider: HumanLoginProvider::Password,
            roles: vec![UserRole::Publisher],
            scopes: AdminScope::PUBLISHER.to_vec(),
            fresh_until: OffsetDateTime::from_unix_timestamp(1_777_500_000).unwrap(),
            expires_at: OffsetDateTime::from_unix_timestamp(1_777_528_800).unwrap(),
        };
        let value = serde_json::to_value(response).unwrap();
        assert!(value.get("session_token").is_none());
        assert!(value.get("csrf_token").is_none());
    }
}
