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
use crate::domain::auth::store::{
    AuthApplyError, AuthCommandError, AuthStore, accept_agent_proof,
    bootstrap_identity as apply_bootstrap_identity, create_browser_session, create_login_challenge,
    create_user, put_human_credential, record_admin_audit_failure, register_agent_credential,
    remove_human_credential, replace_agent_scopes, replace_user_roles, revoke_agent_credential,
    revoke_browser_session, set_user_status,
};
use crate::domain::profile::store::{
    ProfileApplyError, ProfileCommandError, ProfileStore, apply_set_tip_recipient,
    apply_update_profile,
};
#[cfg(test)]
use crate::domain::publication::store::CommandIdempotencyKey;
use crate::domain::publication::store::{
    PublicationMutationError, PublicationStore, StartupSnapshotMutationError, begin_publish_now,
    begin_scheduled_activation, finish_publication, index_content_catalog, install_startup,
    schedule_publication,
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
            DatabaseStore::new(
                AuthStore::new(readers.clone(), mutations.clone()),
                ProfileStore::new(readers.clone(), mutations.clone()),
                PublicationStore::new(readers.clone(), mutations),
            ),
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
        if applied.is_crash_test_candidate() {
            abort_at(WriterCrashPoint::AfterApplyBeforeCommit);
        }

        if let Err(source) = transaction.commit().await {
            applied.send_outcome_unknown();
            return Err(DatabaseWriterError::Commit { source });
        }

        #[cfg(test)]
        if applied.is_crash_test_candidate() {
            abort_at(WriterCrashPoint::AfterCommitBeforeReply);
        }

        applied.send_success();
        Ok(())
    }
}

struct AppliedMutation {
    respond: Box<dyn FnOnce(bool) + Send>,
    #[cfg(test)]
    crash_test_candidate: bool,
}

enum FailedMutation {
    Command(Box<dyn FnOnce() + Send>),
    Operation(sqlx::Error),
    Corrupt(&'static str),
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
    match mutation {
        Mutation::RecordAdminAuditFailure {
            command,
            respond_to,
        } => auth_response(
            respond_to,
            record_admin_audit_failure(transaction, command).await,
        ),
        Mutation::BootstrapIdentity {
            command,
            respond_to,
        } => auth_response(
            respond_to,
            apply_bootstrap_identity(transaction, command).await,
        ),
        Mutation::CreateUser {
            command,
            respond_to,
        } => auth_response(respond_to, create_user(transaction, command).await),
        Mutation::SetUserStatus {
            command,
            respond_to,
        } => auth_response(respond_to, set_user_status(transaction, command).await),
        Mutation::ReplaceUserRoles {
            command,
            respond_to,
        } => auth_response(respond_to, replace_user_roles(transaction, command).await),
        Mutation::PutHumanCredential {
            command,
            respond_to,
        } => auth_response(respond_to, put_human_credential(transaction, command).await),
        Mutation::RemoveHumanCredential {
            command,
            respond_to,
        } => auth_response(
            respond_to,
            remove_human_credential(transaction, command).await,
        ),
        Mutation::CreateLoginChallenge {
            command,
            respond_to,
        } => auth_response(
            respond_to,
            create_login_challenge(transaction, command).await,
        ),
        Mutation::CreateBrowserSession {
            command,
            respond_to,
        } => auth_response(
            respond_to,
            create_browser_session(transaction, command).await,
        ),
        Mutation::RevokeBrowserSession {
            command,
            respond_to,
        } => auth_response(
            respond_to,
            revoke_browser_session(transaction, command).await,
        ),
        Mutation::RegisterAgentCredential {
            command,
            respond_to,
        } => auth_response(
            respond_to,
            register_agent_credential(transaction, command).await,
        ),
        Mutation::ReplaceAgentScopes {
            command,
            respond_to,
        } => auth_response(respond_to, replace_agent_scopes(transaction, command).await),
        Mutation::RevokeAgentCredential {
            command,
            respond_to,
        } => auth_response(
            respond_to,
            revoke_agent_credential(transaction, command).await,
        ),
        Mutation::AcceptAgentProof {
            command,
            respond_to,
        } => auth_response(respond_to, accept_agent_proof(transaction, command).await),
        Mutation::UpdateProfile {
            command,
            respond_to,
        } => profile_response(respond_to, apply_update_profile(transaction, command).await),
        Mutation::SetTipRecipient {
            command,
            respond_to,
        } => profile_response(
            respond_to,
            apply_set_tip_recipient(transaction, command).await,
        ),
        Mutation::InstallStartupSnapshot {
            command,
            respond_to,
        } => database_response(
            respond_to,
            install_startup(transaction, command)
                .await
                .map_err(ApplyError::startup),
            false,
        ),
        Mutation::IndexContentCatalog {
            command,
            respond_to,
        } => database_response(
            respond_to,
            index_content_catalog(transaction, command)
                .await
                .map_err(ApplyError::startup),
            false,
        ),
        Mutation::BeginPublishNow {
            command,
            respond_to,
        } => database_response(
            respond_to,
            begin_publish_now(transaction, command)
                .await
                .map_err(ApplyError::publication),
            true,
        ),
        Mutation::SchedulePublication {
            command,
            respond_to,
        } => database_response(
            respond_to,
            schedule_publication(transaction, command)
                .await
                .map_err(ApplyError::publication),
            false,
        ),
        Mutation::BeginScheduledActivation {
            command,
            respond_to,
        } => database_response(
            respond_to,
            begin_scheduled_activation(transaction, command)
                .await
                .map_err(ApplyError::publication),
            false,
        ),
        Mutation::FinishPublication {
            command,
            respond_to,
        } => database_response(
            respond_to,
            finish_publication(transaction, command)
                .await
                .map_err(ApplyError::publication),
            false,
        ),
    }
}

impl AppliedMutation {
    fn new<Output, CommandError>(
        respond_to: oneshot::Sender<Result<Output, CommandError>>,
        output: Output,
        outcome_unknown: CommandError,
        _crash_test_candidate: bool,
    ) -> Self
    where
        Output: Send + 'static,
        CommandError: Send + 'static,
    {
        Self {
            respond: Box::new(move |committed| {
                let result = if committed {
                    Ok(output)
                } else {
                    Err(outcome_unknown)
                };
                let _ = respond_to.send(result);
            }),
            #[cfg(test)]
            crash_test_candidate: _crash_test_candidate,
        }
    }

