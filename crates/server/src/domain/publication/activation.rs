use std::{collections::BTreeMap, sync::Arc};

use arc_swap::ArcSwap;
use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    content::{
        ContentTreeDigest, DraftStatus, PostId, PostRevisionDigest, PostSlug, PreviewDigest,
        SiteSnapshotDigest, SourceCommit,
    },
    database::store::{DatabaseCommandError, DatabaseMutationError},
    frontend_assets::FrontendAssetManifest,
    render::{
        CatalogRetentionError, ContentCatalog, SiteSnapshot, SiteSnapshotActivator,
        SiteSnapshotBuildError, build_site_snapshot,
        render_bound_post_revision_preview, render_site_shell,
    },
    web::Readiness,
};

use super::{PublicLedgerProjection, PublishedPostRevision};
use super::store::{
    BeginPublishNow, BeginScheduledActivation, BegunPublication, CommandIdempotencyKey,
    CompletedPublication, FinishPublication, FinishedPublication, IndexContentCatalog,
    LookupPublishNow, LookupSchedulePublication, ObservedPostRevision, PublicationStore,
    PublishNowLookupError, PublishNowState, RecoverablePublicationActivation, SchedulePublication,
    SchedulePublicationLookupError, SchedulePublicationReplay, ScheduledPublication, SiteHead,
};

