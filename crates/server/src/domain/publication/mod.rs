//! Publication models, persistence operations, and public HTTP handlers.

pub(crate) mod activation;
pub(crate) mod admin;
pub(crate) mod scheduler;
pub(crate) mod store;
mod visibility;
pub(crate) mod web;

pub use visibility::{PublicLedgerProjection, PublishedPostRevision};

use std::marker::PhantomData;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};

use crate::{
    content::{PostId, PostRevisionDigest, SourceCommit},
    domain::distribution::{
        DistributionTarget, TargetIdempotencyKey, TargetPayload, TargetPayloadDigest,
        target_idempotency_key,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalState {
    Scheduled,
    Activating,
    Blocked,
    Published,
    Superseded,
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

macro_rules! state_markers {
    ($(#[$meta:meta])* $module:ident { $($state:ident),+ $(,)? }) => {
        $(#[$meta])*
        pub mod $module {
            $(
                #[derive(Clone, Copy, Debug, Eq, PartialEq)]
                pub struct $state;
            )+
        }
    };
}

state_markers! {
    /// Compile-time states for [`CanonicalPublication`].
    canonical { Scheduled, Activating, Blocked, Published, Superseded, Cancelled }
}

state_markers! {
    /// Compile-time states for [`TargetJob`].
    target {
        WaitingForCanonical, Scheduled, Ready, Running, Succeeded, Failed, OutcomeUnknown,
        Cancelled,
    }
}

/// A canonical publication whose legal operations are selected by `S`.
///
/// The state parameter prevents invalid transitions from compiling. For
/// example, a scheduled publication cannot be committed as published:
///
/// ```compile_fail
/// use maincopy_server::domain::publication::{CanonicalPublication, canonical};
///
/// fn publish_without_activation(publication: CanonicalPublication<canonical::Scheduled>) {
///     let _ = publication.commit_published(1);
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalPublication<S = canonical::Scheduled> {
    entity: CanonicalPublicationView,
    marker: PhantomData<S>,
}

/// Owned, flat persistence and read representation of a canonical publication.
///
/// Its fields are deliberately public: callers that only need to inspect or
/// persist an entity do not need a parallel getter API. Convert this view into
/// [`CanonicalPublicationStatus`] before applying domain transitions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CanonicalPublicationView {
    pub state: CanonicalState,
    pub stable_post_id: PostId,
    pub pinned_post_digest: PostRevisionDigest,
    pub source_commit: Option<SourceCommit>,
    #[serde(with = "time::serde::rfc3339")]
    pub scheduled_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub activation_started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub published_at: Option<OffsetDateTime>,
    pub current_published_digest: Option<PostRevisionDigest>,
    pub block_reason: Option<ActivationBlockReason>,
    pub version: u64,
}

/// A validated canonical publication restored into its compile-time state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanonicalPublicationStatus {
    Scheduled(CanonicalPublication<canonical::Scheduled>),
    Activating(CanonicalPublication<canonical::Activating>),
    Blocked(CanonicalPublication<canonical::Blocked>),
    Published(CanonicalPublication<canonical::Published>),
    Superseded(CanonicalPublication<canonical::Superseded>),
    Cancelled(CanonicalPublication<canonical::Cancelled>),
}

impl TryFrom<CanonicalPublicationView> for CanonicalPublicationStatus {
    type Error = RehydrationError;

    fn try_from(mut view: CanonicalPublicationView) -> Result<Self, Self::Error> {
        view.validate()?;
        view.normalize_timestamps();
        let state = view.state;
        Ok(match state {
            CanonicalState::Scheduled => Self::Scheduled(canonical_from_view(view)),
            CanonicalState::Activating => Self::Activating(canonical_from_view(view)),
            CanonicalState::Blocked => Self::Blocked(canonical_from_view(view)),
            CanonicalState::Published => Self::Published(canonical_from_view(view)),
            CanonicalState::Superseded => Self::Superseded(canonical_from_view(view)),
            CanonicalState::Cancelled => Self::Cancelled(canonical_from_view(view)),
        })
    }
}

fn canonical_from_view<S>(entity: CanonicalPublicationView) -> CanonicalPublication<S> {
    CanonicalPublication {
        entity,
        marker: PhantomData,
    }
}

impl CanonicalPublicationView {
    fn validate(&self) -> Result<(), RehydrationError> {
        let minimum_version = match self.state {
            CanonicalState::Scheduled => 1,
            CanonicalState::Activating => 2,
            CanonicalState::Blocked | CanonicalState::Published => 3,
            CanonicalState::Superseded => 4,
            CanonicalState::Cancelled => 2,
        };
        if self.version < minimum_version {
            return Err(RehydrationError::CanonicalVersion {
                state: self.state,
                version: self.version,
                minimum: minimum_version,
            });
        }

        let fields_are_valid = match (
            self.activation_started_at,
            self.published_at,
            self.current_published_digest.as_ref(),
            self.block_reason,
        ) {
            (None, None, None, None) => {
                matches!(
                    self.state,
                    CanonicalState::Scheduled | CanonicalState::Cancelled
                )
            }
            (Some(_), None, None, None) => self.state == CanonicalState::Activating,
            (Some(_), None, None, Some(_)) => {
                matches!(
                    self.state,
                    CanonicalState::Blocked | CanonicalState::Cancelled
                )
            }
            (Some(started), Some(published), Some(_), None) => {
                matches!(
                    self.state,
                    CanonicalState::Published | CanonicalState::Superseded
                ) && started == published
            }
            _ => false,
        };
        fields_are_valid
            .then_some(())
            .ok_or(RehydrationError::CanonicalFields { state: self.state })
    }

    fn normalize_timestamps(&mut self) {
        self.scheduled_at = self.scheduled_at.to_offset(UtcOffset::UTC);
        self.activation_started_at = self
            .activation_started_at
            .map(|timestamp| timestamp.to_offset(UtcOffset::UTC));
        self.published_at = self
            .published_at
            .map(|timestamp| timestamp.to_offset(UtcOffset::UTC));
    }
}

impl CanonicalPublication<canonical::Scheduled> {
    pub fn schedule(
        stable_post_id: PostId,
        pinned_post_digest: PostRevisionDigest,
        source_commit: Option<SourceCommit>,
        scheduled_at: OffsetDateTime,
    ) -> Self {
        Self {
            entity: CanonicalPublicationView {
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
            },
            marker: PhantomData,
        }
    }

    pub fn begin_activation(
        self,
        expected_version: u64,
        now: OffsetDateTime,
    ) -> Result<CanonicalPublication<canonical::Activating>, TransitionFailure<Self>> {
        let publication = self.require_version(expected_version)?;
        if now < publication.entity.scheduled_at {
            let error = TransitionError::CanonicalNotDue {
                scheduled_at: publication.entity.scheduled_at,
                now,
            };
            return Err(TransitionFailure::new(publication, error));
        }
        Ok(publication.activate(now))
    }

    pub fn begin_activation_now(
        self,
        expected_version: u64,
        now: OffsetDateTime,
    ) -> Result<CanonicalPublication<canonical::Activating>, TransitionFailure<Self>> {
        let publication = self.require_version(expected_version)?;
        Ok(publication.activate(now))
    }

    fn activate(mut self, now: OffsetDateTime) -> CanonicalPublication<canonical::Activating> {
        self.entity.activation_started_at = Some(now.to_offset(UtcOffset::UTC));
        self.transition(CanonicalState::Activating)
    }
}

impl CanonicalPublication<canonical::Activating> {
    pub fn block_activation(
        self,
        expected_version: u64,
        reason: ActivationBlockReason,
    ) -> Result<CanonicalPublication<canonical::Blocked>, TransitionFailure<Self>> {
        let mut publication = self.require_version(expected_version)?;
        publication.entity.block_reason = Some(reason);
        Ok(publication.transition(CanonicalState::Blocked))
    }

    /// Call only after the pinned revision is visible in the public snapshot.
    pub fn commit_published(
        self,
        expected_version: u64,
    ) -> Result<CanonicalPublication<canonical::Published>, TransitionFailure<Self>> {
        let mut publication = self.require_version(expected_version)?;
        let activation_started_at = publication
            .entity
            .activation_started_at
            .expect("activating publications always have an activation timestamp");
        publication.entity.published_at = Some(activation_started_at);
        publication.entity.current_published_digest =
            Some(publication.entity.pinned_post_digest.clone());
        publication.entity.block_reason = None;
        Ok(publication.transition(CanonicalState::Published))
    }
}

impl CanonicalPublication<canonical::Published> {
    /// Records that a later approved release replaced this public revision.
    pub fn supersede(
        self,
        expected_version: u64,
    ) -> Result<CanonicalPublication<canonical::Superseded>, TransitionFailure<Self>> {
        let publication = self.require_version(expected_version)?;
        Ok(publication.transition(CanonicalState::Superseded))
    }
}

impl CanonicalPublication<canonical::Blocked> {
    pub fn retry_blocked(
        self,
        expected_version: u64,
        now: OffsetDateTime,
    ) -> Result<CanonicalPublication<canonical::Activating>, TransitionFailure<Self>> {
        let mut publication = self.require_version(expected_version)?;
        publication.entity.activation_started_at = Some(now.to_offset(UtcOffset::UTC));
        publication.entity.block_reason = None;
        Ok(publication.transition(CanonicalState::Activating))
    }
}

impl<S> CanonicalPublication<S> {
    pub const fn view(&self) -> &CanonicalPublicationView {
        &self.entity
    }

    pub fn into_view(self) -> CanonicalPublicationView {
        self.entity
    }

    fn require_version(self, expected: u64) -> Result<Self, TransitionFailure<Self>> {
        if self.entity.version == expected {
            Ok(self)
        } else {
            let actual = self.entity.version;
            Err(TransitionFailure::new(
                self,
                TransitionError::VersionConflict { expected, actual },
            ))
        }
    }

    fn transition<T>(mut self, state: CanonicalState) -> CanonicalPublication<T> {
        self.entity.state = state;
        self.entity.version += 1;
        CanonicalPublication {
            entity: self.entity,
            marker: PhantomData,
        }
    }
}

macro_rules! cancellable_publications {
    ($($state:ty),+ $(,)?) => {
        $(
            impl CanonicalPublication<$state> {
                pub fn cancel(
                    self,
                    expected_version: u64,
                ) -> Result<CanonicalPublication<canonical::Cancelled>, TransitionFailure<Self>> {
                    let publication = self.require_version(expected_version)?;
                    Ok(publication.transition(CanonicalState::Cancelled))
                }
            }
        )+
    };
}

cancellable_publications!(canonical::Scheduled, canonical::Blocked);

/// A distribution job whose legal operations are selected by `S`.
///
/// Completed jobs do not expose retry transitions:
///
/// ```compile_fail
/// use maincopy_server::domain::publication::{TargetJob, target};
///
/// fn retry_completed(job: TargetJob<target::Succeeded>) {
///     let _ = job.retry(1);
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetJob<S = target::WaitingForCanonical> {
    entity: TargetJobView,
    marker: PhantomData<S>,
}

/// Owned, flat persistence and read representation of a distribution job.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TargetJobView {
    pub state: TargetJobState,
    pub target: DistributionTarget,
    pub stable_post_id: PostId,
    pub pinned_post_digest: PostRevisionDigest,
    #[serde(with = "time::serde::rfc3339")]
    pub scheduled_at: OffsetDateTime,
    pub payload: TargetPayload,
    pub payload_digest: TargetPayloadDigest,
    pub version: u64,
}

/// A validated target job restored into its compile-time state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TargetJobStatus {
    WaitingForCanonical(TargetJob<target::WaitingForCanonical>),
    Scheduled(TargetJob<target::Scheduled>),
    Ready(TargetJob<target::Ready>),
    Running(TargetJob<target::Running>),
    Succeeded(TargetJob<target::Succeeded>),
    Failed(TargetJob<target::Failed>),
    OutcomeUnknown(TargetJob<target::OutcomeUnknown>),
    Cancelled(TargetJob<target::Cancelled>),
}

impl TryFrom<TargetJobView> for TargetJobStatus {
    type Error = RehydrationError;

    fn try_from(mut view: TargetJobView) -> Result<Self, Self::Error> {
        view.validate()?;
        view.scheduled_at = view.scheduled_at.to_offset(UtcOffset::UTC);
        let state = view.state;

        Ok(match state {
            TargetJobState::WaitingForCanonical => {
                Self::WaitingForCanonical(target_from_view(view))
            }
            TargetJobState::Scheduled => Self::Scheduled(target_from_view(view)),
            TargetJobState::Ready => Self::Ready(target_from_view(view)),
            TargetJobState::Running => Self::Running(target_from_view(view)),
            TargetJobState::Succeeded => Self::Succeeded(target_from_view(view)),
            TargetJobState::Failed => Self::Failed(target_from_view(view)),
            TargetJobState::OutcomeUnknown => Self::OutcomeUnknown(target_from_view(view)),
            TargetJobState::Cancelled => Self::Cancelled(target_from_view(view)),
        })
    }
}

fn target_from_view<S>(entity: TargetJobView) -> TargetJob<S> {
    TargetJob {
        entity,
        marker: PhantomData,
    }
}

impl TargetJobView {
    fn validate(&self) -> Result<(), RehydrationError> {
        let minimum_version = match self.state {
            TargetJobState::WaitingForCanonical => 1,
            TargetJobState::Scheduled | TargetJobState::Ready | TargetJobState::Cancelled => 2,
            TargetJobState::Running | TargetJobState::Succeeded => 3,
            TargetJobState::Failed | TargetJobState::OutcomeUnknown => 4,
        };
        if self.version < minimum_version {
            return Err(RehydrationError::TargetVersion {
                state: self.state,
                version: self.version,
                minimum: minimum_version,
            });
        }
        if self.payload.digest() != self.payload_digest {
            return Err(RehydrationError::TargetPayloadDigestMismatch);
        }
        Ok(())
    }
}

/// The two states selected when a canonical publication releases a job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReleasedTargetJob {
    Scheduled(TargetJob<target::Scheduled>),
    Ready(TargetJob<target::Ready>),
}

