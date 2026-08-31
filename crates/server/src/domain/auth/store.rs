use std::collections::BTreeSet;

use maincopy_shared::auth::{
    AdminAuditEventId, AdminScope, AdminSessionId, AgentCredentialId, HumanLoginProvider,
    InstanceId, LoginChallengeId, UserId, UserRole, UserStatus, effective_scopes,
};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use time::{Duration, OffsetDateTime, UtcOffset};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::{
    CanonicalUsername, CsrfTokenDigest, LoginChallengeDigest, NIP98_FRESHNESS_SECONDS,
    Nip98EventId, NostrPublicKey, SessionTokenDigest, StoredPasswordHash,
};
use crate::database::store::{DatabaseAdmissionError, Mutation};

const MAX_AGENT_LABEL_BYTES: usize = 96;
const MAX_AUDIT_ACTION_BYTES: usize = 96;
const MAX_AUDIT_REASON_BYTES: usize = 64;
const MAX_LIVE_LOGIN_CHALLENGES: i64 = 4_096;
const MAX_LIVE_BROWSER_SESSIONS: i64 = 4_096;
const MAX_LIVE_REPLAY_EVENTS: i64 = 16_384;
const MAX_LIVE_AGENT_CREDENTIALS_PER_USER: i64 = 128;
const MAX_AUTH_CLEANUP_ROWS: i64 = 128;
const MAX_AUTH_PAGE_ROWS: u16 = 100;

/// Authentication persistence accessed through query-only readers and the sole writer.
#[derive(Clone)]
pub(crate) struct AuthStore {
    readers: SqlitePool,
    mutations: mpsc::Sender<Mutation>,
}

impl AuthStore {
    pub(crate) const fn new(readers: SqlitePool, mutations: mpsc::Sender<Mutation>) -> Self {
        Self { readers, mutations }
    }

    pub(crate) async fn identity_state(&self) -> Result<IdentityState, AuthLoadError> {
        let mut transaction = self.readers.begin().await?;
        let instance = sqlx::query_as::<_, InstanceRow>(
            "SELECT instance_id, version, created_at_ns FROM instance_identity WHERE singleton = 1",
        )
        .fetch_optional(&mut *transaction)
        .await?
        .map(StoredInstanceIdentity::try_from)
        .transpose()?;
        let user_count: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
            .fetch_one(&mut *transaction)
            .await?;
        if (instance.is_some() && user_count == 0) || (instance.is_none() && user_count != 0) {
            return Err(AuthLoadError::PartialBootstrap);
        }
        transaction.commit().await?;
        Ok(IdentityState {
            bootstrap_required: instance.is_none(),
            instance,
        })
    }

    pub(crate) async fn user(&self, user_id: UserId) -> Result<Option<StoredUser>, AuthLoadError> {
        let mut transaction = self.readers.begin().await?;
        let user = load_user(&mut transaction, user_id).await?;
        transaction.commit().await?;
        Ok(user)
    }

    pub(crate) async fn users_page(
        &self,
        after: Option<UserId>,
        limit: u16,
    ) -> Result<AuthPage<StoredUser, UserId>, AuthLoadError> {
        validate_page_limit(limit)?;
        let fetch_limit = i64::from(limit) + 1;
        let mut transaction = self.readers.begin().await?;
        let identifiers: Vec<Vec<u8>> = match after {
            Some(after) => {
                sqlx::query_scalar(
                    "SELECT user_id FROM users WHERE user_id > ? ORDER BY user_id LIMIT ?",
                )
                .bind(after.as_uuid().as_bytes().as_slice())
                .bind(fetch_limit)
                .fetch_all(&mut *transaction)
                .await?
            }
            None => {
                sqlx::query_scalar("SELECT user_id FROM users ORDER BY user_id LIMIT ?")
                    .bind(fetch_limit)
                    .fetch_all(&mut *transaction)
                    .await?
            }
        };
        let has_more = identifiers.len() > usize::from(limit);
        let mut users = Vec::with_capacity(identifiers.len().min(usize::from(limit)));
        for encoded in identifiers.into_iter().take(usize::from(limit)) {
            let identifier = user_id(&encoded)?;
            users.push(
                load_user(&mut transaction, identifier)
                    .await?
                    .ok_or(AuthLoadError::PaginationInvariant)?,
            );
        }
        let next_cursor = has_more.then(|| users.last().expect("nonzero page limit").user_id);
        transaction.commit().await?;
        Ok(AuthPage {
            items: users,
            next_cursor,
        })
    }

    pub(crate) async fn user_credentials(
        &self,
        user_id: UserId,
    ) -> Result<Option<Vec<StoredHumanCredential>>, AuthLoadError> {
        let mut transaction = self.readers.begin().await?;
        if load_user(&mut transaction, user_id).await?.is_none() {
            transaction.commit().await?;
            return Ok(None);
        }
        let password = sqlx::query_as::<_, HumanPasswordCredentialRow>(
            "SELECT canonical_username, version, created_at_ns, updated_at_ns \
             FROM user_password_credentials WHERE user_id = ?",
        )
        .bind(user_id.as_uuid().as_bytes().as_slice())
        .fetch_optional(&mut *transaction)
        .await?;
        let nostr = sqlx::query_as::<_, HumanNostrCredentialRow>(
            "SELECT public_key, version, created_at_ns, updated_at_ns \
             FROM user_nostr_credentials WHERE user_id = ?",
        )
        .bind(user_id.as_uuid().as_bytes().as_slice())
        .fetch_optional(&mut *transaction)
        .await?;
        let mut credentials = Vec::with_capacity(2);
        if let Some(password) = password {
            credentials.push(StoredHumanCredential::Password {
                username: CanonicalUsername::parse(&password.canonical_username)
                    .map_err(|_| AuthLoadError::InvalidUsername)?,
                version: positive_u64(password.version).ok_or(AuthLoadError::InvalidVersion)?,
                created_at: timestamp(password.created_at_ns)?,
                updated_at: timestamp(password.updated_at_ns)?,
            });
        }
        if let Some(nostr) = nostr {
            credentials.push(StoredHumanCredential::Nostr {
                public_key: nostr_public_key(nostr.public_key)?,
                version: positive_u64(nostr.version).ok_or(AuthLoadError::InvalidVersion)?,
                created_at: timestamp(nostr.created_at_ns)?,
                updated_at: timestamp(nostr.updated_at_ns)?,
            });
        }
        transaction.commit().await?;
        Ok(Some(credentials))
    }

    /// Fails startup when the configured provider set would strand an enabled user.
    pub(crate) async fn validate_provider_compatibility(
        &self,
        providers: ConfiguredLoginProviders,
    ) -> Result<(), AuthLoadError> {
        let stranded: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT users.user_id FROM users \
             WHERE users.status = 'enabled' AND NOT (\
                (? AND EXISTS(SELECT 1 FROM user_password_credentials AS password \
                              WHERE password.user_id = users.user_id)) OR \
                (? AND EXISTS(SELECT 1 FROM user_nostr_credentials AS nostr \
                              WHERE nostr.user_id = users.user_id))\
             ) ORDER BY users.user_id LIMIT 1",
        )
        .bind(providers.password)
        .bind(providers.nostr)
        .fetch_optional(&self.readers)
        .await?;
        if let Some(stranded) = stranded {
            return Err(AuthLoadError::EnabledUserWithoutConfiguredCredential {
                user_id: user_id(&stranded)?,
            });
        }
        Ok(())
    }

    pub(crate) async fn password_login(
        &self,
        username: &CanonicalUsername,
    ) -> Result<Option<PasswordLoginRecord>, AuthLoadError> {
        let mut transaction = self.readers.begin().await?;
        let row = sqlx::query_as::<_, PasswordLoginRow>(
            "SELECT password.user_id, password.canonical_username, password.password_phc, \
                    password.policy_version, password.version AS credential_version, \
                    users.status, users.version AS user_version \
             FROM user_password_credentials AS password \
             JOIN users ON users.user_id = password.user_id \
             WHERE password.canonical_username = ?",
        )
        .bind(username.as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        let record = match row {
            Some(row) => {
                let user_id = user_id(&row.user_id)?;
                let roles = load_roles(&mut transaction, user_id).await?;
                Some(PasswordLoginRecord {
                    user_id,
                    username: CanonicalUsername::parse(&row.canonical_username)
                        .map_err(|_| AuthLoadError::InvalidUsername)?,
                    password_hash: StoredPasswordHash::parse(&row.password_phc)
                        .map_err(|_| AuthLoadError::InvalidPasswordHash)?,
                    policy_version: positive_u32(row.policy_version)
                        .ok_or(AuthLoadError::InvalidPasswordPolicyVersion)?,
                    credential_version: positive_u64(row.credential_version)
                        .ok_or(AuthLoadError::InvalidVersion)?,
                    user_status: user_status(&row.status)?,
                    user_version: positive_u64(row.user_version)
                        .ok_or(AuthLoadError::InvalidVersion)?,
                    roles,
                })
            }
            None => None,
        };
        transaction.commit().await?;
        Ok(record)
    }

    pub(crate) async fn nostr_login(
        &self,
        public_key: &NostrPublicKey,
    ) -> Result<Option<NostrLoginRecord>, AuthLoadError> {
        let mut transaction = self.readers.begin().await?;
        let row = sqlx::query_as::<_, NostrLoginRow>(
            "SELECT nostr.user_id, nostr.public_key, nostr.version AS credential_version, \
                    users.status, users.version AS user_version \
             FROM user_nostr_credentials AS nostr \
             JOIN users ON users.user_id = nostr.user_id \
             WHERE nostr.public_key = ?",
        )
        .bind(public_key.as_bytes().as_slice())
        .fetch_optional(&mut *transaction)
        .await?;
        let record = match row {
            Some(row) => {
                let user_id = user_id(&row.user_id)?;
                let roles = load_roles(&mut transaction, user_id).await?;
                Some(NostrLoginRecord {
                    user_id,
                    public_key: nostr_public_key(row.public_key)?,
                    credential_version: positive_u64(row.credential_version)
                        .ok_or(AuthLoadError::InvalidVersion)?,
                    user_status: user_status(&row.status)?,
                    user_version: positive_u64(row.user_version)
                        .ok_or(AuthLoadError::InvalidVersion)?,
                    roles,
                })
            }
            None => None,
        };
        transaction.commit().await?;
        Ok(record)
    }

    pub(crate) async fn browser_session(
        &self,
        token_digest: SessionTokenDigest,
    ) -> Result<Option<StoredBrowserSession>, AuthLoadError> {
        let mut transaction = self.readers.begin().await?;
        let row = sqlx::query_as::<_, BrowserSessionRow>(
            "SELECT session.session_id, session.user_id, session.provider, \
                    session.csrf_token_digest, session.instance_version, session.version, \
                    session.authenticated_at_ns, session.fresh_until_ns, \
                    session.expires_at_ns, session.revoked_at_ns, session.last_seen_at_ns, \
                    users.status, users.version AS user_version, \
                    instance.version AS current_instance_version \
             FROM browser_sessions AS session \
             JOIN users ON users.user_id = session.user_id \
             JOIN instance_identity AS instance ON instance.singleton = 1 \
             WHERE session.session_token_digest = ?",
        )
        .bind(token_digest.as_bytes().as_slice())
        .fetch_optional(&mut *transaction)
        .await?;
        let record = match row {
            Some(row) => {
                let user_id = user_id(&row.user_id)?;
                let roles = load_roles(&mut transaction, user_id).await?;
                Some(row.try_into_record(roles)?)
            }
            None => None,
        };
        transaction.commit().await?;
        Ok(record)
    }

    pub(crate) async fn login_challenge(
        &self,
        challenge_id: LoginChallengeId,
    ) -> Result<Option<StoredLoginChallenge>, AuthLoadError> {
        sqlx::query_as::<_, LoginChallengeRow>(
            "SELECT challenge_id, provider, challenge_digest, created_at_ns, expires_at_ns, \
                    consumed_at_ns \
             FROM login_challenges WHERE challenge_id = ?",
        )
        .bind(challenge_id.as_uuid().as_bytes().as_slice())
        .fetch_optional(&self.readers)
        .await?
        .map(StoredLoginChallenge::try_from)
        .transpose()
    }

    pub(crate) async fn agent_credential(
        &self,
        public_key: &NostrPublicKey,
    ) -> Result<Option<StoredAgentCredential>, AuthLoadError> {
        let mut transaction = self.readers.begin().await?;
        let row = sqlx::query_as::<_, AgentCredentialRow>(
            "SELECT agent.agent_credential_id, agent.owner_user_id, agent.issuer_user_id, \
                    agent.public_key, agent.label, agent.version, agent.created_at_ns, \
                    agent.expires_at_ns, agent.last_used_at_ns, agent.revoked_at_ns, \
                    users.status AS owner_status, users.version AS owner_version \
             FROM agent_credentials AS agent \
             JOIN users ON users.user_id = agent.owner_user_id \
             WHERE agent.public_key = ?",
        )
        .bind(public_key.as_bytes().as_slice())
        .fetch_optional(&mut *transaction)
        .await?;
        let record = match row {
            Some(row) => {
                let credential_id = agent_credential_id(&row.agent_credential_id)?;
                let owner_id = user_id(&row.owner_user_id)?;
                let roles = load_roles(&mut transaction, owner_id).await?;
                let scopes = load_agent_scopes(&mut transaction, credential_id).await?;
                Some(row.try_into_record(roles, scopes)?)
            }
            None => None,
        };
        transaction.commit().await?;
        Ok(record)
    }

    pub(crate) async fn agent_credential_by_id(
        &self,
        credential_id: AgentCredentialId,
    ) -> Result<Option<StoredAgentCredential>, AuthLoadError> {
        let mut transaction = self.readers.begin().await?;
        let row = sqlx::query_as::<_, AgentCredentialRow>(
            "SELECT agent.agent_credential_id, agent.owner_user_id, agent.issuer_user_id, \
                    agent.public_key, agent.label, agent.version, agent.created_at_ns, \
                    agent.expires_at_ns, agent.last_used_at_ns, agent.revoked_at_ns, \
                    users.status AS owner_status, users.version AS owner_version \
             FROM agent_credentials AS agent \
             JOIN users ON users.user_id = agent.owner_user_id \
             WHERE agent.agent_credential_id = ?",
        )
        .bind(credential_id.as_uuid().as_bytes().as_slice())
        .fetch_optional(&mut *transaction)
        .await?;
        let record = match row {
            Some(row) => {
                let owner_id = user_id(&row.owner_user_id)?;
                let roles = load_roles(&mut transaction, owner_id).await?;
                let scopes = load_agent_scopes(&mut transaction, credential_id).await?;
                Some(row.try_into_record(roles, scopes)?)
            }
            None => None,
        };
        transaction.commit().await?;
        Ok(record)
    }

    pub(crate) async fn agent_credentials_page(
        &self,
        after: Option<AgentCredentialId>,
        limit: u16,
    ) -> Result<AuthPage<StoredAgentCredential, AgentCredentialId>, AuthLoadError> {
        validate_page_limit(limit)?;
        let fetch_limit = i64::from(limit) + 1;
        let mut transaction = self.readers.begin().await?;
        let identifiers: Vec<Vec<u8>> = match after {
            Some(after) => {
                sqlx::query_scalar(
                    "SELECT agent_credential_id FROM agent_credentials \
                 WHERE agent_credential_id > ? ORDER BY agent_credential_id LIMIT ?",
                )
                .bind(after.as_uuid().as_bytes().as_slice())
                .bind(fetch_limit)
                .fetch_all(&mut *transaction)
                .await?
            }
            None => {
                sqlx::query_scalar(
                    "SELECT agent_credential_id FROM agent_credentials \
                 ORDER BY agent_credential_id LIMIT ?",
                )
                .bind(fetch_limit)
                .fetch_all(&mut *transaction)
                .await?
            }
        };
        let has_more = identifiers.len() > usize::from(limit);
        let mut credentials = Vec::with_capacity(identifiers.len().min(usize::from(limit)));
        for encoded in identifiers.into_iter().take(usize::from(limit)) {
            let credential_id = agent_credential_id(&encoded)?;
            let row = sqlx::query_as::<_, AgentCredentialRow>(
                "SELECT agent.agent_credential_id, agent.owner_user_id, agent.issuer_user_id, \
                        agent.public_key, agent.label, agent.version, agent.created_at_ns, \
                        agent.expires_at_ns, agent.last_used_at_ns, agent.revoked_at_ns, \
                        users.status AS owner_status, users.version AS owner_version \
                 FROM agent_credentials AS agent \
                 JOIN users ON users.user_id = agent.owner_user_id \
                 WHERE agent.agent_credential_id = ?",
            )
            .bind(credential_id.as_uuid().as_bytes().as_slice())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AuthLoadError::PaginationInvariant)?;
            let owner_id = user_id(&row.owner_user_id)?;
            let roles = load_roles(&mut transaction, owner_id).await?;
            let scopes = load_agent_scopes(&mut transaction, credential_id).await?;
            credentials.push(row.try_into_record(roles, scopes)?);
        }
        let next_cursor = has_more.then(|| {
            credentials
                .last()
                .expect("nonzero page limit")
                .credential_id
        });
        transaction.commit().await?;
        Ok(AuthPage {
            items: credentials,
            next_cursor,
        })
    }

    pub(crate) async fn audit_events_page(
        &self,
        after: Option<AdminAuditEventId>,
        limit: u16,
    ) -> Result<AuthPage<StoredAdminAuditEvent, AdminAuditEventId>, AuthLoadError> {
        validate_page_limit(limit)?;
        let fetch_limit = i64::from(limit) + 1;
        let rows = match after {
            Some(after) => {
                let occurred_at: i64 = sqlx::query_scalar(
                    "SELECT occurred_at_ns FROM admin_audit_events WHERE audit_event_id = ?",
                )
                .bind(after.as_uuid().as_bytes().as_slice())
                .fetch_optional(&self.readers)
                .await?
                .ok_or(AuthLoadError::CursorNotFound)?;
                sqlx::query_as::<_, AdminAuditEventRow>(
                    "SELECT audit_event_id, occurred_at_ns, principal_kind, actor_user_id, \
                            session_id, agent_credential_id, request_id, idempotency_key, \
                            action, outcome, reason_code \
                     FROM admin_audit_events \
                     WHERE occurred_at_ns < ? OR (occurred_at_ns = ? AND audit_event_id < ?) \
                     ORDER BY occurred_at_ns DESC, audit_event_id DESC LIMIT ?",
                )
                .bind(occurred_at)
                .bind(occurred_at)
                .bind(after.as_uuid().as_bytes().as_slice())
                .bind(fetch_limit)
                .fetch_all(&self.readers)
                .await?
            }
            None => {
                sqlx::query_as::<_, AdminAuditEventRow>(
                    "SELECT audit_event_id, occurred_at_ns, principal_kind, actor_user_id, \
                        session_id, agent_credential_id, request_id, idempotency_key, \
                        action, outcome, reason_code \
                 FROM admin_audit_events \
                 ORDER BY occurred_at_ns DESC, audit_event_id DESC LIMIT ?",
                )
                .bind(fetch_limit)
                .fetch_all(&self.readers)
                .await?
            }
        };
        let has_more = rows.len() > usize::from(limit);
        let events = rows
            .into_iter()
            .take(usize::from(limit))
            .map(StoredAdminAuditEvent::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor =
            has_more.then(|| events.last().expect("nonzero page limit").audit_event_id);
        Ok(AuthPage {
            items: events,
            next_cursor,
        })
    }

    pub(crate) async fn record_admin_audit_failure(
        &self,
        command: RecordAdminAuditFailure,
    ) -> Result<(), AuthMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::RecordAdminAuditFailure {
            command,
            respond_to,
        })?;
        receive_auth_mutation(response).await
    }

    pub(crate) async fn bootstrap_identity(
        &self,
        command: BootstrapIdentity,
    ) -> Result<BootstrapIdentityResult, AuthMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::BootstrapIdentity {
            command,
            respond_to,
        })?;
        receive_auth_mutation(response).await
    }

    pub(crate) async fn create_user(
        &self,
        command: CreateUser,
    ) -> Result<UserMutationResult, AuthMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::CreateUser {
            command,
            respond_to,
        })?;
        receive_auth_mutation(response).await
    }

    pub(crate) async fn set_user_status(
        &self,
        command: SetUserStatus,
    ) -> Result<UserMutationResult, AuthMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::SetUserStatus {
            command,
            respond_to,
        })?;
        receive_auth_mutation(response).await
    }

    pub(crate) async fn replace_user_roles(
        &self,
        command: ReplaceUserRoles,
    ) -> Result<UserMutationResult, AuthMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::ReplaceUserRoles {
            command,
            respond_to,
        })?;
        receive_auth_mutation(response).await
    }

    pub(crate) async fn put_human_credential(
        &self,
        command: PutHumanCredential,
    ) -> Result<UserMutationResult, AuthMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::PutHumanCredential {
            command,
            respond_to,
        })?;
        receive_auth_mutation(response).await
    }

    pub(crate) async fn remove_human_credential(
        &self,
        command: RemoveHumanCredential,
    ) -> Result<UserMutationResult, AuthMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::RemoveHumanCredential {
            command,
            respond_to,
        })?;
        receive_auth_mutation(response).await
    }

    pub(crate) async fn create_login_challenge(
        &self,
        command: CreateLoginChallenge,
    ) -> Result<StoredLoginChallenge, AuthMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::CreateLoginChallenge {
            command,
            respond_to,
        })?;
        receive_auth_mutation(response).await
    }

    pub(crate) async fn create_browser_session(
        &self,
        command: CreateBrowserSession,
    ) -> Result<BrowserSessionMutationResult, AuthMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::CreateBrowserSession {
            command,
            respond_to,
        })?;
        receive_auth_mutation(response).await
    }

    pub(crate) async fn revoke_browser_session(
        &self,
        command: RevokeBrowserSession,
    ) -> Result<BrowserSessionMutationResult, AuthMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::RevokeBrowserSession {
            command,
            respond_to,
        })?;
        receive_auth_mutation(response).await
    }

    pub(crate) async fn register_agent_credential(
        &self,
        command: RegisterAgentCredential,
    ) -> Result<AgentCredentialMutationResult, AuthMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::RegisterAgentCredential {
            command,
            respond_to,
        })?;
        receive_auth_mutation(response).await
    }

    pub(crate) async fn replace_agent_scopes(
        &self,
        command: ReplaceAgentScopes,
    ) -> Result<AgentCredentialMutationResult, AuthMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::ReplaceAgentScopes {
            command,
            respond_to,
        })?;
        receive_auth_mutation(response).await
    }

    pub(crate) async fn revoke_agent_credential(
        &self,
        command: RevokeAgentCredential,
    ) -> Result<AgentCredentialMutationResult, AuthMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::RevokeAgentCredential {
            command,
            respond_to,
        })?;
        receive_auth_mutation(response).await
    }

    pub(crate) async fn accept_agent_proof(
        &self,
        command: AcceptAgentProof,
    ) -> Result<AgentCredentialMutationResult, AuthMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::AcceptAgentProof {
            command,
            respond_to,
        })?;
        receive_auth_mutation(response).await
    }

    fn admit(&self, mutation: Mutation) -> Result<(), AuthMutationError> {
        self.mutations
            .try_send(mutation)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => DatabaseAdmissionError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => DatabaseAdmissionError::WriterClosed,
            })?;
        Ok(())
    }
}

