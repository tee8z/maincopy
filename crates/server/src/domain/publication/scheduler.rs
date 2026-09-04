use std::{sync::Arc, time::Duration};

use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::database::store::{DatabaseAdmissionError, DatabaseCommandError, DatabaseMutationError};

use super::{
    activation::{
        PublicationActivationError, PublicationCoordinatorHandle,
        PublicationCoordinatorUnavailable, PublishedPublication,
    },
    store::{
        PublicationRouteOwnershipError, PublicationStore, ScheduledPublication,
        StartupSnapshotLoadError,
    },
};

const RETRY_DELAY: Duration = Duration::from_millis(100);

/// Activates durable scheduled approvals when their UTC activation time arrives.
pub(crate) struct PublicationScheduler {
    store: PublicationStore,
    coordinator: PublicationCoordinatorHandle,
    wakeup: Arc<Notify>,
    cancellation: CancellationToken,
}

impl PublicationScheduler {
    pub(crate) fn new(
        store: PublicationStore,
        coordinator: PublicationCoordinatorHandle,
        wakeup: Arc<Notify>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            store,
            coordinator,
            wakeup,
            cancellation,
        }
    }

    /// Runs until cancellation or an activation outcome that cannot be retried safely.
    pub(crate) async fn run(self) -> Result<(), PublicationSchedulerError> {
        loop {
            if self.run_iteration().await? == LoopControl::Stop {
                return Ok(());
            }
        }
    }

    async fn run_iteration(&self) -> Result<LoopControl, PublicationSchedulerError> {
        match self.next_action().await? {
            SchedulerAction::Stop => Ok(LoopControl::Stop),
            SchedulerAction::Wait(delay) => Ok(self.wait(delay).await),
            SchedulerAction::Activate(publication_id) => self.activate(publication_id).await,
        }
    }

    async fn next_action(&self) -> Result<SchedulerAction, PublicationSchedulerError> {
        let next = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => return Ok(SchedulerAction::Stop),
            result = self.store.next_scheduled_publication() => {
                result.map_err(PublicationSchedulerError::Load)?
            }
        };
        Ok(scheduled_action(next, OffsetDateTime::now_utc()))
    }

    async fn wait(&self, delay: Option<Duration>) -> LoopControl {
        match wait_for_requery(delay, &self.wakeup, &self.cancellation).await {
            WaitOutcome::Requery => LoopControl::Continue,
            WaitOutcome::Cancelled => LoopControl::Stop,
        }
    }

    async fn activate(
        &self,
        publication_id: Uuid,
    ) -> Result<LoopControl, PublicationSchedulerError> {
        // Once admitted, scheduled activation must run to a known durable
        // outcome even when shutdown is requested concurrently.
        let result = self
            .coordinator
            .activate_scheduled(publication_id, OffsetDateTime::now_utc())
            .await;
        if cancelled_closed_activation(&self.cancellation, &result) {
            return Ok(LoopControl::Stop);
        }
        match result {
            Ok(_) | Err(PublicationActivationError::ReleaseBlocked { .. }) => {
                Ok(LoopControl::Continue)
            }
            Err(error) if retryable(&error) => Ok(self.wait(Some(RETRY_DELAY)).await),
            Err(source) => Err(PublicationSchedulerError::Activation {
                publication_id,
                source,
            }),
        }
    }
}

