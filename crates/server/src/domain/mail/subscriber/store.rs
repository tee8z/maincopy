//! Consent, budgets, and recipient attempts share the existing site transaction.

use sqlx::{
    FromRow as _, QueryBuilder, Row as _, Sqlite, SqlitePool, Transaction, sqlite::SqliteRow,
};
use subtle::ConstantTimeEq as _;
use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use uuid::{Uuid, Variant, Version};

use super::*;
use crate::{
    database::{
        fingerprint::CommandFingerprintBuilder,
        store::{DatabaseAdmissionError, Mutation, MutationSender},
    },
    domain::auth::store::{
        AuthApplyError, AuthCommandError, append_success_audit, decode_audit_principal,
        require_fresh_browser_owner,
    },
    domain::mail::{
        campaign::{Campaign, CampaignCounts, CampaignState},
        identity::EmailAddress,
        store::{
            CampaignApplyError, CampaignCommandError, quarantine_recipient_history,
            quarantine_reset_campaigns, reconcile_campaign_acceptance, require_campaign_admission,
        },
    },
};

// SQLite and SQLx own database pages and parameter buffers; these driver-owned
// copies are outside Maincopy's zeroizing domain values. Borrowed decodes avoid
// another ordinary owned copy of mailbox, nonce, and provider identifiers.
const ENROLLMENTS: &str = "SELECT CASE WHEN length(enrollment_id)=16 THEN enrollment_id END AS enrollment_id,CASE WHEN length(generation)=16 THEN generation END AS generation,CASE WHEN length(mailbox_digest)=32 THEN mailbox_digest END AS mailbox_digest,CASE WHEN length(CAST(address AS BLOB))<=254 THEN address END AS address,CASE WHEN length(state)<=7 THEN state END AS state,CASE WHEN length(nonce_digest)=32 THEN nonce_digest END AS nonce_digest,nonce_expires_at,pending_expires_at,created_at,confirmed_at,confirmed_sequence,confirmation_requested_at,retire_at,(address IS NULL OR length(CAST(address AS BLOB))<=254) AND (mailbox_digest IS NULL OR length(mailbox_digest)=32) AND (nonce_digest IS NULL OR length(nonce_digest)=32) AS valid_widths FROM mail_enrollments";
const ATTEMPTS: &str = "SELECT CASE WHEN length(mail_epoch)=16 THEN mail_epoch END AS mail_epoch,CASE WHEN length(feedback_source)=32 THEN feedback_source END AS feedback_source,CASE WHEN length(attempt_id)=16 THEN attempt_id END AS attempt_id,CASE WHEN length(attempt_fence)=16 THEN attempt_fence END AS attempt_fence,CASE WHEN length(enrollment_id)=16 THEN enrollment_id END AS enrollment_id,CASE WHEN length(generation)=16 THEN generation END AS generation,CASE WHEN length(campaign_id)=16 THEN campaign_id END AS campaign_id,CASE WHEN length(campaign_fence)=16 THEN campaign_fence END AS campaign_fence,CASE WHEN length(kind)<=12 THEN kind END AS kind,CASE WHEN length(outcome)<=9 THEN outcome END AS outcome,CASE WHEN length(recipient_binding)=32 THEN recipient_binding END AS recipient_binding,CASE WHEN length(configuration_binding)=32 THEN configuration_binding END AS configuration_binding,CASE WHEN length(CAST(provider_message_id AS BLOB))<=256 THEN provider_message_id END AS provider_message_id,instance_version,budget_day,created_at,admitted_at,finished_at,retire_at,(provider_message_id IS NULL OR length(CAST(provider_message_id AS BLOB))<=256) AND (campaign_id IS NULL OR length(campaign_id)=16) AND (campaign_fence IS NULL OR length(campaign_fence)=16) AS valid_widths FROM mail_attempts";