    fn send_success(self) {
        (self.respond)(true);
    }

    fn send_outcome_unknown(self) {
        (self.respond)(false);
    }

    #[cfg(test)]
    const fn is_crash_test_candidate(&self) -> bool {
        self.crash_test_candidate
    }
}

fn auth_response<Output>(
    respond_to: oneshot::Sender<Result<Output, AuthCommandError>>,
    result: Result<Output, AuthApplyError>,
) -> Result<AppliedMutation, FailedMutation>
where
    Output: Send + 'static,
{
    match result {
        Ok(output) => Ok(AppliedMutation::new(
            respond_to,
            output,
            AuthCommandError::OutcomeUnknown,
            false,
        )),
        Err(AuthApplyError::Command(error)) => Err(command_failure(respond_to, error)),
        Err(AuthApplyError::Operation(source)) => Err(FailedMutation::Operation(source)),
        Err(AuthApplyError::CorruptStoredState) => Err(FailedMutation::Corrupt("admin identity")),
    }
}

fn profile_response<Output>(
    respond_to: oneshot::Sender<Result<Output, ProfileCommandError>>,
    result: Result<Output, ProfileApplyError>,
) -> Result<AppliedMutation, FailedMutation>
where
    Output: Send + 'static,
{
    match result {
        Ok(output) => Ok(AppliedMutation::new(
            respond_to,
            output,
            ProfileCommandError::OutcomeUnknown,
            true,
        )),
        Err(ProfileApplyError::Command(error)) => Err(command_failure(respond_to, error)),
        Err(ProfileApplyError::Operation(source)) => Err(FailedMutation::Operation(source)),
        Err(ProfileApplyError::CorruptStoredState) => Err(FailedMutation::Corrupt("user profile")),
    }
}

fn database_response<Output>(
    respond_to: oneshot::Sender<Result<Output, DatabaseCommandError>>,
    result: Result<Output, ApplyError>,
    crash_test_candidate: bool,
) -> Result<AppliedMutation, FailedMutation>
where
    Output: Send + 'static,
{
    match result {
        Ok(output) => Ok(AppliedMutation::new(
            respond_to,
            output,
            DatabaseCommandError::OutcomeUnknown,
            crash_test_candidate,
        )),
        Err(ApplyError::Command(error)) => Err(command_failure(respond_to, error)),
        Err(ApplyError::Operation(source)) => Err(FailedMutation::Operation(source)),
        Err(ApplyError::Corrupt(entity)) => Err(FailedMutation::Corrupt(entity)),
    }
}

fn command_failure<Output, CommandError>(
    respond_to: oneshot::Sender<Result<Output, CommandError>>,
    error: CommandError,
) -> FailedMutation
where
    Output: Send + 'static,
    CommandError: Send + 'static,
{
    FailedMutation::Command(Box::new(move || {
        let _ = respond_to.send(Err(error));
    }))
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
        match self {
            Self::Command(respond) => {
                respond();
                Ok(())
            }
            Self::Operation(source) => Err(DatabaseWriterError::Operation { source }),
            Self::Corrupt(entity) => Err(DatabaseWriterError::CorruptData { entity }),
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
        future::Future as _,
        path::{Path, PathBuf},
        process::Command,
        sync::Arc,
        task::{Context, Poll, Waker},
    };

    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::{DatabaseBusyTimeout, DatabaseReadPoolSize, DatabaseWriterQueueCapacity},
        database,
        domain::publication::store::{
            BeginPublishNow, BeginScheduledActivation, FinishPublication, InstallStartupSnapshot,
            ObservedPostRevision, PublicationRoute, PublicationRouteOwnershipError,
            PublishNowState, SchedulePublication, SiteHead,
        },
    };
    use markdown_compiler::{
        ContentTreeDigest, DraftStatus, PostAlias, PostId, PostRevisionDigest, PostSlug,
        PreviewDigest, SiteSnapshotDigest,
    };

