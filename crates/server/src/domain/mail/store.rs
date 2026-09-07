//! Sole-writer campaign transitions and bounded, validated public-content reads.

use super::subscriber::store as subscriber_store;
use sqlx::{FromRow, QueryBuilder, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::campaign::{
    Campaign, CampaignApproval, CampaignContent, CampaignCounts, CampaignFence, CampaignId,
    CampaignLease, CampaignProgress, CampaignQuarantine, CampaignState, CampaignValidationError,
    CampaignVersion, MAX_CAMPAIGN_RECORD_BYTES, MAX_CAMPAIGNS, MAX_CLAIM_SECONDS,
};
use crate::{
    database::{
        fingerprint::CommandFingerprintBuilder,
        store::{DatabaseAdmissionError, Mutation, MutationSender},
    },
    domain::{
        auth::store::{
            AuthApplyError, AuthCommandError, MutationAuditContext, append_success_audit,
            decode_audit_principal, require_enabled_owner, require_fresh_browser_owner,
        },
        publication::store::{PublicationMutationError, SiteHead, matches_publication_review},
    },
};

const SELECT_CAMPAIGNS: &str = "SELECT campaign_id, version, state, CASE WHEN length(CAST(record AS BLOB)) <= 262144 THEN record END AS record FROM mail_campaigns";
const MAX_PAGE_SIZE: usize = 100;

#[derive(Clone)]
pub(crate) struct CampaignStore {
    readers: SqlitePool,
    mutations: MutationSender,
}

impl CampaignStore {
    pub(crate) const fn new(readers: SqlitePool, mutations: mpsc::Sender<Mutation>) -> Self {
        Self {
            readers,
            mutations: MutationSender::new(mutations),
        }
    }

    pub(crate) async fn campaign(
        &self,
        id: CampaignId,
    ) -> Result<Option<Campaign>, CampaignLoadError> {
        let mut query = QueryBuilder::new(SELECT_CAMPAIGNS);
        query
            .push(" WHERE campaign_id = ")
            .push_bind(id.0.as_bytes().as_slice());
        query
            .build_query_as::<CampaignRow>()
            .fetch_optional(&self.readers)
            .await?
            .map(CampaignRow::decode)
            .transpose()
    }

    pub(crate) async fn active_campaign(&self) -> Result<Option<Campaign>, CampaignLoadError> {
        let mut query = QueryBuilder::new(SELECT_CAMPAIGNS);
        query.push(" WHERE state IN ('draft','queued','claimed','cancelling') LIMIT 2");
        let mut rows = query
            .build_query_as::<CampaignRow>()
            .fetch_all(&self.readers)
            .await?;
        if rows.len() > 1 {
            return Err(CampaignLoadError::CorruptStoredState);
        }
        rows.pop().map(CampaignRow::decode).transpose()
    }

    pub(crate) async fn list(
        &self,
        cursor: Option<CampaignId>,
        limit: usize,
    ) -> Result<CampaignPage, CampaignLoadError> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(CampaignLoadError::InvalidLimit);
        }
        let mut query = QueryBuilder::new(SELECT_CAMPAIGNS);
        let cursor = cursor.map(|id| id.0.into_bytes().to_vec());
        query
            .push(" WHERE (")
            .push_bind(&cursor)
            .push(" IS NULL OR campaign_id > ")
            .push_bind(&cursor)
            .push(") ORDER BY campaign_id LIMIT ")
            .push_bind((limit + 1) as i64);
        let rows = query
            .build_query_as::<CampaignRow>()
            .fetch_all(&self.readers)
            .await?;
        let mut items = rows
            .into_iter()
            .map(CampaignRow::decode)
            .collect::<Result<Vec<_>, _>>()?;
        let more = items.len() > limit;
        items.truncate(limit);
        let next_cursor = more.then(|| {
            items
                .last()
                .expect("a nonempty bounded page has a last item")
                .campaign_id
        });
        Ok(CampaignPage { items, next_cursor })
    }

    pub(crate) async fn create_draft(
        &self,
        command: CreateCampaign,
    ) -> Result<Campaign, CampaignMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::CreateMailCampaign {
                    command,
                    respond_to,
                },
                CampaignCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn approve(
        &self,
        command: ApproveCampaign,
    ) -> Result<Campaign, CampaignMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::ApproveMailCampaign {
                    command,
                    respond_to,
                },
                CampaignCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn cancel(
        &self,
        command: CancelCampaign,
    ) -> Result<Campaign, CampaignMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::CancelMailCampaign {
                    command,
                    respond_to,
                },
                CampaignCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn claim(
        &self,
        command: ClaimCampaign,
    ) -> Result<Option<Campaign>, CampaignMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::ClaimMailCampaign {
                    command,
                    respond_to,
                },
                CampaignCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn finish(
        &self,
        command: FinishCampaign,
    ) -> Result<Campaign, CampaignMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::FinishMailCampaign {
                    command,
                    respond_to,
                },
                CampaignCommandError::OutcomeUnknown,
            )
            .await
    }

    /// Renews worker custody, not permission to submit a recipient. The absolute
    /// target keeps a repeated request stable; the writer checks its own clock.
    pub(crate) async fn renew_claim(
        &self,
        command: RenewCampaignClaim,
    ) -> Result<Campaign, CampaignMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::RenewMailCampaignClaim {
                    command,
                    respond_to,
                },
                CampaignCommandError::OutcomeUnknown,
            )
            .await
    }

    /// Call before starting a replacement worker. Never resume an old claim,
    /// even when its lease has not expired and its last reply was lost.
    pub(crate) async fn quarantine_interrupted(
        &self,
        now: OffsetDateTime,
    ) -> Result<u64, CampaignMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::QuarantineInterruptedMail { now, respond_to },
                CampaignCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn validate_all(&self) -> Result<(), CampaignLoadError> {
        let mut cursor = None;
        let mut count = 0;
        loop {
            let page = self.list(cursor, MAX_PAGE_SIZE).await?;
            count += page.items.len();
            if count > MAX_CAMPAIGNS {
                return Err(CampaignLoadError::CorruptStoredState);
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(());
            }
        }
    }
}

pub(crate) struct CampaignPage {
    pub items: Vec<Campaign>,
    pub next_cursor: Option<CampaignId>,
}

#[derive(Clone, Debug)]
pub(crate) struct CreateCampaign {
    pub proposed_id: CampaignId,
    pub content: CampaignContent,
    pub configuration_binding: [u8; 32],
    pub audit: MutationAuditContext,
    /// Request occurrence time; authorization uses the writer's execution time.
    pub now: OffsetDateTime,
}

#[derive(Clone, Debug)]
pub(crate) struct ApproveCampaign {
    pub campaign_id: CampaignId,
    pub expected_version: CampaignVersion,
    pub configuration_binding: [u8; 32],
    pub audit: MutationAuditContext,
    /// Request occurrence time; authorization uses the writer's execution time.
    pub now: OffsetDateTime,
}

