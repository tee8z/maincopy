use std::fs::File;

use sqlx::{Connection as _, Sqlite, SqliteConnection, SqlitePool, Transaction};
use thiserror::Error;
#[cfg(test)]
use tokio::sync::Barrier;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use super::store::{DatabaseAdmissionError, DatabaseMutationError};
use super::{
    BootstrappedDatabase,
    store::{DatabaseCommandError, DatabaseStore, Mutation},
};
use crate::domain::publication::store::{
    BeginPublishNowResult, CreateTargetJobResult, FinishPublicationResult, FinishedPublication,
    InstallStartupSnapshotResult, PublicationMutationError, PublicationStore, PublishNowState,
    SiteHead, StartupSnapshotMutationError, StoredTargetJob, TargetJobMutationError,
    begin_publish_now, create as create_target_job, finish_publication, install_startup,
};
#[cfg(test)]
use crate::domain::publication::store::{CommandIdempotencyKey, CreateTargetJob};

pub(crate) struct DatabaseWriter {
    connection: SqliteConnection,
    readers: SqlitePool,
    ownership_lock: File,
    mutations: mpsc::Receiver<Mutation>,
    #[cfg(test)]
    control: Option<WriterTestControl>,
}

impl BootstrappedDatabase {
    pub(crate) fn into_store(self, capacity: usize) -> (DatabaseStore, DatabaseWriter) {
        let Self {
            _writer: connection,
            _readers: readers,
            _ownership_lock: ownership_lock,
        } = self;
        let (mutations, receiver) = mpsc::channel(capacity);
        (
            DatabaseStore::new(PublicationStore::new(readers.clone(), mutations)),
            DatabaseWriter {
                connection,
                readers,
                ownership_lock,
                mutations: receiver,
                #[cfg(test)]
                control: None,
            },
        )
    }

    #[cfg(test)]
    fn into_store_with_control(
        self,
        capacity: usize,
        control: WriterTestControl,
    ) -> (DatabaseStore, DatabaseWriter) {
        let (store, mut writer) = self.into_store(capacity);
        writer.control = Some(control);
        (store, writer)
    }
}

impl DatabaseWriter {
    pub(crate) async fn run(
        mut self,
        shutdown: CancellationToken,
    ) -> Result<(), DatabaseWriterError> {
        let processing = self.process_until_shutdown(shutdown).await;
        self.mutations.close();

        let Self {
            connection,
            readers,
            ownership_lock,
            mutations: _,
            #[cfg(test)]
                control: _,
        } = self;
        readers.close().await;
        let close = connection.close().await;
        drop(ownership_lock);

        match (processing, close) {
            (Err(error), Err(close_source)) => {
                tracing::error!(
                    error = %close_source,
                    "database writer close failed after task failure"
                );
                Err(error)
            }
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(source)) => Err(DatabaseWriterError::Close { source }),
        }
    }

    async fn process_until_shutdown(
        &mut self,
        shutdown: CancellationToken,
    ) -> Result<(), DatabaseWriterError> {
        loop {
            let mutation = tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    self.mutations.close();
                    break;
                }
                mutation = self.mutations.recv() => {
                    mutation.ok_or(DatabaseWriterError::MutationChannelClosed)?
                }
            };
            self.execute(mutation).await?;
        }

        while let Some(mutation) = self.mutations.recv().await {
            self.execute(mutation).await?;
        }
        Ok(())
    }

    async fn execute(&mut self, mutation: Mutation) -> Result<(), DatabaseWriterError> {
        #[cfg(test)]
        if let Some(control) = self.control.take() {
            control.dequeued.wait().await;
            control.release.wait().await;
        }
        let mut transaction = self
            .connection
            .begin()
            .await
            .map_err(|source| DatabaseWriterError::Begin { source })?;
        let applied = match apply_mutation(&mut transaction, mutation).await {
            Ok(applied) => applied,
            Err(failed) => {
                transaction
                    .rollback()
                    .await
                    .map_err(|source| DatabaseWriterError::Rollback { source })?;
                return failed.finish();
            }
        };

        #[cfg(test)]
        if applied.output.is_target_creation() {
            abort_at(WriterCrashPoint::AfterApplyBeforeCommit);
        }

        if let Err(source) = transaction.commit().await {
            applied
                .responder
                .send_error(DatabaseCommandError::OutcomeUnknown);
            return Err(DatabaseWriterError::Commit { source });
        }

        #[cfg(test)]
        if applied.output.is_target_creation() {
            abort_at(WriterCrashPoint::AfterCommitBeforeReply);
        }

        applied.responder.send_success(applied.output);
        Ok(())
    }
}

