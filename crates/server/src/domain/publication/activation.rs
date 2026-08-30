use std::sync::Arc;

use thiserror::Error;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    content::{
        DraftStatus, PostId, PostRevisionDigest, PostSlug, PublishedPostRevision,
        SiteSnapshotDigest, SourceCommit,
    },
    database::store::{DatabaseCommandError, DatabaseMutationError},
    frontend_assets::FrontendAssetManifest,
    render::{
        ContentCatalog, PublicLedgerProjection, SiteSnapshot, SiteSnapshotActivator,
        SiteSnapshotBuildError, build_site_snapshot, render_site_shell,
    },
    web::Readiness,
};

use super::store::{
    BeginPublishNow, BegunPublication, CommandIdempotencyKey, FinishPublication,
    FinishedPublication, LookupPublishNow, PublicationStore, PublishNowLookupError,
    PublishNowState, RecoverablePublicationActivation, SiteHead,
};

/// Owns the serialized transition from durable publication intent to public visibility.
///
/// Startup wraps this concrete value in one mutex. Keeping the mutable ledger, site head,
/// and snapshot activator together prevents requests from observing or constructing a second
/// in-process publication transition.
pub(crate) struct PublicationCoordinator {
    pub catalog: Arc<ContentCatalog>,
    pub ledger: PublicLedgerProjection,
    pub site: SiteHead,
    pub activator: SiteSnapshotActivator,
    pub store: PublicationStore,
    pub frontend: &'static FrontendAssetManifest,
    pub source_commit: Option<SourceCommit>,
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
}

/// The exact durable publication produced by the coordinator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublishedPublication {
    pub publication_id: Uuid,
    pub stable_post_id: PostId,
    pub revision: PostRevisionDigest,
    pub published_at: OffsetDateTime,
    pub site: SiteHead,
}

impl PublicationCoordinator {
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
            Some(PublishNowState::Published(finished)) => return published_result(finished),
            Some(PublishNowState::Activating(begun)) => {
                let mut safety = FailClosedGuard::new(&self.readiness, &self.cancellation);
                let selected = select_stored_post(&self.catalog, begun.publication.view())?;
                let candidate = self.candidate_for_begun(
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
        let now = OffsetDateTime::now_utc();
        let prebuilt = build_candidate(
            Arc::clone(&self.catalog),
            self.frontend,
            &self.ledger,
            &selected,
            now,
        )?;
        let mut safety = FailClosedGuard::new(&self.readiness, &self.cancellation);
        let requested = BeginPublishNow {
            creation_key: CommandIdempotencyKey::new(command.creation_key),
            publication_id: command.publication_id,
            stable_post_id: selected.stable_post_id.clone(),
            pinned_post_digest: selected.revision.clone(),
            expected_revision: command.expected_revision,
            expected_site: self.site.clone(),
            source_commit: self.source_commit.clone(),
            now,
            candidate_site_digest: prebuilt.snapshot.digest.clone(),
        };

        let begun = match self.store.begin_publish_now(requested).await {
            Ok(PublishNowState::Activating(begun)) => begun,
            Ok(PublishNowState::Published(finished)) => {
                if !prebuilt.already_published {
                    return Err(PublicationActivationError::DurableStateMismatch);
                }
                let result = validate_finished(finished, &selected)?;
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
        let candidate = self.candidate_for_begun(
            &selected,
            begun.publication.view(),
            &begun.candidate_site_digest,
            Some(prebuilt),
        )?;
        self.activate_and_finish(begun, selected, candidate, &mut safety)
            .await
    }

    /// Reconciles a durable `Activating` publication before listeners are bound.
    pub(crate) async fn recover(
        &mut self,
        activation: RecoverablePublicationActivation,
    ) -> Result<PublishedPublication, PublicationActivationError> {
        let mut safety = FailClosedGuard::new(&self.readiness, &self.cancellation);
        let view = activation.publication.view();
        let selected = select_stored_post(&self.catalog, view)?;
        let candidate =
            self.candidate_for_begun(&selected, view, &activation.candidate_site_digest, None)?;
        let begun = BegunPublication {
            publication_id: activation.publication_id,
            publication: activation.publication,
            site: self.site.clone(),
            candidate_site_digest: activation.candidate_site_digest,
        };
        self.activate_and_finish(begun, selected, candidate, &mut safety)
            .await
    }

    fn candidate_for_begun(
        &self,
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
                Arc::clone(&self.catalog),
                self.frontend,
                &self.ledger,
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
        if result.site.digest != candidate.digest {
            return Err(PublicationActivationError::DurableStateMismatch);
        }
        self.ledger = candidate.ledger;
        self.site = result.site.clone();
        safety.disarm();
        Ok(result)
    }
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
    let (ledger, already_published) = match current.with_published(PublishedPostRevision::new(
        selected.stable_post_id.clone(),
        selected.revision.clone(),
        published_at,
    )) {
        Ok(ledger) => (ledger, false),
        Err(_) => (current.clone(), true),
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
    let result = published_result(finished)?;
    if result.stable_post_id != selected.stable_post_id
        || result.revision != selected.revision
        || !current_matches
    {
        return Err(PublicationActivationError::DurableStateMismatch);
    }
    Ok(result)
}

fn published_result(
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
        published_at,
        site: finished.site,
    })
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
    #[error("post {post_id} is not present in the current content catalog")]
    PostNotFound { post_id: PostId },
    #[error("post {post_id} is still a draft")]
    DraftPost { post_id: PostId },
    #[error("post {post_id} changed from expected revision {expected} to {current}")]
    StaleRevision {
        post_id: PostId,
        expected: Box<PostRevisionDigest>,
        current: Box<PostRevisionDigest>,
    },
    #[error("post {post_id} is already published")]
    AlreadyPublished { post_id: PostId },
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
    Database(#[from] DatabaseMutationError),
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tokio::task::JoinHandle;

    use super::*;
    use crate::{
        config::{
            DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
            DatabaseWriterQueueCapacity,
        },
        content::{
            DiscoveredContentTree, DiscoveredPost, PostCollection, resolve_content_assets,
            tree::{post, publication},
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
        PublishNow {
            creation_key: Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap(),
            publication_id: Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap(),
            stable_post_id: PostId::parse(PUBLISHABLE_ID).unwrap(),
            expected_revision: None,
        }
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
        let mut coordinator = PublicationCoordinator {
            catalog: Arc::clone(&catalog),
            ledger,
            site: head,
            activator,
            store: store.clone(),
            frontend: embedded_manifest(),
            source_commit: None,
            readiness: readiness.clone(),
            cancellation: cancellation.clone(),
        };

        let request = command();
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
        let mut interrupted = PublicationCoordinator {
            catalog: Arc::clone(&catalog),
            ledger: ledger.clone(),
            site: head,
            activator: wrong_activator,
            store: store.clone(),
            frontend: embedded_manifest(),
            source_commit: None,
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
        let mut recovering = PublicationCoordinator {
            catalog,
            ledger: durable.ledger,
            site: durable.site.unwrap(),
            activator,
            store: store.clone(),
            frontend: embedded_manifest(),
            source_commit: None,
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
}
