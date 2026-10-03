//! Channel settings, first-publication teasers, and the delivery ledger. A
//! delivery is claimed before any provider request and leaves `sending` only
//! with a recorded outcome, so no restart can post it twice.

use markdown_compiler::PostId;
use sqlx::{Row as _, Sqlite, SqlitePool, Transaction, sqlite::SqliteRow};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{
    settings::{
        Channel, StoredChannel, SubstackSettings, UpdateSubstack, UpdateX, XSettings,
        stored_substack, stored_x,
    },
    teaser::Teaser,
};
use crate::{
    database::{
        fingerprint::CommandFingerprintBuilder,
        store::{DatabaseAdmissionError, DatabaseCommandError, Mutation, MutationSender},
    },
    domain::{
        auth::store::{
            AuthApplyError, AuthCommandError, MutationAuditContext, append_success_audit,
            decode_audit_principal, require_fresh_browser_owner,
        },
        publication::{CanonicalPublicationView, store::PublicationMutationError},
    },
};

const UPDATE_SUBSTACK_ACTION: &str = "sharing.substack.update";
const UPDATE_X_ACTION: &str = "sharing.x.update";
const SHARE_TEASER_ACTION: &str = "sharing.teaser.share";
const MAX_RECEIPTS: i64 = 10_000;
/// Transient provider failures are retried with doubling waits for about an hour.
const MAX_ATTEMPTS: i64 = 6;
const BASE_RETRY_SECONDS: i64 = 60;
const MAX_POSTED_URL_BYTES: usize = 2048;

#[derive(Clone)]
pub(crate) struct SharingStore {
    readers: SqlitePool,
    mutations: MutationSender,
}

/// Why a delivery stopped without a post. The admin page explains each one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryFailure {
    /// The server stopped mid-request, so the provider's answer is unknown.
    Interrupted,
    Refused,
    UnexpectedResponse,
    RetriesExhausted,
}