enum SchedulerAction {
    Stop,
    Wait(Option<Duration>),
    Activate(Uuid),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoopControl {
    Continue,
    Stop,
}

fn scheduled_action(
    scheduled: Option<ScheduledPublication>,
    now: OffsetDateTime,
) -> SchedulerAction {
    let Some(scheduled) = scheduled else {
        return SchedulerAction::Wait(None);
    };
    let delay = delay_until(scheduled.publication.view().scheduled_at, now);
    if delay.is_zero() {
        SchedulerAction::Activate(scheduled.publication_id)
    } else {
        SchedulerAction::Wait(Some(delay))
    }
}

fn cancelled_closed_activation(
    cancellation: &CancellationToken,
    result: &Result<PublishedPublication, PublicationActivationError>,
) -> bool {
    cancellation.is_cancelled()
        && matches!(
            result,
            Err(PublicationActivationError::Coordinator(
                PublicationCoordinatorUnavailable::Closed
            ))
        )
}

fn delay_until(scheduled_at: OffsetDateTime, now: OffsetDateTime) -> Duration {
    if scheduled_at <= now {
        return Duration::ZERO;
    }
    (scheduled_at - now).try_into().unwrap_or(Duration::MAX)
}

fn retryable(error: &PublicationActivationError) -> bool {
    matches!(
        error,
        PublicationActivationError::Database(
            DatabaseMutationError::Admission(DatabaseAdmissionError::QueueFull)
                | DatabaseMutationError::Command(
                    DatabaseCommandError::IdempotencyConflict | DatabaseCommandError::Rejected
                )
        ) | PublicationActivationError::RouteOwnership(PublicationRouteOwnershipError::Query(_))
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitOutcome {
    Requery,
    Cancelled,
}

async fn wait_for_requery(
    delay: Option<Duration>,
    wakeup: &Notify,
    cancellation: &CancellationToken,
) -> WaitOutcome {
    match delay {
        Some(delay) => {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => WaitOutcome::Cancelled,
                _ = wakeup.notified() => WaitOutcome::Requery,
                _ = tokio::time::sleep(delay) => WaitOutcome::Requery,
            }
        }
        None => {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => WaitOutcome::Cancelled,
                _ = wakeup.notified() => WaitOutcome::Requery,
            }
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum PublicationSchedulerError {
    #[error("could not load the scheduled publication queue")]
    Load(#[source] StartupSnapshotLoadError),
    #[error("could not activate scheduled publication {publication_id}")]
    Activation {
        publication_id: Uuid,
        #[source]
        source: PublicationActivationError,
    },
}

#[cfg(test)]
mod tests {
    use std::{future::Future, path::Path};

    use markdown_compiler::{
        ContentTreeDigest, PostCollection, PostId, PostSlug, resolve_content_assets,
    };
    use tokio::task::JoinHandle;

    use super::*;
    use crate::{
        config::{
            DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
            DatabaseWriterQueueCapacity,
        },
        content_fixtures::{content_tree, post, publication},
        database,
        domain::publication::{
            PublicLedgerProjection,
            activation::{PublicationCoordinator, observed_post_revisions},
            store::{
                CommandIdempotencyKey, InstallStartupSnapshot, PublicationRoute,
                SchedulePublication,
            },
        },
        frontend_assets::embedded_manifest,
        render::{
            ContentCatalog, SiteSnapshotReader, build_site_snapshot, compile_content_catalog,
            render_bound_post_revision_preview, render_site_shell, snapshot_store,
        },
        web::Readiness,
    };

    const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const PREVIEW_REPRODUCTION_PATH: &str = "/api/admin/v1/preview-assets/reproduce";

    struct SupervisedTask {
        cancellation: CancellationToken,
        task: Option<JoinHandle<()>>,
    }

    impl SupervisedTask {
        fn spawn<Start, Operation>(root: Arc<tempfile::TempDir>, start: Start) -> Self
        where
            Start: FnOnce(CancellationToken) -> Operation + Send + 'static,
            Operation: Future<Output = ()> + Send + 'static,
        {
            let cancellation = CancellationToken::new();
            let task_cancellation = cancellation.clone();
            let task = tokio::spawn(async move {
                let _root = root;
                start(task_cancellation).await;
            });
            Self {
                cancellation,
                task: Some(task),
            }
        }

        async fn stop(&mut self) {
            self.cancellation.cancel();
            self.task.take().unwrap().await.unwrap();
        }
    }

    impl Drop for SupervisedTask {
        fn drop(&mut self) {
            self.cancellation.cancel();
            if let Some(task) = self.task.take() {
                task.abort();
            }
        }
    }

    struct SchedulerFixture {
        _root: Arc<tempfile::TempDir>,
        store: PublicationStore,
        coordinator: PublicationCoordinator,
        snapshots: SiteSnapshotReader,
        writer: SupervisedTask,
    }

    impl SchedulerFixture {
        async fn start() -> Self {
            let catalog = catalog();
            let ledger = PublicLedgerProjection::empty();
            let initial = build_site_snapshot(
                render_site_shell(Arc::clone(&catalog), embedded_manifest(), &ledger).unwrap(),
                &ledger,
            )
            .unwrap();
            let initial_digest = initial.digest.clone();
            let root = Arc::new(tempfile::tempdir().unwrap());
            let database = database::bootstrap(database_configuration(
                &root.path().join("state/maincopy.db"),
            ))
            .await
            .unwrap();
            let (stores, writer) = database.into_store(8);
            let writer = SupervisedTask::spawn(Arc::clone(&root), |cancellation| async move {
                writer.run(cancellation).await.unwrap();
            });
            let store = stores.publications;
            let head = store
                .install_startup_snapshot(InstallStartupSnapshot {
                    expected: None,
                    candidate_digest: initial_digest,
                    activated_at: OffsetDateTime::now_utc() - time::Duration::hours(1),
                    source_commit: None,
                    posts: observed_post_revisions(&catalog),
                })
                .await
                .unwrap();
            let (snapshots, activator) = snapshot_store(initial);
            let content_digest = ContentTreeDigest::from_bytes([0x11; 32]);
            let coordinator = PublicationCoordinator {
                catalog: Arc::clone(&catalog),
                content_digest: content_digest.clone(),
                candidates: Arc::new(std::collections::BTreeMap::from([(
                    content_digest,
                    catalog,
                )])),
                ledger,
                site: head,
                activator,
                store: store.clone(),
                profiles: stores.profiles,
                tip_recipient: None,
                frontend: embedded_manifest(),
                source_commit: None,
                scheduled: std::collections::BTreeMap::new(),
                scheduler_wakeup: Arc::new(Notify::new()),
                readiness: Readiness::new(true),
                cancellation: CancellationToken::new(),
            };
            Self {
                _root: root,
                store,
                coordinator,
                snapshots,
                writer,
            }
        }

        async fn seed_scheduled(&mut self, scheduled_at: OffsetDateTime, key: u128) -> Uuid {
            let post_id = PostId::parse(POST_ID).unwrap();
            let rendered = self.coordinator.catalog.current_post(&post_id).unwrap();
            let revision = rendered.revision.clone();
            let accepted_preview_digest = render_bound_post_revision_preview(
                &self.coordinator.catalog,
                embedded_manifest(),
                &post_id,
                &revision,
                None,
                PREVIEW_REPRODUCTION_PATH,
                None,
            )
            .unwrap()
            .unwrap()
            .digest;
            let publication_id = fixture_uuid(key);
            let scheduled = self
                .store
                .schedule_publication(SchedulePublication {
                    creation_key: CommandIdempotencyKey::new(fixture_uuid(key + 100)),
                    publication_id,
                    stable_post_id: post_id,
                    pinned_post_digest: revision,
                    expected_revision: None,
                    expected_site: self.coordinator.site.clone(),
                    source_commit: None,
                    content_digest: self.coordinator.content_digest.clone(),
                    accepted_preview_digest,
                    slug: rendered.document.metadata.slug.clone(),
                    aliases: rendered.document.metadata.aliases.clone().into(),
                    accepted_at: scheduled_at - time::Duration::hours(1),
                    scheduled_at,
                })
                .await
                .unwrap();
            self.coordinator.scheduled.insert(publication_id, scheduled);
            publication_id
        }

        fn start_actor(self) -> RunningSchedulerFixture {
            let Self {
                _root,
                store,
                coordinator,
                snapshots,
                writer,
            } = self;
            let (handle, actor) = coordinator.into_actor(8);
            let actor = SupervisedTask::spawn(Arc::clone(&_root), |cancellation| async move {
                actor.run(cancellation).await.unwrap();
            });
            RunningSchedulerFixture {
                _root,
                store,
                handle,
                snapshots,
                actor,
                writer,
            }
        }
    }

    struct RunningSchedulerFixture {
        _root: Arc<tempfile::TempDir>,
        store: PublicationStore,
        handle: PublicationCoordinatorHandle,
        snapshots: SiteSnapshotReader,
        actor: SupervisedTask,
        writer: SupervisedTask,
    }

    impl RunningSchedulerFixture {
        async fn stop(mut self) {
            self.actor.stop().await;
            self.writer.stop().await;
        }
    }

    fn database_configuration(path: &Path) -> DatabaseConfigurationView<'_> {
        DatabaseConfigurationView {
            path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1_000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(8).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        }
    }

    fn fixture_uuid(discriminator: u128) -> Uuid {
        Uuid::from_u128(0xaaaa_aaaa_aaaa_4aaa_8aaa_0000_0000_0000 | discriminator)
    }

    fn catalog() -> Arc<ContentCatalog> {
        let tree = content_tree(
            publication(
                "publication.toml",
                "[site]\n\
                 title = \"Scheduler tests\"\n\
                 base_url = \"https://example.com/\"\n\
                 description = \"Scheduler tests.\"\n\
                 [author]\n\
                 name = \"Example Author\"\n\
                 [assets]\n\
                 allowed_https_origins = []\n"
                    .to_owned(),
            ),
            vec![post(
                "posts/scheduled.md",
                PostCollection::Posts,
                format!(
                    "+++\n\
                     id = {POST_ID:?}\n\
                     title = \"Scheduled post\"\n\
                     slug = \"scheduled-post\"\n\
                     authored_at = 2026-08-29T15:00:00-04:00\n\
                     description = \"Scheduler activation fixture.\"\n\
                     draft = false\n\
                     +++\n\
                     Scheduled publication body.\n"
                ),
            )],
            Vec::new(),
            0,
        );
        let content = tree.validate().unwrap();
        let assets = resolve_content_assets(&tree, &content).unwrap();
        Arc::new(compile_content_catalog(&content, &assets).unwrap())
    }

    #[test]
    fn retryability_is_limited_to_backpressure_and_ordinary_conflicts() {
        let queue_full =
            PublicationActivationError::Database(DatabaseAdmissionError::QueueFull.into());
        let rejected = PublicationActivationError::Database(DatabaseCommandError::Rejected.into());
        let idempotency_conflict =
            PublicationActivationError::Database(DatabaseCommandError::IdempotencyConflict.into());
        let writer_closed =
            PublicationActivationError::Database(DatabaseAdmissionError::WriterClosed.into());
        let uncertain =
            PublicationActivationError::Database(DatabaseCommandError::OutcomeUnknown.into());
        let invalid =
            PublicationActivationError::Database(DatabaseCommandError::InvalidValue.into());
        let route_query = PublicationActivationError::RouteOwnership(
            PublicationRouteOwnershipError::Query(sqlx::Error::PoolTimedOut),
        );
        let route_conflict =
            PublicationActivationError::RouteOwnership(PublicationRouteOwnershipError::Conflict {
                route: PublicationRoute::Canonical(PostSlug::parse("claimed-route").unwrap()),
            });

        assert!(retryable(&queue_full));
        assert!(retryable(&rejected));
        assert!(retryable(&idempotency_conflict));
        assert!(retryable(&route_query));
        assert!(!retryable(&writer_closed));
        assert!(!retryable(&uncertain));
        assert!(!retryable(&invalid));
        assert!(!retryable(&route_conflict));
        assert!(!retryable(
            &PublicationActivationError::DurableStateMismatch
        ));
    }

    #[tokio::test]
    async fn due_publication_activation_updates_the_snapshot_projection_and_durable_ledger() {
        let mut fixture = SchedulerFixture::start().await;
        let initial_digest = fixture.snapshots.load_full().digest.clone();
        fixture
            .seed_scheduled(OffsetDateTime::now_utc() - time::Duration::seconds(1), 1)
            .await;
        let running = fixture.start_actor();
        let cancellation = CancellationToken::new();
        let scheduler = PublicationScheduler::new(
            running.store.clone(),
            running.handle.clone(),
            running.handle.scheduler_wakeup(),
            cancellation.clone(),
        );

        assert_eq!(
            scheduler.run_iteration().await.unwrap(),
            LoopControl::Continue
        );
        let projection = running.handle.read();
        let post_id = PostId::parse(POST_ID).unwrap();
        assert_eq!(projection.ledger.len(), 1);
        assert!(projection.ledger.published_post(&post_id).is_some());
        assert_ne!(projection.site.digest, initial_digest);
        assert_eq!(running.snapshots.load_full().digest, projection.site.digest);
        let durable = running.store.startup_snapshot_state().await.unwrap();
        assert_eq!(durable.ledger, projection.ledger);
        assert!(durable.scheduled.is_empty());
        assert!(durable.activating.is_empty());
        assert_eq!(
            durable.site.as_ref().map(|site| &site.digest),
            Some(&projection.site.digest)
        );

        cancellation.cancel();
        running.stop().await;
    }

    #[tokio::test]
    async fn rejected_early_activation_waits_before_retrying_and_keeps_the_schedule() {
        let mut fixture = SchedulerFixture::start().await;
        let publication_id = fixture
            .seed_scheduled(OffsetDateTime::now_utc() + time::Duration::hours(1), 2)
            .await;
        let readiness = fixture.coordinator.readiness.clone();
        let running = fixture.start_actor();
        let cancellation = CancellationToken::new();
        let scheduler = PublicationScheduler::new(
            running.store.clone(),
            running.handle.clone(),
            running.handle.scheduler_wakeup(),
            cancellation.clone(),
        );
        tokio::time::pause();
        let started = tokio::time::Instant::now();

        assert_eq!(
            scheduler.activate(publication_id).await.unwrap(),
            LoopControl::Continue
        );
        assert!(tokio::time::Instant::now().duration_since(started) >= RETRY_DELAY);
        assert!(readiness.is_ready());
        assert!(running.handle.read().ledger.is_empty());
        // SQLx's pool acquisition deadline also uses Tokio time. Restore the
        // real clock before inspecting durable state so automatic advancement
        // cannot race a returned read connection under parallel test load.
        tokio::time::resume();
        let durable = running.store.startup_snapshot_state().await.unwrap();
        assert_eq!(durable.scheduled.len(), 1);
        assert_eq!(durable.scheduled[0].publication_id, publication_id);
        assert!(durable.activating.is_empty());

        cancellation.cancel();
        running.stop().await;
    }

    #[tokio::test]
    async fn closed_coordinator_is_fatal_unless_cancellation_is_already_requested() {
        let mut fixture = SchedulerFixture::start().await;
        let publication_id = fixture
            .seed_scheduled(OffsetDateTime::now_utc() - time::Duration::seconds(1), 3)
            .await;
        let SchedulerFixture {
            _root,
            store,
            coordinator,
            snapshots: _,
            mut writer,
        } = fixture;
        let (handle, actor) = coordinator.into_actor(1);
        drop(actor);

        let fatal = PublicationScheduler::new(
            store.clone(),
            handle.clone(),
            handle.scheduler_wakeup(),
            CancellationToken::new(),
        )
        .run()
        .await
        .unwrap_err();
        assert!(matches!(
            fatal,
            PublicationSchedulerError::Activation {
                publication_id: failed_id,
                source: PublicationActivationError::Coordinator(
                    PublicationCoordinatorUnavailable::Closed
                ),
            } if failed_id == publication_id
        ));
        assert_eq!(
            store
                .next_scheduled_publication()
                .await
                .unwrap()
                .map(|scheduled| scheduled.publication_id),
            Some(publication_id)
        );

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let stopping = PublicationScheduler::new(
            store,
            handle.clone(),
            handle.scheduler_wakeup(),
            cancellation,
        );
        assert_eq!(
            stopping.activate(publication_id).await.unwrap(),
            LoopControl::Stop
        );

        writer.stop().await;
        drop(_root);
    }

    #[tokio::test(start_paused = true)]
    async fn timed_wait_requeries_at_deadline() {
        let wakeup = Arc::new(Notify::new());
        let cancellation = CancellationToken::new();
        let task = tokio::spawn({
            let wakeup = Arc::clone(&wakeup);
            let cancellation = cancellation.clone();
            async move { wait_for_requery(Some(Duration::from_secs(60)), &wakeup, &cancellation).await }
        });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!task.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(task.await.unwrap(), WaitOutcome::Requery);
    }

    #[tokio::test(start_paused = true)]
    async fn wakeup_requeries_before_a_later_deadline() {
        let wakeup = Arc::new(Notify::new());
        let cancellation = CancellationToken::new();
        let task = tokio::spawn({
            let wakeup = Arc::clone(&wakeup);
            let cancellation = cancellation.clone();
            async move { wait_for_requery(Some(Duration::from_secs(60)), &wakeup, &cancellation).await }
        });

        tokio::task::yield_now().await;
        wakeup.notify_one();
        assert_eq!(task.await.unwrap(), WaitOutcome::Requery);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_stops_an_idle_wait() {
        let wakeup = Arc::new(Notify::new());
        let cancellation = CancellationToken::new();
        let task = tokio::spawn({
            let wakeup = Arc::clone(&wakeup);
            let cancellation = cancellation.clone();
            async move { wait_for_requery(None, &wakeup, &cancellation).await }
        });

        tokio::task::yield_now().await;
        cancellation.cancel();
        assert_eq!(task.await.unwrap(), WaitOutcome::Cancelled);
    }
}
