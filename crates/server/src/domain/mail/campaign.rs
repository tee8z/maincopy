//! Public campaign content and the closed dispatch lifecycle. No recipient data.

use maincopy_shared::auth::UserId;
use markdown_compiler::{
    PostId, PostRevisionDigest, PostSlug, PublicationBaseUrl, SiteSnapshotDigest,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use super::announcement::{
    ANNOUNCEMENT_TEMPLATE_VERSION, Announcement, MAX_ANNOUNCEMENT_BODY_BYTES,
    announcement_content_digest,
};
use crate::domain::publication::{CanonicalSiteUrl, PublicPagePath};

pub(crate) const MAX_CAMPAIGNS: usize = 10_000;
pub(crate) const MAX_CAMPAIGN_RECORD_BYTES: usize = 256 * 1024;
pub(crate) const MAX_CAMPAIGN_RECIPIENTS: u64 = 1_000_000;
pub(crate) const MAX_CLAIM_SECONDS: u32 = 300;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct CampaignId(pub Uuid);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct CampaignFence(pub Uuid);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "u64", into = "u64")]
pub(crate) struct CampaignVersion(u64);

impl CampaignVersion {
    pub(crate) const INITIAL: Self = Self(1);

    pub(crate) fn next(self) -> Result<Self, CampaignValidationError> {
        Self::try_from(self.0.saturating_add(1))
    }
}

impl TryFrom<u64> for CampaignVersion {
    type Error = CampaignValidationError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if value == 0 || value > i64::MAX as u64 {
            return Err(CampaignValidationError::Version);
        }
        Ok(Self(value))
    }
}

impl From<CampaignVersion> for u64 {
    fn from(value: CampaignVersion) -> Self {
        value.0
    }
}

/// These are the reviewed common bytes, before transient recipient controls.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CampaignContent {
    pub post_id: PostId,
    pub revision: PostRevisionDigest,
    pub snapshot: SiteSnapshotDigest,
    pub site_version: u64,
    pub template_version: u32,
    pub canonical_url: String,
    pub subject: String,
    pub text: String,
    pub html: String,
    pub content_digest: [u8; 32],
}

impl CampaignContent {
    pub(super) fn from_announcement(
        announcement: Announcement,
        site_version: u64,
    ) -> Result<Self, CampaignValidationError> {
        let value = Self {
            post_id: announcement.post_id,
            revision: announcement.revision,
            snapshot: announcement.snapshot,
            site_version,
            template_version: ANNOUNCEMENT_TEMPLATE_VERSION,
            canonical_url: announcement.canonical_url.as_str().to_owned(),
            subject: announcement.subject,
            text: announcement.text,
            html: announcement.html,
            content_digest: announcement.content_digest,
        };
        value.validate()?;
        Ok(value)
    }

    pub(super) fn validate(&self) -> Result<(), CampaignValidationError> {
        CampaignVersion::try_from(self.site_version)?;
        if self.template_version != 1
            || self.subject.is_empty()
            || self.subject.len() > 512
            || self.subject.chars().any(char::is_control)
            || self.text.len() > 32 * 1024
            || self.html.len() > MAX_ANNOUNCEMENT_BODY_BYTES
            || self.canonical_url.len() > 4096
        {
            return Err(CampaignValidationError::Content);
        }
        validate_article_url(&self.canonical_url)?;
        if announcement_content_digest(
            &self.post_id,
            &self.revision,
            &self.canonical_url,
            &self.subject,
            &self.text,
            &self.html,
        ) != self.content_digest
        {
            return Err(CampaignValidationError::Content);
        }
        Ok(())
    }
}

