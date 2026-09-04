use std::{collections::BTreeMap, sync::Arc};

use arc_swap::ArcSwap;
use maincopy_shared::source::SourceSyncId;
use markdown_compiler::{
    ContentTreeDigest, DraftStatus, PostAlias, PostId, PostRevisionDigest, PostSlug, PreviewDigest,
    SiteSnapshotDigest,
};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    database::store::{DatabaseCommandError, DatabaseMutationError},
    domain::{
        auth::store::{
            AuthCommandError, AuthMutationError, AuthStore, SetUserStatus, UserMutationResult,
        },
        profile::{
            ProfileCommandError, ProfileLoadError, ProfileMutationError, ProfileStore,
            SetTipRecipient, StoredTipRecipientSetting, StoredUserProfile, TipRecipientProjection,
            UpdateProfile,
        },
        source::store::{ApplyManagedSourceCatalog, SourceStore},
    },
    frontend_assets::FrontendAssetManifest,
    render::{
        CatalogRetentionError, ContentCatalog, SiteSnapshot, SiteSnapshotActivator,
        SiteSnapshotBuildError, build_site_snapshot, render_bound_post_revision_preview,
        render_site_shell,
    },
    web::Readiness,
};

use super::store::{
    BeginPublishNow, BeginScheduledActivation, BegunPublication, CommandIdempotencyKey,
    CompletedPublication, FinishPublication, FinishedPublication, IndexContentCatalog,
    LookupPublishNow, LookupSchedulePublication, ObservedPostRevision,
    PublicationRouteOwnershipError, PublicationStore, PublishNowLookupError, PublishNowState,
    RecoverablePublicationActivation, SchedulePublication, SchedulePublicationLookupError,
    SchedulePublicationReplay, ScheduledPublication, SiteHead,
};
use super::{PublicLedgerProjection, PublishedPostRevision, SourceCommit};

/// Owns the serialized transition from durable publication intent to public visibility.
pub(crate) struct PublicationCoordinator {
    pub catalog: Arc<ContentCatalog>,
    pub content_digest: ContentTreeDigest,
    pub candidates: Arc<BTreeMap<ContentTreeDigest, Arc<ContentCatalog>>>,
    pub ledger: PublicLedgerProjection,
    pub site: SiteHead,
    pub activator: SiteSnapshotActivator,
    pub store: PublicationStore,
    pub profiles: ProfileStore,
    pub tip_recipient: Option<TipRecipientProjection>,
    pub frontend: &'static FrontendAssetManifest,
    pub source_commit: Option<SourceCommit>,
    pub scheduled: BTreeMap<Uuid, ScheduledPublication>,
    pub scheduler_wakeup: Arc<Notify>,
    pub readiness: Readiness,
    pub cancellation: CancellationToken,
}

/// One request to publish the current catalog revision of a post immediately.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublishNow {
    pub creation_key: Uuid,
    pub publication_id: Uuid,
    pub stable_post_id: PostId,
    pub expected_revision: Option<PostRevisionDigest>,
    pub accepted_preview_digest: PreviewDigest,
}

/// One browser approval bound to the complete public state that was reviewed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublishReviewedNow {
    pub publication: PublishNow,
    pub expected_content_digest: ContentTreeDigest,
    pub expected_site: SiteHead,
    pub expected_public_revision: ReviewedPublicRevision,
}

/// The exact public revision shown while a browser publication was reviewed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReviewedPublicRevision {
    Unpublished,
    Published { revision: PostRevisionDigest },
}

struct PreparedPublishNow {
    selected: SelectedPost,
    prebuilt: CandidateSnapshot,
    requested_content_digest: ContentTreeDigest,
    accepted_preview_digest: PreviewDigest,
    requested: BeginPublishNow,
}

enum PublishNowReview {
    NotRequired,
    Required {
        expected_content_digest: ContentTreeDigest,
        expected_site: SiteHead,
        expected_public_revision: ReviewedPublicRevision,
    },
}

/// The exact durable publication produced by the coordinator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublishedPublication {
    pub publication_id: Uuid,
    pub stable_post_id: PostId,
    pub revision: PostRevisionDigest,
    pub accepted_preview_digest: PreviewDigest,
    pub published_at: OffsetDateTime,
    pub site: SiteHead,
}

/// One future approval that pins the exact current candidate revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Schedule {
    pub creation_key: Uuid,
    pub publication_id: Uuid,
    pub stable_post_id: PostId,
    pub expected_revision: Option<PostRevisionDigest>,
    pub accepted_preview_digest: PreviewDigest,
    pub scheduled_at: OffsetDateTime,
}

/// The durable scheduled approval returned without changing public visibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScheduledApproval {
    pub publication_id: Uuid,
    pub stable_post_id: PostId,
    pub revision: PostRevisionDigest,
    pub accepted_preview_digest: PreviewDigest,
    pub scheduled_at: OffsetDateTime,
    pub site: SiteHead,
}

/// Current durable outcome of replaying a scheduled approval command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ScheduledApprovalOutcome {
    Scheduled(ScheduledApproval),
    Published(PublishedPublication),
}

/// Immutable publication state used by administration reads without entering the actor mailbox.
pub(crate) struct PublicationReadProjection {
    pub catalog: Arc<ContentCatalog>,
    pub content_digest: ContentTreeDigest,
    pub candidates: Arc<BTreeMap<ContentTreeDigest, Arc<ContentCatalog>>>,
    pub ledger: PublicLedgerProjection,
    pub site: SiteHead,
    pub frontend: &'static FrontendAssetManifest,
    pub tip_recipient: Option<TipRecipientProjection>,
}

/// Cloneable bounded command capability for the single publication coordinator actor.
#[derive(Clone)]
pub(crate) struct PublicationCoordinatorHandle {
    commands: mpsc::Sender<PublicationCoordinatorCommand>,
    read_projection: Arc<ArcSwap<PublicationReadProjection>>,
    scheduler_wakeup: Arc<Notify>,
}

/// Owns the mutable coordinator and processes accepted commands in FIFO order.
pub(crate) struct PublicationCoordinatorActor {
    coordinator: PublicationCoordinator,
    commands: mpsc::Receiver<PublicationCoordinatorCommand>,
    read_projection: Arc<ArcSwap<PublicationReadProjection>>,
}

enum PublicationCoordinatorCommand {
    ApplyContentCatalog {
        catalog: Arc<ContentCatalog>,
        content_digest: ContentTreeDigest,
        source_commit: Option<SourceCommit>,
        respond_to: oneshot::Sender<Result<SiteHead, ContentReloadError>>,
    },
    ApplyManagedContentCatalog {
        catalog: Arc<ContentCatalog>,
        content_digest: ContentTreeDigest,
        source_commit: SourceCommit,
        source_store: SourceStore,
        source_sync_id: SourceSyncId,
        expected_sync_version: u64,
        respond_to: oneshot::Sender<Result<SiteHead, ContentReloadError>>,
    },
    PublishNow {
        command: PublishNow,
        respond_to: oneshot::Sender<Result<PublishedPublication, PublicationActivationError>>,
    },
    PublishReviewedNow {
        command: PublishReviewedNow,
        respond_to: oneshot::Sender<Result<PublishedPublication, PublicationActivationError>>,
    },
    Schedule {
        command: Schedule,
        respond_to: oneshot::Sender<Result<ScheduledApprovalOutcome, PublicationActivationError>>,
    },
    ActivateScheduled {
        publication_id: Uuid,
        now: OffsetDateTime,
        respond_to: oneshot::Sender<Result<PublishedPublication, PublicationActivationError>>,
    },
    UpdateProfile {
        command: UpdateProfile,
        respond_to: oneshot::Sender<Result<StoredUserProfile, ProfileTransitionError>>,
    },
    SetTipRecipient {
        command: SetTipRecipient,
        respond_to: oneshot::Sender<Result<StoredTipRecipientSetting, ProfileTransitionError>>,
    },
    SetUserStatus {
        store: AuthStore,
        command: SetUserStatus,
        respond_to: oneshot::Sender<Result<UserMutationResult, UserStatusTransitionError>>,
    },
}

impl PublicationCoordinator {
    /// Converts the initialized coordinator into its sole-owner actor and bounded handle.
    pub(crate) fn into_actor(
        self,
        queue_capacity: usize,
    ) -> (PublicationCoordinatorHandle, PublicationCoordinatorActor) {
        assert!(
            queue_capacity > 0,
            "coordinator queue capacity must be positive"
        );
        let scheduler_wakeup = Arc::clone(&self.scheduler_wakeup);
        let read_projection = Arc::new(ArcSwap::from_pointee(self.read_projection()));
        let (commands, receiver) = mpsc::channel(queue_capacity);
        (
            PublicationCoordinatorHandle {
                commands,
                read_projection: Arc::clone(&read_projection),
                scheduler_wakeup,
            },
            PublicationCoordinatorActor {
                coordinator: self,
                commands: receiver,
                read_projection,
            },
        )
    }

    fn read_projection(&self) -> PublicationReadProjection {
        PublicationReadProjection {
            catalog: Arc::clone(&self.catalog),
            content_digest: self.content_digest.clone(),
            candidates: Arc::clone(&self.candidates),
            ledger: self.ledger.clone(),
            site: self.site.clone(),
            frontend: self.frontend,
            tip_recipient: self.tip_recipient.clone(),
        }
    }
}

impl PublicationCoordinatorHandle {
    /// Loads one internally consistent immutable publication read projection.
    pub(crate) fn read(&self) -> Arc<PublicationReadProjection> {
        self.read_projection.load_full()
    }

    pub(crate) fn scheduler_wakeup(&self) -> Arc<Notify> {
        Arc::clone(&self.scheduler_wakeup)
    }

    pub(crate) async fn apply_content_catalog(
        &self,
        catalog: Arc<ContentCatalog>,
        content_digest: ContentTreeDigest,
        source_commit: Option<SourceCommit>,
    ) -> Result<SiteHead, ContentReloadError> {
        self.request(
            |respond_to| PublicationCoordinatorCommand::ApplyContentCatalog {
                catalog,
                content_digest,
                source_commit,
                respond_to,
            },
        )
        .await
        .map_err(ContentReloadError::from)?
    }

    /// Installs one managed candidate and commits its source operation in the
    /// same database transaction as the private revision index.
    pub(crate) async fn apply_managed_content_catalog(
        &self,
        catalog: Arc<ContentCatalog>,
        content_digest: ContentTreeDigest,
        source_commit: SourceCommit,
        source_store: SourceStore,
        source_sync_id: SourceSyncId,
        expected_sync_version: u64,
    ) -> Result<SiteHead, ContentReloadError> {
        self.request(
            |respond_to| PublicationCoordinatorCommand::ApplyManagedContentCatalog {
                catalog,
                content_digest,
                source_commit,
                source_store,
                source_sync_id,
                expected_sync_version,
                respond_to,
            },
        )
        .await
        .map_err(ContentReloadError::from)?
    }

    pub(crate) async fn publish_now(
        &self,
        command: PublishNow,
    ) -> Result<PublishedPublication, PublicationActivationError> {
        self.request(|respond_to| PublicationCoordinatorCommand::PublishNow {
            command,
            respond_to,
        })
        .await
        .map_err(PublicationActivationError::from)?
    }

    pub(crate) async fn publish_reviewed_now(
        &self,
        command: PublishReviewedNow,
    ) -> Result<PublishedPublication, PublicationActivationError> {
        self.request(
            |respond_to| PublicationCoordinatorCommand::PublishReviewedNow {
                command,
                respond_to,
            },
        )
        .await
        .map_err(PublicationActivationError::from)?
    }

    pub(crate) async fn schedule(
        &self,
        command: Schedule,
    ) -> Result<ScheduledApprovalOutcome, PublicationActivationError> {
        self.request(|respond_to| PublicationCoordinatorCommand::Schedule {
            command,
            respond_to,
        })
        .await
        .map_err(PublicationActivationError::from)?
    }