impl TargetJob<target::WaitingForCanonical> {
    pub fn waiting(
        target: DistributionTarget,
        stable_post_id: PostId,
        pinned_post_digest: PostRevisionDigest,
        scheduled_at: OffsetDateTime,
        payload: TargetPayload,
    ) -> Self {
        let payload_digest = payload.digest();
        Self {
            entity: TargetJobView {
                state: TargetJobState::WaitingForCanonical,
                target,
                stable_post_id,
                pinned_post_digest,
                scheduled_at: scheduled_at.to_offset(UtcOffset::UTC),
                payload,
                payload_digest,
                version: 1,
            },
            marker: PhantomData,
        }
    }

    pub fn release_after_canonical(
        self,
        expected_version: u64,
        canonical: &CanonicalPublication<canonical::Published>,
        now: OffsetDateTime,
    ) -> Result<ReleasedTargetJob, TransitionFailure<Self>> {
        let job = self.require_version(expected_version)?;
        if canonical.entity.stable_post_id != job.entity.stable_post_id {
            return Err(TransitionFailure::new(
                job,
                TransitionError::PublicationMismatch,
            ));
        }
        if canonical.entity.current_published_digest.as_ref()
            != Some(&job.entity.pinned_post_digest)
        {
            return Err(TransitionFailure::new(
                job,
                TransitionError::RevisionMismatch,
            ));
        }

        Ok(if job.entity.scheduled_at <= now {
            ReleasedTargetJob::Ready(job.transition(TargetJobState::Ready))
        } else {
            ReleasedTargetJob::Scheduled(job.transition(TargetJobState::Scheduled))
        })
    }
}