/// Reconstructing the typed public URL rejects credentials, fragments, queries,
/// alternate spellings and paths without duplicating the URL model's rules.
fn validate_article_url(value: &str) -> Result<(), CampaignValidationError> {
    let url = url::Url::parse(value).map_err(|_| CampaignValidationError::Content)?;
    let slug = url
        .path()
        .strip_prefix("/posts/")
        .ok_or(CampaignValidationError::Content)?;
    let slug = PostSlug::parse(slug).map_err(|_| CampaignValidationError::Content)?;
    let base = PublicationBaseUrl::parse(&url.origin().ascii_serialization())
        .map_err(|_| CampaignValidationError::Content)?;
    let canonical = CanonicalSiteUrl::for_path(&base, &PublicPagePath::post(&slug));
    if canonical.as_str() != value {
        return Err(CampaignValidationError::Content);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CampaignCounts {
    pub accepted: u64,
    pub rejected: u64,
    pub unknown: u64,
}

impl CampaignCounts {
    pub(crate) fn validate(self) -> Result<(), CampaignValidationError> {
        let total = self
            .accepted
            .checked_add(self.rejected)
            .and_then(|value| value.checked_add(self.unknown))
            .ok_or(CampaignValidationError::Counts)?;
        if total > MAX_CAMPAIGN_RECIPIENTS {
            return Err(CampaignValidationError::Counts);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CampaignApproval {
    pub owner: UserId,
    pub approved_at: OffsetDateTime,
    pub instance_version: u64,
    pub audience_cutoff: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CampaignLease {
    pub fence: CampaignFence,
    pub claimed_at: OffsetDateTime,
    pub renewed_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CampaignQuarantine {
    FeedbackReset { reset_id: Uuid },
    Restore { restore_id: Uuid },
    Interrupted,
}

/// A lost claim can have submitted recipients without recording its result.
/// Unreconciled is never displayed or interpreted as a known zero count.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    content = "counts",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum CampaignProgress {
    Known(CampaignCounts),
    Unreconciled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CampaignState {
    Draft,
    Queued {
        approval: CampaignApproval,
    },
    Claimed {
        approval: CampaignApproval,
        lease: CampaignLease,
    },
    Cancelling {
        approval: CampaignApproval,
        lease: CampaignLease,
    },
    Completed {
        approval: CampaignApproval,
        counts: CampaignCounts,
    },
    Cancelled {
        approval: Option<CampaignApproval>,
        counts: CampaignCounts,
    },
    Unknown {
        approval: CampaignApproval,
        counts: CampaignCounts,
    },
    Quarantined {
        approval: Option<CampaignApproval>,
        progress: CampaignProgress,
        reason: CampaignQuarantine,
    },
}

impl CampaignState {
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Queued { .. } => "queued",
            Self::Claimed { .. } => "claimed",
            Self::Cancelling { .. } => "cancelling",
            Self::Completed { .. } => "completed",
            Self::Cancelled { .. } => "cancelled",
            Self::Unknown { .. } => "unknown",
            Self::Quarantined { .. } => "quarantined",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Campaign {
    pub campaign_id: CampaignId,
    pub version: CampaignVersion,
    pub content: CampaignContent,
    pub configuration_binding: [u8; 32],
    pub created_by: UserId,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub state: CampaignState,
}

impl Campaign {
    pub(super) fn validate(&self) -> Result<(), CampaignValidationError> {
        self.content.validate()?;
        timestamp(self.created_at)?;
        timestamp(self.updated_at)?;
        if self.updated_at < self.created_at {
            return Err(CampaignValidationError::State);
        }
        match &self.state {
            CampaignState::Draft => {
                if self.version != CampaignVersion::INITIAL {
                    return Err(CampaignValidationError::State);
                }
            }
            CampaignState::Queued { approval } => {
                approval.validate(self.created_at, self.updated_at)?
            }
            CampaignState::Claimed { approval, lease }
            | CampaignState::Cancelling { approval, lease } => {
                approval.validate(self.created_at, self.updated_at)?;
                lease.validate(approval, self.updated_at)?;
            }
            CampaignState::Completed { approval, counts } => {
                approval.validate(self.created_at, self.updated_at)?;
                counts.require_resolved(Some(approval))?;
            }
            CampaignState::Unknown { approval, counts } => {
                approval.validate(self.created_at, self.updated_at)?;
                counts.require_unresolved()?;
            }
            CampaignState::Cancelled { approval, counts } => {
                self.validate_optional_approval(approval.as_ref())?;
                counts.require_resolved(approval.as_ref())?;
            }
            CampaignState::Quarantined {
                approval,
                progress,
                reason,
            } => {
                self.validate_optional_approval(approval.as_ref())?;
                validate_quarantine_progress(approval.as_ref(), *progress, *reason)?;
            }
        }
        Ok(())
    }

    fn validate_optional_approval(
        &self,
        approval: Option<&CampaignApproval>,
    ) -> Result<(), CampaignValidationError> {
        if let Some(approval) = approval {
            approval.validate(self.created_at, self.updated_at)?;
        }
        Ok(())
    }
}

impl CampaignApproval {
    fn validate(
        &self,
        created_at: OffsetDateTime,
        updated_at: OffsetDateTime,
    ) -> Result<(), CampaignValidationError> {
        CampaignVersion::try_from(self.instance_version)?;
        if self.audience_cutoff > i64::MAX as u64 {
            return Err(CampaignValidationError::State);
        }
        timestamp(self.approved_at)?;
        if self.approved_at < created_at || self.approved_at > updated_at {
            return Err(CampaignValidationError::State);
        }
        Ok(())
    }
}

impl CampaignLease {
    fn validate(
        &self,
        approval: &CampaignApproval,
        updated_at: OffsetDateTime,
    ) -> Result<(), CampaignValidationError> {
        timestamp(self.claimed_at)?;
        timestamp(self.expires_at)?;
        timestamp(self.renewed_at)?;
        let duration = self.expires_at - self.renewed_at;
        if self.claimed_at < approval.approved_at
            || self.renewed_at < self.claimed_at
            || self.renewed_at > updated_at
            || duration <= time::Duration::ZERO
            || duration > time::Duration::seconds(i64::from(MAX_CLAIM_SECONDS))
        {
            return Err(CampaignValidationError::Lease);
        }
        Ok(())
    }
}

impl CampaignCounts {
    fn require_resolved(
        self,
        approval: Option<&CampaignApproval>,
    ) -> Result<(), CampaignValidationError> {
        self.validate()?;
        if self.unknown != 0 || (approval.is_none() && self != Self::default()) {
            return Err(CampaignValidationError::State);
        }
        Ok(())
    }

    fn require_unresolved(self) -> Result<(), CampaignValidationError> {
        self.validate()?;
        if self.unknown == 0 {
            return Err(CampaignValidationError::State);
        }
        Ok(())
    }
}

fn validate_quarantine_progress(
    approval: Option<&CampaignApproval>,
    progress: CampaignProgress,
    reason: CampaignQuarantine,
) -> Result<(), CampaignValidationError> {
    match (reason, progress, approval) {
        (
            CampaignQuarantine::Restore { .. }
            | CampaignQuarantine::FeedbackReset { .. }
            | CampaignQuarantine::Interrupted,
            CampaignProgress::Unreconciled,
            Some(_),
        ) => Ok(()),
        (
            CampaignQuarantine::Restore { .. } | CampaignQuarantine::FeedbackReset { .. },
            CampaignProgress::Known(counts),
            approval,
        ) => {
            counts.validate()?;
            if counts == CampaignCounts::default() || approval.is_some() {
                Ok(())
            } else {
                Err(CampaignValidationError::State)
            }
        }
        (CampaignQuarantine::Interrupted, CampaignProgress::Known(counts), Some(_)) => {
            counts.validate()
        }
        (
            CampaignQuarantine::Restore { .. }
            | CampaignQuarantine::FeedbackReset { .. }
            | CampaignQuarantine::Interrupted,
            CampaignProgress::Unreconciled,
            None,
        )
        | (CampaignQuarantine::Interrupted, CampaignProgress::Known(_), None) => {
            Err(CampaignValidationError::State)
        }
    }
}

pub(super) fn timestamp(value: OffsetDateTime) -> Result<i64, CampaignValidationError> {
    i64::try_from(value.unix_timestamp_nanos()).map_err(|_| CampaignValidationError::State)
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum CampaignValidationError {
    #[error("campaign content does not match its bounded public identity")]
    Content,
    #[error("campaign version is outside its storage range")]
    Version,
    #[error("campaign lifecycle fields are inconsistent")]
    State,
    #[error("campaign aggregate counts exceed their bound")]
    Counts,
    #[error("campaign claim does not have a bounded valid lease")]
    Lease,
}