#[derive(Clone)]
pub(crate) struct SubscriberStore {
    readers: SqlitePool,
    mutations: MutationSender,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SubscriberMutationError {
    #[error(transparent)]
    Admission(#[from] DatabaseAdmissionError),
    #[error(transparent)]
    Command(#[from] SubscriberCommandError),
}

#[derive(Debug, Error)]
pub(crate) enum SubscriberLoadError {
    #[error("subscriber query failed")]
    Operation(#[from] sqlx::Error),
    #[error("stored subscriber state failed validation")]
    CorruptStoredState,
    #[error("subscriber query exceeds its bounded page size")]
    InvalidLimit,
}

#[derive(Debug, Error)]
pub(crate) enum SubscriberApplyError {
    #[error(transparent)]
    Command(#[from] SubscriberCommandError),
    #[error("subscriber database operation failed")]
    Operation(#[from] sqlx::Error),
    #[error("stored subscriber state failed validation")]
    CorruptStoredState,
}

impl SubscriberStore {
    pub(crate) fn new(readers: SqlitePool, mutations: mpsc::Sender<Mutation>) -> Self {
        Self {
            readers,
            mutations: MutationSender::new(mutations),
        }
    }

    pub(crate) async fn initialize_controls(
        &self,
        binding: [u8; 32],
    ) -> Result<(), SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::InitializeMailControls {
                    binding,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn pause(&self) -> Result<(), SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::PauseMailSubscribers { respond_to },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn status(&self) -> Result<SubscriberStatus, SubscriberLoadError> {
        let (active,pending,addressed,retained):(i64,i64,i64,i64)=sqlx::query_as("SELECT COALESCE(SUM(state='active'),0),COALESCE(SUM(state='pending'),0),COALESCE(SUM(address IS NOT NULL),0),count(*) FROM mail_enrollments").fetch_one(&self.readers).await?;
        if retained > MAX_ENROLLMENTS
            || active < 0
            || pending < 0
            || addressed < 0
            || retained < addressed
            || addressed != active + pending
        {
            return Err(SubscriberLoadError::CorruptStoredState);
        }
        let mut status = SubscriberStatus {
            control_version: 0,
            mail_epoch: None,
            policy: None,
            feedback_health: FeedbackHealth::Unavailable,
            last_feedback_ok_at: None,
            active_enrollments: active as u64,
            pending_enrollments: pending as u64,
            addressed_enrollments: addressed as u64,
            retained_enrollments: retained as u64,
        };
        let row = sqlx::query("SELECT control_version,mail_epoch,configuration_binding,length(configuration_binding) AS binding_bytes,mode,enrollment_sequence,max_daily_messages,max_daily_confirmations,max_campaign_recipients,last_feedback_ok_at,feedback_gap FROM mail_control_state WHERE singleton=1").fetch_optional(&self.readers).await?;
        if let Some(row) = row {
            let control = ControlStatusRow::from_row(&row)?;
            status.control_version = u64::try_from(control.control_version)
                .ok()
                .filter(|version| *version > 0)
                .ok_or(SubscriberLoadError::CorruptStoredState)?;
            status.mail_epoch = Some(row_uuid(&row, "mail_epoch")?);
            status.policy = PolicyRow::from_row(&row)?.configured()?;
            status.last_feedback_ok_at = stored_optional_time(control.last_feedback_ok_at)?;
            status.feedback_health = control.health(OffsetDateTime::now_utc().unix_timestamp())?;
        }
        Ok(status)
    }

    pub(crate) async fn set_policy(
        &self,
        policy: SubscriberPolicy,
    ) -> Result<(), SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::SetSubscriberPolicy { policy, respond_to },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn record_feedback_health(
        &self,
        binding: [u8; 32],
        health: FeedbackHealth,
    ) -> Result<(), SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::RecordMailFeedbackHealth {
                    binding,
                    health,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn record_feedback_integrity_failure(
        &self,
        binding: [u8; 32],
    ) -> Result<FeedbackHealth, SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::RecordMailFeedbackIntegrityFailure {
                    binding,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn begin_feedback_run(
        &self,
        command: BeginFeedbackRun,
    ) -> Result<FeedbackRunAdmission, SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::BeginMailFeedbackRun {
                    command,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }
    pub(crate) async fn record_feedback_observation(
        &self,
        command: RecordFeedbackObservation,
    ) -> Result<(), SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::RecordMailFeedbackObservation {
                    command,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }
    pub(crate) async fn finish_feedback_run(
        &self,
        configuration_binding: [u8; 32],
        run_id: Uuid,
    ) -> Result<(), SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::FinishMailFeedbackRun {
                    configuration_binding,
                    run_id,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn begin_feedback_poll(
        &self,
        binding: [u8; 32],
        run_id: Uuid,
    ) -> Result<FeedbackPollAdmission, SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::BeginMailFeedbackPoll {
                    binding,
                    run_id,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }
    pub(crate) async fn complete_feedback_poll(
        &self,
        binding: [u8; 32],
        run_id: Uuid,
        poll_id: Uuid,
    ) -> Result<(), SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::CompleteMailFeedbackPoll {
                    binding,
                    run_id,
                    poll_id,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }
    pub(crate) async fn defer_feedback_poll(
        &self,
        binding: [u8; 32],
        run_id: Uuid,
        poll_id: Uuid,
    ) -> Result<(), SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::DeferMailFeedbackPoll {
                    binding,
                    run_id,
                    poll_id,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }
    pub(crate) async fn reset_consent(
        &self,
        command: ResetSubscriberConsent,
    ) -> Result<ResetSubscriberConsentResult, SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::ResetMailSubscriberConsent {
                    command,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn request_enrollment(
        &self,
        command: RequestEnrollment,
    ) -> Result<EnrollmentRequestResult, SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::RequestMailEnrollment {
                    command,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn confirm(
        &self,
        command: ConfirmEnrollment,
    ) -> Result<ControlOutcome, SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::ConfirmMailEnrollment {
                    command,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn remove(
        &self,
        command: ManageEnrollment,
    ) -> Result<ControlOutcome, SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::RemoveMailEnrollment {
                    command,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn claim_confirmation(
        &self,
        command: ClaimConfirmation,
    ) -> Result<DeliveryAdmission, SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::ClaimMailConfirmation {
                    command,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn admit_campaign_recipient(
        &self,
        command: AdmitCampaignRecipient,
    ) -> Result<DeliveryAdmission, SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::AdmitMailRecipient {
                    command,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn finish_attempt(
        &self,
        command: FinishAttempt,
    ) -> Result<AttemptOutcome, SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::FinishMailAttempt {
                    command,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn apply_feedback(
        &self,
        command: ApplyFeedback,
    ) -> Result<ControlOutcome, SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::ApplyMailFeedback {
                    command,
                    respond_to,
                },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn quarantine_interrupted(&self) -> Result<u64, SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::QuarantineMailAttempts { respond_to },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn cleanup(&self) -> Result<SubscriberCleanup, SubscriberMutationError> {
        self.mutations
            .send(
                |respond_to| Mutation::CleanupMailSubscribers { respond_to },
                SubscriberCommandError::OutcomeUnknown,
            )
            .await
    }

    pub(crate) async fn queued_confirmations(
        &self,
        configuration_binding: [u8; 32],
        limit: usize,
    ) -> Result<Vec<ConfirmationHandle>, SubscriberLoadError> {
        bounded_limit(limit)?;
        sqlx::query("SELECT attempt.attempt_id,attempt.enrollment_id,attempt.generation,enrollment.pending_expires_at FROM mail_attempts AS attempt JOIN mail_enrollments AS enrollment ON enrollment.enrollment_id=attempt.enrollment_id AND enrollment.generation=attempt.generation WHERE attempt.kind = 'confirmation' AND attempt.outcome = 'queued' AND enrollment.state='pending' AND attempt.configuration_binding=? ORDER BY attempt.created_at,attempt.attempt_id LIMIT ?")
            .bind(configuration_binding.as_slice()).bind(limit as i64).fetch_all(&self.readers).await?.into_iter().map(|row| Ok(ConfirmationHandle {
                attempt_id: row_uuid(&row,"attempt_id")?, enrollment: row_uuid(&row,"enrollment_id")?, generation: row_uuid(&row,"generation")?,
                pending_expires_at: stored_time(row.try_get("pending_expires_at")?)?,
            })).collect()
    }

    pub(crate) async fn recipients(
        &self,
        campaign: &Campaign,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<RecipientHandle>, SubscriberLoadError> {
        bounded_limit(limit)?;
        let cutoff = match &campaign.state {
            CampaignState::Queued { approval } | CampaignState::Claimed { approval, .. } => {
                approval.audience_cutoff
            }
            CampaignState::Draft
            | CampaignState::Cancelling { .. }
            | CampaignState::Completed { .. }
            | CampaignState::Cancelled { .. }
            | CampaignState::Unknown { .. }
            | CampaignState::Quarantined { .. } => return Ok(Vec::new()),
        };
        sqlx::query("SELECT enrollment_id,generation FROM mail_enrollments AS enrollment WHERE state = 'active' AND confirmed_sequence <= ? AND enrollment_id > ? AND NOT EXISTS (SELECT 1 FROM mail_attempts WHERE campaign_id = ? AND enrollment_id = enrollment.enrollment_id AND generation = enrollment.generation) AND (SELECT count(*) FROM mail_attempts WHERE campaign_id=?) < (SELECT max_campaign_recipients FROM mail_control_state WHERE singleton=1) ORDER BY enrollment_id LIMIT ?")
            .bind(i64::try_from(cutoff).map_err(|_| SubscriberLoadError::CorruptStoredState)?)
            .bind(after.unwrap_or(Uuid::nil()).as_bytes().as_slice()).bind(campaign.campaign_id.0.as_bytes().as_slice()).bind(campaign.campaign_id.0.as_bytes().as_slice()).bind(limit as i64)
            .fetch_all(&self.readers).await?.into_iter().map(|row| Ok(RecipientHandle {
                enrollment: row_uuid(&row,"enrollment_id")?, generation: row_uuid(&row,"generation")?,
            })).collect()
    }

    /// Validate bounded rows before an offline restore can accept their database.
    pub(crate) async fn validate_all(&self) -> Result<(), SubscriberLoadError> {
        let mut transaction = self.readers.begin().await?;
        let counts:(i64,i64,i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM mail_control_state),(SELECT count(*) FROM mail_enrollments),(SELECT count(*) FROM mail_attempts),(SELECT count(*) FROM mail_suppressions),(SELECT count(*) FROM mail_daily_budget)").fetch_one(&mut *transaction).await?;
        if !(0..=1).contains(&counts.0)
            || counts.1 > MAX_ENROLLMENTS
            || [counts.2, counts.3, counts.4]
                .into_iter()
                .any(|count| count > MAX_ATTEMPTS)
            || (counts.0 == 0 && (counts.1, counts.2, counts.3, counts.4) != (0, 0, 0, 0))
        {
            return Err(SubscriberLoadError::CorruptStoredState);
        }
        let valid:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM mail_control_state WHERE singleton!=1 OR length(control_binding)!=32 OR (configuration_binding IS NOT NULL AND length(configuration_binding)!=32) OR mode NOT IN ('enabled','paused') OR (mode='enabled' AND configuration_binding IS NULL) OR enrollment_sequence<0 OR max_daily_messages NOT BETWEEN 1 AND 1000000 OR max_daily_confirmations NOT BETWEEN 1 AND max_daily_messages OR max_campaign_recipients NOT BETWEEN 1 AND 100000 OR feedback_gap NOT IN (0,1) OR (feedback_gap=1 AND last_feedback_ok_at IS NOT NULL)) AND NOT EXISTS(SELECT 1 FROM mail_enrollments WHERE confirmed_sequence>(SELECT enrollment_sequence FROM mail_control_state)) AND NOT EXISTS(SELECT 1 FROM mail_attempts WHERE instance_version>(SELECT version FROM instance_identity)) AND NOT EXISTS(SELECT 1 FROM mail_daily_budget WHERE total<0 OR total>1000000 OR confirmations<0 OR confirmations>total OR day NOT BETWEEN -4371587 AND 2932896)").fetch_one(&mut *transaction).await?;
        if !valid {
            return Err(SubscriberLoadError::CorruptStoredState);
        }
        if let Some(row) = sqlx::query(
            "SELECT *,length(configuration_binding) AS binding_bytes FROM mail_control_state",
        )
        .fetch_optional(&mut *transaction)
        .await?
        {
            validate_control_row(&row)?;
        }
        validate_private_pages(
            &mut transaction,
            ENROLLMENTS,
            "enrollment_id",
            counts.1,
            |row| enrollment(row).map(|value| value.id),
        )
        .await?;
        validate_private_pages(&mut transaction, ATTEMPTS, "attempt_id", counts.2, |row| {
            attempt(row).map(|value| value.id)
        })
        .await?;
        validate_reset_history(&mut transaction).await?;
        // SQL validates the fixed-width suppression digest without copying it to
        // an ordinary allocation. These rows never restore consent themselves.
        let valid:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM mail_suppressions WHERE length(mailbox_digest)!=32 OR reason NOT IN ('hard_bounce','complaint') OR created_at NOT BETWEEN -377705116800 AND 253402300799 OR expires_at<=created_at OR expires_at-created_at>2592000)").fetch_one(&mut *transaction).await?;
        if !valid {
            return Err(SubscriberLoadError::CorruptStoredState);
        }
        transaction.commit().await?;
        Ok(())
    }
}

/// Apply the same rehydration rules in a bounded scan during offline acceptance.
async fn validate_private_pages(
    transaction: &mut Transaction<'_, Sqlite>,
    select: &str,
    id_column: &str,
    expected: i64,
    decode: fn(SqliteRow) -> Result<Uuid, SubscriberLoadError>,
) -> Result<(), SubscriberLoadError> {
    let mut cursor = Uuid::nil();
    let mut checked = 0;
    loop {
        let rows = QueryBuilder::new(select)
            .push(" WHERE ")
            .push(id_column)
            .push(">")
            .push_bind(cursor.as_bytes().as_slice())
            .push(" ORDER BY ")
            .push(id_column)
            .push(" LIMIT 100")
            .build()
            .fetch_all(&mut **transaction)
            .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            cursor = decode(row)?;
            checked += 1;
        }
        if checked > expected {
            return Err(SubscriberLoadError::CorruptStoredState);
        }
    }
    if checked != expected {
        return Err(SubscriberLoadError::CorruptStoredState);
    }
    Ok(())
}

fn validate_control_row(row: &SqliteRow) -> Result<(), SubscriberLoadError> {
    row_bytes::<32>(row, "control_binding")?;
    row_uuid(row, "mail_epoch")?;
    let policy = PolicyRow::from_row(row)?;
    policy.configured()?;
    stored_optional_time(policy.last_feedback_ok_at)?;
    let status = ControlStatusRow::from_row(row)?;
    if status.control_version <= 0 || policy.enrollment_sequence < 0 {
        return Err(SubscriberLoadError::CorruptStoredState);
    }
    status.health(OffsetDateTime::now_utc().unix_timestamp())?;
    FeedbackContinuity::from_row(row)?;
    Ok(())
}

async fn validate_reset_history(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<(), SubscriberLoadError> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mail_consent_resets")
        .fetch_one(&mut **transaction)
        .await?;
    if count > MAX_CONSENT_RESETS {
        return Err(SubscriberLoadError::CorruptStoredState);
    }
    let valid: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM mail_attempts WHERE mail_epoch!=(SELECT mail_epoch FROM mail_control_state)) AND NOT EXISTS(SELECT 1 FROM mail_consent_resets WHERE retired_epoch=(SELECT mail_epoch FROM mail_control_state) OR retired_epoch=new_epoch OR length(command_fingerprint)!=32 OR length(idempotency_key)!=16 OR length(audit_event_id)!=16) AND NOT EXISTS(SELECT 1 FROM mail_consent_resets AS receipt LEFT JOIN admin_audit_events AS audit ON receipt.audit_event_id=audit.audit_event_id WHERE audit.audit_event_id IS NULL OR audit.outcome!='succeeded' OR audit.action NOT IN ('mail.consent.reset','instance.restore.accept') OR receipt.idempotency_key!=audit.idempotency_key)")
        .fetch_one(&mut **transaction).await?;
    if !valid {
        return Err(SubscriberLoadError::CorruptStoredState);
    }
    // Receipts contain only aggregate counts and global random identities.
    let rows = sqlx::query("SELECT version,retired_epoch,new_epoch,discarded_enrollments,discarded_attempts,quarantined_campaigns FROM mail_consent_resets").fetch_all(&mut **transaction).await?;
    for row in rows {
        reset_result(&row)?;
    }
    Ok(())
}

fn stored_time(value: i64) -> Result<OffsetDateTime, SubscriberLoadError> {
    OffsetDateTime::from_unix_timestamp(value).map_err(|_| SubscriberLoadError::CorruptStoredState)
}

fn stored_optional_time(value: Option<i64>) -> Result<Option<OffsetDateTime>, SubscriberLoadError> {
    value.map(stored_time).transpose()
}

fn validate_policy(value: &SubscriberPolicy) -> Result<(), SubscriberCommandError> {
    if !(1..=1_000_000).contains(&value.max_daily_messages)
        || !(1..=value.max_daily_messages).contains(&value.max_daily_confirmations)
        || !(1..=100_000).contains(&value.max_campaign_recipients)
    {
        Err(SubscriberCommandError::InvalidValue)
    } else {
        Ok(())
    }
}

fn bounded_limit(limit: usize) -> Result<(), SubscriberLoadError> {
    if (1..=PAGE_SIZE).contains(&limit) {
        Ok(())
    } else {
        Err(SubscriberLoadError::InvalidLimit)
    }
}

fn random_uuid(value: Uuid) -> Result<(), SubscriberCommandError> {
    if value.get_variant() == Variant::RFC4122 && value.get_version() == Some(Version::Random) {
        Ok(())
    } else {
        Err(SubscriberCommandError::InvalidValue)
    }
}

fn row_uuid(row: &SqliteRow, name: &str) -> Result<Uuid, SubscriberLoadError> {
    let value = Uuid::from_slice(row.try_get::<&[u8], _>(name)?)
        .map_err(|_| SubscriberLoadError::CorruptStoredState)?;
    random_uuid(value).map_err(|_| SubscriberLoadError::CorruptStoredState)?;
    Ok(value)
}

fn load_error(error: SubscriberLoadError) -> SubscriberApplyError {
    match error {
        SubscriberLoadError::Operation(error) => SubscriberApplyError::Operation(error),
        SubscriberLoadError::CorruptStoredState | SubscriberLoadError::InvalidLimit => {
            SubscriberApplyError::CorruptStoredState
        }
    }
}

fn campaign_error(error: CampaignApplyError) -> SubscriberApplyError {
    match error {
        CampaignApplyError::Operation(error) => SubscriberApplyError::Operation(error),
        CampaignApplyError::Command(_) => SubscriberCommandError::CampaignUnavailable.into(),
        CampaignApplyError::CorruptStoredState => SubscriberApplyError::CorruptStoredState,
    }
}

struct Policy {
    binding: Option<[u8; 32]>,
    enabled: bool,
    sequence: u64,
    daily: i64,
    confirmations: i64,
    campaign: i64,
    feedback_at: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct PolicyRow<'r> {
    configuration_binding: Option<&'r [u8]>,
    binding_bytes: Option<i64>,
    mode: &'r str,
    enrollment_sequence: i64,
    max_daily_messages: i64,
    max_daily_confirmations: i64,
    max_campaign_recipients: i64,
    last_feedback_ok_at: Option<i64>,
}

impl PolicyRow<'_> {
    fn configured(&self) -> Result<Option<SubscriberPolicy>, SubscriberLoadError> {
        let mode = match self.mode {
            "paused" => SubscriberMode::Paused,
            "enabled" => SubscriberMode::Enabled,
            _ => return Err(SubscriberLoadError::CorruptStoredState),
        };
        let binding = self
            .configuration_binding
            .map(|bytes| {
                bytes
                    .try_into()
                    .map_err(|_| SubscriberLoadError::CorruptStoredState)
            })
            .transpose()?;
        let value = SubscriberPolicy {
            configuration_binding: binding.unwrap_or_default(),
            mode,
            max_daily_messages: self.max_daily_messages as u64,
            max_daily_confirmations: self.max_daily_confirmations as u64,
            max_campaign_recipients: self.max_campaign_recipients as u64,
        };
        validate_policy(&value).map_err(|_| SubscriberLoadError::CorruptStoredState)?;
        if self.binding_bytes.is_some_and(|width| width != 32)
            || (mode == SubscriberMode::Enabled && binding.is_none())
        {
            return Err(SubscriberLoadError::CorruptStoredState);
        }
        Ok(binding.map(|_| value))
    }
}

#[derive(sqlx::FromRow)]
struct ControlStatusRow {
    control_version: i64,
    feedback_gap: i64,
    last_feedback_ok_at: Option<i64>,
}

impl ControlStatusRow {
    fn health(&self, now: i64) -> Result<FeedbackHealth, SubscriberLoadError> {
        match self.feedback_gap {
            1 if self.last_feedback_ok_at.is_none() => Ok(FeedbackHealth::ReconciliationRequired),
            0 if self.last_feedback_ok_at.is_some_and(|last| {
                (0..=FEEDBACK_FRESH_SECONDS).contains(&now.saturating_sub(last))
            }) =>
            {
                Ok(FeedbackHealth::Healthy)
            }
            0 => Ok(FeedbackHealth::Unavailable),
            _ => Err(SubscriberLoadError::CorruptStoredState),
        }
    }
}

async fn policy(transaction: &mut Transaction<'_, Sqlite>) -> Result<Policy, SubscriberApplyError> {
    let row = sqlx::query("SELECT configuration_binding,length(configuration_binding) AS binding_bytes,mode,enrollment_sequence,max_daily_messages,max_daily_confirmations,max_campaign_recipients,CASE WHEN feedback_gap=0 THEN last_feedback_ok_at END AS last_feedback_ok_at FROM mail_control_state WHERE singleton=1")
        .fetch_optional(&mut **transaction).await?.ok_or(SubscriberCommandError::ControlsUnavailable)?;
    let stored = PolicyRow::from_row(&row)?;
    let configured = stored.configured().map_err(load_error)?;
    stored_optional_time(stored.last_feedback_ok_at).map_err(load_error)?;
    Ok(Policy {
        binding: configured.map(|value| value.configuration_binding),
        enabled: configured.is_some_and(|value| value.mode == SubscriberMode::Enabled),
        sequence: u64::try_from(stored.enrollment_sequence)
            .map_err(|_| SubscriberApplyError::CorruptStoredState)?,
        daily: stored.max_daily_messages,
        confirmations: stored.max_daily_confirmations,
        campaign: stored.max_campaign_recipients,
        feedback_at: stored.last_feedback_ok_at,
    })
}

async fn require_enabled(
    transaction: &mut Transaction<'_, Sqlite>,
    binding: [u8; 32],
    now: i64,
) -> Result<Policy, SubscriberApplyError> {
    let policy = policy(transaction).await?;
    if policy.binding != Some(binding) {
        return Err(SubscriberCommandError::ConfigurationChanged.into());
    }
    if !policy.enabled
        || !policy
            .feedback_at
            .is_some_and(|last| last <= now && now - last <= FEEDBACK_FRESH_SECONDS)
    {
        return Err(SubscriberCommandError::Paused.into());
    }
    Ok(policy)
}

pub(crate) async fn initialize_controls(
    transaction: &mut Transaction<'_, Sqlite>,
    binding: [u8; 32],
) -> Result<(), SubscriberApplyError> {
    let existing: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT control_binding FROM mail_control_state WHERE singleton = 1")
            .fetch_optional(&mut **transaction)
            .await?;
    if let Some(existing) = existing {
        if existing.as_slice() == binding {
            return Ok(());
        }
        let retained: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mail_enrollments) OR EXISTS(SELECT 1 FROM mail_attempts) OR EXISTS(SELECT 1 FROM mail_suppressions)").fetch_one(&mut **transaction).await?;
        if retained {
            return Err(SubscriberCommandError::ControlIdentityChanged.into());
        }
        sqlx::query("UPDATE mail_control_state SET control_binding=?,mode='paused',last_feedback_ok_at=NULL WHERE singleton=1").bind(binding.as_slice()).execute(&mut **transaction).await?;
    } else {
        sqlx::query(
            "INSERT INTO mail_control_state (singleton,control_binding,mail_epoch,control_version,configuration_binding,mode,enrollment_sequence,max_daily_messages,max_daily_confirmations,max_campaign_recipients,last_feedback_ok_at,feedback_gap) VALUES (1,?,?,1,NULL,'paused',0,5000,100,2000,NULL,0)",
        )
        .bind(binding.as_slice()).bind(Uuid::new_v4().as_bytes().as_slice())
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

pub(crate) async fn set_policy(
    transaction: &mut Transaction<'_, Sqlite>,
    value: SubscriberPolicy,
    now: i64,
) -> Result<(), SubscriberApplyError> {
    validate_policy(&value)?;
    cancel_obsolete_confirmations(transaction, value.configuration_binding, now).await?;
    let mode = match value.mode {
        SubscriberMode::Paused => "paused",
        SubscriberMode::Enabled => "enabled",
    };
    let changed = sqlx::query("UPDATE mail_control_state SET control_version=control_version+1,configuration_binding=?,mode=?,max_daily_messages=?,max_daily_confirmations=?,max_campaign_recipients=?,last_feedback_ok_at=NULL WHERE singleton=1")
        .bind(value.configuration_binding.as_slice()).bind(mode).bind(value.max_daily_messages as i64).bind(value.max_daily_confirmations as i64).bind(value.max_campaign_recipients as i64).execute(&mut **transaction).await?.rows_affected();
    if changed != 1 {
        return Err(SubscriberCommandError::ControlsUnavailable.into());
    }
    Ok(())
}

/// A queued confirmation contains no transmitted controls. Reconfiguration
/// must retire it before a different sender identity can capture its address.
async fn cancel_obsolete_confirmations(
    transaction: &mut Transaction<'_, Sqlite>,
    binding: [u8; 32],
    now: i64,
) -> Result<(), SubscriberApplyError> {
    let mut count = 0;
    loop {
        let rows=sqlx::query("SELECT enrollment_id FROM mail_attempts WHERE kind='confirmation' AND outcome='queued' AND configuration_binding!=? ORDER BY attempt_id LIMIT 100").bind(binding.as_slice()).fetch_all(&mut **transaction).await?;
        if rows.is_empty() {
            return Ok(());
        }
        count += rows.len();
        if count > MAX_ENROLLMENTS as usize {
            return Err(SubscriberApplyError::CorruptStoredState);
        }
        for row in rows {
            remove_enrollment(
                transaction,
                row_uuid(&row, "enrollment_id").map_err(load_error)?,
                now,
            )
            .await?;
        }
    }
}

pub(crate) async fn pause(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<(), SubscriberApplyError> {
    sqlx::query("UPDATE mail_control_state SET mode='paused',last_feedback_ok_at=NULL")
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

pub(crate) async fn require_campaign_configuration(
    transaction: &mut Transaction<'_, Sqlite>,
    binding: [u8; 32],
    now: OffsetDateTime,
) -> Result<(), CampaignApplyError> {
    let policy = require_enabled(transaction, binding, now.unix_timestamp())
        .await
        .map_err(|error| match error {
            SubscriberApplyError::Operation(error) => CampaignApplyError::Operation(error),
            SubscriberApplyError::CorruptStoredState => CampaignApplyError::CorruptStoredState,
            SubscriberApplyError::Command(SubscriberCommandError::ConfigurationChanged) => {
                CampaignCommandError::ConfigurationChanged.into()
            }
            SubscriberApplyError::Command(
                SubscriberCommandError::Paused | SubscriberCommandError::ControlsUnavailable,
            ) => CampaignCommandError::SendingUnavailable.into(),
            SubscriberApplyError::Command(SubscriberCommandError::InvalidValue) => {
                CampaignCommandError::InvalidValue.into()
            }
            SubscriberApplyError::Command(
                SubscriberCommandError::Forbidden
                | SubscriberCommandError::StaleVersion
                | SubscriberCommandError::ReconciliationNotRequired
                | SubscriberCommandError::IdempotencyConflict
                | SubscriberCommandError::Capacity
                | SubscriberCommandError::ControlIdentityChanged
                | SubscriberCommandError::AttemptConflict
                | SubscriberCommandError::CampaignUnavailable
                | SubscriberCommandError::OutcomeUnknown,
            ) => CampaignApplyError::CorruptStoredState,
        })?;
    let audience: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mail_enrollments WHERE state='active'")
            .fetch_one(&mut **transaction)
            .await?;
    if audience > policy.campaign {
        return Err(CampaignCommandError::InvalidValue.into());
    }
    Ok(())
}

/// Before a run is established only degradation is permitted. Successful
/// observations require the durable run fence and source continuity checks.
pub(crate) async fn record_feedback_health(
    transaction: &mut Transaction<'_, Sqlite>,
    binding: [u8; 32],
    health: FeedbackHealth,
    _now: i64,
) -> Result<(), SubscriberApplyError> {
    let gap = match health {
        FeedbackHealth::Healthy => return Err(SubscriberCommandError::InvalidValue.into()),
        FeedbackHealth::Unavailable => false,
        FeedbackHealth::ReconciliationRequired => true,
    };
    let changed=sqlx::query("UPDATE mail_control_state SET last_feedback_ok_at=NULL,feedback_gap=max(feedback_gap,?) WHERE singleton=1 AND configuration_binding=?").bind(i64::from(gap)).bind(binding.as_slice()).execute(&mut **transaction).await?.rows_affected();
    if changed != 1 {
        return Err(SubscriberCommandError::ConfigurationChanged.into());
    }
    Ok(())
}

/// A broken source contract can lose or admit untrusted feedback. Empty initial
/// setup has no consent to reconcile; checking that fact belongs to this writer.
pub(crate) async fn record_feedback_integrity_failure(
    transaction: &mut Transaction<'_, Sqlite>,
    binding: [u8; 32],
) -> Result<FeedbackHealth, SubscriberApplyError> {
    let gap: i64 = sqlx::query_scalar("UPDATE mail_control_state SET last_feedback_ok_at=NULL,feedback_gap=max(feedback_gap,EXISTS(SELECT 1 FROM mail_enrollments WHERE state IN ('pending','active')) OR EXISTS(SELECT 1 FROM mail_attempts)) WHERE singleton=1 AND configuration_binding=? RETURNING feedback_gap")
        .bind(binding.as_slice()).fetch_optional(&mut **transaction).await?.ok_or(SubscriberCommandError::ConfigurationChanged)?;
    match gap {
        0 => Ok(FeedbackHealth::Unavailable),
        1 => Ok(FeedbackHealth::ReconciliationRequired),
        _ => Err(SubscriberApplyError::CorruptStoredState),
    }
}

// SQS rejects requests more than 900 seconds after their signed date. One fixed-date
// request then has 20 seconds of long polling and 120 seconds of visibility, plus
// 2 seconds for date granularity.
const RECEIVE_RECOVERY_SECONDS: i64 = 1042;
const RECOVERY_QUIET_SECONDS: i64 = 180;

struct FeedbackContinuity {
    run: Option<Uuid>,
    source: Option<[u8; 32]>,
    retention: Option<u32>,
    observed: Option<i64>,
    gap: bool,
    provider_at: Option<i64>,
    clock_regressed: bool,
    poll: Option<FeedbackPollIntent>,
    recover_after: Option<i64>,
    quiet_since: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct FeedbackContinuityRow<'r> {
    feedback_source: Option<&'r [u8]>,
    feedback_retention: Option<i64>,
    last_feedback_observed_at: Option<i64>,
    feedback_gap: i64,
    feedback_provider_at: Option<i64>,
    feedback_clock_regressed: i64,
    feedback_poll_signed_at: Option<i64>,
    feedback_recover_after: Option<i64>,
    feedback_quiet_since: Option<i64>,
}

impl FeedbackContinuity {
    fn from_row(row: &SqliteRow) -> Result<Self, SubscriberLoadError> {
        let stored = FeedbackContinuityRow::from_row(row)?;
        let poll = match (
            optional_row_uuid(row, "feedback_poll")?,
            stored.feedback_poll_signed_at,
        ) {
            (Some(poll_id), Some(signed_at)) => Some(FeedbackPollIntent {
                poll_id,
                signed_at: stored_time(signed_at)?,
                recover_after: stored_time(signed_at.saturating_add(RECEIVE_RECOVERY_SECONDS))?,
            }),
            (None, None) => None,
            _ => return Err(SubscriberLoadError::CorruptStoredState),
        };
        let state = Self {
            run: optional_row_uuid(row, "feedback_run")?,
            source: stored
                .feedback_source
                .map(|_| row_bytes(row, "feedback_source"))
                .transpose()?,
            retention: stored
                .feedback_retention
                .map(|value| {
                    u32::try_from(value).map_err(|_| SubscriberLoadError::CorruptStoredState)
                })
                .transpose()?,
            observed: stored.last_feedback_observed_at,
            gap: stored_boolean(stored.feedback_gap)?,
            provider_at: stored.feedback_provider_at,
            clock_regressed: stored_boolean(stored.feedback_clock_regressed)?,
            poll,
            recover_after: stored.feedback_recover_after,
            quiet_since: stored.feedback_quiet_since,
        };
        state.validate()?;
        Ok(state)
    }

    fn validate(&self) -> Result<(), SubscriberLoadError> {
        for timestamp in [
            self.observed,
            self.provider_at,
            self.recover_after,
            self.quiet_since,
        ] {
            stored_optional_time(timestamp)?;
        }
        if self.source.is_some() != self.retention.is_some()
            || (self.run.is_some() && self.source.is_none())
            || (self.observed.is_some() && self.source.is_none())
            || self
                .retention
                .is_some_and(|value| !(60..=1_209_600).contains(&value))
            || (self.poll.is_some() && self.run.is_none())
            || (self.quiet_since.is_some() && self.recover_after.is_none())
        {
            return Err(SubscriberLoadError::CorruptStoredState);
        }
        Ok(())
    }
}

fn stored_boolean(value: i64) -> Result<bool, SubscriberLoadError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(SubscriberLoadError::CorruptStoredState),
    }
}

async fn feedback_continuity(
    transaction: &mut Transaction<'_, Sqlite>,
    binding: [u8; 32],
) -> Result<FeedbackContinuity, SubscriberApplyError> {
    let row=sqlx::query("SELECT feedback_run,feedback_source,feedback_retention,last_feedback_observed_at,feedback_gap,feedback_provider_at,feedback_clock_regressed,feedback_poll,feedback_poll_signed_at,feedback_recover_after,feedback_quiet_since FROM mail_control_state WHERE singleton=1 AND configuration_binding=?")
        .bind(binding.as_slice()).fetch_optional(&mut **transaction).await?.ok_or(SubscriberCommandError::ConfigurationChanged)?;
    FeedbackContinuity::from_row(&row).map_err(load_error)
}

async fn has_retained_subscribers(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<bool, SubscriberApplyError> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mail_enrollments WHERE state IN ('pending','active')) OR EXISTS(SELECT 1 FROM mail_attempts)").fetch_one(&mut **transaction).await?)
}

fn feedback_gap(
    state: &FeedbackContinuity,
    source: [u8; 32],
    retention: u32,
    provider_now: i64,
) -> bool {
    state.source != Some(source)
        || state.observed.is_none_or(|observed| {
            provider_now.saturating_sub(observed)
                >= i64::from(retention.min(state.retention.unwrap_or(retention)))
        })
}

pub(crate) async fn begin_feedback_run(
    transaction: &mut Transaction<'_, Sqlite>,
    command: BeginFeedbackRun,
    _now: i64,
) -> Result<FeedbackRunAdmission, SubscriberApplyError> {
    if !(60..=1_209_600).contains(&command.retention_seconds) {
        return Err(SubscriberCommandError::InvalidValue.into());
    }
    let previous = feedback_continuity(transaction, command.configuration_binding).await?;
    let provider_now = command.provider_now.unix_timestamp();
    let gap = previous.gap
        || (has_retained_subscribers(transaction).await?
            && feedback_gap(
                &previous,
                command.source_binding,
                command.retention_seconds,
                provider_now,
            ));
    let recover_after = previous.recover_after.max(
        previous
            .poll
            .map(|poll| poll.recover_after.unix_timestamp()),
    );
    let run_id = Uuid::new_v4();
    let retention = command
        .retention_seconds
        .min(previous.retention.unwrap_or(command.retention_seconds));
    sqlx::query("UPDATE mail_control_state SET feedback_run=?,feedback_source=?,feedback_retention=?,feedback_gap=?,last_feedback_ok_at=NULL,feedback_poll=NULL,feedback_poll_signed_at=NULL,feedback_recover_after=?,feedback_quiet_since=NULL,feedback_provider_at=?,feedback_clock_regressed=? WHERE singleton=1")
        .bind(run_id.as_bytes().as_slice()).bind(command.source_binding.as_slice()).bind(i64::from(retention)).bind(i64::from(gap)).bind(recover_after).bind(Some(provider_now).max(previous.provider_at)).bind(i64::from(previous.provider_at.is_some_and(|saved|provider_now<saved))).execute(&mut **transaction).await?;
    Ok(FeedbackRunAdmission {
        run_id,
        health: if gap {
            FeedbackHealth::ReconciliationRequired
        } else {
            FeedbackHealth::Unavailable
        },
        recover_after: stored_optional_time(recover_after).map_err(load_error)?,
    })
}

pub(crate) async fn record_feedback_observation(
    transaction: &mut Transaction<'_, Sqlite>,
    command: RecordFeedbackObservation,
    now: i64,
) -> Result<(), SubscriberApplyError> {
    if !(60..=1_209_600).contains(&command.retention_seconds) {
        return Err(SubscriberCommandError::InvalidValue.into());
    }
    let previous = feedback_continuity(transaction, command.configuration_binding).await?;
    if previous.run != Some(command.run_id) {
        return Err(SubscriberCommandError::ConfigurationChanged.into());
    }
    let (provider_now, drained, poison, checked) = match command.observation {
        FeedbackObservation::Unavailable => (None, false, false, false),
        FeedbackObservation::Checked { provider_now } => {
            (Some(provider_now.unix_timestamp()), false, false, true)
        }
        FeedbackObservation::ReconciliationRequired => (None, false, true, false),
        FeedbackObservation::Observed {
            provider_now,
            drained,
        } => (Some(provider_now.unix_timestamp()), drained, false, false),
    };
    if drained && previous.poll.is_some() {
        return Err(SubscriberCommandError::InvalidValue.into());
    }
    let advancing =
        provider_now.filter(|time| previous.provider_at.is_none_or(|saved| *time >= saved));
    let gap = previous.gap
        || poison
        || (has_retained_subscribers(transaction).await?
            && advancing.is_some_and(|time| {
                feedback_gap(
                    &previous,
                    command.source_binding,
                    command.retention_seconds,
                    time,
                )
            }));
    let FeedbackReadiness {
        recover_after,
        quiet_since,
        ready,
    } = previous.readiness(advancing, drained, checked, gap, command.source_binding);
    let preserve_freshness = checked
        && advancing.is_some()
        && !gap
        && recover_after.is_none()
        && previous.source == Some(command.source_binding);
    let clock_regressed = if provider_now.is_some() {
        advancing.is_none()
    } else {
        previous.clock_regressed
    };
    let observed = ready.then_some(advancing).flatten();
    let retention = if observed.is_some() {
        command.retention_seconds
    } else {
        command
            .retention_seconds
            .min(previous.retention.unwrap_or(command.retention_seconds))
    };
    sqlx::query("UPDATE mail_control_state SET feedback_source=?,feedback_retention=?,feedback_gap=?,last_feedback_ok_at=CASE WHEN ? THEN last_feedback_ok_at ELSE ? END,last_feedback_observed_at=COALESCE(?,last_feedback_observed_at),feedback_provider_at=COALESCE(?,feedback_provider_at),feedback_recover_after=?,feedback_quiet_since=?,feedback_clock_regressed=? WHERE singleton=1")
        .bind(command.source_binding.as_slice()).bind(i64::from(retention)).bind(i64::from(gap)).bind(preserve_freshness).bind(ready.then_some(now)).bind(observed).bind(advancing).bind(recover_after).bind(quiet_since).bind(i64::from(clock_regressed)).execute(&mut **transaction).await?;
    Ok(())
}

struct FeedbackReadiness {
    recover_after: Option<i64>,
    quiet_since: Option<i64>,
    ready: bool,
}

impl FeedbackContinuity {
    /// Only provider time can expire an uncertain receive. A delivered message,
    /// failed observation, or changed source restarts the subsequent quiet window.
    fn readiness(
        &self,
        advancing: Option<i64>,
        drained: bool,
        checked: bool,
        gap: bool,
        source: [u8; 32],
    ) -> FeedbackReadiness {
        let mut recover_after = self.recover_after;
        let mut quiet_since =
            if checked && advancing.is_some() && !gap && self.source == Some(source) {
                self.quiet_since
            } else {
                None
            };
        let mut ready = drained && advancing.is_some() && !gap;
        if let Some(deadline) = recover_after {
            if let Some(provider_now) =
                advancing.filter(|time| *time >= deadline && drained && !gap)
            {
                let since = self.quiet_since.unwrap_or(provider_now);
                quiet_since = Some(since);
                if provider_now.saturating_sub(since) >= RECOVERY_QUIET_SECONDS {
                    recover_after = None;
                    quiet_since = None;
                } else {
                    ready = false;
                }
            } else {
                ready = false;
            }
        }
        FeedbackReadiness {
            recover_after,
            quiet_since,
            ready,
        }
    }
}

pub(crate) async fn begin_feedback_poll(
    transaction: &mut Transaction<'_, Sqlite>,
    binding: [u8; 32],
    run_id: Uuid,
    now: i64,
) -> Result<FeedbackPollAdmission, SubscriberApplyError> {
    let previous = feedback_continuity(transaction, binding).await?;
    if previous.run != Some(run_id) {
        return Err(SubscriberCommandError::ConfigurationChanged.into());
    }
    if previous.poll.is_some() {
        return Err(SubscriberCommandError::AttemptConflict.into());
    }
    if previous.clock_regressed
        || previous.recover_after.is_some_and(|deadline| {
            previous
                .provider_at
                .is_none_or(|observed| observed < deadline)
        })
    {
        return Ok(FeedbackPollAdmission::Recovering);
    }
    let intent = FeedbackPollIntent {
        poll_id: Uuid::new_v4(),
        signed_at: stored_time(now).map_err(load_error)?,
        recover_after: stored_time(now.saturating_add(RECEIVE_RECOVERY_SECONDS))
            .map_err(load_error)?,
    };
    sqlx::query(
        "UPDATE mail_control_state SET feedback_poll=?,feedback_poll_signed_at=? WHERE singleton=1",
    )
    .bind(intent.poll_id.as_bytes().as_slice())
    .bind(now)
    .execute(&mut **transaction)
    .await?;
    Ok(FeedbackPollAdmission::Ready(intent))
}

pub(crate) async fn complete_feedback_poll(
    transaction: &mut Transaction<'_, Sqlite>,
    binding: [u8; 32],
    run_id: Uuid,
    poll_id: Uuid,
) -> Result<(), SubscriberApplyError> {
    let changed=sqlx::query("UPDATE mail_control_state SET feedback_poll=NULL,feedback_poll_signed_at=NULL WHERE singleton=1 AND configuration_binding=? AND feedback_run=? AND feedback_poll=?").bind(binding.as_slice()).bind(run_id.as_bytes().as_slice()).bind(poll_id.as_bytes().as_slice()).execute(&mut **transaction).await?.rows_affected();
    if changed != 1 {
        return Err(SubscriberCommandError::ConfigurationChanged.into());
    }
    Ok(())
}

pub(crate) async fn defer_feedback_poll(
    transaction: &mut Transaction<'_, Sqlite>,
    binding: [u8; 32],
    run_id: Uuid,
    poll_id: Uuid,
) -> Result<(), SubscriberApplyError> {
    let previous = feedback_continuity(transaction, binding).await?;
    let poll = previous
        .poll
        .filter(|poll| poll.poll_id == poll_id)
        .ok_or(SubscriberCommandError::ConfigurationChanged)?;
    if previous.run != Some(run_id) {
        return Err(SubscriberCommandError::ConfigurationChanged.into());
    }
    let recover_after = previous
        .recover_after
        .max(Some(poll.recover_after.unix_timestamp()));
    sqlx::query("UPDATE mail_control_state SET feedback_poll=NULL,feedback_poll_signed_at=NULL,feedback_recover_after=?,feedback_quiet_since=NULL,last_feedback_ok_at=NULL WHERE singleton=1").bind(recover_after).execute(&mut **transaction).await?;
    Ok(())
}

pub(crate) async fn finish_feedback_run(
    transaction: &mut Transaction<'_, Sqlite>,
    binding: [u8; 32],
    run_id: Uuid,
) -> Result<(), SubscriberApplyError> {
    let changed=sqlx::query("UPDATE mail_control_state SET feedback_run=NULL,last_feedback_ok_at=NULL WHERE singleton=1 AND configuration_binding=? AND feedback_run=? AND feedback_poll IS NULL").bind(binding.as_slice()).bind(run_id.as_bytes().as_slice()).execute(&mut **transaction).await?.rows_affected();
    if changed != 1 {
        return Err(SubscriberCommandError::ConfigurationChanged.into());
    }
    Ok(())
}

pub(crate) async fn audience_cutoff(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<u64, CampaignApplyError> {
    let sequence: Option<i64> =
        sqlx::query_scalar("SELECT enrollment_sequence FROM mail_control_state WHERE singleton=1")
            .fetch_optional(&mut **transaction)
            .await?;
    u64::try_from(sequence.unwrap_or(0)).map_err(|_| CampaignApplyError::CorruptStoredState)
}

async fn reserve_budget(
    transaction: &mut Transaction<'_, Sqlite>,
    policy: &Policy,
    day: i64,
    confirmation: bool,
) -> Result<bool, SubscriberApplyError> {
    sqlx::query("INSERT INTO mail_daily_budget VALUES (?,0,0) ON CONFLICT(day) DO NOTHING")
        .bind(day)
        .execute(&mut **transaction)
        .await?;
    Ok(sqlx::query("UPDATE mail_daily_budget SET total=total+1,confirmations=confirmations+? WHERE day=? AND total < ? AND confirmations+? <= ?")
        .bind(i64::from(confirmation)).bind(day).bind(policy.daily).bind(i64::from(confirmation)).bind(policy.confirmations).execute(&mut **transaction).await?.rows_affected() == 1)
}

async fn instance_version(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<i64, SubscriberApplyError> {
    let version: i64 =
        sqlx::query_scalar("SELECT version FROM instance_identity WHERE singleton=1")
            .fetch_one(&mut **transaction)
            .await?;
    if version <= 0 {
        return Err(SubscriberApplyError::CorruptStoredState);
    }
    Ok(version)
}

fn recipient_binding(attempt: Uuid, digest: &SubscriberDigest) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key("maincopy mail attempt recipient binding v1");
    hasher.update(attempt.as_bytes());
    hasher.update(digest.as_bytes());
    *hasher.finalize().as_bytes()
}

#[derive(Clone, Copy)]
enum EnrollmentState {
    Pending,
    Active,
    Removed,
}

struct Enrollment {
    id: Uuid,
    generation: Uuid,
    address: Option<EmailAddress>,
    digest: Option<SubscriberDigest>,
    nonce: Option<SubscriberDigest>,
    nonce_expires: Option<i64>,
    pending_expires: Option<i64>,
    confirmed_sequence: Option<u64>,
    requested_at: i64,
    state: EnrollmentState,
}

impl Enrollment {
    fn into_permit(
        self,
        mail_epoch: Uuid,
        attempt_id: Uuid,
        attempt_fence: Uuid,
        campaign_id: Option<CampaignId>,
        admitted_at: OffsetDateTime,
    ) -> Result<DeliveryPermit, SubscriberApplyError> {
        Ok(DeliveryPermit {
            mail_epoch,
            attempt_id,
            attempt_fence,
            enrollment: self.id,
            generation: self.generation,
            address: self
                .address
                .ok_or(SubscriberApplyError::CorruptStoredState)?,
            campaign_id,
            admitted_at,
        })
    }

    fn may_request_confirmation(&self, now: i64) -> Result<bool, SubscriberApplyError> {
        match self.state {
            EnrollmentState::Active => Ok(false),
            EnrollmentState::Pending => Ok(now
                >= self
                    .requested_at
                    .saturating_add(CONFIRMATION_COOLDOWN_SECONDS)),
            EnrollmentState::Removed => Err(SubscriberApplyError::CorruptStoredState),
        }
    }

    fn confirmation_expiry(
        &self,
        generation: Uuid,
        requested: OffsetDateTime,
        now: i64,
    ) -> Result<Option<i64>, SubscriberCommandError> {
        let Some(pending) = self.pending_expires.filter(|pending| *pending > now) else {
            return Ok(None);
        };
        if self.generation != generation || !matches!(self.state, EnrollmentState::Pending) {
            return Ok(None);
        }
        let expiry = requested.unix_timestamp();
        if !(now + 1..=pending.min(now + PENDING_SECONDS)).contains(&expiry) {
            return Err(SubscriberCommandError::InvalidValue);
        }
        Ok(Some(expiry))
    }

    fn is_campaign_recipient(&self, generation: Uuid, cutoff: u64) -> bool {
        self.generation == generation
            && matches!(self.state, EnrollmentState::Active)
            && self
                .confirmed_sequence
                .is_some_and(|sequence| sequence <= cutoff)
    }
}

fn row_digest(
    row: &SqliteRow,
    name: &str,
) -> Result<Option<SubscriberDigest>, SubscriberLoadError> {
    row.try_get::<Option<&[u8]>, _>(name)?
        .map(|value| {
            Ok(SubscriberDigest::from_bytes(
                value
                    .try_into()
                    .map_err(|_| SubscriberLoadError::CorruptStoredState)?,
            ))
        })
        .transpose()
}

#[derive(sqlx::FromRow)]
struct EnrollmentRow<'r> {
    address: Option<&'r str>,
    state: &'r str,
    created_at: i64,
    confirmed_at: Option<i64>,
    retire_at: Option<i64>,
    confirmation_requested_at: i64,
    nonce_expires_at: Option<i64>,
    pending_expires_at: Option<i64>,
    confirmed_sequence: Option<i64>,
    valid_widths: bool,
}

impl EnrollmentRow<'_> {
    fn state(
        &self,
        has_digest: bool,
        has_nonce: bool,
    ) -> Result<EnrollmentState, SubscriberLoadError> {
        // Match the complete storage shape before constructing a domain value.
        let shape = (
            self.state,
            self.address.is_some(),
            has_digest,
            has_nonce,
            self.pending_expires_at,
            self.confirmed_sequence,
            self.confirmed_at,
            self.retire_at,
        );
        match shape {
            ("pending", true, true, _, Some(_), None, None, None) => Ok(EnrollmentState::Pending),
            ("active", true, true, false, None, Some(1..), Some(_), None) => {
                Ok(EnrollmentState::Active)
            }
            ("removed", false, false, false, None, None, None, Some(_)) => {
                Ok(EnrollmentState::Removed)
            }
            _ => Err(SubscriberLoadError::CorruptStoredState),
        }
    }

    fn validate_times(&self) -> Result<(), SubscriberLoadError> {
        let created = stored_time(self.created_at)?;
        let requested = stored_time(self.confirmation_requested_at)?;
        let pending = stored_optional_time(self.pending_expires_at)?;
        let nonce = stored_optional_time(self.nonce_expires_at)?;
        stored_optional_time(self.confirmed_at)?;
        stored_optional_time(self.retire_at)?;
        if requested < created
            || pending.is_some_and(|expires| {
                !(time::Duration::seconds(1)..=time::Duration::seconds(PENDING_SECONDS))
                    .contains(&(expires - created))
            })
            || nonce.is_some_and(|expires| expires <= created || Some(expires) > pending)
        {
            return Err(SubscriberLoadError::CorruptStoredState);
        }
        Ok(())
    }
}

fn stored_address(raw: &str) -> Result<EmailAddress, SubscriberLoadError> {
    let address = EmailAddress::parse(raw).map_err(|_| SubscriberLoadError::CorruptStoredState)?;
    if address.as_str() != raw {
        return Err(SubscriberLoadError::CorruptStoredState);
    }
    Ok(address)
}

fn enrollment(row: SqliteRow) -> Result<Enrollment, SubscriberLoadError> {
    let stored = EnrollmentRow::from_row(&row)?;
    let digest = row_digest(&row, "mailbox_digest")?;
    let nonce = row_digest(&row, "nonce_digest")?;
    if !stored.valid_widths || nonce.is_some() != stored.nonce_expires_at.is_some() {
        return Err(SubscriberLoadError::CorruptStoredState);
    }
    let state = stored.state(digest.is_some(), nonce.is_some())?;
    stored.validate_times()?;
    Ok(Enrollment {
        id: row_uuid(&row, "enrollment_id")?,
        generation: row_uuid(&row, "generation")?,
        address: stored.address.map(stored_address).transpose()?,
        digest,
        nonce,
        nonce_expires: stored.nonce_expires_at,
        pending_expires: stored.pending_expires_at,
        confirmed_sequence: stored.confirmed_sequence.map(|sequence| sequence as u64),
        requested_at: stored.confirmation_requested_at,
        state,
    })
}

async fn load_enrollment(
    transaction: &mut Transaction<'_, Sqlite>,
    id: Uuid,
) -> Result<Option<Enrollment>, SubscriberApplyError> {
    QueryBuilder::new(ENROLLMENTS)
        .push(" WHERE enrollment_id=")
        .push_bind(id.as_bytes().as_slice())
        .build()
        .fetch_optional(&mut **transaction)
        .await?
        .map(enrollment)
        .transpose()
        .map_err(load_error)
}

async fn suppressed(
    transaction: &mut Transaction<'_, Sqlite>,
    digest: &SubscriberDigest,
    now: i64,
) -> Result<bool, SubscriberApplyError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM mail_suppressions WHERE mailbox_digest=? AND expires_at>?)",
    )
    .bind(digest.as_bytes().as_slice())
    .bind(now)
    .fetch_one(&mut **transaction)
    .await?)
}

pub(crate) async fn request_enrollment(
    transaction: &mut Transaction<'_, Sqlite>,
    command: RequestEnrollment,
    now: i64,
) -> Result<EnrollmentRequestResult, SubscriberApplyError> {
    let policy = require_enabled(transaction, command.configuration_binding, now).await?;
    for id in [
        command.enrollment,
        command.generation,
        command.confirmation_attempt,
    ] {
        random_uuid(id)?;
    }
    if suppressed(transaction, &command.mailbox_digest, now).await? {
        return Ok(EnrollmentRequestResult::Unchanged);
    }
    let prior = load_mailbox(transaction, &command.mailbox_digest).await?;
    if let Some(prior) = &prior
        && !prior.may_request_confirmation(now)?
    {
        return Ok(EnrollmentRequestResult::Unchanged);
    }
    let (enrollments, attempts): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM mail_enrollments),(SELECT count(*) FROM mail_attempts)",
    )
    .fetch_one(&mut **transaction)
    .await?;
    if enrollments >= MAX_ENROLLMENTS || attempts >= MAX_ATTEMPTS {
        return Ok(EnrollmentRequestResult::Unchanged);
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mail_enrollments WHERE enrollment_id=?) OR EXISTS(SELECT 1 FROM mail_attempts WHERE attempt_id=?)")
        .bind(command.enrollment.as_bytes().as_slice()).bind(command.confirmation_attempt.as_bytes().as_slice()).fetch_one(&mut **transaction).await?;
    if exists {
        return Ok(EnrollmentRequestResult::Unchanged);
    }
    let day = now.div_euclid(86_400);
    if !reserve_budget(transaction, &policy, day, true).await? {
        return Ok(EnrollmentRequestResult::Unchanged);
    }
    if let Some(prior) = prior {
        remove_enrollment(transaction, prior.id, now).await?;
    }
    insert_requested_enrollment(transaction, command, day, now).await?;
    Ok(EnrollmentRequestResult::Queued)
}

async fn load_mailbox(
    transaction: &mut Transaction<'_, Sqlite>,
    digest: &SubscriberDigest,
) -> Result<Option<Enrollment>, SubscriberApplyError> {
    QueryBuilder::new(ENROLLMENTS)
        .push(" WHERE mailbox_digest=")
        .push_bind(digest.as_bytes().as_slice())
        .build()
        .fetch_optional(&mut **transaction)
        .await?
        .map(enrollment)
        .transpose()
        .map_err(load_error)
}

async fn insert_requested_enrollment(
    transaction: &mut Transaction<'_, Sqlite>,
    command: RequestEnrollment,
    day: i64,
    now: i64,
) -> Result<(), SubscriberApplyError> {
    let instance = instance_version(transaction).await?;
    let (mail_epoch, feedback_source) = current_delivery_identity(transaction).await?;
    sqlx::query("INSERT INTO mail_enrollments (enrollment_id,generation,mailbox_digest,address,state,pending_expires_at,created_at,confirmation_requested_at) VALUES (?,?,?,?,'pending',?,?,?)")
        .bind(command.enrollment.as_bytes().as_slice()).bind(command.generation.as_bytes().as_slice()).bind(command.mailbox_digest.as_bytes().as_slice())
        .bind(command.address.as_str()).bind(now+PENDING_SECONDS).bind(now).bind(now).execute(&mut **transaction).await?;
    sqlx::query("INSERT INTO mail_attempts (mail_epoch,feedback_source,attempt_id,attempt_fence,enrollment_id,generation,kind,recipient_binding,configuration_binding,instance_version,outcome,budget_day,created_at,retire_at) VALUES (?,?,?,?,?,?,'confirmation',?,?,?,'queued',?,?,?)")
        .bind(mail_epoch.as_bytes().as_slice()).bind(feedback_source.as_slice()).bind(command.confirmation_attempt.as_bytes().as_slice()).bind(Uuid::new_v4().as_bytes().as_slice()).bind(command.enrollment.as_bytes().as_slice()).bind(command.generation.as_bytes().as_slice())
        .bind(recipient_binding(command.confirmation_attempt,&command.mailbox_digest).as_slice()).bind(command.configuration_binding.as_slice()).bind(instance).bind(day).bind(now).bind(now+ATTEMPT_RETENTION_SECONDS).execute(&mut **transaction).await?;
    Ok(())
}

async fn remove_enrollment(
    transaction: &mut Transaction<'_, Sqlite>,
    enrollment: Uuid,
    now: i64,
) -> Result<(), SubscriberApplyError> {
    sqlx::query("UPDATE mail_attempts SET outcome='cancelled',finished_at=? WHERE enrollment_id=? AND outcome='queued'")
        .bind(now).bind(enrollment.as_bytes().as_slice()).execute(&mut **transaction).await?;
    sqlx::query("UPDATE mail_enrollments SET address=NULL,mailbox_digest=NULL,nonce_digest=NULL,nonce_expires_at=NULL,pending_expires_at=NULL,confirmed_at=NULL,confirmed_sequence=NULL,state='removed',retire_at=? WHERE enrollment_id=?")
        .bind(now+ATTEMPT_RETENTION_SECONDS).bind(enrollment.as_bytes().as_slice()).execute(&mut **transaction).await?;
    Ok(())
}

pub(crate) async fn remove(
    transaction: &mut Transaction<'_, Sqlite>,
    command: ManageEnrollment,
    now: i64,
) -> Result<ControlOutcome, SubscriberApplyError> {
    random_uuid(command.enrollment)?;
    random_uuid(command.generation)?;
    let Some(enrollment) = load_enrollment(transaction, command.enrollment).await? else {
        return Ok(ControlOutcome::Unchanged);
    };
    if enrollment.generation != command.generation
        || matches!(enrollment.state, EnrollmentState::Removed)
    {
        return Ok(ControlOutcome::Unchanged);
    }
    remove_enrollment(transaction, enrollment.id, now).await?;
    Ok(ControlOutcome::Changed)
}

pub(crate) async fn confirm(
    transaction: &mut Transaction<'_, Sqlite>,
    command: ConfirmEnrollment,
    now: i64,
) -> Result<ControlOutcome, SubscriberApplyError> {
    random_uuid(command.enrollment)?;
    random_uuid(command.generation)?;
    let Some(enrollment) = load_enrollment(transaction, command.enrollment).await? else {
        return Ok(ControlOutcome::Unchanged);
    };
    if enrollment.generation != command.generation
        || !matches!(enrollment.state, EnrollmentState::Pending)
        || enrollment.nonce_expires != Some(command.expires_at.unix_timestamp())
        || command.expires_at.unix_timestamp() <= now
        || enrollment
            .pending_expires
            .is_none_or(|expires| expires <= now)
        || !enrollment.nonce.as_ref().is_some_and(|nonce| {
            bool::from(nonce.as_bytes().ct_eq(command.nonce_digest.as_bytes()))
        })
    {
        return Ok(ControlOutcome::Unchanged);
    }
    let digest = enrollment
        .digest
        .as_ref()
        .ok_or(SubscriberApplyError::CorruptStoredState)?;
    if suppressed(transaction, digest, now).await? {
        return Ok(ControlOutcome::Unchanged);
    }
    let state = policy(transaction).await?;
    let sequence = state
        .sequence
        .checked_add(1)
        .and_then(|v| i64::try_from(v).ok())
        .ok_or(SubscriberApplyError::CorruptStoredState)?;
    sqlx::query("UPDATE mail_control_state SET enrollment_sequence=? WHERE singleton=1")
        .bind(sequence)
        .execute(&mut **transaction)
        .await?;
    sqlx::query("UPDATE mail_enrollments SET state='active',nonce_digest=NULL,nonce_expires_at=NULL,pending_expires_at=NULL,confirmed_at=?,confirmed_sequence=? WHERE enrollment_id=?")
        .bind(now).bind(sequence).bind(command.enrollment.as_bytes().as_slice()).execute(&mut **transaction).await?;
    Ok(ControlOutcome::Changed)
}

struct Attempt {
    mail_epoch: Uuid,
    feedback_source: [u8; 32],
    id: Uuid,
    fence: Uuid,
    enrollment: Uuid,
    generation: Uuid,
    campaign: Option<CampaignId>,
    recipient_binding: SubscriberDigest,
    configuration_binding: [u8; 32],
    instance_version: i64,
    outcome: AttemptOutcome,
    provider_id: Option<MessageId>,
    day: i64,
    created_at: i64,
    admitted_at: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct AttemptRow<'r> {
    kind: &'r str,
    outcome: &'r str,
    provider_message_id: Option<&'r str>,
    instance_version: i64,
    budget_day: i64,
    created_at: i64,
    retire_at: i64,
    admitted_at: Option<i64>,
    finished_at: Option<i64>,
    valid_widths: bool,
}

impl AttemptRow<'_> {
    fn validate_times(&self) -> Result<(), SubscriberLoadError> {
        let created = stored_time(self.created_at)?;
        let retired = stored_time(self.retire_at)?;
        let admitted = stored_optional_time(self.admitted_at)?;
        let finished = stored_optional_time(self.finished_at)?;
        let day = self
            .budget_day
            .checked_mul(86_400)
            .ok_or(SubscriberLoadError::CorruptStoredState)?;
        stored_time(day)?;
        if self.instance_version <= 0
            || !(time::Duration::seconds(1)..=time::Duration::seconds(ATTEMPT_RETENTION_SECONDS))
                .contains(&(retired - created))
            || finished
                .zip(admitted)
                .is_some_and(|(finished, admitted)| finished < admitted)
        {
            return Err(SubscriberLoadError::CorruptStoredState);
        }
        Ok(())
    }

    fn outcome(&self) -> Result<AttemptOutcome, SubscriberLoadError> {
        let shape = (
            self.outcome,
            self.admitted_at,
            self.finished_at,
            self.provider_message_id.is_some(),
        );
        match shape {
            ("queued", None, None, false) => Ok(AttemptOutcome::Queued),
            ("admitted", Some(_), None, false) => Ok(AttemptOutcome::Admitted),
            ("cancelled", None, Some(_), false) => Ok(AttemptOutcome::Cancelled),
            ("accepted", Some(_), Some(_), true) => Ok(AttemptOutcome::Accepted),
            ("rejected", Some(_), Some(_), false) => Ok(AttemptOutcome::Rejected),
            ("unknown", Some(_), Some(_), false) => Ok(AttemptOutcome::Unknown),
            _ => Err(SubscriberLoadError::CorruptStoredState),
        }
    }

    fn campaign(&self, row: &SqliteRow) -> Result<Option<CampaignId>, SubscriberLoadError> {
        // Campaign IDs precede this mail implementation and need not be UUID v4.
        let campaign = row
            .try_get::<Option<&[u8]>, _>("campaign_id")?
            .map(|bytes| {
                Uuid::from_slice(bytes)
                    .map(CampaignId)
                    .map_err(|_| SubscriberLoadError::CorruptStoredState)
            })
            .transpose()?;
        let fence = optional_row_uuid(row, "campaign_fence")?;
        match (self.kind, campaign, fence, self.outcome) {
            ("confirmation", None, None, _) => Ok(None),
            (
                "campaign",
                Some(campaign),
                Some(_),
                "admitted" | "accepted" | "rejected" | "unknown",
            ) => Ok(Some(campaign)),
            _ => Err(SubscriberLoadError::CorruptStoredState),
        }
    }
}

fn row_bytes<const N: usize>(row: &SqliteRow, name: &str) -> Result<[u8; N], SubscriberLoadError> {
    row.try_get::<&[u8], _>(name)?
        .try_into()
        .map_err(|_| SubscriberLoadError::CorruptStoredState)
}

fn optional_row_uuid(row: &SqliteRow, name: &str) -> Result<Option<Uuid>, SubscriberLoadError> {
    row.try_get::<Option<&[u8]>, _>(name)?
        .map(|_| row_uuid(row, name))
        .transpose()
}

fn attempt(row: SqliteRow) -> Result<Attempt, SubscriberLoadError> {
    let stored = AttemptRow::from_row(&row)?;
    if !stored.valid_widths {
        return Err(SubscriberLoadError::CorruptStoredState);
    }
    stored.validate_times()?;
    Ok(Attempt {
        mail_epoch: row_uuid(&row, "mail_epoch")?,
        feedback_source: row_bytes(&row, "feedback_source")?,
        id: row_uuid(&row, "attempt_id")?,
        fence: row_uuid(&row, "attempt_fence")?,
        enrollment: row_uuid(&row, "enrollment_id")?,
        generation: row_uuid(&row, "generation")?,
        campaign: stored.campaign(&row)?,
        recipient_binding: SubscriberDigest::from_bytes(row_bytes(&row, "recipient_binding")?),
        configuration_binding: row_bytes(&row, "configuration_binding")?,
        instance_version: stored.instance_version,
        outcome: stored.outcome()?,
        provider_id: stored
            .provider_message_id
            .map(MessageId::parse)
            .transpose()
            .map_err(|_| SubscriberLoadError::CorruptStoredState)?,
        day: stored.budget_day,
        created_at: stored.created_at,
        admitted_at: stored.admitted_at,
    })
}

async fn load_attempt(
    transaction: &mut Transaction<'_, Sqlite>,
    id: Uuid,
) -> Result<Option<Attempt>, SubscriberApplyError> {
    QueryBuilder::new(ATTEMPTS)
        .push(" WHERE attempt_id=")
        .push_bind(id.as_bytes().as_slice())
        .build()
        .fetch_optional(&mut **transaction)
        .await?
        .map(attempt)
        .transpose()
        .map_err(load_error)
}

pub(crate) async fn claim_confirmation(
    transaction: &mut Transaction<'_, Sqlite>,
    command: ClaimConfirmation,
    now: i64,
) -> Result<DeliveryAdmission, SubscriberApplyError> {
    let policy = require_enabled(transaction, command.configuration_binding, now).await?;
    let Some(attempt) = load_attempt(transaction, command.attempt_id).await? else {
        return Ok(DeliveryAdmission::Unavailable);
    };
    if attempt.campaign.is_some() || attempt.configuration_binding != command.configuration_binding
    {
        return Err(SubscriberCommandError::AttemptConflict.into());
    }
    if attempt.outcome != AttemptOutcome::Queued {
        return Ok(DeliveryAdmission::AlreadyRecorded(attempt.outcome));
    }
    let Some(enrollment) = load_enrollment(transaction, attempt.enrollment).await? else {
        return Err(SubscriberApplyError::CorruptStoredState);
    };
    if attempt.instance_version != instance_version(transaction).await? {
        return Ok(DeliveryAdmission::Unavailable);
    }
    let Some(expiry) =
        enrollment.confirmation_expiry(attempt.generation, command.expires_at, now)?
    else {
        return Ok(DeliveryAdmission::Unavailable);
    };
    let digest = enrollment
        .digest
        .as_ref()
        .ok_or(SubscriberApplyError::CorruptStoredState)?;
    if suppressed(transaction, digest, now).await? {
        return Ok(DeliveryAdmission::Unavailable);
    }
    let day = now.div_euclid(86_400);
    if attempt.day != day && !reserve_budget(transaction, &policy, day, true).await? {
        return Ok(DeliveryAdmission::Deferred);
    }
    sqlx::query(
        "UPDATE mail_enrollments SET nonce_digest=?,nonce_expires_at=? WHERE enrollment_id=?",
    )
    .bind(command.nonce_digest.as_bytes().as_slice())
    .bind(expiry)
    .bind(enrollment.id.as_bytes().as_slice())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "UPDATE mail_attempts SET outcome='admitted',admitted_at=?,budget_day=? WHERE attempt_id=?",
    )
    .bind(now)
    .bind(day)
    .bind(attempt.id.as_bytes().as_slice())
    .execute(&mut **transaction)
    .await?;
    enrollment
        .into_permit(
            attempt.mail_epoch,
            attempt.id,
            attempt.fence,
            None,
            stored_time(now).map_err(load_error)?,
        )
        .map(DeliveryAdmission::Ready)
}

async fn load_campaign_attempt(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &AdmitCampaignRecipient,
) -> Result<Option<Attempt>, SubscriberApplyError> {
    QueryBuilder::new(ATTEMPTS)
        .push(" WHERE campaign_id=")
        .push_bind(command.campaign_id.0.as_bytes().as_slice())
        .push(" AND enrollment_id=")
        .push_bind(command.enrollment.as_bytes().as_slice())
        .push(" AND generation=")
        .push_bind(command.generation.as_bytes().as_slice())
        .build()
        .fetch_optional(&mut **transaction)
        .await?
        .map(attempt)
        .transpose()
        .map_err(load_error)
}

pub(crate) async fn admit_campaign_recipient(
    transaction: &mut Transaction<'_, Sqlite>,
    command: AdmitCampaignRecipient,
    now: OffsetDateTime,
) -> Result<DeliveryAdmission, SubscriberApplyError> {
    let seconds = now.unix_timestamp();
    let policy = require_enabled(transaction, command.configuration_binding, seconds).await?;
    for id in [command.enrollment, command.generation, command.attempt_id] {
        random_uuid(id)?;
    }
    let approval = require_campaign_admission(
        transaction,
        command.campaign_id,
        command.campaign_fence,
        command.configuration_binding,
        now,
    )
    .await
    .map_err(campaign_error)?;
    if let Some(existing) = load_campaign_attempt(transaction, &command).await? {
        return Ok(DeliveryAdmission::AlreadyRecorded(existing.outcome));
    }
    if load_attempt(transaction, command.attempt_id)
        .await?
        .is_some()
    {
        return Err(SubscriberCommandError::AttemptConflict.into());
    }
    let Some(enrollment) = load_enrollment(transaction, command.enrollment).await? else {
        return Ok(DeliveryAdmission::Unavailable);
    };
    if !enrollment.is_campaign_recipient(command.generation, approval.audience_cutoff) {
        return Ok(DeliveryAdmission::Unavailable);
    }
    let digest = enrollment
        .digest
        .as_ref()
        .ok_or(SubscriberApplyError::CorruptStoredState)?;
    if suppressed(transaction, digest, seconds).await? {
        return Ok(DeliveryAdmission::Unavailable);
    }
    match reserve_campaign_budget(transaction, &policy, command.campaign_id, seconds).await? {
        CampaignBudgetAdmission::Granted => {}
        CampaignBudgetAdmission::RecipientLimit => return Ok(DeliveryAdmission::Unavailable),
        CampaignBudgetAdmission::Deferred => return Ok(DeliveryAdmission::Deferred),
    }
    let day = seconds.div_euclid(86_400);
    let fence = Uuid::new_v4();
    let instance = instance_version(transaction).await?;
    let (mail_epoch, feedback_source) = current_delivery_identity(transaction).await?;
    sqlx::query("INSERT INTO mail_attempts (mail_epoch,feedback_source,attempt_id,attempt_fence,enrollment_id,generation,kind,campaign_id,campaign_fence,recipient_binding,configuration_binding,instance_version,outcome,budget_day,created_at,admitted_at,retire_at) VALUES (?,?,?,?,?,?,'campaign',?,?,?,?,?,'admitted',?,?,?,?)")
        .bind(mail_epoch.as_bytes().as_slice()).bind(feedback_source.as_slice()).bind(command.attempt_id.as_bytes().as_slice()).bind(fence.as_bytes().as_slice()).bind(enrollment.id.as_bytes().as_slice()).bind(enrollment.generation.as_bytes().as_slice())
        .bind(command.campaign_id.0.as_bytes().as_slice()).bind(command.campaign_fence.0.as_bytes().as_slice()).bind(recipient_binding(command.attempt_id,digest).as_slice()).bind(command.configuration_binding.as_slice()).bind(instance)
        .bind(day).bind(seconds).bind(seconds).bind(seconds+ATTEMPT_RETENTION_SECONDS).execute(&mut **transaction).await?;
    enrollment
        .into_permit(
            mail_epoch,
            command.attempt_id,
            fence,
            Some(command.campaign_id),
            now,
        )
        .map(DeliveryAdmission::Ready)
}

enum CampaignBudgetAdmission {
    Granted,
    RecipientLimit,
    Deferred,
}

async fn reserve_campaign_budget(
    transaction: &mut Transaction<'_, Sqlite>,
    policy: &Policy,
    campaign_id: CampaignId,
    seconds: i64,
) -> Result<CampaignBudgetAdmission, SubscriberApplyError> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mail_attempts WHERE campaign_id=?")
        .bind(campaign_id.0.as_bytes().as_slice())
        .fetch_one(&mut **transaction)
        .await?;
    if count >= policy.campaign {
        return Ok(CampaignBudgetAdmission::RecipientLimit);
    }
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM mail_attempts")
        .fetch_one(&mut **transaction)
        .await?;
    if total >= MAX_ATTEMPTS {
        return Ok(CampaignBudgetAdmission::Deferred);
    }
    let day = seconds.div_euclid(86_400);
    if !reserve_budget(transaction, policy, day, false).await? {
        return Ok(CampaignBudgetAdmission::Deferred);
    }
    Ok(CampaignBudgetAdmission::Granted)
}

fn auth_error(error: AuthApplyError) -> SubscriberApplyError {
    match error {
        AuthApplyError::Operation(error) => SubscriberApplyError::Operation(error),
        AuthApplyError::CorruptStoredState => SubscriberApplyError::CorruptStoredState,
        AuthApplyError::Command(AuthCommandError::ScopeEscalation | AuthCommandError::NotFound) => {
            SubscriberCommandError::Forbidden.into()
        }
        AuthApplyError::Command(
            AuthCommandError::Conflict | AuthCommandError::IdempotencyConflict,
        ) => SubscriberCommandError::IdempotencyConflict.into(),
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
        ) => SubscriberApplyError::CorruptStoredState,
    }
}

const MAX_CONSENT_RESETS: i64 = 10_000;

fn reset_fingerprint(command: &ResetSubscriberConsent) -> [u8; 32] {
    let mut fingerprint = CommandFingerprintBuilder::new("mail.consent.reset");
    fingerprint.version(command.expected_version);
    fingerprint.field(&command.configuration_binding);
    fingerprint.finish()
}

fn reset_result(row: &SqliteRow) -> Result<ResetSubscriberConsentResult, SubscriberLoadError> {
    let version = u64::try_from(row.try_get::<i64, _>("version")?)
        .map_err(|_| SubscriberLoadError::CorruptStoredState)?;
    let enrollments = row.try_get::<i64, _>("discarded_enrollments")?;
    let attempts = row.try_get::<i64, _>("discarded_attempts")?;
    let campaigns = row.try_get::<i64, _>("quarantined_campaigns")?;
    if version == 0
        || !(0..=MAX_ENROLLMENTS).contains(&enrollments)
        || !(0..=MAX_ATTEMPTS).contains(&attempts)
        || !(0..=10_000).contains(&campaigns)
    {
        return Err(SubscriberLoadError::CorruptStoredState);
    }
    Ok(ResetSubscriberConsentResult {
        version,
        retired_epoch: row_uuid(row, "retired_epoch")?,
        new_epoch: row_uuid(row, "new_epoch")?,
        discarded_enrollments: enrollments as u64,
        discarded_attempts: attempts as u64,
        quarantined_campaigns: campaigns as u64,
    })
}

async fn replay_reset(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &ResetSubscriberConsent,
    fingerprint: [u8; 32],
) -> Result<Option<ResetSubscriberConsentResult>, SubscriberApplyError> {
    let row=sqlx::query("SELECT audit.principal_kind,audit.actor_user_id,audit.session_id,audit.agent_credential_id,audit.action,audit.outcome,receipt.command_fingerprint,receipt.version,receipt.retired_epoch,receipt.new_epoch,receipt.discarded_enrollments,receipt.discarded_attempts,receipt.quarantined_campaigns FROM admin_audit_events AS audit LEFT JOIN mail_consent_resets AS receipt ON receipt.audit_event_id=audit.audit_event_id WHERE audit.idempotency_key=?").bind(command.audit.idempotency_key.0.as_bytes().as_slice()).fetch_optional(&mut **transaction).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let principal = decode_audit_principal(
        row.try_get("principal_kind")?,
        row.try_get("actor_user_id")?,
        row.try_get("session_id")?,
        row.try_get("agent_credential_id")?,
    )
    .map_err(|_| SubscriberApplyError::CorruptStoredState)?;
    if principal != command.audit.principal
        || row.try_get::<&str, _>("action")? != "mail.consent.reset"
        || row.try_get::<Option<&[u8]>, _>("command_fingerprint")? != Some(fingerprint.as_slice())
    {
        return Err(SubscriberCommandError::IdempotencyConflict.into());
    }
    if row.try_get::<&str, _>("outcome")? != "succeeded" {
        return Err(SubscriberApplyError::CorruptStoredState);
    }
    Ok(Some(reset_result(&row).map_err(load_error)?))
}

pub(crate) async fn reset_consent(
    transaction: &mut Transaction<'_, Sqlite>,
    command: ResetSubscriberConsent,
    executed_at: OffsetDateTime,
) -> Result<ResetSubscriberConsentResult, SubscriberApplyError> {
    let fingerprint = reset_fingerprint(&command);
    if let Some(result) = replay_reset(transaction, &command, fingerprint).await? {
        return Ok(result);
    }
    require_fresh_browser_owner(transaction, &command.audit.principal, executed_at)
        .await
        .map_err(auth_error)?;
    let row=sqlx::query("SELECT control_version,configuration_binding,feedback_gap FROM mail_control_state WHERE singleton=1").fetch_optional(&mut **transaction).await?.ok_or(SubscriberCommandError::ControlsUnavailable)?;
    if row.try_get::<Option<&[u8]>, _>("configuration_binding")?
        != Some(command.configuration_binding.as_slice())
    {
        return Err(SubscriberCommandError::ConfigurationChanged.into());
    }
    if u64::try_from(row.try_get::<i64, _>("control_version")?).ok()
        != Some(command.expected_version)
    {
        return Err(SubscriberCommandError::StaleVersion.into());
    }
    if row.try_get::<i64, _>("feedback_gap")? != 1 {
        return Err(SubscriberCommandError::ReconciliationNotRequired.into());
    }
    let quarantined =
        quarantine_reset_campaigns(transaction, command.audit.idempotency_key.0, executed_at)
            .await
            .map_err(campaign_error)?;
    let result = rotate_consent_epoch(transaction, quarantined).await?;
    append_success_audit(
        transaction,
        &command.audit,
        command.now,
        "mail.consent.reset",
    )
    .await
    .map_err(auth_error)?;
    insert_reset_receipt(
        transaction,
        command.audit.idempotency_key.0,
        *command.audit.audit_event_id.as_uuid(),
        fingerprint,
        result,
    )
    .await?;
    Ok(result)
}

async fn rotate_consent_epoch(
    transaction: &mut Transaction<'_, Sqlite>,
    quarantined_campaigns: u64,
) -> Result<ResetSubscriberConsentResult, SubscriberApplyError> {
    let history: i64 = sqlx::query_scalar("SELECT count(*) FROM mail_consent_resets")
        .fetch_one(&mut **transaction)
        .await?;
    if history >= MAX_CONSENT_RESETS {
        return Err(SubscriberCommandError::Capacity.into());
    }
    let row=sqlx::query("SELECT mail_epoch,control_version,(SELECT count(*) FROM mail_enrollments) AS enrollments,(SELECT count(*) FROM mail_attempts) AS attempts FROM mail_control_state WHERE singleton=1").fetch_one(&mut **transaction).await?;
    let version = row
        .try_get::<i64, _>("control_version")?
        .checked_add(1)
        .ok_or(SubscriberCommandError::Capacity)?;
    let result = ResetSubscriberConsentResult {
        version: version as u64,
        retired_epoch: row_uuid(&row, "mail_epoch").map_err(load_error)?,
        new_epoch: Uuid::new_v4(),
        discarded_enrollments: row.try_get::<i64, _>("enrollments")? as u64,
        discarded_attempts: row.try_get::<i64, _>("attempts")? as u64,
        quarantined_campaigns,
    };
    sqlx::query("DELETE FROM mail_attempts")
        .execute(&mut **transaction)
        .await?;
    sqlx::query("DELETE FROM mail_enrollments")
        .execute(&mut **transaction)
        .await?;
    // The reset does not refund budgets or erase hard-bounce/complaint suppression.
    // No restored or retired generation can be reconstructed by a later result.
    sqlx::query("UPDATE mail_control_state SET mail_epoch=?,control_version=?,mode='paused',feedback_gap=0,last_feedback_ok_at=NULL,last_feedback_observed_at=NULL,feedback_run=NULL,feedback_poll=NULL,feedback_poll_signed_at=NULL,feedback_recover_after=NULL,feedback_quiet_since=NULL,feedback_provider_at=NULL,feedback_clock_regressed=0,feedback_source=NULL,feedback_retention=NULL WHERE singleton=1").bind(result.new_epoch.as_bytes().as_slice()).bind(version).execute(&mut **transaction).await?;
    Ok(result)
}

async fn insert_reset_receipt(
    transaction: &mut Transaction<'_, Sqlite>,
    key: Uuid,
    audit: Uuid,
    fingerprint: [u8; 32],
    result: ResetSubscriberConsentResult,
) -> Result<(), SubscriberApplyError> {
    sqlx::query("INSERT INTO mail_consent_resets VALUES (?,?,?,?,?,?,?,?,?)")
        .bind(key.as_bytes().as_slice())
        .bind(audit.as_bytes().as_slice())
        .bind(fingerprint.as_slice())
        .bind(result.version as i64)
        .bind(result.retired_epoch.as_bytes().as_slice())
        .bind(result.new_epoch.as_bytes().as_slice())
        .bind(result.discarded_enrollments as i64)
        .bind(result.discarded_attempts as i64)
        .bind(result.quarantined_campaigns as i64)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn current_delivery_identity(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<(Uuid, [u8; 32]), SubscriberApplyError> {
    let row =
        sqlx::query("SELECT mail_epoch,feedback_source FROM mail_control_state WHERE singleton=1")
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(SubscriberCommandError::ControlsUnavailable)?;
    Ok((
        row_uuid(&row, "mail_epoch").map_err(load_error)?,
        row.try_get::<&[u8], _>("feedback_source")?
            .try_into()
            .map_err(|_| SubscriberApplyError::CorruptStoredState)?,
    ))
}

async fn retired_epoch(
    transaction: &mut Transaction<'_, Sqlite>,
    epoch: Uuid,
) -> Result<bool, SubscriberApplyError> {
    random_uuid(epoch)?;
    Ok(
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM mail_consent_resets WHERE retired_epoch=?)",
        )
        .bind(epoch.as_bytes().as_slice())
        .fetch_one(&mut **transaction)
        .await?,
    )
}

pub(crate) async fn finish_attempt(
    transaction: &mut Transaction<'_, Sqlite>,
    command: FinishAttempt,
    now: i64,
) -> Result<AttemptOutcome, SubscriberApplyError> {
    if retired_epoch(transaction, command.mail_epoch).await? {
        return Ok(AttemptOutcome::Unknown);
    }
    let existing = load_attempt(transaction, command.attempt_id)
        .await?
        .ok_or(SubscriberCommandError::AttemptConflict)?;
    if existing.mail_epoch != command.mail_epoch {
        return Err(SubscriberCommandError::AttemptConflict.into());
    }
    if existing.fence != command.attempt_fence
        || existing.instance_version != instance_version(transaction).await?
    {
        return Err(SubscriberCommandError::AttemptConflict.into());
    }
    if existing.outcome != AttemptOutcome::Admitted {
        if let SubmissionOutcome::Accepted(message) = &command.outcome
            && existing
                .provider_id
                .as_ref()
                .is_some_and(|saved| saved.as_str() != message.as_str())
        {
            return Err(SubscriberCommandError::AttemptConflict.into());
        }
        return Ok(existing.outcome);
    }
    let (outcome, kind, provider) = match &command.outcome {
        SubmissionOutcome::Accepted(message) => {
            (AttemptOutcome::Accepted, "accepted", Some(message.as_str()))
        }
        SubmissionOutcome::Rejected => (AttemptOutcome::Rejected, "rejected", None),
        SubmissionOutcome::Unknown => (AttemptOutcome::Unknown, "unknown", None),
    };
    sqlx::query(
        "UPDATE mail_attempts SET outcome=?,provider_message_id=?,finished_at=? WHERE attempt_id=?",
    )
    .bind(kind)
    .bind(provider)
    .bind(now.max(existing.admitted_at.unwrap_or(now)))
    .bind(existing.id.as_bytes().as_slice())
    .execute(&mut **transaction)
    .await?;
    Ok(outcome)
}

impl Attempt {
    fn require_feedback(
        &self,
        command: &ApplyFeedback,
        now: i64,
    ) -> Result<(), SubscriberCommandError> {
        if self.campaign != command.campaign_id
            || self.mail_epoch != command.mail_epoch
            || self.feedback_source != command.source_binding
            || !bool::from(self.recipient_binding.as_bytes().ct_eq(&recipient_binding(
                command.attempt_id,
                &command.mailbox_digest,
            )))
            || self
                .provider_id
                .as_ref()
                .is_some_and(|saved| saved.as_str() != command.provider_message_id.as_str())
            || command.sent_at.unix_timestamp() < self.created_at.saturating_sub(300)
            || command.occurred_at.unix_timestamp()
                < command.sent_at.unix_timestamp().saturating_sub(300)
            || command.sent_at.unix_timestamp() > now.saturating_add(300)
            || command.occurred_at.unix_timestamp() > now.saturating_add(300)
        {
            return Err(SubscriberCommandError::AttemptConflict);
        }
        match self.outcome {
            AttemptOutcome::Queued | AttemptOutcome::Cancelled | AttemptOutcome::Rejected => {
                return Err(SubscriberCommandError::AttemptConflict);
            }
            AttemptOutcome::Admitted | AttemptOutcome::Unknown | AttemptOutcome::Accepted => {}
        }
        Ok(())
    }
}

pub(crate) async fn apply_feedback(
    transaction: &mut Transaction<'_, Sqlite>,
    command: ApplyFeedback,
    now: i64,
) -> Result<ControlOutcome, SubscriberApplyError> {
    if policy(transaction).await?.binding != Some(command.configuration_binding) {
        return Err(SubscriberCommandError::ConfigurationChanged.into());
    }
    if retired_epoch(transaction, command.mail_epoch).await? {
        return Ok(ControlOutcome::Unchanged);
    }
    let existing = load_attempt(transaction, command.attempt_id)
        .await?
        .ok_or(SubscriberCommandError::AttemptConflict)?;
    existing.require_feedback(&command, now)?;
    let mut changed = existing.outcome != AttemptOutcome::Accepted;
    if changed {
        sqlx::query("UPDATE mail_attempts SET outcome='accepted',provider_message_id=?,finished_at=? WHERE attempt_id=?")
            .bind(command.provider_message_id.as_str()).bind(now.max(existing.admitted_at.unwrap_or(now))).bind(existing.id.as_bytes().as_slice()).execute(&mut **transaction).await?;
        if let Some(campaign) = existing.campaign {
            reconcile_campaign_acceptance(
                transaction,
                campaign,
                stored_time(now).map_err(load_error)?,
            )
            .await
            .map_err(campaign_error)?;
        }
    }
    changed |= suppress_feedback_enrollment(transaction, &existing, command.kind, now).await?;
    Ok(if changed {
        ControlOutcome::Changed
    } else {
        ControlOutcome::Unchanged
    })
}

async fn suppress_feedback_enrollment(
    transaction: &mut Transaction<'_, Sqlite>,
    existing: &Attempt,
    kind: FeedbackKind,
    now: i64,
) -> Result<bool, SubscriberApplyError> {
    let reason = match kind {
        FeedbackKind::HardBounce => Some("hard_bounce"),
        FeedbackKind::Complaint => Some("complaint"),
        FeedbackKind::Accepted | FeedbackKind::Delivered | FeedbackKind::DeliveryFailed => None,
    };
    if let Some(reason) = reason
        && let Some(enrollment) = load_enrollment(transaction, existing.enrollment).await?
        && enrollment.generation == existing.generation
        && !matches!(enrollment.state, EnrollmentState::Removed)
        && let Some(digest) = enrollment.digest
    {
        let suppression_count: i64 = sqlx::query_scalar("SELECT count(*) FROM mail_suppressions")
            .fetch_one(&mut **transaction)
            .await?;
        if suppression_count >= MAX_ATTEMPTS && !suppressed(transaction, &digest, now).await? {
            return Err(SubscriberCommandError::InvalidValue.into());
        }
        // Only this still-current enrollment is changed. Late feedback for a
        // removed generation cannot erase a new voluntary enrollment.
        sqlx::query("INSERT INTO mail_suppressions VALUES (?,?,?,?) ON CONFLICT(mailbox_digest) DO UPDATE SET reason=CASE WHEN reason='complaint' THEN reason ELSE excluded.reason END,created_at=excluded.created_at,expires_at=max(expires_at,excluded.expires_at)")
            .bind(digest.as_bytes().as_slice()).bind(reason).bind(now).bind(now+SUPPRESSION_RETENTION_SECONDS).execute(&mut **transaction).await?;
        remove_enrollment(transaction, enrollment.id, now).await?;
        return Ok(true);
    }
    Ok(false)
}

pub(crate) async fn quarantine_interrupted(
    transaction: &mut Transaction<'_, Sqlite>,
    now: i64,
) -> Result<u64, SubscriberApplyError> {
    Ok(sqlx::query("UPDATE mail_attempts SET outcome='unknown',finished_at=max(admitted_at,?) WHERE outcome='admitted'").bind(now).execute(&mut **transaction).await?.rows_affected())
}

pub(crate) async fn campaign_counts(
    transaction: &mut Transaction<'_, Sqlite>,
    campaign: CampaignId,
) -> Result<CampaignCounts, CampaignApplyError> {
    let (accepted,rejected,unknown):(i64,i64,i64)=sqlx::query_as("SELECT COALESCE(SUM(outcome='accepted'),0),COALESCE(SUM(outcome='rejected'),0),COALESCE(SUM(outcome IN ('unknown','admitted')),0) FROM mail_attempts WHERE campaign_id=?")
        .bind(campaign.0.as_bytes().as_slice()).fetch_one(&mut **transaction).await?;
    let counts = CampaignCounts {
        accepted: accepted as u64,
        rejected: rejected as u64,
        unknown: unknown as u64,
    };
    counts
        .validate()
        .map_err(|_| CampaignApplyError::CorruptStoredState)?;
    Ok(counts)
}

pub(crate) async fn discard_restored_subscribers(
    transaction: &mut Transaction<'_, Sqlite>,
    restore_id: Uuid,
) -> Result<(), SubscriberApplyError> {
    let initialized: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mail_control_state)")
        .fetch_one(&mut **transaction)
        .await?;
    if initialized {
        let quarantined:i64=sqlx::query_scalar("SELECT count(*) FROM mail_campaigns WHERE state='quarantined' AND json_extract(record,'$.state.reason.kind')='restore' AND json_extract(record,'$.state.reason.restore_id')=?").bind(restore_id.to_string()).fetch_one(&mut **transaction).await?;
        let result = rotate_consent_epoch(transaction, quarantined as u64).await?;
        let row=sqlx::query("SELECT audit_event_id FROM admin_audit_events WHERE action='instance.restore.accept' AND idempotency_key=?").bind(restore_id.as_bytes().as_slice()).fetch_one(&mut **transaction).await?;
        let audit = Uuid::from_slice(row.try_get("audit_event_id")?)
            .map_err(|_| SubscriberApplyError::CorruptStoredState)?;
        let mut fingerprint = CommandFingerprintBuilder::new("mail.consent.restore");
        fingerprint.uuid(&restore_id);
        insert_reset_receipt(transaction, restore_id, audit, fingerprint.finish(), result).await?;
    }
    sqlx::query("DELETE FROM mail_suppressions")
        .execute(&mut **transaction)
        .await?;
    sqlx::query("DELETE FROM mail_daily_budget")
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

pub(crate) async fn cleanup(
    transaction: &mut Transaction<'_, Sqlite>,
    now: i64,
) -> Result<SubscriberCleanup, SubscriberApplyError> {
    let pending=sqlx::query("SELECT enrollment_id FROM mail_enrollments WHERE state='pending' AND pending_expires_at<=? ORDER BY pending_expires_at LIMIT 100")
        .bind(now).fetch_all(&mut **transaction).await?;
    let expired_enrollments = pending.len() as u64;
    for row in pending {
        remove_enrollment(
            transaction,
            row_uuid(&row, "enrollment_id").map_err(load_error)?,
            now,
        )
        .await?;
    }
    // Retiring recipient history must close campaign admission first; otherwise
    // deleting uniqueness evidence could authorize a second delivery.
    let campaigns=sqlx::query("SELECT DISTINCT attempt.campaign_id FROM mail_attempts AS attempt JOIN mail_campaigns AS campaign ON campaign.campaign_id=attempt.campaign_id WHERE attempt.retire_at<=? AND campaign.state IN ('claimed','cancelling') LIMIT 100")
        .bind(now).fetch_all(&mut **transaction).await?;
    for row in campaigns {
        let id = CampaignId(
            Uuid::from_slice(row.try_get::<&[u8], _>("campaign_id")?)
                .map_err(|_| SubscriberApplyError::CorruptStoredState)?,
        );
        let counts = campaign_counts(transaction, id)
            .await
            .map_err(campaign_error)?;
        quarantine_recipient_history(
            transaction,
            id,
            counts,
            OffsetDateTime::from_unix_timestamp(now)
                .map_err(|_| SubscriberCommandError::InvalidValue)?,
        )
        .await
        .map_err(campaign_error)?;
    }
    let removed_attempts=sqlx::query("DELETE FROM mail_attempts WHERE attempt_id IN (SELECT attempt_id FROM mail_attempts WHERE retire_at<=? ORDER BY retire_at LIMIT 100)").bind(now).execute(&mut **transaction).await?.rows_affected();
    let removed_enrollments=sqlx::query("DELETE FROM mail_enrollments WHERE enrollment_id IN (SELECT enrollment_id FROM mail_enrollments AS enrollment WHERE state='removed' AND NOT EXISTS (SELECT 1 FROM mail_attempts WHERE enrollment_id=enrollment.enrollment_id) LIMIT 100)").execute(&mut **transaction).await?.rows_affected();
    let removed_suppressions=sqlx::query("DELETE FROM mail_suppressions WHERE mailbox_digest IN (SELECT mailbox_digest FROM mail_suppressions WHERE expires_at<=? ORDER BY expires_at LIMIT 100)").bind(now).execute(&mut **transaction).await?.rows_affected();
    sqlx::query("DELETE FROM mail_daily_budget WHERE day<?")
        .bind(now.div_euclid(86_400) - 14)
        .execute(&mut **transaction)
        .await?;
    Ok(SubscriberCleanup {
        expired_enrollments,
        removed_attempts,
        removed_enrollments,
        removed_suppressions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{
            DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
            DatabaseWriterQueueCapacity,
        },
        database,
        domain::auth::store::invalidate_restored_credentials,
    };
    use sqlx::{
        Connection as _, SqliteConnection,
        sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    };

    const NOW: i64 = 1_700_000_000;
    const CONFIG: [u8; 32] = [7; 32];

    struct Fixture {
        root: tempfile::TempDir,
        connection: SqliteConnection,
        store: SubscriberStore,
    }

    impl Fixture {
        async fn new() -> Self {
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
            sqlx::query("INSERT INTO instance_identity VALUES(1,?,1,0)")
                .bind(Uuid::new_v4().as_bytes().as_slice())
                .execute(&mut connection)
                .await
                .unwrap();
            let readers = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(options.read_only(true).pragma("query_only", "ON"))
                .await
                .unwrap();
            let (sender, receiver) = mpsc::channel(1);
            drop(receiver);
            let store = SubscriberStore::new(readers, sender);
            let mut transaction = connection.begin().await.unwrap();
            initialize_controls(&mut transaction, [9; 32])
                .await
                .unwrap();
            set_policy(&mut transaction, configured(100), NOW)
                .await
                .unwrap();
            observe(&mut transaction, NOW).await.unwrap();
            transaction.commit().await.unwrap();
            Self {
                root,
                connection,
                store,
            }
        }

        async fn close(self) {
            self.store.readers.close().await;
            self.connection.close().await.unwrap();
            self.root.close().unwrap();
        }
    }

    async fn observe(
        transaction: &mut Transaction<'_, Sqlite>,
        now: i64,
    ) -> Result<(), SubscriberApplyError> {
        let prior = feedback_continuity(transaction, CONFIG).await?;
        let run_id = match prior.run {
            Some(run) => run,
            None => {
                begin_feedback_run(
                    transaction,
                    BeginFeedbackRun {
                        provider_now: stored_time(now).unwrap(),
                        configuration_binding: CONFIG,
                        source_binding: [6; 32],
                        retention_seconds: 1_209_600,
                    },
                    now,
                )
                .await?
                .run_id
            }
        };
        record_feedback_observation(
            transaction,
            RecordFeedbackObservation {
                configuration_binding: CONFIG,
                run_id,
                source_binding: [6; 32],
                retention_seconds: 1_209_600,
                observation: FeedbackObservation::Observed {
                    provider_now: stored_time(now).unwrap(),
                    drained: true,
                },
            },
            now,
        )
        .await
    }

    fn configured(limit: u64) -> SubscriberPolicy {
        SubscriberPolicy {
            configuration_binding: CONFIG,
            mode: SubscriberMode::Enabled,
            max_daily_messages: limit,
            max_daily_confirmations: limit,
            max_campaign_recipients: 100,
        }
    }

    struct EnrollmentIds {
        mail_epoch: Uuid,
        enrollment: Uuid,
        generation: Uuid,
        attempt: Uuid,
        expires: i64,
    }

    fn request(address: &str, digest: u8) -> RequestEnrollment {
        RequestEnrollment {
            address: EmailAddress::parse(address).unwrap(),
            mailbox_digest: SubscriberDigest::from_bytes([digest; 32]),
            enrollment: Uuid::new_v4(),
            generation: Uuid::new_v4(),
            confirmation_attempt: Uuid::new_v4(),
            configuration_binding: CONFIG,
        }
    }

    async fn enroll(
        transaction: &mut Transaction<'_, Sqlite>,
        address: &str,
        digest: u8,
        now: i64,
    ) -> EnrollmentIds {
        let command = request(address, digest);
        let ids = EnrollmentIds {
            mail_epoch: current_delivery_identity(transaction).await.unwrap().0,
            enrollment: command.enrollment,
            generation: command.generation,
            attempt: command.confirmation_attempt,
            expires: now + PENDING_SECONDS,
        };
        assert_eq!(
            request_enrollment(transaction, command, now).await.unwrap(),
            EnrollmentRequestResult::Queued
        );
        ids
    }

    fn claim(ids: &EnrollmentIds, nonce: u8) -> ClaimConfirmation {
        ClaimConfirmation {
            attempt_id: ids.attempt,
            nonce_digest: SubscriberDigest::from_bytes([nonce; 32]),
            expires_at: stored_time(ids.expires).unwrap(),
            configuration_binding: CONFIG,
        }
    }

    fn confirmation(ids: &EnrollmentIds, nonce: u8) -> ConfirmEnrollment {
        ConfirmEnrollment {
            enrollment: ids.enrollment,
            generation: ids.generation,
            nonce_digest: SubscriberDigest::from_bytes([nonce; 32]),
            expires_at: stored_time(ids.expires).unwrap(),
        }
    }

    fn manage(ids: &EnrollmentIds) -> ManageEnrollment {
        ManageEnrollment {
            enrollment: ids.enrollment,
            generation: ids.generation,
        }
    }

    fn ready(admission: DeliveryAdmission) -> DeliveryAttempt {
        match admission {
            DeliveryAdmission::Ready(permit) => permit.into_attempt(),
            _ => panic!("fresh delivery admission was not granted"),
        }
    }

    fn feedback(ids: &EnrollmentIds, digest: u8, kind: FeedbackKind) -> ApplyFeedback {
        ApplyFeedback {
            mail_epoch: ids.mail_epoch,
            source_binding: [6; 32],
            attempt_id: ids.attempt,
            campaign_id: None,
            mailbox_digest: SubscriberDigest::from_bytes([digest; 32]),
            configuration_binding: CONFIG,
            provider_message_id: MessageId::parse("010001-fixture-provider").unwrap(),
            kind,
            sent_at: stored_time(NOW + 1).unwrap(),
            occurred_at: stored_time(NOW + 2).unwrap(),
        }
    }

    #[tokio::test]
    async fn confirmation_and_removal_bind_the_current_generation_and_preserve_delivery_spelling() {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        let original = enroll(&mut transaction, "Case@EXAMPLE.COM", 1, NOW).await;
        assert_eq!(
            request_enrollment(&mut transaction, request("case@example.com", 1), NOW)
                .await
                .unwrap(),
            EnrollmentRequestResult::Unchanged
        );
        let admitted = ready(
            claim_confirmation(&mut transaction, claim(&original, 2), NOW + 1)
                .await
                .unwrap(),
        );
        assert_eq!(admitted.address.as_str(), "Case@example.com");
        assert!(matches!(
            claim_confirmation(&mut transaction, claim(&original, 2), NOW + 2)
                .await
                .unwrap(),
            DeliveryAdmission::AlreadyRecorded(AttemptOutcome::Admitted)
        ));
        assert_eq!(
            confirm(&mut transaction, confirmation(&original, 3), NOW + 2)
                .await
                .unwrap(),
            ControlOutcome::Unchanged
        );
        assert_eq!(
            confirm(&mut transaction, confirmation(&original, 2), NOW + 2)
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        assert_eq!(
            confirm(&mut transaction, confirmation(&original, 2), NOW + 3)
                .await
                .unwrap(),
            ControlOutcome::Unchanged
        );
        pause(&mut transaction).await.unwrap();
        assert_eq!(
            remove(&mut transaction, manage(&original), NOW + 4)
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        let cleared = load_enrollment(&mut transaction, original.enrollment)
            .await
            .unwrap()
            .unwrap();
        assert!(cleared.address.is_none() && cleared.digest.is_none() && cleared.nonce.is_none());
        set_policy(&mut transaction, configured(100), NOW)
            .await
            .unwrap();
        observe(&mut transaction, NOW + 5).await.unwrap();
        let replacement = enroll(&mut transaction, "case@example.com", 1, NOW + 5).await;
        assert_eq!(
            remove(&mut transaction, manage(&original), NOW + 6)
                .await
                .unwrap(),
            ControlOutcome::Unchanged
        );
        assert_eq!(
            confirm(&mut transaction, confirmation(&original, 2), NOW + 6)
                .await
                .unwrap(),
            ControlOutcome::Unchanged
        );
        let replacement_attempt = ready(
            claim_confirmation(&mut transaction, claim(&replacement, 4), NOW + 6)
                .await
                .unwrap(),
        );
        drop(replacement_attempt);
        assert_eq!(
            confirm(&mut transaction, confirmation(&replacement, 4), NOW + 7)
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        assert_eq!(audience_cutoff(&mut transaction).await.unwrap(), 2);
        transaction.commit().await.unwrap();
        fixture.store.validate_all().await.unwrap();
        assert_eq!(fixture.store.status().await.unwrap().active_enrollments, 1);
        fixture.close().await;
    }

    #[tokio::test]
    async fn feedback_reconciles_unknown_once_and_suppression_never_reactivates_old_consent() {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        let ids = enroll(&mut transaction, "member@example.com", 1, NOW).await;
        let permit = ready(
            claim_confirmation(&mut transaction, claim(&ids, 2), NOW + 1)
                .await
                .unwrap(),
        );
        assert_eq!(
            finish_attempt(
                &mut transaction,
                FinishAttempt {
                    mail_epoch: permit.mail_epoch,
                    attempt_id: permit.attempt_id,
                    attempt_fence: permit.attempt_fence,
                    outcome: SubmissionOutcome::Unknown
                },
                NOW + 2
            )
            .await
            .unwrap(),
            AttemptOutcome::Unknown
        );
        assert!(matches!(
            apply_feedback(
                &mut transaction,
                feedback(&ids, 8, FeedbackKind::Accepted),
                NOW + 3
            )
            .await,
            Err(SubscriberApplyError::Command(
                SubscriberCommandError::AttemptConflict
            ))
        ));
        assert_eq!(
            apply_feedback(
                &mut transaction,
                feedback(&ids, 1, FeedbackKind::Accepted),
                NOW + 3
            )
            .await
            .unwrap(),
            ControlOutcome::Changed
        );
        assert_eq!(
            apply_feedback(
                &mut transaction,
                feedback(&ids, 1, FeedbackKind::Delivered),
                NOW + 4
            )
            .await
            .unwrap(),
            ControlOutcome::Unchanged
        );
        assert_eq!(
            apply_feedback(
                &mut transaction,
                feedback(&ids, 1, FeedbackKind::Complaint),
                NOW + 5
            )
            .await
            .unwrap(),
            ControlOutcome::Changed
        );
        assert_eq!(
            request_enrollment(&mut transaction, request("member@example.com", 1), NOW + 6)
                .await
                .unwrap(),
            EnrollmentRequestResult::Unchanged
        );
        let later = NOW + SUPPRESSION_RETENTION_SECONDS + 6;
        observe(&mut transaction, NOW + 10 * 86_400).await.unwrap();
        observe(&mut transaction, NOW + 20 * 86_400).await.unwrap();
        observe(&mut transaction, later).await.unwrap();
        let fresh = enroll(&mut transaction, "member@example.com", 1, later).await;
        assert_eq!(
            apply_feedback(
                &mut transaction,
                feedback(&ids, 1, FeedbackKind::HardBounce),
                later
            )
            .await
            .unwrap(),
            ControlOutcome::Unchanged
        );
        assert!(matches!(
            load_enrollment(&mut transaction, fresh.enrollment)
                .await
                .unwrap()
                .unwrap()
                .state,
            EnrollmentState::Pending
        ));
        assert_eq!(
            confirm(&mut transaction, confirmation(&ids, 2), later)
                .await
                .unwrap(),
            ControlOutcome::Unchanged
        );
        transaction.commit().await.unwrap();
        fixture.store.validate_all().await.unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn readiness_and_daily_budgets_are_writer_authoritative_without_membership_disclosure() {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        set_policy(&mut transaction, configured(1), NOW)
            .await
            .unwrap();
        assert!(matches!(
            request_enrollment(&mut transaction, request("a@example.com", 1), NOW).await,
            Err(SubscriberApplyError::Command(
                SubscriberCommandError::Paused
            ))
        ));
        observe(&mut transaction, NOW).await.unwrap();
        let pending = enroll(&mut transaction, "a@example.com", 1, NOW).await;
        assert_eq!(
            request_enrollment(&mut transaction, request("a@example.com", 1), NOW + 1)
                .await
                .unwrap(),
            EnrollmentRequestResult::Unchanged
        );
        assert_eq!(
            request_enrollment(&mut transaction, request("b@example.com", 2), NOW + 1)
                .await
                .unwrap(),
            EnrollmentRequestResult::Unchanged
        );
        let tomorrow = NOW + 86_400;
        observe(&mut transaction, tomorrow).await.unwrap();
        assert!(matches!(
            claim_confirmation(&mut transaction, claim(&pending, 2), tomorrow)
                .await
                .unwrap(),
            DeliveryAdmission::Unavailable
        ));
        record_feedback_health(
            &mut transaction,
            CONFIG,
            FeedbackHealth::ReconciliationRequired,
            tomorrow,
        )
        .await
        .unwrap();
        set_policy(&mut transaction, configured(100), NOW)
            .await
            .unwrap();
        observe(&mut transaction, tomorrow).await.unwrap();
        assert!(matches!(
            request_enrollment(&mut transaction, request("c@example.com", 3), tomorrow).await,
            Err(SubscriberApplyError::Command(
                SubscriberCommandError::Paused
            ))
        ));
        assert_eq!(
            remove(&mut transaction, manage(&pending), tomorrow)
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        transaction.commit().await.unwrap();
        let status = fixture.store.status().await.unwrap();
        assert_eq!(
            status.feedback_health,
            FeedbackHealth::ReconciliationRequired
        );
        assert_eq!(status.addressed_enrollments, 0);
        fixture.store.validate_all().await.unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn confirmation_reservations_charge_the_actual_send_day_before_claiming() {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        let midnight = NOW.div_euclid(86_400) * 86_400 + 86_400;
        observe(&mut transaction, midnight - 1).await.unwrap();
        set_policy(&mut transaction, configured(1), NOW)
            .await
            .unwrap();
        observe(&mut transaction, midnight - 1).await.unwrap();
        let yesterday = enroll(&mut transaction, "yesterday@example.com", 1, midnight - 1).await;
        observe(&mut transaction, midnight + 1).await.unwrap();
        let today = enroll(&mut transaction, "today@example.com", 2, midnight + 1).await;
        assert!(matches!(
            claim_confirmation(&mut transaction, claim(&yesterday, 3), midnight + 2)
                .await
                .unwrap(),
            DeliveryAdmission::Deferred
        ));
        let today_permit = ready(
            claim_confirmation(&mut transaction, claim(&today, 4), midnight + 2)
                .await
                .unwrap(),
        );
        drop(today_permit);
        assert!(
            load_enrollment(&mut transaction, yesterday.enrollment)
                .await
                .unwrap()
                .unwrap()
                .nonce
                .is_none()
        );
        transaction.commit().await.unwrap();
        fixture.store.validate_all().await.unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn interrupted_attempts_are_never_reclaimed_and_expired_private_state_is_removed() {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        let pending = enroll(&mut transaction, "pending@example.com", 1, NOW).await;
        let attempted = enroll(&mut transaction, "unknown@example.com", 2, NOW).await;
        let permit = ready(
            claim_confirmation(&mut transaction, claim(&attempted, 3), NOW + 1)
                .await
                .unwrap(),
        );
        assert_eq!(
            quarantine_interrupted(&mut transaction, NOW + 2)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            quarantine_interrupted(&mut transaction, NOW + 3)
                .await
                .unwrap(),
            0
        );
        assert!(matches!(
            claim_confirmation(&mut transaction, claim(&attempted, 3), NOW + 3)
                .await
                .unwrap(),
            DeliveryAdmission::AlreadyRecorded(AttemptOutcome::Unknown)
        ));
        assert_eq!(
            finish_attempt(
                &mut transaction,
                FinishAttempt {
                    mail_epoch: permit.mail_epoch,
                    attempt_id: permit.attempt_id,
                    attempt_fence: permit.attempt_fence,
                    outcome: SubmissionOutcome::Rejected
                },
                NOW + 4
            )
            .await
            .unwrap(),
            AttemptOutcome::Unknown
        );
        cleanup(&mut transaction, NOW + PENDING_SECONDS)
            .await
            .unwrap();
        for ids in [&pending, &attempted] {
            let record = load_enrollment(&mut transaction, ids.enrollment)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(record.state, EnrollmentState::Removed));
            assert!(record.address.is_none() && record.digest.is_none());
        }
        let removed = cleanup(&mut transaction, NOW + ATTEMPT_RETENTION_SECONDS + 1)
            .await
            .unwrap();
        assert_eq!(removed.removed_attempts, 2);
        assert_eq!(removed.removed_enrollments, 2);
        transaction.commit().await.unwrap();
        fixture.store.validate_all().await.unwrap();
        assert_eq!(
            fixture.store.status().await.unwrap().retained_enrollments,
            0
        );
        fixture.close().await;
    }

    #[tokio::test]
    async fn restore_discards_all_subscriber_eligibility_and_rotates_the_delivery_epoch() {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        let ids = enroll(&mut transaction, "member@example.com", 1, NOW).await;
        let permit = ready(
            claim_confirmation(&mut transaction, claim(&ids, 2), NOW + 1)
                .await
                .unwrap(),
        );
        drop(permit);
        confirm(&mut transaction, confirmation(&ids, 2), NOW + 2)
            .await
            .unwrap();
        assert!(matches!(
            initialize_controls(&mut transaction, [8; 32]).await,
            Err(SubscriberApplyError::Command(
                SubscriberCommandError::ControlIdentityChanged
            ))
        ));
        record_feedback_health(
            &mut transaction,
            CONFIG,
            FeedbackHealth::ReconciliationRequired,
            NOW + 3,
        )
        .await
        .unwrap();
        let restore_id = Uuid::new_v4();
        invalidate_restored_credentials(
            &mut transaction,
            restore_id,
            stored_time(NOW + 3).unwrap(),
        )
        .await
        .unwrap();
        discard_restored_subscribers(&mut transaction, restore_id)
            .await
            .unwrap();
        assert_eq!(audience_cutoff(&mut transaction).await.unwrap(), 1);
        assert_eq!(
            confirm(&mut transaction, confirmation(&ids, 2), NOW + 4)
                .await
                .unwrap(),
            ControlOutcome::Unchanged
        );
        assert_eq!(
            remove(&mut transaction, manage(&ids), NOW + 4)
                .await
                .unwrap(),
            ControlOutcome::Unchanged
        );
        let identity: Vec<u8> =
            sqlx::query_scalar("SELECT control_binding FROM mail_control_state")
                .fetch_one(&mut *transaction)
                .await
                .unwrap();
        assert_eq!(identity, [9; 32]);
        transaction.commit().await.unwrap();
        let status = fixture.store.status().await.unwrap();
        assert_eq!(status.retained_enrollments, 0);
        assert_eq!(status.policy.unwrap().mode, SubscriberMode::Paused);
        assert_eq!(status.feedback_health, FeedbackHealth::Unavailable);
        fixture.store.validate_all().await.unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn bounded_restore_inspection_rejects_corrupt_private_rows_without_returning_them() {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        let ids = enroll(&mut transaction, "member@example.com", 1, NOW).await;
        transaction.commit().await.unwrap();
        fixture.store.validate_all().await.unwrap();
        assert!(matches!(
            fixture.store.queued_confirmations(CONFIG, 101).await,
            Err(SubscriberLoadError::InvalidLimit)
        ));
        sqlx::query("PRAGMA ignore_check_constraints=ON")
            .execute(&mut fixture.connection)
            .await
            .unwrap();
        sqlx::query("UPDATE mail_enrollments SET address=zeroblob(1048576) WHERE enrollment_id=?")
            .bind(ids.enrollment.as_bytes().as_slice())
            .execute(&mut fixture.connection)
            .await
            .unwrap_err();
        sqlx::query("UPDATE mail_enrollments SET address=CAST(zeroblob(1048576) AS TEXT) WHERE enrollment_id=?").bind(ids.enrollment.as_bytes().as_slice()).execute(&mut fixture.connection).await.unwrap();
        assert!(fixture.store.validate_all().await.is_err());
        fixture.close().await;
    }
    #[tokio::test]
    async fn policy_change_retires_unsent_confirmations_but_preserves_issued_removal_and_confirmation_controls()
     {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        let queued = enroll(&mut transaction, "queued@example.com", 1, NOW).await;
        let issued = enroll(&mut transaction, "issued@example.com", 2, NOW).await;
        let permit = ready(
            claim_confirmation(&mut transaction, claim(&issued, 3), NOW + 1)
                .await
                .unwrap(),
        );
        drop(permit);
        let mut next = configured(100);
        next.configuration_binding = [8; 32];
        set_policy(&mut transaction, next, NOW + 2).await.unwrap();
        let retired = load_enrollment(&mut transaction, queued.enrollment)
            .await
            .unwrap()
            .unwrap();
        assert!(retired.address.is_none() && matches!(retired.state, EnrollmentState::Removed));
        assert_eq!(
            confirm(&mut transaction, confirmation(&issued, 3), NOW + 3)
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        assert_eq!(
            remove(&mut transaction, manage(&issued), NOW + 4)
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        transaction.commit().await.unwrap();
        assert!(
            fixture
                .store
                .queued_confirmations(CONFIG, 100)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            fixture
                .store
                .queued_confirmations([8; 32], 100)
                .await
                .unwrap()
                .is_empty()
        );
        fixture.store.validate_all().await.unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn interrupted_receive_waits_for_provider_lease_and_a_continuous_quiet_window() {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        enroll(&mut transaction, "member@example.com", 1, NOW).await;
        let old = feedback_continuity(&mut transaction, CONFIG)
            .await
            .unwrap()
            .run
            .unwrap();
        let FeedbackPollAdmission::Ready(intent) =
            begin_feedback_poll(&mut transaction, CONFIG, old, NOW + 1)
                .await
                .unwrap()
        else {
            panic!("the original consumer can receive")
        };
        assert!(matches!(
            begin_feedback_poll(&mut transaction, CONFIG, old, NOW + 2).await,
            Err(SubscriberApplyError::Command(
                SubscriberCommandError::AttemptConflict
            ))
        ));
        assert!(
            finish_feedback_run(&mut transaction, CONFIG, old)
                .await
                .is_err()
        );
        let restarted = begin_feedback_run(
            &mut transaction,
            BeginFeedbackRun {
                provider_now: stored_time(NOW + 2).unwrap(),
                configuration_binding: CONFIG,
                source_binding: [6; 32],
                retention_seconds: 1_209_600,
            },
            NOW + 2,
        )
        .await
        .unwrap();
        assert_eq!(restarted.health, FeedbackHealth::Unavailable);
        assert_eq!(restarted.recover_after, Some(intent.recover_after));
        assert!(matches!(
            complete_feedback_poll(&mut transaction, CONFIG, old, intent.poll_id).await,
            Err(SubscriberApplyError::Command(
                SubscriberCommandError::ConfigurationChanged
            ))
        ));
        // Advancing the host clock alone cannot expire an uncertain provider receive.
        assert_eq!(
            begin_feedback_poll(&mut transaction, CONFIG, restarted.run_id, NOW + 10_000)
                .await
                .unwrap(),
            FeedbackPollAdmission::Recovering
        );
        let deadline = intent.recover_after.unix_timestamp();
        let observation = |time, drained| RecordFeedbackObservation {
            configuration_binding: CONFIG,
            run_id: restarted.run_id,
            source_binding: [6; 32],
            retention_seconds: 1_209_600,
            observation: FeedbackObservation::Observed {
                provider_now: stored_time(time).unwrap(),
                drained,
            },
        };
        record_feedback_observation(&mut transaction, observation(deadline, true), deadline)
            .await
            .unwrap();
        assert_eq!(
            feedback_continuity(&mut transaction, CONFIG)
                .await
                .unwrap()
                .quiet_since,
            Some(deadline)
        );
        // A delivered message breaks the quiet window; a later empty poll starts a new one.
        record_feedback_observation(
            &mut transaction,
            observation(deadline + 100, false),
            deadline + 100,
        )
        .await
        .unwrap();
        assert_eq!(
            feedback_continuity(&mut transaction, CONFIG)
                .await
                .unwrap()
                .quiet_since,
            None
        );
        record_feedback_observation(
            &mut transaction,
            observation(deadline + 120, true),
            deadline + 120,
        )
        .await
        .unwrap();
        record_feedback_observation(
            &mut transaction,
            observation(deadline + 299, true),
            deadline + 299,
        )
        .await
        .unwrap();
        assert!(matches!(
            request_enrollment(
                &mut transaction,
                request("blocked@example.com", 2),
                deadline + 299
            )
            .await,
            Err(SubscriberApplyError::Command(
                SubscriberCommandError::Paused
            ))
        ));
        record_feedback_observation(
            &mut transaction,
            observation(deadline + 300, true),
            deadline + 300,
        )
        .await
        .unwrap();
        let recovered = feedback_continuity(&mut transaction, CONFIG).await.unwrap();
        assert_eq!(recovered.recover_after, None);
        assert!(!recovered.gap);
        assert_eq!(
            request_enrollment(
                &mut transaction,
                request("new@example.com", 2),
                deadline + 300
            )
            .await
            .unwrap(),
            EnrollmentRequestResult::Queued
        );
        transaction.commit().await.unwrap();
        fixture.store.validate_all().await.unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn feedback_retention_growth_cannot_hide_a_gap_during_an_unavailable_run() {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        let run_id = feedback_continuity(&mut transaction, CONFIG)
            .await
            .unwrap()
            .run
            .unwrap();
        record_feedback_observation(
            &mut transaction,
            RecordFeedbackObservation {
                configuration_binding: CONFIG,
                run_id,
                source_binding: [6; 32],
                retention_seconds: 60,
                observation: FeedbackObservation::Observed {
                    provider_now: stored_time(NOW).unwrap(),
                    drained: true,
                },
            },
            NOW,
        )
        .await
        .unwrap();
        let ids = enroll(&mut transaction, "member@example.com", 1, NOW).await;
        record_feedback_observation(
            &mut transaction,
            RecordFeedbackObservation {
                configuration_binding: CONFIG,
                run_id,
                source_binding: [6; 32],
                retention_seconds: 3600,
                observation: FeedbackObservation::Unavailable,
            },
            NOW + 30,
        )
        .await
        .unwrap();
        let stale = feedback_continuity(&mut transaction, CONFIG).await.unwrap();
        assert_eq!(stale.observed, Some(NOW));
        assert_eq!(stale.retention, Some(60));
        record_feedback_observation(
            &mut transaction,
            RecordFeedbackObservation {
                configuration_binding: CONFIG,
                run_id,
                source_binding: [6; 32],
                retention_seconds: 3600,
                observation: FeedbackObservation::Observed {
                    provider_now: stored_time(NOW + 61).unwrap(),
                    drained: true,
                },
            },
            NOW + 61,
        )
        .await
        .unwrap();
        assert!(
            feedback_continuity(&mut transaction, CONFIG)
                .await
                .unwrap()
                .gap
        );
        assert_eq!(
            remove(&mut transaction, manage(&ids), NOW + 62)
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        transaction.commit().await.unwrap();
        fixture.store.validate_all().await.unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn clean_feedback_restart_still_rejects_expired_watermarks_and_changed_sources_with_consent()
     {
        for changed_source in [false, true] {
            let mut fixture = Fixture::new().await;
            let mut transaction = fixture.connection.begin().await.unwrap();
            let ids = enroll(&mut transaction, "member@example.com", 1, NOW).await;
            let old = feedback_continuity(&mut transaction, CONFIG)
                .await
                .unwrap()
                .run
                .unwrap();
            finish_feedback_run(&mut transaction, CONFIG, old)
                .await
                .unwrap();
            let source_binding = if changed_source { [5; 32] } else { [6; 32] };
            let now = if changed_source {
                NOW + 1
            } else {
                NOW + 1_209_600
            };
            let run = begin_feedback_run(
                &mut transaction,
                BeginFeedbackRun {
                    provider_now: stored_time(now).unwrap(),
                    configuration_binding: CONFIG,
                    source_binding,
                    retention_seconds: 1_209_600,
                },
                now,
            )
            .await
            .unwrap();
            assert_eq!(run.health, FeedbackHealth::ReconciliationRequired);
            assert!(
                load_enrollment(&mut transaction, ids.enrollment)
                    .await
                    .unwrap()
                    .unwrap()
                    .address
                    .is_some()
            );
            transaction.commit().await.unwrap();
            fixture.store.validate_all().await.unwrap();
            fixture.close().await;
        }
    }

    #[tokio::test]
    async fn feedback_after_attempt_expiration_requires_reconciliation_instead_of_acknowledging_missing_correlation()
     {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        let ids = enroll(&mut transaction, "member@example.com", 1, NOW).await;
        let permit = ready(
            claim_confirmation(&mut transaction, claim(&ids, 2), NOW + 1)
                .await
                .unwrap(),
        );
        confirm(&mut transaction, confirmation(&ids, 2), NOW + 2)
            .await
            .unwrap();
        finish_attempt(
            &mut transaction,
            FinishAttempt {
                mail_epoch: permit.mail_epoch,
                attempt_id: permit.attempt_id,
                attempt_fence: permit.attempt_fence,
                outcome: SubmissionOutcome::Accepted(
                    MessageId::parse("010001-fixture-provider").unwrap(),
                ),
            },
            NOW + 2,
        )
        .await
        .unwrap();
        cleanup(&mut transaction, NOW + ATTEMPT_RETENTION_SECONDS + 1)
            .await
            .unwrap();
        assert!(matches!(
            apply_feedback(
                &mut transaction,
                feedback(&ids, 1, FeedbackKind::Complaint),
                NOW + ATTEMPT_RETENTION_SECONDS + 2
            )
            .await,
            Err(SubscriberApplyError::Command(
                SubscriberCommandError::AttemptConflict
            ))
        ));
        assert!(matches!(
            load_enrollment(&mut transaction, ids.enrollment)
                .await
                .unwrap()
                .unwrap()
                .state,
            EnrollmentState::Active
        ));
        transaction.commit().await.unwrap();
        fixture.store.validate_all().await.unwrap();
        fixture.close().await;
    }
    #[tokio::test]
    async fn invalid_source_contract_latches_only_when_the_writer_has_retained_eligibility() {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        assert_eq!(
            record_feedback_integrity_failure(&mut transaction, CONFIG)
                .await
                .unwrap(),
            FeedbackHealth::Unavailable
        );
        observe(&mut transaction, NOW).await.unwrap();
        let ids = enroll(&mut transaction, "member@example.com", 1, NOW).await;
        assert_eq!(
            record_feedback_integrity_failure(&mut transaction, CONFIG)
                .await
                .unwrap(),
            FeedbackHealth::ReconciliationRequired
        );
        remove(&mut transaction, manage(&ids), NOW + 1)
            .await
            .unwrap();
        assert_eq!(
            record_feedback_integrity_failure(&mut transaction, CONFIG)
                .await
                .unwrap(),
            FeedbackHealth::ReconciliationRequired
        );
        assert!(matches!(
            record_feedback_integrity_failure(&mut transaction, [0; 32]).await,
            Err(SubscriberApplyError::Command(
                SubscriberCommandError::ConfigurationChanged
            ))
        ));
        transaction.commit().await.unwrap();
        assert_eq!(
            fixture.store.status().await.unwrap().feedback_health,
            FeedbackHealth::ReconciliationRequired
        );
        fixture.close().await;
    }
    #[tokio::test]
    async fn confirmation_expires_at_its_exact_deadline_and_removal_remains_available_while_paused()
    {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        let mut timely = enroll(&mut transaction, "timely@example.com", 1, NOW).await;
        let mut expired = enroll(&mut transaction, "expired@example.com", 2, NOW).await;
        timely.expires = NOW + 10;
        expired.expires = NOW + 10;
        drop(ready(
            claim_confirmation(&mut transaction, claim(&timely, 3), NOW + 1)
                .await
                .unwrap(),
        ));
        drop(ready(
            claim_confirmation(&mut transaction, claim(&expired, 4), NOW + 1)
                .await
                .unwrap(),
        ));
        let mut altered = confirmation(&timely, 3);
        altered.expires_at = stored_time(NOW + 11).unwrap();
        assert_eq!(
            confirm(&mut transaction, altered, NOW + 2).await.unwrap(),
            ControlOutcome::Unchanged
        );
        let mut wrong_generation = confirmation(&timely, 3);
        wrong_generation.generation = Uuid::new_v4();
        assert_eq!(
            confirm(&mut transaction, wrong_generation, NOW + 2)
                .await
                .unwrap(),
            ControlOutcome::Unchanged
        );
        assert_eq!(
            confirm(&mut transaction, confirmation(&timely, 3), NOW + 9)
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        assert_eq!(
            confirm(&mut transaction, confirmation(&expired, 4), NOW + 10)
                .await
                .unwrap(),
            ControlOutcome::Unchanged
        );
        assert_eq!(audience_cutoff(&mut transaction).await.unwrap(), 1);
        pause(&mut transaction).await.unwrap();
        assert_eq!(
            remove(&mut transaction, manage(&expired), NOW + 11)
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        assert_eq!(
            remove(&mut transaction, manage(&timely), NOW + 11)
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        transaction.commit().await.unwrap();
        assert_eq!(
            fixture.store.status().await.unwrap().addressed_enrollments,
            0
        );
        fixture.store.validate_all().await.unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn regressed_provider_time_pauses_feedback_without_erasing_consent_or_latching_a_gap() {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        enroll(&mut transaction, "member@example.com", 1, NOW).await;
        let run = begin_feedback_run(
            &mut transaction,
            BeginFeedbackRun {
                provider_now: stored_time(NOW - 1).unwrap(),
                configuration_binding: CONFIG,
                source_binding: [6; 32],
                retention_seconds: 1_209_600,
            },
            NOW + 1,
        )
        .await
        .unwrap();
        assert_eq!(run.health, FeedbackHealth::Unavailable);
        let observation = |provider| RecordFeedbackObservation {
            configuration_binding: CONFIG,
            run_id: run.run_id,
            source_binding: [6; 32],
            retention_seconds: 1_209_600,
            observation: FeedbackObservation::Observed {
                provider_now: stored_time(provider).unwrap(),
                drained: true,
            },
        };
        record_feedback_observation(&mut transaction, observation(NOW - 1), NOW + 1)
            .await
            .unwrap();
        assert_eq!(
            begin_feedback_poll(&mut transaction, CONFIG, run.run_id, NOW + 1000)
                .await
                .unwrap(),
            FeedbackPollAdmission::Recovering
        );
        let paused = feedback_continuity(&mut transaction, CONFIG).await.unwrap();
        assert!(!paused.gap);
        assert!(paused.clock_regressed);
        assert!(matches!(
            request_enrollment(&mut transaction, request("new@example.com", 2), NOW + 1).await,
            Err(SubscriberApplyError::Command(
                SubscriberCommandError::Paused
            ))
        ));
        record_feedback_observation(&mut transaction, observation(NOW), NOW + 2)
            .await
            .unwrap();
        assert!(
            !feedback_continuity(&mut transaction, CONFIG)
                .await
                .unwrap()
                .clock_regressed
        );
        assert_eq!(
            request_enrollment(&mut transaction, request("new@example.com", 2), NOW + 2)
                .await
                .unwrap(),
            EnrollmentRequestResult::Queued
        );
        transaction.commit().await.unwrap();
        assert_eq!(
            fixture.store.status().await.unwrap().addressed_enrollments,
            2
        );
        fixture.store.validate_all().await.unwrap();
        fixture.close().await;
    }
    #[tokio::test]
    async fn deferred_receive_retains_its_exact_lease_across_a_clean_restart() {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        enroll(&mut transaction, "member@example.com", 1, NOW).await;
        let run = feedback_continuity(&mut transaction, CONFIG)
            .await
            .unwrap()
            .run
            .unwrap();
        let FeedbackPollAdmission::Ready(intent) =
            begin_feedback_poll(&mut transaction, CONFIG, run, NOW + 1)
                .await
                .unwrap()
        else {
            panic!("current consumer can start one receive")
        };
        for (binding, run_id, poll_id) in [
            ([0; 32], run, intent.poll_id),
            (CONFIG, Uuid::new_v4(), intent.poll_id),
            (CONFIG, run, Uuid::new_v4()),
        ] {
            assert!(matches!(
                defer_feedback_poll(&mut transaction, binding, run_id, poll_id).await,
                Err(SubscriberApplyError::Command(
                    SubscriberCommandError::ConfigurationChanged
                ))
            ));
            assert_eq!(
                feedback_continuity(&mut transaction, CONFIG)
                    .await
                    .unwrap()
                    .poll,
                Some(intent)
            );
        }
        defer_feedback_poll(&mut transaction, CONFIG, run, intent.poll_id)
            .await
            .unwrap();
        let deferred = feedback_continuity(&mut transaction, CONFIG).await.unwrap();
        assert_eq!(deferred.poll, None);
        assert_eq!(
            deferred.recover_after,
            Some(intent.recover_after.unix_timestamp())
        );
        assert_eq!(deferred.quiet_since, None);
        assert!(!deferred.gap);
        assert!(matches!(
            defer_feedback_poll(&mut transaction, CONFIG, run, intent.poll_id).await,
            Err(SubscriberApplyError::Command(
                SubscriberCommandError::ConfigurationChanged
            ))
        ));
        finish_feedback_run(&mut transaction, CONFIG, run)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        // A fresh transaction sees the saved uncertain receive after clean shutdown.
        let mut transaction = fixture.connection.begin().await.unwrap();
        let resumed = begin_feedback_run(
            &mut transaction,
            BeginFeedbackRun {
                provider_now: stored_time(NOW + 2).unwrap(),
                configuration_binding: CONFIG,
                source_binding: [6; 32],
                retention_seconds: 1_209_600,
            },
            NOW + 2,
        )
        .await
        .unwrap();
        assert_eq!(resumed.recover_after, Some(intent.recover_after));
        assert_eq!(
            begin_feedback_poll(&mut transaction, CONFIG, resumed.run_id, NOW + 10_000)
                .await
                .unwrap(),
            FeedbackPollAdmission::Recovering
        );
        let deadline = intent.recover_after.unix_timestamp();
        for time in [deadline, deadline + RECOVERY_QUIET_SECONDS] {
            record_feedback_observation(
                &mut transaction,
                RecordFeedbackObservation {
                    configuration_binding: CONFIG,
                    run_id: resumed.run_id,
                    source_binding: [6; 32],
                    retention_seconds: 1_209_600,
                    observation: FeedbackObservation::Observed {
                        provider_now: stored_time(time).unwrap(),
                        drained: true,
                    },
                },
                time,
            )
            .await
            .unwrap();
        }
        let FeedbackPollAdmission::Ready(next) = begin_feedback_poll(
            &mut transaction,
            CONFIG,
            resumed.run_id,
            deadline + RECOVERY_QUIET_SECONDS,
        )
        .await
        .unwrap() else {
            panic!("provider lease and quiet window have both elapsed")
        };
        assert_ne!(next.poll_id, intent.poll_id);
        assert_eq!(
            feedback_continuity(&mut transaction, CONFIG)
                .await
                .unwrap()
                .recover_after,
            None
        );
        complete_feedback_poll(&mut transaction, CONFIG, resumed.run_id, next.poll_id)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        fixture.store.validate_all().await.unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn pending_expiry_reports_work_until_a_bounded_backlog_is_drained() {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        set_policy(&mut transaction, configured(200), NOW)
            .await
            .unwrap();
        observe(&mut transaction, NOW).await.unwrap();
        for number in 0..101_u8 {
            enroll(
                &mut transaction,
                &format!("pending{number}@example.com"),
                number,
                NOW,
            )
            .await;
        }
        let first = cleanup(&mut transaction, NOW + PENDING_SECONDS)
            .await
            .unwrap();
        assert_eq!(first.expired_enrollments, 100);
        assert_eq!(first.removed_attempts, 0);
        assert_eq!(first.removed_enrollments, 0);
        let second = cleanup(&mut transaction, NOW + PENDING_SECONDS)
            .await
            .unwrap();
        assert_eq!(second.expired_enrollments, 1);
        assert_eq!(second.removed_attempts, 0);
        assert_eq!(second.removed_enrollments, 0);
        assert_eq!(
            cleanup(&mut transaction, NOW + PENDING_SECONDS)
                .await
                .unwrap()
                .expired_enrollments,
            0
        );
        transaction.commit().await.unwrap();
        let status = fixture.store.status().await.unwrap();
        assert_eq!(status.addressed_enrollments, 0);
        assert_eq!(status.retained_enrollments, 101);
        fixture.store.validate_all().await.unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn suppression_cleanup_requires_a_new_voluntary_confirmation_after_retirement() {
        let mut fixture = Fixture::new().await;
        let mut transaction = fixture.connection.begin().await.unwrap();
        let ids = enroll(&mut transaction, "member@example.com", 1, NOW).await;
        drop(ready(
            claim_confirmation(&mut transaction, claim(&ids, 2), NOW + 1)
                .await
                .unwrap(),
        ));
        confirm(&mut transaction, confirmation(&ids, 2), NOW + 2)
            .await
            .unwrap();
        apply_feedback(
            &mut transaction,
            feedback(&ids, 1, FeedbackKind::Complaint),
            NOW + 3,
        )
        .await
        .unwrap();
        let expires = NOW + 3 + SUPPRESSION_RETENTION_SECONDS;
        observe(&mut transaction, NOW + 10 * 86_400).await.unwrap();
        observe(&mut transaction, NOW + 20 * 86_400).await.unwrap();
        observe(&mut transaction, expires - 1).await.unwrap();
        let before = cleanup(&mut transaction, expires - 1).await.unwrap();
        assert_eq!(before.removed_suppressions, 0);
        assert_eq!(
            request_enrollment(
                &mut transaction,
                request("member@example.com", 1),
                expires - 1
            )
            .await
            .unwrap(),
            EnrollmentRequestResult::Unchanged
        );
        let retired = cleanup(&mut transaction, expires).await.unwrap();
        assert_eq!(retired.removed_suppressions, 1);
        assert_eq!(
            confirm(&mut transaction, confirmation(&ids, 2), expires)
                .await
                .unwrap(),
            ControlOutcome::Unchanged
        );
        let replacement = enroll(&mut transaction, "member@example.com", 1, expires).await;
        assert_ne!(replacement.enrollment, ids.enrollment);
        assert!(matches!(
            load_enrollment(&mut transaction, replacement.enrollment)
                .await
                .unwrap()
                .unwrap()
                .state,
            EnrollmentState::Pending
        ));
        transaction.commit().await.unwrap();
        assert_eq!(fixture.store.status().await.unwrap().active_enrollments, 0);
        fixture.store.validate_all().await.unwrap();
        fixture.close().await;
    }
}