impl TargetJob<target::Scheduled> {
    pub fn mark_due(
        self,
        expected_version: u64,
        now: OffsetDateTime,
    ) -> Result<TargetJob<target::Ready>, TransitionFailure<Self>> {
        let job = self.require_version(expected_version)?;
        if now < job.entity.scheduled_at {
            let error = TransitionError::TargetNotDue {
                scheduled_at: job.entity.scheduled_at,
                now,
            };
            return Err(TransitionFailure::new(job, error));
        }
        Ok(job.transition(TargetJobState::Ready))
    }
}

impl<S> TargetJob<S> {
    pub const fn view(&self) -> &TargetJobView {
        &self.entity
    }

    pub fn into_view(self) -> TargetJobView {
        self.entity
    }

    pub fn idempotency_key(&self) -> TargetIdempotencyKey {
        target_idempotency_key(
            &self.entity.stable_post_id,
            &self.entity.pinned_post_digest,
            self.entity.target,
        )
    }

    fn require_version(self, expected: u64) -> Result<Self, TransitionFailure<Self>> {
        if self.entity.version == expected {
            Ok(self)
        } else {
            let actual = self.entity.version;
            Err(TransitionFailure::new(
                self,
                TransitionError::VersionConflict { expected, actual },
            ))
        }
    }