async fn receive_auth_mutation<Output>(
    response: oneshot::Receiver<Result<Output, AuthCommandError>>,
) -> Result<Output, AuthMutationError> {
    response
        .await
        .map_err(|_| AuthMutationError::Command(AuthCommandError::OutcomeUnknown))?
        .map_err(AuthMutationError::Command)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IdentityState {
    pub(crate) instance: Option<StoredInstanceIdentity>,
    pub(crate) bootstrap_required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredInstanceIdentity {
    pub(crate) instance_id: InstanceId,
    pub(crate) version: u64,
    pub(crate) created_at: OffsetDateTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredUser {
    pub(crate) user_id: UserId,
    pub(crate) status: UserStatus,
    pub(crate) version: u64,
    pub(crate) roles: BTreeSet<UserRole>,
    pub(crate) has_password: bool,
    pub(crate) has_nostr: bool,
    pub(crate) created_at: OffsetDateTime,
    pub(crate) updated_at: OffsetDateTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StoredHumanCredential {
    Password {
        username: CanonicalUsername,
        version: u64,
        created_at: OffsetDateTime,
        updated_at: OffsetDateTime,
    },
    Nostr {
        public_key: NostrPublicKey,
        version: u64,
        created_at: OffsetDateTime,
        updated_at: OffsetDateTime,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthPage<Item, Cursor> {
    pub(crate) items: Vec<Item>,
    pub(crate) next_cursor: Option<Cursor>,
}

impl StoredUser {
    pub(crate) fn scopes(&self) -> BTreeSet<AdminScope> {
        effective_scopes(self.roles.iter().copied())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct PasswordLoginRecord {
    pub(crate) user_id: UserId,
    pub(crate) username: CanonicalUsername,
    pub(crate) password_hash: StoredPasswordHash,
    pub(crate) policy_version: u32,
    pub(crate) credential_version: u64,
    pub(crate) user_status: UserStatus,
    pub(crate) user_version: u64,
    pub(crate) roles: BTreeSet<UserRole>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NostrLoginRecord {
    pub(crate) user_id: UserId,
    pub(crate) public_key: NostrPublicKey,
    pub(crate) credential_version: u64,
    pub(crate) user_status: UserStatus,
    pub(crate) user_version: u64,
    pub(crate) roles: BTreeSet<UserRole>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredBrowserSession {
    pub(crate) session_id: AdminSessionId,
    pub(crate) user_id: UserId,
    pub(crate) provider: HumanLoginProvider,
    pub(crate) csrf_token_digest: CsrfTokenDigest,
    pub(crate) instance_version: u64,
    pub(crate) current_instance_version: u64,
    pub(crate) version: u64,
    pub(crate) authenticated_at: OffsetDateTime,
    pub(crate) fresh_until: OffsetDateTime,
    pub(crate) expires_at: OffsetDateTime,
    pub(crate) revoked_at: Option<OffsetDateTime>,
    pub(crate) last_seen_at: OffsetDateTime,
    pub(crate) user_status: UserStatus,
    pub(crate) user_version: u64,
    pub(crate) roles: BTreeSet<UserRole>,
}

impl StoredBrowserSession {
    pub(crate) fn is_active_at(&self, now: OffsetDateTime) -> bool {
        self.user_status == UserStatus::Enabled
            && self.revoked_at.is_none()
            && now < self.expires_at
            && self.instance_version == self.current_instance_version
    }

    pub(crate) fn scopes(&self) -> BTreeSet<AdminScope> {
        effective_scopes(self.roles.iter().copied())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredLoginChallenge {
    pub(crate) challenge_id: LoginChallengeId,
    pub(crate) provider: HumanLoginProvider,
    pub(crate) challenge_digest: LoginChallengeDigest,
    pub(crate) created_at: OffsetDateTime,
    pub(crate) expires_at: OffsetDateTime,
    pub(crate) consumed_at: Option<OffsetDateTime>,
}

impl StoredLoginChallenge {
    pub(crate) fn is_usable_at(&self, now: OffsetDateTime) -> bool {
        self.consumed_at.is_none() && now < self.expires_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredAgentCredential {
    pub(crate) credential_id: AgentCredentialId,
    pub(crate) owner_user_id: UserId,
    pub(crate) issuer_user_id: UserId,
    pub(crate) public_key: NostrPublicKey,
    pub(crate) label: Box<str>,
    pub(crate) scopes: BTreeSet<AdminScope>,
    pub(crate) owner_roles: BTreeSet<UserRole>,
    pub(crate) owner_status: UserStatus,
    pub(crate) owner_version: u64,
    pub(crate) version: u64,
    pub(crate) created_at: OffsetDateTime,
    pub(crate) expires_at: Option<OffsetDateTime>,
    pub(crate) last_used_at: Option<OffsetDateTime>,
    pub(crate) revoked_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredAdminAuditEvent {
    pub(crate) audit_event_id: AdminAuditEventId,
    pub(crate) occurred_at: OffsetDateTime,
    pub(crate) principal: AuditPrincipalReference,
    pub(crate) request_id: Option<Uuid>,
    pub(crate) idempotency_key: Option<Uuid>,
    pub(crate) action: Box<str>,
    pub(crate) outcome: AdminAuditOutcome,
    pub(crate) reason_code: Option<Box<str>>,
}

impl StoredAgentCredential {
    pub(crate) fn effective_scopes(&self) -> BTreeSet<AdminScope> {
        let owner = effective_scopes(self.owner_roles.iter().copied());
        self.scopes.intersection(&owner).copied().collect()
    }

    pub(crate) fn is_active_at(&self, now: OffsetDateTime) -> bool {
        self.owner_status == UserStatus::Enabled
            && self.revoked_at.is_none()
            && self.expires_at.is_none_or(|expires_at| now < expires_at)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConfiguredLoginProviders {
    password: bool,
    nostr: bool,
}

impl ConfiguredLoginProviders {
    pub(crate) fn new(password: bool, nostr: bool) -> Result<Self, AuthCommandError> {
        if !password && !nostr {
            return Err(AuthCommandError::NoLoginProvider);
        }
        Ok(Self { password, nostr })
    }

    pub(crate) const fn accepts(self, provider: HumanLoginProvider) -> bool {
        match provider {
            HumanLoginProvider::Password => self.password,
            HumanLoginProvider::Nostr => self.nostr,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum NewHumanCredential {
    Password {
        username: CanonicalUsername,
        password_hash: StoredPasswordHash,
        policy_version: u32,
    },
    Nostr {
        public_key: NostrPublicKey,
    },
}

impl NewHumanCredential {
    const fn provider(&self) -> HumanLoginProvider {
        match self {
            Self::Password { .. } => HumanLoginProvider::Password,
            Self::Nostr { .. } => HumanLoginProvider::Nostr,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HumanCredentialKind {
    Password,
    Nostr,
}

impl HumanCredentialKind {
    const fn provider(self) -> HumanLoginProvider {
        match self {
            Self::Password => HumanLoginProvider::Password,
            Self::Nostr => HumanLoginProvider::Nostr,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, FromRow)]
struct InstanceRow {
    instance_id: Vec<u8>,
    version: i64,
    created_at_ns: i64,
}

impl TryFrom<InstanceRow> for StoredInstanceIdentity {
    type Error = AuthLoadError;

    fn try_from(row: InstanceRow) -> Result<Self, Self::Error> {
        Ok(Self {
            instance_id: instance_id(&row.instance_id)?,
            version: positive_u64(row.version).ok_or(AuthLoadError::InvalidVersion)?,
            created_at: timestamp(row.created_at_ns)?,
        })
    }
}

#[derive(FromRow)]
struct UserRow {
    user_id: Vec<u8>,
    status: String,
    version: i64,
    created_at_ns: i64,
    updated_at_ns: i64,
    has_password: i64,
    has_nostr: i64,
}

#[derive(FromRow)]
struct HumanPasswordCredentialRow {
    canonical_username: String,
    version: i64,
    created_at_ns: i64,
    updated_at_ns: i64,
}

#[derive(FromRow)]
struct HumanNostrCredentialRow {
    public_key: Vec<u8>,
    version: i64,
    created_at_ns: i64,
    updated_at_ns: i64,
}

#[derive(FromRow)]
struct PasswordLoginRow {
    user_id: Vec<u8>,
    canonical_username: String,
    password_phc: String,
    policy_version: i64,
    credential_version: i64,
    status: String,
    user_version: i64,
}

#[derive(FromRow)]
struct NostrLoginRow {
    user_id: Vec<u8>,
    public_key: Vec<u8>,
    credential_version: i64,
    status: String,
    user_version: i64,
}

#[derive(FromRow)]
struct BrowserSessionRow {
    session_id: Vec<u8>,
    user_id: Vec<u8>,
    provider: String,
    csrf_token_digest: Vec<u8>,
    instance_version: i64,
    current_instance_version: i64,
    version: i64,
    authenticated_at_ns: i64,
    fresh_until_ns: i64,
    expires_at_ns: i64,
    revoked_at_ns: Option<i64>,
    last_seen_at_ns: i64,
    status: String,
    user_version: i64,
}

impl BrowserSessionRow {
    fn try_into_record(
        self,
        roles: BTreeSet<UserRole>,
    ) -> Result<StoredBrowserSession, AuthLoadError> {
        Ok(StoredBrowserSession {
            session_id: admin_session_id(&self.session_id)?,
            user_id: user_id(&self.user_id)?,
            provider: login_provider(&self.provider)?,
            csrf_token_digest: CsrfTokenDigest::parse_bytes(&self.csrf_token_digest)
                .map_err(|_| AuthLoadError::InvalidDigest)?,
            instance_version: positive_u64(self.instance_version)
                .ok_or(AuthLoadError::InvalidVersion)?,
            current_instance_version: positive_u64(self.current_instance_version)
                .ok_or(AuthLoadError::InvalidVersion)?,
            version: positive_u64(self.version).ok_or(AuthLoadError::InvalidVersion)?,
            authenticated_at: timestamp(self.authenticated_at_ns)?,
            fresh_until: timestamp(self.fresh_until_ns)?,
            expires_at: timestamp(self.expires_at_ns)?,
            revoked_at: self.revoked_at_ns.map(timestamp).transpose()?,
            last_seen_at: timestamp(self.last_seen_at_ns)?,
            user_status: user_status(&self.status)?,
            user_version: positive_u64(self.user_version).ok_or(AuthLoadError::InvalidVersion)?,
            roles,
        })
    }
}

#[derive(FromRow)]
struct LoginChallengeRow {
    challenge_id: Vec<u8>,
    provider: String,
    challenge_digest: Vec<u8>,
    created_at_ns: i64,
    expires_at_ns: i64,
    consumed_at_ns: Option<i64>,
}

impl TryFrom<LoginChallengeRow> for StoredLoginChallenge {
    type Error = AuthLoadError;

    fn try_from(row: LoginChallengeRow) -> Result<Self, Self::Error> {
        Ok(Self {
            challenge_id: login_challenge_id(&row.challenge_id)?,
            provider: login_provider(&row.provider)?,
            challenge_digest: LoginChallengeDigest::parse_bytes(&row.challenge_digest)
                .map_err(|_| AuthLoadError::InvalidDigest)?,
            created_at: timestamp(row.created_at_ns)?,
            expires_at: timestamp(row.expires_at_ns)?,
            consumed_at: row.consumed_at_ns.map(timestamp).transpose()?,
        })
    }
}

#[derive(FromRow)]
struct AgentCredentialRow {
    agent_credential_id: Vec<u8>,
    owner_user_id: Vec<u8>,
    issuer_user_id: Vec<u8>,
    public_key: Vec<u8>,
    label: String,
    version: i64,
    created_at_ns: i64,
    expires_at_ns: Option<i64>,
    last_used_at_ns: Option<i64>,
    revoked_at_ns: Option<i64>,
    owner_status: String,
    owner_version: i64,
}

#[derive(FromRow)]
struct AdminAuditEventRow {
    audit_event_id: Vec<u8>,
    occurred_at_ns: i64,
    principal_kind: String,
    actor_user_id: Option<Vec<u8>>,
    session_id: Option<Vec<u8>>,
    agent_credential_id: Option<Vec<u8>>,
    request_id: Option<Vec<u8>>,
    idempotency_key: Option<Vec<u8>>,
    action: String,
    outcome: String,
    reason_code: Option<String>,
}

#[derive(FromRow)]
struct IdentityMutationReceiptRow {
    command_fingerprint: Vec<u8>,
    result_kind: String,
    result_id: Vec<u8>,
    result_version: i64,
    principal_kind: String,
    actor_user_id: Option<Vec<u8>>,
    session_id: Option<Vec<u8>>,
    agent_credential_id: Option<Vec<u8>>,
    action: String,
}

impl TryFrom<AdminAuditEventRow> for StoredAdminAuditEvent {
    type Error = AuthLoadError;

    fn try_from(row: AdminAuditEventRow) -> Result<Self, Self::Error> {
        let actor = row.actor_user_id.as_deref().map(user_id).transpose()?;
        let session = row
            .session_id
            .as_deref()
            .map(admin_session_id)
            .transpose()?;
        let agent = row
            .agent_credential_id
            .as_deref()
            .map(agent_credential_id)
            .transpose()?;
        let principal = audit_principal(&row.principal_kind, actor, session, agent)?;
        if row.action.is_empty()
            || row.action.len() > MAX_AUDIT_ACTION_BYTES
            || row.action.chars().any(char::is_control)
            || row.reason_code.as_ref().is_some_and(|reason| {
                reason.is_empty()
                    || reason.len() > MAX_AUDIT_REASON_BYTES
                    || reason.chars().any(char::is_control)
            })
        {
            return Err(AuthLoadError::InvalidAuditText);
        }
        let outcome = match row.outcome.as_str() {
            "succeeded" => AdminAuditOutcome::Succeeded,
            "denied" => AdminAuditOutcome::Denied,
            "failed" => AdminAuditOutcome::Failed,
            _ => return Err(AuthLoadError::InvalidAuditOutcome),
        };
        Ok(Self {
            audit_event_id: admin_audit_event_id(&row.audit_event_id)?,
            occurred_at: timestamp(row.occurred_at_ns)?,
            principal,
            request_id: row.request_id.as_deref().map(uuid).transpose()?,
            idempotency_key: row.idempotency_key.as_deref().map(uuid).transpose()?,
            action: row.action.into_boxed_str(),
            outcome,
            reason_code: row.reason_code.map(String::into_boxed_str),
        })
    }
}

fn audit_principal(
    kind: &str,
    actor: Option<UserId>,
    session: Option<AdminSessionId>,
    agent: Option<AgentCredentialId>,
) -> Result<AuditPrincipalReference, AuthLoadError> {
    match (kind, actor, session, agent) {
        ("browser_session", Some(user_id), Some(session_id), None) => {
            Ok(AuditPrincipalReference::BrowserSession {
                user_id,
                session_id,
            })
        }
        ("agent_credential", Some(user_id), None, Some(credential_id)) => {
            Ok(AuditPrincipalReference::AgentCredential {
                user_id,
                credential_id,
            })
        }
        ("offline", user_id, None, None) => Ok(AuditPrincipalReference::Offline { user_id }),
        ("unauthenticated", None, None, None) => Ok(AuditPrincipalReference::Unauthenticated),
        _ => Err(AuthLoadError::InvalidAuditPrincipal),
    }
}

impl AgentCredentialRow {
    fn try_into_record(
        self,
        owner_roles: BTreeSet<UserRole>,
        scopes: BTreeSet<AdminScope>,
    ) -> Result<StoredAgentCredential, AuthLoadError> {
        if self.label.is_empty() || self.label.len() > MAX_AGENT_LABEL_BYTES {
            return Err(AuthLoadError::InvalidAgentLabel);
        }
        Ok(StoredAgentCredential {
            credential_id: agent_credential_id(&self.agent_credential_id)?,
            owner_user_id: user_id(&self.owner_user_id)?,
            issuer_user_id: user_id(&self.issuer_user_id)?,
            public_key: nostr_public_key(self.public_key)?,
            label: self.label.into_boxed_str(),
            scopes,
            owner_roles,
            owner_status: user_status(&self.owner_status)?,
            owner_version: positive_u64(self.owner_version).ok_or(AuthLoadError::InvalidVersion)?,
            version: positive_u64(self.version).ok_or(AuthLoadError::InvalidVersion)?,
            created_at: timestamp(self.created_at_ns)?,
            expires_at: self.expires_at_ns.map(timestamp).transpose()?,
            last_used_at: self.last_used_at_ns.map(timestamp).transpose()?,
            revoked_at: self.revoked_at_ns.map(timestamp).transpose()?,
        })
    }
}

async fn load_user(
    transaction: &mut Transaction<'_, Sqlite>,
    user: UserId,
) -> Result<Option<StoredUser>, AuthLoadError> {
    let row = sqlx::query_as::<_, UserRow>(
        "SELECT users.user_id, users.status, users.version, users.created_at_ns, \
                users.updated_at_ns, \
                EXISTS(SELECT 1 FROM user_password_credentials AS password \
                       WHERE password.user_id = users.user_id) AS has_password, \
                EXISTS(SELECT 1 FROM user_nostr_credentials AS nostr \
                       WHERE nostr.user_id = users.user_id) AS has_nostr \
         FROM users WHERE users.user_id = ?",
    )
    .bind(user.as_uuid().as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let user_id = user_id(&row.user_id)?;
    let roles = load_roles(transaction, user_id).await?;
    if roles.is_empty() {
        return Err(AuthLoadError::UserWithoutRole);
    }
    Ok(Some(StoredUser {
        user_id,
        status: user_status(&row.status)?,
        version: positive_u64(row.version).ok_or(AuthLoadError::InvalidVersion)?,
        roles,
        has_password: boolean(row.has_password)?,
        has_nostr: boolean(row.has_nostr)?,
        created_at: timestamp(row.created_at_ns)?,
        updated_at: timestamp(row.updated_at_ns)?,
    }))
}

async fn load_roles(
    transaction: &mut Transaction<'_, Sqlite>,
    user_id: UserId,
) -> Result<BTreeSet<UserRole>, AuthLoadError> {
    let values: Vec<String> =
        sqlx::query_scalar("SELECT role FROM user_roles WHERE user_id = ? ORDER BY role")
            .bind(user_id.as_uuid().as_bytes().as_slice())
            .fetch_all(&mut **transaction)
            .await?;
    values
        .into_iter()
        .map(|value| UserRole::parse(&value).ok_or(AuthLoadError::InvalidRole))
        .collect()
}

async fn load_agent_scopes(
    transaction: &mut Transaction<'_, Sqlite>,
    credential_id: AgentCredentialId,
) -> Result<BTreeSet<AdminScope>, AuthLoadError> {
    let values: Vec<String> = sqlx::query_scalar(
        "SELECT scope FROM agent_credential_scopes WHERE agent_credential_id = ? ORDER BY scope",
    )
    .bind(credential_id.as_uuid().as_bytes().as_slice())
    .fetch_all(&mut **transaction)
    .await?;
    values
        .into_iter()
        .map(|value| AdminScope::parse(&value).ok_or(AuthLoadError::InvalidScope))
        .collect()
}

fn instance_id(value: &[u8]) -> Result<InstanceId, AuthLoadError> {
    uuid(value).map(InstanceId::from_uuid)
}

fn user_id(value: &[u8]) -> Result<UserId, AuthLoadError> {
    uuid(value).map(UserId::from_uuid)
}

fn admin_session_id(value: &[u8]) -> Result<AdminSessionId, AuthLoadError> {
    uuid(value).map(AdminSessionId::from_uuid)
}

fn login_challenge_id(value: &[u8]) -> Result<LoginChallengeId, AuthLoadError> {
    uuid(value).map(LoginChallengeId::from_uuid)
}

fn agent_credential_id(value: &[u8]) -> Result<AgentCredentialId, AuthLoadError> {
    uuid(value).map(AgentCredentialId::from_uuid)
}

fn admin_audit_event_id(value: &[u8]) -> Result<AdminAuditEventId, AuthLoadError> {
    uuid(value).map(AdminAuditEventId::from_uuid)
}

fn uuid(value: &[u8]) -> Result<Uuid, AuthLoadError> {
    Uuid::from_slice(value).map_err(|_| AuthLoadError::InvalidIdentifier)
}

fn nostr_public_key(value: Vec<u8>) -> Result<NostrPublicKey, AuthLoadError> {
    value
        .try_into()
        .map_err(|_| AuthLoadError::InvalidNostrPublicKey)
        .and_then(|bytes| {
            NostrPublicKey::from_bytes(bytes).map_err(|_| AuthLoadError::InvalidNostrPublicKey)
        })
}

fn user_status(value: &str) -> Result<UserStatus, AuthLoadError> {
    UserStatus::parse(value).ok_or(AuthLoadError::InvalidUserStatus)
}

fn login_provider(value: &str) -> Result<HumanLoginProvider, AuthLoadError> {
    HumanLoginProvider::parse(value).ok_or(AuthLoadError::InvalidLoginProvider)
}

fn boolean(value: i64) -> Result<bool, AuthLoadError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(AuthLoadError::InvalidBoolean),
    }
}

fn positive_u64(value: i64) -> Option<u64> {
    u64::try_from(value).ok().filter(|value| *value > 0)
}

fn positive_u32(value: i64) -> Option<u32> {
    u32::try_from(value).ok().filter(|value| *value > 0)
}

fn validate_page_limit(limit: u16) -> Result<(), AuthLoadError> {
    if (1..=MAX_AUTH_PAGE_ROWS).contains(&limit) {
        Ok(())
    } else {
        Err(AuthLoadError::InvalidPageLimit)
    }
}

fn timestamp(value: i64) -> Result<OffsetDateTime, AuthLoadError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(value))
        .map(|timestamp| timestamp.to_offset(UtcOffset::UTC))
        .map_err(|_| AuthLoadError::InvalidTimestamp)
}

#[derive(Debug, Error)]
pub(crate) enum AuthLoadError {
    #[error("could not query authentication state")]
    Query(#[from] sqlx::Error),
    #[error("identity bootstrap is only partially persisted")]
    PartialBootstrap,
    #[error("an authentication identifier is invalid")]
    InvalidIdentifier,
    #[error("an authentication digest is invalid")]
    InvalidDigest,
    #[error("an authentication resource version is invalid")]
    InvalidVersion,
    #[error("an authentication timestamp is invalid")]
    InvalidTimestamp,
    #[error("a stored boolean is invalid")]
    InvalidBoolean,
    #[error("a stored user status is invalid")]
    InvalidUserStatus,
    #[error("a stored role is invalid")]
    InvalidRole,
    #[error("a stored scope is invalid")]
    InvalidScope,
    #[error("a stored login provider is invalid")]
    InvalidLoginProvider,
    #[error("a stored username is invalid")]
    InvalidUsername,
    #[error("a stored password hash is invalid")]
    InvalidPasswordHash,
    #[error("a stored password policy version is invalid")]
    InvalidPasswordPolicyVersion,
    #[error("a stored Nostr public key is invalid")]
    InvalidNostrPublicKey,
    #[error("an enabled user has no role")]
    UserWithoutRole,
    #[error("enabled user {user_id} has no credential accepted by the configured login providers")]
    EnabledUserWithoutConfiguredCredential { user_id: UserId },
    #[error("a stored agent label is invalid")]
    InvalidAgentLabel,
    #[error("the authentication page limit is invalid")]
    InvalidPageLimit,
    #[error("the authentication pagination cursor does not exist")]
    CursorNotFound,
    #[error("authentication pagination observed inconsistent state")]
    PaginationInvariant,
    #[error("a stored audit principal is invalid")]
    InvalidAuditPrincipal,
    #[error("a stored audit outcome is invalid")]
    InvalidAuditOutcome,
    #[error("stored audit text is invalid")]
    InvalidAuditText,
}

#[derive(Debug, Error)]
pub(crate) enum AuthMutationError {
    #[error(transparent)]
    Admission(#[from] DatabaseAdmissionError),
    #[error(transparent)]
    Command(#[from] AuthCommandError),
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum AuthCommandError {
    #[error("identity bootstrap is already complete")]
    AlreadyBootstrapped,
    #[error("identity bootstrap must complete first")]
    BootstrapRequired,
    #[error("the authentication resource does not exist")]
    NotFound,
    #[error("an authentication identifier or credential is already in use")]
    Conflict,
    #[error("the authentication resource version is stale")]
    StaleVersion,
    #[error("at least one human login provider must be enabled")]
    NoLoginProvider,
    #[error("an enabled user requires a credential accepted by an enabled provider")]
    EnabledUserRequiresCredential,
    #[error("the last enabled owner cannot be disabled or demoted")]
    LastEnabledOwner,
    #[error("an agent scope exceeds the issuer or owner authority")]
    ScopeEscalation,
    #[error("the login challenge is invalid, expired, or already consumed")]
    InvalidChallenge,
    #[error("the durable live-login-challenge limit has been reached")]
    ChallengeCapacity,
    #[error("the durable live-browser-session limit has been reached")]
    SessionCapacity,
    #[error("the durable live-NIP-98-replay-event limit has been reached")]
    ReplayCapacity,
    #[error("the durable live-agent-credential limit has been reached for this user")]
    AgentCredentialCapacity,
    #[error("the NIP-98 event has already been accepted")]
    ReplayedProof,
    #[error("the idempotency key is already bound to a different identity command")]
    IdempotencyConflict,
    #[error("the authentication command contains an invalid value")]
    InvalidValue,
    #[error("the authentication command outcome is unknown")]
    OutcomeUnknown,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct BootstrapIdentity {
    pub(crate) instance_id: InstanceId,
    pub(crate) owner_user_id: UserId,
    pub(crate) credential: NewHumanCredential,
    pub(crate) configured_providers: ConfiguredLoginProviders,
    pub(crate) occurred_at: OffsetDateTime,
    pub(crate) audit_event_id: AdminAuditEventId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BootstrapIdentityResult {
    pub(crate) instance: StoredInstanceIdentity,
    pub(crate) owner_user_id: UserId,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct CreateUser {
    pub(crate) user_id: UserId,
    pub(crate) created_by_user_id: UserId,
    pub(crate) status: UserStatus,
    pub(crate) roles: BTreeSet<UserRole>,
    pub(crate) credentials: Vec<NewHumanCredential>,
    pub(crate) configured_providers: ConfiguredLoginProviders,
    pub(crate) occurred_at: OffsetDateTime,
    pub(crate) audit: MutationAuditContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SetUserStatus {
    pub(crate) user_id: UserId,
    pub(crate) changed_by_user_id: UserId,
    pub(crate) expected_version: u64,
    pub(crate) status: UserStatus,
    pub(crate) configured_providers: ConfiguredLoginProviders,
    pub(crate) occurred_at: OffsetDateTime,
    pub(crate) audit: MutationAuditContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReplaceUserRoles {
    pub(crate) user_id: UserId,
    pub(crate) expected_version: u64,
    pub(crate) roles: BTreeSet<UserRole>,
    pub(crate) assigned_by_user_id: UserId,
    pub(crate) occurred_at: OffsetDateTime,
    pub(crate) audit: MutationAuditContext,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct PutHumanCredential {
    pub(crate) user_id: UserId,
    pub(crate) managed_by_user_id: UserId,
    pub(crate) credential: NewHumanCredential,
    /// `None` creates a credential. `Some(version)` replaces that exact version.
    pub(crate) expected_version: Option<u64>,
    pub(crate) occurred_at: OffsetDateTime,
    pub(crate) audit: MutationAuditContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemoveHumanCredential {
    pub(crate) user_id: UserId,
    pub(crate) managed_by_user_id: UserId,
    pub(crate) kind: HumanCredentialKind,
    pub(crate) expected_version: u64,
    pub(crate) configured_providers: ConfiguredLoginProviders,
    pub(crate) occurred_at: OffsetDateTime,
    pub(crate) audit: MutationAuditContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CreateLoginChallenge {
    pub(crate) challenge_id: LoginChallengeId,
    pub(crate) provider: HumanLoginProvider,
    pub(crate) challenge_digest: LoginChallengeDigest,
    pub(crate) created_at: OffsetDateTime,
    pub(crate) expires_at: OffsetDateTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SessionAuthenticationEvidence {
    Password {
        expected_credential_version: u64,
    },
    Nostr {
        expected_credential_version: u64,
        challenge_id: LoginChallengeId,
        challenge_digest: LoginChallengeDigest,
        event_id: Nip98EventId,
        proof_created_at: OffsetDateTime,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CreateBrowserSession {
    pub(crate) session_id: AdminSessionId,
    pub(crate) user_id: UserId,
    pub(crate) expected_user_version: u64,
    pub(crate) session_token_digest: SessionTokenDigest,
    pub(crate) csrf_token_digest: CsrfTokenDigest,
    pub(crate) evidence: SessionAuthenticationEvidence,
    pub(crate) authenticated_at: OffsetDateTime,
    pub(crate) fresh_until: OffsetDateTime,
    pub(crate) expires_at: OffsetDateTime,
    pub(crate) audit: SessionAuditContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RevokeBrowserSession {
    pub(crate) session_id: AdminSessionId,
    pub(crate) expected_version: u64,
    pub(crate) revoked_at: OffsetDateTime,
    pub(crate) audit: SessionAuditContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RegisterAgentCredential {
    pub(crate) credential_id: AgentCredentialId,
    pub(crate) owner_user_id: UserId,
    pub(crate) issuer_user_id: UserId,
    pub(crate) public_key: NostrPublicKey,
    pub(crate) label: Box<str>,
    pub(crate) scopes: BTreeSet<AdminScope>,
    pub(crate) created_at: OffsetDateTime,
    pub(crate) expires_at: Option<OffsetDateTime>,
    pub(crate) audit: MutationAuditContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReplaceAgentScopes {
    pub(crate) credential_id: AgentCredentialId,
    pub(crate) expected_version: u64,
    pub(crate) issuer_user_id: UserId,
    pub(crate) scopes: BTreeSet<AdminScope>,
    pub(crate) occurred_at: OffsetDateTime,
    pub(crate) audit: MutationAuditContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RevokeAgentCredential {
    pub(crate) credential_id: AgentCredentialId,
    pub(crate) revoked_by_user_id: UserId,
    pub(crate) expected_version: u64,
    pub(crate) revoked_at: OffsetDateTime,
    pub(crate) audit: MutationAuditContext,
}

impl CreateUser {
    fn fingerprint(&self) -> CommandFingerprint {
        let mut builder = FingerprintBuilder::new("identity.user.create");
        builder.field(self.status.as_str().as_bytes());
        fingerprint_roles(&mut builder, &self.roles);
        for credential in &self.credentials {
            fingerprint_credential(&mut builder, credential);
        }
        builder.finish()
    }
}

impl SetUserStatus {
    fn fingerprint(&self) -> CommandFingerprint {
        let mut builder = FingerprintBuilder::new("identity.user.status.replace");
        builder.uuid(self.user_id.as_uuid());
        builder.version(self.expected_version);
        builder.field(self.status.as_str().as_bytes());
        builder.finish()
    }
}

impl ReplaceUserRoles {
    fn fingerprint(&self) -> CommandFingerprint {
        let mut builder = FingerprintBuilder::new("identity.user.roles.replace");
        builder.uuid(self.user_id.as_uuid());
        builder.version(self.expected_version);
        fingerprint_roles(&mut builder, &self.roles);
        builder.finish()
    }
}

impl PutHumanCredential {
    fn fingerprint(&self) -> CommandFingerprint {
        let mut builder = FingerprintBuilder::new("identity.user.credential.put");
        builder.uuid(self.user_id.as_uuid());
        match self.expected_version {
            Some(version) => {
                builder.field(b"replace");
                builder.version(version);
            }
            None => builder.field(b"create"),
        }
        fingerprint_credential(&mut builder, &self.credential);
        builder.finish()
    }
}

impl RemoveHumanCredential {
    fn fingerprint(&self) -> CommandFingerprint {
        let mut builder = FingerprintBuilder::new("identity.user.credential.remove");
        builder.uuid(self.user_id.as_uuid());
        builder.version(self.expected_version);
        builder.field(self.kind.provider().as_str().as_bytes());
        builder.finish()
    }
}

impl RegisterAgentCredential {
    fn fingerprint(&self) -> CommandFingerprint {
        let mut builder = FingerprintBuilder::new("identity.agent.register");
        builder.uuid(self.owner_user_id.as_uuid());
        builder.field(self.public_key.as_bytes());
        builder.field(self.label.as_bytes());
        fingerprint_scopes(&mut builder, &self.scopes);
        match self.expires_at {
            Some(expires_at) => builder.field(&expires_at.unix_timestamp_nanos().to_be_bytes()),
            None => builder.field(b"no-expiry"),
        }
        builder.finish()
    }
}

impl ReplaceAgentScopes {
    fn fingerprint(&self) -> CommandFingerprint {
        let mut builder = FingerprintBuilder::new("identity.agent.scopes.replace");
        builder.uuid(self.credential_id.as_uuid());
        builder.version(self.expected_version);
        fingerprint_scopes(&mut builder, &self.scopes);
        builder.finish()
    }
}

impl RevokeAgentCredential {
    fn fingerprint(&self) -> CommandFingerprint {
        let mut builder = FingerprintBuilder::new("identity.agent.revoke");
        builder.uuid(self.credential_id.as_uuid());
        builder.version(self.expected_version);
        builder.finish()
    }
}

fn fingerprint_roles(builder: &mut FingerprintBuilder, roles: &BTreeSet<UserRole>) {
    for role in roles {
        builder.field(role.as_str().as_bytes());
    }
}

fn fingerprint_scopes(builder: &mut FingerprintBuilder, scopes: &BTreeSet<AdminScope>) {
    for scope in scopes {
        builder.field(scope.as_str().as_bytes());
    }
}

fn fingerprint_credential(builder: &mut FingerprintBuilder, credential: &NewHumanCredential) {
    match credential {
        NewHumanCredential::Password { username, .. } => {
            builder.field(HumanLoginProvider::Password.as_str().as_bytes());
            builder.field(username.as_str().as_bytes());
        }
        NewHumanCredential::Nostr { public_key } => {
            builder.field(HumanLoginProvider::Nostr.as_str().as_bytes());
            builder.field(public_key.as_bytes());
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MutationAuditContext {
    pub(crate) audit_event_id: AdminAuditEventId,
    pub(crate) principal: AuditPrincipalReference,
    pub(crate) request_id: Option<Uuid>,
    pub(crate) idempotency_key: AdminMutationKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SessionAuditContext {
    pub(crate) audit_event_id: AdminAuditEventId,
    pub(crate) request_id: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdminAuditFailureOutcome {
    Denied,
}

impl AdminAuditFailureOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct RecordAdminAuditFailure {
    pub(crate) audit_event_id: AdminAuditEventId,
    pub(crate) principal: AuditPrincipalReference,
    pub(crate) request_id: Option<Uuid>,
    pub(crate) action: Box<str>,
    pub(crate) outcome: AdminAuditFailureOutcome,
    pub(crate) reason_code: Box<str>,
    pub(crate) occurred_at: OffsetDateTime,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct AdminMutationKey(pub(crate) Uuid);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CommandFingerprint([u8; 32]);

impl CommandFingerprint {
    const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

struct FingerprintBuilder(blake3::Hasher);

impl FingerprintBuilder {
    fn new(action: &'static str) -> Self {
        let mut builder = Self(blake3::Hasher::new());
        builder.field(action.as_bytes());
        builder
    }

    fn field(&mut self, value: &[u8]) {
        self.0.update(&(value.len() as u64).to_be_bytes());
        self.0.update(value);
    }

    fn uuid(&mut self, value: &Uuid) {
        self.field(value.as_bytes());
    }

    fn version(&mut self, value: u64) {
        self.field(&value.to_be_bytes());
    }

    fn finish(self) -> CommandFingerprint {
        CommandFingerprint(*self.0.finalize().as_bytes())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IdentityMutationResult {
    User(UserMutationResult),
    AgentCredential(AgentCredentialMutationResult),
}

impl IdentityMutationResult {
    const fn kind(self) -> &'static str {
        match self {
            Self::User(_) => "user",
            Self::AgentCredential(_) => "agent_credential",
        }
    }

    const fn identifier(self) -> Uuid {
        match self {
            Self::User(result) => result.user_id.into_uuid(),
            Self::AgentCredential(result) => result.credential_id.into_uuid(),
        }
    }

    const fn version(self) -> u64 {
        match self {
            Self::User(result) => result.version,
            Self::AgentCredential(result) => result.version,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcceptAgentProof {
    pub(crate) credential_id: AgentCredentialId,
    pub(crate) expected_credential_version: u64,
    pub(crate) expected_owner_version: u64,
    pub(crate) event_id: Nip98EventId,
    pub(crate) accepted_at: OffsetDateTime,
    pub(crate) proof_created_at: OffsetDateTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdminAuditOutcome {
    Succeeded,
    Denied,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AuditPrincipalReference {
    BrowserSession {
        user_id: UserId,
        session_id: AdminSessionId,
    },
    AgentCredential {
        user_id: UserId,
        credential_id: AgentCredentialId,
    },
    Offline {
        user_id: Option<UserId>,
    },
    Unauthenticated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UserMutationResult {
    pub(crate) user_id: UserId,
    pub(crate) version: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BrowserSessionMutationResult {
    pub(crate) session_id: AdminSessionId,
    pub(crate) version: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AgentCredentialMutationResult {
    pub(crate) credential_id: AgentCredentialId,
    pub(crate) version: u64,
}

pub(crate) async fn record_admin_audit_failure(
    transaction: &mut Transaction<'_, Sqlite>,
    command: RecordAdminAuditFailure,
) -> Result<(), AuthApplyError> {
    validate_audit_text(&command.action, MAX_AUDIT_ACTION_BYTES)?;
    validate_audit_text(&command.reason_code, MAX_AUDIT_REASON_BYTES)?;
    append_audit_event(
        transaction,
        AuditEventInsert {
            audit_event_id: command.audit_event_id,
            principal: &command.principal,
            request_id: command.request_id,
            idempotency_key: None,
            occurred_at: command.occurred_at,
            action: &command.action,
            outcome: command.outcome.as_str(),
            reason_code: Some(&command.reason_code),
        },
    )
    .await
}

pub(crate) async fn bootstrap_identity(
    transaction: &mut Transaction<'_, Sqlite>,
    command: BootstrapIdentity,
) -> Result<BootstrapIdentityResult, AuthApplyError> {
    if !command
        .configured_providers
        .accepts(command.credential.provider())
    {
        return Err(AuthCommandError::EnabledUserRequiresCredential.into());
    }
    let timestamp = command_timestamp(command.occurred_at)?;
    let instance_count: i64 = sqlx::query_scalar("SELECT count(*) FROM instance_identity")
        .fetch_one(&mut **transaction)
        .await?;
    let user_count: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&mut **transaction)
        .await?;
    if instance_count != 0 || user_count != 0 {
        return Err(AuthCommandError::AlreadyBootstrapped.into());
    }

    sqlx::query(
        "INSERT INTO instance_identity (singleton, instance_id, version, created_at_ns) \
         VALUES (1, ?, 1, ?)",
    )
    .bind(command.instance_id.as_uuid().as_bytes().as_slice())
    .bind(timestamp)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO users (user_id, status, version, created_at_ns, updated_at_ns) \
         VALUES (?, 'enabled', 1, ?, ?)",
    )
    .bind(command.owner_user_id.as_uuid().as_bytes().as_slice())
    .bind(timestamp)
    .bind(timestamp)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO user_roles (user_id, role, assigned_by_user_id, assigned_at_ns) \
         VALUES (?, 'owner', NULL, ?)",
    )
    .bind(command.owner_user_id.as_uuid().as_bytes().as_slice())
    .bind(timestamp)
    .execute(&mut **transaction)
    .await?;
    insert_new_credential(
        transaction,
        command.owner_user_id,
        command.credential,
        timestamp,
    )
    .await?;

    sqlx::query(
        "INSERT INTO admin_audit_events (\
            audit_event_id, occurred_at_ns, principal_kind, actor_user_id, action, outcome\
         ) VALUES (?, ?, 'offline', ?, 'identity.bootstrap', 'succeeded')",
    )
    .bind(command.audit_event_id.as_uuid().as_bytes().as_slice())
    .bind(timestamp)
    .bind(command.owner_user_id.as_uuid().as_bytes().as_slice())
    .execute(&mut **transaction)
    .await?;

    Ok(BootstrapIdentityResult {
        instance: StoredInstanceIdentity {
            instance_id: command.instance_id,
            version: 1,
            created_at: command.occurred_at.to_offset(UtcOffset::UTC),
        },
        owner_user_id: command.owner_user_id,
    })
}

pub(crate) async fn create_user(
    transaction: &mut Transaction<'_, Sqlite>,
    command: CreateUser,
) -> Result<UserMutationResult, AuthApplyError> {
    require_mutation_actor(
        transaction,
        &command.audit,
        command.created_by_user_id,
        AdminScope::UserManage,
        command.occurred_at,
    )
    .await?;
    let fingerprint = command.fingerprint();
    if let Some(result) = replay_identity_mutation(
        transaction,
        &command.audit,
        "identity.user.create",
        fingerprint,
        "user",
    )
    .await?
    {
        return user_mutation_result(result);
    }
    require_bootstrapped(transaction).await?;
    validate_roles(&command.roles)?;
    require_user_scope(
        transaction,
        command.created_by_user_id,
        AdminScope::UserManage,
    )
    .await?;
    if command.roles != BTreeSet::from([UserRole::Publisher]) {
        require_user_scope(
            transaction,
            command.created_by_user_id,
            AdminScope::RoleAssign,
        )
        .await?;
    }
    validate_new_credentials(&command.credentials)?;
    if command.status == UserStatus::Enabled
        && !command
            .credentials
            .iter()
            .any(|credential| command.configured_providers.accepts(credential.provider()))
    {
        return Err(AuthCommandError::EnabledUserRequiresCredential.into());
    }
    let timestamp = command_timestamp(command.occurred_at)?;
    let insertion = sqlx::query(
        "INSERT INTO users (user_id, status, version, created_at_ns, updated_at_ns) \
         VALUES (?, ?, 1, ?, ?)",
    )
    .bind(command.user_id.as_uuid().as_bytes().as_slice())
    .bind(command.status.as_str())
    .bind(timestamp)
    .bind(timestamp)
    .execute(&mut **transaction)
    .await;
    map_conflict(insertion)?;

    for role in command.roles {
        sqlx::query(
            "INSERT INTO user_roles (user_id, role, assigned_by_user_id, assigned_at_ns) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(command.user_id.as_uuid().as_bytes().as_slice())
        .bind(role.as_str())
        .bind(command.created_by_user_id.as_uuid().as_bytes().as_slice())
        .bind(timestamp)
        .execute(&mut **transaction)
        .await?;
    }
    for credential in command.credentials {
        insert_new_credential(transaction, command.user_id, credential, timestamp).await?;
    }
    user_mutation_result(
        complete_identity_mutation(
            transaction,
            command.audit,
            command.occurred_at,
            "identity.user.create",
            fingerprint,
            IdentityMutationResult::User(UserMutationResult {
                user_id: command.user_id,
                version: 1,
            }),
        )
        .await?,
    )
}

pub(crate) async fn set_user_status(
    transaction: &mut Transaction<'_, Sqlite>,
    command: SetUserStatus,
) -> Result<UserMutationResult, AuthApplyError> {
    require_mutation_actor(
        transaction,
        &command.audit,
        command.changed_by_user_id,
        AdminScope::UserManage,
        command.occurred_at,
    )
    .await?;
    let fingerprint = command.fingerprint();
    if let Some(result) = replay_identity_mutation(
        transaction,
        &command.audit,
        "identity.user.status.replace",
        fingerprint,
        "user",
    )
    .await?
    {
        return user_mutation_result(result);
    }
    let user = require_user_management_authority(
        transaction,
        command.changed_by_user_id,
        command.user_id,
        AdminScope::UserManage,
    )
    .await?;
    require_version(user.version, command.expected_version)?;
    if command.occurred_at < user.updated_at {
        return Err(AuthCommandError::InvalidValue.into());
    }
    if command.status == UserStatus::Enabled
        && !user_has_accepted_credential(&user, command.configured_providers)
    {
        return Err(AuthCommandError::EnabledUserRequiresCredential.into());
    }
    if user.status == UserStatus::Enabled
        && command.status == UserStatus::Disabled
        && user.roles.contains(&UserRole::Owner)
    {
        require_another_enabled_owner(transaction, command.user_id).await?;
    }
    let next_version = checked_next_version(user.version)?;
    let result = sqlx::query(
        "UPDATE users SET status = ?, version = ?, updated_at_ns = ? \
         WHERE user_id = ? AND version = ?",
    )
    .bind(command.status.as_str())
    .bind(version_i64(next_version)?)
    .bind(command_timestamp(command.occurred_at)?)
    .bind(command.user_id.as_uuid().as_bytes().as_slice())
    .bind(version_i64(command.expected_version)?)
    .execute(&mut **transaction)
    .await?;
    require_one_row(result.rows_affected())?;
    if command.status == UserStatus::Disabled {
        revoke_user_sessions(transaction, command.user_id, command.occurred_at).await?;
        revoke_user_agents(transaction, command.user_id, command.occurred_at).await?;
    }
    user_mutation_result(
        complete_identity_mutation(
            transaction,
            command.audit,
            command.occurred_at,
            "identity.user.status.replace",
            fingerprint,
            IdentityMutationResult::User(UserMutationResult {
                user_id: command.user_id,
                version: next_version,
            }),
        )
        .await?,
    )
}

pub(crate) async fn replace_user_roles(
    transaction: &mut Transaction<'_, Sqlite>,
    command: ReplaceUserRoles,
) -> Result<UserMutationResult, AuthApplyError> {
    require_mutation_actor(
        transaction,
        &command.audit,
        command.assigned_by_user_id,
        AdminScope::RoleAssign,
        command.occurred_at,
    )
    .await?;
    let fingerprint = command.fingerprint();
    if let Some(result) = replay_identity_mutation(
        transaction,
        &command.audit,
        "identity.user.roles.replace",
        fingerprint,
        "user",
    )
    .await?
    {
        return user_mutation_result(result);
    }
    validate_roles(&command.roles)?;
    require_user_scope(
        transaction,
        command.assigned_by_user_id,
        AdminScope::RoleAssign,
    )
    .await?;
    let user = required_user(transaction, command.user_id).await?;
    require_version(user.version, command.expected_version)?;
    if command.occurred_at < user.updated_at {
        return Err(AuthCommandError::InvalidValue.into());
    }
    if user.status == UserStatus::Enabled
        && user.roles.contains(&UserRole::Owner)
        && !command.roles.contains(&UserRole::Owner)
    {
        require_another_enabled_owner(transaction, command.user_id).await?;
    }
    sqlx::query("DELETE FROM user_roles WHERE user_id = ?")
        .bind(command.user_id.as_uuid().as_bytes().as_slice())
        .execute(&mut **transaction)
        .await?;
    let assigned_at = command_timestamp(command.occurred_at)?;
    for role in command.roles {
        sqlx::query(
            "INSERT INTO user_roles (user_id, role, assigned_by_user_id, assigned_at_ns) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(command.user_id.as_uuid().as_bytes().as_slice())
        .bind(role.as_str())
        .bind(command.assigned_by_user_id.as_uuid().as_bytes().as_slice())
        .bind(assigned_at)
        .execute(&mut **transaction)
        .await?;
    }
    let next_version = checked_next_version(user.version)?;
    let result = sqlx::query(
        "UPDATE users SET version = ?, updated_at_ns = ? WHERE user_id = ? AND version = ?",
    )
    .bind(version_i64(next_version)?)
    .bind(assigned_at)
    .bind(command.user_id.as_uuid().as_bytes().as_slice())
    .bind(version_i64(command.expected_version)?)
    .execute(&mut **transaction)
    .await?;
    require_one_row(result.rows_affected())?;
    user_mutation_result(
        complete_identity_mutation(
            transaction,
            command.audit,
            command.occurred_at,
            "identity.user.roles.replace",
            fingerprint,
            IdentityMutationResult::User(UserMutationResult {
                user_id: command.user_id,
                version: next_version,
            }),
        )
        .await?,
    )
}

pub(crate) async fn put_human_credential(
    transaction: &mut Transaction<'_, Sqlite>,
    command: PutHumanCredential,
) -> Result<UserMutationResult, AuthApplyError> {
    require_mutation_actor(
        transaction,
        &command.audit,
        command.managed_by_user_id,
        AdminScope::CredentialManage,
        command.occurred_at,
    )
    .await?;
    let fingerprint = command.fingerprint();
    if let Some(result) = replay_identity_mutation(
        transaction,
        &command.audit,
        "identity.user.credential.put",
        fingerprint,
        "user",
    )
    .await?
    {
        return user_mutation_result(result);
    }
    let user = require_user_management_authority(
        transaction,
        command.managed_by_user_id,
        command.user_id,
        AdminScope::CredentialManage,
    )
    .await?;
    if command.occurred_at < user.created_at {
        return Err(AuthCommandError::InvalidValue.into());
    }
    let provider = command.credential.provider();
    let current = credential_state(transaction, command.user_id, provider).await?;
    match (current, command.expected_version) {
        (None, None) => {
            insert_new_credential(
                transaction,
                command.user_id,
                command.credential,
                command_timestamp(command.occurred_at)?,
            )
            .await?;
            Ok::<(), AuthApplyError>(())
        }
        (Some(_), None) => Err(AuthCommandError::Conflict.into()),
        (None, Some(_)) => Err(AuthCommandError::NotFound.into()),
        (Some(current), Some(expected)) => {
            require_version(current.version, expected)?;
            if command.occurred_at < current.created_at {
                return Err(AuthCommandError::InvalidValue.into());
            }
            let next = checked_next_version(current.version)?;
            update_credential(
                transaction,
                command.user_id,
                command.credential,
                expected,
                next,
                command_timestamp(command.occurred_at)?,
            )
            .await?;
            revoke_user_sessions(transaction, command.user_id, command.occurred_at).await?;
            Ok(())
        }
    }?;
    let next_user_version = checked_next_version(user.version)?;
    let updated = sqlx::query(
        "UPDATE users SET version = ?, updated_at_ns = ? WHERE user_id = ? AND version = ?",
    )
    .bind(version_i64(next_user_version)?)
    .bind(command_timestamp(command.occurred_at)?)
    .bind(command.user_id.as_uuid().as_bytes().as_slice())
    .bind(version_i64(user.version)?)
    .execute(&mut **transaction)
    .await?;
    require_one_row(updated.rows_affected())?;
    user_mutation_result(
        complete_identity_mutation(
            transaction,
            command.audit,
            command.occurred_at,
            "identity.user.credential.put",
            fingerprint,
            IdentityMutationResult::User(UserMutationResult {
                user_id: command.user_id,
                version: next_user_version,
            }),
        )
        .await?,
    )
}

pub(crate) async fn remove_human_credential(
    transaction: &mut Transaction<'_, Sqlite>,
    command: RemoveHumanCredential,
) -> Result<UserMutationResult, AuthApplyError> {
    require_mutation_actor(
        transaction,
        &command.audit,
        command.managed_by_user_id,
        AdminScope::CredentialManage,
        command.occurred_at,
    )
    .await?;
    let fingerprint = command.fingerprint();
    if let Some(result) = replay_identity_mutation(
        transaction,
        &command.audit,
        "identity.user.credential.remove",
        fingerprint,
        "user",
    )
    .await?
    {
        return user_mutation_result(result);
    }
    let user = require_user_management_authority(
        transaction,
        command.managed_by_user_id,
        command.user_id,
        AdminScope::CredentialManage,
    )
    .await?;
    let provider = command.kind.provider();
    let current = credential_state(transaction, command.user_id, provider)
        .await?
        .ok_or(AuthCommandError::NotFound)?;
    require_version(current.version, command.expected_version)?;
    if command.occurred_at < current.created_at {
        return Err(AuthCommandError::InvalidValue.into());
    }
    if user.status == UserStatus::Enabled {
        let retained = match command.kind {
            HumanCredentialKind::Password => {
                user.has_nostr
                    && command
                        .configured_providers
                        .accepts(HumanLoginProvider::Nostr)
            }
            HumanCredentialKind::Nostr => {
                user.has_password
                    && command
                        .configured_providers
                        .accepts(HumanLoginProvider::Password)
            }
        };
        if !retained {
            return Err(AuthCommandError::EnabledUserRequiresCredential.into());
        }
    }
    let result = match command.kind {
        HumanCredentialKind::Password => {
            sqlx::query("DELETE FROM user_password_credentials WHERE user_id = ? AND version = ?")
                .bind(command.user_id.as_uuid().as_bytes().as_slice())
                .bind(version_i64(command.expected_version)?)
                .execute(&mut **transaction)
                .await?
        }
        HumanCredentialKind::Nostr => {
            sqlx::query("DELETE FROM user_nostr_credentials WHERE user_id = ? AND version = ?")
                .bind(command.user_id.as_uuid().as_bytes().as_slice())
                .bind(version_i64(command.expected_version)?)
                .execute(&mut **transaction)
                .await?
        }
    };
    require_one_row(result.rows_affected())?;
    revoke_user_sessions(transaction, command.user_id, command.occurred_at).await?;
    let next_user_version = checked_next_version(user.version)?;
    let updated = sqlx::query(
        "UPDATE users SET version = ?, updated_at_ns = ? WHERE user_id = ? AND version = ?",
    )
    .bind(version_i64(next_user_version)?)
    .bind(command_timestamp(command.occurred_at)?)
    .bind(command.user_id.as_uuid().as_bytes().as_slice())
    .bind(version_i64(user.version)?)
    .execute(&mut **transaction)
    .await?;
    require_one_row(updated.rows_affected())?;
    user_mutation_result(
        complete_identity_mutation(
            transaction,
            command.audit,
            command.occurred_at,
            "identity.user.credential.remove",
            fingerprint,
            IdentityMutationResult::User(UserMutationResult {
                user_id: user.user_id,
                version: next_user_version,
            }),
        )
        .await?,
    )
}

pub(crate) async fn create_login_challenge(
    transaction: &mut Transaction<'_, Sqlite>,
    command: CreateLoginChallenge,
) -> Result<StoredLoginChallenge, AuthApplyError> {
    require_bootstrapped(transaction).await?;
    if command.expires_at <= command.created_at {
        return Err(AuthCommandError::InvalidValue.into());
    }
    cleanup_login_challenges(transaction, command.created_at).await?;
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM login_challenges \
         WHERE consumed_at_ns IS NULL AND expires_at_ns > ?",
    )
    .bind(command_timestamp(command.created_at)?)
    .fetch_one(&mut **transaction)
    .await?;
    if live >= MAX_LIVE_LOGIN_CHALLENGES {
        return Err(AuthCommandError::ChallengeCapacity.into());
    }
    let insertion = sqlx::query(
        "INSERT INTO login_challenges (\
            challenge_id, provider, challenge_digest, created_at_ns, expires_at_ns\
         ) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(command.challenge_id.as_uuid().as_bytes().as_slice())
    .bind(command.provider.as_str())
    .bind(command.challenge_digest.as_bytes().as_slice())
    .bind(command_timestamp(command.created_at)?)
    .bind(command_timestamp(command.expires_at)?)
    .execute(&mut **transaction)
    .await;
    map_conflict(insertion)?;
    Ok(StoredLoginChallenge {
        challenge_id: command.challenge_id,
        provider: command.provider,
        challenge_digest: command.challenge_digest,
        created_at: command.created_at.to_offset(UtcOffset::UTC),
        expires_at: command.expires_at.to_offset(UtcOffset::UTC),
        consumed_at: None,
    })
}

pub(crate) async fn create_browser_session(
    transaction: &mut Transaction<'_, Sqlite>,
    command: CreateBrowserSession,
) -> Result<BrowserSessionMutationResult, AuthApplyError> {
    if command.fresh_until < command.authenticated_at
        || command.expires_at < command.fresh_until
        || command.session_token_digest.as_bytes() == command.csrf_token_digest.as_bytes()
    {
        return Err(AuthCommandError::InvalidValue.into());
    }
    cleanup_login_challenges(transaction, command.authenticated_at).await?;
    cleanup_browser_sessions(transaction, command.authenticated_at).await?;
    let live_sessions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM browser_sessions \
         WHERE revoked_at_ns IS NULL AND expires_at_ns > ?",
    )
    .bind(command_timestamp(command.authenticated_at)?)
    .fetch_one(&mut **transaction)
    .await?;
    if live_sessions >= MAX_LIVE_BROWSER_SESSIONS {
        return Err(AuthCommandError::SessionCapacity.into());
    }
    let instance_version: i64 =
        sqlx::query_scalar("SELECT version FROM instance_identity WHERE singleton = 1")
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(AuthCommandError::BootstrapRequired)?;
    let instance_version =
        positive_u64(instance_version).ok_or(AuthApplyError::CorruptStoredState)?;
    let user = required_user(transaction, command.user_id).await?;
    if user.status != UserStatus::Enabled {
        return Err(AuthCommandError::NotFound.into());
    }
    require_version(user.version, command.expected_user_version)?;
    let provider = match command.evidence {
        SessionAuthenticationEvidence::Password {
            expected_credential_version,
        } => {
            if !user.has_password {
                return Err(AuthCommandError::NotFound.into());
            }
            let actual =
                credential_state(transaction, command.user_id, HumanLoginProvider::Password)
                    .await?
                    .ok_or(AuthCommandError::NotFound)?;
            require_version(actual.version, expected_credential_version)?;
            HumanLoginProvider::Password
        }
        SessionAuthenticationEvidence::Nostr {
            expected_credential_version,
            challenge_id,
            challenge_digest,
            event_id,
            proof_created_at,
        } => {
            if !user.has_nostr {
                return Err(AuthCommandError::InvalidChallenge.into());
            }
            let actual = credential_state(transaction, command.user_id, HumanLoginProvider::Nostr)
                .await?
                .ok_or(AuthCommandError::NotFound)?;
            require_version(actual.version, expected_credential_version)?;
            let result = sqlx::query(
                "UPDATE login_challenges SET consumed_at_ns = ? \
                 WHERE challenge_id = ? AND provider = 'nostr' AND challenge_digest = ? \
                   AND consumed_at_ns IS NULL AND expires_at_ns > ?",
            )
            .bind(command_timestamp(command.authenticated_at)?)
            .bind(challenge_id.as_uuid().as_bytes().as_slice())
            .bind(challenge_digest.as_bytes().as_slice())
            .bind(command_timestamp(command.authenticated_at)?)
            .execute(&mut **transaction)
            .await?;
            if result.rows_affected() != 1 {
                return Err(AuthCommandError::InvalidChallenge.into());
            }
            insert_replay_event(
                transaction,
                event_id,
                ReplayPrincipal::Human(command.user_id),
                command.authenticated_at,
                proof_created_at,
            )
            .await?;
            HumanLoginProvider::Nostr
        }
    };
    let insertion = sqlx::query(
        "INSERT INTO browser_sessions (\
            session_id, user_id, provider, session_token_digest, csrf_token_digest, \
            instance_version, version, authenticated_at_ns, fresh_until_ns, expires_at_ns, \
            last_seen_at_ns\
         ) VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?)",
    )
    .bind(command.session_id.as_uuid().as_bytes().as_slice())
    .bind(command.user_id.as_uuid().as_bytes().as_slice())
    .bind(provider.as_str())
    .bind(command.session_token_digest.as_bytes().as_slice())
    .bind(command.csrf_token_digest.as_bytes().as_slice())
    .bind(version_i64(instance_version)?)
    .bind(command_timestamp(command.authenticated_at)?)
    .bind(command_timestamp(command.fresh_until)?)
    .bind(command_timestamp(command.expires_at)?)
    .bind(command_timestamp(command.authenticated_at)?)
    .execute(&mut **transaction)
    .await;
    map_conflict(insertion)?;
    append_audit_event(
        transaction,
        AuditEventInsert {
            audit_event_id: command.audit.audit_event_id,
            principal: &AuditPrincipalReference::BrowserSession {
                user_id: command.user_id,
                session_id: command.session_id,
            },
            request_id: command.audit.request_id,
            idempotency_key: None,
            occurred_at: command.authenticated_at,
            action: "auth.session.login",
            outcome: "succeeded",
            reason_code: None,
        },
    )
    .await?;
    Ok(BrowserSessionMutationResult {
        session_id: command.session_id,
        version: 1,
    })
}

pub(crate) async fn revoke_browser_session(
    transaction: &mut Transaction<'_, Sqlite>,
    command: RevokeBrowserSession,
) -> Result<BrowserSessionMutationResult, AuthApplyError> {
    let current: Option<(Vec<u8>, i64, i64, Option<i64>)> = sqlx::query_as(
        "SELECT user_id, version, authenticated_at_ns, revoked_at_ns \
         FROM browser_sessions WHERE session_id = ?",
    )
    .bind(command.session_id.as_uuid().as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await?;
    let (stored_user_id, current_version, authenticated_at, revoked_at) =
        current.ok_or(AuthCommandError::NotFound)?;
    let user_id = user_id(&stored_user_id).map_err(AuthApplyError::from_load)?;
    let current_version =
        positive_u64(current_version).ok_or(AuthApplyError::CorruptStoredState)?;
    require_version(current_version, command.expected_version)?;
    if revoked_at.is_some() {
        return Err(AuthCommandError::NotFound.into());
    }
    let authenticated_at = timestamp(authenticated_at).map_err(AuthApplyError::from_load)?;
    if command.revoked_at < authenticated_at {
        return Err(AuthCommandError::InvalidValue.into());
    }
    let next = checked_next_version(command.expected_version)?;
    let result = sqlx::query(
        "UPDATE browser_sessions SET revoked_at_ns = ?, version = ? \
         WHERE session_id = ? AND version = ? AND revoked_at_ns IS NULL",
    )
    .bind(command_timestamp(command.revoked_at)?)
    .bind(version_i64(next)?)
    .bind(command.session_id.as_uuid().as_bytes().as_slice())
    .bind(version_i64(command.expected_version)?)
    .execute(&mut **transaction)
    .await?;
    require_one_row(result.rows_affected())?;
    append_audit_event(
        transaction,
        AuditEventInsert {
            audit_event_id: command.audit.audit_event_id,
            principal: &AuditPrincipalReference::BrowserSession {
                user_id,
                session_id: command.session_id,
            },
            request_id: command.audit.request_id,
            idempotency_key: None,
            occurred_at: command.revoked_at,
            action: "auth.session.logout",
            outcome: "succeeded",
            reason_code: None,
        },
    )
    .await?;
    Ok(BrowserSessionMutationResult {
        session_id: command.session_id,
        version: next,
    })
}

pub(crate) async fn register_agent_credential(
    transaction: &mut Transaction<'_, Sqlite>,
    command: RegisterAgentCredential,
) -> Result<AgentCredentialMutationResult, AuthApplyError> {
    require_mutation_actor(
        transaction,
        &command.audit,
        command.issuer_user_id,
        AdminScope::CredentialManage,
        command.created_at,
    )
    .await?;
    let fingerprint = command.fingerprint();
    if let Some(result) = replay_identity_mutation(
        transaction,
        &command.audit,
        "identity.agent.register",
        fingerprint,
        "agent_credential",
    )
    .await?
    {
        return agent_mutation_result(result);
    }
    validate_agent_label(&command.label)?;
    if command.scopes.is_empty()
        || command
            .expires_at
            .is_some_and(|expires_at| expires_at <= command.created_at)
    {
        return Err(AuthCommandError::InvalidValue.into());
    }
    require_user_management_authority(
        transaction,
        command.issuer_user_id,
        command.owner_user_id,
        AdminScope::CredentialManage,
    )
    .await?;
    require_scope_subset(
        transaction,
        command.owner_user_id,
        command.issuer_user_id,
        &command.scopes,
    )
    .await?;
    let live_credentials: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_credentials \
         WHERE owner_user_id = ? AND revoked_at_ns IS NULL \
           AND (expires_at_ns IS NULL OR expires_at_ns > ?)",
    )
    .bind(command.owner_user_id.as_uuid().as_bytes().as_slice())
    .bind(command_timestamp(command.created_at)?)
    .fetch_one(&mut **transaction)
    .await?;
    if live_credentials >= MAX_LIVE_AGENT_CREDENTIALS_PER_USER {
        return Err(AuthCommandError::AgentCredentialCapacity.into());
    }
    let insertion = sqlx::query(
        "INSERT INTO agent_credentials (\
            agent_credential_id, owner_user_id, issuer_user_id, public_key, label, version, \
            created_at_ns, expires_at_ns\
         ) VALUES (?, ?, ?, ?, ?, 1, ?, ?)",
    )
    .bind(command.credential_id.as_uuid().as_bytes().as_slice())
    .bind(command.owner_user_id.as_uuid().as_bytes().as_slice())
    .bind(command.issuer_user_id.as_uuid().as_bytes().as_slice())
    .bind(command.public_key.as_bytes().as_slice())
    .bind(command.label.as_ref())
    .bind(command_timestamp(command.created_at)?)
    .bind(command.expires_at.map(command_timestamp).transpose()?)
    .execute(&mut **transaction)
    .await;
    map_conflict(insertion)?;
    insert_agent_scopes(transaction, command.credential_id, &command.scopes).await?;
    agent_mutation_result(
        complete_identity_mutation(
            transaction,
            command.audit,
            command.created_at,
            "identity.agent.register",
            fingerprint,
            IdentityMutationResult::AgentCredential(AgentCredentialMutationResult {
                credential_id: command.credential_id,
                version: 1,
            }),
        )
        .await?,
    )
}

pub(crate) async fn replace_agent_scopes(
    transaction: &mut Transaction<'_, Sqlite>,
    command: ReplaceAgentScopes,
) -> Result<AgentCredentialMutationResult, AuthApplyError> {
    require_mutation_actor(
        transaction,
        &command.audit,
        command.issuer_user_id,
        AdminScope::CredentialManage,
        command.occurred_at,
    )
    .await?;
    let fingerprint = command.fingerprint();
    if let Some(result) = replay_identity_mutation(
        transaction,
        &command.audit,
        "identity.agent.scopes.replace",
        fingerprint,
        "agent_credential",
    )
    .await?
    {
        return agent_mutation_result(result);
    }
    if command.scopes.is_empty() {
        return Err(AuthCommandError::InvalidValue.into());
    }
    let row: Option<(Vec<u8>, i64, Option<i64>)> = sqlx::query_as(
        "SELECT owner_user_id, version, revoked_at_ns FROM agent_credentials \
         WHERE agent_credential_id = ?",
    )
    .bind(command.credential_id.as_uuid().as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await?;
    let (owner, version, revoked_at) = row.ok_or(AuthCommandError::NotFound)?;
    let owner = user_id(&owner).map_err(AuthApplyError::from_load)?;
    let version = positive_u64(version).ok_or(AuthApplyError::CorruptStoredState)?;
    require_version(version, command.expected_version)?;
    if revoked_at.is_some() {
        return Err(AuthCommandError::NotFound.into());
    }
    require_user_management_authority(
        transaction,
        command.issuer_user_id,
        owner,
        AdminScope::CredentialManage,
    )
    .await?;
    require_scope_subset(transaction, owner, command.issuer_user_id, &command.scopes).await?;
    sqlx::query("DELETE FROM agent_credential_scopes WHERE agent_credential_id = ?")
        .bind(command.credential_id.as_uuid().as_bytes().as_slice())
        .execute(&mut **transaction)
        .await?;
    insert_agent_scopes(transaction, command.credential_id, &command.scopes).await?;
    let next = checked_next_version(version)?;
    let result = sqlx::query(
        "UPDATE agent_credentials SET version = ? \
         WHERE agent_credential_id = ? AND version = ? AND revoked_at_ns IS NULL",
    )
    .bind(version_i64(next)?)
    .bind(command.credential_id.as_uuid().as_bytes().as_slice())
    .bind(version_i64(command.expected_version)?)
    .execute(&mut **transaction)
    .await?;
    require_one_row(result.rows_affected())?;
    agent_mutation_result(
        complete_identity_mutation(
            transaction,
            command.audit,
            command.occurred_at,
            "identity.agent.scopes.replace",
            fingerprint,
            IdentityMutationResult::AgentCredential(AgentCredentialMutationResult {
                credential_id: command.credential_id,
                version: next,
            }),
        )
        .await?,
    )
}

pub(crate) async fn revoke_agent_credential(
    transaction: &mut Transaction<'_, Sqlite>,
    command: RevokeAgentCredential,
) -> Result<AgentCredentialMutationResult, AuthApplyError> {
    require_mutation_actor(
        transaction,
        &command.audit,
        command.revoked_by_user_id,
        AdminScope::CredentialManage,
        command.revoked_at,
    )
    .await?;
    let fingerprint = command.fingerprint();
    if let Some(result) = replay_identity_mutation(
        transaction,
        &command.audit,
        "identity.agent.revoke",
        fingerprint,
        "agent_credential",
    )
    .await?
    {
        return agent_mutation_result(result);
    }
    let owner: Vec<u8> = sqlx::query_scalar(
        "SELECT owner_user_id FROM agent_credentials WHERE agent_credential_id = ?",
    )
    .bind(command.credential_id.as_uuid().as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(AuthCommandError::NotFound)?;
    require_user_management_authority(
        transaction,
        command.revoked_by_user_id,
        user_id(&owner).map_err(AuthApplyError::from_load)?,
        AdminScope::CredentialManage,
    )
    .await?;
    let next = checked_next_version(command.expected_version)?;
    let result = sqlx::query(
        "UPDATE agent_credentials \
         SET revoked_at_ns = max(?, created_at_ns, coalesce(last_used_at_ns, created_at_ns)), \
             version = ? \
         WHERE agent_credential_id = ? AND version = ? AND revoked_at_ns IS NULL \
           AND created_at_ns <= ?",
    )
    .bind(command_timestamp(command.revoked_at)?)
    .bind(version_i64(next)?)
    .bind(command.credential_id.as_uuid().as_bytes().as_slice())
    .bind(version_i64(command.expected_version)?)
    .bind(command_timestamp(command.revoked_at)?)
    .execute(&mut **transaction)
    .await?;
    if result.rows_affected() != 1 {
        return Err(classify_agent_version(
            transaction,
            command.credential_id,
            command.expected_version,
        )
        .await?
        .into());
    }
    agent_mutation_result(
        complete_identity_mutation(
            transaction,
            command.audit,
            command.revoked_at,
            "identity.agent.revoke",
            fingerprint,
            IdentityMutationResult::AgentCredential(AgentCredentialMutationResult {
                credential_id: command.credential_id,
                version: next,
            }),
        )
        .await?,
    )
}

type AgentProofRow = (Vec<u8>, i64, Option<i64>, Option<i64>, i64);

pub(crate) async fn accept_agent_proof(
    transaction: &mut Transaction<'_, Sqlite>,
    command: AcceptAgentProof,
) -> Result<AgentCredentialMutationResult, AuthApplyError> {
    let row: Option<AgentProofRow> = sqlx::query_as(
        "SELECT agent.owner_user_id, agent.version, agent.expires_at_ns, agent.revoked_at_ns, \
                users.version AS owner_version \
         FROM agent_credentials AS agent \
         JOIN users ON users.user_id = agent.owner_user_id \
         WHERE agent.agent_credential_id = ? AND users.status = 'enabled'",
    )
    .bind(command.credential_id.as_uuid().as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await?;
    let (owner, version, expires_at, revoked_at, owner_version) =
        row.ok_or(AuthCommandError::NotFound)?;
    if revoked_at.is_some()
        || expires_at.is_some_and(|expires_at| {
            timestamp(expires_at).map_or(true, |expires_at| expires_at <= command.accepted_at)
        })
    {
        return Err(AuthCommandError::NotFound.into());
    }
    let owner = user_id(&owner).map_err(AuthApplyError::from_load)?;
    let version = positive_u64(version).ok_or(AuthApplyError::CorruptStoredState)?;
    let owner_version = positive_u64(owner_version).ok_or(AuthApplyError::CorruptStoredState)?;
    require_version(version, command.expected_credential_version)?;
    require_version(owner_version, command.expected_owner_version)?;
    insert_replay_event(
        transaction,
        command.event_id,
        ReplayPrincipal::Agent(command.credential_id),
        command.accepted_at,
        command.proof_created_at,
    )
    .await?;
    sqlx::query(
        "UPDATE agent_credentials SET last_used_at_ns = CASE \
            WHEN last_used_at_ns IS NULL OR last_used_at_ns < ? THEN ? \
            ELSE last_used_at_ns END \
         WHERE agent_credential_id = ?",
    )
    .bind(command_timestamp(command.accepted_at)?)
    .bind(command_timestamp(command.accepted_at)?)
    .bind(command.credential_id.as_uuid().as_bytes().as_slice())
    .execute(&mut **transaction)
    .await?;
    let _ = owner;
    Ok(AgentCredentialMutationResult {
        credential_id: command.credential_id,
        version,
    })
}

async fn replay_identity_mutation(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: &MutationAuditContext,
    action: &'static str,
    fingerprint: CommandFingerprint,
    expected_result_kind: &'static str,
) -> Result<Option<IdentityMutationResult>, AuthApplyError> {
    let row = sqlx::query_as::<_, IdentityMutationReceiptRow>(
        "SELECT receipt.command_fingerprint, receipt.result_kind, receipt.result_id, \
                receipt.result_version, audit.principal_kind, audit.actor_user_id, \
                audit.session_id, audit.agent_credential_id, audit.action \
         FROM admin_identity_mutation_receipts AS receipt \
         JOIN admin_audit_events AS audit ON audit.audit_event_id = receipt.audit_event_id \
         WHERE receipt.idempotency_key = ?",
    )
    .bind(audit.idempotency_key.0.as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        let claimed: bool = sqlx::query_scalar(
            "SELECT EXISTS(\
                SELECT 1 FROM admin_audit_events WHERE idempotency_key = ?\
             )",
        )
        .bind(audit.idempotency_key.0.as_bytes().as_slice())
        .fetch_one(&mut **transaction)
        .await?;
        return if claimed {
            Err(AuthCommandError::IdempotencyConflict.into())
        } else {
            Ok(None)
        };
    };
    let actor = row
        .actor_user_id
        .as_deref()
        .map(user_id)
        .transpose()
        .map_err(AuthApplyError::from_load)?;
    let session = row
        .session_id
        .as_deref()
        .map(admin_session_id)
        .transpose()
        .map_err(AuthApplyError::from_load)?;
    let agent = row
        .agent_credential_id
        .as_deref()
        .map(agent_credential_id)
        .transpose()
        .map_err(AuthApplyError::from_load)?;
    let principal = audit_principal(&row.principal_kind, actor, session, agent)
        .map_err(AuthApplyError::from_load)?;
    if row.command_fingerprint.as_slice() != fingerprint.as_bytes()
        || row.action != action
        || row.result_kind != expected_result_kind
        || principal != audit.principal
    {
        return Err(AuthCommandError::IdempotencyConflict.into());
    }
    let identifier = uuid(&row.result_id).map_err(AuthApplyError::from_load)?;
    let version = positive_u64(row.result_version).ok_or(AuthApplyError::CorruptStoredState)?;
    let result = match row.result_kind.as_str() {
        "user" => IdentityMutationResult::User(UserMutationResult {
            user_id: UserId::from_uuid(identifier),
            version,
        }),
        "agent_credential" => {
            IdentityMutationResult::AgentCredential(AgentCredentialMutationResult {
                credential_id: AgentCredentialId::from_uuid(identifier),
                version,
            })
        }
        _ => return Err(AuthApplyError::CorruptStoredState),
    };
    Ok(Some(result))
}

async fn require_mutation_actor(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: &MutationAuditContext,
    expected_user_id: UserId,
    required_scope: AdminScope,
    occurred_at: OffsetDateTime,
) -> Result<(), AuthApplyError> {
    let actor_user_id = match audit.principal {
        AuditPrincipalReference::BrowserSession { user_id, .. }
        | AuditPrincipalReference::AgentCredential { user_id, .. }
        | AuditPrincipalReference::Offline {
            user_id: Some(user_id),
        } => user_id,
        AuditPrincipalReference::Offline { user_id: None }
        | AuditPrincipalReference::Unauthenticated => {
            return Err(AuthCommandError::InvalidValue.into());
        }
    };
    if actor_user_id != expected_user_id {
        return Err(AuthCommandError::ScopeEscalation.into());
    }
    require_principal_scope(transaction, &audit.principal, required_scope, occurred_at).await
}

fn user_mutation_result(
    result: IdentityMutationResult,
) -> Result<UserMutationResult, AuthApplyError> {
    match result {
        IdentityMutationResult::User(result) => Ok(result),
        IdentityMutationResult::AgentCredential(_) => Err(AuthApplyError::CorruptStoredState),
    }
}

fn agent_mutation_result(
    result: IdentityMutationResult,
) -> Result<AgentCredentialMutationResult, AuthApplyError> {
    match result {
        IdentityMutationResult::AgentCredential(result) => Ok(result),
        IdentityMutationResult::User(_) => Err(AuthApplyError::CorruptStoredState),
    }
}

async fn complete_identity_mutation(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: MutationAuditContext,
    occurred_at: OffsetDateTime,
    action: &'static str,
    fingerprint: CommandFingerprint,
    result: IdentityMutationResult,
) -> Result<IdentityMutationResult, AuthApplyError> {
    append_success_audit(transaction, &audit, occurred_at, action).await?;
    let insertion = sqlx::query(
        "INSERT INTO admin_identity_mutation_receipts (\
            idempotency_key, audit_event_id, command_fingerprint, result_kind, result_id, \
            result_version, completed_at_ns\
         ) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(audit.idempotency_key.0.as_bytes().as_slice())
    .bind(audit.audit_event_id.as_uuid().as_bytes().as_slice())
    .bind(fingerprint.as_bytes().as_slice())
    .bind(result.kind())
    .bind(result.identifier().as_bytes().to_vec())
    .bind(version_i64(result.version())?)
    .bind(command_timestamp(occurred_at)?)
    .execute(&mut **transaction)
    .await;
    match insertion {
        Ok(_) => Ok(result),
        Err(error) if is_unique_violation(&error) => {
            Err(AuthCommandError::IdempotencyConflict.into())
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) async fn append_success_audit(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: &MutationAuditContext,
    occurred_at: OffsetDateTime,
    action: &'static str,
) -> Result<(), AuthApplyError> {
    append_audit_event(
        transaction,
        AuditEventInsert {
            audit_event_id: audit.audit_event_id,
            principal: &audit.principal,
            request_id: audit.request_id,
            idempotency_key: Some(audit.idempotency_key),
            occurred_at,
            action,
            outcome: "succeeded",
            reason_code: None,
        },
    )
    .await
}

struct AuditEventInsert<'a> {
    audit_event_id: AdminAuditEventId,
    principal: &'a AuditPrincipalReference,
    request_id: Option<Uuid>,
    idempotency_key: Option<AdminMutationKey>,
    occurred_at: OffsetDateTime,
    action: &'a str,
    outcome: &'static str,
    reason_code: Option<&'a str>,
}

async fn append_audit_event(
    transaction: &mut Transaction<'_, Sqlite>,
    insert: AuditEventInsert<'_>,
) -> Result<(), AuthApplyError> {
    validate_audit_text(insert.action, MAX_AUDIT_ACTION_BYTES)?;
    if let Some(reason_code) = insert.reason_code {
        validate_audit_text(reason_code, MAX_AUDIT_REASON_BYTES)?;
    }
    let (principal_kind, actor, session, agent) = match *insert.principal {
        AuditPrincipalReference::BrowserSession {
            user_id,
            session_id,
        } => ("browser_session", Some(user_id), Some(session_id), None),
        AuditPrincipalReference::AgentCredential {
            user_id,
            credential_id,
        } => ("agent_credential", Some(user_id), None, Some(credential_id)),
        AuditPrincipalReference::Offline { user_id } => ("offline", user_id, None, None),
        AuditPrincipalReference::Unauthenticated => ("unauthenticated", None, None, None),
    };
    let insertion = sqlx::query(
        "INSERT INTO admin_audit_events (\
            audit_event_id, occurred_at_ns, principal_kind, actor_user_id, session_id, \
            agent_credential_id, request_id, idempotency_key, action, outcome, reason_code\
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(insert.audit_event_id.as_uuid().as_bytes().as_slice())
    .bind(command_timestamp(insert.occurred_at)?)
    .bind(principal_kind)
    .bind(actor.map(|value| value.into_uuid().as_bytes().to_vec()))
    .bind(session.map(|value| value.into_uuid().as_bytes().to_vec()))
    .bind(agent.map(|value| value.into_uuid().as_bytes().to_vec()))
    .bind(insert.request_id.map(|value| value.as_bytes().to_vec()))
    .bind(
        insert
            .idempotency_key
            .map(|value| value.0.as_bytes().to_vec()),
    )
    .bind(insert.action)
    .bind(insert.outcome)
    .bind(insert.reason_code)
    .execute(&mut **transaction)
    .await;
    map_conflict(insertion)?;
    Ok(())
}

async fn require_bootstrapped(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<(), AuthApplyError> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM instance_identity WHERE singleton = 1)")
            .fetch_one(&mut **transaction)
            .await?;
    if exists {
        Ok(())
    } else {
        Err(AuthCommandError::BootstrapRequired.into())
    }
}

async fn required_user(
    transaction: &mut Transaction<'_, Sqlite>,
    user_id: UserId,
) -> Result<StoredUser, AuthApplyError> {
    load_user(transaction, user_id)
        .await
        .map_err(AuthApplyError::from_load)?
        .ok_or_else(|| AuthCommandError::NotFound.into())
}

pub(crate) async fn require_user_scope(
    transaction: &mut Transaction<'_, Sqlite>,
    user_id: UserId,
    scope: AdminScope,
) -> Result<(), AuthApplyError> {
    let user = required_user(transaction, user_id).await?;
    if user.status != UserStatus::Enabled {
        return Err(AuthCommandError::NotFound.into());
    }
    if !user.scopes().contains(&scope) {
        return Err(AuthCommandError::ScopeEscalation.into());
    }
    Ok(())
}

pub(crate) async fn require_principal_scope(
    transaction: &mut Transaction<'_, Sqlite>,
    principal: &AuditPrincipalReference,
    scope: AdminScope,
    now: OffsetDateTime,
) -> Result<(), AuthApplyError> {
    let now = command_timestamp(now)?;
    let user_id = match *principal {
        AuditPrincipalReference::BrowserSession {
            user_id,
            session_id,
        } => {
            let active: bool = sqlx::query_scalar(
                "SELECT EXISTS(\
                    SELECT 1 FROM browser_sessions AS session \
                    JOIN instance_identity AS instance ON instance.singleton = 1 \
                    WHERE session.session_id = ? AND session.user_id = ? \
                      AND session.instance_version = instance.version \
                      AND session.authenticated_at_ns <= ? \
                      AND session.revoked_at_ns IS NULL AND session.expires_at_ns > ?\
                 )",
            )
            .bind(session_id.as_uuid().as_bytes().as_slice())
            .bind(user_id.as_uuid().as_bytes().as_slice())
            .bind(now)
            .bind(now)
            .fetch_one(&mut **transaction)
            .await?;
            if !active {
                return Err(AuthCommandError::ScopeEscalation.into());
            }
            user_id
        }
        AuditPrincipalReference::AgentCredential {
            user_id,
            credential_id,
        } => {
            let active_with_scope: bool = sqlx::query_scalar(
                "SELECT EXISTS(\
                    SELECT 1 FROM agent_credentials AS agent \
                    JOIN agent_credential_scopes AS credential_scope \
                      ON credential_scope.agent_credential_id = agent.agent_credential_id \
                    WHERE agent.agent_credential_id = ? AND agent.owner_user_id = ? \
                      AND agent.created_at_ns <= ? \
                      AND agent.revoked_at_ns IS NULL \
                      AND (agent.expires_at_ns IS NULL OR agent.expires_at_ns > ?) \
                      AND credential_scope.scope = ?\
                 )",
            )
            .bind(credential_id.as_uuid().as_bytes().as_slice())
            .bind(user_id.as_uuid().as_bytes().as_slice())
            .bind(now)
            .bind(now)
            .bind(scope.as_str())
            .fetch_one(&mut **transaction)
            .await?;
            if !active_with_scope {
                return Err(AuthCommandError::ScopeEscalation.into());
            }
            user_id
        }
        AuditPrincipalReference::Offline {
            user_id: Some(user_id),
        } => user_id,
        AuditPrincipalReference::Offline { user_id: None }
        | AuditPrincipalReference::Unauthenticated => {
            return Err(AuthCommandError::ScopeEscalation.into());
        }
    };
    require_user_scope(transaction, user_id, scope).await
}

async fn require_user_management_authority(
    transaction: &mut Transaction<'_, Sqlite>,
    actor_user_id: UserId,
    target_user_id: UserId,
    scope: AdminScope,
) -> Result<StoredUser, AuthApplyError> {
    let actor = required_user(transaction, actor_user_id).await?;
    if actor.status != UserStatus::Enabled {
        return Err(AuthCommandError::NotFound.into());
    }
    let actor_scopes = actor.scopes();
    if !actor_scopes.contains(&scope) {
        return Err(AuthCommandError::ScopeEscalation.into());
    }
    let target = required_user(transaction, target_user_id).await?;
    if !target.scopes().is_subset(&actor_scopes) {
        return Err(AuthCommandError::ScopeEscalation.into());
    }
    Ok(target)
}

fn validate_roles(roles: &BTreeSet<UserRole>) -> Result<(), AuthApplyError> {
    if roles.is_empty() {
        Err(AuthCommandError::InvalidValue.into())
    } else {
        Ok(())
    }
}

fn validate_new_credentials(credentials: &[NewHumanCredential]) -> Result<(), AuthApplyError> {
    if credentials.len() > 2 {
        return Err(AuthCommandError::InvalidValue.into());
    }
    let password_count = credentials
        .iter()
        .filter(|credential| credential.provider() == HumanLoginProvider::Password)
        .count();
    let nostr_count = credentials.len() - password_count;
    if password_count > 1 || nostr_count > 1 {
        return Err(AuthCommandError::InvalidValue.into());
    }
    Ok(())
}

fn user_has_accepted_credential(user: &StoredUser, providers: ConfiguredLoginProviders) -> bool {
    (user.has_password && providers.accepts(HumanLoginProvider::Password))
        || (user.has_nostr && providers.accepts(HumanLoginProvider::Nostr))
}

async fn require_another_enabled_owner(
    transaction: &mut Transaction<'_, Sqlite>,
    excluded: UserId,
) -> Result<(), AuthApplyError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM users \
         JOIN user_roles ON user_roles.user_id = users.user_id \
         WHERE users.status = 'enabled' AND user_roles.role = 'owner' AND users.user_id <> ?",
    )
    .bind(excluded.as_uuid().as_bytes().as_slice())
    .fetch_one(&mut **transaction)
    .await?;
    if count > 0 {
        Ok(())
    } else {
        Err(AuthCommandError::LastEnabledOwner.into())
    }
}

struct CredentialState {
    version: u64,
    created_at: OffsetDateTime,
}

async fn credential_state(
    transaction: &mut Transaction<'_, Sqlite>,
    user_id: UserId,
    provider: HumanLoginProvider,
) -> Result<Option<CredentialState>, AuthApplyError> {
    let value: Option<(i64, i64)> =
        match provider {
            HumanLoginProvider::Password => sqlx::query_as(
                "SELECT version, created_at_ns FROM user_password_credentials WHERE user_id = ?",
            )
            .bind(user_id.as_uuid().as_bytes().as_slice())
            .fetch_optional(&mut **transaction)
            .await?,
            HumanLoginProvider::Nostr => {
                sqlx::query_as(
                    "SELECT version, created_at_ns FROM user_nostr_credentials WHERE user_id = ?",
                )
                .bind(user_id.as_uuid().as_bytes().as_slice())
                .fetch_optional(&mut **transaction)
                .await?
            }
        };
    value
        .map(|(version, created_at)| {
            Ok(CredentialState {
                version: positive_u64(version).ok_or(AuthApplyError::CorruptStoredState)?,
                created_at: timestamp(created_at).map_err(AuthApplyError::from_load)?,
            })
        })
        .transpose()
}

async fn insert_new_credential(
    transaction: &mut Transaction<'_, Sqlite>,
    user_id: UserId,
    credential: NewHumanCredential,
    timestamp: i64,
) -> Result<(), AuthApplyError> {
    let result = match credential {
        NewHumanCredential::Password {
            username,
            password_hash,
            policy_version,
        } => {
            if policy_version == 0 {
                return Err(AuthCommandError::InvalidValue.into());
            }
            sqlx::query(
                "INSERT INTO user_password_credentials (\
                    user_id, canonical_username, password_phc, policy_version, version, \
                    created_at_ns, updated_at_ns\
                 ) VALUES (?, ?, ?, ?, 1, ?, ?)",
            )
            .bind(user_id.as_uuid().as_bytes().as_slice())
            .bind(username.as_str())
            .bind(password_hash.as_str())
            .bind(i64::from(policy_version))
            .bind(timestamp)
            .bind(timestamp)
            .execute(&mut **transaction)
            .await
        }
        NewHumanCredential::Nostr { public_key } => {
            sqlx::query(
                "INSERT INTO user_nostr_credentials (\
                    user_id, public_key, version, created_at_ns, updated_at_ns\
                 ) VALUES (?, ?, 1, ?, ?)",
            )
            .bind(user_id.as_uuid().as_bytes().as_slice())
            .bind(public_key.as_bytes().as_slice())
            .bind(timestamp)
            .bind(timestamp)
            .execute(&mut **transaction)
            .await
        }
    };
    map_conflict(result)?;
    Ok(())
}

async fn update_credential(
    transaction: &mut Transaction<'_, Sqlite>,
    user_id: UserId,
    credential: NewHumanCredential,
    expected_version: u64,
    next_version: u64,
    timestamp: i64,
) -> Result<(), AuthApplyError> {
    let result =
        match credential {
            NewHumanCredential::Password {
                username,
                password_hash,
                policy_version,
            } => {
                if policy_version == 0 {
                    return Err(AuthCommandError::InvalidValue.into());
                }
                sqlx::query(
                "UPDATE user_password_credentials SET canonical_username = ?, password_phc = ?, \
                    policy_version = ?, version = ?, updated_at_ns = ? \
                 WHERE user_id = ? AND version = ?",
            )
            .bind(username.as_str())
            .bind(password_hash.as_str())
            .bind(i64::from(policy_version))
            .bind(version_i64(next_version)?)
            .bind(timestamp)
            .bind(user_id.as_uuid().as_bytes().as_slice())
            .bind(version_i64(expected_version)?)
            .execute(&mut **transaction)
            .await
            }
            NewHumanCredential::Nostr { public_key } => sqlx::query(
                "UPDATE user_nostr_credentials SET public_key = ?, version = ?, updated_at_ns = ? \
                 WHERE user_id = ? AND version = ?",
            )
            .bind(public_key.as_bytes().as_slice())
            .bind(version_i64(next_version)?)
            .bind(timestamp)
            .bind(user_id.as_uuid().as_bytes().as_slice())
            .bind(version_i64(expected_version)?)
            .execute(&mut **transaction)
            .await,
        };
    let result = map_conflict(result)?;
    require_one_row(result.rows_affected())
}

enum ReplayPrincipal {
    Human(UserId),
    Agent(AgentCredentialId),
}

async fn insert_replay_event(
    transaction: &mut Transaction<'_, Sqlite>,
    event_id: Nip98EventId,
    principal: ReplayPrincipal,
    accepted_at: OffsetDateTime,
    proof_created_at: OffsetDateTime,
) -> Result<(), AuthApplyError> {
    let freshness = i64::try_from(NIP98_FRESHNESS_SECONDS)
        .map(Duration::seconds)
        .map_err(|_| AuthCommandError::InvalidValue)?;
    if (accepted_at - proof_created_at).abs() > freshness {
        return Err(AuthCommandError::InvalidValue.into());
    }
    let replay_anchor = accepted_at.max(proof_created_at);
    let expires_at = replay_anchor
        .checked_add(freshness)
        .ok_or(AuthCommandError::InvalidValue)?;
    sqlx::query(
        "DELETE FROM nip98_replay_events WHERE event_id IN (\
            SELECT event_id FROM nip98_replay_events WHERE expires_at_ns <= ? \
            ORDER BY expires_at_ns, event_id LIMIT ?\
         )",
    )
    .bind(command_timestamp(accepted_at)?)
    .bind(MAX_AUTH_CLEANUP_ROWS)
    .execute(&mut **transaction)
    .await?;
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM nip98_replay_events WHERE expires_at_ns > ?")
            .bind(command_timestamp(accepted_at)?)
            .fetch_one(&mut **transaction)
            .await?;
    if live >= MAX_LIVE_REPLAY_EVENTS {
        return Err(AuthCommandError::ReplayCapacity.into());
    }
    let (kind, user, agent) = match principal {
        ReplayPrincipal::Human(user_id) => ("human_nostr", Some(user_id), None),
        ReplayPrincipal::Agent(credential_id) => ("agent_credential", None, Some(credential_id)),
    };
    let result = sqlx::query(
        "INSERT INTO nip98_replay_events (\
            event_id, principal_kind, user_id, agent_credential_id, accepted_at_ns, expires_at_ns\
         ) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(event_id.as_bytes().as_slice())
    .bind(kind)
    .bind(user.map(|value| value.into_uuid().as_bytes().to_vec()))
    .bind(agent.map(|value| value.into_uuid().as_bytes().to_vec()))
    .bind(command_timestamp(accepted_at)?)
    .bind(command_timestamp(expires_at)?)
    .execute(&mut **transaction)
    .await;
    match result {
        Ok(_) => Ok(()),
        Err(error) if is_unique_violation(&error) => Err(AuthCommandError::ReplayedProof.into()),
        Err(error) => Err(error.into()),
    }
}

async fn cleanup_login_challenges(
    transaction: &mut Transaction<'_, Sqlite>,
    now: OffsetDateTime,
) -> Result<(), AuthApplyError> {
    sqlx::query(
        "DELETE FROM login_challenges WHERE challenge_id IN (\
            SELECT challenge_id FROM login_challenges \
            WHERE consumed_at_ns IS NOT NULL OR expires_at_ns <= ? \
            ORDER BY expires_at_ns, challenge_id LIMIT ?\
         )",
    )
    .bind(command_timestamp(now)?)
    .bind(MAX_AUTH_CLEANUP_ROWS)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn cleanup_browser_sessions(
    transaction: &mut Transaction<'_, Sqlite>,
    now: OffsetDateTime,
) -> Result<(), AuthApplyError> {
    sqlx::query(
        "DELETE FROM browser_sessions WHERE session_id IN (\
            SELECT session_id FROM browser_sessions \
            WHERE revoked_at_ns IS NOT NULL OR expires_at_ns <= ? \
            ORDER BY expires_at_ns, session_id LIMIT ?\
         )",
    )
    .bind(command_timestamp(now)?)
    .bind(MAX_AUTH_CLEANUP_ROWS)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn revoke_user_sessions(
    transaction: &mut Transaction<'_, Sqlite>,
    user_id: UserId,
    revoked_at: OffsetDateTime,
) -> Result<(), AuthApplyError> {
    sqlx::query(
        "UPDATE browser_sessions \
         SET revoked_at_ns = max(?, authenticated_at_ns), version = version + 1 \
         WHERE user_id = ? AND revoked_at_ns IS NULL",
    )
    .bind(command_timestamp(revoked_at)?)
    .bind(user_id.as_uuid().as_bytes().as_slice())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn revoke_user_agents(
    transaction: &mut Transaction<'_, Sqlite>,
    user_id: UserId,
    revoked_at: OffsetDateTime,
) -> Result<(), AuthApplyError> {
    sqlx::query(
        "UPDATE agent_credentials \
         SET revoked_at_ns = max(?, created_at_ns, coalesce(last_used_at_ns, created_at_ns)), \
             version = version + 1 \
         WHERE owner_user_id = ? AND revoked_at_ns IS NULL",
    )
    .bind(command_timestamp(revoked_at)?)
    .bind(user_id.as_uuid().as_bytes().as_slice())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn require_scope_subset(
    transaction: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    issuer: UserId,
    requested: &BTreeSet<AdminScope>,
) -> Result<(), AuthApplyError> {
    let owner = required_user(transaction, owner).await?;
    let issuer = required_user(transaction, issuer).await?;
    if owner.status != UserStatus::Enabled || issuer.status != UserStatus::Enabled {
        return Err(AuthCommandError::NotFound.into());
    }
    let owner_scopes = owner.scopes();
    let issuer_scopes = issuer.scopes();
    if requested.is_subset(&owner_scopes) && requested.is_subset(&issuer_scopes) {
        Ok(())
    } else {
        Err(AuthCommandError::ScopeEscalation.into())
    }
}

async fn insert_agent_scopes(
    transaction: &mut Transaction<'_, Sqlite>,
    credential_id: AgentCredentialId,
    scopes: &BTreeSet<AdminScope>,
) -> Result<(), AuthApplyError> {
    for scope in scopes {
        sqlx::query(
            "INSERT INTO agent_credential_scopes (agent_credential_id, scope) VALUES (?, ?)",
        )
        .bind(credential_id.as_uuid().as_bytes().as_slice())
        .bind(scope.as_str())
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

async fn classify_agent_version(
    transaction: &mut Transaction<'_, Sqlite>,
    credential_id: AgentCredentialId,
    expected_version: u64,
) -> Result<AuthCommandError, AuthApplyError> {
    let row: Option<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT version, revoked_at_ns FROM agent_credentials WHERE agent_credential_id = ?",
    )
    .bind(credential_id.as_uuid().as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await?;
    match row {
        None => Ok(AuthCommandError::NotFound),
        Some((version, _))
            if positive_u64(version).ok_or(AuthApplyError::CorruptStoredState)?
                != expected_version =>
        {
            Ok(AuthCommandError::StaleVersion)
        }
        Some(_) => Ok(AuthCommandError::NotFound),
    }
}

fn validate_agent_label(label: &str) -> Result<(), AuthApplyError> {
    if label.is_empty()
        || label.len() > MAX_AGENT_LABEL_BYTES
        || label.trim() != label
        || label.chars().any(char::is_control)
    {
        Err(AuthCommandError::InvalidValue.into())
    } else {
        Ok(())
    }
}

fn validate_audit_text(value: &str, maximum: usize) -> Result<(), AuthApplyError> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        Err(AuthCommandError::InvalidValue.into())
    } else {
        Ok(())
    }
}

fn require_version(actual: u64, expected: u64) -> Result<(), AuthApplyError> {
    if actual == expected {
        Ok(())
    } else {
        Err(AuthCommandError::StaleVersion.into())
    }
}

fn require_one_row(rows: u64) -> Result<(), AuthApplyError> {
    if rows == 1 {
        Ok(())
    } else {
        Err(AuthCommandError::StaleVersion.into())
    }
}

fn checked_next_version(version: u64) -> Result<u64, AuthApplyError> {
    version
        .checked_add(1)
        .filter(|version| i64::try_from(*version).is_ok())
        .ok_or_else(|| AuthCommandError::InvalidValue.into())
}

fn version_i64(version: u64) -> Result<i64, AuthApplyError> {
    i64::try_from(version).map_err(|_| AuthCommandError::InvalidValue.into())
}

fn command_timestamp(value: OffsetDateTime) -> Result<i64, AuthApplyError> {
    i64::try_from(value.unix_timestamp_nanos()).map_err(|_| AuthCommandError::InvalidValue.into())
}

fn map_conflict(
    result: Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error>,
) -> Result<sqlx::sqlite::SqliteQueryResult, AuthApplyError> {
    match result {
        Ok(result) => Ok(result),
        Err(error) if is_unique_violation(&error) => Err(AuthCommandError::Conflict.into()),
        Err(error) => Err(error.into()),
    }
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
}

#[derive(Debug, Error)]
pub(crate) enum AuthApplyError {
    #[error(transparent)]
    Command(#[from] AuthCommandError),
    #[error("authentication persistence operation failed")]
    Operation(#[from] sqlx::Error),
    #[error("stored authentication state is invalid")]
    CorruptStoredState,
}

impl AuthApplyError {
    fn from_load(error: AuthLoadError) -> Self {
        match error {
            AuthLoadError::Query(source) => Self::Operation(source),
            _ => Self::CorruptStoredState,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        config::{DatabaseBusyTimeout, DatabaseReadPoolSize, DatabaseWriterQueueCapacity},
        database,
        domain::auth::Argon2idPolicy,
    };

    const OWNER_KEY: &str = "63fe6318dc58583cfe16810f86dd09e18bfd76aabc24a0081ce2856f330504ed";
    const PUBLISHER_KEY: &str = "c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5";
    const AGENT_KEY: &str = "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9";

    struct Harness {
        _root: tempfile::TempDir,
        path: PathBuf,
        store: crate::database::store::DatabaseStore,
        shutdown: CancellationToken,
        writer: JoinHandle<()>,
    }

    impl Harness {
        async fn start() -> Self {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("state/maincopy.db");
            let database = database::bootstrap(configuration(&path)).await.unwrap();
            let (store, writer) = database.into_store(32);
            let shutdown = CancellationToken::new();
            let task_shutdown = shutdown.clone();
            let task = tokio::spawn(async move {
                writer.run(task_shutdown).await.unwrap();
            });
            Self {
                _root: root,
                path,
                store,
                shutdown,
                writer: task,
            }
        }

        async fn stop(self) {
            self.shutdown.cancel();
            self.writer.await.unwrap();
        }

        async fn restart(self) -> Self {
            let Self {
                _root,
                path,
                store,
                shutdown,
                writer,
            } = self;
            drop(store);
            shutdown.cancel();
            writer.await.unwrap();
            let database = database::bootstrap(configuration(&path)).await.unwrap();
            let (store, database_writer) = database.into_store(32);
            let shutdown = CancellationToken::new();
            let task_shutdown = shutdown.clone();
            let writer = tokio::spawn(async move {
                database_writer.run(task_shutdown).await.unwrap();
            });
            Self {
                _root,
                path,
                store,
                shutdown,
                writer,
            }
        }
    }

    fn configuration(path: &Path) -> crate::config::DatabaseConfigurationView<'_> {
        crate::config::DatabaseConfigurationView {
            path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1_000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(32).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        }
    }

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    fn uuid(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn instance(value: u128) -> InstanceId {
        InstanceId::from_uuid(uuid(value))
    }

    fn user(value: u128) -> UserId {
        UserId::from_uuid(uuid(value))
    }

    fn session(value: u128) -> AdminSessionId {
        AdminSessionId::from_uuid(uuid(value))
    }

    fn challenge(value: u128) -> LoginChallengeId {
        LoginChallengeId::from_uuid(uuid(value))
    }

    fn agent(value: u128) -> AgentCredentialId {
        AgentCredentialId::from_uuid(uuid(value))
    }

    fn audit(value: u128) -> AdminAuditEventId {
        AdminAuditEventId::from_uuid(uuid(value))
    }

    fn mutation_audit(actor: UserId, value: u128) -> MutationAuditContext {
        MutationAuditContext {
            audit_event_id: audit(0x1_0000 + value),
            principal: AuditPrincipalReference::Offline {
                user_id: Some(actor),
            },
            request_id: Some(uuid(0x3_0000 + value)),
            idempotency_key: AdminMutationKey(uuid(0x4_0000 + value)),
        }
    }

    fn session_audit(value: u128) -> SessionAuditContext {
        SessionAuditContext {
            audit_event_id: audit(0x5_0000 + value),
            request_id: Some(uuid(0x6_0000 + value)),
        }
    }

    fn providers() -> ConfiguredLoginProviders {
        ConfiguredLoginProviders::new(true, true).unwrap()
    }

    fn password_credential() -> NewHumanCredential {
        NewHumanCredential::Password {
            username: CanonicalUsername::parse("owner").unwrap(),
            password_hash: Argon2idPolicy::v1()
                .hash_password("correct horse battery staple")
                .unwrap(),
            policy_version: 1,
        }
    }

    fn nostr_credential(key: &str) -> NewHumanCredential {
        NewHumanCredential::Nostr {
            public_key: NostrPublicKey::parse(key).unwrap(),
        }
    }

    fn bootstrap_with(owner: UserId, credential: NewHumanCredential) -> BootstrapIdentity {
        BootstrapIdentity {
            instance_id: instance(1),
            owner_user_id: owner,
            credential,
            configured_providers: providers(),
            occurred_at: at(10),
            audit_event_id: audit(2),
        }
    }

    fn assert_command_error<T>(result: Result<T, AuthMutationError>, expected: AuthCommandError) {
        assert!(
            matches!(result, Err(AuthMutationError::Command(actual)) if actual == expected),
            "expected {expected:?}"
        );
    }

    #[tokio::test]
    async fn bootstrap_is_atomic_single_use_and_queryable_without_secrets() {
        let harness = Harness::start().await;
        assert!(
            harness
                .store
                .auth
                .identity_state()
                .await
                .unwrap()
                .bootstrap_required
        );
        let owner = user(10);

        let result = harness
            .store
            .auth
            .bootstrap_identity(bootstrap_with(owner, password_credential()))
            .await
            .unwrap();
        assert_eq!(result.owner_user_id, owner);
        assert_eq!(result.instance.instance_id, instance(1));

        let state = harness.store.auth.identity_state().await.unwrap();
        assert!(!state.bootstrap_required);
        assert_eq!(state.instance.unwrap().version, 1);
        let stored = harness.store.auth.user(owner).await.unwrap().unwrap();
        assert_eq!(stored.status, UserStatus::Enabled);
        assert_eq!(stored.roles, BTreeSet::from([UserRole::Owner]));
        assert!(stored.has_password);
        assert!(!stored.has_nostr);
        let login = harness
            .store
            .auth
            .password_login(&CanonicalUsername::parse("owner").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(login.user_id, owner);
        assert_eq!(login.credential_version, 1);
        assert_eq!(
            format!("{:?}", login.password_hash),
            "StoredPasswordHash(<redacted>)"
        );

        assert_command_error(
            harness
                .store
                .auth
                .bootstrap_identity(BootstrapIdentity {
                    instance_id: instance(99),
                    owner_user_id: user(99),
                    credential: nostr_credential(PUBLISHER_KEY),
                    configured_providers: providers(),
                    occurred_at: at(11),
                    audit_event_id: audit(99),
                })
                .await,
            AuthCommandError::AlreadyBootstrapped,
        );

        harness.stop().await;
    }

    #[tokio::test]
    async fn enabled_users_keep_a_configured_credential_and_the_last_owner() {
        let harness = Harness::start().await;
        let owner = user(20);
        harness
            .store
            .auth
            .bootstrap_identity(bootstrap_with(owner, password_credential()))
            .await
            .unwrap();
        assert!(matches!(
            harness
                .store
                .auth
                .validate_provider_compatibility(
                    ConfiguredLoginProviders::new(false, true).unwrap()
                )
                .await,
            Err(AuthLoadError::EnabledUserWithoutConfiguredCredential { user_id })
                if user_id == owner
        ));

        assert_command_error(
            harness
                .store
                .auth
                .remove_human_credential(RemoveHumanCredential {
                    user_id: owner,
                    managed_by_user_id: owner,
                    kind: HumanCredentialKind::Password,
                    expected_version: 1,
                    configured_providers: providers(),
                    occurred_at: at(11),
                    audit: mutation_audit(owner, 1),
                })
                .await,
            AuthCommandError::EnabledUserRequiresCredential,
        );

        harness
            .store
            .auth
            .put_human_credential(PutHumanCredential {
                user_id: owner,
                managed_by_user_id: owner,
                credential: nostr_credential(OWNER_KEY),
                expected_version: None,
                occurred_at: at(12),
                audit: mutation_audit(owner, 2),
            })
            .await
            .unwrap();
        harness
            .store
            .auth
            .validate_provider_compatibility(ConfiguredLoginProviders::new(false, true).unwrap())
            .await
            .unwrap();
        harness
            .store
            .auth
            .remove_human_credential(RemoveHumanCredential {
                user_id: owner,
                managed_by_user_id: owner,
                kind: HumanCredentialKind::Password,
                expected_version: 1,
                configured_providers: providers(),
                occurred_at: at(13),
                audit: mutation_audit(owner, 3),
            })
            .await
            .unwrap();
        assert!(
            harness
                .store
                .auth
                .password_login(&CanonicalUsername::parse("owner").unwrap())
                .await
                .unwrap()
                .is_none()
        );

        assert_command_error(
            harness
                .store
                .auth
                .replace_user_roles(ReplaceUserRoles {
                    user_id: owner,
                    expected_version: 3,
                    roles: BTreeSet::from([UserRole::Publisher]),
                    assigned_by_user_id: owner,
                    occurred_at: at(14),
                    audit: mutation_audit(owner, 4),
                })
                .await,
            AuthCommandError::LastEnabledOwner,
        );

        let second_owner = user(21);
        harness
            .store
            .auth
            .create_user(CreateUser {
                user_id: second_owner,
                created_by_user_id: owner,
                status: UserStatus::Enabled,
                roles: BTreeSet::from([UserRole::Owner]),
                credentials: vec![nostr_credential(PUBLISHER_KEY)],
                configured_providers: providers(),
                occurred_at: at(15),
                audit: mutation_audit(owner, 5),
            })
            .await
            .unwrap();
        let demoted = harness
            .store
            .auth
            .replace_user_roles(ReplaceUserRoles {
                user_id: owner,
                expected_version: 3,
                roles: BTreeSet::from([UserRole::Publisher]),
                assigned_by_user_id: second_owner,
                occurred_at: at(16),
                audit: mutation_audit(second_owner, 6),
            })
            .await
            .unwrap();
        assert_eq!(demoted.version, 4);

        harness.stop().await;
    }

    #[tokio::test]
    async fn credential_mutation_retries_return_the_durable_result_without_replacing_the_secret() {
        let harness = Harness::start().await;
        let owner = user(25);
        harness
            .store
            .auth
            .bootstrap_identity(bootstrap_with(owner, password_credential()))
            .await
            .unwrap();

        let first_hash = Argon2idPolicy::v1()
            .hash_password("first replacement password")
            .unwrap();
        let first_phc = first_hash.as_str().to_owned();
        let first = harness
            .store
            .auth
            .put_human_credential(PutHumanCredential {
                user_id: owner,
                managed_by_user_id: owner,
                credential: NewHumanCredential::Password {
                    username: CanonicalUsername::parse("owner").unwrap(),
                    password_hash: first_hash,
                    policy_version: 1,
                },
                expected_version: Some(1),
                occurred_at: at(11),
                audit: mutation_audit(owner, 50),
            })
            .await
            .unwrap();
        assert_eq!(first.version, 2);

        let harness = harness.restart().await;
        let replay = harness
            .store
            .auth
            .put_human_credential(PutHumanCredential {
                user_id: owner,
                managed_by_user_id: owner,
                credential: NewHumanCredential::Password {
                    username: CanonicalUsername::parse("owner").unwrap(),
                    password_hash: Argon2idPolicy::v1()
                        .hash_password("different retry password")
                        .unwrap(),
                    policy_version: 1,
                },
                expected_version: Some(1),
                occurred_at: at(12),
                audit: mutation_audit(owner, 50),
            })
            .await
            .unwrap();
        assert_eq!(replay, first);
        let login = harness
            .store
            .auth
            .password_login(&CanonicalUsername::parse("owner").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(login.password_hash.as_str(), first_phc);

        let events = harness
            .store
            .auth
            .audit_events_page(None, 10)
            .await
            .unwrap()
            .items;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.action.as_ref() == "identity.user.credential.put")
                .count(),
            1
        );

        assert_command_error(
            harness
                .store
                .auth
                .put_human_credential(PutHumanCredential {
                    user_id: owner,
                    managed_by_user_id: owner,
                    credential: NewHumanCredential::Password {
                        username: CanonicalUsername::parse("changed-owner").unwrap(),
                        password_hash: Argon2idPolicy::v1()
                            .hash_password("different retry password")
                            .unwrap(),
                        policy_version: 1,
                    },
                    expected_version: Some(1),
                    occurred_at: at(13),
                    audit: mutation_audit(owner, 50),
                })
                .await,
            AuthCommandError::IdempotencyConflict,
        );

        harness.stop().await;
    }

    #[tokio::test]
    async fn session_creation_consumes_nostr_challenges_and_rejects_stale_or_replayed_proofs() {
        let harness = Harness::start().await;
        let owner = user(30);
        harness
            .store
            .auth
            .bootstrap_identity(bootstrap_with(owner, nostr_credential(OWNER_KEY)))
            .await
            .unwrap();

        let first_challenge = challenge(31);
        harness
            .store
            .auth
            .create_login_challenge(CreateLoginChallenge {
                challenge_id: first_challenge,
                provider: HumanLoginProvider::Nostr,
                challenge_digest: LoginChallengeDigest::from_bytes([0x31; 32]),
                created_at: at(11),
                expires_at: at(20),
            })
            .await
            .unwrap();
        let token_digest = SessionTokenDigest::from_bytes([0x41; 32]);
        let event_id = Nip98EventId::from_bytes([0x51; 32]);
        harness
            .store
            .auth
            .create_browser_session(CreateBrowserSession {
                session_id: session(32),
                user_id: owner,
                expected_user_version: 1,
                session_token_digest: token_digest,
                csrf_token_digest: CsrfTokenDigest::from_bytes([0x42; 32]),
                evidence: SessionAuthenticationEvidence::Nostr {
                    expected_credential_version: 1,
                    challenge_id: first_challenge,
                    challenge_digest: LoginChallengeDigest::from_bytes([0x31; 32]),
                    event_id,
                    proof_created_at: at(12),
                },
                authenticated_at: at(12),
                fresh_until: at(13),
                expires_at: at(50),
                audit: session_audit(32),
            })
            .await
            .unwrap();
        let stored = harness
            .store
            .auth
            .browser_session(token_digest)
            .await
            .unwrap()
            .unwrap();
        assert!(stored.is_active_at(at(20)));
        assert!(stored.scopes().contains(&AdminScope::AuditRead));
        assert_eq!(
            harness
                .store
                .auth
                .login_challenge(first_challenge)
                .await
                .unwrap()
                .unwrap()
                .consumed_at,
            Some(at(12))
        );

        let replay_challenge = challenge(33);
        harness
            .store
            .auth
            .create_login_challenge(CreateLoginChallenge {
                challenge_id: replay_challenge,
                provider: HumanLoginProvider::Nostr,
                challenge_digest: LoginChallengeDigest::from_bytes([0x32; 32]),
                created_at: at(13),
                expires_at: at(25),
            })
            .await
            .unwrap();
        assert_command_error(
            harness
                .store
                .auth
                .create_browser_session(CreateBrowserSession {
                    session_id: session(34),
                    user_id: owner,
                    expected_user_version: 1,
                    session_token_digest: SessionTokenDigest::from_bytes([0x43; 32]),
                    csrf_token_digest: CsrfTokenDigest::from_bytes([0x44; 32]),
                    evidence: SessionAuthenticationEvidence::Nostr {
                        expected_credential_version: 1,
                        challenge_id: replay_challenge,
                        challenge_digest: LoginChallengeDigest::from_bytes([0x32; 32]),
                        event_id,
                        proof_created_at: at(14),
                    },
                    authenticated_at: at(14),
                    fresh_until: at(15),
                    expires_at: at(50),
                    audit: session_audit(34),
                })
                .await,
            AuthCommandError::ReplayedProof,
        );
        assert_eq!(
            harness
                .store
                .auth
                .login_challenge(replay_challenge)
                .await
                .unwrap()
                .unwrap()
                .consumed_at,
            None,
            "the failed replay transaction must not consume the challenge"
        );

        harness
            .store
            .auth
            .put_human_credential(PutHumanCredential {
                user_id: owner,
                managed_by_user_id: owner,
                credential: nostr_credential(OWNER_KEY),
                expected_version: Some(1),
                occurred_at: at(16),
                audit: mutation_audit(owner, 7),
            })
            .await
            .unwrap();
        let stale_challenge = challenge(35);
        harness
            .store
            .auth
            .create_login_challenge(CreateLoginChallenge {
                challenge_id: stale_challenge,
                provider: HumanLoginProvider::Nostr,
                challenge_digest: LoginChallengeDigest::from_bytes([0x33; 32]),
                created_at: at(17),
                expires_at: at(30),
            })
            .await
            .unwrap();
        assert_command_error(
            harness
                .store
                .auth
                .create_browser_session(CreateBrowserSession {
                    session_id: session(36),
                    user_id: owner,
                    expected_user_version: 1,
                    session_token_digest: SessionTokenDigest::from_bytes([0x45; 32]),
                    csrf_token_digest: CsrfTokenDigest::from_bytes([0x46; 32]),
                    evidence: SessionAuthenticationEvidence::Nostr {
                        expected_credential_version: 1,
                        challenge_id: stale_challenge,
                        challenge_digest: LoginChallengeDigest::from_bytes([0x33; 32]),
                        event_id: Nip98EventId::from_bytes([0x52; 32]),
                        proof_created_at: at(18),
                    },
                    authenticated_at: at(18),
                    fresh_until: at(19),
                    expires_at: at(50),
                    audit: session_audit(36),
                })
                .await,
            AuthCommandError::StaleVersion,
        );

        harness.stop().await;
    }

    #[tokio::test]
    async fn agent_scopes_cannot_escalate_and_proof_admission_is_version_bound() {
        let harness = Harness::start().await;
        let owner = user(40);
        harness
            .store
            .auth
            .bootstrap_identity(bootstrap_with(owner, nostr_credential(OWNER_KEY)))
            .await
            .unwrap();
        let publisher = user(41);
        harness
            .store
            .auth
            .create_user(CreateUser {
                user_id: publisher,
                created_by_user_id: owner,
                status: UserStatus::Enabled,
                roles: BTreeSet::from([UserRole::Publisher]),
                credentials: vec![nostr_credential(PUBLISHER_KEY)],
                configured_providers: providers(),
                occurred_at: at(11),
                audit: mutation_audit(owner, 8),
            })
            .await
            .unwrap();
        let credential_id = agent(42);
        assert_command_error(
            harness
                .store
                .auth
                .register_agent_credential(RegisterAgentCredential {
                    credential_id,
                    owner_user_id: publisher,
                    issuer_user_id: owner,
                    public_key: NostrPublicKey::parse(AGENT_KEY).unwrap(),
                    label: "publishing agent".into(),
                    scopes: BTreeSet::from([AdminScope::UserManage]),
                    created_at: at(12),
                    expires_at: None,
                    audit: mutation_audit(owner, 9),
                })
                .await,
            AuthCommandError::ScopeEscalation,
        );
        harness
            .store
            .auth
            .register_agent_credential(RegisterAgentCredential {
                credential_id,
                owner_user_id: publisher,
                issuer_user_id: owner,
                public_key: NostrPublicKey::parse(AGENT_KEY).unwrap(),
                label: "publishing agent".into(),
                scopes: BTreeSet::from([AdminScope::PreviewRead, AdminScope::ReleaseManage]),
                created_at: at(12),
                expires_at: None,
                audit: mutation_audit(owner, 10),
            })
            .await
            .unwrap();
        let stored = harness
            .store
            .auth
            .agent_credential(&NostrPublicKey::parse(AGENT_KEY).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.owner_user_id, publisher);
        assert!(
            stored
                .effective_scopes()
                .contains(&AdminScope::ReleaseManage)
        );
        assert!(!stored.effective_scopes().contains(&AdminScope::UserManage));

        let event = Nip98EventId::from_bytes([0x61; 32]);
        harness
            .store
            .auth
            .accept_agent_proof(AcceptAgentProof {
                credential_id,
                expected_credential_version: 1,
                expected_owner_version: 1,
                event_id: event,
                accepted_at: at(13),
                proof_created_at: at(13),
            })
            .await
            .unwrap();
        assert_command_error(
            harness
                .store
                .auth
                .accept_agent_proof(AcceptAgentProof {
                    credential_id,
                    expected_credential_version: 1,
                    expected_owner_version: 1,
                    event_id: event,
                    accepted_at: at(14),
                    proof_created_at: at(14),
                })
                .await,
            AuthCommandError::ReplayedProof,
        );

        harness
            .store
            .auth
            .replace_user_roles(ReplaceUserRoles {
                user_id: publisher,
                expected_version: 1,
                roles: BTreeSet::from([UserRole::Administrator]),
                assigned_by_user_id: owner,
                occurred_at: at(15),
                audit: mutation_audit(owner, 11),
            })
            .await
            .unwrap();
        assert_command_error(
            harness
                .store
                .auth
                .accept_agent_proof(AcceptAgentProof {
                    credential_id,
                    expected_credential_version: 1,
                    expected_owner_version: 1,
                    event_id: Nip98EventId::from_bytes([0x62; 32]),
                    accepted_at: at(16),
                    proof_created_at: at(16),
                })
                .await,
            AuthCommandError::StaleVersion,
        );

        harness.stop().await;
    }
}