/// Owns the serialized transition from durable publication intent to public visibility.
pub(crate) struct PublicationCoordinator {
    pub catalog: Arc<ContentCatalog>,
    pub content_digest: ContentTreeDigest,
    pub candidates: Arc<BTreeMap<ContentTreeDigest, Arc<ContentCatalog>>>,
    pub ledger: PublicLedgerProjection,
    pub site: SiteHead,
    pub activator: SiteSnapshotActivator,
    pub store: PublicationStore,
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
    PublishNow {
        command: PublishNow,
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
pub(crate) enum PublicationCoordinatorActorError {
    #[error("all publication coordinator handles were dropped")]
    HandlesDropped,
}

impl PublicationCoordinator {
    /// Installs one validated candidate as the private preview without changing public state.
    pub(crate) async fn apply_content_catalog(
        &mut self,
        catalog: Arc<ContentCatalog>,
        content_digest: ContentTreeDigest,
        source_commit: Option<SourceCommit>,
    ) -> Result<SiteHead, ContentReloadError> {
        let retained_candidate = Arc::clone(&catalog);
        let mut candidate = catalog.as_ref().clone();
        candidate.retain_ledger_revisions_from(&self.catalog, &self.ledger)?;
        candidate.retain_revisions_from(
            &self.catalog,
            self.scheduled.values().map(|scheduled| {
                let view = scheduled.publication.view();
                (view.stable_post_id.clone(), view.pinned_post_digest.clone())
            }),
        )?;
        let catalog = Arc::new(candidate);
        let observed_posts = observed_post_revisions(&catalog);
        self.store
            .index_content_catalog(IndexContentCatalog {
                observed_at: OffsetDateTime::now_utc(),
                source_commit: source_commit.clone(),
                posts: observed_posts,
            })
            .await?;
        self.catalog = catalog;
        Arc::make_mut(&mut self.candidates).insert(content_digest.clone(), retained_candidate);
        self.content_digest = content_digest;
        self.source_commit = source_commit;
        Ok(self.site.clone())
    }

    /// Publishes one current, publishable catalog revision.
    pub(crate) async fn publish_now(
        &mut self,
        command: PublishNow,
    ) -> Result<PublishedPublication, PublicationActivationError> {
        let replay = self
            .store
            .publish_now_replay(LookupPublishNow {
                creation_key: CommandIdempotencyKey::new(command.creation_key),
                stable_post_id: command.stable_post_id.clone(),
                expected_revision: command.expected_revision.clone(),
                accepted_preview_digest: command.accepted_preview_digest.clone(),
            })
            .await;
        let replay = match replay {
            Ok(replay) => replay,
            Err(error @ PublishNowLookupError::InvalidStoredState) => {
                let _safety = FailClosedGuard::new(&self.readiness, &self.cancellation);
                return Err(PublicationActivationError::Lookup(error));
            }
            Err(error) => return Err(PublicationActivationError::Lookup(error)),
        };
        match replay {
            Some(PublishNowState::Published(completed)) => {
                return Ok(completed_result(completed));
            }
            Some(PublishNowState::Activating(begun)) => {
                let mut safety = FailClosedGuard::new(&self.readiness, &self.cancellation);
                let catalog = self.catalog_for_content_digest(&begun.content_digest)?;
                let selected = select_stored_post(&catalog, begun.publication.view())?;
                require_accepted_preview(
                    reproduce_preview_digest(&catalog, self.frontend, &self.ledger, &selected)?,
                    &begun.accepted_preview_digest,
                )?;
                let candidate = self.candidate_for_begun(
                    catalog,
                    &selected,
                    begun.publication.view(),
                    &begun.candidate_site_digest,
                    None,
                )?;
                return self
                    .activate_and_finish(begun, selected, candidate, &mut safety)
                    .await;
            }
            None => {}
        }

        let selected = select_post(
            &self.catalog,
            &command.stable_post_id,
            command.expected_revision.as_ref(),
        )?;
        require_update_precondition(&self.ledger, &selected, command.expected_revision.as_ref())?;
        require_accepted_preview(
            reproduce_preview_digest(&self.catalog, self.frontend, &self.ledger, &selected)?,
            &command.accepted_preview_digest,
        )?;
        let now = OffsetDateTime::now_utc();
        let prebuilt = build_candidate(
            Arc::clone(&self.catalog),
            self.frontend,
            &self.ledger,
            &selected,
            now,
        )?;
        let requested_content_digest = self.content_digest.clone();
        let accepted_preview_digest = command.accepted_preview_digest.clone();
        let mut safety = FailClosedGuard::new(&self.readiness, &self.cancellation);
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

        let begun = match self.store.begin_publish_now(requested).await {
            Ok(PublishNowState::Activating(begun)) => begun,
            Ok(PublishNowState::Published(completed)) => {
                if !prebuilt.already_published {
                    return Err(PublicationActivationError::DurableStateMismatch);
                }
                let result = validate_completed(completed, &selected)?;
                safety.disarm();
                return Ok(result);
            }
            Err(error) => {
                if definitely_unclaimed(&error) {
                    safety.disarm();
                }
                if prebuilt.already_published
                    && matches!(
                        error,
                        DatabaseMutationError::Command(DatabaseCommandError::Rejected)
                    )
                {
                    return Err(PublicationActivationError::AlreadyPublished {
                        post_id: selected.stable_post_id,
                    });
                }
                return Err(PublicationActivationError::Database(error));
            }
        };

        if prebuilt.already_published {
            return Err(PublicationActivationError::DurableStateMismatch);
        }
        if begun.content_digest != requested_content_digest
            || begun.accepted_preview_digest != accepted_preview_digest
        {
            return Err(PublicationActivationError::DurableStateMismatch);
        }
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
                SchedulePublicationReplay::Published(completed) => Ok(
                    ScheduledApprovalOutcome::Published(completed_result(completed)),
                ),
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
            reproduce_preview_digest(&self.catalog, self.frontend, &self.ledger, &selected)?,
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
            reproduce_preview_digest(&catalog, self.frontend, &self.ledger, &selected)?,
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
            .retain_ledger_revisions_from(&self.catalog, &self.ledger)
            .map_err(|_| PublicationActivationError::DurableStateMismatch)?;
        let catalog = Arc::new(catalog);
        let selected = select_stored_post(&catalog, view)?;
        require_accepted_preview(
            reproduce_preview_digest(&catalog, self.frontend, &self.ledger, &selected)?,
            &scheduled.accepted_preview_digest,
        )?;
        let prebuilt = build_candidate(catalog, self.frontend, &self.ledger, &selected, now)?;
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
            reproduce_preview_digest(&catalog, self.frontend, &self.ledger, &selected)?,
            &activation.accepted_preview_digest,
        )?;
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
            _ => build_candidate(catalog, self.frontend, &self.ledger, selected, published_at)?,
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
            .retain_ledger_revisions_from(&self.catalog, &self.ledger)
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
            })
            .await?;
        let result = validate_finished(finished, &selected)?;
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
    })
}