    fn transition<T>(mut self, state: TargetJobState) -> TargetJob<T> {
        self.entity.state = state;
        self.entity.version += 1;
        TargetJob {
            entity: self.entity,
            marker: PhantomData,
        }
    }
}

macro_rules! target_transitions {
    ($($from:ty => $method:ident -> $to:ty = $state:expr);+ $(;)?) => {
        $(
            impl TargetJob<$from> {
                pub fn $method(
                    self,
                    expected_version: u64,
                ) -> Result<TargetJob<$to>, TransitionFailure<Self>> {
                    let job = self.require_version(expected_version)?;
                    Ok(job.transition($state))
                }
            }
        )+
    };
}

target_transitions! {
    target::Ready => claim -> target::Running = TargetJobState::Running;
    target::Running => succeed -> target::Succeeded = TargetJobState::Succeeded;
    target::Ready => complete_manually -> target::Succeeded = TargetJobState::Succeeded;
    target::Running => fail -> target::Failed = TargetJobState::Failed;
    target::Running => mark_outcome_unknown -> target::OutcomeUnknown = TargetJobState::OutcomeUnknown;
    target::Failed => retry -> target::Ready = TargetJobState::Ready;
    target::OutcomeUnknown => retry -> target::Ready = TargetJobState::Ready;
    target::WaitingForCanonical => cancel -> target::Cancelled = TargetJobState::Cancelled;
    target::Scheduled => cancel -> target::Cancelled = TargetJobState::Cancelled;
    target::Ready => cancel -> target::Cancelled = TargetJobState::Cancelled;
    target::Failed => cancel -> target::Cancelled = TargetJobState::Cancelled;
    target::OutcomeUnknown => cancel -> target::Cancelled = TargetJobState::Cancelled;
}