struct AppliedMutation {
    responder: MutationResponder,
    output: MutationOutput,
}

struct FailedMutation {
    responder: MutationResponder,
    error: ApplyError,
}

enum MutationResponder {
    Startup(oneshot::Sender<InstallStartupSnapshotResult>),
    Target(oneshot::Sender<CreateTargetJobResult>),
    BeginPublication(oneshot::Sender<BeginPublishNowResult>),
    FinishPublication(oneshot::Sender<FinishPublicationResult>),
}

enum MutationOutput {
    Startup(SiteHead),
    Target(StoredTargetJob),
    BeginPublication(PublishNowState),
    FinishPublication(FinishedPublication),
}

enum ApplyError {
    Command(DatabaseCommandError),
    Operation(sqlx::Error),
    Corrupt(&'static str),
}

async fn apply_mutation(
    transaction: &mut Transaction<'_, Sqlite>,
    mutation: Mutation,
) -> Result<AppliedMutation, FailedMutation> {
    let (responder, result) = match mutation {
        Mutation::InstallStartupSnapshot {
            command,
            respond_to,
        } => (
            MutationResponder::Startup(respond_to),
            install_startup(transaction, command)
                .await
                .map(MutationOutput::Startup)
                .map_err(ApplyError::startup),
        ),
        Mutation::CreateTargetJob {
            command,
            respond_to,
        } => (
            MutationResponder::Target(respond_to),
            create_target_job(transaction, command)
                .await
                .map(MutationOutput::Target)
                .map_err(ApplyError::target),
        ),
        Mutation::BeginPublishNow {
            command,
            respond_to,
        } => (
            MutationResponder::BeginPublication(respond_to),
            begin_publish_now(transaction, command)
                .await
                .map(MutationOutput::BeginPublication)
                .map_err(ApplyError::publication),
        ),
        Mutation::FinishPublication {
            command,
            respond_to,
        } => (
            MutationResponder::FinishPublication(respond_to),
            finish_publication(transaction, command)
                .await
                .map(MutationOutput::FinishPublication)
                .map_err(ApplyError::publication),
        ),
    };
    match result {
        Ok(output) => Ok(AppliedMutation { responder, output }),
        Err(error) => Err(FailedMutation { responder, error }),
    }
}

impl MutationResponder {
    fn send_success(self, output: MutationOutput) {
        match (self, output) {
            (Self::Startup(sender), MutationOutput::Startup(value)) => {
                let _ = sender.send(Ok(value));
            }
            (Self::Target(sender), MutationOutput::Target(value)) => {
                let _ = sender.send(Ok(value));
            }
            (Self::BeginPublication(sender), MutationOutput::BeginPublication(value)) => {
                let _ = sender.send(Ok(value));
            }
            (Self::FinishPublication(sender), MutationOutput::FinishPublication(value)) => {
                let _ = sender.send(Ok(value));
            }
            _ => unreachable!("mutation responder and output were constructed together"),
        }
    }

    fn send_error(self, error: DatabaseCommandError) {
        match self {
            Self::Startup(sender) => {
                let _ = sender.send(Err(error));
            }
            Self::Target(sender) => {
                let _ = sender.send(Err(error));
            }
            Self::BeginPublication(sender) => {
                let _ = sender.send(Err(error));
            }
            Self::FinishPublication(sender) => {
                let _ = sender.send(Err(error));
            }
        }
    }
}

impl MutationOutput {
    #[cfg(test)]
    const fn is_target_creation(&self) -> bool {
        matches!(self, Self::Target(_))
    }
}

impl ApplyError {
    fn startup(error: StartupSnapshotMutationError) -> Self {
        match error {
            StartupSnapshotMutationError::Command(error) => Self::Command(error),
            StartupSnapshotMutationError::Operation(source) => Self::Operation(source),
            StartupSnapshotMutationError::CorruptStoredState => {
                Self::Corrupt("startup publication state")
            }
        }
    }

    fn target(error: TargetJobMutationError) -> Self {
        match error {
            TargetJobMutationError::Command(error) => Self::Command(error),
            TargetJobMutationError::Operation(source) => Self::Operation(source),
            TargetJobMutationError::CorruptStoredJob => Self::Corrupt("target job"),
        }
    }

