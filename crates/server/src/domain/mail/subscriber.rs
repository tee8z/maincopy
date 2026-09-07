//! Consent and delivery identities owned by the shared database writer.

pub(crate) mod store;

use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::domain::auth::store::MutationAuditContext;

use super::{
    campaign::{CampaignFence, CampaignId},
    identity::EmailAddress,
    ses::MessageId,
};

pub(super) const MAX_ENROLLMENTS: i64 = 100_000;
pub(super) const MAX_ATTEMPTS: i64 = 1_000_000;
pub(super) const PENDING_SECONDS: i64 = 86_400;
pub(super) const CONFIRMATION_COOLDOWN_SECONDS: i64 = 3_600;
pub(super) const ATTEMPT_RETENTION_SECONDS: i64 = 14 * 86_400;
pub(super) const SUPPRESSION_RETENTION_SECONDS: i64 = 30 * 86_400;
pub(super) const FEEDBACK_FRESH_SECONDS: i64 = 90;
pub(super) const PAGE_SIZE: usize = 100;

/// Keyed mailbox or confirmation digest. Digests remain linkable private data.
pub(crate) struct SubscriberDigest(Zeroizing<[u8; 32]>);

impl SubscriberDigest {
    pub(super) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub(super) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubscriberMode {
    Paused,
    Enabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FeedbackHealth {
    Healthy,
    Unavailable,
    ReconciliationRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SubscriberPolicy {
    pub configuration_binding: [u8; 32],
    pub mode: SubscriberMode,
    pub max_daily_messages: u64,
    pub max_daily_confirmations: u64,
    pub max_campaign_recipients: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SubscriberStatus {
    pub control_version: u64,
    pub mail_epoch: Option<Uuid>,
    pub policy: Option<SubscriberPolicy>,
    pub feedback_health: FeedbackHealth,
    pub last_feedback_ok_at: Option<OffsetDateTime>,
    pub active_enrollments: u64,
    pub pending_enrollments: u64,
    pub addressed_enrollments: u64,
    pub retained_enrollments: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BeginFeedbackRun {
    pub provider_now: OffsetDateTime,
    pub configuration_binding: [u8; 32],
    pub source_binding: [u8; 32],
    pub retention_seconds: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FeedbackRunAdmission {
    pub recover_after: Option<OffsetDateTime>,
    pub run_id: Uuid,
    pub health: FeedbackHealth,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecordFeedbackObservation {
    pub configuration_binding: [u8; 32],
    pub run_id: Uuid,
    pub source_binding: [u8; 32],
    pub retention_seconds: u32,
    pub observation: FeedbackObservation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FeedbackObservation {
    Checked {
        provider_now: OffsetDateTime,
    },
    Unavailable,
    Observed {
        provider_now: OffsetDateTime,
        drained: bool,
    },
    ReconciliationRequired,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FeedbackPollIntent {
    pub poll_id: Uuid,
    pub signed_at: OffsetDateTime,
    pub recover_after: OffsetDateTime,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FeedbackPollAdmission {
    Ready(FeedbackPollIntent),
    Recovering,
}

#[derive(Clone, Debug)]
pub(crate) struct ResetSubscriberConsent {
    pub expected_version: u64,
    pub configuration_binding: [u8; 32],
    pub audit: MutationAuditContext,
    pub now: OffsetDateTime,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResetSubscriberConsentResult {
    pub version: u64,
    pub retired_epoch: Uuid,
    pub new_epoch: Uuid,
    pub discarded_enrollments: u64,
    pub discarded_attempts: u64,
    pub quarantined_campaigns: u64,
}

pub(crate) struct RequestEnrollment {
    pub(super) address: EmailAddress,
    pub(super) mailbox_digest: SubscriberDigest,
    pub(super) enrollment: Uuid,
    pub(super) generation: Uuid,
    pub(super) confirmation_attempt: Uuid,
    pub(super) configuration_binding: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnrollmentRequestResult {
    Queued,
    Unchanged,
}

pub(crate) struct ConfirmEnrollment {
    pub(super) enrollment: Uuid,
    pub(super) generation: Uuid,
    pub(super) nonce_digest: SubscriberDigest,
    pub(super) expires_at: OffsetDateTime,
}

pub(crate) struct ManageEnrollment {
    pub(super) enrollment: Uuid,
    pub(super) generation: Uuid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControlOutcome {
    Changed,
    Unchanged,
}

pub(crate) struct ConfirmationHandle {
    pub(super) attempt_id: Uuid,
    pub(super) enrollment: Uuid,
    pub(super) generation: Uuid,
    pub(super) pending_expires_at: OffsetDateTime,
}

pub(crate) struct RecipientHandle {
    pub(super) enrollment: Uuid,
    pub(super) generation: Uuid,
}

pub(crate) struct ClaimConfirmation {
    pub(super) attempt_id: Uuid,
    pub(super) nonce_digest: SubscriberDigest,
    pub(super) expires_at: OffsetDateTime,
    pub(super) configuration_binding: [u8; 32],
}

pub(crate) struct AdmitCampaignRecipient {
    pub(super) campaign_id: CampaignId,
    pub(super) campaign_fence: CampaignFence,
    pub(super) enrollment: Uuid,
    pub(super) generation: Uuid,
    pub(super) attempt_id: Uuid,
    pub(super) configuration_binding: [u8; 32],
}

/// A fresh committed attempt, consumed once by the sender. No public constructor,
/// cloning, serialization, or debug output can turn a replay into another send.
pub(crate) struct DeliveryPermit {
    mail_epoch: Uuid,
    attempt_id: Uuid,
    attempt_fence: Uuid,
    enrollment: Uuid,
    generation: Uuid,
    address: EmailAddress,
    campaign_id: Option<CampaignId>,
    admitted_at: OffsetDateTime,
}

impl DeliveryPermit {
    pub(super) fn into_attempt(self) -> DeliveryAttempt {
        DeliveryAttempt {
            mail_epoch: self.mail_epoch,
            attempt_id: self.attempt_id,
            attempt_fence: self.attempt_fence,
            enrollment: self.enrollment,
            generation: self.generation,
            address: self.address,
            campaign_id: self.campaign_id,
            admitted_at: self.admitted_at,
        }
    }
}

pub(crate) struct DeliveryAttempt {
    pub(super) mail_epoch: Uuid,
    pub(super) attempt_id: Uuid,
    pub(super) attempt_fence: Uuid,
    pub(super) enrollment: Uuid,
    pub(super) generation: Uuid,
    pub(super) address: EmailAddress,
    pub(super) campaign_id: Option<CampaignId>,
    pub(super) admitted_at: OffsetDateTime,
}

pub(crate) enum DeliveryAdmission {
    Ready(DeliveryPermit),
    AlreadyRecorded(AttemptOutcome),
    Unavailable,
    Deferred,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttemptOutcome {
    Queued,
    Admitted,
    Accepted,
    Rejected,
    Unknown,
    Cancelled,
}

pub(crate) enum SubmissionOutcome {
    Accepted(MessageId),
    Rejected,
    Unknown,
}

pub(crate) struct FinishAttempt {
    pub(super) mail_epoch: Uuid,
    pub(super) attempt_id: Uuid,
    pub(super) attempt_fence: Uuid,
    pub(super) outcome: SubmissionOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FeedbackKind {
    Accepted,
    Delivered,
    HardBounce,
    Complaint,
    DeliveryFailed,
}

pub(crate) struct ApplyFeedback {
    pub(super) source_binding: [u8; 32],
    pub(super) mail_epoch: Uuid,
    pub(super) attempt_id: Uuid,
    pub(super) campaign_id: Option<CampaignId>,
    pub(super) mailbox_digest: SubscriberDigest,
    pub(super) configuration_binding: [u8; 32],
    pub(super) provider_message_id: MessageId,
    pub(super) kind: FeedbackKind,
    pub(super) sent_at: OffsetDateTime,
    pub(super) occurred_at: OffsetDateTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SubscriberCleanup {
    pub expired_enrollments: u64,
    pub removed_attempts: u64,
    pub removed_enrollments: u64,
    pub removed_suppressions: u64,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SubscriberCommandError {
    #[error("consent recovery requires a fresh Owner browser session")]
    Forbidden,
    #[error("the subscriber control version changed")]
    StaleVersion,
    #[error("subscriber recovery requires a recorded feedback reconciliation gap")]
    ReconciliationNotRequired,
    #[error("the recovery idempotency key belongs to another command or session")]
    IdempotencyConflict,
    #[error("the subscriber recovery history limit was reached")]
    Capacity,
    #[error("mail admission is paused")]
    Paused,
    #[error("mail configuration changed")]
    ConfigurationChanged,
    #[error("mail control identity cannot change while retained state exists")]
    ControlIdentityChanged,
    #[error("mail control identity has not been initialized")]
    ControlsUnavailable,
    #[error("subscriber input exceeds its validated bounds")]
    InvalidValue,
    #[error("the mail attempt conflicts with its existing identity")]
    AttemptConflict,
    #[error("the campaign no longer authorizes recipient admission")]
    CampaignUnavailable,
    #[error("the admitted subscriber operation outcome is unknown")]
    OutcomeUnknown,
}