/// A failed transition together with the unchanged machine that was supplied.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
#[error("{error}")]
pub struct TransitionFailure<M> {
    pub machine: Box<M>,
    #[source]
    pub error: TransitionError,
}

impl<M> TransitionFailure<M> {
    fn new(machine: M, error: TransitionError) -> Self {
        Self {
            machine: Box::new(machine),
            error,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum RehydrationError {
    #[error(
        "canonical publication {state:?} requires version {minimum} or later, received {version}"
    )]
    CanonicalVersion {
        state: CanonicalState,
        version: u64,
        minimum: u64,
    },
    #[error("canonical publication fields are inconsistent with {state:?} state")]
    CanonicalFields { state: CanonicalState },
    #[error("target job {state:?} requires version {minimum} or later, received {version}")]
    TargetVersion {
        state: TargetJobState,
        version: u64,
        minimum: u64,
    },
    #[error("target job payload does not match its stored digest")]
    TargetPayloadDigestMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum TransitionError {
    #[error("resource version conflict: expected {expected}, actual {actual}")]
    VersionConflict { expected: u64, actual: u64 },
    #[error("target job does not belong to the published canonical publication")]
    PublicationMismatch,
    #[error("target job revision does not match the published canonical revision")]
    RevisionMismatch,
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
    fn scheduled_publication(
        scheduled_at: OffsetDateTime,
    ) -> CanonicalPublication<canonical::Scheduled> {
        CanonicalPublication::schedule(post_id(POST_A), digest(DIGEST_A), None, scheduled_at)
    }
    fn published_publication(
        scheduled_at: OffsetDateTime,
    ) -> CanonicalPublication<canonical::Published> {
        scheduled_publication(scheduled_at)
            .begin_activation(1, scheduled_at)
            .unwrap()
            .commit_published(2)
            .unwrap()
    }
    fn job(scheduled_at: OffsetDateTime) -> TargetJob<target::WaitingForCanonical> {
        TargetJob::waiting(
            DistributionTarget::X,
            post_id(POST_A),
            digest(DIGEST_A),
            scheduled_at,
            TargetPayload::new("copy").unwrap(),
        )
    }
    fn ready_job(scheduled_at: OffsetDateTime, now: OffsetDateTime) -> TargetJob<target::Ready> {
        match job(scheduled_at)
            .release_after_canonical(1, &published_publication(at(10)), now)
            .unwrap()
        {
            ReleasedTargetJob::Ready(job) => job,
            ReleasedTargetJob::Scheduled(_) => panic!("expected a ready job"),
        }
    }
    fn scheduled_job(
        scheduled_at: OffsetDateTime,
        now: OffsetDateTime,
    ) -> TargetJob<target::Scheduled> {
        match job(scheduled_at)
            .release_after_canonical(1, &published_publication(at(10)), now)
            .unwrap()
        {
            ReleasedTargetJob::Scheduled(job) => job,
            ReleasedTargetJob::Ready(_) => panic!("expected a scheduled job"),
        }
    }
    fn restore_canonical(view: CanonicalPublicationView) -> CanonicalPublicationStatus {
        let encoded = serde_json::to_string(&view).unwrap();
        let decoded: CanonicalPublicationView = serde_json::from_str(&encoded).unwrap();
        CanonicalPublicationStatus::try_from(decoded).unwrap()
    }
    fn restore_target(view: TargetJobView) -> TargetJobStatus {
        let encoded = serde_json::to_string(&view).unwrap();
        let decoded: TargetJobView = serde_json::from_str(&encoded).unwrap();
        TargetJobStatus::try_from(decoded).unwrap()
    }

    #[test]
    fn canonical_success_path_assigns_publication_time() {
        let publication = CanonicalPublication::schedule(
            post_id(POST_A),
            digest(DIGEST_A),
            Some(SourceCommit::parse(SOURCE_COMMIT_A).unwrap()),
            at(10),
        )
        .begin_activation(1, at(11))
        .unwrap()
        .commit_published(2)
        .unwrap();
        let view = publication.view();

        assert_eq!(view.state, CanonicalState::Published);
        assert_eq!(view.published_at, Some(at(11)));
        assert_eq!(
            view.current_published_digest
                .as_ref()
                .map(PostRevisionDigest::as_str),
            Some(DIGEST_A)
        );
        assert_eq!(view.version, 3);

        fn requires_published(_: &CanonicalPublication<canonical::Published>) {}
        requires_published(&publication);
    }

    #[test]
    fn scheduled_activation_waits_until_due_but_publish_now_does_not() {
        let scheduled = scheduled_publication(at(20));
        let failure = scheduled.begin_activation(1, at(19)).unwrap_err();
        assert_eq!(
            failure.error,
            TransitionError::CanonicalNotDue {
                scheduled_at: at(20),
                now: at(19),
            }
        );

        let activating = (*failure.machine).begin_activation_now(1, at(19)).unwrap();
        assert_eq!(activating.view().state, CanonicalState::Activating);
        assert_eq!(activating.view().activation_started_at, Some(at(19)));
    }

    #[test]
    fn blocked_retry_preserves_pinned_revision() {
        let publication = scheduled_publication(at(10))
            .begin_activation(1, at(10))
            .unwrap()
            .block_activation(2, ActivationBlockReason::RevisionUnavailable)
            .unwrap();
        assert_eq!(
            publication.view().block_reason,
            Some(ActivationBlockReason::RevisionUnavailable)
        );
        let publication = publication.retry_blocked(3, at(12)).unwrap();
        let view = publication.view();
        assert_eq!(view.state, CanonicalState::Activating);
        assert_eq!(view.pinned_post_digest.as_str(), DIGEST_A);
        assert_eq!(view.published_at, None);
        assert_eq!(view.block_reason, None);
        assert_eq!(view.activation_started_at, Some(at(12)));
    }

    #[test]
    fn stale_version_is_rejected() {
        let failure = scheduled_publication(at(10))
            .begin_activation(2, at(10))
            .unwrap_err();
        assert_eq!(
            failure.error,
            TransitionError::VersionConflict {
                expected: 2,
                actual: 1
            }
        );
        assert_eq!(failure.machine.view().version, 1);
        assert_eq!(failure.machine.view().state, CanonicalState::Scheduled);
    }

    #[test]
    fn target_release_respects_target_schedule() {
        let publication = published_publication(at(10));
        let future = job(at(20))
            .release_after_canonical(1, &publication, at(10))
            .unwrap();
        let ReleasedTargetJob::Scheduled(future) = future else {
            panic!("future job must remain scheduled");
        };
        assert_eq!(future.view().state, TargetJobState::Scheduled);

        let due = job(at(10))
            .release_after_canonical(1, &publication, at(10))
            .unwrap();
        let ReleasedTargetJob::Ready(due) = due else {
            panic!("due job must be ready");
        };
        assert_eq!(due.view().state, TargetJobState::Ready);
    }

    #[test]
    fn target_release_requires_the_matching_published_revision() {
        let other_post =
            CanonicalPublication::schedule(post_id(POST_B), digest(DIGEST_A), None, at(10))
                .begin_activation(1, at(10))
                .unwrap()
                .commit_published(2)
                .unwrap();
        let failure = job(at(10))
            .release_after_canonical(1, &other_post, at(10))
            .unwrap_err();
        assert_eq!(failure.error, TransitionError::PublicationMismatch);

        let other_revision =
            CanonicalPublication::schedule(post_id(POST_A), digest(DIGEST_B), None, at(10))
                .begin_activation(1, at(10))
                .unwrap()
                .commit_published(2)
                .unwrap();
        assert_eq!(
            (*failure.machine)
                .release_after_canonical(1, &other_revision, at(10))
                .unwrap_err()
                .error,
            TransitionError::RevisionMismatch
        );
    }

    #[test]
    fn failed_and_unknown_jobs_can_retry() {
        let failed = ready_job(at(10), at(10))
            .claim(2)
            .unwrap()
            .fail(3)
            .unwrap()
            .retry(4)
            .unwrap();
        assert_eq!(failed.view().state, TargetJobState::Ready);

        let unknown = ready_job(at(10), at(10))
            .claim(2)
            .unwrap()
            .mark_outcome_unknown(3)
            .unwrap()
            .retry(4)
            .unwrap();
        assert_eq!(unknown.view().state, TargetJobState::Ready);
    }

    #[test]
    fn scheduled_target_cannot_become_ready_early() {
        let failure = scheduled_job(at(20), at(10))
            .mark_due(2, at(19))
            .unwrap_err();
        assert_eq!(
            failure.error,
            TransitionError::TargetNotDue {
                scheduled_at: at(20),
                now: at(19),
            }
        );
        let ready = (*failure.machine).mark_due(2, at(20)).unwrap();
        assert_eq!(ready.view().state, TargetJobState::Ready);
    }

    #[test]
    fn ready_manual_target_can_complete_without_being_claimed() {
        let target = ready_job(at(10), at(10)).complete_manually(2).unwrap();
        assert_eq!(target.view().state, TargetJobState::Succeeded);

        fn requires_succeeded(_: &TargetJob<target::Succeeded>) {}
        requires_succeeded(&target);
    }

    #[test]
    fn due_target_can_be_claimed_and_succeed() {
        let ready = scheduled_job(at(20), at(10)).mark_due(2, at(20)).unwrap();
        let running = ready.claim(3).unwrap();
        let succeeded = running.succeed(4).unwrap();

        fn requires_succeeded(_: &TargetJob<target::Succeeded>) {}
        requires_succeeded(&succeeded);
        assert_eq!(succeeded.view().state, TargetJobState::Succeeded);
        assert_eq!(succeeded.view().version, 5);
    }

    #[test]
    fn state_names_are_stable() {
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
    }

    #[test]
    fn views_preserve_the_exact_flat_json_contract() {
        let publication = CanonicalPublication::schedule(
            post_id(POST_A),
            digest(DIGEST_A),
            Some(SourceCommit::parse(SOURCE_COMMIT_A).unwrap()),
            at(10),
        );
        assert_eq!(
            serde_json::to_value(publication.view()).unwrap(),
            serde_json::json!({
                "state": "scheduled",
                "stable_post_id": POST_A,
                "pinned_post_digest": DIGEST_A,
                "source_commit": SOURCE_COMMIT_A,
                "scheduled_at": "1970-01-01T00:00:10Z",
                "activation_started_at": null,
                "published_at": null,
                "current_published_digest": null,
                "block_reason": null,
                "version": 1,
            })
        );

        assert_eq!(
            serde_json::to_value(job(at(10)).view()).unwrap(),
            serde_json::json!({
                "state": "waiting_for_canonical",
                "target": "x",
                "stable_post_id": POST_A,
                "pinned_post_digest": DIGEST_A,
                "scheduled_at": "1970-01-01T00:00:10Z",
                "payload": {"version": 1, "body": "copy"},
                "payload_digest": "target-payload-b3-v1-149abb216bf262f3259b06dd1e16e590c49d208657753fe83b892adcab70c73c",
                "version": 1,
            })
        );
    }

    #[test]
    fn canonical_views_rehydrate_every_typed_state() {
        let views = [
            scheduled_publication(at(10)).into_view(),
            scheduled_publication(at(10))
                .begin_activation(1, at(10))
                .unwrap()
                .into_view(),
            scheduled_publication(at(10))
                .begin_activation(1, at(10))
                .unwrap()
                .block_activation(2, ActivationBlockReason::RevisionUnavailable)
                .unwrap()
                .into_view(),
            published_publication(at(10)).into_view(),
            scheduled_publication(at(10)).cancel(1).unwrap().into_view(),
            scheduled_publication(at(10))
                .begin_activation(1, at(10))
                .unwrap()
                .block_activation(2, ActivationBlockReason::RevisionUnavailable)
                .unwrap()
                .cancel(3)
                .unwrap()
                .into_view(),
        ];

        for expected in views {
            let state = expected.state;
            let actual = match restore_canonical(expected.clone()) {
                CanonicalPublicationStatus::Scheduled(publication) => publication.into_view(),
                CanonicalPublicationStatus::Activating(publication) => publication.into_view(),
                CanonicalPublicationStatus::Blocked(publication) => publication.into_view(),
                CanonicalPublicationStatus::Published(publication) => publication.into_view(),
                CanonicalPublicationStatus::Superseded(publication) => publication.into_view(),
                CanonicalPublicationStatus::Cancelled(publication) => publication.into_view(),
            };
            assert_eq!(actual.state, state);
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn target_views_rehydrate_every_typed_state() {
        let views = [
            job(at(10)).into_view(),
            scheduled_job(at(20), at(10)).into_view(),
            ready_job(at(10), at(10)).into_view(),
            ready_job(at(10), at(10)).claim(2).unwrap().into_view(),
            ready_job(at(10), at(10))
                .complete_manually(2)
                .unwrap()
                .into_view(),
            ready_job(at(10), at(10))
                .claim(2)
                .unwrap()
                .fail(3)
                .unwrap()
                .into_view(),
            ready_job(at(10), at(10))
                .claim(2)
                .unwrap()
                .mark_outcome_unknown(3)
                .unwrap()
                .into_view(),
            job(at(10)).cancel(1).unwrap().into_view(),
        ];

        for expected in views {
            let state = expected.state;
            let actual = match restore_target(expected.clone()) {
                TargetJobStatus::WaitingForCanonical(job) => job.into_view(),
                TargetJobStatus::Scheduled(job) => job.into_view(),
                TargetJobStatus::Ready(job) => job.into_view(),
                TargetJobStatus::Running(job) => job.into_view(),
                TargetJobStatus::Succeeded(job) => job.into_view(),
                TargetJobStatus::Failed(job) => job.into_view(),
                TargetJobStatus::OutcomeUnknown(job) => job.into_view(),
                TargetJobStatus::Cancelled(job) => job.into_view(),
            };
            assert_eq!(actual.state, state);
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn rehydration_rejects_unreachable_state_data() {
        let mut canonical = scheduled_publication(at(10)).into_view();
        canonical.state = CanonicalState::Published;
        canonical.version = 3;
        assert_eq!(
            CanonicalPublicationStatus::try_from(canonical).unwrap_err(),
            RehydrationError::CanonicalFields {
                state: CanonicalState::Published
            }
        );

        let mut canonical = scheduled_publication(at(10)).into_view();
        canonical.published_at = Some(at(10));
        assert_eq!(
            CanonicalPublicationStatus::try_from(canonical).unwrap_err(),
            RehydrationError::CanonicalFields {
                state: CanonicalState::Scheduled
            }
        );

        let mut reloaded = published_publication(at(10)).into_view();
        reloaded.current_published_digest = Some(digest(DIGEST_B));
        assert!(matches!(
            CanonicalPublicationStatus::try_from(reloaded),
            Ok(CanonicalPublicationStatus::Published(_))
        ));

        let mut canonical = scheduled_publication(at(10)).into_view();
        canonical.version = 0;
        assert_eq!(
            CanonicalPublicationStatus::try_from(canonical).unwrap_err(),
            RehydrationError::CanonicalVersion {
                state: CanonicalState::Scheduled,
                version: 0,
                minimum: 1,
            }
        );

        let mut target = job(at(10)).into_view();
        target.state = TargetJobState::Running;
        target.version = 2;
        assert_eq!(
            TargetJobStatus::try_from(target).unwrap_err(),
            RehydrationError::TargetVersion {
                state: TargetJobState::Running,
                version: 2,
                minimum: 3,
            }
        );

        let mut target = job(at(10)).into_view();
        target.payload = TargetPayload::new("tampered copy").unwrap();
        assert_eq!(
            TargetJobStatus::try_from(target).unwrap_err(),
            RehydrationError::TargetPayloadDigestMismatch
        );
    }

    #[test]
    fn stored_timestamps_are_normalized_to_utc() {
        let east = UtcOffset::from_hms(2, 0, 0).unwrap();
        let represented_with_offset = at(10).to_offset(east);
        let publication = scheduled_publication(represented_with_offset);
        assert_eq!(publication.view().scheduled_at.offset(), UtcOffset::UTC);

        let publication = publication
            .begin_activation(1, represented_with_offset)
            .unwrap();
        assert_eq!(
            publication.view().activation_started_at.unwrap().offset(),
            UtcOffset::UTC
        );
        let publication = publication.commit_published(2).unwrap();
        assert_eq!(publication.view().published_at, Some(at(10)));

        let target = job(represented_with_offset);
        assert_eq!(target.view().scheduled_at.offset(), UtcOffset::UTC);

        let mut persisted = scheduled_publication(at(10)).into_view();
        persisted.scheduled_at = represented_with_offset;
        let CanonicalPublicationStatus::Scheduled(restored) =
            CanonicalPublicationStatus::try_from(persisted).unwrap()
        else {
            panic!("expected a scheduled publication");
        };
        assert_eq!(restored.view().scheduled_at.offset(), UtcOffset::UTC);
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
    fn all_cancellable_states_reach_typed_cancelled_states() {
        let publication = scheduled_publication(at(10)).cancel(1).unwrap();
        assert_eq!(publication.view().state, CanonicalState::Cancelled);
        assert_eq!(publication.view().published_at, None);

        let publication = scheduled_publication(at(10))
            .begin_activation(1, at(10))
            .unwrap()
            .block_activation(2, ActivationBlockReason::RevisionUnavailable)
            .unwrap()
            .cancel(3)
            .unwrap();
        assert_eq!(publication.view().state, CanonicalState::Cancelled);

        let waiting = job(at(10)).cancel(1).unwrap();
        let scheduled = scheduled_job(at(20), at(10)).cancel(2).unwrap();
        let ready = ready_job(at(10), at(10)).cancel(2).unwrap();
        let failed = ready_job(at(10), at(10))
            .claim(2)
            .unwrap()
            .fail(3)
            .unwrap()
            .cancel(4)
            .unwrap();
        let unknown = ready_job(at(10), at(10))
            .claim(2)
            .unwrap()
            .mark_outcome_unknown(3)
            .unwrap()
            .cancel(4)
            .unwrap();

        fn requires_cancelled(_: &TargetJob<target::Cancelled>) {}
        for cancelled in [&waiting, &scheduled, &ready, &failed, &unknown] {
            requires_cancelled(cancelled);
            assert_eq!(cancelled.view().state, TargetJobState::Cancelled);
        }
    }
}