#[derive(Clone, Debug)]
pub(crate) struct CancelCampaign {
    pub campaign_id: CampaignId,
    pub expected_version: CampaignVersion,
    pub audit: MutationAuditContext,
    /// Request occurrence time; authorization uses the writer's execution time.
    pub now: OffsetDateTime,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ClaimCampaign {
    pub configuration_binding: [u8; 32],
    pub lease_seconds: u32,
    pub now: OffsetDateTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FinishIntent {
    Complete,
    Cancel,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FinishCampaign {
    pub campaign_id: CampaignId,
    pub fence: CampaignFence,
    pub intent: FinishIntent,
    pub now: OffsetDateTime,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RenewCampaignClaim {
    pub campaign_id: CampaignId,
    pub fence: CampaignFence,
    pub configuration_binding: [u8; 32],
    pub expires_at: OffsetDateTime,
}

#[derive(Debug, Error)]
pub(crate) enum CampaignLoadError {
    #[error("campaign query failed")]
    Operation(#[from] sqlx::Error),
    #[error("stored campaign data failed validation")]
    CorruptStoredState,
    #[error("campaign page size must be between 1 and 100")]
    InvalidLimit,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum CampaignCommandError {
    #[error("campaign authorization requires a fresh Owner browser session")]
    Forbidden,
    #[error("the approving user is no longer an enabled Owner")]
    ApprovalRevoked,
    #[error("the campaign does not exist")]
    NotFound,
    #[error("the campaign version changed")]
    StaleVersion,
    #[error("another campaign is active")]
    ActiveCampaign,
    #[error("the campaign lifecycle does not permit this operation")]
    InvalidTransition,
    #[error("the public revision or site changed since review")]
    PublicationChanged,
    #[error("the mail configuration changed since review")]
    ConfigurationChanged,
    #[error("mail sending is unavailable until its configured policy and feedback are ready")]
    SendingUnavailable,
    #[error("the campaign claim is no longer authoritative")]
    StaleClaim,
    #[error("the campaign command exceeds its validated bounds")]
    InvalidValue,
    #[error("the campaign history limit has been reached")]
    Capacity,
    #[error("the idempotency key is bound to another command or session")]
    IdempotencyConflict,
    #[error("the admitted campaign operation outcome is unknown")]
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum CampaignMutationError {
    #[error(transparent)]
    Admission(#[from] DatabaseAdmissionError),
    #[error(transparent)]
    Command(#[from] CampaignCommandError),
}

#[derive(Debug, Error)]
pub(crate) enum CampaignApplyError {
    #[error(transparent)]
    Command(#[from] CampaignCommandError),
    #[error("campaign database operation failed")]
    Operation(#[from] sqlx::Error),
    #[error("stored campaign data failed validation")]
    CorruptStoredState,
}

impl From<CampaignValidationError> for CampaignApplyError {
    fn from(_: CampaignValidationError) -> Self {
        CampaignCommandError::InvalidValue.into()
    }
}

#[derive(FromRow)]
struct CampaignRow {
    campaign_id: Vec<u8>,
    version: i64,
    state: String,
    record: Option<String>,
}

impl CampaignRow {
    fn decode(self) -> Result<Campaign, CampaignLoadError> {
        let value = decode_record(self.record.as_deref())?;
        if value.campaign_id.0.as_bytes().as_slice() != self.campaign_id
            || u64::from(value.version) != self.version as u64
            || value.state.as_str() != self.state
        {
            return Err(CampaignLoadError::CorruptStoredState);
        }
        Ok(value)
    }
}

fn decode_record(record: Option<&str>) -> Result<Campaign, CampaignLoadError> {
    let record = record
        .filter(|value| value.len() <= MAX_CAMPAIGN_RECORD_BYTES)
        .ok_or(CampaignLoadError::CorruptStoredState)?;
    let value: Campaign =
        serde_json::from_str(record).map_err(|_| CampaignLoadError::CorruptStoredState)?;
    value
        .validate()
        .map_err(|_| CampaignLoadError::CorruptStoredState)?;
    Ok(value)
}

fn encode_record(campaign: &Campaign) -> Result<String, CampaignApplyError> {
    campaign
        .validate()
        .map_err(|_| CampaignApplyError::CorruptStoredState)?;
    let record =
        serde_json::to_string(campaign).map_err(|_| CampaignApplyError::CorruptStoredState)?;
    if record.len() > MAX_CAMPAIGN_RECORD_BYTES {
        return Err(CampaignCommandError::InvalidValue.into());
    }
    Ok(record)
}

fn load_error(error: CampaignLoadError) -> CampaignApplyError {
    match error {
        CampaignLoadError::Operation(error) => CampaignApplyError::Operation(error),
        CampaignLoadError::CorruptStoredState | CampaignLoadError::InvalidLimit => {
            CampaignApplyError::CorruptStoredState
        }
    }
}

fn auth_error(error: AuthApplyError) -> CampaignApplyError {
    match error {
        AuthApplyError::Operation(error) => CampaignApplyError::Operation(error),
        AuthApplyError::CorruptStoredState => CampaignApplyError::CorruptStoredState,
        AuthApplyError::Command(AuthCommandError::ScopeEscalation | AuthCommandError::NotFound) => {
            CampaignCommandError::Forbidden.into()
        }
        AuthApplyError::Command(
            AuthCommandError::Conflict | AuthCommandError::IdempotencyConflict,
        ) => CampaignCommandError::IdempotencyConflict.into(),
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
        ) => CampaignApplyError::CorruptStoredState,
    }
}

async fn load(
    transaction: &mut Transaction<'_, Sqlite>,
    id: CampaignId,
) -> Result<Campaign, CampaignApplyError> {
    let mut query = QueryBuilder::new(SELECT_CAMPAIGNS);
    query
        .push(" WHERE campaign_id = ")
        .push_bind(id.0.as_bytes().as_slice());
    let row = query
        .build_query_as::<CampaignRow>()
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(CampaignCommandError::NotFound)?;
    row.decode().map_err(load_error)
}

async fn persist(
    transaction: &mut Transaction<'_, Sqlite>,
    campaign: &mut Campaign,
    now: OffsetDateTime,
) -> Result<(), CampaignApplyError> {
    if now < campaign.updated_at {
        return Err(CampaignCommandError::InvalidValue.into());
    }
    let prior = campaign.version;
    campaign.version = prior.next()?;
    campaign.updated_at = now;
    let record = encode_record(campaign)?;
    let result = sqlx::query("UPDATE mail_campaigns SET version = ?, state = ?, record = ? WHERE campaign_id = ? AND version = ?")
        .bind(u64::from(campaign.version) as i64).bind(campaign.state.as_str()).bind(record)
        .bind(campaign.campaign_id.0.as_bytes().as_slice()).bind(u64::from(prior) as i64)
        .execute(&mut **transaction).await?;
    if result.rows_affected() != 1 {
        return Err(CampaignCommandError::StaleVersion.into());
    }
    Ok(())
}

async fn require_review(
    transaction: &mut Transaction<'_, Sqlite>,
    content: &CampaignContent,
) -> Result<(), CampaignApplyError> {
    let current = matches_publication_review(
        transaction,
        &content.post_id,
        &content.revision,
        &SiteHead {
            digest: content.snapshot.clone(),
            version: content.site_version,
        },
    )
    .await
    .map_err(|error| match error {
        PublicationMutationError::Operation(error) => CampaignApplyError::Operation(error),
        PublicationMutationError::Command(_) => CampaignCommandError::PublicationChanged.into(),
        PublicationMutationError::CorruptStoredState => CampaignApplyError::CorruptStoredState,
    })?;
    if current {
        Ok(())
    } else {
        Err(CampaignCommandError::PublicationChanged.into())
    }
}

async fn instance_version(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<u64, CampaignApplyError> {
    let version: i64 =
        sqlx::query_scalar("SELECT version FROM instance_identity WHERE singleton = 1")
            .fetch_one(&mut **transaction)
            .await?;
    CampaignVersion::try_from(version as u64)
        .map_err(|_| CampaignApplyError::CorruptStoredState)?;
    Ok(version as u64)
}

fn require_version(
    campaign: &Campaign,
    expected: CampaignVersion,
) -> Result<(), CampaignApplyError> {
    if campaign.version == expected {
        Ok(())
    } else {
        Err(CampaignCommandError::StaleVersion.into())
    }
}

fn change_fingerprint(
    action: &'static str,
    id: CampaignId,
    version: CampaignVersion,
) -> CommandFingerprintBuilder {
    let mut fingerprint = CommandFingerprintBuilder::new(action);
    fingerprint.uuid(&id.0);
    fingerprint.version(u64::from(version));
    fingerprint
}

pub(crate) async fn create_campaign(
    transaction: &mut Transaction<'_, Sqlite>,
    command: CreateCampaign,
    executed_at: OffsetDateTime,
) -> Result<Campaign, CampaignApplyError> {
    command.content.validate()?;
    let mut fingerprint = CommandFingerprintBuilder::new("mail.campaign.create");
    fingerprint.uuid(&command.proposed_id.0);
    fingerprint.field(&command.content.content_digest);
    fingerprint.field(command.content.snapshot.as_bytes());
    fingerprint.version(command.content.site_version);
    fingerprint.field(&command.configuration_binding);
    let fingerprint = fingerprint.finish();
    if let Some(value) = replay(
        transaction,
        &command.audit,
        "mail.campaign.create",
        fingerprint,
    )
    .await?
    {
        return Ok(value);
    }
    let owner = require_fresh_browser_owner(transaction, &command.audit.principal, executed_at)
        .await
        .map_err(auth_error)?;
    require_review(transaction, &command.content).await?;
    let (count, active): (i64, i64) = sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(state IN ('draft','queued','claimed','cancelling')),0) FROM mail_campaigns")
        .fetch_one(&mut **transaction).await?;
    if active != 0 {
        return Err(CampaignCommandError::ActiveCampaign.into());
    }
    if count >= MAX_CAMPAIGNS as i64 {
        return Err(CampaignCommandError::Capacity.into());
    }
    let campaign = Campaign {
        campaign_id: command.proposed_id,
        version: CampaignVersion::INITIAL,
        content: command.content,
        configuration_binding: command.configuration_binding,
        created_by: owner,
        created_at: command.now,
        updated_at: command.now,
        state: CampaignState::Draft,
    };
    campaign.validate()?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mail_campaigns WHERE campaign_id = ?)")
            .bind(campaign.campaign_id.0.as_bytes().as_slice())
            .fetch_one(&mut **transaction)
            .await?;
    if exists {
        return Err(CampaignCommandError::IdempotencyConflict.into());
    }
    sqlx::query(
        "INSERT INTO mail_campaigns (campaign_id,version,state,record) VALUES (?,1,'draft',?)",
    )
    .bind(campaign.campaign_id.0.as_bytes().as_slice())
    .bind(encode_record(&campaign)?)
    .execute(&mut **transaction)
    .await?;
    receipt(
        transaction,
        &command.audit,
        "mail.campaign.create",
        fingerprint,
        &campaign,
    )
    .await?;
    Ok(campaign)
}

pub(crate) async fn approve_campaign(
    transaction: &mut Transaction<'_, Sqlite>,
    command: ApproveCampaign,
    executed_at: OffsetDateTime,
) -> Result<Campaign, CampaignApplyError> {
    let mut fingerprint = change_fingerprint(
        "mail.campaign.approve",
        command.campaign_id,
        command.expected_version,
    );
    fingerprint.field(&command.configuration_binding);
    let fingerprint = fingerprint.finish();
    if let Some(value) = replay(
        transaction,
        &command.audit,
        "mail.campaign.approve",
        fingerprint,
    )
    .await?
    {
        return Ok(value);
    }
    let owner = require_fresh_browser_owner(transaction, &command.audit.principal, executed_at)
        .await
        .map_err(auth_error)?;
    let mut campaign = load(transaction, command.campaign_id).await?;
    require_version(&campaign, command.expected_version)?;
    if campaign.state != CampaignState::Draft {
        return Err(CampaignCommandError::InvalidTransition.into());
    }
    if campaign.configuration_binding != command.configuration_binding {
        return Err(CampaignCommandError::ConfigurationChanged.into());
    }
    require_review(transaction, &campaign.content).await?;
    subscriber_store::require_campaign_configuration(
        transaction,
        command.configuration_binding,
        executed_at,
    )
    .await?;
    campaign.state = CampaignState::Queued {
        approval: CampaignApproval {
            owner,
            approved_at: command.now,
            instance_version: instance_version(transaction).await?,
            audience_cutoff: subscriber_store::audience_cutoff(transaction).await?,
        },
    };
    persist(transaction, &mut campaign, command.now).await?;
    receipt(
        transaction,
        &command.audit,
        "mail.campaign.approve",
        fingerprint,
        &campaign,
    )
    .await?;
    Ok(campaign)
}

pub(crate) async fn cancel_campaign(
    transaction: &mut Transaction<'_, Sqlite>,
    command: CancelCampaign,
    executed_at: OffsetDateTime,
) -> Result<Campaign, CampaignApplyError> {
    let fingerprint = change_fingerprint(
        "mail.campaign.cancel",
        command.campaign_id,
        command.expected_version,
    )
    .finish();
    if let Some(value) = replay(
        transaction,
        &command.audit,
        "mail.campaign.cancel",
        fingerprint,
    )
    .await?
    {
        return Ok(value);
    }
    require_fresh_browser_owner(transaction, &command.audit.principal, executed_at)
        .await
        .map_err(auth_error)?;
    let mut campaign = load(transaction, command.campaign_id).await?;
    require_version(&campaign, command.expected_version)?;
    campaign.state = match campaign.state {
        CampaignState::Draft => CampaignState::Cancelled {
            approval: None,
            counts: CampaignCounts::default(),
        },
        CampaignState::Queued { approval } => CampaignState::Cancelled {
            approval: Some(approval),
            counts: CampaignCounts::default(),
        },
        CampaignState::Claimed { approval, lease } => CampaignState::Cancelling { approval, lease },
        CampaignState::Cancelling { .. }
        | CampaignState::Completed { .. }
        | CampaignState::Cancelled { .. }
        | CampaignState::Unknown { .. }
        | CampaignState::Quarantined { .. } => {
            return Err(CampaignCommandError::InvalidTransition.into());
        }
    };
    persist(transaction, &mut campaign, command.now).await?;
    receipt(
        transaction,
        &command.audit,
        "mail.campaign.cancel",
        fingerprint,
        &campaign,
    )
    .await?;
    Ok(campaign)
}

pub(crate) async fn claim_campaign(
    transaction: &mut Transaction<'_, Sqlite>,
    command: ClaimCampaign,
) -> Result<Option<Campaign>, CampaignApplyError> {
    if !(1..=MAX_CLAIM_SECONDS).contains(&command.lease_seconds) {
        return Err(CampaignCommandError::InvalidValue.into());
    }
    let mut query = QueryBuilder::new(SELECT_CAMPAIGNS);
    query.push(" WHERE state IN ('queued','claimed','cancelling') LIMIT 2");
    let mut rows = query
        .build_query_as::<CampaignRow>()
        .fetch_all(&mut **transaction)
        .await?;
    if rows.len() > 1 {
        return Err(CampaignApplyError::CorruptStoredState);
    }
    let Some(row) = rows.pop() else {
        return Ok(None);
    };
    let mut campaign = row.decode().map_err(load_error)?;
    let approval = match &campaign.state {
        CampaignState::Queued { approval } => approval.clone(),
        CampaignState::Claimed { lease, .. } | CampaignState::Cancelling { lease, .. } => {
            if lease.expires_at <= command.now {
                quarantine_one(
                    transaction,
                    &mut campaign,
                    CampaignQuarantine::Interrupted,
                    command.now,
                )
                .await?;
            }
            return Ok(None);
        }
        CampaignState::Draft
        | CampaignState::Completed { .. }
        | CampaignState::Cancelled { .. }
        | CampaignState::Unknown { .. }
        | CampaignState::Quarantined { .. } => return Err(CampaignApplyError::CorruptStoredState),
    };
    if campaign.configuration_binding != command.configuration_binding {
        return Err(CampaignCommandError::ConfigurationChanged.into());
    }
    if approval.instance_version != instance_version(transaction).await? {
        return Err(CampaignCommandError::StaleClaim.into());
    }
    campaign.state = CampaignState::Claimed {
        approval,
        lease: CampaignLease {
            fence: CampaignFence(Uuid::new_v4()),
            claimed_at: command.now,
            renewed_at: command.now,
            expires_at: command
                .now
                .checked_add(time::Duration::seconds(i64::from(command.lease_seconds)))
                .ok_or(CampaignCommandError::InvalidValue)?,
        },
    };
    persist(transaction, &mut campaign, command.now).await?;
    Ok(Some(campaign))
}

pub(crate) async fn renew_campaign_claim(
    transaction: &mut Transaction<'_, Sqlite>,
    command: RenewCampaignClaim,
    now: OffsetDateTime,
) -> Result<Campaign, CampaignApplyError> {
    let duration = command.expires_at - now;
    if duration <= time::Duration::ZERO
        || duration > time::Duration::seconds(i64::from(MAX_CLAIM_SECONDS))
    {
        return Err(CampaignCommandError::InvalidValue.into());
    }
    let mut campaign = load(transaction, command.campaign_id).await?;
    if campaign.configuration_binding != command.configuration_binding {
        return Err(CampaignCommandError::ConfigurationChanged.into());
    }
    let CampaignState::Claimed { approval, lease } = &mut campaign.state else {
        return Err(CampaignCommandError::StaleClaim.into());
    };
    if lease.fence != command.fence
        || lease.expires_at <= now
        || campaign.updated_at > now
        || approval.instance_version != instance_version(transaction).await?
    {
        return Err(CampaignCommandError::StaleClaim.into());
    }
    if command.expires_at <= lease.expires_at {
        return Ok(campaign);
    }
    lease.renewed_at = now;
    lease.expires_at = command.expires_at;
    persist(transaction, &mut campaign, now).await?;
    Ok(campaign)
}

/// The subscriber store calls this inside the SAME transaction that checks
/// consent/budgets and records a durable unique attempt. It grants no separate
/// permission that can outlive the transaction.
pub(crate) async fn require_campaign_admission(
    transaction: &mut Transaction<'_, Sqlite>,
    campaign_id: CampaignId,
    fence: CampaignFence,
    configuration_binding: [u8; 32],
    now: OffsetDateTime,
) -> Result<CampaignApproval, CampaignApplyError> {
    let campaign = load(transaction, campaign_id).await?;
    if campaign.configuration_binding != configuration_binding {
        return Err(CampaignCommandError::ConfigurationChanged.into());
    }
    let CampaignState::Claimed { approval, lease } = campaign.state else {
        return Err(CampaignCommandError::StaleClaim.into());
    };
    if lease.fence != fence
        || lease.expires_at <= now
        || campaign.updated_at > now
        || approval.instance_version != instance_version(transaction).await?
    {
        return Err(CampaignCommandError::StaleClaim.into());
    }
    require_enabled_owner(transaction, approval.owner)
        .await
        .map_err(|error| match error {
            AuthApplyError::Command(
                AuthCommandError::NotFound | AuthCommandError::ScopeEscalation,
            ) => CampaignCommandError::ApprovalRevoked.into(),
            error => auth_error(error),
        })?;
    Ok(approval)
}

pub(crate) async fn finish_campaign(
    transaction: &mut Transaction<'_, Sqlite>,
    command: FinishCampaign,
) -> Result<Campaign, CampaignApplyError> {
    let fingerprint = finish_fingerprint(&command);
    if let Some(result) = replay_finish(transaction, &command, fingerprint).await? {
        return Ok(result);
    }
    let mut campaign = load(transaction, command.campaign_id).await?;
    let (approval, lease, cancelling) = match &campaign.state {
        CampaignState::Claimed { approval, lease } => (approval.clone(), lease, false),
        CampaignState::Cancelling { approval, lease } => (approval.clone(), lease, true),
        CampaignState::Draft
        | CampaignState::Queued { .. }
        | CampaignState::Completed { .. }
        | CampaignState::Cancelled { .. }
        | CampaignState::Unknown { .. }
        | CampaignState::Quarantined { .. } => return Err(CampaignCommandError::StaleClaim.into()),
    };
    if lease.fence != command.fence
        || approval.instance_version != instance_version(transaction).await?
    {
        return Err(CampaignCommandError::StaleClaim.into());
    }
    let counts = subscriber_store::campaign_counts(transaction, command.campaign_id).await?;
    match (command.intent, cancelling) {
        (FinishIntent::Complete, true) | (FinishIntent::Cancel, false) => {
            return Err(CampaignCommandError::InvalidTransition.into());
        }
        (FinishIntent::Complete, false) | (FinishIntent::Cancel, true) => {}
    }
    campaign.state = if counts.unknown > 0 {
        CampaignState::Unknown { approval, counts }
    } else if cancelling {
        CampaignState::Cancelled {
            approval: Some(approval),
            counts,
        }
    } else {
        CampaignState::Completed { approval, counts }
    };
    campaign.validate()?;
    persist(transaction, &mut campaign, command.now).await?;
    sqlx::query("INSERT INTO mail_campaign_finishes (campaign_id,fence,intent,command_fingerprint,result) VALUES (?,?,?,?,?)")
        .bind(command.campaign_id.0.as_bytes().as_slice()).bind(command.fence.0.as_bytes().as_slice())
        .bind(match command.intent {FinishIntent::Complete=>"complete",FinishIntent::Cancel=>"cancel"}).bind(fingerprint.as_slice()).bind(encode_record(&campaign)?)
        .execute(&mut **transaction).await?;
    Ok(campaign)
}

fn finish_fingerprint(command: &FinishCampaign) -> [u8; 32] {
    let mut builder = CommandFingerprintBuilder::new("mail.campaign.finish");
    builder.uuid(&command.campaign_id.0);
    builder.uuid(&command.fence.0);
    builder.field(match command.intent {
        FinishIntent::Complete => b"complete",
        FinishIntent::Cancel => b"cancel",
    });
    builder.finish()
}

async fn replay_finish(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &FinishCampaign,
    fingerprint: [u8; 32],
) -> Result<Option<Campaign>, CampaignApplyError> {
    let row: Option<(Vec<u8>, Vec<u8>, Option<String>)> = sqlx::query_as("SELECT fence,command_fingerprint,CASE WHEN length(CAST(result AS BLOB)) <= 262144 THEN result END FROM mail_campaign_finishes WHERE campaign_id = ?")
        .bind(command.campaign_id.0.as_bytes().as_slice()).fetch_optional(&mut **transaction).await?;
    let Some((fence, saved, result)) = row else {
        return Ok(None);
    };
    if fence != command.fence.0.as_bytes() || saved != fingerprint {
        return Err(CampaignCommandError::StaleClaim.into());
    }
    let result = decode_record(result.as_deref()).map_err(load_error)?;
    if result.campaign_id != command.campaign_id {
        return Err(CampaignApplyError::CorruptStoredState);
    }
    Ok(Some(result))
}

/// An authenticated provider result settles exactly one previously unknown
/// attempt. Apply a delta to durable totals: retention may already have removed
/// other attempts, so recounting the remaining ledger would lose history.
pub(crate) async fn reconcile_campaign_acceptance(
    transaction: &mut Transaction<'_, Sqlite>,
    campaign_id: CampaignId,
    now: OffsetDateTime,
) -> Result<(), CampaignApplyError> {
    let mut campaign = load(transaction, campaign_id).await?;
    campaign.state = match campaign.state {
        CampaignState::Claimed { .. } | CampaignState::Cancelling { .. } => return Ok(()),
        CampaignState::Unknown {
            approval,
            mut counts,
        } => {
            settle_unknown_count(&mut counts)?;
            if counts.unknown > 0 {
                CampaignState::Unknown { approval, counts }
            } else {
                let intent:Option<String>=sqlx::query_scalar("SELECT CASE WHEN length(intent)<=8 THEN intent END FROM mail_campaign_finishes WHERE campaign_id=?").bind(campaign_id.0.as_bytes().as_slice()).fetch_optional(&mut **transaction).await?.flatten();
                match intent.as_deref() {
                    Some("complete") => CampaignState::Completed { approval, counts },
                    Some("cancel") => CampaignState::Cancelled {
                        approval: Some(approval),
                        counts,
                    },
                    _ => return Err(CampaignApplyError::CorruptStoredState),
                }
            }
        }
        CampaignState::Quarantined {
            approval,
            progress: CampaignProgress::Known(mut counts),
            reason,
        } => {
            settle_unknown_count(&mut counts)?;
            CampaignState::Quarantined {
                approval,
                progress: CampaignProgress::Known(counts),
                reason,
            }
        }
        CampaignState::Quarantined {
            progress: CampaignProgress::Unreconciled,
            ..
        } => return Ok(()),
        CampaignState::Draft
        | CampaignState::Queued { .. }
        | CampaignState::Completed { .. }
        | CampaignState::Cancelled { .. } => return Err(CampaignApplyError::CorruptStoredState),
    };
    let updated = now.max(campaign.updated_at);
    persist(transaction, &mut campaign, updated).await
}

fn settle_unknown_count(counts: &mut CampaignCounts) -> Result<(), CampaignApplyError> {
    counts.unknown = counts
        .unknown
        .checked_sub(1)
        .ok_or(CampaignApplyError::CorruptStoredState)?;
    counts.accepted = counts
        .accepted
        .checked_add(1)
        .ok_or(CampaignApplyError::CorruptStoredState)?;
    counts
        .validate()
        .map_err(|_| CampaignApplyError::CorruptStoredState)
}

pub(crate) async fn quarantine_interrupted_campaigns(
    transaction: &mut Transaction<'_, Sqlite>,
    now: OffsetDateTime,
) -> Result<u64, CampaignApplyError> {
    quarantine(transaction, CampaignQuarantine::Interrupted, now).await
}

/// Recipient retention must close admission before its uniqueness records leave
/// storage. The durable ledger supplies known aggregate progress at this point.
pub(crate) async fn quarantine_recipient_history(
    transaction: &mut Transaction<'_, Sqlite>,
    campaign_id: CampaignId,
    counts: CampaignCounts,
    now: OffsetDateTime,
) -> Result<(), CampaignApplyError> {
    let mut campaign = load(transaction, campaign_id).await?;
    let approval = match campaign.state {
        CampaignState::Claimed { approval, .. } | CampaignState::Cancelling { approval, .. } => {
            approval
        }
        CampaignState::Draft
        | CampaignState::Queued { .. }
        | CampaignState::Completed { .. }
        | CampaignState::Cancelled { .. }
        | CampaignState::Unknown { .. }
        | CampaignState::Quarantined { .. } => return Ok(()),
    };
    campaign.state = CampaignState::Quarantined {
        approval: Some(approval),
        progress: CampaignProgress::Known(counts),
        reason: CampaignQuarantine::Interrupted,
    };
    persist(transaction, &mut campaign, now).await
}

/// Runs under the offline restore's database lock and acceptance transaction.
/// Receipts remain immutable; no restored approval or outstanding claim sends.
pub(crate) async fn quarantine_restored_campaigns(
    transaction: &mut Transaction<'_, Sqlite>,
    restore_id: Uuid,
    now: OffsetDateTime,
) -> Result<u64, CampaignApplyError> {
    quarantine(transaction, CampaignQuarantine::Restore { restore_id }, now).await
}

pub(crate) async fn quarantine_reset_campaigns(
    transaction: &mut Transaction<'_, Sqlite>,
    reset_id: Uuid,
    now: OffsetDateTime,
) -> Result<u64, CampaignApplyError> {
    quarantine(
        transaction,
        CampaignQuarantine::FeedbackReset { reset_id },
        now,
    )
    .await
}

async fn quarantine(
    transaction: &mut Transaction<'_, Sqlite>,
    reason: CampaignQuarantine,
    now: OffsetDateTime,
) -> Result<u64, CampaignApplyError> {
    let states = match reason {
        CampaignQuarantine::Interrupted => "('claimed','cancelling')",
        CampaignQuarantine::Restore { .. } | CampaignQuarantine::FeedbackReset { .. } => {
            "('draft','queued','claimed','cancelling','unknown')"
        }
    };
    let mut count = 0;
    loop {
        let mut query = QueryBuilder::new(SELECT_CAMPAIGNS);
        query
            .push(" WHERE state IN ")
            .push(states)
            .push(" ORDER BY campaign_id LIMIT 100");
        let rows = query
            .build_query_as::<CampaignRow>()
            .fetch_all(&mut **transaction)
            .await?;
        if rows.is_empty() {
            return Ok(count);
        }
        count += rows.len() as u64;
        if count > MAX_CAMPAIGNS as u64 {
            return Err(CampaignApplyError::CorruptStoredState);
        }
        for row in rows {
            let mut campaign = row.decode().map_err(load_error)?;
            quarantine_one(transaction, &mut campaign, reason, now).await?;
        }
    }
}

async fn quarantine_one(
    transaction: &mut Transaction<'_, Sqlite>,
    campaign: &mut Campaign,
    reason: CampaignQuarantine,
    now: OffsetDateTime,
) -> Result<(), CampaignApplyError> {
    let (approval, progress) = match campaign.state.clone() {
        CampaignState::Draft => (None, CampaignProgress::Known(CampaignCounts::default())),
        CampaignState::Queued { approval } => (
            Some(approval),
            CampaignProgress::Known(CampaignCounts::default()),
        ),
        CampaignState::Claimed { approval, .. } | CampaignState::Cancelling { approval, .. } => (
            Some(approval),
            CampaignProgress::Known(
                subscriber_store::campaign_counts(transaction, campaign.campaign_id).await?,
            ),
        ),
        CampaignState::Unknown { approval, counts } => {
            (Some(approval), CampaignProgress::Known(counts))
        }
        CampaignState::Completed { .. }
        | CampaignState::Cancelled { .. }
        | CampaignState::Quarantined { .. } => return Err(CampaignApplyError::CorruptStoredState),
    };
    campaign.state = CampaignState::Quarantined {
        approval,
        progress,
        reason,
    };
    persist(transaction, campaign, now.max(campaign.updated_at)).await
}

#[derive(FromRow)]
struct ReceiptRow {
    principal_kind: String,
    actor_user_id: Option<Vec<u8>>,
    session_id: Option<Vec<u8>>,
    agent_credential_id: Option<Vec<u8>>,
    action: String,
    outcome: String,
    command_fingerprint: Option<Vec<u8>>,
    campaign_id: Option<Vec<u8>>,
    result: Option<String>,
}

async fn replay(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: &MutationAuditContext,
    action: &'static str,
    fingerprint: [u8; 32],
) -> Result<Option<Campaign>, CampaignApplyError> {
    let row = sqlx::query_as::<_, ReceiptRow>(
        "SELECT audit.principal_kind,audit.actor_user_id,audit.session_id,audit.agent_credential_id,audit.action,audit.outcome,receipt.command_fingerprint,receipt.campaign_id,CASE WHEN length(CAST(receipt.result AS BLOB)) <= 262144 THEN receipt.result END AS result \
         FROM admin_audit_events AS audit LEFT JOIN mail_campaign_receipts AS receipt ON receipt.audit_event_id = audit.audit_event_id WHERE audit.idempotency_key = ?",
    ).bind(audit.idempotency_key.0.as_bytes().as_slice()).fetch_optional(&mut **transaction).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let principal = decode_audit_principal(
        &row.principal_kind,
        row.actor_user_id.as_deref(),
        row.session_id.as_deref(),
        row.agent_credential_id.as_deref(),
    )
    .map_err(|_| CampaignApplyError::CorruptStoredState)?;
    if principal != audit.principal
        || row.action != action
        || row.command_fingerprint.as_deref() != Some(fingerprint.as_slice())
    {
        return Err(CampaignCommandError::IdempotencyConflict.into());
    }
    if row.outcome != "succeeded" {
        return Err(CampaignApplyError::CorruptStoredState);
    }
    let result = decode_record(row.result.as_deref()).map_err(load_error)?;
    if row.campaign_id.as_deref() != Some(result.campaign_id.0.as_bytes().as_slice()) {
        return Err(CampaignApplyError::CorruptStoredState);
    }
    Ok(Some(result))
}

async fn receipt(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: &MutationAuditContext,
    action: &'static str,
    fingerprint: [u8; 32],
    campaign: &Campaign,
) -> Result<(), CampaignApplyError> {
    append_success_audit(transaction, audit, campaign.updated_at, action)
        .await
        .map_err(auth_error)?;
    sqlx::query("INSERT INTO mail_campaign_receipts (idempotency_key,audit_event_id,command_fingerprint,campaign_id,result) VALUES (?,?,?,?,?)")
        .bind(audit.idempotency_key.0.as_bytes().as_slice()).bind(audit.audit_event_id.as_uuid().as_bytes().as_slice())
        .bind(fingerprint.as_slice()).bind(campaign.campaign_id.0.as_bytes().as_slice()).bind(encode_record(campaign)?)
        .execute(&mut **transaction).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use maincopy_shared::auth::{
        AdminAuditEventId, AdminSessionId, AgentCredentialId, UserId, UserRole, UserStatus,
    };
    use markdown_compiler::{PostId, PostRevisionDigest, SiteSnapshotDigest};
    use sqlx::{Connection as _, SqliteConnection, sqlite::SqliteConnectOptions};
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        config::{
            DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
            DatabaseWriterQueueCapacity,
        },
        database::{self, store::DatabaseStore},
        domain::{
            auth::store::{
                AdminMutationKey, AuditPrincipalReference, ConfiguredLoginProviders,
                ReplaceUserRoles, SetUserStatus,
            },
            mail::{
                announcement::announcement_content_digest,
                identity::EmailAddress,
                ses::MessageId,
                subscriber::{
                    AdmitCampaignRecipient, ApplyFeedback, AttemptOutcome, BeginFeedbackRun,
                    ClaimConfirmation, ConfirmEnrollment, ControlOutcome, DeliveryAdmission,
                    DeliveryPermit, EnrollmentRequestResult, FeedbackHealth, FeedbackKind,
                    FeedbackObservation, FinishAttempt, RecipientHandle, RecordFeedbackObservation,
                    RequestEnrollment, ResetSubscriberConsent, SubmissionOutcome,
                    SubscriberCommandError, SubscriberDigest, SubscriberMode, SubscriberPolicy,
                    store::{SubscriberMutationError, cleanup as cleanup_subscribers},
                },
            },
            publication::store::InstallStartupSnapshot,
        },
        restore::RestoreError,
    };

    struct Harness {
        root: tempfile::TempDir,
        store: DatabaseStore,
        shutdown: CancellationToken,
        writer: JoinHandle<()>,
        feedback_run: Option<Uuid>,
    }

    impl Harness {
        async fn start() -> Self {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("state/maincopy.db");
            database::bootstrap(configuration(&path))
                .await
                .unwrap()
                .close()
                .await
                .unwrap();
            let mut connection = SqliteConnection::connect_with(
                &SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
            seed(&mut connection).await;
            connection.close().await.unwrap();
            let mut harness = Self::open(root).await;
            harness
                .store
                .subscribers
                .initialize_controls([9; 32])
                .await
                .unwrap();
            harness
                .store
                .subscribers
                .set_policy(SubscriberPolicy {
                    configuration_binding: [7; 32],
                    mode: SubscriberMode::Enabled,
                    max_daily_messages: 100,
                    max_daily_confirmations: 100,
                    max_campaign_recipients: 100,
                })
                .await
                .unwrap();
            let run = harness
                .store
                .subscribers
                .begin_feedback_run(BeginFeedbackRun {
                    provider_now: OffsetDateTime::now_utc(),
                    configuration_binding: [7; 32],
                    source_binding: [6; 32],
                    retention_seconds: 1_209_600,
                })
                .await
                .unwrap();
            harness.feedback_run = Some(run.run_id);
            harness.healthy_feedback().await;
            harness
        }

        async fn open(root: tempfile::TempDir) -> Self {
            let path = root.path().join("state/maincopy.db");
            let database = database::bootstrap(configuration(&path)).await.unwrap();
            let (store, writer) = database.into_store(16);
            let shutdown = CancellationToken::new();
            let stopping = shutdown.clone();
            let writer = tokio::spawn(async move {
                writer.run(stopping).await.unwrap();
            });
            Self {
                root,
                store,
                shutdown,
                writer,
                feedback_run: None,
            }
        }

        async fn healthy_feedback(&self) {
            self.store
                .subscribers
                .record_feedback_observation(RecordFeedbackObservation {
                    configuration_binding: [7; 32],
                    run_id: self
                        .feedback_run
                        .expect("fresh fixtures own the feedback run"),
                    source_binding: [6; 32],
                    retention_seconds: 1_209_600,
                    observation: FeedbackObservation::Observed {
                        provider_now: OffsetDateTime::now_utc(),
                        drained: true,
                    },
                })
                .await
                .unwrap();
        }

        async fn close(self) -> tempfile::TempDir {
            drop(self.store);
            self.shutdown.cancel();
            self.writer.await.unwrap();
            self.root
        }

        async fn draft(&self, key: u128) -> Campaign {
            self.store.mail.create_draft(create(key)).await.unwrap()
        }

        async fn queued(&self, key: u128) -> Campaign {
            let draft = self.draft(key).await;
            self.store
                .mail
                .approve(approve(&draft, key + 1))
                .await
                .unwrap()
        }

        async fn claimed(&self, key: u128) -> Campaign {
            self.claimed_at(key, at(120)).await
        }

        async fn claimed_at(&self, key: u128, now: OffsetDateTime) -> Campaign {
            self.queued(key).await;
            self.store
                .mail
                .claim(ClaimCampaign {
                    configuration_binding: [7; 32],
                    lease_seconds: 30,
                    now,
                })
                .await
                .unwrap()
                .unwrap()
        }

        async fn recipient(&self, mailbox: &str) -> RecipientHandle {
            let recipient = RecipientHandle {
                enrollment: Uuid::new_v4(),
                generation: Uuid::new_v4(),
            };
            let confirmation_attempt = Uuid::new_v4();
            assert_eq!(
                self.store
                    .subscribers
                    .request_enrollment(RequestEnrollment {
                        address: EmailAddress::parse(mailbox).unwrap(),
                        mailbox_digest: SubscriberDigest::from_bytes(
                            *blake3::keyed_hash(&[8; 32], mailbox.as_bytes()).as_bytes()
                        ),
                        enrollment: recipient.enrollment,
                        generation: recipient.generation,
                        confirmation_attempt,
                        configuration_binding: [7; 32],
                    })
                    .await
                    .unwrap(),
                EnrollmentRequestResult::Queued
            );
            let nonce_digest = *blake3::hash(Uuid::new_v4().as_bytes()).as_bytes();
            let expires_at = OffsetDateTime::from_unix_timestamp(
                OffsetDateTime::now_utc().unix_timestamp() + 3600,
            )
            .unwrap();
            let DeliveryAdmission::Ready(permit) = self
                .store
                .subscribers
                .claim_confirmation(ClaimConfirmation {
                    attempt_id: confirmation_attempt,
                    nonce_digest: SubscriberDigest::from_bytes(nonce_digest),
                    expires_at,
                    configuration_binding: [7; 32],
                })
                .await
                .unwrap()
            else {
                panic!("current pending confirmation is admitted")
            };
            self.finish_attempt(permit, accepted()).await;
            assert_eq!(
                self.store
                    .subscribers
                    .confirm(ConfirmEnrollment {
                        enrollment: recipient.enrollment,
                        generation: recipient.generation,
                        nonce_digest: SubscriberDigest::from_bytes(nonce_digest),
                        expires_at,
                    })
                    .await
                    .unwrap(),
                ControlOutcome::Changed
            );
            recipient
        }

        async fn admit(&self, campaign: &Campaign, recipient: &RecipientHandle) -> DeliveryPermit {
            let DeliveryAdmission::Ready(permit) = self
                .store
                .subscribers
                .admit_campaign_recipient(admission(campaign, recipient))
                .await
                .unwrap()
            else {
                panic!("confirmed recipient receives exactly one fresh attempt")
            };
            permit
        }

        async fn finish_attempt(&self, permit: DeliveryPermit, outcome: SubmissionOutcome) {
            let permit = permit.into_attempt();
            self.store
                .subscribers
                .finish_attempt(FinishAttempt {
                    mail_epoch: permit.mail_epoch,
                    attempt_id: permit.attempt_id,
                    attempt_fence: permit.attempt_fence,
                    outcome,
                })
                .await
                .unwrap();
        }
    }

    fn configuration(path: &Path) -> DatabaseConfigurationView<'_> {
        DatabaseConfigurationView {
            path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(16).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        }
    }

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    fn content() -> CampaignContent {
        let post_id = PostId::parse("11111111-1111-4111-8111-111111111111").unwrap();
        let revision = PostRevisionDigest::from_bytes([2; 32]);
        let canonical_url = "https://example.test/posts/article";
        let subject = "Reviewed public article";
        let text = "Public description and article link.";
        let html = "<p>Public description and article link.</p>";
        CampaignContent {
            content_digest: announcement_content_digest(
                &post_id,
                &revision,
                canonical_url,
                subject,
                text,
                html,
            ),
            post_id,
            revision,
            snapshot: SiteSnapshotDigest::from_bytes([3; 32]),
            site_version: 1,
            template_version: 1,
            canonical_url: canonical_url.into(),
            subject: subject.into(),
            text: text.into(),
            html: html.into(),
        }
    }

    fn audit(key: u128, user: u128, session: u128) -> MutationAuditContext {
        MutationAuditContext {
            audit_event_id: AdminAuditEventId::from_uuid(Uuid::from_u128(key + 1_000_000)),
            principal: AuditPrincipalReference::BrowserSession {
                user_id: UserId::from_uuid(Uuid::from_u128(user)),
                session_id: AdminSessionId::from_uuid(Uuid::from_u128(session)),
            },
            request_id: None,
            idempotency_key: AdminMutationKey(Uuid::from_u128(key)),
        }
    }

    fn create(key: u128) -> CreateCampaign {
        CreateCampaign {
            proposed_id: CampaignId(Uuid::from_u128(key)),
            content: content(),
            configuration_binding: [7; 32],
            audit: audit(key, 1, 1001),
            now: at(100),
        }
    }

    fn approve(campaign: &Campaign, key: u128) -> ApproveCampaign {
        ApproveCampaign {
            campaign_id: campaign.campaign_id,
            expected_version: campaign.version,
            configuration_binding: [7; 32],
            audit: audit(key, 1, 1001),
            now: at(110),
        }
    }

    fn cancel(campaign: &Campaign, key: u128) -> CancelCampaign {
        CancelCampaign {
            campaign_id: campaign.campaign_id,
            expected_version: campaign.version,
            audit: audit(key, 1, 1001),
            now: at(125),
        }
    }

    fn fence(campaign: &Campaign) -> CampaignFence {
        match &campaign.state {
            CampaignState::Claimed { lease, .. } | CampaignState::Cancelling { lease, .. } => {
                lease.fence
            }
            _ => panic!("fixture must have a live claim"),
        }
    }

    fn admission(campaign: &Campaign, recipient: &RecipientHandle) -> AdmitCampaignRecipient {
        AdmitCampaignRecipient {
            campaign_id: campaign.campaign_id,
            campaign_fence: fence(campaign),
            configuration_binding: campaign.configuration_binding,
            enrollment: recipient.enrollment,
            generation: recipient.generation,
            attempt_id: Uuid::new_v4(),
        }
    }

    fn accepted() -> SubmissionOutcome {
        SubmissionOutcome::Accepted(MessageId::parse(&Uuid::new_v4().to_string()).unwrap())
    }

    async fn seed(connection: &mut SqliteConnection) {
        sqlx::query("INSERT INTO instance_identity VALUES (?, ?, 1, 0)")
            .bind(1_i64)
            .bind(Uuid::from_u128(9).as_bytes().as_slice())
            .execute(&mut *connection)
            .await
            .unwrap();
        for (id, role, status) in [
            (1_u128, "owner", "enabled"),
            (2, "publisher", "enabled"),
            (3, "administrator", "enabled"),
            (4, "owner", "disabled"),
            (5, "owner", "enabled"),
        ] {
            sqlx::query("INSERT INTO users VALUES (?, ?, 1, 0, 0)")
                .bind(Uuid::from_u128(id).as_bytes().as_slice())
                .bind(status)
                .execute(&mut *connection)
                .await
                .unwrap();
            sqlx::query("INSERT INTO user_roles VALUES (?, ?, NULL, 0)")
                .bind(Uuid::from_u128(id).as_bytes().as_slice())
                .bind(role)
                .execute(&mut *connection)
                .await
                .unwrap();
        }
        for (session, user, fresh, revoked, instance) in [
            (1001_u128, 1_u128, 500_i64, None, 1_i64),
            (1002, 2, 500, None, 1),
            (1003, 3, 500, None, 1),
            (1004, 4, 500, None, 1),
            (1005, 1, 90, None, 1),
            (1006, 1, 500, Some(50_i64), 1),
            (1007, 1, 500, None, 2),
            (1008, 1, 500, None, 1),
            (1009, 5, 500, None, 1),
        ] {
            let fresh_until = if fresh == 500 {
                OffsetDateTime::now_utc() + time::Duration::hours(1)
            } else {
                at(fresh)
            };
            let expires_at = OffsetDateTime::now_utc() + time::Duration::days(1);
            sqlx::query("INSERT INTO browser_sessions (session_id,user_id,provider,session_token_digest,csrf_token_digest,instance_version,version,authenticated_at_ns,fresh_until_ns,expires_at_ns,revoked_at_ns,last_seen_at_ns) VALUES (?,?,'nostr',?,?,?,1,0,?,?,?,0)")
                .bind(Uuid::from_u128(session).as_bytes().as_slice()).bind(Uuid::from_u128(user).as_bytes().as_slice())
                .bind(vec![session as u8;32]).bind(vec![(session+100) as u8;32]).bind(instance)
                .bind(i64::try_from(fresh_until.unix_timestamp_nanos()).unwrap())
                .bind(i64::try_from(expires_at.unix_timestamp_nanos()).unwrap())
                .bind(revoked.map(|value| value*1_000_000_000))
                .execute(&mut *connection).await.unwrap();
        }
        let content = content();
        sqlx::query("INSERT INTO site_revisions VALUES (?,1,0,NULL)")
            .bind(content.snapshot.as_bytes().as_slice())
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("INSERT INTO site_state VALUES (1,?,1)")
            .bind(content.snapshot.as_bytes().as_slice())
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("INSERT INTO post_revisions VALUES (?,?,'publishable',0,'article',NULL)")
            .bind(content.post_id.as_uuid().as_bytes().as_slice())
            .bind(content.revision.as_bytes().as_slice())
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("INSERT INTO canonical_publications (publication_id,command_kind,stable_post_id,pinned_post_digest,content_tree_digest,accepted_preview_digest,state,version,scheduled_at_ns,activation_at_ns,activation_site_digest,published_at_ns,current_published_digest) VALUES (?,'immediate',?,?,?,?,'published',3,0,0,?,0,?)")
            .bind(Uuid::from_u128(10).as_bytes().as_slice()).bind(content.post_id.as_uuid().as_bytes().as_slice())
            .bind(content.revision.as_bytes().as_slice()).bind([4_u8;32].as_slice()).bind([5_u8;32].as_slice())
            .bind(content.snapshot.as_bytes().as_slice()).bind(content.revision.as_bytes().as_slice()).execute(&mut *connection).await.unwrap();
    }

    #[tokio::test]
    async fn campaign_receipts_bind_exact_body_session_and_versions_across_restart() {
        let harness = Harness::start().await;
        let command = create(2000);
        let draft = harness
            .store
            .mail
            .create_draft(command.clone())
            .await
            .unwrap();
        assert_eq!(
            harness
                .store
                .mail
                .create_draft(command.clone())
                .await
                .unwrap(),
            draft
        );
        let mut conflicting = command.clone();
        conflicting.configuration_binding = [8; 32];
        assert_eq!(
            harness.store.mail.create_draft(conflicting).await,
            Err(CampaignCommandError::IdempotencyConflict.into())
        );
        let mut conflicting = command;
        conflicting.audit = audit(2000, 1, 1008);
        assert_eq!(
            harness.store.mail.create_draft(conflicting).await,
            Err(CampaignCommandError::IdempotencyConflict.into())
        );
        assert_eq!(
            harness.store.mail.create_draft(create(2010)).await,
            Err(CampaignCommandError::ActiveCampaign.into())
        );
        let mut stale = approve(&draft, 2001);
        stale.expected_version = CampaignVersion::try_from(9).unwrap();
        assert_eq!(
            harness.store.mail.approve(stale).await,
            Err(CampaignCommandError::StaleVersion.into())
        );
        let mut wrong_config = approve(&draft, 2001);
        wrong_config.configuration_binding = [9; 32];
        assert_eq!(
            harness.store.mail.approve(wrong_config).await,
            Err(CampaignCommandError::ConfigurationChanged.into())
        );
        let approval = approve(&draft, 2001);
        let queued = harness.store.mail.approve(approval.clone()).await.unwrap();
        assert_eq!(
            harness.store.mail.active_campaign().await.unwrap(),
            Some(queued.clone())
        );
        assert!(matches!(
            harness.store.mail.list(None, 0).await,
            Err(CampaignLoadError::InvalidLimit)
        ));
        let root = harness.close().await;
        let harness = Harness::open(root).await;
        assert_eq!(harness.store.mail.approve(approval).await.unwrap(), queued);
        assert_eq!(
            harness.store.mail.create_draft(create(2000)).await.unwrap(),
            draft
        );
        assert_eq!(
            harness
                .store
                .mail
                .campaign(queued.campaign_id)
                .await
                .unwrap(),
            Some(queued)
        );
        harness.close().await;
    }

    #[tokio::test]
    async fn campaign_writes_require_current_fresh_owner_browser_authority() {
        let harness = Harness::start().await;
        for (offset, user, session) in [
            (0, 2, 1002),
            (1, 3, 1003),
            (2, 4, 1004),
            (3, 1, 1005),
            (4, 1, 1006),
            (5, 1, 1007),
        ] {
            let mut command = create(2100 + offset);
            command.audit = audit(2100 + offset, user, session);
            assert_eq!(
                harness.store.mail.create_draft(command).await,
                Err(CampaignCommandError::Forbidden.into())
            );
        }
        for (offset, principal) in [
            AuditPrincipalReference::AgentCredential {
                user_id: UserId::from_uuid(Uuid::from_u128(1)),
                credential_id: AgentCredentialId::from_uuid(Uuid::from_u128(800)),
            },
            AuditPrincipalReference::Offline {
                user_id: Some(UserId::from_uuid(Uuid::from_u128(1))),
            },
            AuditPrincipalReference::Unauthenticated,
        ]
        .into_iter()
        .enumerate()
        {
            let mut command = create(2200 + offset as u128);
            command.audit.principal = principal;
            assert_eq!(
                harness.store.mail.create_draft(command).await,
                Err(CampaignCommandError::Forbidden.into())
            );
        }
        assert!(
            harness
                .store
                .mail
                .list(None, 10)
                .await
                .unwrap()
                .items
                .is_empty()
        );
        harness.close().await;
    }

    #[tokio::test]
    async fn approval_rechecks_public_site_and_preserves_the_unapproved_draft() {
        let harness = Harness::start().await;
        let mut stale = create(2300);
        stale.content.site_version = 2;
        assert_eq!(
            harness.store.mail.create_draft(stale).await,
            Err(CampaignCommandError::PublicationChanged.into())
        );
        let draft = harness.draft(2300).await;
        harness
            .store
            .publications
            .install_startup_snapshot(InstallStartupSnapshot {
                expected: Some(SiteHead {
                    digest: draft.content.snapshot.clone(),
                    version: 1,
                }),
                candidate_digest: SiteSnapshotDigest::from_bytes([9; 32]),
                activated_at: at(105),
                source_commit: None,
                posts: vec![],
            })
            .await
            .unwrap();
        assert_eq!(
            harness.store.mail.approve(approve(&draft, 2301)).await,
            Err(CampaignCommandError::PublicationChanged.into())
        );
        assert_eq!(
            harness
                .store
                .mail
                .campaign(draft.campaign_id)
                .await
                .unwrap(),
            Some(draft)
        );
        harness.close().await;
    }

    #[tokio::test]
    async fn approval_requires_current_enabled_feedback_but_receipt_replay_survives_pause() {
        for feedback in [
            FeedbackHealth::Unavailable,
            FeedbackHealth::ReconciliationRequired,
        ] {
            let harness = Harness::start().await;
            let draft = harness.draft(2350).await;
            let approval = approve(&draft, 2351);
            harness
                .store
                .subscribers
                .record_feedback_health([7; 32], feedback)
                .await
                .unwrap();
            assert_eq!(
                harness.store.mail.approve(approval.clone()).await,
                Err(CampaignCommandError::SendingUnavailable.into())
            );
            assert_eq!(
                harness
                    .store
                    .mail
                    .campaign(draft.campaign_id)
                    .await
                    .unwrap(),
                Some(draft)
            );
            harness.healthy_feedback().await;
            match feedback {
                FeedbackHealth::Unavailable => {
                    let queued = harness.store.mail.approve(approval.clone()).await.unwrap();
                    harness.store.subscribers.pause().await.unwrap();
                    assert_eq!(harness.store.mail.approve(approval).await.unwrap(), queued);
                }
                FeedbackHealth::ReconciliationRequired => {
                    assert_eq!(
                        harness.store.mail.approve(approval).await,
                        Err(CampaignCommandError::SendingUnavailable.into())
                    );
                }
                FeedbackHealth::Healthy => unreachable!("the scenario begins with failed feedback"),
            }
            harness.close().await;
        }
        let harness = Harness::start().await;
        let draft = harness.draft(2360).await;
        let approval = approve(&draft, 2361);
        harness.store.subscribers.pause().await.unwrap();
        assert_eq!(
            harness.store.mail.approve(approval.clone()).await,
            Err(CampaignCommandError::SendingUnavailable.into())
        );
        harness
            .store
            .subscribers
            .set_policy(SubscriberPolicy {
                configuration_binding: [8; 32],
                mode: SubscriberMode::Enabled,
                max_daily_messages: 100,
                max_daily_confirmations: 100,
                max_campaign_recipients: 100,
            })
            .await
            .unwrap();

        assert_eq!(
            harness.store.mail.approve(approval).await,
            Err(CampaignCommandError::ConfigurationChanged.into())
        );
        assert_eq!(
            harness
                .store
                .mail
                .campaign(draft.campaign_id)
                .await
                .unwrap(),
            Some(draft)
        );
        harness.close().await;
    }

    #[tokio::test]
    async fn cancellation_stops_renewal_and_preserves_drained_submission_counts() {
        let harness = Harness::start().await;
        let first = harness.recipient("first@example.test").await;
        let second = harness.recipient("second@example.test").await;
        let third = harness.recipient("third@example.test").await;
        let claimed = harness.claimed_at(2400, OffsetDateTime::now_utc()).await;
        let first = harness.admit(&claimed, &first).await;
        let second = harness.admit(&claimed, &second).await;
        let third = harness.admit(&claimed, &third).await;
        let claim_fence = fence(&claimed);
        assert_eq!(
            harness
                .store
                .mail
                .claim(ClaimCampaign {
                    configuration_binding: [7; 32],
                    lease_seconds: 30,
                    now: OffsetDateTime::now_utc()
                })
                .await
                .unwrap(),
            None
        );
        let renewal = RenewCampaignClaim {
            campaign_id: claimed.campaign_id,
            fence: claim_fence,
            configuration_binding: [7; 32],
            expires_at: OffsetDateTime::now_utc() + time::Duration::seconds(300),
        };
        let renewed = harness.store.mail.renew_claim(renewal).await.unwrap();
        assert_eq!(
            harness.store.mail.renew_claim(renewal).await.unwrap(),
            renewed
        );
        assert_eq!(fence(&renewed), claim_fence);
        let mut cancellation = cancel(&renewed, 2402);
        cancellation.now = OffsetDateTime::now_utc();
        let cancelling = harness.store.mail.cancel(cancellation).await.unwrap();
        assert!(matches!(cancelling.state, CampaignState::Cancelling { .. }));
        assert_eq!(
            harness.store.mail.renew_claim(renewal).await,
            Err(CampaignCommandError::StaleClaim.into())
        );
        // Cancellation permits the already admitted attempts to drain. The
        // campaign finish derives their totals from the durable attempt ledger.
        harness.finish_attempt(first, accepted()).await;
        harness.finish_attempt(second, accepted()).await;
        harness
            .finish_attempt(third, SubmissionOutcome::Rejected)
            .await;
        let counts = CampaignCounts {
            accepted: 2,
            rejected: 1,
            unknown: 0,
        };
        let mut finish = FinishCampaign {
            campaign_id: claimed.campaign_id,
            fence: CampaignFence(Uuid::new_v4()),
            intent: FinishIntent::Cancel,
            now: renewal.expires_at + time::Duration::seconds(1),
        };
        assert_eq!(
            harness.store.mail.finish(finish).await,
            Err(CampaignCommandError::StaleClaim.into())
        );
        finish.fence = claim_fence;
        finish.intent = FinishIntent::Complete;
        assert_eq!(
            harness.store.mail.finish(finish).await,
            Err(CampaignCommandError::InvalidTransition.into())
        );
        finish.intent = FinishIntent::Cancel;
        let cancelled = harness.store.mail.finish(finish).await.unwrap();
        assert!(
            matches!(cancelled.state,CampaignState::Cancelled { counts: actual,.. } if actual == counts)
        );
        assert_eq!(harness.store.mail.finish(finish).await.unwrap(), cancelled);
        assert_eq!(harness.store.mail.active_campaign().await.unwrap(), None);
        harness.close().await;
    }

    #[tokio::test]
    async fn uncertain_or_expired_claims_never_become_automatically_sendable() {
        let harness = Harness::start().await;
        let first = harness.recipient("first@example.test").await;
        let second = harness.recipient("second@example.test").await;
        let claimed = harness.claimed_at(2500, OffsetDateTime::now_utc()).await;
        harness
            .finish_attempt(harness.admit(&claimed, &first).await, accepted())
            .await;
        harness
            .finish_attempt(
                harness.admit(&claimed, &second).await,
                SubmissionOutcome::Unknown,
            )
            .await;
        let unknown = harness
            .store
            .mail
            .finish(FinishCampaign {
                campaign_id: claimed.campaign_id,
                fence: fence(&claimed),
                intent: FinishIntent::Complete,
                now: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();
        assert!(
            matches!(unknown.state, CampaignState::Unknown { counts, .. }
            if counts == CampaignCounts { accepted: 1, rejected: 0, unknown: 1 })
        );
        assert_eq!(
            harness.store.mail.cancel(cancel(&unknown, 2502)).await,
            Err(CampaignCommandError::InvalidTransition.into())
        );
        assert_eq!(
            harness.store.mail.approve(approve(&unknown, 2503)).await,
            Err(CampaignCommandError::InvalidTransition.into())
        );
        let claimed = harness.claimed(2510).await;
        let old_fence = fence(&claimed);
        assert_eq!(
            harness
                .store
                .mail
                .renew_claim(RenewCampaignClaim {
                    campaign_id: claimed.campaign_id,
                    fence: old_fence,
                    configuration_binding: [7; 32],
                    expires_at: OffsetDateTime::now_utc() + time::Duration::seconds(30),
                })
                .await,
            Err(CampaignCommandError::StaleClaim.into())
        );
        assert_eq!(
            harness
                .store
                .mail
                .claim(ClaimCampaign {
                    configuration_binding: [7; 32],
                    lease_seconds: 30,
                    now: at(150)
                })
                .await
                .unwrap(),
            None
        );
        let quarantined = harness
            .store
            .mail
            .campaign(claimed.campaign_id)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            quarantined.state,
            CampaignState::Quarantined {
                progress: CampaignProgress::Known(CampaignCounts {
                    accepted: 0,
                    rejected: 0,
                    unknown: 0
                }),
                reason: CampaignQuarantine::Interrupted,
                ..
            }
        ));
        assert_eq!(
            harness
                .store
                .mail
                .finish(FinishCampaign {
                    campaign_id: claimed.campaign_id,
                    fence: old_fence,
                    intent: FinishIntent::Complete,
                    now: at(151)
                })
                .await,
            Err(CampaignCommandError::StaleClaim.into())
        );
        let live = harness.claimed(2520).await;
        assert_eq!(
            harness
                .store
                .mail
                .quarantine_interrupted(at(121))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            harness
                .store
                .mail
                .quarantine_interrupted(at(122))
                .await
                .unwrap(),
            0
        );
        assert!(matches!(
            harness
                .store
                .mail
                .campaign(live.campaign_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            CampaignState::Quarantined {
                progress: CampaignProgress::Known(CampaignCounts {
                    accepted: 0,
                    rejected: 0,
                    unknown: 0
                }),
                ..
            }
        ));
        let first = harness.store.mail.list(None, 2).await.unwrap();
        assert_eq!(first.items.len(), 2);
        assert_eq!(
            harness
                .store
                .mail
                .list(first.next_cursor, 2)
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        harness.close().await;
    }

    #[tokio::test]
    async fn restore_quarantines_unfinished_work_without_rewriting_completed_receipts() {
        let harness = Harness::start().await;
        let first = harness.recipient("first@example.test").await;
        let second = harness.recipient("second@example.test").await;
        let completed = harness.claimed_at(2600, OffsetDateTime::now_utc()).await;
        harness
            .finish_attempt(harness.admit(&completed, &first).await, accepted())
            .await;
        harness
            .finish_attempt(
                harness.admit(&completed, &second).await,
                SubmissionOutcome::Rejected,
            )
            .await;
        let finish = FinishCampaign {
            campaign_id: completed.campaign_id,
            fence: fence(&completed),
            intent: FinishIntent::Complete,
            now: OffsetDateTime::now_utc(),
        };
        let completed = harness.store.mail.finish(finish).await.unwrap();
        assert!(
            matches!(completed.state, CampaignState::Completed { counts, .. }
            if counts == (CampaignCounts { accepted: 1, rejected: 1, unknown: 0 }))
        );
        let unknown = harness.claimed_at(2610, OffsetDateTime::now_utc()).await;
        harness
            .finish_attempt(harness.admit(&unknown, &first).await, accepted())
            .await;
        harness
            .finish_attempt(
                harness.admit(&unknown, &second).await,
                SubmissionOutcome::Unknown,
            )
            .await;
        let counts = CampaignCounts {
            accepted: 1,
            rejected: 0,
            unknown: 1,
        };
        let unknown = harness
            .store
            .mail
            .finish(FinishCampaign {
                campaign_id: unknown.campaign_id,
                fence: fence(&unknown),
                intent: FinishIntent::Complete,
                now: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();
        let live = harness.claimed_at(2620, OffsetDateTime::now_utc()).await;
        harness
            .finish_attempt(harness.admit(&live, &first).await, accepted())
            .await;
        let root = harness.close().await;
        let path = root.path().join("state/maincopy.db");
        let restore_id = Uuid::new_v4();
        database::restore::accept(&path, restore_id).await.unwrap();
        let inspection = database::restore::inspect(&path).await.unwrap();
        assert_eq!(
            inspection
                .store
                .mail
                .campaign(completed.campaign_id)
                .await
                .unwrap(),
            Some(completed.clone())
        );
        assert!(
            matches!(inspection.store.mail.campaign(unknown.campaign_id).await.unwrap().unwrap().state,CampaignState::Quarantined { progress:CampaignProgress::Known(actual),reason:CampaignQuarantine::Restore { restore_id:actual_restore },.. } if actual == counts && actual_restore == restore_id)
        );
        assert!(matches!(
            inspection
                .store
                .mail
                .campaign(live.campaign_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            CampaignState::Quarantined {
                progress: CampaignProgress::Known(CampaignCounts {
                    accepted: 1,
                    rejected: 0,
                    unknown: 0
                }),
                ..
            }
        ));
        inspection.close().await;
        let harness = Harness::open(root).await;
        assert_eq!(harness.store.mail.finish(finish).await.unwrap(), completed);
        assert_eq!(
            harness
                .store
                .mail
                .claim(ClaimCampaign {
                    configuration_binding: [7; 32],
                    lease_seconds: 30,
                    now: at(150)
                })
                .await
                .unwrap(),
            None
        );
        let replay = harness.store.mail.create_draft(create(2620)).await.unwrap();
        assert_eq!(replay.state, CampaignState::Draft);
        assert!(matches!(
            harness
                .store
                .mail
                .campaign(live.campaign_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            CampaignState::Quarantined { .. }
        ));
        harness.close().await;
    }

    #[tokio::test]
    async fn recipient_admission_is_ordered_with_cancellation_and_drains_only_recorded_attempts() {
        for admit_first in [true, false] {
            let harness = Harness::start().await;
            let recipient = harness.recipient("recipient@example.test").await;
            let claimed = harness.claimed_at(2700, OffsetDateTime::now_utc()).await;
            let mut cancellation = cancel(&claimed, 2702);
            cancellation.now = OffsetDateTime::now_utc();
            let before = OffsetDateTime::now_utc();
            // The current-thread runtime polls both send futures in this order
            // before the writer can run; no scheduler sleep establishes order.
            let (permission, cancelled) = if admit_first {
                tokio::join!(
                    biased;
                    harness.store.subscribers.admit_campaign_recipient(admission(&claimed, &recipient)),
                    harness.store.mail.cancel(cancellation),
                )
            } else {
                let (cancelled, permission) = tokio::join!(
                    biased;
                    harness.store.mail.cancel(cancellation),
                    harness.store.subscribers.admit_campaign_recipient(admission(&claimed, &recipient)),
                );
                (permission, cancelled)
            };
            let cancelled = cancelled.unwrap();
            assert_eq!(cancelled.version, claimed.version.next().unwrap());
            assert!(matches!(cancelled.state, CampaignState::Cancelling { .. }));
            if admit_first {
                let DeliveryAdmission::Ready(permit) = permission.unwrap() else {
                    panic!("the attempt committed before cancellation")
                };
                let attempt = permit.into_attempt();
                assert_eq!(attempt.campaign_id, Some(claimed.campaign_id));
                assert_eq!(attempt.enrollment, recipient.enrollment);
                assert_eq!(attempt.generation, recipient.generation);
                assert!(attempt.admitted_at >= before);
                // Cancellation cannot discard an in-flight attempt's result.
                harness
                    .store
                    .subscribers
                    .finish_attempt(FinishAttempt {
                        mail_epoch: attempt.mail_epoch,
                        attempt_id: attempt.attempt_id,
                        attempt_fence: attempt.attempt_fence,
                        outcome: SubmissionOutcome::Rejected,
                    })
                    .await
                    .unwrap();
            } else {
                assert!(matches!(
                    permission,
                    Err(SubscriberMutationError::Command(
                        SubscriberCommandError::CampaignUnavailable
                    ))
                ));
            }
            assert!(matches!(
                harness
                    .store
                    .subscribers
                    .admit_campaign_recipient(admission(&claimed, &recipient))
                    .await,
                Err(SubscriberMutationError::Command(
                    SubscriberCommandError::CampaignUnavailable
                ))
            ));
            let attempts: i64 =
                sqlx::query_scalar("SELECT count(*) FROM mail_attempts WHERE campaign_id = ?")
                    .bind(claimed.campaign_id.0.as_bytes().as_slice())
                    .fetch_one(&harness.store.mail.readers)
                    .await
                    .unwrap();
            assert_eq!(attempts, i64::from(admit_first));
            let cancelled = harness
                .store
                .mail
                .finish(FinishCampaign {
                    campaign_id: claimed.campaign_id,
                    fence: fence(&claimed),
                    intent: FinishIntent::Cancel,
                    now: OffsetDateTime::now_utc(),
                })
                .await
                .unwrap();
            assert!(
                matches!(cancelled.state, CampaignState::Cancelled { counts, .. }
                if counts == (CampaignCounts { accepted: 0, rejected: u64::from(admit_first), unknown: 0 }))
            );
            let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM mail_campaign_receipts")
                .fetch_one(&harness.store.mail.readers)
                .await
                .unwrap();
            assert_eq!(
                receipts, 3,
                "only create, approve, and cancel write admin receipts"
            );
            harness.close().await;
        }
    }

    #[tokio::test]
    async fn every_recipient_gate_rechecks_the_approving_owners_current_eligibility() {
        for disable in [true, false] {
            let harness = Harness::start().await;
            let recipient = harness.recipient("first@example.test").await;
            let later_recipient = harness.recipient("later@example.test").await;
            let claimed = harness.claimed_at(2800, OffsetDateTime::now_utc()).await;
            let first = harness.admit(&claimed, &recipient).await;
            assert!(matches!(
                harness
                    .store
                    .subscribers
                    .admit_campaign_recipient(admission(&claimed, &recipient))
                    .await
                    .unwrap(),
                DeliveryAdmission::AlreadyRecorded(AttemptOutcome::Admitted)
            ));
            assert_eq!(
                harness
                    .store
                    .mail
                    .campaign(claimed.campaign_id)
                    .await
                    .unwrap(),
                Some(claimed.clone())
            );
            let owner = UserId::from_uuid(Uuid::from_u128(1));
            let other_owner = UserId::from_uuid(Uuid::from_u128(5));
            let now = OffsetDateTime::now_utc();
            if disable {
                harness
                    .store
                    .auth
                    .set_user_status(SetUserStatus {
                        user_id: owner,
                        changed_by_user_id: other_owner,
                        expected_version: 1,
                        status: UserStatus::Disabled,
                        configured_providers: ConfiguredLoginProviders::new(true, true).unwrap(),
                        occurred_at: now,
                        audit: audit(2802, 5, 1009),
                    })
                    .await
                    .unwrap();
            } else {
                harness
                    .store
                    .auth
                    .replace_user_roles(ReplaceUserRoles {
                        user_id: owner,
                        expected_version: 1,
                        roles: [UserRole::Administrator].into_iter().collect(),
                        assigned_by_user_id: other_owner,
                        occurred_at: now,
                        audit: audit(2802, 5, 1009),
                    })
                    .await
                    .unwrap();
            }
            assert!(matches!(
                harness
                    .store
                    .subscribers
                    .admit_campaign_recipient(admission(&claimed, &later_recipient))
                    .await,
                Err(SubscriberMutationError::Command(
                    SubscriberCommandError::CampaignUnavailable
                ))
            ));
            harness
                .finish_attempt(first, SubmissionOutcome::Rejected)
                .await;
            harness.close().await;
        }
    }

    #[tokio::test]
    async fn campaign_gate_and_renewal_use_execution_time_and_never_revive_expired_claims() {
        let harness = Harness::start().await;
        let claimed = harness.claimed(2900).await;
        let root = harness.close().await;
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(root.path().join("state/maincopy.db"))
                .foreign_keys(true),
        )
        .await
        .unwrap();
        let mut transaction = connection.begin().await.unwrap();
        let approval = require_campaign_admission(
            &mut transaction,
            claimed.campaign_id,
            fence(&claimed),
            claimed.configuration_binding,
            at(149),
        )
        .await
        .unwrap();
        assert_eq!(approval.owner, UserId::from_uuid(Uuid::from_u128(1)));
        for now in [at(119), at(150), at(151)] {
            assert!(matches!(
                require_campaign_admission(
                    &mut transaction,
                    claimed.campaign_id,
                    fence(&claimed),
                    claimed.configuration_binding,
                    now
                )
                .await,
                Err(CampaignApplyError::Command(
                    CampaignCommandError::StaleClaim
                ))
            ));
        }
        assert!(matches!(
            require_campaign_admission(
                &mut transaction,
                claimed.campaign_id,
                fence(&claimed),
                [8; 32],
                at(149)
            )
            .await,
            Err(CampaignApplyError::Command(
                CampaignCommandError::ConfigurationChanged
            ))
        ));
        assert!(matches!(
            require_campaign_admission(
                &mut transaction,
                claimed.campaign_id,
                CampaignFence(Uuid::new_v4()),
                claimed.configuration_binding,
                at(149)
            )
            .await,
            Err(CampaignApplyError::Command(
                CampaignCommandError::StaleClaim
            ))
        ));
        let renewal = RenewCampaignClaim {
            campaign_id: claimed.campaign_id,
            fence: fence(&claimed),
            configuration_binding: [7; 32],
            expires_at: at(200),
        };
        assert!(matches!(
            renew_campaign_claim(&mut transaction, renewal, at(150)).await,
            Err(CampaignApplyError::Command(
                CampaignCommandError::StaleClaim
            ))
        ));
        for target in [at(149), at(450)] {
            assert!(matches!(
                renew_campaign_claim(
                    &mut transaction,
                    RenewCampaignClaim {
                        expires_at: target,
                        ..renewal
                    },
                    at(149)
                )
                .await,
                Err(CampaignApplyError::Command(
                    CampaignCommandError::InvalidValue
                ))
            ));
        }
        for wrong in [
            RenewCampaignClaim {
                configuration_binding: [8; 32],
                ..renewal
            },
            RenewCampaignClaim {
                fence: CampaignFence(Uuid::new_v4()),
                ..renewal
            },
        ] {
            assert!(
                renew_campaign_claim(&mut transaction, wrong, at(149))
                    .await
                    .is_err()
            );
        }
        let renewed = renew_campaign_claim(&mut transaction, renewal, at(149))
            .await
            .unwrap();
        assert_eq!(
            renew_campaign_claim(&mut transaction, renewal, at(150))
                .await
                .unwrap(),
            renewed
        );
        let CampaignState::Claimed { lease, .. } = &renewed.state else {
            panic!("renewal retains the claim")
        };
        assert_eq!(lease.renewed_at, at(149));
        assert_eq!(lease.expires_at, at(200));
        sqlx::query("UPDATE instance_identity SET version = 2 WHERE singleton = 1")
            .execute(&mut *transaction)
            .await
            .unwrap();
        assert!(matches!(
            require_campaign_admission(
                &mut transaction,
                renewed.campaign_id,
                fence(&renewed),
                renewed.configuration_binding,
                at(150)
            )
            .await,
            Err(CampaignApplyError::Command(
                CampaignCommandError::StaleClaim
            ))
        ));
        assert!(matches!(
            renew_campaign_claim(&mut transaction, renewal, at(150)).await,
            Err(CampaignApplyError::Command(
                CampaignCommandError::StaleClaim
            ))
        ));
        transaction.rollback().await.unwrap();
        connection.close().await.unwrap();
    }

    #[tokio::test]
    async fn rehydration_rejects_impossible_cancelled_unknown_and_quarantine_progress() {
        let harness = Harness::start().await;
        let mut campaign = harness.claimed(3000).await;
        let CampaignState::Claimed { approval, .. } = campaign.state.clone() else {
            panic!("fixture is claimed")
        };
        let known = CampaignCounts {
            accepted: 1,
            rejected: 0,
            unknown: 0,
        };
        let unknown = CampaignCounts {
            unknown: 1,
            ..known
        };
        let restored = CampaignQuarantine::Restore {
            restore_id: Uuid::new_v4(),
        };
        let invalid = [
            CampaignState::Cancelled {
                approval: None,
                counts: known,
            },
            CampaignState::Cancelled {
                approval: Some(approval.clone()),
                counts: unknown,
            },
            CampaignState::Unknown {
                approval: approval.clone(),
                counts: known,
            },
            CampaignState::Quarantined {
                approval: None,
                progress: CampaignProgress::Known(unknown),
                reason: restored,
            },
            CampaignState::Quarantined {
                approval: None,
                progress: CampaignProgress::Unreconciled,
                reason: restored,
            },
        ];
        for state in invalid {
            campaign.state = state;
            let record = serde_json::to_string(&campaign).unwrap();
            assert!(matches!(
                decode_record(Some(&record)),
                Err(CampaignLoadError::CorruptStoredState)
            ));
        }
        let valid = [
            CampaignState::Quarantined {
                approval: Some(approval.clone()),
                progress: CampaignProgress::Known(known),
                reason: restored,
            },
            CampaignState::Quarantined {
                approval: Some(approval.clone()),
                progress: CampaignProgress::Known(CampaignCounts::default()),
                reason: CampaignQuarantine::Interrupted,
            },
            CampaignState::Cancelled {
                approval: None,
                counts: CampaignCounts::default(),
            },
            CampaignState::Cancelled {
                approval: Some(approval.clone()),
                counts: known,
            },
            CampaignState::Unknown {
                approval: approval.clone(),
                counts: unknown,
            },
            CampaignState::Quarantined {
                approval: None,
                progress: CampaignProgress::Known(CampaignCounts::default()),
                reason: restored,
            },
            CampaignState::Quarantined {
                approval: Some(approval.clone()),
                progress: CampaignProgress::Known(CampaignCounts::default()),
                reason: restored,
            },
            CampaignState::Quarantined {
                approval: Some(approval.clone()),
                progress: CampaignProgress::Known(unknown),
                reason: restored,
            },
            CampaignState::Quarantined {
                approval: Some(approval.clone()),
                progress: CampaignProgress::Unreconciled,
                reason: restored,
            },
            CampaignState::Quarantined {
                approval: Some(approval),
                progress: CampaignProgress::Unreconciled,
                reason: CampaignQuarantine::Interrupted,
            },
        ];
        for state in valid {
            campaign.state = state;
            let record = serde_json::to_string(&campaign).unwrap();
            assert_eq!(decode_record(Some(&record)).unwrap(), campaign);
        }
        harness.close().await;
    }

    #[tokio::test]
    async fn queued_campaign_commands_cannot_reuse_authentication_fresh_at_request_time() {
        let harness = Harness::start().await;
        // Session 1005 was fresh at second 80 but expired at second 90. These
        // requests retain their old occurrence time while the writer uses UTC.
        let mut stale_create = create(3100);
        stale_create.audit = audit(3100, 1, 1005);
        stale_create.now = at(80);
        assert_eq!(
            harness.store.mail.create_draft(stale_create).await,
            Err(CampaignCommandError::Forbidden.into())
        );
        let mut current_create = create(3110);
        current_create.now = at(50);
        let draft = harness
            .store
            .mail
            .create_draft(current_create)
            .await
            .unwrap();
        assert_eq!(
            draft.created_at,
            at(50),
            "audit history retains occurrence time"
        );
        let mut stale_approve = approve(&draft, 3111);
        stale_approve.audit = audit(3111, 1, 1005);
        stale_approve.now = at(80);
        assert_eq!(
            harness.store.mail.approve(stale_approve).await,
            Err(CampaignCommandError::Forbidden.into())
        );
        let mut stale_cancel = cancel(&draft, 3112);
        stale_cancel.audit = audit(3112, 1, 1005);
        stale_cancel.now = at(80);
        assert_eq!(
            harness.store.mail.cancel(stale_cancel).await,
            Err(CampaignCommandError::Forbidden.into())
        );
        assert_eq!(
            harness
                .store
                .mail
                .campaign(draft.campaign_id)
                .await
                .unwrap(),
            Some(draft)
        );
        let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM mail_campaign_receipts")
            .fetch_one(&harness.store.mail.readers)
            .await
            .unwrap();
        assert_eq!(receipts, 1);
        harness.close().await;
    }

    #[tokio::test]
    async fn rehydration_rejects_changed_public_bytes_and_strict_schema_rejects_missing_state() {
        let harness = Harness::start().await;
        let draft = harness.draft(2700).await;
        let root = harness.close().await;
        let path = root.path().join("state/maincopy.db");
        let mut connection =
            SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
                .await
                .unwrap();
        assert!(
            sqlx::query("UPDATE mail_campaigns SET record = '{}'")
                .execute(&mut connection)
                .await
                .is_err()
        );
        sqlx::query("UPDATE mail_campaigns SET record = json_set(record,'$.content.html','changed unreviewed bytes')").execute(&mut connection).await.unwrap();
        connection.close().await.unwrap();
        assert!(matches!(
            database::restore::inspect(&path).await,
            Err(RestoreError::CampaignLoad(
                CampaignLoadError::CorruptStoredState
            ))
        ));
        let harness = Harness::open(root).await;
        assert!(matches!(
            harness.store.mail.campaign(draft.campaign_id).await,
            Err(CampaignLoadError::CorruptStoredState)
        ));
        harness.close().await;
    }
    #[tokio::test]
    async fn authenticated_late_acceptance_updates_totals_without_recounting_retired_rows_or_rewriting_receipts()
     {
        for (key, intent) in [(3200, FinishIntent::Complete), (3300, FinishIntent::Cancel)] {
            let harness = Harness::start().await;
            let first = harness.recipient("first@example.test").await;
            let second = harness.recipient("second@example.test").await;
            let claimed = harness.claimed_at(key, OffsetDateTime::now_utc()).await;
            let first_attempt = harness.admit(&claimed, &first).await.into_attempt();
            let retired_id = first_attempt.attempt_id;
            harness
                .store
                .subscribers
                .finish_attempt(FinishAttempt {
                    mail_epoch: first_attempt.mail_epoch,
                    attempt_id: first_attempt.attempt_id,
                    attempt_fence: first_attempt.attempt_fence,
                    outcome: accepted(),
                })
                .await
                .unwrap();
            let second_attempt = harness.admit(&claimed, &second).await.into_attempt();
            let pending_id = second_attempt.attempt_id;
            let admitted_at = second_attempt.admitted_at;
            harness
                .store
                .subscribers
                .finish_attempt(FinishAttempt {
                    mail_epoch: second_attempt.mail_epoch,
                    attempt_id: pending_id,
                    attempt_fence: second_attempt.attempt_fence,
                    outcome: SubmissionOutcome::Unknown,
                })
                .await
                .unwrap();
            if intent == FinishIntent::Cancel {
                let mut cancellation = cancel(&claimed, key + 2);
                cancellation.now = OffsetDateTime::now_utc();
                harness.store.mail.cancel(cancellation).await.unwrap();
            }
            let finish = FinishCampaign {
                campaign_id: claimed.campaign_id,
                fence: fence(&claimed),
                intent,
                now: OffsetDateTime::now_utc(),
            };
            let receipt = harness.store.mail.finish(finish).await.unwrap();
            assert!(
                matches!(receipt.state,CampaignState::Unknown {counts,..} if counts==CampaignCounts {accepted:1,rejected:0,unknown:1})
            );
            let root = harness.close().await;
            let path = root.path().join("state/maincopy.db");
            let mut connection = SqliteConnection::connect_with(
                &SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
            // Retention can remove already-settled attempts before delayed
            // feedback arrives for a different, still-retained attempt.
            sqlx::query("DELETE FROM mail_attempts WHERE attempt_id=?")
                .bind(retired_id.as_bytes().as_slice())
                .execute(&mut connection)
                .await
                .unwrap();
            connection.close().await.unwrap();
            let harness = Harness::open(root).await;
            let feedback = || ApplyFeedback {
                mail_epoch: second_attempt.mail_epoch,
                source_binding: [6; 32],
                attempt_id: pending_id,
                campaign_id: Some(claimed.campaign_id),
                mailbox_digest: SubscriberDigest::from_bytes(
                    *blake3::keyed_hash(&[8; 32], b"second@example.test").as_bytes(),
                ),
                configuration_binding: [7; 32],
                provider_message_id: MessageId::parse("010001-late-fixture").unwrap(),
                kind: FeedbackKind::Accepted,
                sent_at: admitted_at,
                occurred_at: OffsetDateTime::now_utc(),
            };
            assert_eq!(
                harness
                    .store
                    .subscribers
                    .apply_feedback(feedback())
                    .await
                    .unwrap(),
                ControlOutcome::Changed
            );
            let settled = harness
                .store
                .mail
                .campaign(claimed.campaign_id)
                .await
                .unwrap()
                .unwrap();
            let counts = match (&settled.state, intent) {
                (CampaignState::Completed { counts, .. }, FinishIntent::Complete)
                | (CampaignState::Cancelled { counts, .. }, FinishIntent::Cancel) => *counts,
                _ => panic!("the original finish intent determines the reconciled terminal state"),
            };
            assert_eq!(
                counts,
                CampaignCounts {
                    accepted: 2,
                    rejected: 0,
                    unknown: 0
                }
            );
            assert_eq!(
                harness
                    .store
                    .subscribers
                    .apply_feedback(feedback())
                    .await
                    .unwrap(),
                ControlOutcome::Unchanged
            );
            assert_eq!(harness.store.mail.finish(finish).await.unwrap(), receipt);
            assert_eq!(
                harness
                    .store
                    .mail
                    .campaign(claimed.campaign_id)
                    .await
                    .unwrap()
                    .unwrap(),
                settled
            );
            harness.close().await;
        }
    }
    #[tokio::test]
    async fn consent_reset_requires_a_fresh_owner_and_preserves_its_receipt_across_restart() {
        let harness = Harness::start().await;
        let recipient = harness.recipient("member@example.test").await;
        let claimed = harness.claimed_at(3400, OffsetDateTime::now_utc()).await;
        let attempt = harness.admit(&claimed, &recipient).await.into_attempt();
        let before = harness.store.subscribers.status().await.unwrap();
        let command = ResetSubscriberConsent {
            expected_version: before.control_version,
            configuration_binding: [7; 32],
            audit: audit(3410, 1, 1001),
            now: OffsetDateTime::now_utc(),
        };
        assert!(matches!(
            harness
                .store
                .subscribers
                .reset_consent(command.clone())
                .await,
            Err(SubscriberMutationError::Command(
                SubscriberCommandError::ReconciliationNotRequired
            ))
        ));
        harness
            .store
            .subscribers
            .record_feedback_health([7; 32], FeedbackHealth::ReconciliationRequired)
            .await
            .unwrap();
        for (user, session) in [
            (2, 1002),
            (3, 1003),
            (4, 1004),
            (1, 1005),
            (1, 1006),
            (1, 1007),
        ] {
            let mut forbidden = command.clone();
            forbidden.audit = audit(3411, user, session);
            assert!(matches!(
                harness.store.subscribers.reset_consent(forbidden).await,
                Err(SubscriberMutationError::Command(
                    SubscriberCommandError::Forbidden
                ))
            ));
        }
        let mut stale = command.clone();
        stale.expected_version += 1;
        assert!(matches!(
            harness.store.subscribers.reset_consent(stale).await,
            Err(SubscriberMutationError::Command(
                SubscriberCommandError::StaleVersion
            ))
        ));
        let result = harness
            .store
            .subscribers
            .reset_consent(command.clone())
            .await
            .unwrap();
        assert_eq!(result.retired_epoch, attempt.mail_epoch);
        assert_ne!(result.new_epoch, result.retired_epoch);
        assert_eq!(result.discarded_enrollments, 1);
        assert_eq!(result.discarded_attempts, 2);
        assert_eq!(result.quarantined_campaigns, 1);
        let after = harness.store.subscribers.status().await.unwrap();
        assert_eq!(after.retained_enrollments, 0);
        assert_eq!(after.policy.unwrap().mode, SubscriberMode::Paused);
        assert!(matches!(
            harness
                .store
                .mail
                .campaign(claimed.campaign_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            CampaignState::Quarantined {
                reason: CampaignQuarantine::FeedbackReset { .. },
                ..
            }
        ));
        assert_eq!(
            harness
                .store
                .subscribers
                .finish_attempt(FinishAttempt {
                    mail_epoch: attempt.mail_epoch,
                    attempt_id: attempt.attempt_id,
                    attempt_fence: attempt.attempt_fence,
                    outcome: accepted(),
                })
                .await
                .unwrap(),
            AttemptOutcome::Unknown
        );
        let old_feedback = || ApplyFeedback {
            mail_epoch: attempt.mail_epoch,
            source_binding: [6; 32],
            attempt_id: attempt.attempt_id,
            campaign_id: attempt.campaign_id,
            mailbox_digest: SubscriberDigest::from_bytes([8; 32]),
            configuration_binding: [7; 32],
            provider_message_id: MessageId::parse("010001-retired").unwrap(),
            kind: FeedbackKind::Complaint,
            sent_at: attempt.admitted_at,
            occurred_at: OffsetDateTime::now_utc(),
        };
        assert_eq!(
            harness
                .store
                .subscribers
                .apply_feedback(old_feedback())
                .await
                .unwrap(),
            ControlOutcome::Unchanged
        );
        let mut unknown = old_feedback();
        unknown.mail_epoch = Uuid::new_v4();
        assert!(matches!(
            harness.store.subscribers.apply_feedback(unknown).await,
            Err(SubscriberMutationError::Command(
                SubscriberCommandError::AttemptConflict
            ))
        ));
        let root = harness.close().await;
        let harness = Harness::open(root).await;
        assert_eq!(
            harness
                .store
                .subscribers
                .reset_consent(command.clone())
                .await
                .unwrap(),
            result
        );
        let mut changed = command.clone();
        changed.expected_version += 1;
        assert!(matches!(
            harness.store.subscribers.reset_consent(changed).await,
            Err(SubscriberMutationError::Command(
                SubscriberCommandError::IdempotencyConflict
            ))
        ));
        assert_eq!(harness.store.subscribers.status().await.unwrap(), after);
        harness.store.subscribers.validate_all().await.unwrap();
        harness.close().await;
    }
    #[tokio::test]
    async fn recipient_history_cleanup_preserves_counts_and_quarantines_the_campaign_before_forgetting_attempts()
     {
        for (key, cancelling) in [(3600, false), (3700, true)] {
            let harness = Harness::start().await;
            let first = harness.recipient("first@example.test").await;
            let second = harness.recipient("second@example.test").await;
            let campaign = harness.claimed_at(key, OffsetDateTime::now_utc()).await;
            let accepted_permit = harness.admit(&campaign, &first).await;
            harness.finish_attempt(accepted_permit, accepted()).await;
            let uncertain = harness.admit(&campaign, &second).await.into_attempt();
            if cancelling {
                let mut cancellation = cancel(&campaign, key + 2);
                cancellation.now = OffsetDateTime::now_utc();
                harness.store.mail.cancel(cancellation).await.unwrap();
            }
            let expiration =
                uncertain.admitted_at + time::Duration::days(14) + time::Duration::seconds(1);
            let root = harness.close().await;
            let mut connection = SqliteConnection::connect_with(
                &SqliteConnectOptions::new()
                    .filename(root.path().join("state/maincopy.db"))
                    .foreign_keys(true),
            )
            .await
            .unwrap();
            let mut transaction = connection.begin().await.unwrap();
            let cleaned = cleanup_subscribers(&mut transaction, expiration.unix_timestamp())
                .await
                .unwrap();
            assert_eq!(cleaned.removed_attempts, 4);
            assert_eq!(cleaned.removed_enrollments, 0);
            transaction.commit().await.unwrap();
            connection.close().await.unwrap();
            let harness = Harness::open(root).await;
            let quarantined = harness
                .store
                .mail
                .campaign(campaign.campaign_id)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                quarantined.state,
                CampaignState::Quarantined {
                    progress: CampaignProgress::Known(CampaignCounts {
                        accepted: 1,
                        rejected: 0,
                        unknown: 1
                    }),
                    reason: CampaignQuarantine::Interrupted,
                    ..
                }
            ));
            assert!(matches!(
                harness
                    .store
                    .subscribers
                    .admit_campaign_recipient(admission(&campaign, &first))
                    .await,
                Err(SubscriberMutationError::Command(
                    SubscriberCommandError::CampaignUnavailable
                ))
            ));
            assert!(
                harness
                    .store
                    .subscribers
                    .recipients(&quarantined, None, 100)
                    .await
                    .unwrap()
                    .is_empty()
            );
            harness.store.subscribers.validate_all().await.unwrap();
            harness.close().await;
        }
    }
}