    fn publication(error: PublicationMutationError) -> Self {
        match error {
            PublicationMutationError::Command(error) => Self::Command(error),
            PublicationMutationError::Operation(source) => Self::Operation(source),
            PublicationMutationError::CorruptStoredState => Self::Corrupt("canonical publication"),
        }
    }
}

impl FailedMutation {
    fn finish(self) -> Result<(), DatabaseWriterError> {
        match self.error {
            ApplyError::Command(error) => {
                self.responder.send_error(error);
                Ok(())
            }
            ApplyError::Operation(source) => Err(DatabaseWriterError::Operation { source }),
            ApplyError::Corrupt(entity) => Err(DatabaseWriterError::CorruptData { entity }),
        }
    }
}

#[cfg(test)]
const CRASH_POINT_ENV: &str = "MAINCOPY_TEST_WRITER_CRASH_POINT";

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WriterCrashPoint {
    AfterApplyBeforeCommit,
    AfterCommitBeforeReply,
}

#[cfg(test)]
impl WriterCrashPoint {
    const fn name(self) -> &'static str {
        match self {
            Self::AfterApplyBeforeCommit => "after-apply-before-commit",
            Self::AfterCommitBeforeReply => "after-commit-before-reply",
        }
    }
}

#[cfg(test)]
fn abort_at(reached: WriterCrashPoint) {
    if std::env::var(CRASH_POINT_ENV).as_deref() == Ok(reached.name()) {
        eprintln!("writer crash point reached: {reached:?}");
        std::process::abort();
    }
}

#[cfg(test)]
struct WriterTestControl {
    dequeued: std::sync::Arc<Barrier>,
    release: std::sync::Arc<Barrier>,
}

#[cfg(test)]
impl WriterTestControl {
    fn new() -> Self {
        Self {
            dequeued: std::sync::Arc::new(Barrier::new(2)),
            release: std::sync::Arc::new(Barrier::new(2)),
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum DatabaseWriterError {
    #[error("all database store handles closed unexpectedly")]
    MutationChannelClosed,
    #[error("database transaction could not begin")]
    Begin {
        #[source]
        source: sqlx::Error,
    },
    #[error("database command execution failed")]
    Operation {
        #[source]
        source: sqlx::Error,
    },
    #[error("persisted {entity} data is invalid")]
    CorruptData { entity: &'static str },
    #[error("database transaction rollback failed")]
    Rollback {
        #[source]
        source: sqlx::Error,
    },
    #[error("database transaction commit result is unknown")]
    Commit {
        #[source]
        source: sqlx::Error,
    },
    #[error("database writer connection failed to close")]
    Close {
        #[source]
        source: sqlx::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        process::Command,
    };

    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::{DatabaseBusyTimeout, DatabaseReadPoolSize, DatabaseWriterQueueCapacity},
        content::{DraftStatus, PostId, PostRevisionDigest, PostSlug, SiteSnapshotDigest},
        database,
        domain::{
            distribution::{DistributionTarget, TargetPayload},
            publication::{
                TargetJob, TargetJobStatus,
                store::{
                    BeginPublishNow, FinishPublication, InstallStartupSnapshot,
                    ObservedPostRevision, PublishNowState, SiteHead,
                },
            },
        },
    };

    const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const OTHER_POST_ID: &str = "22222222-2222-4222-8222-222222222222";
    const REVISION: &str =
        "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111";
    const REVISION_BYTES: [u8; 32] = [0x11; 32];
    const PUBLICATION_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const OTHER_PUBLICATION_ID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const JOB_ID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    const OTHER_JOB_ID: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    const COMMAND_ID: &str = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    const OTHER_COMMAND_ID: &str = "ffffffff-ffff-4fff-8fff-ffffffffffff";
    const CRASH_DATABASE_ENV: &str = "MAINCOPY_TEST_WRITER_CRASH_DATABASE";
    const CRASH_HELPER_TEST: &str = "database::writer::tests::create_target_job_crash_process";

    fn configuration(path: &Path) -> crate::config::DatabaseConfigurationView<'_> {
        crate::config::DatabaseConfigurationView {
            path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1_000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(8).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        }
    }

    async fn database_with_canonical() -> (tempfile::TempDir, PathBuf, BootstrappedDatabase) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/maincopy.db");
        let mut database = database::bootstrap(configuration(&path)).await.unwrap();
        sqlx::query(
            "INSERT INTO post_revisions (\
                stable_post_id, revision_digest, slug, publication_status, first_observed_at_ns\
             ) VALUES (?, ?, 'first', 'publishable', 1)",
        )
        .bind(uuid(POST_ID).as_bytes().as_slice())
        .bind(REVISION_BYTES.as_slice())
        .execute(&mut database._writer)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO canonical_publications (\
                publication_id, stable_post_id, pinned_post_digest, state, \
                scheduled_at_ns, version\
             ) VALUES (?, ?, ?, 'scheduled', 10, 1)",
        )
        .bind(uuid(PUBLICATION_ID).as_bytes().as_slice())
        .bind(uuid(POST_ID).as_bytes().as_slice())
        .bind(REVISION_BYTES.as_slice())
        .execute(&mut database._writer)
        .await
        .unwrap();
        (root, path, database)
    }

