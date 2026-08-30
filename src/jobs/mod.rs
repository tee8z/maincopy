//! Canonical-publication and target-job state machines.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};

pub use crate::content::{PostRevisionDigest, SourceCommit};

use crate::{
    content::PostId,
    distribution::{
        DistributionTarget, TargetIdempotencyKey, TargetPayload, target_idempotency_key,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalState {
    Scheduled,
    Activating,
    Blocked,
    Published,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetJobState {
    WaitingForCanonical,
    Scheduled,
    Ready,
    Running,
    Succeeded,
    Failed,
    OutcomeUnknown,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationBlockReason {
    RevisionUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CanonicalPublication {
    state: CanonicalState,
    stable_post_id: PostId,
    pinned_post_digest: PostRevisionDigest,
    source_commit: Option<SourceCommit>,
    #[serde(with = "time::serde::rfc3339")]
    scheduled_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    activation_started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    published_at: Option<OffsetDateTime>,
    current_published_digest: Option<PostRevisionDigest>,
    block_reason: Option<ActivationBlockReason>,
    version: u64,
}

impl CanonicalPublication {
    pub fn schedule(
        stable_post_id: PostId,
        pinned_post_digest: PostRevisionDigest,
        source_commit: Option<SourceCommit>,
        scheduled_at: OffsetDateTime,
    ) -> Self {
        Self {
            state: CanonicalState::Scheduled,
            stable_post_id,
            pinned_post_digest,
            source_commit,
            scheduled_at: scheduled_at.to_offset(UtcOffset::UTC),
            activation_started_at: None,
            published_at: None,
            current_published_digest: None,
            block_reason: None,
            version: 1,
        }
    }

    pub const fn state(&self) -> CanonicalState {
        self.state
    }
    pub const fn stable_post_id(&self) -> &PostId {
        &self.stable_post_id
    }
    pub const fn pinned_post_digest(&self) -> &PostRevisionDigest {
        &self.pinned_post_digest
    }
    pub const fn source_commit(&self) -> Option<&SourceCommit> {
        self.source_commit.as_ref()
    }
    pub const fn scheduled_at(&self) -> OffsetDateTime {
        self.scheduled_at
    }
    pub const fn activation_started_at(&self) -> Option<OffsetDateTime> {
        self.activation_started_at
    }
    pub const fn published_at(&self) -> Option<OffsetDateTime> {
        self.published_at
    }
    pub const fn current_published_digest(&self) -> Option<&PostRevisionDigest> {
        self.current_published_digest.as_ref()
    }
    pub const fn block_reason(&self) -> Option<ActivationBlockReason> {
        self.block_reason
    }
    pub const fn version(&self) -> u64 {
        self.version
    }

    pub fn begin_activation(
        &mut self,
        expected_version: u64,
        now: OffsetDateTime,
    ) -> Result<(), TransitionError> {
        self.require_version(expected_version)?;
        self.require_state(&[CanonicalState::Scheduled])?;
        if now < self.scheduled_at {
            return Err(TransitionError::CanonicalNotDue {
                scheduled_at: self.scheduled_at,
                now,
            });
        }
        self.activate(now);
        Ok(())
    }

    pub fn begin_activation_now(
        &mut self,
        expected_version: u64,
        now: OffsetDateTime,
    ) -> Result<(), TransitionError> {
        self.require_version(expected_version)?;
        self.require_state(&[CanonicalState::Scheduled])?;
        self.activate(now);
        Ok(())
    }

    fn activate(&mut self, now: OffsetDateTime) {
        self.state = CanonicalState::Activating;
        self.activation_started_at = Some(now.to_offset(UtcOffset::UTC));
        self.version += 1;
    }

    pub fn block_activation(
        &mut self,
        expected_version: u64,
        reason: ActivationBlockReason,
    ) -> Result<(), TransitionError> {
        self.require_version(expected_version)?;
        self.require_state(&[CanonicalState::Activating])?;
        self.state = CanonicalState::Blocked;
        self.block_reason = Some(reason);
        self.version += 1;
        Ok(())
    }

    pub fn retry_blocked(
        &mut self,
        expected_version: u64,
        now: OffsetDateTime,
    ) -> Result<(), TransitionError> {
        self.require_version(expected_version)?;
        self.require_state(&[CanonicalState::Blocked])?;
        self.state = CanonicalState::Activating;
        self.activation_started_at = Some(now.to_offset(UtcOffset::UTC));
        self.block_reason = None;
        self.version += 1;
        Ok(())
    }

    /// Call only after the pinned revision is visible in the public snapshot.
    pub fn commit_published(&mut self, expected_version: u64) -> Result<(), TransitionError> {
        self.require_version(expected_version)?;
        self.require_state(&[CanonicalState::Activating])?;
        let activation_started_at = self
            .activation_started_at
            .ok_or(TransitionError::MissingActivationTimestamp)?;
        self.state = CanonicalState::Published;
        self.published_at = Some(activation_started_at);
        self.current_published_digest = Some(self.pinned_post_digest.clone());
        self.block_reason = None;
        self.version += 1;
        Ok(())
    }

    pub fn cancel(&mut self, expected_version: u64) -> Result<(), TransitionError> {
        self.require_version(expected_version)?;
        self.require_state(&[CanonicalState::Scheduled, CanonicalState::Blocked])?;
        self.state = CanonicalState::Cancelled;
        self.version += 1;
        Ok(())
    }

    fn require_version(&self, expected: u64) -> Result<(), TransitionError> {
        (self.version == expected)
            .then_some(())
            .ok_or(TransitionError::VersionConflict {
                expected,
                actual: self.version,
            })
    }

    fn require_state(&self, allowed: &[CanonicalState]) -> Result<(), TransitionError> {
        allowed
            .contains(&self.state)
            .then_some(())
            .ok_or(TransitionError::InvalidCanonicalTransition { from: self.state })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TargetJob {
    state: TargetJobState,
    target: DistributionTarget,
    stable_post_id: PostId,
    pinned_post_digest: PostRevisionDigest,
    #[serde(with = "time::serde::rfc3339")]
    scheduled_at: OffsetDateTime,
    payload: TargetPayload,
    version: u64,
}

impl TargetJob {
    pub fn waiting(
        target: DistributionTarget,
        stable_post_id: PostId,
        pinned_post_digest: PostRevisionDigest,
        scheduled_at: OffsetDateTime,
        payload: TargetPayload,
    ) -> Self {
        Self {
            state: TargetJobState::WaitingForCanonical,
            target,
            stable_post_id,
            pinned_post_digest,
            scheduled_at: scheduled_at.to_offset(UtcOffset::UTC),
            payload,
            version: 1,
        }
    }

    pub const fn state(&self) -> TargetJobState {
        self.state
    }
    pub const fn target(&self) -> &DistributionTarget {
        &self.target
    }
    pub const fn stable_post_id(&self) -> &PostId {
        &self.stable_post_id
    }
    pub const fn pinned_post_digest(&self) -> &PostRevisionDigest {
        &self.pinned_post_digest
    }
    pub const fn scheduled_at(&self) -> OffsetDateTime {
        self.scheduled_at
    }
    pub const fn payload(&self) -> &TargetPayload {
        &self.payload
    }
    pub const fn version(&self) -> u64 {
        self.version
    }
    pub fn idempotency_key(&self) -> TargetIdempotencyKey {
        target_idempotency_key(
            self.stable_post_id(),
            self.pinned_post_digest(),
            *self.target(),
        )
    }

    pub fn release_after_canonical(
        &mut self,
        expected_version: u64,
        canonical: &CanonicalPublication,
        now: OffsetDateTime,
    ) -> Result<(), TransitionError> {
        self.require_version(expected_version)?;
        self.require_state(&[TargetJobState::WaitingForCanonical])?;
        if canonical.state() != CanonicalState::Published {
            return Err(TransitionError::CanonicalNotPublished {
                state: canonical.state(),
            });
        }
        if canonical.stable_post_id() != self.stable_post_id() {
            return Err(TransitionError::PublicationMismatch);
        }
        if canonical.current_published_digest() != Some(self.pinned_post_digest()) {
            return Err(TransitionError::RevisionMismatch);
        }
        self.state = if self.scheduled_at <= now {
            TargetJobState::Ready
        } else {
            TargetJobState::Scheduled
        };
        self.version += 1;
        Ok(())
    }

    pub fn mark_due(
        &mut self,
        expected_version: u64,
        now: OffsetDateTime,
    ) -> Result<(), TransitionError> {
        self.require_version(expected_version)?;
        self.require_state(&[TargetJobState::Scheduled])?;
        if now < self.scheduled_at {
            return Err(TransitionError::TargetNotDue {
                scheduled_at: self.scheduled_at,
                now,
            });
        }
        self.state = TargetJobState::Ready;
        self.version += 1;
        Ok(())
    }

    pub fn claim(&mut self, expected_version: u64) -> Result<(), TransitionError> {
        self.move_from(
            expected_version,
            &[TargetJobState::Ready],
            TargetJobState::Running,
        )
    }

    pub fn succeed(&mut self, expected_version: u64) -> Result<(), TransitionError> {
        self.move_from(
            expected_version,
            &[TargetJobState::Running],
            TargetJobState::Succeeded,
        )
    }

    pub fn complete_manually(&mut self, expected_version: u64) -> Result<(), TransitionError> {
        self.move_from(
            expected_version,
            &[TargetJobState::Ready],
            TargetJobState::Succeeded,
        )
    }

    pub fn fail(&mut self, expected_version: u64) -> Result<(), TransitionError> {
        self.move_from(
            expected_version,
            &[TargetJobState::Running],
            TargetJobState::Failed,
        )
    }

    pub fn mark_outcome_unknown(&mut self, expected_version: u64) -> Result<(), TransitionError> {
        self.move_from(
            expected_version,
            &[TargetJobState::Running],
            TargetJobState::OutcomeUnknown,
        )
    }

    pub fn retry(&mut self, expected_version: u64) -> Result<(), TransitionError> {
        self.move_from(
            expected_version,
            &[TargetJobState::Failed, TargetJobState::OutcomeUnknown],
            TargetJobState::Ready,
        )
    }

    pub fn cancel(&mut self, expected_version: u64) -> Result<(), TransitionError> {
        self.move_from(
            expected_version,
            &[
                TargetJobState::WaitingForCanonical,
                TargetJobState::Scheduled,
                TargetJobState::Ready,
                TargetJobState::Failed,
                TargetJobState::OutcomeUnknown,
            ],
            TargetJobState::Cancelled,
        )
    }

    fn move_from(
        &mut self,
        expected_version: u64,
        allowed: &[TargetJobState],
        destination: TargetJobState,
    ) -> Result<(), TransitionError> {
        self.require_version(expected_version)?;
        self.require_state(allowed)?;
        self.state = destination;
        self.version += 1;
        Ok(())
    }

    fn require_version(&self, expected: u64) -> Result<(), TransitionError> {
        (self.version == expected)
            .then_some(())
            .ok_or(TransitionError::VersionConflict {
                expected,
                actual: self.version,
            })
    }

    fn require_state(&self, allowed: &[TargetJobState]) -> Result<(), TransitionError> {
        allowed
            .contains(&self.state)
            .then_some(())
            .ok_or(TransitionError::InvalidTargetTransition { from: self.state })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum TransitionError {
    #[error("resource version conflict: expected {expected}, actual {actual}")]
    VersionConflict { expected: u64, actual: u64 },
    #[error("canonical publication cannot transition from {from:?}")]
    InvalidCanonicalTransition { from: CanonicalState },
    #[error("target job cannot transition from {from:?}")]
    InvalidTargetTransition { from: TargetJobState },
    #[error("target job cannot be released while canonical publication is {state:?}")]
    CanonicalNotPublished { state: CanonicalState },
    #[error("target job does not belong to the published canonical publication")]
    PublicationMismatch,
    #[error("target job revision does not match the published canonical revision")]
    RevisionMismatch,
    #[error("activating publication has no activation timestamp")]
    MissingActivationTimestamp,
    #[error("canonical publication is not due until {scheduled_at:?}; current time is {now:?}")]
    CanonicalNotDue {
        scheduled_at: OffsetDateTime,
        now: OffsetDateTime,
    },
    #[error("target job is not due until {scheduled_at:?}; current time is {now:?}")]
    TargetNotDue {
        scheduled_at: OffsetDateTime,
        now: OffsetDateTime,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    const POST_A: &str = "11111111-1111-4111-8111-111111111111";
    const POST_B: &str = "22222222-2222-4222-8222-222222222222";
    const DIGEST_A: &str =
        "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111";
    const DIGEST_B: &str =
        "post-b3-v1-2222222222222222222222222222222222222222222222222222222222222222";
    const SOURCE_COMMIT_A: &str = "git-sha1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }
    fn post_id(value: &str) -> PostId {
        PostId::parse(value).unwrap()
    }
    fn digest(value: &str) -> PostRevisionDigest {
        PostRevisionDigest::parse(value).unwrap()
    }
    fn scheduled_publication(scheduled_at: OffsetDateTime) -> CanonicalPublication {
        CanonicalPublication::schedule(post_id(POST_A), digest(DIGEST_A), None, scheduled_at)
    }
    fn published_publication(scheduled_at: OffsetDateTime) -> CanonicalPublication {
        let mut publication = scheduled_publication(scheduled_at);
        publication.begin_activation(1, scheduled_at).unwrap();
        publication.commit_published(2).unwrap();
        publication
    }
    fn job(scheduled_at: OffsetDateTime) -> TargetJob {
        TargetJob::waiting(
            DistributionTarget::X,
            post_id(POST_A),
            digest(DIGEST_A),
            scheduled_at,
            TargetPayload::new("copy").unwrap(),
        )
    }

    #[test]
    fn canonical_success_path_assigns_publication_time() {
        let mut publication = CanonicalPublication::schedule(
            post_id(POST_A),
            digest(DIGEST_A),
            Some(SourceCommit::parse(SOURCE_COMMIT_A).unwrap()),
            at(10),
        );
        publication.begin_activation(1, at(11)).unwrap();
        publication.commit_published(2).unwrap();
        assert_eq!(publication.state(), CanonicalState::Published);
        assert_eq!(publication.published_at(), Some(at(11)));
        assert_eq!(
            publication
                .current_published_digest()
                .map(PostRevisionDigest::as_str),
            Some(DIGEST_A)
        );
        assert_eq!(publication.version(), 3);
    }

    #[test]
    fn scheduled_activation_waits_until_due_but_publish_now_does_not() {
        let mut scheduled = scheduled_publication(at(20));
        let before = scheduled.clone();
        assert_eq!(
            scheduled.begin_activation(1, at(19)),
            Err(TransitionError::CanonicalNotDue {
                scheduled_at: at(20),
                now: at(19),
            })
        );
        assert_eq!(scheduled, before);

        scheduled.begin_activation_now(1, at(19)).unwrap();
        assert_eq!(scheduled.state(), CanonicalState::Activating);
        assert_eq!(scheduled.activation_started_at(), Some(at(19)));
    }

    #[test]
    fn blocked_retry_preserves_pinned_revision() {
        let mut publication = scheduled_publication(at(10));
        publication.begin_activation(1, at(10)).unwrap();
        publication
            .block_activation(2, ActivationBlockReason::RevisionUnavailable)
            .unwrap();
        assert_eq!(
            publication.block_reason(),
            Some(ActivationBlockReason::RevisionUnavailable)
        );
        publication.retry_blocked(3, at(12)).unwrap();
        assert_eq!(publication.state(), CanonicalState::Activating);
        assert_eq!(publication.pinned_post_digest().as_str(), DIGEST_A);
        assert_eq!(publication.published_at(), None);
        assert_eq!(publication.block_reason(), None);
        assert_eq!(publication.activation_started_at(), Some(at(12)));
    }

    #[test]
    fn rejected_transition_does_not_mutate() {
        let mut publication = scheduled_publication(at(10));
        let before = publication.clone();
        assert!(matches!(
            publication.commit_published(1),
            Err(TransitionError::InvalidCanonicalTransition { .. })
        ));
        assert_eq!(publication, before);
    }

    #[test]
    fn stale_version_does_not_mutate() {
        let mut publication = scheduled_publication(at(10));
        let before = publication.clone();
        assert_eq!(
            publication.begin_activation(2, at(10)),
            Err(TransitionError::VersionConflict {
                expected: 2,
                actual: 1
            })
        );
        assert_eq!(publication, before);
    }

    #[test]
    fn target_release_respects_target_schedule() {
        let publication = published_publication(at(10));
        let mut future = job(at(20));
        future
            .release_after_canonical(1, &publication, at(10))
            .unwrap();
        assert_eq!(future.state(), TargetJobState::Scheduled);
        let mut due = job(at(10));
        due.release_after_canonical(1, &publication, at(10))
            .unwrap();
        assert_eq!(due.state(), TargetJobState::Ready);
    }

    #[test]
    fn target_cannot_run_before_canonical_release() {
        let mut target = job(at(10));
        let before = target.clone();
        assert!(matches!(
            target.claim(1),
            Err(TransitionError::InvalidTargetTransition {
                from: TargetJobState::WaitingForCanonical
            })
        ));
        assert_eq!(target, before);
    }

    #[test]
    fn target_release_requires_the_matching_published_revision() {
        let scheduled = scheduled_publication(at(10));
        let mut target = job(at(10));
        let before = target.clone();
        assert_eq!(
            target.release_after_canonical(1, &scheduled, at(10)),
            Err(TransitionError::CanonicalNotPublished {
                state: CanonicalState::Scheduled,
            })
        );
        assert_eq!(target, before);

        let mut other_post =
            CanonicalPublication::schedule(post_id(POST_B), digest(DIGEST_A), None, at(10));
        other_post.begin_activation(1, at(10)).unwrap();
        other_post.commit_published(2).unwrap();
        assert_eq!(
            target.release_after_canonical(1, &other_post, at(10)),
            Err(TransitionError::PublicationMismatch)
        );
        assert_eq!(target, before);

        let mut other_revision =
            CanonicalPublication::schedule(post_id(POST_A), digest(DIGEST_B), None, at(10));
        other_revision.begin_activation(1, at(10)).unwrap();
        other_revision.commit_published(2).unwrap();
        assert_eq!(
            target.release_after_canonical(1, &other_revision, at(10)),
            Err(TransitionError::RevisionMismatch)
        );
        assert_eq!(target, before);
    }

    #[test]
    fn retry_is_limited_to_failed_or_unknown_jobs() {
        let publication = published_publication(at(10));
        for finish in [TargetJob::fail, TargetJob::mark_outcome_unknown] {
            let mut target = job(at(10));
            target
                .release_after_canonical(1, &publication, at(10))
                .unwrap();
            target.claim(2).unwrap();
            finish(&mut target, 3).unwrap();
            target.retry(4).unwrap();
            assert_eq!(target.state(), TargetJobState::Ready);
        }
        let mut succeeded = job(at(10));
        succeeded
            .release_after_canonical(1, &publication, at(10))
            .unwrap();
        succeeded.claim(2).unwrap();
        succeeded.succeed(3).unwrap();
        assert!(matches!(
            succeeded.retry(4),
            Err(TransitionError::InvalidTargetTransition {
                from: TargetJobState::Succeeded
            })
        ));
    }

    #[test]
    fn scheduled_target_cannot_become_ready_early() {
        let publication = published_publication(at(10));
        let mut target = job(at(20));
        target
            .release_after_canonical(1, &publication, at(10))
            .unwrap();
        let before = target.clone();
        assert!(matches!(
            target.mark_due(2, at(19)),
            Err(TransitionError::TargetNotDue { .. })
        ));
        assert_eq!(target, before);
    }

    #[test]
    fn ready_manual_target_can_complete_without_being_claimed() {
        let publication = published_publication(at(10));
        let mut target = job(at(10));
        target
            .release_after_canonical(1, &publication, at(10))
            .unwrap();
        target.complete_manually(2).unwrap();
        assert_eq!(target.state(), TargetJobState::Succeeded);
    }

    #[test]
    fn serialized_contract_uses_stable_names_and_rfc3339_utc() {
        for (state, name) in [
            (CanonicalState::Scheduled, "scheduled"),
            (CanonicalState::Activating, "activating"),
            (CanonicalState::Blocked, "blocked"),
            (CanonicalState::Published, "published"),
            (CanonicalState::Cancelled, "cancelled"),
        ] {
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(name)
            );
        }
        for (state, name) in [
            (TargetJobState::WaitingForCanonical, "waiting_for_canonical"),
            (TargetJobState::Scheduled, "scheduled"),
            (TargetJobState::Ready, "ready"),
            (TargetJobState::Running, "running"),
            (TargetJobState::Succeeded, "succeeded"),
            (TargetJobState::Failed, "failed"),
            (TargetJobState::OutcomeUnknown, "outcome_unknown"),
            (TargetJobState::Cancelled, "cancelled"),
        ] {
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(name)
            );
        }
        assert_eq!(
            serde_json::to_value(ActivationBlockReason::RevisionUnavailable).unwrap(),
            serde_json::json!("revision_unavailable")
        );

        let publication = CanonicalPublication::schedule(
            post_id(POST_A),
            digest(DIGEST_A),
            Some(SourceCommit::parse(SOURCE_COMMIT_A).unwrap()),
            at(10),
        );
        let publication = serde_json::to_value(publication).unwrap();
        assert_eq!(
            publication["scheduled_at"],
            serde_json::json!("1970-01-01T00:00:10Z")
        );
        assert_eq!(
            publication["pinned_post_digest"],
            serde_json::json!(DIGEST_A)
        );
        assert_eq!(
            publication["source_commit"],
            serde_json::json!(SOURCE_COMMIT_A)
        );

        let target = serde_json::to_value(job(at(10))).unwrap();
        assert_eq!(target["pinned_post_digest"], serde_json::json!(DIGEST_A));
    }

    #[test]
    fn stored_timestamps_are_normalized_to_utc() {
        let east = UtcOffset::from_hms(2, 0, 0).unwrap();
        let represented_with_offset = at(10).to_offset(east);
        let mut publication = scheduled_publication(represented_with_offset);
        assert_eq!(publication.scheduled_at().offset(), UtcOffset::UTC);

        publication
            .begin_activation(1, represented_with_offset)
            .unwrap();
        assert_eq!(
            publication.activation_started_at().unwrap().offset(),
            UtcOffset::UTC
        );
        publication.commit_published(2).unwrap();
        assert_eq!(publication.published_at(), Some(at(10)));

        let target = job(represented_with_offset);
        assert_eq!(target.scheduled_at().offset(), UtcOffset::UTC);
    }

    #[test]
    fn target_job_builds_its_key_from_typed_identity() {
        let target = job(at(10));
        assert_eq!(
            target.idempotency_key().as_str(),
            concat!(
                "36:11111111-1111-4111-8111-111111111111|75:",
                "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111",
                "|1:x"
            )
        );
    }

    #[test]
    fn jobs_reexport_the_content_owned_identity_types() {
        let content_digest: crate::content::PostRevisionDigest = digest(DIGEST_A);
        let jobs_digest: PostRevisionDigest = content_digest;
        assert_eq!(jobs_digest.as_str(), DIGEST_A);

        let content_commit = crate::content::SourceCommit::parse(SOURCE_COMMIT_A).unwrap();
        let jobs_commit: SourceCommit = content_commit;
        assert_eq!(jobs_commit.as_str(), SOURCE_COMMIT_A);
    }

    #[test]
    fn cancelled_publication_has_no_publication_time() {
        let mut publication = scheduled_publication(at(10));
        publication.cancel(1).unwrap();
        assert_eq!(publication.state(), CanonicalState::Cancelled);
        assert_eq!(publication.published_at(), None);
    }
}