fn reproduce_preview_digest(
    catalog: &ContentCatalog,
    frontend: &'static FrontendAssetManifest,
    ledger: &PublicLedgerProjection,
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
    let shell = render_site_shell(catalog, frontend, &ledger)?;
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
    use std::path::Path;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use tokio::task::JoinHandle;

    use super::*;
    use crate::{
        config::{
            DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
            DatabaseWriterQueueCapacity,
        },
        content::{
            DiscoveredContentTree, DiscoveredPost, LogicalAssetPath, PostCollection,
            resolve_content_assets,
            tree::{asset, post, publication},
        },
        database,
        domain::publication::store::{InstallStartupSnapshot, ObservedPostRevision},
        frontend_assets::embedded_manifest,
        render::{compile_content_catalog, snapshot_store},
    };

    const PUBLISHABLE_ID: &str = "11111111-1111-4111-8111-111111111111";
    const DRAFT_ID: &str = "22222222-2222-4222-8222-222222222222";

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

    fn revised_catalog() -> Arc<ContentCatalog> {
        compile_catalog(vec![
            post(
                "posts/publishable.md",
                PostCollection::Posts,
                post_source(PUBLISHABLE_ID, "publishable", false)
                    .replace("Publication body.", "Revised publication body."),
            ),
            post(
                "drafts/draft.md",
                PostCollection::Drafts,
                post_source(DRAFT_ID, "draft", true),
            ),
        ])
    }

    fn compile_catalog(posts: Vec<DiscoveredPost>) -> Arc<ContentCatalog> {
        let tree = DiscoveredContentTree::new(
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
        let tree = DiscoveredContentTree::new(
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
        format!(
            "+++\n\
             id = {id:?}\n\
             title = {slug:?}\n\
             slug = {slug:?}\n\
             authored_at = 2026-08-29T15:00:00-04:00\n\
             description = \"Activation fixture.\"\n\
             draft = {draft}\n\
             +++\n\
             Publication body.\n"
        )
    }

    fn snapshot(catalog: &Arc<ContentCatalog>, ledger: &PublicLedgerProjection) -> SiteSnapshot {
        let shell = render_site_shell(Arc::clone(catalog), embedded_manifest(), ledger).unwrap();
        build_site_snapshot(shell, ledger).unwrap()
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
        (root, publications, head, shutdown, task)
    }

    fn database_configuration(path: &Path) -> DatabaseConfigurationView<'_> {
        DatabaseConfigurationView {
            path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1_000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(8).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
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

    fn schedule_command(scheduled_at: OffsetDateTime) -> Schedule {
        let catalog = catalog();
        let ledger = PublicLedgerProjection::empty();
        schedule_command_for(&catalog, &ledger, scheduled_at)
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
        reproduce_preview_digest(catalog, embedded_manifest(), ledger, &selected).unwrap()
    }

    async fn coordinator_fixture(
        catalog: Arc<ContentCatalog>,
    ) -> (
        tempfile::TempDir,
        PublicationCoordinator,
        PublicationStore,
        CancellationToken,
        JoinHandle<()>,
    ) {
        let ledger = PublicLedgerProjection::empty();
        let initial = snapshot(&catalog, &ledger);
        let initial_digest = initial.digest.clone();
        let (root, store, head, writer_shutdown, writer_task) =
            start_store(&catalog, initial_digest).await;
        let (_, activator) = snapshot_store(initial);
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
            writer_keepalive,
            writer_shutdown,
            writer_task,
        )
    }

    async fn wait_for_full_mailbox(handle: &PublicationCoordinatorHandle) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while handle.commands.capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a command should fill the bounded actor mailbox");
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
        let (root, coordinator, writer_keepalive, writer_shutdown, writer_task) =
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
        let (root, coordinator, writer_keepalive, writer_shutdown, writer_task) =
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
    async fn actor_shutdown_closes_admission_and_drains_an_accepted_command() {
        let (root, coordinator, writer_keepalive, writer_shutdown, writer_task) =
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
    async fn publishes_through_the_real_store_and_replays_the_creation_key() {
        let catalog = catalog();
        let ledger = PublicLedgerProjection::empty();
        let initial = snapshot(&catalog, &ledger);
        let initial_digest = initial.digest.clone();
        let (_root, store, head, shutdown, task) = start_store(&catalog, initial_digest).await;
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
    async fn activation_conflict_fails_closed_and_startup_recovery_finishes_it() {
        let catalog = catalog();
        let ledger = PublicLedgerProjection::empty();
        let initial = snapshot(&catalog, &ledger);
        let initial_digest = initial.digest.clone();
        let (_root, store, head, shutdown, task) = start_store(&catalog, initial_digest).await;

        let selected =
            select_post(&catalog, &PostId::parse(PUBLISHABLE_ID).unwrap(), None).unwrap();
        let wrong_snapshot = build_candidate(
            Arc::clone(&catalog),
            embedded_manifest(),
            &ledger,
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

        let mut durable = store.startup_snapshot_state().await.unwrap();
        assert!(durable.ledger.is_empty());
        assert_eq!(durable.activating.len(), 1);
        let activation = durable.activating.pop().unwrap();
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
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(Notify::new()),
            readiness: Readiness::default(),
            cancellation: recovery_cancellation.clone(),
        };
        let recovered = recovering.recover(activation).await.unwrap();
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
        let catalog = catalog();
        let ledger = PublicLedgerProjection::empty();
        let initial = snapshot(&catalog, &ledger);
        let initial_digest = initial.digest.clone();
        let (_root, store, head, shutdown, task) = start_store(&catalog, initial_digest).await;
        let (_reader, activator) = snapshot_store(initial);
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
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(Notify::new()),
            readiness: readiness.clone(),
            cancellation: cancellation.clone(),
        };

        let pinned = select_post(&catalog, &PostId::parse(PUBLISHABLE_ID).unwrap(), None)
            .unwrap()
            .revision;
        let scheduled_at = OffsetDateTime::now_utc() + time::Duration::hours(1);
        let request = schedule_command(scheduled_at);
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
            &selected_a,
            scheduled_at,
        )
        .unwrap();
        let expected_b = build_candidate(
            Arc::clone(&candidate_b),
            embedded_manifest(),
            &ledger,
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
        let (_root, store, head, shutdown, task) = start_store(&candidate_a, initial_digest).await;
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

        let active_favicon = active
            .public_assets()
            .find(|asset| asset.asset.path.as_str() == "assets/favicon.png")
            .unwrap();
        let expected_a_favicon = expected_a
            .snapshot
            .public_assets()
            .find(|asset| asset.asset.path.as_str() == "assets/favicon.png")
            .unwrap();
        let expected_b_favicon = expected_b
            .snapshot
            .public_assets()
            .find(|asset| asset.asset.path.as_str() == "assets/favicon.png")
            .unwrap();
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
        let (root, store, head, shutdown, task) = start_store(&catalog, initial_digest).await;
        let (_reader, activator) = snapshot_store(initial);
        let content_digest = ContentTreeDigest::from_bytes([0x11; 32]);
        let mut coordinator = PublicationCoordinator {
            catalog: Arc::clone(&catalog),
            content_digest: content_digest.clone(),
            candidates: candidate_catalogs(&catalog, content_digest),
            ledger,
            site: head,
            activator,
            store: store.clone(),
            frontend: embedded_manifest(),
            source_commit: None,
            scheduled: BTreeMap::new(),
            scheduler_wakeup: Arc::new(Notify::new()),
            readiness: Readiness::new(true),
            cancellation: CancellationToken::new(),
        };

        let first = coordinator.publish_now(command()).await.unwrap();
        let revised = revised_catalog();
        let revised_pin = select_post(&revised, &PostId::parse(PUBLISHABLE_ID).unwrap(), None)
            .unwrap()
            .revision;
        coordinator
            .apply_content_catalog(revised, ContentTreeDigest::from_bytes([0x22; 32]), None)
            .await
            .unwrap();
        let update_id = Uuid::parse_str("eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee").unwrap();
        let updated = coordinator
            .publish_now(PublishNow {
                creation_key: Uuid::parse_str("ffffffff-ffff-4fff-8fff-ffffffffffff").unwrap(),
                publication_id: update_id,
                stable_post_id: PostId::parse(PUBLISHABLE_ID).unwrap(),
                expected_revision: Some(revised_pin.clone()),
                accepted_preview_digest: preview_digest(&coordinator.catalog, &coordinator.ledger),
            })
            .await
            .unwrap();
        assert_eq!(updated.revision, revised_pin);
        assert_ne!(updated.revision, first.revision);

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
        assert_eq!(
            durable
                .ledger
                .published_post(&updated.stable_post_id)
                .unwrap()
                .revision,
            updated.revision
        );
        inspection.close().await;
        drop(coordinator);
        shutdown.cancel();
        task.await.unwrap();
    }
}