    async fn empty_database() -> (tempfile::TempDir, PathBuf, BootstrappedDatabase) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/maincopy.db");
        let database = database::bootstrap(configuration(&path)).await.unwrap();
        (root, path, database)
    }

    fn uuid(value: &str) -> Uuid {
        Uuid::parse_str(value).unwrap()
    }

    fn create_command(
        command_id: &str,
        job_id: &str,
        publication_id: &str,
        body: &str,
    ) -> CreateTargetJob {
        create_command_for_post(command_id, job_id, publication_id, POST_ID, body)
    }

    fn create_command_for_post(
        command_id: &str,
        job_id: &str,
        publication_id: &str,
        post_id: &str,
        body: &str,
    ) -> CreateTargetJob {
        CreateTargetJob {
            idempotency_key: CommandIdempotencyKey::new(uuid(command_id)),
            publication_job_id: uuid(job_id),
            publication_id: uuid(publication_id),
            job: TargetJob::waiting(
                DistributionTarget::X,
                PostId::parse(post_id).unwrap(),
                PostRevisionDigest::parse(REVISION).unwrap(),
                OffsetDateTime::from_unix_timestamp(10).unwrap(),
                TargetPayload::new(body).unwrap(),
            ),
        }
    }

    fn startup_command(
        expected: Option<SiteHead>,
        candidate: SiteSnapshotDigest,
    ) -> InstallStartupSnapshot {
        InstallStartupSnapshot {
            expected,
            candidate_digest: candidate,
            activated_at: OffsetDateTime::from_unix_timestamp(10).unwrap(),
            source_commit: None,
            posts: vec![ObservedPostRevision {
                stable_post_id: PostId::parse(POST_ID).unwrap(),
                revision_digest: PostRevisionDigest::from_bytes(REVISION_BYTES),
                publication_status: DraftStatus::Publishable,
                slug: PostSlug::parse("first").unwrap(),
            }],
        }
    }

    fn site_digest(byte: u8) -> SiteSnapshotDigest {
        SiteSnapshotDigest::from_bytes([byte; 32])
    }

    fn begin_publication(
        creation_key: &str,
        publication_id: &str,
        expected_site: SiteHead,
        candidate: SiteSnapshotDigest,
        now: i64,
    ) -> BeginPublishNow {
        BeginPublishNow {
            creation_key: CommandIdempotencyKey::new(uuid(creation_key)),
            publication_id: uuid(publication_id),
            stable_post_id: PostId::parse(POST_ID).unwrap(),
            pinned_post_digest: PostRevisionDigest::from_bytes(REVISION_BYTES),
            expected_site,
            source_commit: None,
            now: OffsetDateTime::from_unix_timestamp(now).unwrap(),
            candidate_site_digest: candidate,
        }
    }

    fn finish_publication_command(
        publication_id: &str,
        expected_site: SiteHead,
        candidate: SiteSnapshotDigest,
    ) -> FinishPublication {
        FinishPublication {
            publication_id: uuid(publication_id),
            expected_publication_version: 2,
            expected_site,
            candidate_site_digest: candidate,
            slug: PostSlug::parse("first").unwrap(),
        }
    }

    async fn stop_writer(
        shutdown: CancellationToken,
        task: tokio::task::JoinHandle<Result<(), DatabaseWriterError>>,
    ) {
        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    fn run_crash_process(path: &Path, crash_point: WriterCrashPoint) {
        let output = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(CRASH_HELPER_TEST)
            .arg("--nocapture")
            .env(CRASH_DATABASE_ENV, path)
            .env(CRASH_POINT_ENV, crash_point.name())
            .output()
            .unwrap();

        assert!(
            !output.status.success(),
            "crash process unexpectedly survived:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("writer crash point reached: {crash_point:?}")),
            "crash process failed before reaching {crash_point:?}:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    async fn count_target_jobs(database: &mut BootstrappedDatabase) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM publication_jobs")
            .fetch_one(&mut database._writer)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn startup_snapshot_install_is_idempotent_cas_guarded_and_reuses_history() {
        let (_root, path, database) = empty_database().await;
        let (store, writer) = database.into_store(4);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));

        let first = store
            .publications
            .install_startup_snapshot(startup_command(None, site_digest(0x11)))
            .await
            .unwrap();
        assert_eq!(first.version, 1);
        assert_eq!(
            store
                .publications
                .startup_snapshot_state()
                .await
                .unwrap()
                .site,
            Some(first.clone())
        );

        let retry = store
            .publications
            .install_startup_snapshot(startup_command(None, first.digest.clone()))
            .await
            .unwrap();
        assert_eq!(retry, first);

        let second = store
            .publications
            .install_startup_snapshot(startup_command(Some(first.clone()), site_digest(0x22)))
            .await
            .unwrap();
        assert_eq!(second.version, 2);

        let restored = store
            .publications
            .install_startup_snapshot(startup_command(Some(second.clone()), first.digest.clone()))
            .await
            .unwrap();
        assert_eq!(restored.version, 3);
        assert_eq!(restored.digest, first.digest);

        let stale = store
            .publications
            .install_startup_snapshot(startup_command(Some(first), site_digest(0x33)))
            .await;
        assert_eq!(
            stale,
            Err(DatabaseMutationError::Command(
                DatabaseCommandError::Rejected
            ))
        );
        assert_eq!(
            store
                .publications
                .startup_snapshot_state()
                .await
                .unwrap()
                .site,
            Some(restored)
        );

        stop_writer(shutdown, task).await;
        drop(store);
        let mut reopened = database::bootstrap(configuration(&path)).await.unwrap();
        let site_revisions: i64 = sqlx::query_scalar("SELECT count(*) FROM site_revisions")
            .fetch_one(&mut reopened._writer)
            .await
            .unwrap();
        let post_revisions: i64 = sqlx::query_scalar("SELECT count(*) FROM post_revisions")
            .fetch_one(&mut reopened._writer)
            .await
            .unwrap();
        assert_eq!(site_revisions, 2);
        assert_eq!(post_revisions, 1);
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn immediate_publication_is_recoverable_idempotent_and_releases_waiting_jobs() {
        let (_root, path, database) = empty_database().await;
        let (store, writer) = database.into_store(8);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));

        let initial = store
            .publications
            .install_startup_snapshot(startup_command(None, site_digest(0x10)))
            .await
            .unwrap();
        let candidate = site_digest(0x20);
        let begun = store
            .publications
            .begin_publish_now(begin_publication(
                COMMAND_ID,
                PUBLICATION_ID,
                initial.clone(),
                candidate.clone(),
                20,
            ))
            .await
            .unwrap();
        let PublishNowState::Activating(begun) = begun else {
            panic!("new publication must be activating");
        };
        assert_eq!(begun.publication.view().version, 2);
        assert_eq!(
            begun
                .publication
                .view()
                .activation_started_at
                .unwrap()
                .unix_timestamp(),
            20
        );
        assert_eq!(begun.candidate_site_digest, candidate);

        let recovery = store.publications.startup_snapshot_state().await.unwrap();
        assert!(recovery.ledger.is_empty());
        assert_eq!(recovery.activating.len(), 1);
        assert_eq!(recovery.activating[0].candidate_site_digest, candidate);

        store
            .publications
            .create_target_job(create_command(
                OTHER_COMMAND_ID,
                JOB_ID,
                PUBLICATION_ID,
                "publish copy",
            ))
            .await
            .unwrap();

        let finished = store
            .publications
            .finish_publication(finish_publication_command(
                PUBLICATION_ID,
                initial.clone(),
                candidate.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(finished.publication.view().version, 3);
        assert_eq!(
            finished
                .publication
                .view()
                .published_at
                .unwrap()
                .unix_timestamp(),
            20
        );
        assert_eq!(finished.site.digest, candidate);
        assert_eq!(finished.site.version, 2);

        let job = store
            .publications
            .target_job(uuid(JOB_ID))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(job.status, TargetJobStatus::Ready(_)));
        let state = store.publications.startup_snapshot_state().await.unwrap();
        assert!(state.activating.is_empty());
        assert_eq!(state.ledger.len(), 1);
        assert_eq!(state.site, Some(finished.site.clone()));

        let finish_retry = store
            .publications
            .finish_publication(finish_publication_command(
                PUBLICATION_ID,
                initial.clone(),
                candidate.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(finish_retry, finished);

        let begin_retry = store
            .publications
            .begin_publish_now(begin_publication(
                COMMAND_ID,
                OTHER_PUBLICATION_ID,
                initial,
                site_digest(0x99),
                99,
            ))
            .await
            .unwrap();
        let PublishNowState::Published(begin_retry) = begin_retry else {
            panic!("creation-key retry must return the committed publication");
        };
        assert_eq!(begin_retry, finished);

        stop_writer(shutdown, task).await;
        drop(store);
        let mut reopened = database::bootstrap(configuration(&path)).await.unwrap();
        let route: (Vec<u8>, Vec<u8>, String) = sqlx::query_as(
            "SELECT stable_post_id, revision_digest, kind \
             FROM published_routes WHERE route = 'first'",
        )
        .fetch_one(&mut reopened._writer)
        .await
        .unwrap();
        assert_eq!(route.0, uuid(POST_ID).as_bytes());
        assert_eq!(route.1, REVISION_BYTES);
        assert_eq!(route.2, "post");
        let canonical: (String, Vec<u8>, Vec<u8>) = sqlx::query_as(
            "SELECT state, creation_key, activation_site_digest \
             FROM canonical_publications WHERE publication_id = ?",
        )
        .bind(uuid(PUBLICATION_ID).as_bytes().as_slice())
        .fetch_one(&mut reopened._writer)
        .await
        .unwrap();
        assert_eq!(canonical.0, "published");
        assert_eq!(canonical.1, uuid(COMMAND_ID).as_bytes());
        assert_eq!(canonical.2, candidate.as_bytes());
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn publication_begin_guards_head_history_concurrency_and_creation_identity() {
        let (_root, _path, database) = empty_database().await;
        let (store, writer) = database.into_store(8);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));
        let first = store
            .publications
            .install_startup_snapshot(startup_command(None, site_digest(0x31)))
            .await
            .unwrap();
        let current = store
            .publications
            .install_startup_snapshot(startup_command(Some(first.clone()), site_digest(0x32)))
            .await
            .unwrap();

        for rejected in [
            begin_publication(
                COMMAND_ID,
                PUBLICATION_ID,
                first.clone(),
                site_digest(0x33),
                20,
            ),
            begin_publication(
                COMMAND_ID,
                PUBLICATION_ID,
                current.clone(),
                first.digest.clone(),
                20,
            ),
        ] {
            assert_eq!(
                store.publications.begin_publish_now(rejected).await,
                Err(DatabaseMutationError::Command(
                    DatabaseCommandError::Rejected
                ))
            );
        }

        store
            .publications
            .begin_publish_now(begin_publication(
                COMMAND_ID,
                PUBLICATION_ID,
                current.clone(),
                site_digest(0x34),
                20,
            ))
            .await
            .unwrap();
        assert_eq!(
            store
                .publications
                .begin_publish_now(begin_publication(
                    OTHER_COMMAND_ID,
                    OTHER_PUBLICATION_ID,
                    current.clone(),
                    site_digest(0x35),
                    21,
                ))
                .await,
            Err(DatabaseMutationError::Command(
                DatabaseCommandError::Rejected
            ))
        );

        let mut conflicting = begin_publication(
            COMMAND_ID,
            OTHER_PUBLICATION_ID,
            current,
            site_digest(0x36),
            22,
        );
        conflicting.stable_post_id = PostId::parse(OTHER_POST_ID).unwrap();
        assert_eq!(
            store.publications.begin_publish_now(conflicting).await,
            Err(DatabaseMutationError::Command(
                DatabaseCommandError::IdempotencyConflict
            ))
        );
        stop_writer(shutdown, task).await;
    }

    #[tokio::test]
    async fn publication_begin_rejects_a_draft_revision_without_consuming_the_key() {
        let (_root, _path, database) = empty_database().await;
        let (store, writer) = database.into_store(4);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));
        let mut install = startup_command(None, site_digest(0x40));
        install.posts[0].publication_status = DraftStatus::Draft;
        let initial = store
            .publications
            .install_startup_snapshot(install)
            .await
            .unwrap();

        assert_eq!(
            store
                .publications
                .begin_publish_now(begin_publication(
                    COMMAND_ID,
                    PUBLICATION_ID,
                    initial,
                    site_digest(0x41),
                    20,
                ))
                .await,
            Err(DatabaseMutationError::Command(
                DatabaseCommandError::Rejected
            ))
        );
        stop_writer(shutdown, task).await;
    }

    #[tokio::test]
    async fn create_target_job_crash_process() {
        let Some(path) = std::env::var_os(CRASH_DATABASE_ENV) else {
            return;
        };
        let path = PathBuf::from(path);
        let database = database::bootstrap(configuration(&path)).await.unwrap();
        let (handle, writer) = database.into_store(1);
        let _task = tokio::spawn(writer.run(CancellationToken::new()));
        let response = handle
            .publications
            .admit_create_target_job(create_command(COMMAND_ID, JOB_ID, PUBLICATION_ID, "copy"))
            .unwrap();

        let response = response.await;
        panic!("writer did not abort: response={response:?}");
    }

    #[tokio::test]
    async fn create_target_job_recovers_at_both_crash_boundaries() {
        for (crash_point, committed_rows) in [
            (WriterCrashPoint::AfterApplyBeforeCommit, 0),
            (WriterCrashPoint::AfterCommitBeforeReply, 1),
        ] {
            let (_root, path, database) = database_with_canonical().await;
            database.close().await.unwrap();

            run_crash_process(&path, crash_point);

            let mut reopened = database::bootstrap(configuration(&path)).await.unwrap();
            assert_eq!(count_target_jobs(&mut reopened).await, committed_rows);

            if crash_point == WriterCrashPoint::AfterCommitBeforeReply {
                let (handle, writer) = reopened.into_store(1);
                let shutdown = CancellationToken::new();
                let task = tokio::spawn(writer.run(shutdown.clone()));
                let retry = handle
                    .publications
                    .admit_create_target_job(create_command(
                        COMMAND_ID,
                        JOB_ID,
                        PUBLICATION_ID,
                        "copy",
                    ))
                    .unwrap()
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(retry.publication_job_id, uuid(JOB_ID));
                stop_writer(shutdown, task).await;
                drop(handle);

                let mut reopened = database::bootstrap(configuration(&path)).await.unwrap();
                assert_eq!(count_target_jobs(&mut reopened).await, 1);
                reopened.close().await.unwrap();
            } else {
                reopened.close().await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn create_retry_is_idempotent_and_conflicting_reuse_is_rejected() {
        let (_root, _path, database) = database_with_canonical().await;
        let (handle, writer) = database.into_store(4);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));

        let first = handle
            .publications
            .create_target_job(create_command(COMMAND_ID, JOB_ID, PUBLICATION_ID, "copy"))
            .await
            .unwrap();
        let retry = handle
            .publications
            .admit_create_target_job(create_command(COMMAND_ID, JOB_ID, PUBLICATION_ID, "copy"))
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, retry);
        assert_eq!(
            handle
                .publications
                .target_job(uuid(JOB_ID))
                .await
                .unwrap()
                .unwrap(),
            first
        );

        let conflict = handle
            .publications
            .create_target_job(create_command(
                COMMAND_ID,
                OTHER_JOB_ID,
                PUBLICATION_ID,
                "changed copy",
            ))
            .await;
        assert_eq!(
            conflict,
            Err(DatabaseMutationError::Command(
                DatabaseCommandError::IdempotencyConflict
            ))
        );

        stop_writer(shutdown, task).await;
    }

    #[tokio::test]
    async fn a_dropped_response_does_not_cancel_an_accepted_write() {
        let (_root, _path, database) = database_with_canonical().await;
        let (handle, writer) = database.into_store(4);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));

        let abandoned = handle
            .publications
            .admit_create_target_job(create_command(COMMAND_ID, JOB_ID, PUBLICATION_ID, "copy"))
            .unwrap();
        drop(abandoned);
        let retry = handle
            .publications
            .admit_create_target_job(create_command(COMMAND_ID, JOB_ID, PUBLICATION_ID, "copy"))
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retry.publication_job_id, uuid(JOB_ID));

        stop_writer(shutdown, task).await;
    }

    #[tokio::test]
    async fn command_failure_rolls_back_and_writer_continues() {
        let (_root, path, database) = database_with_canonical().await;
        let (handle, writer) = database.into_store(4);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));

        let rejected = handle
            .publications
            .admit_create_target_job(create_command(
                COMMAND_ID,
                JOB_ID,
                OTHER_PUBLICATION_ID,
                "copy",
            ))
            .unwrap()
            .await
            .unwrap();
        assert_eq!(rejected, Err(DatabaseCommandError::Rejected));

        handle
            .publications
            .admit_create_target_job(create_command(
                OTHER_COMMAND_ID,
                OTHER_JOB_ID,
                PUBLICATION_ID,
                "valid copy",
            ))
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        stop_writer(shutdown, task).await;
        drop(handle);

        let mut reopened = database::bootstrap(configuration(&path)).await.unwrap();
        let failed_keys: i64 =
            sqlx::query_scalar("SELECT count(*) FROM publication_jobs WHERE idempotency_key = ?")
                .bind(uuid(COMMAND_ID).as_bytes().as_slice())
                .fetch_one(&mut reopened._writer)
                .await
                .unwrap();
        let job_count: i64 = sqlx::query_scalar("SELECT count(*) FROM publication_jobs")
            .fetch_one(&mut reopened._writer)
            .await
            .unwrap();
        assert_eq!(failed_keys, 0);
        assert_eq!(job_count, 1);
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn canonical_identity_mismatch_is_rejected_without_consuming_the_key() {
        let (_root, _path, database) = database_with_canonical().await;
        let (handle, writer) = database.into_store(4);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));

        let rejected = handle
            .publications
            .admit_create_target_job(create_command_for_post(
                COMMAND_ID,
                JOB_ID,
                PUBLICATION_ID,
                OTHER_POST_ID,
                "copy",
            ))
            .unwrap()
            .await
            .unwrap();
        assert_eq!(rejected, Err(DatabaseCommandError::Rejected));

        let accepted = handle
            .publications
            .admit_create_target_job(create_command(COMMAND_ID, JOB_ID, PUBLICATION_ID, "copy"))
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(accepted.publication_job_id, uuid(JOB_ID));

        stop_writer(shutdown, task).await;
    }

    #[tokio::test]
    async fn retry_returns_the_job_after_its_state_has_advanced() {
        let (_root, path, database) = database_with_canonical().await;
        let (handle, writer) = database.into_store(4);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));

        handle
            .publications
            .admit_create_target_job(create_command(COMMAND_ID, JOB_ID, PUBLICATION_ID, "copy"))
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        stop_writer(shutdown, task).await;
        drop(handle);

        let mut reopened = database::bootstrap(configuration(&path)).await.unwrap();
        sqlx::query(
            "UPDATE publication_jobs \
             SET state = 'scheduled', version = 2 \
             WHERE publication_job_id = ?",
        )
        .bind(uuid(JOB_ID).as_bytes().as_slice())
        .execute(&mut reopened._writer)
        .await
        .unwrap();

        let (handle, writer) = reopened.into_store(4);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));
        let retry = handle
            .publications
            .admit_create_target_job(create_command(COMMAND_ID, JOB_ID, PUBLICATION_ID, "copy"))
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(retry.status, TargetJobStatus::Scheduled(_)));

        stop_writer(shutdown, task).await;
    }

    #[tokio::test]
    async fn bounded_queue_rejects_full_then_drains_accepted_commands_on_shutdown() {
        let (_root, path, database) = database_with_canonical().await;
        let control = WriterTestControl::new();
        let dequeued = control.dequeued.clone();
        let release = control.release.clone();
        let (handle, writer) = database.into_store_with_control(1, control);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));

        let first = handle
            .publications
            .admit_create_target_job(create_command(COMMAND_ID, JOB_ID, PUBLICATION_ID, "first"))
            .unwrap();
        dequeued.wait().await;
        let second = handle
            .publications
            .admit_create_target_job(create_command(
                OTHER_COMMAND_ID,
                OTHER_JOB_ID,
                PUBLICATION_ID,
                "second",
            ))
            .unwrap();
        assert!(matches!(
            handle.publications.admit_create_target_job(create_command(
                "12345678-1234-4234-8234-123456789abc",
                "23456789-2345-4345-8345-23456789abcd",
                PUBLICATION_ID,
                "third",
            )),
            Err(DatabaseAdmissionError::QueueFull)
        ));

        shutdown.cancel();
        release.wait().await;
        assert!(first.await.unwrap().is_ok());
        assert_eq!(second.await.unwrap(), Err(DatabaseCommandError::Rejected));
        task.await.unwrap().unwrap();
        assert!(matches!(
            handle.publications.admit_create_target_job(create_command(
                "3456789a-3456-4456-8456-3456789abcde",
                "456789ab-4567-4567-8567-456789abcdef",
                PUBLICATION_ID,
                "late",
            )),
            Err(DatabaseAdmissionError::WriterClosed)
        ));
        drop(handle);

        let mut reopened = database::bootstrap(configuration(&path)).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM publication_jobs")
            .fetch_one(&mut reopened._writer)
            .await
            .unwrap();
        assert_eq!(count, 1);
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn losing_every_handle_stops_the_writer_as_a_failure() {
        let (_root, _path, database) = database_with_canonical().await;
        let (handle, writer) = database.into_store(1);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown));
        drop(handle);

        assert!(matches!(
            task.await.unwrap(),
            Err(DatabaseWriterError::MutationChannelClosed)
        ));
    }
}