    pub(crate) async fn activate_scheduled(
        &self,
        publication_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<PublishedPublication, PublicationActivationError> {
        self.request(
            |respond_to| PublicationCoordinatorCommand::ActivateScheduled {
                publication_id,
                now,
                respond_to,
            },
        )
        .await
        .map_err(PublicationActivationError::from)?
    }

    pub(crate) async fn update_profile(
        &self,
        command: UpdateProfile,
    ) -> Result<StoredUserProfile, ProfileTransitionError> {
        self.request(|respond_to| PublicationCoordinatorCommand::UpdateProfile {
            command,
            respond_to,
        })
        .await
        .map_err(ProfileTransitionError::from)?
    }

    pub(crate) async fn set_tip_recipient(
        &self,
        command: SetTipRecipient,
    ) -> Result<StoredTipRecipientSetting, ProfileTransitionError> {
        self.request(
            |respond_to| PublicationCoordinatorCommand::SetTipRecipient {
                command,
                respond_to,
            },
        )
        .await
        .map_err(ProfileTransitionError::from)?
    }

    pub(crate) async fn set_user_status(
        &self,
        store: AuthStore,
        command: SetUserStatus,
    ) -> Result<UserMutationResult, UserStatusTransitionError> {
        self.request(|respond_to| PublicationCoordinatorCommand::SetUserStatus {
            store,
            command,
            respond_to,
        })
        .await
        .map_err(UserStatusTransitionError::from)?
    }

    async fn request<Response>(
        &self,
        command: impl FnOnce(oneshot::Sender<Response>) -> PublicationCoordinatorCommand,
    ) -> Result<Response, PublicationCoordinatorUnavailable> {
        let (respond_to, response) = oneshot::channel();
        self.commands
            .send(command(respond_to))
            .await
            .map_err(|_| PublicationCoordinatorUnavailable::Closed)?;
        response
            .await
            .map_err(|_| PublicationCoordinatorUnavailable::OutcomeUnknown)
    }
}

impl PublicationCoordinatorActor {
    /// Runs until cancellation, draining every command accepted before admission closes.
    pub(crate) async fn run(
        mut self,
        cancellation: CancellationToken,
    ) -> Result<(), PublicationCoordinatorActorError> {
        loop {
            let command = tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    self.commands.close();
                    break;
                }
                command = self.commands.recv() => match command {
                    Some(command) => command,
                    None => return Err(PublicationCoordinatorActorError::HandlesDropped),
                }
            };
            self.execute(command).await;
        }
        while let Some(command) = self.commands.recv().await {
            self.execute(command).await;
        }
        Ok(())
    }

    async fn execute(&mut self, command: PublicationCoordinatorCommand) {
        match command {
            PublicationCoordinatorCommand::ApplyContentCatalog {
                catalog,
                content_digest,
                source_commit,
                respond_to,
            } => {
                let result = self
                    .coordinator
                    .apply_content_catalog(catalog, content_digest, source_commit)
                    .await;
                if result.is_ok() {
                    self.publish_read_projection();
                }
                let _ = respond_to.send(result);
            }
            PublicationCoordinatorCommand::ApplyManagedContentCatalog {
                catalog,
                content_digest,
                source_commit,
                source_store,
                source_sync_id,
                expected_sync_version,
                respond_to,
            } => {
                let result = self
                    .coordinator
                    .apply_managed_content_catalog(
                        catalog,
                        content_digest,
                        source_commit,
                        source_store,
                        source_sync_id,
                        expected_sync_version,
                    )
                    .await;
                if result.is_ok() {
                    self.publish_read_projection();
                }
                let _ = respond_to.send(result);
            }
            PublicationCoordinatorCommand::PublishNow {
                command,
                respond_to,
            } => {
                let result = self.coordinator.publish_now(command).await;
                if result.is_ok() {
                    self.publish_read_projection();
                }
                let _ = respond_to.send(result);
            }
            PublicationCoordinatorCommand::PublishReviewedNow {
                command,
                respond_to,
            } => {
                let result = self.coordinator.publish_reviewed_now(command).await;
                if result.is_ok() {
                    self.publish_read_projection();
                }
                let _ = respond_to.send(result);
            }
            PublicationCoordinatorCommand::Schedule {
                command,
                respond_to,
            } => {
                let result = self.coordinator.schedule(command).await;
                let _ = respond_to.send(result);
            }
            PublicationCoordinatorCommand::ActivateScheduled {
                publication_id,
                now,
                respond_to,
            } => {
                let result = self
                    .coordinator
                    .activate_scheduled(publication_id, now)
                    .await;
                if result.is_ok() {
                    self.publish_read_projection();
                }
                let _ = respond_to.send(result);
            }
            PublicationCoordinatorCommand::UpdateProfile {
                command,
                respond_to,
            } => {
                let mut safety = FailClosedGuard::new(
                    &self.coordinator.readiness,
                    &self.coordinator.cancellation,
                );
                let result = self.coordinator.update_profile(command).await;
                if result.is_ok() {
                    self.publish_read_projection();
                    safety.disarm();
                } else if result
                    .as_ref()
                    .is_err_and(profile_transition_definitely_uncommitted)
                {
                    safety.disarm();
                }
                let _ = respond_to.send(result);
            }
            PublicationCoordinatorCommand::SetTipRecipient {
                command,
                respond_to,
            } => {
                let mut safety = FailClosedGuard::new(
                    &self.coordinator.readiness,
                    &self.coordinator.cancellation,
                );
                let result = self.coordinator.set_tip_recipient(command).await;
                if result.is_ok() {
                    self.publish_read_projection();
                    safety.disarm();
                } else if result
                    .as_ref()
                    .is_err_and(profile_transition_definitely_uncommitted)
                {
                    safety.disarm();
                }
                let _ = respond_to.send(result);
            }
            PublicationCoordinatorCommand::SetUserStatus {
                store,
                command,
                respond_to,
            } => {
                let mut safety = FailClosedGuard::new(
                    &self.coordinator.readiness,
                    &self.coordinator.cancellation,
                );
                let result = self.coordinator.set_user_status(&store, command).await;
                if result.is_ok() {
                    self.publish_read_projection();
                    safety.disarm();
                } else if result
                    .as_ref()
                    .is_err_and(user_status_transition_definitely_uncommitted)
                {
                    safety.disarm();
                }
                let _ = respond_to.send(result);
            }
        }
    }

    fn publish_read_projection(&self) {
        self.read_projection
            .store(Arc::new(self.coordinator.read_projection()));
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum PublicationCoordinatorUnavailable {
    #[error("the publication coordinator stopped before accepting the command")]
    Closed,
    #[error("the accepted publication command outcome is unknown")]
    OutcomeUnknown,
}

#[derive(Debug, Error)]
pub(crate) enum ProfileTransitionError {
    #[error(transparent)]
    Coordinator(#[from] PublicationCoordinatorUnavailable),
    #[error(transparent)]
    Mutation(#[from] ProfileMutationError),
    #[error(transparent)]
    Load(#[from] ProfileLoadError),
    #[error(transparent)]
    Snapshot(#[from] SiteSnapshotBuildError),
    #[error("the active presentation snapshot changed during the profile transition")]
    SnapshotActivationConflict,
}

#[derive(Debug, Error)]
pub(crate) enum UserStatusTransitionError {
    #[error(transparent)]
    Coordinator(#[from] PublicationCoordinatorUnavailable),
    #[error(transparent)]
    Mutation(#[from] AuthMutationError),
    #[error(transparent)]
    Presentation(#[from] ProfileTransitionError),
}

fn user_status_transition_definitely_uncommitted(error: &UserStatusTransitionError) -> bool {
    match error {
        UserStatusTransitionError::Mutation(AuthMutationError::Admission(_)) => true,
        UserStatusTransitionError::Mutation(AuthMutationError::Command(
            AuthCommandError::OutcomeUnknown,
        ))
        | UserStatusTransitionError::Coordinator(_)
        | UserStatusTransitionError::Presentation(_) => false,
        UserStatusTransitionError::Mutation(AuthMutationError::Command(
            AuthCommandError::AlreadyBootstrapped
            | AuthCommandError::BootstrapRequired
            | AuthCommandError::NotFound
            | AuthCommandError::Conflict
            | AuthCommandError::StaleVersion
            | AuthCommandError::NoLoginProvider
            | AuthCommandError::EnabledUserRequiresCredential
            | AuthCommandError::LastEnabledOwner
            | AuthCommandError::ScopeEscalation
            | AuthCommandError::InvalidChallenge
            | AuthCommandError::ChallengeCapacity
            | AuthCommandError::ReplayCapacity
            | AuthCommandError::SessionCapacity
            | AuthCommandError::AgentCredentialCapacity
            | AuthCommandError::ReplayedProof
            | AuthCommandError::IdempotencyConflict
            | AuthCommandError::InvalidValue,
        )) => true,
    }
}

fn profile_transition_definitely_uncommitted(error: &ProfileTransitionError) -> bool {
    matches!(
        error,
        ProfileTransitionError::Mutation(ProfileMutationError::Admission(_))
            | ProfileTransitionError::Mutation(ProfileMutationError::Command(
                ProfileCommandError::NotFound
                    | ProfileCommandError::Conflict
                    | ProfileCommandError::Forbidden
                    | ProfileCommandError::StaleVersion
                    | ProfileCommandError::IdempotencyConflict
                    | ProfileCommandError::InvalidValue
            ))
    )
}

#[derive(Debug, Error)]
pub(crate) enum PublicationCoordinatorActorError {
    #[error("all publication coordinator handles were dropped")]
    HandlesDropped,
}

impl PublicationCoordinator {
    async fn set_user_status(
        &mut self,
        store: &AuthStore,
        command: SetUserStatus,
    ) -> Result<UserMutationResult, UserStatusTransitionError> {
        let result = store.set_user_status(command).await?;
        self.refresh_tip_presentation().await?;
        Ok(result)
    }

    async fn update_profile(
        &mut self,
        command: UpdateProfile,
    ) -> Result<StoredUserProfile, ProfileTransitionError> {
        let profile = self.profiles.update_profile(command).await?;
        self.refresh_tip_presentation().await?;
        Ok(profile)
    }

    async fn set_tip_recipient(
        &mut self,
        command: SetTipRecipient,
    ) -> Result<StoredTipRecipientSetting, ProfileTransitionError> {
        let setting = self.profiles.set_tip_recipient(command).await?;
        self.refresh_tip_presentation().await?;
        Ok(setting)
    }

    async fn refresh_tip_presentation(&mut self) -> Result<(), ProfileTransitionError> {
        let tip_recipient = self.profiles.effective_tip_recipient().await?;
        let shell = render_site_shell(Arc::clone(&self.catalog), self.frontend, &self.ledger)?
            .bind_tip_recipient(tip_recipient.clone());
        let snapshot = build_site_snapshot(shell, &self.ledger)?;
        self.activator
            .activate(&self.site.digest, snapshot)
            .map_err(|_| ProfileTransitionError::SnapshotActivationConflict)?;
        self.tip_recipient = tip_recipient;
        Ok(())
    }

    /// Installs one validated candidate as the private preview without changing public state.
    pub(crate) async fn apply_content_catalog(
        &mut self,
        catalog: Arc<ContentCatalog>,
        content_digest: ContentTreeDigest,
        source_commit: Option<SourceCommit>,
    ) -> Result<SiteHead, ContentReloadError> {
        let retained_candidate = Arc::clone(&catalog);
        let catalog = self.retain_pinned_revisions(catalog)?;
        let observed_posts = observed_post_revisions(&catalog);
        self.store
            .index_content_catalog(IndexContentCatalog {
                observed_at: OffsetDateTime::now_utc(),
                source_commit: source_commit.clone(),
                posts: observed_posts,
            })
            .await?;
        self.install_private_catalog(catalog, retained_candidate, content_digest, source_commit);
        Ok(self.site.clone())
    }

    async fn apply_managed_content_catalog(
        &mut self,
        catalog: Arc<ContentCatalog>,
        content_digest: ContentTreeDigest,
        source_commit: SourceCommit,
        source_store: SourceStore,
        source_sync_id: SourceSyncId,
        expected_sync_version: u64,
    ) -> Result<SiteHead, ContentReloadError> {
        let retained_candidate = Arc::clone(&catalog);
        let catalog = self.retain_pinned_revisions(catalog)?;
        source_store
            .apply_catalog(ApplyManagedSourceCatalog {
                source_sync_id,
                expected_sync_version,
                source_commit: source_commit.clone(),
                content_digest: content_digest.clone(),
                observed_posts: observed_post_revisions(&catalog),
                completed_at: OffsetDateTime::now_utc(),
            })
            .await?;
        self.install_private_catalog(
            catalog,
            retained_candidate,
            content_digest,
            Some(source_commit),
        );
        Ok(self.site.clone())
    }

    fn retain_pinned_revisions(
        &self,
        catalog: Arc<ContentCatalog>,
    ) -> Result<Arc<ContentCatalog>, CatalogRetentionError> {
        let mut candidate = catalog.as_ref().clone();
        candidate.retain_revisions_from(&self.catalog, self.ledger.revision_keys())?;
        candidate.retain_revisions_from(
            &self.catalog,
            self.scheduled.values().map(|scheduled| {
                let view = scheduled.publication.view();
                (view.stable_post_id.clone(), view.pinned_post_digest.clone())
            }),
        )?;
        Ok(Arc::new(candidate))
    }

    fn install_private_catalog(
        &mut self,
        catalog: Arc<ContentCatalog>,
        retained_candidate: Arc<ContentCatalog>,
        content_digest: ContentTreeDigest,
        source_commit: Option<SourceCommit>,
    ) {
        self.catalog = catalog;
        Arc::make_mut(&mut self.candidates).insert(content_digest.clone(), retained_candidate);
        self.content_digest = content_digest;
        self.source_commit = source_commit;
    }

    /// Publishes one current, publishable catalog revision.
    pub(crate) async fn publish_now(
        &mut self,
        command: PublishNow,
    ) -> Result<PublishedPublication, PublicationActivationError> {
        self.publish_now_with_review(command, PublishNowReview::NotRequired)
            .await
    }

    /// Publishes only if the browser-reviewed candidate and public heads are still current.
    pub(crate) async fn publish_reviewed_now(
        &mut self,
        command: PublishReviewedNow,
    ) -> Result<PublishedPublication, PublicationActivationError> {
        let PublishReviewedNow {
            publication,
            expected_content_digest,
            expected_site,
            expected_public_revision,
        } = command;
        self.publish_now_with_review(
            publication,
            PublishNowReview::Required {
                expected_content_digest,
                expected_site,
                expected_public_revision,
            },
        )
        .await
    }

    async fn publish_now_with_review(
        &mut self,
        command: PublishNow,
        review: PublishNowReview,
    ) -> Result<PublishedPublication, PublicationActivationError> {
        let Some(replay) = self.publish_now_replay(&command).await? else {
            self.require_publish_now_review(&command.stable_post_id, &review)?;
            let prepared = self.prepare_publish_now(command)?;
            return self.begin_prepared_publish_now(prepared).await;
        };
        self.resume_publish_now_state(replay).await
    }

    fn require_publish_now_review(
        &self,
        post_id: &PostId,
        review: &PublishNowReview,
    ) -> Result<(), PublicationActivationError> {
        let PublishNowReview::Required {
            expected_content_digest,
            expected_site,
            expected_public_revision,
        } = review
        else {
            return Ok(());
        };
        if expected_content_digest != &self.content_digest {
            return Err(PublishReviewError::Content {
                reviewed: Box::new(expected_content_digest.clone()),
                current: Box::new(self.content_digest.clone()),
            }
            .into());
        }
        if expected_site != &self.site {
            return Err(PublishReviewError::SiteHead {
                reviewed: Box::new(expected_site.clone()),
                current: Box::new(self.site.clone()),
            }
            .into());
        }
        let current_public_revision = match self.ledger.published_post(post_id) {
            Some(published) => ReviewedPublicRevision::Published {
                revision: published.revision.clone(),
            },
            None => ReviewedPublicRevision::Unpublished,
        };
        if expected_public_revision != &current_public_revision {
            return Err(PublishReviewError::PublicRevision {
                post_id: post_id.clone(),
                reviewed: Box::new(expected_public_revision.clone()),
                current: Box::new(current_public_revision),
            }
            .into());
        }
        Ok(())
    }

    async fn publish_now_replay(
        &self,
        command: &PublishNow,
    ) -> Result<Option<PublishNowState>, PublicationActivationError> {
        match self
            .store
            .publish_now_replay(LookupPublishNow {
                creation_key: CommandIdempotencyKey::new(command.creation_key),
                stable_post_id: command.stable_post_id.clone(),
                expected_revision: command.expected_revision.clone(),
                accepted_preview_digest: command.accepted_preview_digest.clone(),
            })
            .await
        {
            Ok(replay) => Ok(replay),
            Err(error @ PublishNowLookupError::InvalidStoredState) => {
                let _safety = FailClosedGuard::new(&self.readiness, &self.cancellation);
                Err(PublicationActivationError::Lookup(error))
            }
            Err(error) => Err(PublicationActivationError::Lookup(error)),
        }
    }

    async fn resume_publish_now_state(
        &mut self,
        replay: PublishNowState,
    ) -> Result<PublishedPublication, PublicationActivationError> {
        match replay {
            PublishNowState::Published(completed) => {
                canonical_publication_result(&self.ledger, completed_result(completed))
            }
            PublishNowState::Activating(begun) => self.resume_publish_now(begun).await,
        }
    }

    async fn resume_publish_now(
        &mut self,
        begun: BegunPublication,
    ) -> Result<PublishedPublication, PublicationActivationError> {
        let mut safety = FailClosedGuard::new(&self.readiness, &self.cancellation);
        let catalog = self.catalog_for_content_digest(&begun.content_digest)?;
        let selected = select_stored_post(&catalog, begun.publication.view())?;
        require_accepted_preview(
            reproduce_preview_digest(
                &catalog,
                self.frontend,
                &self.ledger,
                self.tip_recipient.as_ref(),
                &selected,
            )?,
            &begun.accepted_preview_digest,
        )?;
        self.ensure_routes_available(&selected).await?;
        let candidate = self.candidate_for_begun(
            catalog,
            &selected,
            begun.publication.view(),
            &begun.candidate_site_digest,
            None,
        )?;
        self.activate_and_finish(begun, selected, candidate, &mut safety)
            .await
    }

    fn prepare_publish_now(
        &self,
        command: PublishNow,
    ) -> Result<PreparedPublishNow, PublicationActivationError> {
        let selected = select_post(
            &self.catalog,
            &command.stable_post_id,
            command.expected_revision.as_ref(),
        )?;
        require_update_precondition(&self.ledger, &selected, command.expected_revision.as_ref())?;
        require_accepted_preview(
            reproduce_preview_digest(
                &self.catalog,
                self.frontend,
                &self.ledger,
                self.tip_recipient.as_ref(),
                &selected,
            )?,
            &command.accepted_preview_digest,
        )?;
        let now = OffsetDateTime::now_utc();
        let prebuilt = build_candidate(
            Arc::clone(&self.catalog),
            self.frontend,
            &self.ledger,
            self.tip_recipient.as_ref(),
            &selected,
            now,
        )?;
        let requested_content_digest = self.content_digest.clone();
        let accepted_preview_digest = command.accepted_preview_digest.clone();
        let requested = BeginPublishNow {
            creation_key: CommandIdempotencyKey::new(command.creation_key),
            publication_id: command.publication_id,
            stable_post_id: selected.stable_post_id.clone(),
            pinned_post_digest: selected.revision.clone(),
            expected_revision: command.expected_revision,
            expected_site: self.site.clone(),
            source_commit: self.source_commit.clone(),
            content_digest: requested_content_digest.clone(),
            accepted_preview_digest: command.accepted_preview_digest,
            now,
            candidate_site_digest: prebuilt.snapshot.digest.clone(),
        };
        Ok(PreparedPublishNow {
            selected,
            prebuilt,
            requested_content_digest,
            accepted_preview_digest,
            requested,
        })
    }

    async fn begin_prepared_publish_now(
        &mut self,
        prepared: PreparedPublishNow,
    ) -> Result<PublishedPublication, PublicationActivationError> {
        let PreparedPublishNow {
            selected,
            prebuilt,
            requested_content_digest,
            accepted_preview_digest,
            requested,
        } = prepared;
        self.ensure_routes_available(&selected).await?;
        let mut safety = FailClosedGuard::new(&self.readiness, &self.cancellation);
        let begun = match self.store.begin_publish_now(requested).await {
            Ok(PublishNowState::Activating(begun)) => begun,
            Ok(PublishNowState::Published(completed)) => {
                if !prebuilt.already_published {
                    return Err(PublicationActivationError::DurableStateMismatch);
                }
                let result = canonical_publication_result(
                    &self.ledger,
                    validate_completed(completed, &selected)?,
                )?;
                safety.disarm();
                return Ok(result);
            }
            Err(error) => {
                if definitely_unclaimed(&error) {
                    safety.disarm();
                }
                return Err(publish_now_database_error(
                    error,
                    prebuilt.already_published,
                    selected.stable_post_id,
                ));
            }
        };

        validate_begun_publish_now(
            &begun,
            &prebuilt,
            &requested_content_digest,
            &accepted_preview_digest,
        )?;
        let candidate = self.candidate_for_begun(
            Arc::clone(&self.catalog),
            &selected,
            begun.publication.view(),
            &begun.candidate_site_digest,
            Some(prebuilt),
        )?;
        self.activate_and_finish(begun, selected, candidate, &mut safety)
            .await
    }

    /// Approves and pins one exact candidate revision for future activation.
    pub(crate) async fn schedule(
        &mut self,
        command: Schedule,
    ) -> Result<ScheduledApprovalOutcome, PublicationActivationError> {
        let replay = self
            .store
            .schedule_publication_replay(LookupSchedulePublication {
                creation_key: CommandIdempotencyKey::new(command.creation_key),
                stable_post_id: command.stable_post_id.clone(),
                expected_revision: command.expected_revision.clone(),
                accepted_preview_digest: command.accepted_preview_digest.clone(),
                scheduled_at: command.scheduled_at,
            })
            .await;
        let replay = match replay {
            Ok(replay) => replay,
            Err(error @ SchedulePublicationLookupError::ActivationInProgress) => {
                return Err(PublicationActivationError::ScheduleLookup(error));
            }
            Err(error @ SchedulePublicationLookupError::InvalidStoredState) => {
                let _safety = FailClosedGuard::new(&self.readiness, &self.cancellation);
                return Err(PublicationActivationError::ScheduleLookup(error));
            }
            Err(error) => return Err(PublicationActivationError::ScheduleLookup(error)),
        };
        if let Some(replay) = replay {
            return match replay {
                SchedulePublicationReplay::Scheduled(scheduled) => self
                    .accept_scheduled_replay(&command, scheduled)
                    .map(ScheduledApprovalOutcome::Scheduled),
                SchedulePublicationReplay::Published(completed) => {
                    canonical_publication_result(&self.ledger, completed_result(completed))
                        .map(ScheduledApprovalOutcome::Published)
                }
            };
        }

        let now = OffsetDateTime::now_utc();
        if command.scheduled_at <= now {
            return Err(PublicationActivationError::ScheduleNotFuture {
                scheduled_at: command.scheduled_at,
                now,
            });
        }
        let selected = select_post(
            &self.catalog,
            &command.stable_post_id,
            command.expected_revision.as_ref(),
        )?;
        require_update_precondition(&self.ledger, &selected, command.expected_revision.as_ref())?;
        require_accepted_preview(
            reproduce_preview_digest(
                &self.catalog,
                self.frontend,
                &self.ledger,
                self.tip_recipient.as_ref(),
                &selected,
            )?,
            &command.accepted_preview_digest,
        )?;
        let scheduled = self
            .store
            .schedule_publication(SchedulePublication {
                creation_key: CommandIdempotencyKey::new(command.creation_key),
                publication_id: command.publication_id,
                stable_post_id: selected.stable_post_id.clone(),
                pinned_post_digest: selected.revision.clone(),
                expected_revision: command.expected_revision,
                expected_site: self.site.clone(),
                source_commit: self.source_commit.clone(),
                content_digest: self.content_digest.clone(),
                accepted_preview_digest: command.accepted_preview_digest.clone(),
                slug: selected.slug.clone(),
                aliases: Arc::clone(&selected.aliases),
                accepted_at: now,
                scheduled_at: command.scheduled_at,
            })
            .await?;
        let view = scheduled.publication.view();
        if !self.candidates.contains_key(&scheduled.content_digest)
            || scheduled.accepted_preview_digest != command.accepted_preview_digest
            || view.stable_post_id != selected.stable_post_id
            || view.pinned_post_digest != selected.revision
            || view.scheduled_at != command.scheduled_at.to_offset(time::UtcOffset::UTC)
        {
            return Err(PublicationActivationError::DurableStateMismatch);
        }
        let approval = ScheduledApproval {
            publication_id: scheduled.publication_id,
            stable_post_id: view.stable_post_id.clone(),
            revision: view.pinned_post_digest.clone(),
            accepted_preview_digest: scheduled.accepted_preview_digest.clone(),
            scheduled_at: view.scheduled_at,
            site: self.site.clone(),
        };
        self.scheduled.insert(scheduled.publication_id, scheduled);
        self.scheduler_wakeup.notify_one();
        Ok(ScheduledApprovalOutcome::Scheduled(approval))
    }

    fn accept_scheduled_replay(
        &mut self,
        command: &Schedule,
        scheduled: ScheduledPublication,
    ) -> Result<ScheduledApproval, PublicationActivationError> {
        let catalog = self.catalog_for_content_digest(&scheduled.content_digest)?;
        let selected = select_stored_post(&catalog, scheduled.publication.view())?;
        require_accepted_preview(
            reproduce_preview_digest(
                &catalog,
                self.frontend,
                &self.ledger,
                self.tip_recipient.as_ref(),
                &selected,
            )?,
            &scheduled.accepted_preview_digest,
        )?;

        let view = scheduled.publication.view();
        if scheduled.accepted_preview_digest != command.accepted_preview_digest
            || view.stable_post_id != command.stable_post_id
            || command
                .expected_revision
                .as_ref()
                .is_some_and(|expected| expected != &view.pinned_post_digest)
            || view.scheduled_at != command.scheduled_at.to_offset(time::UtcOffset::UTC)
        {
            return Err(PublicationActivationError::DurableStateMismatch);
        }
        let approval = ScheduledApproval {
            publication_id: scheduled.publication_id,
            stable_post_id: view.stable_post_id.clone(),
            revision: view.pinned_post_digest.clone(),
            accepted_preview_digest: scheduled.accepted_preview_digest.clone(),
            scheduled_at: view.scheduled_at,
            site: self.site.clone(),
        };
        self.scheduled.insert(scheduled.publication_id, scheduled);
        self.scheduler_wakeup.notify_one();
        Ok(approval)
    }

    /// Activates one due scheduled approval using its exact retained revision.
    pub(crate) async fn activate_scheduled(
        &mut self,
        publication_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<PublishedPublication, PublicationActivationError> {
        let scheduled = self.scheduled.get(&publication_id).cloned().ok_or(
            PublicationActivationError::ScheduledPublicationUnavailable { publication_id },
        )?;
        let view = scheduled.publication.view();
        let retained = self
            .candidates
            .get(&scheduled.content_digest)
            .ok_or(PublicationActivationError::DurableStateMismatch)?;
        let mut catalog = retained.as_ref().clone();
        catalog
            .retain_revisions_from(&self.catalog, self.ledger.revision_keys())
            .map_err(|_| PublicationActivationError::DurableStateMismatch)?;
        let catalog = Arc::new(catalog);
        let selected = select_stored_post(&catalog, view)?;
        require_accepted_preview(
            reproduce_preview_digest(
                &catalog,
                self.frontend,
                &self.ledger,
                self.tip_recipient.as_ref(),
                &selected,
            )?,
            &scheduled.accepted_preview_digest,
        )?;
        self.ensure_routes_available(&selected).await?;
        let prebuilt = build_candidate(
            catalog,
            self.frontend,
            &self.ledger,
            self.tip_recipient.as_ref(),
            &selected,
            now,
        )?;
        if prebuilt.already_published {
            return Err(PublicationActivationError::DurableStateMismatch);
        }
        let mut safety = FailClosedGuard::new(&self.readiness, &self.cancellation);
        let begun = match self
            .store
            .begin_scheduled_activation(BeginScheduledActivation {
                publication_id,
                expected_publication_version: view.version,
                expected_site: self.site.clone(),
                now,
                candidate_site_digest: prebuilt.digest.clone(),
            })
            .await
        {
            Ok(begun) => begun,
            Err(error) => {
                if definitely_unclaimed(&error) {
                    safety.disarm();
                }
                return Err(PublicationActivationError::Database(error));
            }
        };
        if begun.content_digest != scheduled.content_digest
            || begun.accepted_preview_digest != scheduled.accepted_preview_digest
        {
            return Err(PublicationActivationError::DurableStateMismatch);
        }
        let candidate = self.candidate_for_begun(
            self.catalog_for_content_digest(&begun.content_digest)?,
            &selected,
            begun.publication.view(),
            &begun.candidate_site_digest,
            Some(prebuilt),
        )?;
        let published = self
            .activate_and_finish(begun, selected, candidate, &mut safety)
            .await?;
        self.scheduled.remove(&publication_id);
        Ok(published)
    }

    /// Reconciles a durable `Activating` publication before listeners are bound.
    pub(crate) async fn recover(
        &mut self,
        activation: RecoverablePublicationActivation,
    ) -> Result<PublishedPublication, PublicationActivationError> {
        let mut safety = FailClosedGuard::new(&self.readiness, &self.cancellation);
        let view = activation.publication.view();
        let catalog = self.catalog_for_content_digest(&activation.content_digest)?;
        let selected = select_stored_post(&catalog, view)?;
        require_accepted_preview(
            reproduce_preview_digest(
                &catalog,
                self.frontend,
                &self.ledger,
                self.tip_recipient.as_ref(),
                &selected,
            )?,
            &activation.accepted_preview_digest,
        )?;
        self.ensure_routes_available(&selected).await?;
        let candidate = self.candidate_for_begun(
            catalog,
            &selected,
            view,
            &activation.candidate_site_digest,
            None,
        )?;
        let begun = BegunPublication {
            publication_id: activation.publication_id,
            publication: activation.publication,
            site: self.site.clone(),
            content_digest: activation.content_digest,
            accepted_preview_digest: activation.accepted_preview_digest,
            candidate_site_digest: activation.candidate_site_digest,
        };
        self.activate_and_finish(begun, selected, candidate, &mut safety)
            .await
    }

    async fn ensure_routes_available(
        &self,
        selected: &SelectedPost,
    ) -> Result<(), PublicationActivationError> {
        // The coordinator is the sole production route claimant and serializes every
        // publication command. A successful ownership read therefore remains valid
        // until this command claims the same routes in `finish_publication`.
        self.store
            .ensure_routes_available(&selected.stable_post_id, &selected.slug, &selected.aliases)
            .await?;
        Ok(())
    }

    fn candidate_for_begun(
        &self,
        catalog: Arc<ContentCatalog>,
        selected: &SelectedPost,
        publication: &super::CanonicalPublicationView,
        stored_digest: &SiteSnapshotDigest,
        prebuilt: Option<CandidateSnapshot>,
    ) -> Result<CandidateSnapshot, PublicationActivationError> {
        if publication.stable_post_id != selected.stable_post_id
            || publication.pinned_post_digest != selected.revision
            || self.site.digest == *stored_digest
        {
            return Err(PublicationActivationError::DurableStateMismatch);
        }
        validate_activating(publication, selected)?;
        let published_at = publication
            .activation_started_at
            .ok_or(PublicationActivationError::DurableStateMismatch)?;
        let candidate = match prebuilt {
            Some(prebuilt)
                if prebuilt.published_at == published_at && prebuilt.digest == *stored_digest =>
            {
                prebuilt
            }
            _ => build_candidate(
                catalog,
                self.frontend,
                &self.ledger,
                self.tip_recipient.as_ref(),
                selected,
                published_at,
            )?,
        };
        if candidate.already_published {
            return Err(PublicationActivationError::DurableStateMismatch);
        }
        require_candidate_digest(&candidate, stored_digest)?;
        Ok(candidate)
    }

    fn catalog_for_content_digest(
        &self,
        content_digest: &ContentTreeDigest,
    ) -> Result<Arc<ContentCatalog>, PublicationActivationError> {
        let retained = self
            .candidates
            .get(content_digest)
            .ok_or(PublicationActivationError::DurableStateMismatch)?;
        let mut catalog = retained.as_ref().clone();
        catalog
            .retain_revisions_from(&self.catalog, self.ledger.revision_keys())
            .map_err(|_| PublicationActivationError::DurableStateMismatch)?;
        Ok(Arc::new(catalog))
    }

    async fn activate_and_finish(
        &mut self,
        begun: BegunPublication,
        selected: SelectedPost,
        candidate: CandidateSnapshot,
        safety: &mut FailClosedGuard,
    ) -> Result<PublishedPublication, PublicationActivationError> {
        if begun.site != self.site {
            return Err(PublicationActivationError::DurableStateMismatch);
        }
        require_candidate_digest(&candidate, &begun.candidate_site_digest)?;
        self.activator
            .activate(&begun.site.digest, candidate.snapshot)
            .map_err(|_| PublicationActivationError::SnapshotActivationConflict)?;

        let publication_version = begun.publication.view().version;
        let accepted_preview_digest = begun.accepted_preview_digest.clone();
        let finished = self
            .store
            .finish_publication(FinishPublication {
                publication_id: begun.publication_id,
                expected_publication_version: publication_version,
                expected_site: begun.site,
                candidate_site_digest: begun.candidate_site_digest,
                slug: selected.slug.clone(),
                aliases: Arc::clone(&selected.aliases),
            })
            .await?;
        let result = canonical_publication_result(
            &candidate.ledger,
            validate_finished(finished, &selected)?,
        )?;
        if result.site.digest != candidate.digest
            || result.accepted_preview_digest != accepted_preview_digest
        {
            return Err(PublicationActivationError::DurableStateMismatch);
        }
        self.ledger = candidate.ledger;
        self.site = result.site.clone();
        safety.disarm();
        Ok(result)
    }
}

pub(crate) fn observed_post_revisions(catalog: &ContentCatalog) -> Vec<ObservedPostRevision> {
    catalog
        .rendered_posts()
        .map(|post| ObservedPostRevision {
            stable_post_id: post.document.metadata.id.clone(),
            revision_digest: post.revision.clone(),
            publication_status: post.document.metadata.draft,
            slug: post.document.metadata.slug.clone(),
        })
        .collect()
}

#[derive(Clone)]
struct SelectedPost {
    stable_post_id: PostId,
    revision: PostRevisionDigest,
    slug: PostSlug,
    aliases: Arc<[PostAlias]>,
}

fn select_post(
    catalog: &ContentCatalog,
    post_id: &PostId,
    expected_revision: Option<&PostRevisionDigest>,
) -> Result<SelectedPost, PublicationActivationError> {
    let rendered =
        catalog
            .current_post(post_id)
            .ok_or_else(|| PublicationActivationError::PostNotFound {
                post_id: post_id.clone(),
            })?;
    if let Some(expected) = expected_revision
        && expected != &rendered.revision
    {
        return Err(PublicationActivationError::StaleRevision {
            post_id: post_id.clone(),
            expected: Box::new(expected.clone()),
            current: Box::new(rendered.revision.clone()),
        });
    }
    if rendered.document.metadata.draft == DraftStatus::Draft {
        return Err(PublicationActivationError::DraftPost {
            post_id: post_id.clone(),
        });
    }
    Ok(SelectedPost {
        stable_post_id: rendered.document.metadata.id.clone(),
        revision: rendered.revision.clone(),
        slug: rendered.document.metadata.slug.clone(),
        aliases: rendered.document.metadata.aliases.clone().into(),
    })
}

fn require_update_precondition(
    ledger: &PublicLedgerProjection,
    selected: &SelectedPost,
    expected_revision: Option<&PostRevisionDigest>,
) -> Result<(), PublicationActivationError> {
    let Some(published) = ledger.published_post(&selected.stable_post_id) else {
        return Ok(());
    };
    if published.revision == selected.revision {
        return Err(PublicationActivationError::AlreadyPublished {
            post_id: selected.stable_post_id.clone(),
        });
    }
    if expected_revision.is_none() {
        return Err(PublicationActivationError::UpdateRevisionRequired {
            post_id: selected.stable_post_id.clone(),
        });
    }
    Ok(())
}

fn select_stored_post(
    catalog: &ContentCatalog,
    publication: &super::CanonicalPublicationView,
) -> Result<SelectedPost, PublicationActivationError> {
    let rendered = catalog
        .get(&publication.stable_post_id, &publication.pinned_post_digest)
        .ok_or(PublicationActivationError::DurableStateMismatch)?;
    if rendered.document.metadata.draft == DraftStatus::Draft {
        return Err(PublicationActivationError::DurableStateMismatch);
    }
    Ok(SelectedPost {
        stable_post_id: rendered.document.metadata.id.clone(),
        revision: rendered.revision.clone(),
        slug: rendered.document.metadata.slug.clone(),
        aliases: rendered.document.metadata.aliases.clone().into(),
    })
}

fn reproduce_preview_digest(
    catalog: &ContentCatalog,
    frontend: &'static FrontendAssetManifest,
    ledger: &PublicLedgerProjection,
    tip_recipient: Option<&TipRecipientProjection>,
    selected: &SelectedPost,
) -> Result<PreviewDigest, PublicationActivationError> {
    let published_at = ledger
        .published_post(&selected.stable_post_id)
        .map(|published| published.published_at);
    let preview = render_bound_post_revision_preview(
        catalog,
        frontend,
        &selected.stable_post_id,
        &selected.revision,
        tip_recipient,
        "/api/admin/v1/preview-assets/reproduce",
        published_at,
    )?
    .ok_or(PublicationActivationError::DurableStateMismatch)?;
    if preview.revision != selected.revision {
        return Err(PublicationActivationError::DurableStateMismatch);
    }
    Ok(preview.digest)
}

fn require_accepted_preview(
    current: PreviewDigest,
    accepted: &PreviewDigest,
) -> Result<(), PublicationActivationError> {
    if &current != accepted {
        return Err(PublicationActivationError::StalePreview {
            accepted: accepted.clone(),
            current,
        });
    }
    Ok(())
}

struct CandidateSnapshot {
    ledger: PublicLedgerProjection,
    snapshot: SiteSnapshot,
    digest: SiteSnapshotDigest,
    published_at: OffsetDateTime,
    already_published: bool,
}

fn build_candidate(
    catalog: Arc<ContentCatalog>,
    frontend: &'static FrontendAssetManifest,
    current: &PublicLedgerProjection,
    tip_recipient: Option<&TipRecipientProjection>,
    selected: &SelectedPost,
    published_at: OffsetDateTime,
) -> Result<CandidateSnapshot, PublicationActivationError> {
    let already_published = current
        .published_post(&selected.stable_post_id)
        .is_some_and(|published| published.revision == selected.revision);
    let ledger = if already_published {
        current.clone()
    } else {
        current.with_approved(PublishedPostRevision::new(
            selected.stable_post_id.clone(),
            selected.revision.clone(),
            published_at,
        ))
    };
    let shell =
        render_site_shell(catalog, frontend, &ledger)?.bind_tip_recipient(tip_recipient.cloned());
    let snapshot = build_site_snapshot(shell, &ledger)?;
    let digest = snapshot.digest.clone();
    Ok(CandidateSnapshot {
        ledger,
        snapshot,
        digest,
        published_at,
        already_published,
    })
}

fn validate_activating(
    publication: &super::CanonicalPublicationView,
    selected: &SelectedPost,
) -> Result<(), PublicationActivationError> {
    if publication.stable_post_id != selected.stable_post_id
        || publication.pinned_post_digest != selected.revision
        || publication.activation_started_at.is_none()
    {
        return Err(PublicationActivationError::DurableStateMismatch);
    }
    Ok(())
}

fn require_candidate_digest(
    candidate: &CandidateSnapshot,
    stored: &SiteSnapshotDigest,
) -> Result<(), PublicationActivationError> {
    if &candidate.digest != stored {
        return Err(PublicationActivationError::CandidateDigestMismatch {
            stored: stored.clone(),
            rebuilt: candidate.digest.clone(),
        });
    }
    Ok(())
}

fn validate_finished(
    finished: FinishedPublication,
    selected: &SelectedPost,
) -> Result<PublishedPublication, PublicationActivationError> {
    let current_matches = finished
        .publication
        .view()
        .current_published_digest
        .as_ref()
        == Some(&selected.revision);
    let result = finished_result(finished)?;
    if result.stable_post_id != selected.stable_post_id
        || result.revision != selected.revision
        || !current_matches
    {
        return Err(PublicationActivationError::DurableStateMismatch);
    }
    Ok(result)
}

fn validate_completed(
    completed: CompletedPublication,
    selected: &SelectedPost,
) -> Result<PublishedPublication, PublicationActivationError> {
    let result = completed_result(completed);
    if result.stable_post_id != selected.stable_post_id || result.revision != selected.revision {
        return Err(PublicationActivationError::DurableStateMismatch);
    }
    Ok(result)
}

fn finished_result(
    finished: FinishedPublication,
) -> Result<PublishedPublication, PublicationActivationError> {
    let view = finished.publication.view();
    let published_at = view
        .published_at
        .ok_or(PublicationActivationError::DurableStateMismatch)?;
    Ok(PublishedPublication {
        publication_id: finished.publication_id,
        stable_post_id: view.stable_post_id.clone(),
        revision: view.pinned_post_digest.clone(),
        accepted_preview_digest: finished.accepted_preview_digest,
        published_at,
        site: finished.site,
    })
}

fn completed_result(completed: CompletedPublication) -> PublishedPublication {
    PublishedPublication {
        publication_id: completed.publication_id,
        stable_post_id: completed.stable_post_id,
        revision: completed.revision,
        accepted_preview_digest: completed.accepted_preview_digest,
        published_at: completed.published_at,
        site: completed.site,
    }
}

fn canonical_publication_result(
    ledger: &PublicLedgerProjection,
    mut result: PublishedPublication,
) -> Result<PublishedPublication, PublicationActivationError> {
    result.published_at = ledger
        .published_post(&result.stable_post_id)
        .ok_or(PublicationActivationError::DurableStateMismatch)?
        .published_at;
    Ok(result)
}

fn definitely_unclaimed(error: &DatabaseMutationError) -> bool {
    matches!(
        error,
        DatabaseMutationError::Admission(_)
            | DatabaseMutationError::Command(
                DatabaseCommandError::IdempotencyConflict
                    | DatabaseCommandError::Rejected
                    | DatabaseCommandError::InvalidValue
            )
    )
}

fn publish_now_database_error(
    error: DatabaseMutationError,
    already_published: bool,
    post_id: PostId,
) -> PublicationActivationError {
    if already_published
        && matches!(
            &error,
            DatabaseMutationError::Command(DatabaseCommandError::Rejected)
        )
    {
        PublicationActivationError::AlreadyPublished { post_id }
    } else {
        PublicationActivationError::Database(error)
    }
}

fn validate_begun_publish_now(
    begun: &BegunPublication,
    prebuilt: &CandidateSnapshot,
    requested_content_digest: &ContentTreeDigest,
    accepted_preview_digest: &PreviewDigest,
) -> Result<(), PublicationActivationError> {
    if prebuilt.already_published
        || &begun.content_digest != requested_content_digest
        || &begun.accepted_preview_digest != accepted_preview_digest
    {
        return Err(PublicationActivationError::DurableStateMismatch);
    }
    Ok(())
}

struct FailClosedGuard {
    readiness: Readiness,
    cancellation: CancellationToken,
    armed: bool,
}

impl FailClosedGuard {
    fn new(readiness: &Readiness, cancellation: &CancellationToken) -> Self {
        Self {
            readiness: readiness.clone(),
            cancellation: cancellation.clone(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for FailClosedGuard {
    fn drop(&mut self) {
        if self.armed {
            self.readiness.mark_not_ready();
            self.cancellation.cancel();
        }
    }
}

/// Why a browser-reviewed publication no longer matches coordinator state.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum PublishReviewError {
    #[error("the content catalog changed after browser publication review")]
    Content {
        reviewed: Box<ContentTreeDigest>,
        current: Box<ContentTreeDigest>,
    },
    #[error("the public site head changed after browser publication review")]
    SiteHead {
        reviewed: Box<SiteHead>,
        current: Box<SiteHead>,
    },
    #[error("the public revision for post {post_id} changed after browser publication review")]
    PublicRevision {
        post_id: PostId,
        reviewed: Box<ReviewedPublicRevision>,
        current: Box<ReviewedPublicRevision>,
    },
}

#[derive(Debug, Error)]
pub(crate) enum PublicationActivationError {
    #[error(transparent)]
    Coordinator(#[from] PublicationCoordinatorUnavailable),
    #[error("post {post_id} is not present in the current content catalog")]
    PostNotFound { post_id: PostId },
    #[error("post {post_id} is still a draft")]
    DraftPost { post_id: PostId },
    #[error("publishing an update to post {post_id} requires expected_revision")]
    UpdateRevisionRequired { post_id: PostId },
    #[error("post {post_id} changed from expected revision {expected} to {current}")]
    StaleRevision {
        post_id: PostId,
        expected: Box<PostRevisionDigest>,
        current: Box<PostRevisionDigest>,
    },
    #[error("accepted preview {accepted} does not match current preview {current}")]
    StalePreview {
        accepted: PreviewDigest,
        current: PreviewDigest,
    },
    #[error(transparent)]
    StaleReview(#[from] PublishReviewError),
    #[error("post {post_id} is already published")]
    AlreadyPublished { post_id: PostId },
    #[error("scheduled publication {publication_id} is not retained by the coordinator")]
    ScheduledPublicationUnavailable { publication_id: Uuid },
    #[error("scheduled publication time {scheduled_at} is not later than {now}")]
    ScheduleNotFuture {
        scheduled_at: OffsetDateTime,
        now: OffsetDateTime,
    },
    #[error("the durable publication state does not match the in-process candidate")]
    DurableStateMismatch,
    #[error("the active in-process site snapshot changed during publication")]
    SnapshotActivationConflict,
    #[error("the rebuilt site digest {rebuilt} does not match stored candidate {stored}")]
    CandidateDigestMismatch {
        stored: SiteSnapshotDigest,
        rebuilt: SiteSnapshotDigest,
    },
    #[error(transparent)]
    SnapshotBuild(#[from] SiteSnapshotBuildError),
    #[error(transparent)]
    Lookup(#[from] PublishNowLookupError),
    #[error(transparent)]
    ScheduleLookup(#[from] SchedulePublicationLookupError),
    #[error(transparent)]
    RouteOwnership(#[from] PublicationRouteOwnershipError),
    #[error(transparent)]
    Database(#[from] DatabaseMutationError),
}

#[derive(Debug, Error)]
pub(crate) enum ContentReloadError {
    #[error(transparent)]
    Coordinator(#[from] PublicationCoordinatorUnavailable),
    #[error("the candidate preview could not retain the approved public revisions")]
    Retention(#[from] CatalogRetentionError),
    #[error("the database rejected the candidate preview catalog")]
    Database(#[from] DatabaseMutationError),
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::Path};

    use maincopy_shared::{
        auth::{
            AdminAuditEventId, AdminSessionId, HumanLoginProvider, InstanceId, LoginChallengeId,
            UserId, UserRole, UserStatus,
        },
        profile::{LightningAddress, ProfileDisplayName, ProfileVersion},
    };
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use time::format_description::well_known::Rfc2822;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::{
        config::{
            DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
            DatabaseWriterQueueCapacity,
        },
        database,
        domain::{
            auth::{
                CsrfTokenDigest, LoginChallengeDigest, Nip98EventId, NostrPublicKey,
                SessionTokenDigest,
                store::{
                    AdminMutationKey, AuditPrincipalReference, BootstrapIdentity,
                    ConfiguredLoginProviders, CreateBrowserSession, CreateLoginChallenge,
                    CreateUser, MutationAuditContext, NewHumanCredential, SessionAuditContext,
                    SessionAuthenticationEvidence,
                },
            },
            profile::ProfilePrecondition,
            publication::store::{InstallStartupSnapshot, ObservedPostRevision, PublicationRoute},
        },
        frontend_assets::embedded_manifest,
        render::{
            SiteSnapshotReader, SnapshotAssetPath, SnapshotPublicAsset, compile_content_catalog,
            snapshot_store,
        },
    };
    use markdown_compiler::{
        DiscoveredPost, LogicalAssetPath, PostCollection, resolve_content_assets,
    };

    use crate::content_fixtures::{asset, content_tree, post, publication};

    const PUBLISHABLE_ID: &str = "11111111-1111-4111-8111-111111111111";
    const DRAFT_ID: &str = "22222222-2222-4222-8222-222222222222";
    const OTHER_PUBLISHABLE_ID: &str = "33333333-3333-4333-8333-333333333333";
    const OWNER_NOSTR_KEY: &str =
        "63fe6318dc58583cfe16810f86dd09e18bfd76aabc24a0081ce2856f330504ed";
    const RECIPIENT_NOSTR_KEY: &str =
        "c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5";

    fn catalog() -> Arc<ContentCatalog> {
        compile_catalog(vec![
            post(
                "posts/publishable.md",
                PostCollection::Posts,
                post_source(PUBLISHABLE_ID, "publishable", false),
            ),
            post(
                "drafts/draft.md",
                PostCollection::Drafts,
                post_source(DRAFT_ID, "draft", true),
            ),
        ])
    }

    fn catalog_without_publishable_post() -> Arc<ContentCatalog> {
        compile_catalog(vec![post(
            "drafts/draft.md",
            PostCollection::Drafts,
            post_source(DRAFT_ID, "draft", true),
        )])
    }

    fn catalog_with_alias(alias: &str) -> Arc<ContentCatalog> {
        compile_catalog(vec![
            post(
                "posts/publishable.md",
                PostCollection::Posts,
                post_source_with_alias(PUBLISHABLE_ID, "publishable", Some(alias)),
            ),
            post(
                "drafts/draft.md",
                PostCollection::Drafts,
                post_source(DRAFT_ID, "draft", true),
            ),
        ])
    }

    fn revised_catalog() -> Arc<ContentCatalog> {
        compile_catalog(vec![
            post(
                "posts/publishable.md",
                PostCollection::Posts,
                post_source_with_metadata(
                    PUBLISHABLE_ID,
                    "Revised publishable",
                    "revised-publishable",
                    "Revised activation summary.",
                    false,
                )
                .replace("Publication body.", "Revised publication body."),
            ),
            post(
                "drafts/draft.md",
                PostCollection::Drafts,
                post_source(DRAFT_ID, "draft", true),
            ),
        ])
    }

    fn route_ownership_catalog(
        first_alias: Option<&str>,
        second_alias: Option<&str>,
        first_body: &str,
    ) -> Arc<ContentCatalog> {
        compile_catalog(vec![
            post(
                "posts/first.md",
                PostCollection::Posts,
                post_source_with_alias(PUBLISHABLE_ID, "first", first_alias)
                    .replace("Publication body.", first_body),
            ),
            post(
                "posts/second.md",
                PostCollection::Posts,
                post_source_with_alias(OTHER_PUBLISHABLE_ID, "second", second_alias),
            ),
        ])
    }

    fn tips_catalog(body: &str) -> Arc<ContentCatalog> {
        let tree = content_tree(
            publication(
                "publication.toml",
                "[site]\n\
                 title = \"Activation tips tests\"\n\
                 base_url = \"https://example.com/\"\n\
                 description = \"Activation tips tests.\"\n\
                 [author]\n\
                 name = \"Example Author\"\n\
                 [tips]\n\
                 enabled = true\n\
                 [assets]\n\
                 allowed_https_origins = []\n"
                    .to_owned(),
            ),
            vec![post(
                "posts/publishable.md",
                PostCollection::Posts,
                post_source(PUBLISHABLE_ID, "publishable", false)
                    .replace("Publication body.", body),
            )],
            Vec::new(),
            0,
        );
        let content = tree.validate().unwrap();
        let assets = resolve_content_assets(&tree, &content).unwrap();
        Arc::new(compile_content_catalog(&content, &assets).unwrap())
    }

    fn compile_catalog(posts: Vec<DiscoveredPost>) -> Arc<ContentCatalog> {
        let tree = content_tree(
            publication(
                "publication.toml",
                "[site]\n\
                 title = \"Activation tests\"\n\
                 base_url = \"https://example.com/\"\n\
                 description = \"Activation tests.\"\n\
                 [author]\n\
                 name = \"Example Author\"\n\
                 [assets]\n\
                 allowed_https_origins = []\n"
                    .to_owned(),
            ),
            posts,
            Vec::new(),
            0,
        );
        let content = tree.validate().unwrap();
        let assets = resolve_content_assets(&tree, &content).unwrap();
        Arc::new(compile_content_catalog(&content, &assets).unwrap())
    }

    fn catalog_with_site_candidate(title: &str, favicon_bytes: &[u8]) -> Arc<ContentCatalog> {
        let tree = content_tree(
            publication(
                "publication.toml",
                format!(
                    "[site]\n\
                     title = {title:?}\n\
                     base_url = \"https://example.com/\"\n\
                     description = \"Candidate-bound activation fixture.\"\n\
                     favicon = \"assets/favicon.png\"\n\
                     [author]\n\
                     name = \"Example Author\"\n\
                     [assets]\n\
                     allowed_https_origins = []\n"
                ),
            ),
            vec![post(
                "posts/publishable.md",
                PostCollection::Posts,
                post_source(PUBLISHABLE_ID, "publishable", false),
            )],
            vec![asset(
                LogicalAssetPath::parse("assets/favicon.png").unwrap(),
                favicon_bytes.to_vec(),
            )],
            0,
        );
        let content = tree.validate().unwrap();
        let assets = resolve_content_assets(&tree, &content).unwrap();
        Arc::new(compile_content_catalog(&content, &assets).unwrap())
    }

    fn post_source(id: &str, slug: &str, draft: bool) -> String {
        post_source_with_metadata(id, slug, slug, "Activation fixture.", draft)
    }

    fn post_source_with_alias(id: &str, slug: &str, alias: Option<&str>) -> String {
        let source = post_source(id, slug, false);
        match alias {
            Some(alias) => source.replace(
                "authored_at = 2026-08-29T15:00:00-04:00\n",
                &format!("authored_at = 2026-08-29T15:00:00-04:00\naliases = [{alias:?}]\n"),
            ),
            None => source,
        }
    }

    fn post_source_with_metadata(
        id: &str,
        title: &str,
        slug: &str,
        description: &str,
        draft: bool,
    ) -> String {
        format!(
            "+++\n\
             id = {id:?}\n\
             title = {title:?}\n\
             slug = {slug:?}\n\
             authored_at = 2026-08-29T15:00:00-04:00\n\
             description = {description:?}\n\
             draft = {draft}\n\
             +++\n\
             Publication body.\n"
        )
    }

    fn snapshot(catalog: &Arc<ContentCatalog>, ledger: &PublicLedgerProjection) -> SiteSnapshot {
        let shell = render_site_shell(Arc::clone(catalog), embedded_manifest(), ledger).unwrap();
        build_site_snapshot(shell, ledger).unwrap()
    }

    fn snapshot_asset<'snapshot>(
        snapshot: &'snapshot SiteSnapshot,
        logical_path: &str,
    ) -> &'snapshot SnapshotPublicAsset {
        let logical_path = LogicalAssetPath::parse(logical_path).unwrap();
        let public_path = SnapshotAssetPath::new(&snapshot.digest, &logical_path).unwrap();
        snapshot.public_asset(&public_path).unwrap()
    }

    fn candidate_catalogs(
        catalog: &Arc<ContentCatalog>,
        digest: ContentTreeDigest,
    ) -> Arc<BTreeMap<ContentTreeDigest, Arc<ContentCatalog>>> {
        Arc::new(BTreeMap::from([(digest, Arc::clone(catalog))]))
    }

    fn observed_posts(catalog: &ContentCatalog) -> Vec<ObservedPostRevision> {
        catalog
            .rendered_posts()
            .map(|post| ObservedPostRevision {
                stable_post_id: post.document.metadata.id.clone(),
                revision_digest: post.revision.clone(),
                publication_status: post.document.metadata.draft,
                slug: post.document.metadata.slug.clone(),
            })
            .collect()
    }

    async fn start_store(
        catalog: &ContentCatalog,
        initial_digest: SiteSnapshotDigest,
    ) -> (
        tempfile::TempDir,
        PublicationStore,
        ProfileStore,
        AuthStore,
        SiteHead,
        CancellationToken,
        JoinHandle<()>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/maincopy.db");
        let database = database::bootstrap(database_configuration(&path))
            .await
            .unwrap();
        let (store, writer) = database.into_store(8);
        let shutdown = CancellationToken::new();
        let writer_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            writer.run(writer_shutdown).await.unwrap();
        });
        let auth = store.auth;
        let profiles = store.profiles;
        let publications = store.publications;
        let head = publications
            .install_startup_snapshot(InstallStartupSnapshot {
                expected: None,
                candidate_digest: initial_digest,
                activated_at: OffsetDateTime::from_unix_timestamp(1_000).unwrap(),
                source_commit: None,
                posts: observed_posts(catalog),
            })
            .await
            .unwrap();
        (root, publications, profiles, auth, head, shutdown, task)
    }

    async fn profile_transition_fixture(
        catalog: Arc<ContentCatalog>,
    ) -> (
        tempfile::TempDir,
        PublicationCoordinator,
        AuthStore,
        SiteSnapshotReader,
        CancellationToken,
        JoinHandle<()>,
    ) {
        let ledger = PublicLedgerProjection::empty();
        let initial = snapshot(&catalog, &ledger);
        let initial_digest = initial.digest.clone();
        let (root, store, profiles, auth, head, writer_shutdown, writer_task) =
            start_store(&catalog, initial_digest).await;
        let (reader, activator) = snapshot_store(initial);
        let content_digest = ContentTreeDigest::from_bytes([0x11; 32]);
        let coordinator = PublicationCoordinator {
            catalog: Arc::clone(&catalog),
            content_digest: content_digest.clone(),
            candidates: candidate_catalogs(&catalog, content_digest),
            ledger,
            site: head,
            activator,
            store,
            profiles,
            tip_recipient: None,
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(Notify::new()),
            readiness: Readiness::new(true),
            cancellation: CancellationToken::new(),
        };
        (
            root,
            coordinator,
            auth,
            reader,
            writer_shutdown,
            writer_task,
        )
    }

    fn database_configuration(path: &Path) -> DatabaseConfigurationView<'_> {
        DatabaseConfigurationView {
            path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1_000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(8).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        }
    }

    #[derive(Clone, Copy)]
    struct TipStatusUsers {
        owner: UserId,
        recipient: UserId,
        providers: ConfiguredLoginProviders,
    }

    fn fixture_uuid(discriminator: u128) -> Uuid {
        Uuid::from_u128(0xaaaa_aaaa_aaaa_4aaa_8aaa_0000_0000_0000 | discriminator)
    }

    fn fixture_time(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    fn mutation_audit(actor: UserId, discriminator: u128) -> MutationAuditContext {
        MutationAuditContext {
            audit_event_id: AdminAuditEventId::from_uuid(fixture_uuid(100 + discriminator)),
            principal: AuditPrincipalReference::BrowserSession {
                user_id: actor,
                session_id: actor_session_id(actor),
            },
            request_id: Some(fixture_uuid(300 + discriminator)),
            idempotency_key: AdminMutationKey(fixture_uuid(400 + discriminator)),
        }
    }

    fn actor_session_id(actor: UserId) -> AdminSessionId {
        let discriminator = if actor.as_uuid() == &fixture_uuid(1) {
            201
        } else {
            202
        };
        AdminSessionId::from_uuid(fixture_uuid(discriminator))
    }

    async fn create_nostr_session(
        auth: &AuthStore,
        user_id: UserId,
        discriminator: u8,
        authenticated_at: OffsetDateTime,
    ) {
        let challenge_id =
            LoginChallengeId::from_uuid(fixture_uuid(700 + u128::from(discriminator)));
        let challenge_digest = LoginChallengeDigest::parse_bytes(&[discriminator; 32]).unwrap();
        auth.create_login_challenge(CreateLoginChallenge {
            challenge_id,
            provider: HumanLoginProvider::Nostr,
            challenge_digest,
            created_at: authenticated_at - time::Duration::seconds(10),
            expires_at: authenticated_at + time::Duration::minutes(1),
        })
        .await
        .unwrap();
        auth.create_browser_session(CreateBrowserSession {
            session_id: actor_session_id(user_id),
            user_id,
            expected_user_version: 1,
            session_token_digest: SessionTokenDigest::parse_bytes(&[discriminator + 10; 32])
                .unwrap(),
            csrf_token_digest: CsrfTokenDigest::parse_bytes(&[discriminator + 20; 32]).unwrap(),
            evidence: SessionAuthenticationEvidence::Nostr {
                expected_credential_version: 1,
                challenge_id,
                challenge_digest,
                event_id: Nip98EventId::parse_bytes(&[discriminator + 30; 32]).unwrap(),
                proof_created_at: authenticated_at,
            },
            authenticated_at,
            fresh_until: authenticated_at + time::Duration::hours(1),
            expires_at: authenticated_at + time::Duration::hours(2),
            audit: SessionAuditContext {
                audit_event_id: AdminAuditEventId::from_uuid(fixture_uuid(
                    800 + u128::from(discriminator),
                )),
                request_id: Some(fixture_uuid(900 + u128::from(discriminator))),
            },
        })
        .await
        .unwrap();
    }

    async fn seed_tip_recipient(
        coordinator: &mut PublicationCoordinator,
        auth: &AuthStore,
    ) -> TipStatusUsers {
        let owner = UserId::from_uuid(fixture_uuid(1));
        let recipient = UserId::from_uuid(fixture_uuid(2));
        let providers = ConfiguredLoginProviders::new(false, true).unwrap();
        auth.bootstrap_identity(BootstrapIdentity {
            instance_id: InstanceId::from_uuid(fixture_uuid(3)),
            owner_user_id: owner,
            credential: NewHumanCredential::Nostr {
                public_key: NostrPublicKey::parse(OWNER_NOSTR_KEY).unwrap(),
            },
            configured_providers: providers,
            occurred_at: fixture_time(2_000),
            audit_event_id: AdminAuditEventId::from_uuid(fixture_uuid(4)),
        })
        .await
        .unwrap();
        create_nostr_session(auth, owner, 1, fixture_time(2_050)).await;
        auth.create_user(CreateUser {
            user_id: recipient,
            created_by_user_id: owner,
            status: UserStatus::Enabled,
            roles: BTreeSet::from([UserRole::Administrator]),
            credentials: vec![NewHumanCredential::Nostr {
                public_key: NostrPublicKey::parse(RECIPIENT_NOSTR_KEY).unwrap(),
            }],
            configured_providers: providers,
            occurred_at: fixture_time(2_100),
            audit: mutation_audit(owner, 1),
        })
        .await
        .unwrap();
        create_nostr_session(auth, recipient, 2, fixture_time(2_150)).await;
        coordinator
            .update_profile(UpdateProfile {
                user_id: recipient,
                precondition: ProfilePrecondition::Create,
                display_name: Some(ProfileDisplayName::parse("Tip Recipient").unwrap()),
                lightning_address: Some(LightningAddress::parse("tips@example.com").unwrap()),
                tips_enabled: true,
                occurred_at: fixture_time(2_200),
                audit: mutation_audit(recipient, 2),
            })
            .await
            .unwrap();
        coordinator
            .set_tip_recipient(SetTipRecipient {
                expected_version: ProfileVersion::new(1).unwrap(),
                recipient_user_id: Some(recipient),
                occurred_at: fixture_time(2_300),
                audit: mutation_audit(owner, 3),
            })
            .await
            .unwrap();
        assert!(coordinator.tip_recipient.is_some());
        TipStatusUsers {
            owner,
            recipient,
            providers,
        }
    }

    fn disable_recipient(users: TipStatusUsers, discriminator: u128) -> SetUserStatus {
        SetUserStatus {
            user_id: users.recipient,
            changed_by_user_id: users.owner,
            expected_version: 1,
            status: UserStatus::Disabled,
            configured_providers: users.providers,
            occurred_at: fixture_time(2_400),
            audit: mutation_audit(users.owner, discriminator),
        }
    }

    fn replace_recipient_address(
        users: TipStatusUsers,
        address: &str,
        discriminator: u128,
    ) -> UpdateProfile {
        UpdateProfile {
            user_id: users.recipient,
            precondition: ProfilePrecondition::Replace(ProfileVersion::new(1).unwrap()),
            display_name: Some(ProfileDisplayName::parse("Tip Recipient").unwrap()),
            lightning_address: Some(LightningAddress::parse(address).unwrap()),
            tips_enabled: true,
            occurred_at: fixture_time(2_500),
            audit: mutation_audit(users.recipient, discriminator),
        }
    }

    fn command() -> PublishNow {
        let catalog = catalog();
        let ledger = PublicLedgerProjection::empty();
        PublishNow {
            creation_key: Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap(),
            publication_id: Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap(),
            stable_post_id: PostId::parse(PUBLISHABLE_ID).unwrap(),
            expected_revision: None,
            accepted_preview_digest: preview_digest(&catalog, &ledger),
        }
    }

    fn schedule_command_for(
        catalog: &ContentCatalog,
        ledger: &PublicLedgerProjection,
        scheduled_at: OffsetDateTime,
    ) -> Schedule {
        Schedule {
            creation_key: Uuid::parse_str("cccccccc-cccc-4ccc-8ccc-cccccccccccc").unwrap(),
            publication_id: Uuid::parse_str("dddddddd-dddd-4ddd-8ddd-dddddddddddd").unwrap(),
            stable_post_id: PostId::parse(PUBLISHABLE_ID).unwrap(),
            expected_revision: None,
            accepted_preview_digest: preview_digest(catalog, ledger),
            scheduled_at,
        }
    }

    fn preview_digest(catalog: &ContentCatalog, ledger: &PublicLedgerProjection) -> PreviewDigest {
        let selected = select_post(catalog, &PostId::parse(PUBLISHABLE_ID).unwrap(), None).unwrap();
        reproduce_preview_digest(catalog, embedded_manifest(), ledger, None, &selected).unwrap()
    }

    fn publish_command_for(
        catalog: &ContentCatalog,
        ledger: &PublicLedgerProjection,
        post_id: &str,
        expected_revision: Option<PostRevisionDigest>,
        discriminator: u128,
    ) -> PublishNow {
        let post_id = PostId::parse(post_id).unwrap();
        let selected = select_post(catalog, &post_id, expected_revision.as_ref()).unwrap();
        PublishNow {
            creation_key: fixture_uuid(1_000 + discriminator),
            publication_id: fixture_uuid(2_000 + discriminator),
            stable_post_id: post_id,
            expected_revision,
            accepted_preview_digest: reproduce_preview_digest(
                catalog,
                embedded_manifest(),
                ledger,
                None,
                &selected,
            )
            .unwrap(),
        }
    }

    fn reviewed_publish_command(
        coordinator: &PublicationCoordinator,
        publication: PublishNow,
    ) -> PublishReviewedNow {
        let expected_public_revision = match coordinator
            .ledger
            .published_post(&publication.stable_post_id)
        {
            Some(published) => ReviewedPublicRevision::Published {
                revision: published.revision.clone(),
            },
            None => ReviewedPublicRevision::Unpublished,
        };
        PublishReviewedNow {
            publication,
            expected_content_digest: coordinator.content_digest.clone(),
            expected_site: coordinator.site.clone(),
            expected_public_revision,
        }
    }

    async fn coordinator_fixture(
        catalog: Arc<ContentCatalog>,
    ) -> (
        tempfile::TempDir,
        PublicationCoordinator,
        SiteSnapshotReader,
        PublicationStore,
        CancellationToken,
        JoinHandle<()>,
    ) {
        let ledger = PublicLedgerProjection::empty();
        let initial = snapshot(&catalog, &ledger);
        let initial_digest = initial.digest.clone();
        let (root, store, profiles, _auth, head, writer_shutdown, writer_task) =
            start_store(&catalog, initial_digest).await;
        let (reader, activator) = snapshot_store(initial);
        let content_digest = ContentTreeDigest::from_bytes([0x11; 32]);
        let writer_keepalive = store.clone();
        let coordinator = PublicationCoordinator {
            catalog: Arc::clone(&catalog),
            content_digest: content_digest.clone(),
            candidates: candidate_catalogs(&catalog, content_digest),
            ledger,
            site: head,
            activator,
            store,
            profiles,
            tip_recipient: None,
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(Notify::new()),
            readiness: Readiness::new(true),
            cancellation: CancellationToken::new(),
        };
        (
            root,
            coordinator,
            reader,
            writer_keepalive,
            writer_shutdown,
            writer_task,
        )
    }

    async fn wait_for_full_mailbox(handle: &PublicationCoordinatorHandle) {
        wait_for_mailbox_capacity(handle, 0).await;
    }

    async fn wait_for_mailbox_capacity(
        handle: &PublicationCoordinatorHandle,
        expected_capacity: usize,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while handle.commands.capacity() != expected_capacity {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("commands should reach the expected bounded mailbox capacity");
    }

    #[tokio::test]
    async fn actor_publishes_coherent_lock_free_read_projections() {
        let initial_catalog = catalog();
        let initial_revision = select_post(
            &initial_catalog,
            &PostId::parse(PUBLISHABLE_ID).unwrap(),
            None,
        )
        .unwrap()
        .revision;
        let (root, coordinator, _reader, writer_keepalive, writer_shutdown, writer_task) =
            coordinator_fixture(initial_catalog).await;
        let (handle, actor) = coordinator.into_actor(2);
        let before = handle.read();
        let actor_cancellation = CancellationToken::new();
        let actor_task = tokio::spawn(actor.run(actor_cancellation.clone()));

        let revised = revised_catalog();
        let revised_revision = select_post(&revised, &PostId::parse(PUBLISHABLE_ID).unwrap(), None)
            .unwrap()
            .revision;
        let revised_digest = ContentTreeDigest::from_bytes([0x22; 32]);
        handle
            .apply_content_catalog(revised, revised_digest.clone(), None)
            .await
            .unwrap();

        let after = handle.read();
        assert_eq!(
            before.content_digest,
            ContentTreeDigest::from_bytes([0x11; 32])
        );
        assert_eq!(after.content_digest, revised_digest);
        assert_eq!(before.site, after.site);
        assert_eq!(before.ledger, after.ledger);
        assert_eq!(
            before
                .catalog
                .current_post(&PostId::parse(PUBLISHABLE_ID).unwrap())
                .unwrap()
                .revision,
            initial_revision
        );
        assert_eq!(
            after
                .catalog
                .current_post(&PostId::parse(PUBLISHABLE_ID).unwrap())
                .unwrap()
                .revision,
            revised_revision
        );

        actor_cancellation.cancel();
        actor_task.await.unwrap().unwrap();
        drop(handle);
        writer_shutdown.cancel();
        writer_task.await.unwrap();
        drop(writer_keepalive);
        drop(root);
    }

    #[tokio::test]
    async fn bounded_actor_mailbox_preserves_fifo_command_order() {
        let (root, coordinator, _reader, writer_keepalive, writer_shutdown, writer_task) =
            coordinator_fixture(catalog()).await;
        let (handle, actor) = coordinator.into_actor(1);
        let first_digest = ContentTreeDigest::from_bytes([0x22; 32]);
        let second_digest = ContentTreeDigest::from_bytes([0x33; 32]);
        let first = tokio::spawn({
            let handle = handle.clone();
            let first_digest = first_digest.clone();
            async move {
                handle
                    .apply_content_catalog(revised_catalog(), first_digest, None)
                    .await
            }
        });
        wait_for_full_mailbox(&handle).await;
        let second = tokio::spawn({
            let handle = handle.clone();
            let second_digest = second_digest.clone();
            async move {
                handle
                    .apply_content_catalog(catalog_without_publishable_post(), second_digest, None)
                    .await
            }
        });
        tokio::task::yield_now().await;
        assert!(
            !second.is_finished(),
            "the second command must await bounded admission"
        );

        let actor_cancellation = CancellationToken::new();
        let actor_task = tokio::spawn(actor.run(actor_cancellation.clone()));
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        let projection = handle.read();
        assert_eq!(projection.content_digest, second_digest);
        assert!(
            projection
                .catalog
                .current_post(&PostId::parse(PUBLISHABLE_ID).unwrap())
                .is_none()
        );

        actor_cancellation.cancel();
        actor_task.await.unwrap().unwrap();
        drop(handle);
        writer_shutdown.cancel();
        writer_task.await.unwrap();
        drop(writer_keepalive);
        drop(root);
    }

    #[tokio::test]
    async fn actor_rejects_a_review_queued_behind_a_public_head_change() {
        let catalog = route_ownership_catalog(None, None, "Publication body.");
        let (root, coordinator, _reader, store, writer_shutdown, writer_task) =
            coordinator_fixture(catalog).await;
        let head_change = publish_command_for(
            &coordinator.catalog,
            &coordinator.ledger,
            OTHER_PUBLISHABLE_ID,
            None,
            580,
        );
        let reviewed = reviewed_publish_command(
            &coordinator,
            publish_command_for(
                &coordinator.catalog,
                &coordinator.ledger,
                PUBLISHABLE_ID,
                None,
                581,
            ),
        );
        let reviewed_post_id = reviewed.publication.stable_post_id.clone();
        let changed_post_id = head_change.stable_post_id.clone();
        let (handle, actor) = coordinator.into_actor(1);

        let change = tokio::spawn({
            let handle = handle.clone();
            async move { handle.publish_now(head_change).await }
        });
        wait_for_full_mailbox(&handle).await;
        let publication = tokio::spawn({
            let handle = handle.clone();
            async move { handle.publish_reviewed_now(reviewed).await }
        });
        tokio::task::yield_now().await;
        assert!(
            !publication.is_finished(),
            "the reviewed publication must remain behind the accepted head change"
        );

        let actor_cancellation = CancellationToken::new();
        let actor_task = tokio::spawn(actor.run(actor_cancellation.clone()));
        change.await.unwrap().unwrap();
        assert!(matches!(
            publication.await.unwrap(),
            Err(PublicationActivationError::StaleReview(
                PublishReviewError::SiteHead { .. }
            ))
        ));
        let projection = handle.read();
        assert!(projection.ledger.published_post(&changed_post_id).is_some());
        assert!(
            projection
                .ledger
                .published_post(&reviewed_post_id)
                .is_none()
        );
        assert!(
            store
                .startup_snapshot_state()
                .await
                .unwrap()
                .activating
                .is_empty()
        );

        actor_cancellation.cancel();
        actor_task.await.unwrap().unwrap();
        drop(handle);
        writer_shutdown.cancel();
        writer_task.await.unwrap();
        drop(store);
        drop(root);
    }

    #[tokio::test]
    async fn actor_shutdown_closes_admission_and_drains_an_accepted_command() {
        let (root, coordinator, _reader, writer_keepalive, writer_shutdown, writer_task) =
            coordinator_fixture(catalog()).await;
        let (handle, actor) = coordinator.into_actor(1);
        let accepted_digest = ContentTreeDigest::from_bytes([0x44; 32]);
        let accepted = tokio::spawn({
            let handle = handle.clone();
            let accepted_digest = accepted_digest.clone();
            async move {
                handle
                    .apply_content_catalog(revised_catalog(), accepted_digest, None)
                    .await
            }
        });
        wait_for_full_mailbox(&handle).await;

        let actor_cancellation = CancellationToken::new();
        actor_cancellation.cancel();
        let actor_task = tokio::spawn(actor.run(actor_cancellation));
        accepted.await.unwrap().unwrap();
        actor_task.await.unwrap().unwrap();
        assert_eq!(handle.read().content_digest, accepted_digest);
        assert!(matches!(
            handle
                .apply_content_catalog(
                    catalog_without_publishable_post(),
                    ContentTreeDigest::from_bytes([0x55; 32]),
                    None,
                )
                .await,
            Err(ContentReloadError::Coordinator(
                PublicationCoordinatorUnavailable::Closed
            ))
        ));

        drop(handle);
        writer_shutdown.cancel();
        writer_task.await.unwrap();
        drop(writer_keepalive);
        drop(root);
    }

    #[tokio::test]
    async fn status_post_commit_presentation_failure_marks_the_service_unready_and_cancels_it() {
        let catalog = catalog();
        let ledger = PublicLedgerProjection::empty();
        let initial = snapshot(&catalog, &ledger);
        let initial_digest = initial.digest.clone();
        let (root, store, profiles, auth, head, writer_shutdown, writer_task) =
            start_store(&catalog, initial_digest).await;
        let (_, activator) = snapshot_store(initial);
        let content_digest = ContentTreeDigest::from_bytes([0x11; 32]);
        let mut coordinator = PublicationCoordinator {
            catalog: Arc::clone(&catalog),
            content_digest: content_digest.clone(),
            candidates: candidate_catalogs(&catalog, content_digest),
            ledger,
            site: head,
            activator,
            store,
            profiles,
            tip_recipient: None,
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(Notify::new()),
            readiness: Readiness::new(true),
            cancellation: CancellationToken::new(),
        };
        let users = seed_tip_recipient(&mut coordinator, &auth).await;
        let readiness = coordinator.readiness.clone();
        let cancellation = coordinator.cancellation.clone();
        let conflicting_catalog = catalog_with_site_candidate("Conflicting site", b"different");
        let conflicting_snapshot = snapshot(&conflicting_catalog, &PublicLedgerProjection::empty());
        let (_, conflicting_activator) = snapshot_store(conflicting_snapshot);
        coordinator.activator = conflicting_activator;

        let (handle, actor) = coordinator.into_actor(1);
        let actor_task = tokio::spawn(actor.run(cancellation.clone()));

        assert!(matches!(
            handle
                .set_user_status(auth.clone(), disable_recipient(users, 4))
                .await,
            Err(UserStatusTransitionError::Presentation(
                ProfileTransitionError::SnapshotActivationConflict
            ))
        ));
        assert_eq!(
            auth.user(users.recipient).await.unwrap().unwrap().status,
            UserStatus::Disabled
        );
        assert!(!readiness.is_ready());
        assert!(cancellation.is_cancelled());

        actor_task.await.unwrap().unwrap();
        drop(handle);
        writer_shutdown.cancel();
        writer_task.await.unwrap();
        drop(auth);
        drop(root);
    }

    #[tokio::test]
    async fn disabling_selected_recipient_is_a_fifo_barrier_for_reload_and_activation() {
        let catalog = tips_catalog("Publication body.");
        let ledger = PublicLedgerProjection::empty();
        let initial = snapshot(&catalog, &ledger);
        let initial_digest = initial.digest.clone();
        let (root, store, profiles, auth, head, writer_shutdown, writer_task) =
            start_store(&catalog, initial_digest).await;
        let (reader, activator) = snapshot_store(initial);
        let content_digest = ContentTreeDigest::from_bytes([0x11; 32]);
        let mut coordinator = PublicationCoordinator {
            catalog: Arc::clone(&catalog),
            content_digest: content_digest.clone(),
            candidates: candidate_catalogs(&catalog, content_digest),
            ledger,
            site: head,
            activator,
            store,
            profiles,
            tip_recipient: None,
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(Notify::new()),
            readiness: Readiness::new(true),
            cancellation: CancellationToken::new(),
        };
        let users = seed_tip_recipient(&mut coordinator, &auth).await;
        let selected = select_post(
            &coordinator.catalog,
            &PostId::parse(PUBLISHABLE_ID).unwrap(),
            None,
        )
        .unwrap();
        let accepted_preview_digest = reproduce_preview_digest(
            &coordinator.catalog,
            embedded_manifest(),
            &coordinator.ledger,
            coordinator.tip_recipient.as_ref(),
            &selected,
        )
        .unwrap();
        let stable_post_id = selected.stable_post_id.clone();
        let published_slug = selected.slug.clone();
        coordinator
            .publish_now(PublishNow {
                creation_key: fixture_uuid(500),
                publication_id: fixture_uuid(501),
                stable_post_id,
                expected_revision: None,
                accepted_preview_digest,
            })
            .await
            .unwrap();
        assert!(
            reader
                .load_full()
                .post_page(&published_slug)
                .unwrap()
                .contains("class=\"tip-cta\"")
        );

        let revised = tips_catalog("Revised publication body.");
        let revised_digest = ContentTreeDigest::from_bytes([0x22; 32]);
        let mut retained = revised.as_ref().clone();
        retained
            .retain_revisions_from(&coordinator.catalog, coordinator.ledger.revision_keys())
            .unwrap();
        let retained = Arc::new(retained);
        let selected =
            select_post(&retained, &PostId::parse(PUBLISHABLE_ID).unwrap(), None).unwrap();
        let accepted_preview_digest = reproduce_preview_digest(
            &retained,
            embedded_manifest(),
            &coordinator.ledger,
            None,
            &selected,
        )
        .unwrap();
        let update = PublishNow {
            creation_key: fixture_uuid(502),
            publication_id: fixture_uuid(503),
            stable_post_id: selected.stable_post_id.clone(),
            expected_revision: Some(selected.revision.clone()),
            accepted_preview_digest,
        };
        let readiness = coordinator.readiness.clone();
        let service_cancellation = coordinator.cancellation.clone();
        let (handle, actor) = coordinator.into_actor(2);

        let status = tokio::spawn({
            let handle = handle.clone();
            let auth = auth.clone();
            async move {
                handle
                    .set_user_status(auth, disable_recipient(users, 5))
                    .await
            }
        });
        wait_for_mailbox_capacity(&handle, 1).await;
        let reload = tokio::spawn({
            let handle = handle.clone();
            let revised_digest = revised_digest.clone();
            async move {
                handle
                    .apply_content_catalog(revised, revised_digest, None)
                    .await
            }
        });
        wait_for_full_mailbox(&handle).await;
        let activation = tokio::spawn({
            let handle = handle.clone();
            async move { handle.publish_now(update).await }
        });
        tokio::task::yield_now().await;
        assert!(!activation.is_finished());

        let actor_cancellation = CancellationToken::new();
        let actor_task = tokio::spawn(actor.run(actor_cancellation.clone()));
        let result = status.await.unwrap().unwrap();
        assert_eq!(result.user_id, users.recipient);
        assert_eq!(result.version, 2);
        reload.await.unwrap().unwrap();
        activation.await.unwrap().unwrap();

        let projection = handle.read();
        assert!(projection.tip_recipient.is_none());
        assert!(
            !reader
                .load_full()
                .post_page(&published_slug)
                .unwrap()
                .contains("class=\"tip-cta\"")
        );
        assert!(readiness.is_ready());
        assert!(!service_cancellation.is_cancelled());

        actor_cancellation.cancel();
        actor_task.await.unwrap().unwrap();
        drop(handle);
        writer_shutdown.cancel();
        writer_task.await.unwrap();
        drop(auth);
        drop(root);
    }

    #[tokio::test]
    async fn selected_address_replacement_changes_only_public_presentation_identity() {
        let catalog = tips_catalog("Publication body.");
        let (root, mut coordinator, auth, reader, writer_shutdown, writer_task) =
            profile_transition_fixture(catalog).await;
        let users = seed_tip_recipient(&mut coordinator, &auth).await;
        let selected = select_post(
            &coordinator.catalog,
            &PostId::parse(PUBLISHABLE_ID).unwrap(),
            None,
        )
        .unwrap();
        let accepted_preview_digest = reproduce_preview_digest(
            &coordinator.catalog,
            embedded_manifest(),
            &coordinator.ledger,
            coordinator.tip_recipient.as_ref(),
            &selected,
        )
        .unwrap();
        let stable_post_id = selected.stable_post_id.clone();
        let slug = selected.slug.clone();
        let published = coordinator
            .publish_now(PublishNow {
                creation_key: fixture_uuid(510),
                publication_id: fixture_uuid(511),
                stable_post_id: stable_post_id.clone(),
                expected_revision: None,
                accepted_preview_digest,
            })
            .await
            .unwrap();
        let before = reader.load_full();
        let before_site_digest = before.digest.clone();
        let before_presentation_digest = before.presentation_digest;
        assert!(
            before
                .post_page(&slug)
                .unwrap()
                .contains("tips@example.com")
        );

        let readiness = coordinator.readiness.clone();
        let service_cancellation = coordinator.cancellation.clone();
        let (handle, actor) = coordinator.into_actor(1);
        let actor_cancellation = CancellationToken::new();
        let actor_task = tokio::spawn(actor.run(actor_cancellation.clone()));
        let updated = handle
            .update_profile(replace_recipient_address(
                users,
                "recipient@new.example.com",
                6,
            ))
            .await
            .unwrap();
        assert_eq!(updated.version, ProfileVersion::new(2).unwrap());

        let after = reader.load_full();
        let html = after.post_page(&slug).unwrap();
        assert!(html.contains("recipient@new.example.com"));
        assert!(!html.contains("tips@example.com"));
        assert_eq!(after.digest, before_site_digest);
        assert_ne!(after.presentation_digest, before_presentation_digest);
        assert_eq!(
            handle
                .read()
                .ledger
                .published_post(&stable_post_id)
                .unwrap()
                .revision,
            published.revision
        );
        assert!(readiness.is_ready());
        assert!(!service_cancellation.is_cancelled());

        actor_cancellation.cancel();
        actor_task.await.unwrap().unwrap();
        drop(handle);
        writer_shutdown.cancel();
        writer_task.await.unwrap();
        drop(auth);
        drop(root);
    }

    #[tokio::test]
    async fn profile_post_commit_presentation_failure_keeps_durable_edit_and_fails_closed() {
        let catalog = catalog();
        let (root, mut coordinator, auth, _reader, writer_shutdown, writer_task) =
            profile_transition_fixture(catalog).await;
        let users = seed_tip_recipient(&mut coordinator, &auth).await;
        let profiles = coordinator.profiles.clone();
        let readiness = coordinator.readiness.clone();
        let cancellation = coordinator.cancellation.clone();
        let conflicting_catalog = catalog_with_site_candidate("Conflicting site", b"different");
        let conflicting_snapshot = snapshot(&conflicting_catalog, &PublicLedgerProjection::empty());
        let (_, conflicting_activator) = snapshot_store(conflicting_snapshot);
        coordinator.activator = conflicting_activator;

        let (handle, actor) = coordinator.into_actor(1);
        let actor_task = tokio::spawn(actor.run(cancellation.clone()));
        assert!(matches!(
            handle
                .update_profile(replace_recipient_address(
                    users,
                    "durable@new.example.com",
                    7,
                ))
                .await,
            Err(ProfileTransitionError::SnapshotActivationConflict)
        ));

        let durable = profiles.profile(users.recipient).await.unwrap().unwrap();
        assert_eq!(durable.version, ProfileVersion::new(2).unwrap());
        assert_eq!(
            durable.lightning_address.unwrap().as_str(),
            "durable@new.example.com"
        );
        assert!(!readiness.is_ready());
        assert!(cancellation.is_cancelled());

        actor_task.await.unwrap().unwrap();
        drop(handle);
        writer_shutdown.cancel();
        writer_task.await.unwrap();
        drop(profiles);
        drop(auth);
        drop(root);
    }

    #[test]
    fn catalog_selection_rejects_missing_draft_and_stale_posts() {
        let catalog = catalog();
        let missing = PostId::parse("33333333-3333-4333-8333-333333333333").unwrap();
        assert!(matches!(
            select_post(&catalog, &missing, None),
            Err(PublicationActivationError::PostNotFound { post_id }) if post_id == missing
        ));

        let draft = PostId::parse(DRAFT_ID).unwrap();
        assert!(matches!(
            select_post(&catalog, &draft, None),
            Err(PublicationActivationError::DraftPost { post_id }) if post_id == draft
        ));

        let publishable = PostId::parse(PUBLISHABLE_ID).unwrap();
        let stale = PostRevisionDigest::from_bytes([0x55; 32]);
        assert!(matches!(
            select_post(&catalog, &publishable, Some(&stale)),
            Err(PublicationActivationError::StaleRevision {
                post_id,
                expected,
                ..
            }) if post_id == publishable && *expected == stale
        ));
    }

    #[tokio::test]
    async fn historical_alias_owner_rejects_another_post_before_snapshot_activation() {
        let reserved_alias = PostAlias::parse("reserved-route").unwrap();
        let initial_catalog = route_ownership_catalog(
            Some(reserved_alias.as_str()),
            None,
            "First publication body.",
        );
        let (_root, mut coordinator, reader, store, writer_shutdown, writer_task) =
            coordinator_fixture(Arc::clone(&initial_catalog)).await;

        let first = coordinator
            .publish_now(publish_command_for(
                &initial_catalog,
                &coordinator.ledger,
                PUBLISHABLE_ID,
                None,
                1,
            ))
            .await
            .unwrap();
        assert_eq!(
            reader
                .load_full()
                .alias_target(&reserved_alias)
                .unwrap()
                .as_str(),
            "https://example.com/posts/first"
        );

        let without_alias = route_ownership_catalog(None, None, "Revised first body.");
        coordinator
            .apply_content_catalog(
                Arc::clone(&without_alias),
                ContentTreeDigest::from_bytes([0x42; 32]),
                None,
            )
            .await
            .unwrap();
        let revised = select_post(
            &without_alias,
            &PostId::parse(PUBLISHABLE_ID).unwrap(),
            None,
        )
        .unwrap();
        coordinator
            .publish_now(publish_command_for(
                &without_alias,
                &coordinator.ledger,
                PUBLISHABLE_ID,
                Some(revised.revision),
                2,
            ))
            .await
            .unwrap();
        assert!(reader.load_full().alias_target(&reserved_alias).is_none());

        let conflicting_catalog =
            route_ownership_catalog(None, Some(reserved_alias.as_str()), "Revised first body.");
        coordinator
            .apply_content_catalog(
                Arc::clone(&conflicting_catalog),
                ContentTreeDigest::from_bytes([0x43; 32]),
                None,
            )
            .await
            .unwrap();
        let active_before = reader.load_full();
        let site_before = coordinator.site.clone();
        let ledger_before = coordinator.ledger.clone();
        let error = coordinator
            .publish_now(publish_command_for(
                &conflicting_catalog,
                &coordinator.ledger,
                OTHER_PUBLISHABLE_ID,
                None,
                3,
            ))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PublicationActivationError::RouteOwnership(
                PublicationRouteOwnershipError::Conflict {
                    route: PublicationRoute::Alias(alias),
                }
            ) if alias == reserved_alias
        ));
        assert!(Arc::ptr_eq(&active_before, &reader.load_full()));
        assert_eq!(coordinator.site, site_before);
        assert_eq!(coordinator.ledger, ledger_before);
        assert!(coordinator.readiness.is_ready());
        assert!(!coordinator.cancellation.is_cancelled());
        assert!(
            store
                .startup_snapshot_state()
                .await
                .unwrap()
                .activating
                .is_empty()
        );
        assert_eq!(first.stable_post_id.as_str(), PUBLISHABLE_ID);

        drop(coordinator);
        writer_shutdown.cancel();
        writer_task.await.unwrap();
    }

    #[tokio::test]
    async fn publishes_through_the_real_store_and_replays_the_creation_key() {
        let catalog = catalog();
        let ledger = PublicLedgerProjection::empty();
        let initial = snapshot(&catalog, &ledger);
        let initial_digest = initial.digest.clone();
        let (_root, store, profiles, _auth, head, shutdown, task) =
            start_store(&catalog, initial_digest).await;
        let (reader, activator) = snapshot_store(initial);
        let readiness = Readiness::new(true);
        let cancellation = CancellationToken::new();
        let content_digest = ContentTreeDigest::from_bytes([0x11; 32]);
        let mut coordinator = PublicationCoordinator {
            catalog: Arc::clone(&catalog),
            content_digest: content_digest.clone(),
            candidates: candidate_catalogs(&catalog, content_digest),
            ledger,
            site: head,
            activator,
            store: store.clone(),
            profiles,
            tip_recipient: None,
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(Notify::new()),
            readiness: readiness.clone(),
            cancellation: cancellation.clone(),
        };

        let request = command();
        let mut stale_preview = request.clone();
        stale_preview.creation_key = Uuid::new_v4();
        stale_preview.publication_id = Uuid::new_v4();
        stale_preview.accepted_preview_digest = PreviewDigest::from_bytes([0x99; 32]);
        assert!(matches!(
            coordinator.publish_now(stale_preview).await,
            Err(PublicationActivationError::StalePreview { .. })
        ));
        assert!(coordinator.ledger.is_empty());
        let published = coordinator.publish_now(request.clone()).await.unwrap();
        assert_eq!(published.stable_post_id.as_str(), PUBLISHABLE_ID);
        assert_eq!(published.site, coordinator.site);
        assert_eq!(coordinator.ledger.len(), 1);
        let selected = select_post(&catalog, &published.stable_post_id, None).unwrap();
        assert!(
            reader
                .load_full()
                .post_canonical_url(&selected.slug)
                .is_some()
        );

        let mut duplicate = command();
        duplicate.creation_key = Uuid::new_v4();
        duplicate.publication_id = Uuid::new_v4();
        assert!(matches!(
            coordinator.publish_now(duplicate).await,
            Err(PublicationActivationError::AlreadyPublished { .. })
        ));

        coordinator.catalog = catalog_without_publishable_post();
        assert_eq!(
            coordinator.publish_now(request.clone()).await.unwrap(),
            published
        );

        let mut changed_precondition = request.clone();
        changed_precondition.expected_revision = Some(published.revision.clone());
        assert!(matches!(
            coordinator.publish_now(changed_precondition).await,
            Err(PublicationActivationError::Lookup(
                PublishNowLookupError::IdempotencyConflict
            ))
        ));

        let mut changed_post = request;
        changed_post.stable_post_id =
            PostId::parse("33333333-3333-4333-8333-333333333333").unwrap();
        assert!(matches!(
            coordinator.publish_now(changed_post).await,
            Err(PublicationActivationError::Lookup(
                PublishNowLookupError::IdempotencyConflict
            ))
        ));
        assert!(readiness.is_ready());
        assert!(!cancellation.is_cancelled());

        let durable = store.startup_snapshot_state().await.unwrap();
        assert_eq!(durable.ledger.len(), 1);
        assert!(durable.activating.is_empty());
        drop(coordinator);
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn reviewed_publish_rejects_each_changed_review_head_before_claiming_intent() {
        let catalog = catalog();
        let (root, mut coordinator, _reader, store, shutdown, task) =
            coordinator_fixture(Arc::clone(&catalog)).await;
        let reviewed = reviewed_publish_command(&coordinator, command());

        let changed_content_digest = ContentTreeDigest::from_bytes([0x22; 32]);
        let mut stale_content = reviewed.clone();
        stale_content.expected_content_digest = changed_content_digest.clone();
        assert!(matches!(
            coordinator.publish_reviewed_now(stale_content).await,
            Err(PublicationActivationError::StaleReview(
                PublishReviewError::Content { reviewed, current }
            )) if *reviewed == changed_content_digest && *current == coordinator.content_digest
        ));

        let mut changed_site = coordinator.site.clone();
        changed_site.version += 1;
        let mut stale_site = reviewed.clone();
        stale_site.expected_site = changed_site.clone();
        assert!(matches!(
            coordinator.publish_reviewed_now(stale_site).await,
            Err(PublicationActivationError::StaleReview(
                PublishReviewError::SiteHead { reviewed, current }
            )) if *reviewed == changed_site && *current == coordinator.site
        ));

        let candidate_revision = catalog
            .current_post(&reviewed.publication.stable_post_id)
            .unwrap()
            .revision
            .clone();
        let mut stale_public_revision = reviewed;
        stale_public_revision.expected_public_revision = ReviewedPublicRevision::Published {
            revision: candidate_revision.clone(),
        };
        assert!(matches!(
            coordinator
                .publish_reviewed_now(stale_public_revision)
                .await,
            Err(PublicationActivationError::StaleReview(
                PublishReviewError::PublicRevision {
                    reviewed,
                    current,
                    ..
                }
            )) if *reviewed == ReviewedPublicRevision::Published {
                revision: candidate_revision
            } && *current == ReviewedPublicRevision::Unpublished
        ));

        assert!(coordinator.ledger.is_empty());
        assert!(
            store
                .startup_snapshot_state()
                .await
                .unwrap()
                .activating
                .is_empty()
        );
        drop(coordinator);
        shutdown.cancel();
        task.await.unwrap();
        drop(store);
        drop(root);
    }

    #[tokio::test]
    async fn reviewed_update_rejects_an_unseen_public_site_change() {
        let initial_catalog = route_ownership_catalog(None, None, "Publication body.");
        let (root, mut coordinator, _reader, store, shutdown, task) =
            coordinator_fixture(Arc::clone(&initial_catalog)).await;
        let first = coordinator
            .publish_now(publish_command_for(
                &coordinator.catalog,
                &coordinator.ledger,
                PUBLISHABLE_ID,
                None,
                600,
            ))
            .await
            .unwrap();

        let revised_catalog = route_ownership_catalog(None, None, "Revised publication body.");
        coordinator
            .apply_content_catalog(
                revised_catalog,
                ContentTreeDigest::from_bytes([0x22; 32]),
                None,
            )
            .await
            .unwrap();
        let revised = coordinator
            .catalog
            .current_post(&first.stable_post_id)
            .unwrap()
            .revision
            .clone();
        let update = publish_command_for(
            &coordinator.catalog,
            &coordinator.ledger,
            PUBLISHABLE_ID,
            Some(revised),
            601,
        );
        let reviewed_update = reviewed_publish_command(&coordinator, update);
        assert_eq!(
            reviewed_update.expected_public_revision,
            ReviewedPublicRevision::Published {
                revision: first.revision.clone()
            }
        );

        coordinator
            .publish_now(publish_command_for(
                &coordinator.catalog,
                &coordinator.ledger,
                OTHER_PUBLISHABLE_ID,
                None,
                602,
            ))
            .await
            .unwrap();
        assert_ne!(reviewed_update.expected_site, coordinator.site);
        assert!(matches!(
            coordinator.publish_reviewed_now(reviewed_update).await,
            Err(PublicationActivationError::StaleReview(
                PublishReviewError::SiteHead { .. }
            ))
        ));
        assert_eq!(
            coordinator
                .ledger
                .published_post(&first.stable_post_id)
                .unwrap()
                .revision,
            first.revision
        );

        drop(coordinator);
        shutdown.cancel();
        task.await.unwrap();
        drop(store);
        drop(root);
    }

    #[tokio::test]
    async fn reviewed_publish_replays_durable_success_before_stale_review_checks() {
        let catalog = catalog();
        let (root, mut coordinator, _reader, store, shutdown, task) =
            coordinator_fixture(catalog).await;
        let reviewed = reviewed_publish_command(&coordinator, command());
        let published = coordinator
            .publish_reviewed_now(reviewed.clone())
            .await
            .unwrap();
        coordinator
            .apply_content_catalog(
                revised_catalog(),
                ContentTreeDigest::from_bytes([0x22; 32]),
                None,
            )
            .await
            .unwrap();
        assert_ne!(reviewed.expected_content_digest, coordinator.content_digest);
        assert_ne!(reviewed.expected_site, coordinator.site);
        assert_ne!(
            reviewed.expected_public_revision,
            ReviewedPublicRevision::Published {
                revision: published.revision.clone()
            }
        );

        let replayed = coordinator.publish_reviewed_now(reviewed).await.unwrap();
        assert_eq!(replayed, published);

        drop(coordinator);
        shutdown.cancel();
        task.await.unwrap();
        drop(store);
        drop(root);
    }

    #[tokio::test]
    async fn activation_conflict_fails_closed_and_retry_resumes_the_durable_intent() {
        let catalog = catalog();
        let ledger = PublicLedgerProjection::empty();
        let initial = snapshot(&catalog, &ledger);
        let initial_digest = initial.digest.clone();
        let (_root, store, profiles, _auth, head, shutdown, task) =
            start_store(&catalog, initial_digest).await;

        let selected =
            select_post(&catalog, &PostId::parse(PUBLISHABLE_ID).unwrap(), None).unwrap();
        let wrong_snapshot = build_candidate(
            Arc::clone(&catalog),
            embedded_manifest(),
            &ledger,
            None,
            &selected,
            OffsetDateTime::from_unix_timestamp(2_000).unwrap(),
        )
        .unwrap()
        .snapshot;
        let (_, wrong_activator) = snapshot_store(wrong_snapshot);
        let readiness = Readiness::new(true);
        let cancellation = CancellationToken::new();
        let content_digest = ContentTreeDigest::from_bytes([0x11; 32]);
        let mut interrupted = PublicationCoordinator {
            catalog: Arc::clone(&catalog),
            content_digest: content_digest.clone(),
            candidates: candidate_catalogs(&catalog, content_digest),
            ledger: ledger.clone(),
            site: head,
            activator: wrong_activator,
            store: store.clone(),
            profiles: profiles.clone(),
            tip_recipient: None,
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(Notify::new()),
            readiness: readiness.clone(),
            cancellation: cancellation.clone(),
        };
        assert!(matches!(
            interrupted.publish_now(command()).await,
            Err(PublicationActivationError::SnapshotActivationConflict)
        ));
        assert!(!readiness.is_ready());
        assert!(cancellation.is_cancelled());

        let durable = store.startup_snapshot_state().await.unwrap();
        assert!(durable.ledger.is_empty());
        assert_eq!(durable.activating.len(), 1);
        let base = snapshot(&catalog, &durable.ledger);
        let (reader, activator) = snapshot_store(base);
        let recovery_cancellation = CancellationToken::new();
        let content_digest = ContentTreeDigest::from_bytes([0x11; 32]);
        let mut recovering = PublicationCoordinator {
            catalog: Arc::clone(&catalog),
            content_digest: content_digest.clone(),
            candidates: candidate_catalogs(&catalog, content_digest),
            ledger: durable.ledger,
            site: durable.site.unwrap(),
            activator,
            store: store.clone(),
            profiles,
            tip_recipient: None,
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(Notify::new()),
            readiness: Readiness::default(),
            cancellation: recovery_cancellation.clone(),
        };
        let recovered = recovering.publish_now(command()).await.unwrap();
        assert_eq!(recovered.stable_post_id.as_str(), PUBLISHABLE_ID);
        assert!(!recovery_cancellation.is_cancelled());
        assert!(
            reader
                .load_full()
                .post_canonical_url(&selected.slug)
                .is_some()
        );

        let durable = store.startup_snapshot_state().await.unwrap();
        assert_eq!(durable.ledger.len(), 1);
        assert!(durable.activating.is_empty());
        drop(recovering);
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn scheduled_release_replays_the_exact_pin_and_only_activates_when_due() {
        let scheduled_alias = PostAlias::parse("scheduled-alias").unwrap();
        let catalog = catalog_with_alias(scheduled_alias.as_str());
        let ledger = PublicLedgerProjection::empty();
        let initial = snapshot(&catalog, &ledger);
        let initial_digest = initial.digest.clone();
        let (_root, store, profiles, _auth, head, shutdown, task) =
            start_store(&catalog, initial_digest).await;
        let (reader, activator) = snapshot_store(initial);
        let readiness = Readiness::new(true);
        let cancellation = CancellationToken::new();
        let content_digest = ContentTreeDigest::from_bytes([0x11; 32]);
        let mut coordinator = PublicationCoordinator {
            catalog: Arc::clone(&catalog),
            content_digest: content_digest.clone(),
            candidates: candidate_catalogs(&catalog, content_digest),
            ledger: ledger.clone(),
            site: head,
            activator,
            store: store.clone(),
            profiles,
            tip_recipient: None,
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(Notify::new()),
            readiness: readiness.clone(),
            cancellation: cancellation.clone(),
        };

        let selected =
            select_post(&catalog, &PostId::parse(PUBLISHABLE_ID).unwrap(), None).unwrap();
        let pinned = selected.revision.clone();
        let canonical_url = format!("https://example.com/posts/{}", selected.slug.as_str());
        let scheduled_at = OffsetDateTime::now_utc() + time::Duration::hours(1);
        let request = schedule_command_for(&catalog, &ledger, scheduled_at);
        let ScheduledApprovalOutcome::Scheduled(approval) =
            coordinator.schedule(request.clone()).await.unwrap()
        else {
            panic!("a future approval must remain scheduled");
        };
        assert_eq!(approval.revision, pinned);
        assert_eq!(
            coordinator.schedule(request.clone()).await.unwrap(),
            ScheduledApprovalOutcome::Scheduled(approval.clone())
        );
        assert!(!reader.load_full().feed.body.contains(PUBLISHABLE_ID));
        assert!(!reader.load_full().sitemap.body.contains(&canonical_url));
        assert!(reader.load_full().alias_target(&scheduled_alias).is_none());

        let durable = store.next_scheduled_publication().await.unwrap().unwrap();
        assert_eq!(durable.publication_id, approval.publication_id);
        assert_eq!(durable.publication.view().pinned_post_digest, pinned);
        assert_eq!(durable.publication.view().scheduled_at, scheduled_at);

        assert!(matches!(
            coordinator
                .activate_scheduled(
                    approval.publication_id,
                    scheduled_at - time::Duration::nanoseconds(1),
                )
                .await,
            Err(PublicationActivationError::Database(
                DatabaseMutationError::Command(DatabaseCommandError::Rejected)
            ))
        ));
        assert!(readiness.is_ready());
        assert!(!cancellation.is_cancelled());

        let revised = revised_catalog();
        let revised_pin = select_post(&revised, &PostId::parse(PUBLISHABLE_ID).unwrap(), None)
            .unwrap()
            .revision;
        assert_ne!(revised_pin, pinned);
        coordinator
            .apply_content_catalog(revised, ContentTreeDigest::from_bytes([0x22; 32]), None)
            .await
            .unwrap();
        assert_eq!(
            coordinator.schedule(request).await.unwrap(),
            ScheduledApprovalOutcome::Scheduled(approval.clone())
        );

        let published = coordinator
            .activate_scheduled(approval.publication_id, scheduled_at)
            .await
            .unwrap();
        assert_eq!(published.revision, pinned);
        assert_eq!(published.published_at, scheduled_at);
        let active = reader.load_full();
        assert!(active.feed.body.contains(PUBLISHABLE_ID));
        assert!(active.sitemap.body.contains(&canonical_url));
        assert_eq!(
            active.alias_target(&scheduled_alias).unwrap().as_str(),
            canonical_url
        );
        assert!(
            active.feed.body.contains(
                &published
                    .published_at
                    .to_offset(time::UtcOffset::UTC)
                    .format(&Rfc2822)
                    .unwrap()
            )
        );
        assert!(coordinator.scheduled.is_empty());
        assert!(store.next_scheduled_publication().await.unwrap().is_none());
        assert_eq!(
            store
                .startup_snapshot_state()
                .await
                .unwrap()
                .ledger
                .published_post(&published.stable_post_id)
                .unwrap()
                .revision,
            pinned
        );

        drop(coordinator);
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn scheduled_release_pins_the_full_content_candidate_not_only_the_post_revision() {
        let candidate_a = catalog_with_site_candidate("Candidate A shell", b"favicon-a");
        let candidate_b = catalog_with_site_candidate("Candidate B shell", b"favicon-b");
        let post_id = PostId::parse(PUBLISHABLE_ID).unwrap();
        let selected_a = select_post(&candidate_a, &post_id, None).unwrap();
        let selected_b = select_post(&candidate_b, &post_id, None).unwrap();
        assert_eq!(selected_a.revision, selected_b.revision);

        let ledger = PublicLedgerProjection::empty();
        let scheduled_at = OffsetDateTime::from_unix_timestamp(2_000_000_000).unwrap();
        let expected_a = build_candidate(
            Arc::clone(&candidate_a),
            embedded_manifest(),
            &ledger,
            None,
            &selected_a,
            scheduled_at,
        )
        .unwrap();
        let expected_b = build_candidate(
            Arc::clone(&candidate_b),
            embedded_manifest(),
            &ledger,
            None,
            &selected_b,
            scheduled_at,
        )
        .unwrap();
        assert_ne!(expected_a.digest, expected_b.digest);
        assert_ne!(
            expected_a.snapshot.index_page(),
            expected_b.snapshot.index_page()
        );

        let initial = snapshot(&candidate_a, &ledger);
        let initial_digest = initial.digest.clone();
        let (_root, store, profiles, _auth, head, shutdown, task) =
            start_store(&candidate_a, initial_digest).await;
        let (reader, activator) = snapshot_store(initial);
        let content_a = ContentTreeDigest::from_bytes([0x31; 32]);
        let content_b = ContentTreeDigest::from_bytes([0x32; 32]);
        let mut coordinator = PublicationCoordinator {
            catalog: Arc::clone(&candidate_a),
            content_digest: content_a.clone(),
            candidates: candidate_catalogs(&candidate_a, content_a.clone()),
            ledger,
            site: head,
            activator,
            store: store.clone(),
            profiles,
            tip_recipient: None,
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(Notify::new()),
            readiness: Readiness::new(true),
            cancellation: CancellationToken::new(),
        };

        let ScheduledApprovalOutcome::Scheduled(approval) = coordinator
            .schedule(schedule_command_for(
                &candidate_a,
                &PublicLedgerProjection::empty(),
                scheduled_at,
            ))
            .await
            .unwrap()
        else {
            panic!("a future approval must remain scheduled");
        };
        assert_eq!(
            store
                .next_scheduled_publication()
                .await
                .unwrap()
                .unwrap()
                .content_digest,
            content_a
        );
        coordinator
            .apply_content_catalog(Arc::clone(&candidate_b), content_b.clone(), None)
            .await
            .unwrap();
        assert_eq!(coordinator.content_digest, content_b);

        let published = coordinator
            .activate_scheduled(approval.publication_id, scheduled_at)
            .await
            .unwrap();
        let active = reader.load_full();
        assert_eq!(published.site.digest, expected_a.digest);
        assert_eq!(active.digest, expected_a.digest);
        assert_ne!(active.digest, expected_b.digest);
        assert_eq!(active.index_page(), expected_a.snapshot.index_page());
        assert!(active.index_page().contains("Candidate A shell"));
        assert!(!active.index_page().contains("Candidate B shell"));

        let active_favicon = snapshot_asset(&active, "assets/favicon.png");
        let expected_a_favicon = snapshot_asset(&expected_a.snapshot, "assets/favicon.png");
        let expected_b_favicon = snapshot_asset(&expected_b.snapshot, "assets/favicon.png");
        assert_eq!(active_favicon, expected_a_favicon);
        assert_ne!(active_favicon, expected_b_favicon);
        assert_eq!(active_favicon.bytes.as_ref(), b"favicon-a");

        drop(coordinator);
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn update_supersedes_one_release_and_startup_selects_only_the_new_digest() {
        let catalog = catalog();
        let ledger = PublicLedgerProjection::empty();
        let initial = snapshot(&catalog, &ledger);
        let initial_digest = initial.digest.clone();
        let (root, store, profiles, _auth, head, shutdown, task) =
            start_store(&catalog, initial_digest).await;
        let (reader, activator) = snapshot_store(initial);
        let content_digest = ContentTreeDigest::from_bytes([0x11; 32]);
        let mut coordinator = PublicationCoordinator {
            catalog: Arc::clone(&catalog),
            content_digest: content_digest.clone(),
            candidates: candidate_catalogs(&catalog, content_digest),
            ledger,
            site: head,
            activator,
            store: store.clone(),
            profiles,
            tip_recipient: None,
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(Notify::new()),
            readiness: Readiness::new(true),
            cancellation: CancellationToken::new(),
        };

        let original_selected =
            select_post(&catalog, &PostId::parse(PUBLISHABLE_ID).unwrap(), None).unwrap();
        let original_url = format!(
            "https://example.com/posts/{}",
            original_selected.slug.as_str()
        );
        let first = coordinator.publish_now(command()).await.unwrap();
        let first_snapshot = reader.load_full();
        let first_feed_body = Arc::clone(&first_snapshot.feed.body);
        let first_feed_digest = first_snapshot.feed.digest;
        let first_sitemap_body = Arc::clone(&first_snapshot.sitemap.body);
        let first_sitemap_digest = first_snapshot.sitemap.digest;
        let revised = revised_catalog();
        let revised_selected =
            select_post(&revised, &PostId::parse(PUBLISHABLE_ID).unwrap(), None).unwrap();
        let revised_pin = revised_selected.revision.clone();
        let revised_url = format!(
            "https://example.com/posts/{}",
            revised_selected.slug.as_str()
        );
        coordinator
            .apply_content_catalog(revised, ContentTreeDigest::from_bytes([0x22; 32]), None)
            .await
            .unwrap();
        let update_id = Uuid::parse_str("eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee").unwrap();
        let update_command = PublishNow {
            creation_key: Uuid::parse_str("ffffffff-ffff-4fff-8fff-ffffffffffff").unwrap(),
            publication_id: update_id,
            stable_post_id: PostId::parse(PUBLISHABLE_ID).unwrap(),
            expected_revision: Some(revised_pin.clone()),
            accepted_preview_digest: preview_digest(&coordinator.catalog, &coordinator.ledger),
        };
        let updated = coordinator
            .publish_now(update_command.clone())
            .await
            .unwrap();
        assert_eq!(updated.revision, revised_pin);
        assert_ne!(updated.revision, first.revision);
        assert_eq!(updated.published_at, first.published_at);
        let replayed = coordinator
            .publish_now(update_command.clone())
            .await
            .unwrap();
        assert_eq!(replayed, updated);
        assert_eq!(
            coordinator
                .ledger
                .published_post(&updated.stable_post_id)
                .unwrap()
                .published_at,
            first.published_at
        );
        let updated_snapshot = reader.load_full();
        assert_ne!(updated_snapshot.feed.body, first_feed_body);
        assert_ne!(updated_snapshot.feed.digest, first_feed_digest);
        assert_ne!(updated_snapshot.sitemap.body, first_sitemap_body);
        assert_ne!(updated_snapshot.sitemap.digest, first_sitemap_digest);
        assert!(!updated_snapshot.sitemap.body.contains(&original_url));
        assert!(updated_snapshot.sitemap.body.contains(&revised_url));
        assert!(updated_snapshot.feed.body.contains(PUBLISHABLE_ID));
        assert!(first_feed_body.contains(PUBLISHABLE_ID));
        assert!(
            updated_snapshot
                .feed
                .body
                .contains("<title>Revised publishable</title>")
        );
        assert!(
            updated_snapshot
                .feed
                .body
                .contains("<description>Revised activation summary.</description>")
        );
        assert!(
            updated_snapshot
                .feed
                .body
                .contains("<link>https://example.com/posts/revised-publishable</link>")
        );
        assert!(
            !updated_snapshot
                .feed
                .body
                .contains("Revised publication body.")
        );
        assert!(
            updated_snapshot.feed.body.contains(
                &first
                    .published_at
                    .to_offset(time::UtcOffset::UTC)
                    .format(&Rfc2822)
                    .unwrap()
            )
        );

        let inspection = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(root.path().join("state/maincopy.db"))
                    .read_only(true),
            )
            .await
            .unwrap();
        let rows: Vec<(Vec<u8>, String, Vec<u8>)> = sqlx::query_as(
            "SELECT publication_id, state, pinned_post_digest \
             FROM canonical_publications WHERE stable_post_id = ?",
        )
        .bind(
            PostId::parse(PUBLISHABLE_ID)
                .unwrap()
                .as_uuid()
                .as_bytes()
                .as_slice(),
        )
        .fetch_all(&inspection)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter()
                .filter(|(_, state, _)| state == "superseded")
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|(_, state, _)| state == "published")
                .count(),
            1
        );
        assert!(rows.iter().any(|(publication_id, state, revision)| {
            publication_id == command().publication_id.as_bytes()
                && state == "superseded"
                && revision == first.revision.as_bytes()
        }));
        assert!(rows.iter().any(|(publication_id, state, revision)| {
            publication_id == update_id.as_bytes()
                && state == "published"
                && revision == updated.revision.as_bytes()
        }));

        let durable = store.startup_snapshot_state().await.unwrap();
        assert_eq!(durable.ledger.len(), 1);
        let durable_post = durable
            .ledger
            .published_post(&updated.stable_post_id)
            .unwrap();
        assert_eq!(durable_post.revision, updated.revision);
        assert_eq!(durable_post.published_at, first.published_at);
        let restarted_snapshot = snapshot(&coordinator.catalog, &durable.ledger);
        assert_eq!(
            restarted_snapshot.sitemap.body,
            updated_snapshot.sitemap.body
        );
        assert_eq!(
            restarted_snapshot.sitemap.digest,
            updated_snapshot.sitemap.digest
        );
        let (_restart_reader, restart_activator) = snapshot_store(restarted_snapshot);
        let mut restarted = PublicationCoordinator {
            catalog: Arc::clone(&coordinator.catalog),
            content_digest: coordinator.content_digest.clone(),
            candidates: Arc::clone(&coordinator.candidates),
            ledger: durable.ledger,
            site: durable.site.unwrap(),
            activator: restart_activator,
            store: store.clone(),
            profiles: coordinator.profiles.clone(),
            tip_recipient: coordinator.tip_recipient.clone(),
            frontend: coordinator.frontend,
            source_commit: coordinator.source_commit.clone(),
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(Notify::new()),
            readiness: Readiness::new(true),
            cancellation: CancellationToken::new(),
        };
        let restarted_replay = restarted.publish_now(update_command).await.unwrap();
        assert_eq!(restarted_replay, updated);
        inspection.close().await;
        drop(coordinator);
        shutdown.cancel();
        task.await.unwrap();
    }
}
