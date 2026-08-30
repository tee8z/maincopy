use std::fs::File;

use sqlx::{Connection as _, SqliteConnection, SqlitePool};
use thiserror::Error;
#[cfg(test)]
use tokio::sync::Barrier;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use super::store::{DatabaseAdmissionError, DatabaseMutationError};
use super::{
    BootstrappedDatabase,
    store::{DatabaseCommandError, DatabaseStore, Mutation},
};
#[cfg(test)]
use crate::domain::publication::store::{CommandIdempotencyKey, CreateTargetJob};
use crate::domain::publication::store::{
    PublicationStore, TargetJobMutationError, create as create_target_job,
};

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

        match mutation {
            Mutation::CreateTargetJob {
                command,
                respond_to,
            } => {
                let mut transaction = self
                    .connection
                    .begin()
                    .await
                    .map_err(|source| DatabaseWriterError::Begin { source })?;
                match create_target_job(&mut transaction, command).await {
                    Ok(job) => {
                        #[cfg(test)]
                        abort_at(WriterCrashPoint::AfterApplyBeforeCommit);

                        match transaction.commit().await {
                            Ok(()) => {
                                #[cfg(test)]
                                abort_at(WriterCrashPoint::AfterCommitBeforeReply);

                                let _ = respond_to.send(Ok(job));
                                Ok(())
                            }
                            Err(source) => {
                                let _ = respond_to.send(Err(DatabaseCommandError::OutcomeUnknown));
                                Err(DatabaseWriterError::Commit { source })
                            }
                        }
                    }
                    Err(TargetJobMutationError::Command(error)) => {
                        transaction
                            .rollback()
                            .await
                            .map_err(|source| DatabaseWriterError::Rollback { source })?;
                        let _ = respond_to.send(Err(error));
                        Ok(())
                    }
                    Err(TargetJobMutationError::Operation(source)) => {
                        transaction
                            .rollback()
                            .await
                            .map_err(|source| DatabaseWriterError::Rollback { source })?;
                        Err(DatabaseWriterError::Operation { source })
                    }
                    Err(TargetJobMutationError::CorruptStoredJob) => {
                        transaction
                            .rollback()
                            .await
                            .map_err(|source| DatabaseWriterError::Rollback { source })?;
                        Err(DatabaseWriterError::CorruptData {
                            entity: "target job",
                        })
                    }
                }
            }
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
        content::{PostId, PostRevisionDigest},
        database,
        domain::{
            distribution::{DistributionTarget, TargetPayload},
            publication::{TargetJob, TargetJobStatus},
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