impl DeliveryFailure {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Interrupted => "interrupted",
            Self::Refused => "refused",
            Self::UnexpectedResponse => "unexpected_response",
            Self::RetriesExhausted => "retries_exhausted",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [
            Self::Interrupted,
            Self::Refused,
            Self::UnexpectedResponse,
            Self::RetriesExhausted,
        ]
        .into_iter()
        .find(|failure| failure.as_str() == value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryState {
    Queued { retry_after: OffsetDateTime },
    Sending,
    Posted { url: String },
    Failed(DeliveryFailure),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Delivery {
    pub channel: Channel,
    pub state: DeliveryState,
}

/// A teaser with every channel it was queued for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SharedTeaser {
    pub teaser: Teaser,
    pub created_at: OffsetDateTime,
    pub deliveries: Vec<Delivery>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DueDelivery {
    pub teaser: Teaser,
    /// A Substack draft an earlier attempt created and did not publish.
    pub draft: Option<u64>,
}

/// Queue a teaser for a channel it missed, or retry one that failed.
pub(crate) struct ShareTeaser {
    pub post_id: PostId,
    pub channel: Channel,
    pub audit: MutationAuditContext,
}

pub(crate) struct ClaimDelivery {
    pub post_id: PostId,
    pub channel: Channel,
    /// The claim is refused once the Owner pauses or replaces these settings.
    pub settings_version: u64,
}

pub(crate) struct FinishDelivery {
    pub post_id: PostId,
    pub channel: Channel,
    pub settings_version: u64,
    pub outcome: DeliveryOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryOutcome {
    Posted {
        url: String,
    },
    /// Nothing was posted. A created Substack draft is kept for the retry.
    Retry {
        draft: Option<u64>,
    },
    Failed(DeliveryFailure),
    /// The provider refused the credentials. The teaser waits, uncounted,
    /// until an Owner saves new ones.
    CredentialsRejected {
        draft: Option<u64>,
    },
}

#[derive(Debug, Error)]
pub(crate) enum SharingLoadError {
    #[error("sharing query failed")]
    Operation(#[from] sqlx::Error),
    #[error("stored sharing state failed validation")]
    CorruptStoredState,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SharingCommandError {
    #[error("a freshly signed-in Owner is required")]
    Forbidden,
    #[error("the sharing settings changed")]
    StaleVersion,
    #[error("the idempotency key is already bound to a different command")]
    IdempotencyConflict,
    #[error("the sharing command contains an invalid value")]
    InvalidValue,
    #[error("the teaser does not exist")]
    NotFound,
    #[error("the delivery is not in the required state")]
    StateConflict,
    #[error("the sharing history limit has been reached")]
    Capacity,
    #[error("the sharing command outcome is unknown")]
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SharingMutationError {
    #[error(transparent)]
    Admission(#[from] DatabaseAdmissionError),
    #[error(transparent)]
    Command(#[from] SharingCommandError),
}

#[derive(Debug, Error)]
pub(crate) enum SharingApplyError {
    #[error(transparent)]
    Command(#[from] SharingCommandError),
    #[error("sharing database operation failed")]
    Operation(#[from] sqlx::Error),
    #[error("stored sharing state failed validation")]
    CorruptStoredState,
}

impl SharingStore {
    pub(crate) fn new(readers: SqlitePool, mutations: mpsc::Sender<Mutation>) -> Self {
        Self {
            readers,
            mutations: MutationSender::new(mutations),
        }
    }

    pub(crate) async fn substack(
        &self,
    ) -> Result<Option<StoredChannel<SubstackSettings>>, SharingLoadError> {
        let row = sqlx::query(
            "SELECT version,mode,credentials_rejected,subdomain,session_cookie \
             FROM sharing_substack WHERE singleton=1",
        )
        .fetch_optional(&self.readers)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let settings = stored_substack(
            row.try_get("mode")?,
            row.try_get("subdomain")?,
            row.try_get("session_cookie")?,
        )
        .ok_or(SharingLoadError::CorruptStoredState)?;
        stored_channel(&row, settings).map(Some)
    }

    pub(crate) async fn x(&self) -> Result<Option<StoredChannel<XSettings>>, SharingLoadError> {
        let row = sqlx::query(
            "SELECT version,mode,credentials_rejected,api_key,api_secret,access_token,\
             access_token_secret FROM sharing_x WHERE singleton=1",
        )
        .fetch_optional(&self.readers)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let settings = stored_x(
            row.try_get("mode")?,
            [
                row.try_get("api_key")?,
                row.try_get("api_secret")?,
                row.try_get("access_token")?,
                row.try_get("access_token_secret")?,
            ],
        )
        .ok_or(SharingLoadError::CorruptStoredState)?;
        stored_channel(&row, settings).map(Some)
    }

    /// The longest-waiting queued delivery whose retry time has passed.
    pub(crate) async fn due(
        &self,
        channel: Channel,
        now: OffsetDateTime,
    ) -> Result<Option<DueDelivery>, SharingLoadError> {
        let row = sqlx::query(
            "SELECT teaser.post_id,teaser.title,teaser.summary,teaser.url,delivery.draft \
             FROM sharing_deliveries AS delivery \
             JOIN sharing_teasers AS teaser ON teaser.post_id=delivery.post_id \
             WHERE delivery.channel=? AND delivery.state='queued' AND delivery.retry_after<=? \
             ORDER BY delivery.updated_at,delivery.post_id LIMIT 1",
        )
        .bind(channel.as_str())
        .bind(now.unix_timestamp())
        .fetch_optional(&self.readers)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let draft = row
            .try_get::<Option<i64>, _>("draft")?
            .map(|draft| u64::try_from(draft).map_err(|_| SharingLoadError::CorruptStoredState))
            .transpose()?;
        Ok(Some(DueDelivery {
            teaser: stored_teaser(&row)?,
            draft,
        }))
    }

    /// The newest teasers with their deliveries, read as one snapshot.
    pub(crate) async fn recent(&self, limit: u32) -> Result<Vec<SharedTeaser>, SharingLoadError> {
        let mut transaction = self.readers.begin().await?;
        let teasers = sqlx::query(
            "SELECT post_id,created_at,title,summary,url FROM sharing_teasers \
             ORDER BY created_at DESC,post_id LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&mut *transaction)
        .await?;
        let deliveries = sqlx::query(
            "SELECT post_id,channel,state,retry_after,posted_url,failure \
             FROM sharing_deliveries WHERE post_id IN (\
             SELECT post_id FROM sharing_teasers ORDER BY created_at DESC,post_id LIMIT ?)",
        )
        .bind(limit)
        .fetch_all(&mut *transaction)
        .await?;
        transaction.commit().await?;
        let deliveries = deliveries
            .iter()
            .map(|row| Ok((stored_post_id(row)?, stored_delivery(row)?)))
            .collect::<Result<Vec<_>, SharingLoadError>>()?;
        teasers
            .iter()
            .map(|row| {
                let teaser = stored_teaser(row)?;
                let created_at = OffsetDateTime::from_unix_timestamp(row.try_get("created_at")?)
                    .map_err(|_| SharingLoadError::CorruptStoredState)?;
                let deliveries = deliveries
                    .iter()
                    .filter(|(post_id, _)| post_id == teaser.view().post_id)
                    .map(|(_, delivery)| delivery.clone())
                    .collect();
                Ok(SharedTeaser {
                    teaser,
                    created_at,
                    deliveries,
                })
            })
            .collect()
    }

    pub(crate) async fn update_substack(
        &self,
        command: UpdateSubstack,
    ) -> Result<(), SharingMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::UpdateSharingSubstack {
                    command,
                    respond_to,
                },
                SharingCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn update_x(&self, command: UpdateX) -> Result<(), SharingMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::UpdateSharingX {
                    command,
                    respond_to,
                },
                SharingCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn share(&self, command: ShareTeaser) -> Result<(), SharingMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::ShareTeaser {
                    command,
                    respond_to,
                },
                SharingCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn claim(&self, command: ClaimDelivery) -> Result<(), SharingMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::ClaimSharingDelivery {
                    command,
                    respond_to,
                },
                SharingCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn finish(&self, command: FinishDelivery) -> Result<(), SharingMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::FinishSharingDelivery {
                    command,
                    respond_to,
                },
                SharingCommandError::OutcomeUnknown,
            )
            .await
    }

    /// Fail deliveries a previous process left mid-request. Returns the count.
    pub(crate) async fn fail_interrupted(&self) -> Result<u64, SharingMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::FailInterruptedSharing { respond_to },
                SharingCommandError::OutcomeUnknown,
            )
            .await
    }
}

fn stored_channel<Settings>(
    row: &SqliteRow,
    settings: Settings,
) -> Result<StoredChannel<Settings>, SharingLoadError> {
    let version = u64::try_from(row.try_get::<i64, _>("version")?)
        .ok()
        .filter(|version| *version > 0)
        .ok_or(SharingLoadError::CorruptStoredState)?;
    Ok(StoredChannel {
        version,
        credentials_rejected: row.try_get("credentials_rejected")?,
        settings,
    })
}

fn stored_post_id(row: &SqliteRow) -> Result<PostId, SharingLoadError> {
    let id = Uuid::from_slice(row.try_get("post_id")?)
        .map_err(|_| SharingLoadError::CorruptStoredState)?;
    PostId::parse(&id.hyphenated().to_string()).map_err(|_| SharingLoadError::CorruptStoredState)
}

fn stored_teaser(row: &SqliteRow) -> Result<Teaser, SharingLoadError> {
    Teaser::from_parts(
        stored_post_id(row)?,
        row.try_get("title")?,
        row.try_get("summary")?,
        row.try_get("url")?,
    )
    .map_err(|_| SharingLoadError::CorruptStoredState)
}

fn stored_delivery(row: &SqliteRow) -> Result<Delivery, SharingLoadError> {
    let channel =
        Channel::parse(row.try_get("channel")?).ok_or(SharingLoadError::CorruptStoredState)?;
    let posted_url: Option<String> = row.try_get("posted_url")?;
    let failure: Option<&str> = row.try_get("failure")?;
    let state = match (row.try_get::<&str, _>("state")?, posted_url, failure) {
        ("queued", None, None) => DeliveryState::Queued {
            retry_after: OffsetDateTime::from_unix_timestamp(row.try_get("retry_after")?)
                .map_err(|_| SharingLoadError::CorruptStoredState)?,
        },
        ("sending", None, None) => DeliveryState::Sending,
        ("posted", Some(url), None) => DeliveryState::Posted { url },
        ("failed", None, Some(failure)) => DeliveryState::Failed(
            DeliveryFailure::parse(failure).ok_or(SharingLoadError::CorruptStoredState)?,
        ),
        _ => return Err(SharingLoadError::CorruptStoredState),
    };
    Ok(Delivery { channel, state })
}

fn auth_error(error: AuthApplyError) -> SharingApplyError {
    match error {
        AuthApplyError::Operation(error) => SharingApplyError::Operation(error),
        AuthApplyError::CorruptStoredState => SharingApplyError::CorruptStoredState,
        AuthApplyError::Command(AuthCommandError::ScopeEscalation | AuthCommandError::NotFound) => {
            SharingCommandError::Forbidden.into()
        }
        AuthApplyError::Command(
            AuthCommandError::Conflict | AuthCommandError::IdempotencyConflict,
        ) => SharingCommandError::IdempotencyConflict.into(),
        AuthApplyError::Command(
            AuthCommandError::AlreadyBootstrapped
            | AuthCommandError::BootstrapRequired
            | AuthCommandError::StaleVersion
            | AuthCommandError::NoLoginProvider
            | AuthCommandError::EnabledUserRequiresCredential
            | AuthCommandError::LastEnabledOwner
            | AuthCommandError::InvalidValue
            | AuthCommandError::InvalidChallenge
            | AuthCommandError::ChallengeCapacity
            | AuthCommandError::ReplayCapacity
            | AuthCommandError::SessionCapacity
            | AuthCommandError::AgentCredentialCapacity
            | AuthCommandError::ReplayedProof
            | AuthCommandError::OutcomeUnknown,
        ) => SharingApplyError::CorruptStoredState,
    }
}

/// An exact retry of a recorded command succeeds again without a second write.
async fn replayed(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: &MutationAuditContext,
    action: &'static str,
    fingerprint: [u8; 32],
) -> Result<bool, SharingApplyError> {
    let row = sqlx::query(
        "SELECT audit.principal_kind,audit.actor_user_id,audit.session_id,\
         audit.agent_credential_id,audit.action,audit.outcome,receipt.command_fingerprint \
         FROM admin_audit_events AS audit LEFT JOIN sharing_receipts AS receipt \
         ON receipt.audit_event_id=audit.audit_event_id WHERE audit.idempotency_key=?",
    )
    .bind(audit.idempotency_key.0.as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let principal = decode_audit_principal(
        row.try_get("principal_kind")?,
        row.try_get("actor_user_id")?,
        row.try_get("session_id")?,
        row.try_get("agent_credential_id")?,
    )
    .map_err(|_| SharingApplyError::CorruptStoredState)?;
    if principal != audit.principal
        || row.try_get::<&str, _>("action")? != action
        || row.try_get::<&str, _>("outcome")? != "succeeded"
        || row.try_get::<Option<&[u8]>, _>("command_fingerprint")? != Some(fingerprint.as_slice())
    {
        return Err(SharingCommandError::IdempotencyConflict.into());
    }
    Ok(true)
}

async fn record_receipt(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: &MutationAuditContext,
    executed_at: OffsetDateTime,
    action: &'static str,
    fingerprint: [u8; 32],
) -> Result<(), SharingApplyError> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sharing_receipts")
        .fetch_one(&mut **transaction)
        .await?;
    if count >= MAX_RECEIPTS {
        return Err(SharingCommandError::Capacity.into());
    }
    append_success_audit(transaction, audit, executed_at, action)
        .await
        .map_err(auth_error)?;
    sqlx::query(
        "INSERT INTO sharing_receipts(idempotency_key,audit_event_id,command_fingerprint) \
         VALUES(?,?,?)",
    )
    .bind(audit.idempotency_key.0.as_bytes().as_slice())
    .bind(audit.audit_event_id.as_uuid().as_bytes().as_slice())
    .bind(fingerprint.as_slice())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// Returns the version to write after checking the form's expected version.
async fn next_version(
    transaction: &mut Transaction<'_, Sqlite>,
    channel: Channel,
    expected_version: u64,
) -> Result<(Option<i64>, i64), SharingApplyError> {
    let current: Option<i64> = sqlx::query_scalar(match channel {
        Channel::Substack => "SELECT version FROM sharing_substack WHERE singleton=1",
        Channel::X => "SELECT version FROM sharing_x WHERE singleton=1",
    })
    .fetch_optional(&mut **transaction)
    .await?;
    if current.unwrap_or(0)
        != i64::try_from(expected_version).map_err(|_| SharingCommandError::InvalidValue)?
    {
        return Err(SharingCommandError::StaleVersion.into());
    }
    let next = current
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(SharingCommandError::Capacity)?;
    Ok((current, next))
}

pub(crate) async fn update_substack(
    transaction: &mut Transaction<'_, Sqlite>,
    command: UpdateSubstack,
    executed_at: OffsetDateTime,
) -> Result<(), SharingApplyError> {
    let mut fingerprint = CommandFingerprintBuilder::new(UPDATE_SUBSTACK_ACTION);
    fingerprint.version(command.expected_version);
    fingerprint.field(command.mode.as_str().as_bytes());
    fingerprint.field(command.subdomain.as_str().as_bytes());
    fingerprint.optional_field(
        command
            .session
            .as_ref()
            .map(|session| session.expose().as_bytes()),
    );
    let fingerprint = fingerprint.finish();
    if replayed(
        transaction,
        &command.audit,
        UPDATE_SUBSTACK_ACTION,
        fingerprint,
    )
    .await?
    {
        return Ok(());
    }
    require_fresh_browser_owner(transaction, &command.audit.principal, executed_at)
        .await
        .map_err(auth_error)?;
    let (current, version) =
        next_version(transaction, Channel::Substack, command.expected_version).await?;
    match (&command.session, current) {
        // New credentials clear an earlier provider rejection.
        (Some(session), _) => {
            sqlx::query(
                "INSERT INTO sharing_substack(singleton,version,mode,credentials_rejected,\
                 subdomain,session_cookie) VALUES(1,?,?,0,?,?) ON CONFLICT(singleton) DO UPDATE \
                 SET version=excluded.version,mode=excluded.mode,credentials_rejected=0,\
                 subdomain=excluded.subdomain,session_cookie=excluded.session_cookie",
            )
            .bind(version)
            .bind(command.mode.as_str())
            .bind(command.subdomain.as_str())
            .bind(session.expose())
            .execute(&mut **transaction)
            .await?;
        }
        (None, Some(_)) => {
            sqlx::query(
                "UPDATE sharing_substack SET version=?,mode=?,subdomain=? WHERE singleton=1",
            )
            .bind(version)
            .bind(command.mode.as_str())
            .bind(command.subdomain.as_str())
            .execute(&mut **transaction)
            .await?;
        }
        (None, None) => return Err(SharingCommandError::InvalidValue.into()),
    }
    record_receipt(
        transaction,
        &command.audit,
        executed_at,
        UPDATE_SUBSTACK_ACTION,
        fingerprint,
    )
    .await
}

pub(crate) async fn update_x(
    transaction: &mut Transaction<'_, Sqlite>,
    command: UpdateX,
    executed_at: OffsetDateTime,
) -> Result<(), SharingApplyError> {
    let mut fingerprint = CommandFingerprintBuilder::new(UPDATE_X_ACTION);
    fingerprint.version(command.expected_version);
    fingerprint.field(command.mode.as_str().as_bytes());
    match &command.credentials {
        Some(credentials) => {
            fingerprint.field(b"replace");
            for secret in [
                &credentials.api_key,
                &credentials.api_secret,
                &credentials.access_token,
                &credentials.access_token_secret,
            ] {
                fingerprint.field(secret.expose().as_bytes());
            }
        }
        None => fingerprint.field(b"keep"),
    }
    let fingerprint = fingerprint.finish();
    if replayed(transaction, &command.audit, UPDATE_X_ACTION, fingerprint).await? {
        return Ok(());
    }
    require_fresh_browser_owner(transaction, &command.audit.principal, executed_at)
        .await
        .map_err(auth_error)?;
    let (current, version) =
        next_version(transaction, Channel::X, command.expected_version).await?;
    match (&command.credentials, current) {
        // New credentials clear an earlier provider rejection.
        (Some(credentials), _) => {
            sqlx::query(
                "INSERT INTO sharing_x(singleton,version,mode,credentials_rejected,api_key,\
                 api_secret,access_token,access_token_secret) VALUES(1,?,?,0,?,?,?,?) \
                 ON CONFLICT(singleton) DO UPDATE SET version=excluded.version,\
                 mode=excluded.mode,credentials_rejected=0,api_key=excluded.api_key,\
                 api_secret=excluded.api_secret,access_token=excluded.access_token,\
                 access_token_secret=excluded.access_token_secret",
            )
            .bind(version)
            .bind(command.mode.as_str())
            .bind(credentials.api_key.expose())
            .bind(credentials.api_secret.expose())
            .bind(credentials.access_token.expose())
            .bind(credentials.access_token_secret.expose())
            .execute(&mut **transaction)
            .await?;
        }
        (None, Some(_)) => {
            sqlx::query("UPDATE sharing_x SET version=?,mode=? WHERE singleton=1")
                .bind(version)
                .bind(command.mode.as_str())
                .execute(&mut **transaction)
                .await?;
        }
        (None, None) => return Err(SharingCommandError::InvalidValue.into()),
    }
    record_receipt(
        transaction,
        &command.audit,
        executed_at,
        UPDATE_X_ACTION,
        fingerprint,
    )
    .await
}

pub(crate) async fn share(
    transaction: &mut Transaction<'_, Sqlite>,
    command: ShareTeaser,
    executed_at: OffsetDateTime,
) -> Result<(), SharingApplyError> {
    let mut fingerprint = CommandFingerprintBuilder::new(SHARE_TEASER_ACTION);
    fingerprint.uuid(&command.post_id.as_uuid());
    fingerprint.field(command.channel.as_str().as_bytes());
    let fingerprint = fingerprint.finish();
    if replayed(
        transaction,
        &command.audit,
        SHARE_TEASER_ACTION,
        fingerprint,
    )
    .await?
    {
        return Ok(());
    }
    require_fresh_browser_owner(transaction, &command.audit.principal, executed_at)
        .await
        .map_err(auth_error)?;
    let post_id = command.post_id.as_uuid();
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sharing_teasers WHERE post_id=?)")
            .bind(post_id.as_bytes().as_slice())
            .fetch_one(&mut **transaction)
            .await?;
    if !exists {
        return Err(SharingCommandError::NotFound.into());
    }
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM sharing_deliveries WHERE post_id=? AND channel=?")
            .bind(post_id.as_bytes().as_slice())
            .bind(command.channel.as_str())
            .fetch_optional(&mut **transaction)
            .await?;
    let now = executed_at.unix_timestamp();
    match state.as_deref() {
        None => insert_delivery(transaction, post_id, command.channel, now).await?,
        Some("failed") => {
            sqlx::query(
                "UPDATE sharing_deliveries SET state='queued',attempts=0,retry_after=0,\
                 failure=NULL,updated_at=? WHERE post_id=? AND channel=?",
            )
            .bind(now)
            .bind(post_id.as_bytes().as_slice())
            .bind(command.channel.as_str())
            .execute(&mut **transaction)
            .await?;
        }
        Some("queued" | "sending" | "posted") => {
            return Err(SharingCommandError::StateConflict.into());
        }
        Some(_) => return Err(SharingApplyError::CorruptStoredState),
    }
    record_receipt(
        transaction,
        &command.audit,
        executed_at,
        SHARE_TEASER_ACTION,
        fingerprint,
    )
    .await
}

async fn insert_delivery(
    transaction: &mut Transaction<'_, Sqlite>,
    post_id: Uuid,
    channel: Channel,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO sharing_deliveries(post_id,channel,state,attempts,retry_after,updated_at) \
         VALUES(?,?,'queued',0,0,?)",
    )
    .bind(post_id.as_bytes().as_slice())
    .bind(channel.as_str())
    .bind(now)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn claim(
    transaction: &mut Transaction<'_, Sqlite>,
    command: ClaimDelivery,
    now: OffsetDateTime,
) -> Result<(), SharingApplyError> {
    let version =
        i64::try_from(command.settings_version).map_err(|_| SharingCommandError::InvalidValue)?;
    let sendable: bool = sqlx::query_scalar(match command.channel {
        Channel::Substack => {
            "SELECT EXISTS(SELECT 1 FROM sharing_substack WHERE version=? AND mode='enabled' \
             AND credentials_rejected=0)"
        }
        Channel::X => {
            "SELECT EXISTS(SELECT 1 FROM sharing_x WHERE version=? AND mode='enabled' \
             AND credentials_rejected=0)"
        }
    })
    .bind(version)
    .fetch_one(&mut **transaction)
    .await?;
    if !sendable {
        return Err(SharingCommandError::StaleVersion.into());
    }
    let claimed = sqlx::query(
        "UPDATE sharing_deliveries SET state='sending',attempts=attempts+1,updated_at=? \
         WHERE post_id=? AND channel=? AND state='queued' AND retry_after<=?",
    )
    .bind(now.unix_timestamp())
    .bind(command.post_id.as_uuid().as_bytes().as_slice())
    .bind(command.channel.as_str())
    .bind(now.unix_timestamp())
    .execute(&mut **transaction)
    .await?;
    if claimed.rows_affected() != 1 {
        return Err(SharingCommandError::StateConflict.into());
    }
    Ok(())
}

pub(crate) async fn finish(
    transaction: &mut Transaction<'_, Sqlite>,
    command: FinishDelivery,
    now: OffsetDateTime,
) -> Result<(), SharingApplyError> {
    let post_id = command.post_id.as_uuid();
    let attempts: Option<i64> = sqlx::query_scalar(
        "SELECT attempts FROM sharing_deliveries WHERE post_id=? AND channel=? AND state='sending'",
    )
    .bind(post_id.as_bytes().as_slice())
    .bind(command.channel.as_str())
    .fetch_optional(&mut **transaction)
    .await?;
    let attempts = attempts.ok_or(SharingCommandError::StateConflict)?;
    let now = now.unix_timestamp();
    let outcome = match command.outcome {
        DeliveryOutcome::Retry { .. } if attempts >= MAX_ATTEMPTS => {
            DeliveryOutcome::Failed(DeliveryFailure::RetriesExhausted)
        }
        outcome => outcome,
    };
    let update = match &outcome {
        DeliveryOutcome::Posted { url } => {
            if url.is_empty() || url.len() > MAX_POSTED_URL_BYTES {
                return Err(SharingCommandError::InvalidValue.into());
            }
            sqlx::query("UPDATE sharing_deliveries SET state='posted',posted_url=?,updated_at=? WHERE post_id=? AND channel=?")
                .bind(url.as_str())
                .bind(now)
        }
        DeliveryOutcome::Retry { draft } => {
            let wait = BASE_RETRY_SECONDS << attempts.clamp(0, MAX_ATTEMPTS);
            sqlx::query("UPDATE sharing_deliveries SET state='queued',retry_after=?,draft=COALESCE(?,draft),updated_at=? WHERE post_id=? AND channel=?")
                .bind(now.saturating_add(wait))
                .bind(stored_draft(*draft)?)
                .bind(now)
        }
        DeliveryOutcome::Failed(failure) => {
            sqlx::query("UPDATE sharing_deliveries SET state='failed',failure=?,updated_at=? WHERE post_id=? AND channel=?")
                .bind(failure.as_str())
                .bind(now)
        }
        DeliveryOutcome::CredentialsRejected { draft } => {
            sqlx::query("UPDATE sharing_deliveries SET state='queued',attempts=MAX(attempts-1,0),draft=COALESCE(?,draft),updated_at=? WHERE post_id=? AND channel=?")
                .bind(stored_draft(*draft)?)
                .bind(now)
        }
    };
    update
        .bind(post_id.as_bytes().as_slice())
        .bind(command.channel.as_str())
        .execute(&mut **transaction)
        .await?;
    if matches!(outcome, DeliveryOutcome::CredentialsRejected { .. }) {
        // A newer save keeps its fresh credentials: only this version is marked.
        sqlx::query(match command.channel {
            Channel::Substack => {
                "UPDATE sharing_substack SET credentials_rejected=1 WHERE singleton=1 AND version=?"
            }
            Channel::X => {
                "UPDATE sharing_x SET credentials_rejected=1 WHERE singleton=1 AND version=?"
            }
        })
        .bind(
            i64::try_from(command.settings_version)
                .map_err(|_| SharingCommandError::InvalidValue)?,
        )
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

fn stored_draft(draft: Option<u64>) -> Result<Option<i64>, SharingCommandError> {
    draft
        .map(|draft| {
            i64::try_from(draft)
                .ok()
                .filter(|draft| *draft > 0)
                .ok_or(SharingCommandError::InvalidValue)
        })
        .transpose()
}

pub(crate) async fn fail_interrupted(
    transaction: &mut Transaction<'_, Sqlite>,
    now: OffsetDateTime,
) -> Result<u64, SharingApplyError> {
    let failed = sqlx::query(
        "UPDATE sharing_deliveries SET state='failed',failure='interrupted',updated_at=? \
         WHERE state='sending'",
    )
    .bind(now.unix_timestamp())
    .execute(&mut **transaction)
    .await?;
    Ok(failed.rows_affected())
}

/// Runs under the offline restore's database lock and acceptance transaction.
/// A provider may have accepted any unfinished teaser after the backup was
/// taken, so none is sent again and every channel waits for its Owner.
pub(crate) async fn hold_restored_sharing(
    transaction: &mut Transaction<'_, Sqlite>,
    now: OffsetDateTime,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE sharing_deliveries SET state='failed',failure='interrupted',updated_at=? \
         WHERE state IN ('queued','sending')",
    )
    .bind(now.unix_timestamp())
    .execute(&mut **transaction)
    .await?;
    for pause in [
        "UPDATE sharing_substack SET mode='paused'",
        "UPDATE sharing_x SET mode='paused'",
    ] {
        sqlx::query(pause).execute(&mut **transaction).await?;
    }
    Ok(())
}

/// Record an article's teaser with its first publication and queue it for the
/// channels enabled at that moment. Edits and later publications add nothing.
pub(crate) async fn queue_teaser(
    transaction: &mut Transaction<'_, Sqlite>,
    publication_id: Uuid,
    publication: &CanonicalPublicationView,
    teaser: Option<Teaser>,
) -> Result<(), PublicationMutationError> {
    let Some(teaser) = teaser else {
        return Ok(());
    };
    let view = teaser.view();
    if *view.post_id != publication.stable_post_id {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::InvalidValue,
        ));
    }
    let post_id = publication.stable_post_id.as_uuid();
    // The newsletter's marker is the durable record of first publication.
    let first_publication: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM mail_article_notifications WHERE post_id=? AND publication_id=?)",
    )
    .bind(post_id.as_bytes().as_slice())
    .bind(publication_id.as_bytes().as_slice())
    .fetch_one(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    if !first_publication {
        return Ok(());
    }
    let now = publication
        .published_at
        .ok_or(PublicationMutationError::CorruptStoredState)?
        .unix_timestamp();
    let inserted = sqlx::query(
        "INSERT INTO sharing_teasers(post_id,publication_id,created_at,title,summary,url) \
         VALUES(?,?,?,?,?,?) ON CONFLICT(post_id) DO NOTHING",
    )
    .bind(post_id.as_bytes().as_slice())
    .bind(publication_id.as_bytes().as_slice())
    .bind(now)
    .bind(view.title)
    .bind(view.summary)
    .bind(view.url)
    .execute(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    if inserted.rows_affected() == 0 {
        return Ok(());
    }
    for channel in Channel::ALL {
        let enabled: bool = sqlx::query_scalar(match channel {
            Channel::Substack => {
                "SELECT EXISTS(SELECT 1 FROM sharing_substack WHERE mode='enabled')"
            }
            Channel::X => "SELECT EXISTS(SELECT 1 FROM sharing_x WHERE mode='enabled')",
        })
        .fetch_one(&mut **transaction)
        .await
        .map_err(PublicationMutationError::Operation)?;
        if enabled {
            insert_delivery(transaction, post_id, channel, now)
                .await
                .map_err(PublicationMutationError::Operation)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::{Connection as _, SqliteConnection, sqlite::SqliteConnectOptions};

    use super::*;
    use crate::{
        config::{
            DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
            DatabaseWriterQueueCapacity,
        },
        database,
    };

    #[tokio::test]
    async fn restore_pauses_channels_and_never_resends_an_unfinished_teaser() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/maincopy.db");
        database::bootstrap(DatabaseConfigurationView {
            path: &path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(16).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        })
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        let mut transaction = connection.begin().await.unwrap();
        for setup in [
            "INSERT INTO sharing_substack VALUES(1,3,'enabled',0,'example','s%3Asession')",
            "INSERT INTO sharing_x VALUES(1,1,'enabled',0,'a','b','c','d')",
            "INSERT INTO sharing_teasers VALUES(x'11111111111141118111111111111111',\
             x'22222222222242228222222222222222',0,'Title','','https://example.test/posts/a')",
            "INSERT INTO sharing_teasers VALUES(x'33333333333343338333333333333333',\
             x'44444444444444448444444444444444',0,'Title','','https://example.test/posts/b')",
            "INSERT INTO sharing_deliveries VALUES(x'11111111111141118111111111111111',\
             'substack','queued',0,0,0,NULL,NULL,NULL)",
            "INSERT INTO sharing_deliveries VALUES(x'11111111111141118111111111111111',\
             'x','sending',1,0,0,NULL,NULL,NULL)",
            "INSERT INTO sharing_deliveries VALUES(x'33333333333343338333333333333333',\
             'x','posted',1,0,0,NULL,'https://x.com/i/status/1',NULL)",
        ] {
            sqlx::query(setup).execute(&mut *transaction).await.unwrap();
        }
        hold_restored_sharing(&mut transaction, OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
        let states: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT state,failure FROM sharing_deliveries ORDER BY post_id,channel")
                .fetch_all(&mut *transaction)
                .await
                .unwrap();
        let interrupted = ("failed".to_owned(), Some("interrupted".to_owned()));
        assert_eq!(
            states,
            [
                interrupted.clone(),
                interrupted,
                ("posted".to_owned(), None)
            ]
        );
        let modes: Vec<String> = sqlx::query_scalar(
            "SELECT mode FROM sharing_substack UNION ALL SELECT mode FROM sharing_x",
        )
        .fetch_all(&mut *transaction)
        .await
        .unwrap();
        assert_eq!(modes, ["paused", "paused"]);
        transaction.commit().await.unwrap();
        connection.close().await.unwrap();
    }
}