    const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const OTHER_POST_ID: &str = "22222222-2222-4222-8222-222222222222";
    const REVISION_BYTES: [u8; 32] = [0x11; 32];
    const PUBLICATION_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const OTHER_PUBLICATION_ID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const COMMAND_ID: &str = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    const OTHER_COMMAND_ID: &str = "ffffffff-ffff-4fff-8fff-ffffffffffff";
    const CRASH_DATABASE_ENV: &str = "MAINCOPY_TEST_WRITER_CRASH_DATABASE";
    const CRASH_HELPER_TEST: &str = "database::writer::tests::begin_publication_crash_process";

    fn configuration(path: &Path) -> crate::config::DatabaseConfigurationView<'_> {
        crate::config::DatabaseConfigurationView {
            path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1_000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(8).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        }
    }

    async fn database_with_initial_snapshot() -> (tempfile::TempDir, PathBuf, BootstrappedDatabase)
    {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/maincopy.db");
        let mut database = database::bootstrap(configuration(&path)).await.unwrap();
        sqlx::query(
            "INSERT INTO site_revisions (\
                site_revision_digest, version, activated_at_ns\
             ) VALUES (?, 1, 10)",
        )
        .bind(site_digest(0x10).as_bytes().as_slice())
        .execute(&mut database._writer)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO site_state (singleton, current_site_digest, version) VALUES (1, ?, 1)",
        )
        .bind(site_digest(0x10).as_bytes().as_slice())
        .execute(&mut database._writer)
        .await
        .unwrap();
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

    fn content_digest(byte: u8) -> ContentTreeDigest {
        ContentTreeDigest::from_bytes([byte; 32])
    }

    fn preview_digest(byte: u8) -> PreviewDigest {
        PreviewDigest::from_bytes([byte; 32])
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
            expected_revision: None,
            expected_site,
            source_commit: None,
            content_digest: content_digest(0x44),
            accepted_preview_digest: preview_digest(0x55),
            now: OffsetDateTime::from_unix_timestamp(now).unwrap(),
            candidate_site_digest: candidate,
        }
    }

    fn schedule_command(
        creation_key: &str,
        publication_id: &str,
        expected_site: SiteHead,
        retained_content: ContentTreeDigest,
    ) -> SchedulePublication {
        SchedulePublication {
            creation_key: CommandIdempotencyKey::new(uuid(creation_key)),
            publication_id: uuid(publication_id),
            stable_post_id: PostId::parse(POST_ID).unwrap(),
            pinned_post_digest: PostRevisionDigest::from_bytes(REVISION_BYTES),
            expected_revision: None,
            expected_site,
            source_commit: None,
            content_digest: retained_content,
            accepted_preview_digest: preview_digest(0x55),
            slug: PostSlug::parse("first").unwrap(),
            aliases: Arc::from([
                PostAlias::parse("former-first").unwrap(),
                PostAlias::parse("legacy-first").unwrap(),
            ]),
            accepted_at: OffsetDateTime::from_unix_timestamp(20).unwrap(),
            scheduled_at: OffsetDateTime::from_unix_timestamp(30).unwrap(),
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
            aliases: Arc::from([
                PostAlias::parse("former-first").unwrap(),
                PostAlias::parse("legacy-first").unwrap(),
            ]),
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

    async fn count_canonical_publications(database: &mut BootstrappedDatabase) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM canonical_publications")
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
    async fn immediate_publication_is_recoverable_and_idempotent() {
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
        assert_eq!(begun.content_digest, content_digest(0x44));
        assert_eq!(begun.accepted_preview_digest, preview_digest(0x55));

        let mut activating_replay = begin_publication(
            COMMAND_ID,
            OTHER_PUBLICATION_ID,
            initial.clone(),
            site_digest(0x99),
            99,
        );
        activating_replay.content_digest = content_digest(0x99);
        let PublishNowState::Activating(activating_replay) = store
            .publications
            .begin_publish_now(activating_replay)
            .await
            .unwrap()
        else {
            panic!("an activating retry must remain activating");
        };
        assert_eq!(activating_replay.content_digest, content_digest(0x44));
        assert_eq!(
            activating_replay.accepted_preview_digest,
            preview_digest(0x55)
        );

        let recovery = store.publications.startup_snapshot_state().await.unwrap();
        assert!(recovery.ledger.is_empty());
        assert_eq!(recovery.activating.len(), 1);
        assert_eq!(recovery.activating[0].candidate_site_digest, candidate);
        assert_eq!(recovery.activating[0].content_digest, content_digest(0x44));
        assert_eq!(
            recovery.activating[0].accepted_preview_digest,
            preview_digest(0x55)
        );

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
        assert_eq!(finished.accepted_preview_digest, preview_digest(0x55));

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

        let mut replay = begin_publication(
            COMMAND_ID,
            OTHER_PUBLICATION_ID,
            initial,
            site_digest(0x99),
            99,
        );
        replay.content_digest = content_digest(0x99);
        let begin_retry = store.publications.begin_publish_now(replay).await.unwrap();
        let PublishNowState::Published(begin_retry) = begin_retry else {
            panic!("creation-key retry must return the committed publication");
        };
        let finished_view = finished.publication.view();
        assert_eq!(begin_retry.publication_id, finished.publication_id);
        assert_eq!(begin_retry.stable_post_id, finished_view.stable_post_id);
        assert_eq!(begin_retry.revision, finished_view.pinned_post_digest);
        assert_eq!(
            begin_retry.accepted_preview_digest,
            finished.accepted_preview_digest
        );
        assert_eq!(
            begin_retry.published_at,
            finished_view.published_at.unwrap()
        );
        assert_eq!(begin_retry.site, finished.site);

        let mut conflicting_replay = begin_publication(
            COMMAND_ID,
            OTHER_PUBLICATION_ID,
            finished.site.clone(),
            site_digest(0x98),
            98,
        );
        conflicting_replay.accepted_preview_digest = preview_digest(0x56);
        assert_eq!(
            store
                .publications
                .begin_publish_now(conflicting_replay)
                .await,
            Err(DatabaseMutationError::Command(
                DatabaseCommandError::IdempotencyConflict
            ))
        );

        stop_writer(shutdown, task).await;
        drop(store);
        let mut reopened = database::bootstrap(configuration(&path)).await.unwrap();
        let routes: Vec<(String, Vec<u8>, Vec<u8>, String)> = sqlx::query_as(
            "SELECT route, stable_post_id, revision_digest, kind \
             FROM publication_routes ORDER BY route",
        )
        .fetch_all(&mut reopened._writer)
        .await
        .unwrap();
        assert_eq!(routes.len(), 3);
        for (_, post_id, revision, _) in &routes {
            assert_eq!(post_id, uuid(POST_ID).as_bytes());
            assert_eq!(revision, &REVISION_BYTES);
        }
        assert_eq!(routes[0].0, "first");
        assert_eq!(routes[0].3, "post");
        assert_eq!(routes[1].0, "former-first");
        assert_eq!(routes[1].3, "alias");
        assert_eq!(routes[2].0, "legacy-first");
        assert_eq!(routes[2].3, "alias");
        let canonical: (String, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) = sqlx::query_as(
            "SELECT state, creation_key, activation_site_digest, content_tree_digest, \
                    accepted_preview_digest \
             FROM canonical_publications WHERE publication_id = ?",
        )
        .bind(uuid(PUBLICATION_ID).as_bytes().as_slice())
        .fetch_one(&mut reopened._writer)
        .await
        .unwrap();
        assert_eq!(canonical.0, "published");
        assert_eq!(canonical.1, uuid(COMMAND_ID).as_bytes());
        assert_eq!(canonical.2, candidate.as_bytes());
        assert_eq!(canonical.3, content_digest(0x44).as_bytes());
        assert_eq!(canonical.4, preview_digest(0x55).as_bytes());
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn publication_finish_rejects_a_route_claimed_after_activation_began() {
        let (_root, _path, database) = empty_database().await;
        let (store, writer) = database.into_store(8);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));

        let mut startup = startup_command(None, site_digest(0x31));
        startup.posts.push(ObservedPostRevision {
            stable_post_id: PostId::parse(OTHER_POST_ID).unwrap(),
            revision_digest: PostRevisionDigest::from_bytes([0x22; 32]),
            publication_status: DraftStatus::Publishable,
            slug: PostSlug::parse("second").unwrap(),
        });
        let initial = store
            .publications
            .install_startup_snapshot(startup)
            .await
            .unwrap();
        let candidate = site_digest(0x32);
        let activating = store
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
        assert!(matches!(activating, PublishNowState::Activating(_)));

        let mut competing = schedule_command(
            OTHER_COMMAND_ID,
            OTHER_PUBLICATION_ID,
            initial.clone(),
            content_digest(0x52),
        );
        competing.stable_post_id = PostId::parse(OTHER_POST_ID).unwrap();
        competing.pinned_post_digest = PostRevisionDigest::from_bytes([0x22; 32]);
        competing.slug = PostSlug::parse("second").unwrap();
        competing.aliases = Arc::from([PostAlias::parse("legacy-first").unwrap()]);
        store
            .publications
            .schedule_publication(competing)
            .await
            .unwrap();

        assert_eq!(
            store
                .publications
                .finish_publication(finish_publication_command(
                    PUBLICATION_ID,
                    initial.clone(),
                    candidate,
                ))
                .await,
            Err(DatabaseMutationError::Command(
                DatabaseCommandError::Rejected
            ))
        );
        let durable = store.publications.startup_snapshot_state().await.unwrap();
        assert_eq!(durable.site, Some(initial));
        assert_eq!(durable.activating.len(), 1);
        assert_eq!(durable.scheduled.len(), 1);
        assert!(
            store
                .publications
                .ensure_routes_available(
                    &PostId::parse(POST_ID).unwrap(),
                    &PostSlug::parse("first").unwrap(),
                    &[PostAlias::parse("former-first").unwrap()],
                )
                .await
                .is_ok()
        );
        assert!(matches!(
            store
                .publications
                .ensure_routes_available(
                    &PostId::parse(POST_ID).unwrap(),
                    &PostSlug::parse("unclaimed").unwrap(),
                    &[PostAlias::parse("legacy-first").unwrap()],
                )
                .await,
            Err(PublicationRouteOwnershipError::Conflict {
                route: PublicationRoute::Alias(alias),
            }) if alias.as_str() == "legacy-first"
        ));

        stop_writer(shutdown, task).await;
        drop(store);
    }

    #[tokio::test]
    async fn scheduled_publication_reserves_routes_and_replays_retained_content() {
        let (_root, path, database) = empty_database().await;
        let (store, writer) = database.into_store(8);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));

        let mut startup = startup_command(None, site_digest(0x21));
        startup.posts.push(ObservedPostRevision {
            stable_post_id: PostId::parse(OTHER_POST_ID).unwrap(),
            revision_digest: PostRevisionDigest::from_bytes([0x22; 32]),
            publication_status: DraftStatus::Publishable,
            slug: PostSlug::parse("second").unwrap(),
        });
        let initial = store
            .publications
            .install_startup_snapshot(startup)
            .await
            .unwrap();
        let retained = content_digest(0x51);
        let scheduled = store
            .publications
            .schedule_publication(schedule_command(
                COMMAND_ID,
                PUBLICATION_ID,
                initial.clone(),
                retained.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(scheduled.content_digest, retained);
        assert_eq!(scheduled.accepted_preview_digest, preview_digest(0x55));
        assert!(matches!(
            store
                .publications
                .ensure_routes_available(
                    &PostId::parse(OTHER_POST_ID).unwrap(),
                    &PostSlug::parse("unrelated").unwrap(),
                    &[PostAlias::parse("former-first").unwrap()],
                )
                .await,
            Err(PublicationRouteOwnershipError::Conflict {
                route: PublicationRoute::Alias(alias),
            }) if alias.as_str() == "former-first"
        ));

        let mut route_conflict = schedule_command(
            OTHER_COMMAND_ID,
            OTHER_PUBLICATION_ID,
            initial.clone(),
            content_digest(0x54),
        );
        route_conflict.stable_post_id = PostId::parse(OTHER_POST_ID).unwrap();
        route_conflict.pinned_post_digest = PostRevisionDigest::from_bytes([0x22; 32]);
        route_conflict.slug = PostSlug::parse("second").unwrap();
        route_conflict.aliases = Arc::from([
            PostAlias::parse("new-reservation").unwrap(),
            PostAlias::parse("former-first").unwrap(),
        ]);
        assert_eq!(
            store
                .publications
                .schedule_publication(route_conflict)
                .await,
            Err(DatabaseMutationError::Command(
                DatabaseCommandError::Rejected
            ))
        );
        assert!(
            store
                .publications
                .ensure_routes_available(
                    &PostId::parse("33333333-3333-4333-8333-333333333333").unwrap(),
                    &PostSlug::parse("second").unwrap(),
                    &[PostAlias::parse("new-reservation").unwrap()],
                )
                .await
                .is_ok()
        );
        let replay = store
            .publications
            .schedule_publication(schedule_command(
                COMMAND_ID,
                OTHER_PUBLICATION_ID,
                initial.clone(),
                content_digest(0x52),
            ))
            .await
            .unwrap();
        assert_eq!(replay, scheduled);
        let mut preview_conflict = schedule_command(
            COMMAND_ID,
            OTHER_PUBLICATION_ID,
            initial.clone(),
            content_digest(0x53),
        );
        preview_conflict.accepted_preview_digest = preview_digest(0x56);
        assert_eq!(
            store
                .publications
                .schedule_publication(preview_conflict)
                .await,
            Err(DatabaseMutationError::Command(
                DatabaseCommandError::IdempotencyConflict
            ))
        );
        let mut conflicting = schedule_command(
            COMMAND_ID,
            OTHER_PUBLICATION_ID,
            initial.clone(),
            content_digest(0x53),
        );
        conflicting.scheduled_at = OffsetDateTime::from_unix_timestamp(31).unwrap();
        assert_eq!(
            store.publications.schedule_publication(conflicting).await,
            Err(DatabaseMutationError::Command(
                DatabaseCommandError::IdempotencyConflict
            ))
        );
        let next = store
            .publications
            .next_scheduled_publication()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.content_digest, retained);
        assert_eq!(next.accepted_preview_digest, preview_digest(0x55));

        let begun = store
            .publications
            .begin_scheduled_activation(BeginScheduledActivation {
                publication_id: uuid(PUBLICATION_ID),
                expected_publication_version: 1,
                expected_site: initial,
                now: OffsetDateTime::from_unix_timestamp(30).unwrap(),
                candidate_site_digest: site_digest(0x22),
            })
            .await
            .unwrap();
        assert_eq!(begun.content_digest, retained);
        assert_eq!(begun.accepted_preview_digest, preview_digest(0x55));

        stop_writer(shutdown, task).await;
        drop(store);
        let mut reopened = database::bootstrap(configuration(&path)).await.unwrap();
        let reserved_routes: i64 = sqlx::query_scalar("SELECT count(*) FROM publication_routes")
            .fetch_one(&mut reopened._writer)
            .await
            .unwrap();
        assert_eq!(reserved_routes, 3);
        let persisted: (Vec<u8>, Vec<u8>) = sqlx::query_as(
            "SELECT content_tree_digest, accepted_preview_digest \
             FROM canonical_publications WHERE publication_id = ?",
        )
        .bind(uuid(PUBLICATION_ID).as_bytes().as_slice())
        .fetch_one(&mut reopened._writer)
        .await
        .unwrap();
        assert_eq!(persisted.0, retained.as_bytes());
        assert_eq!(persisted.1, preview_digest(0x55).as_bytes());
        let (reopened_store, reopened_writer) = reopened.into_store(8);
        let reopened_shutdown = CancellationToken::new();
        let reopened_task = tokio::spawn(reopened_writer.run(reopened_shutdown.clone()));
        assert!(matches!(
            reopened_store
                .publications
                .ensure_routes_available(
                    &PostId::parse(OTHER_POST_ID).unwrap(),
                    &PostSlug::parse("unrelated").unwrap(),
                    &[PostAlias::parse("former-first").unwrap()],
                )
                .await,
            Err(PublicationRouteOwnershipError::Conflict {
                route: PublicationRoute::Alias(alias),
            }) if alias.as_str() == "former-first"
        ));
        stop_writer(reopened_shutdown, reopened_task).await;
        drop(reopened_store);
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
    async fn begin_publication_crash_process() {
        let Some(path) = std::env::var_os(CRASH_DATABASE_ENV) else {
            return;
        };
        let path = PathBuf::from(path);
        let database = database::bootstrap(configuration(&path)).await.unwrap();
        let (handle, writer) = database.into_store(1);
        let _task = tokio::spawn(writer.run(CancellationToken::new()));
        let response = handle
            .publications
            .begin_publish_now(begin_publication(
                COMMAND_ID,
                PUBLICATION_ID,
                SiteHead {
                    digest: site_digest(0x10),
                    version: 1,
                },
                site_digest(0x20),
                20,
            ))
            .await;

        panic!("writer did not abort: response={response:?}");
    }

    #[tokio::test]
    async fn begin_publication_recovers_at_both_crash_boundaries() {
        for (crash_point, committed_rows) in [
            (WriterCrashPoint::AfterApplyBeforeCommit, 0),
            (WriterCrashPoint::AfterCommitBeforeReply, 1),
        ] {
            let (_root, path, database) = database_with_initial_snapshot().await;
            database.close().await.unwrap();

            run_crash_process(&path, crash_point);

            let mut reopened = database::bootstrap(configuration(&path)).await.unwrap();
            assert_eq!(
                count_canonical_publications(&mut reopened).await,
                committed_rows
            );

            if crash_point == WriterCrashPoint::AfterCommitBeforeReply {
                let (handle, writer) = reopened.into_store(1);
                let shutdown = CancellationToken::new();
                let task = tokio::spawn(writer.run(shutdown.clone()));
                let retry = handle
                    .publications
                    .begin_publish_now(begin_publication(
                        COMMAND_ID,
                        PUBLICATION_ID,
                        SiteHead {
                            digest: site_digest(0x10),
                            version: 1,
                        },
                        site_digest(0x20),
                        20,
                    ))
                    .await
                    .unwrap();
                let PublishNowState::Activating(retry) = retry else {
                    panic!("a committed immediate publication must replay as activating");
                };
                assert_eq!(retry.publication_id, uuid(PUBLICATION_ID));
                assert_eq!(retry.candidate_site_digest, site_digest(0x20));
                stop_writer(shutdown, task).await;
                drop(handle);

                let mut reopened = database::bootstrap(configuration(&path)).await.unwrap();
                assert_eq!(count_canonical_publications(&mut reopened).await, 1);
                reopened.close().await.unwrap();
            } else {
                reopened.close().await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn a_dropped_response_does_not_cancel_an_accepted_write() {
        let (_root, _path, database) = database_with_initial_snapshot().await;
        let control = WriterTestControl::new();
        let dequeued = control.dequeued.clone();
        let release = control.release.clone();
        let (handle, writer) = database.into_store_with_control(4, control);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));

        let abandoned_handle = handle.clone();
        let abandoned = tokio::spawn(async move {
            abandoned_handle
                .publications
                .begin_publish_now(begin_publication(
                    COMMAND_ID,
                    PUBLICATION_ID,
                    SiteHead {
                        digest: site_digest(0x10),
                        version: 1,
                    },
                    site_digest(0x20),
                    20,
                ))
                .await
        });
        dequeued.wait().await;
        abandoned.abort();
        assert!(abandoned.await.unwrap_err().is_cancelled());
        release.wait().await;

        let replay = handle
            .publications
            .begin_publish_now(begin_publication(
                COMMAND_ID,
                PUBLICATION_ID,
                SiteHead {
                    digest: site_digest(0x10),
                    version: 1,
                },
                site_digest(0x20),
                20,
            ))
            .await
            .unwrap();
        let PublishNowState::Activating(replay) = replay else {
            panic!("the accepted publication must commit after its receiver is dropped");
        };
        assert_eq!(replay.publication_id, uuid(PUBLICATION_ID));

        stop_writer(shutdown, task).await;
    }

    #[tokio::test]
    async fn command_failure_rolls_back_and_writer_continues() {
        let (_root, path, database) = database_with_initial_snapshot().await;
        let (handle, writer) = database.into_store(4);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));

        let rejected = handle
            .publications
            .begin_publish_now(begin_publication(
                COMMAND_ID,
                PUBLICATION_ID,
                SiteHead {
                    digest: site_digest(0x99),
                    version: 1,
                },
                site_digest(0x20),
                20,
            ))
            .await;
        assert_eq!(
            rejected,
            Err(DatabaseMutationError::Command(
                DatabaseCommandError::Rejected
            ))
        );

        let accepted = handle
            .publications
            .begin_publish_now(begin_publication(
                COMMAND_ID,
                PUBLICATION_ID,
                SiteHead {
                    digest: site_digest(0x10),
                    version: 1,
                },
                site_digest(0x20),
                20,
            ))
            .await
            .unwrap();
        assert!(matches!(accepted, PublishNowState::Activating(_)));
        stop_writer(shutdown, task).await;
        drop(handle);

        let mut reopened = database::bootstrap(configuration(&path)).await.unwrap();
        let key_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM canonical_publications WHERE creation_key = ?",
        )
        .bind(uuid(COMMAND_ID).as_bytes().as_slice())
        .fetch_one(&mut reopened._writer)
        .await
        .unwrap();
        assert_eq!(key_count, 1);
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn bounded_queue_rejects_full_then_drains_accepted_commands_on_shutdown() {
        let (_root, path, database) = empty_database().await;
        let control = WriterTestControl::new();
        let dequeued = control.dequeued.clone();
        let release = control.release.clone();
        let (handle, writer) = database.into_store_with_control(1, control);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(writer.run(shutdown.clone()));

        let first_handle = handle.clone();
        let first = tokio::spawn(async move {
            first_handle
                .publications
                .install_startup_snapshot(startup_command(None, site_digest(0x11)))
                .await
        });
        dequeued.wait().await;
        let mut second = Box::pin(
            handle
                .publications
                .install_startup_snapshot(startup_command(None, site_digest(0x22))),
        );
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(second.as_mut().poll(&mut context), Poll::Pending));
        assert_eq!(
            handle
                .publications
                .install_startup_snapshot(startup_command(None, site_digest(0x33)))
                .await,
            Err(DatabaseMutationError::Admission(
                DatabaseAdmissionError::QueueFull
            ))
        );

        shutdown.cancel();
        release.wait().await;
        first.await.unwrap().unwrap();
        assert_eq!(
            second.await,
            Err(DatabaseMutationError::Command(
                DatabaseCommandError::Rejected
            ))
        );
        task.await.unwrap().unwrap();
        assert_eq!(
            handle
                .publications
                .install_startup_snapshot(startup_command(None, site_digest(0x44)))
                .await,
            Err(DatabaseMutationError::Admission(
                DatabaseAdmissionError::WriterClosed
            ))
        );
        drop(handle);

        let mut reopened = database::bootstrap(configuration(&path)).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM site_revisions")
            .fetch_one(&mut reopened._writer)
            .await
            .unwrap();
        assert_eq!(count, 1);
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn losing_every_handle_stops_the_writer_as_a_failure() {
        let (_root, _path, database) = empty_database().await;
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
