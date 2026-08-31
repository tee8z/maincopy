use sqlx::{Executor, FromRow, Sqlite, Transaction, error::ErrorKind};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::{
    content::{
        ContentTreeDigest, DraftStatus, PostId, PostRevisionDigest, PostSlug, PreviewDigest,
        SiteSnapshotDigest, SourceCommit,
    },
    database::store::{
        DatabaseAdmissionError, DatabaseCommandError, DatabaseMutationError, Mutation,
    },
    domain::distribution::{DistributionTarget, PayloadError, TargetPayload, TargetPayloadDigest},
};

use super::{
    ActivationBlockReason, CanonicalPublication, CanonicalPublicationStatus,
    CanonicalPublicationView, CanonicalState, RehydrationError, ReleasedTargetJob, TargetJob,
    TargetJobState, TargetJobStatus, TargetJobView, PublicLedgerProjection,
    PublishedPostRevision, canonical, target,
};

const MAX_STARTUP_POST_REVISIONS: usize = 10_000;

/// Publication queries and mutations backed by Maincopy's database.
#[derive(Clone)]
pub(crate) struct PublicationStore {
    readers: sqlx::SqlitePool,
    mutations: mpsc::Sender<Mutation>,
}

impl PublicationStore {
    pub(crate) const fn new(readers: sqlx::SqlitePool, mutations: mpsc::Sender<Mutation>) -> Self {
        Self { readers, mutations }
    }

    /// Loads the exact durable publication view used to build the startup snapshot.
    pub(crate) async fn startup_snapshot_state(
        &self,
    ) -> Result<StartupSnapshotState, StartupSnapshotLoadError> {
        let mut transaction = self.readers.begin().await?;
        let reload_states: Vec<String> =
            sqlx::query_scalar("SELECT state FROM reload_operations ORDER BY reload_operation_id")
                .fetch_all(&mut *transaction)
                .await?;
        validate_reload_states(&reload_states)?;

        let site = load_site_head(&mut *transaction).await?;
        let rows = sqlx::query_as::<_, CanonicalPublicationRow>(LOAD_CANONICAL_PUBLICATIONS)
            .fetch_all(&mut *transaction)
            .await?;
        if site.is_none() && !rows.is_empty() {
            return Err(StartupSnapshotLoadError::MissingSiteHead);
        }

        let mut published = Vec::new();
        let mut activating = Vec::new();
        let mut scheduled = Vec::new();
        for row in rows {
            let stored = decode_canonical_publication(row)?;
            match stored.status {
                CanonicalPublicationStatus::Published(publication) => {
                    let view = publication.into_view();
                    published.push(PublishedPostRevision::new(
                        view.stable_post_id,
                        view.current_published_digest
                            .expect("published publications have a current revision"),
                        view.published_at
                            .expect("published publications have a publication timestamp"),
                    ));
                }
                CanonicalPublicationStatus::Activating(publication) => {
                    activating.push(RecoverablePublicationActivation {
                        publication_id: stored.publication_id,
                        publication,
                        creation_key: stored.creation_key,
                        content_digest: stored.content_digest,
                        accepted_preview_digest: stored.accepted_preview_digest,
                        candidate_site_digest: stored
                            .activation_site_digest
                            .ok_or(StartupSnapshotLoadError::MissingActivationSiteDigest)?,
                    });
                }
                CanonicalPublicationStatus::Scheduled(publication) => {
                    scheduled.push(ScheduledPublication {
                        publication_id: stored.publication_id,
                        publication,
                        creation_key: stored.creation_key,
                        content_digest: stored.content_digest,
                        accepted_preview_digest: stored.accepted_preview_digest,
                    });
                }
                CanonicalPublicationStatus::Blocked(_)
                | CanonicalPublicationStatus::Superseded(_)
                | CanonicalPublicationStatus::Cancelled(_) => {}
            }
        }
        if activating.len() > 1 {
            return Err(StartupSnapshotLoadError::MultipleActivations);
        }
        let ledger = PublicLedgerProjection::try_from_exact_entries(published)
            .map_err(|_| StartupSnapshotLoadError::DuplicatePublishedPost)?;
        transaction.commit().await?;
        Ok(StartupSnapshotState {
            site,
            ledger,
            activating,
            scheduled,
        })
    }

    /// Installs a fully built startup snapshot through the sole writer task.
    pub(crate) async fn install_startup_snapshot(
        &self,
        command: InstallStartupSnapshot,
    ) -> Result<SiteHead, DatabaseMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.mutations
            .try_send(Mutation::InstallStartupSnapshot {
                command,
                respond_to,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => DatabaseAdmissionError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => DatabaseAdmissionError::WriterClosed,
            })?;
        response
            .await
            .map_err(|_| DatabaseMutationError::Command(DatabaseCommandError::OutcomeUnknown))?
            .map_err(DatabaseMutationError::Command)
    }

    /// Records one compiler-produced preview candidate through the sole writer task.
    pub(crate) async fn index_content_catalog(
        &self,
        command: IndexContentCatalog,
    ) -> Result<(), DatabaseMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.mutations
            .try_send(Mutation::IndexContentCatalog {
                command,
                respond_to,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => DatabaseAdmissionError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => DatabaseAdmissionError::WriterClosed,
            })?;
        response
            .await
            .map_err(|_| DatabaseMutationError::Command(DatabaseCommandError::OutcomeUnknown))?
            .map_err(DatabaseMutationError::Command)
    }

    /// Replays a previously accepted immediate-publication request before catalog resolution.
    pub(crate) async fn publish_now_replay(
        &self,
        command: LookupPublishNow,
    ) -> Result<Option<PublishNowState>, PublishNowLookupError> {
        let mut transaction = self.readers.begin().await?;
        let stored = sqlx::query_as::<_, CanonicalPublicationRow>(LOAD_CANONICAL_BY_CREATION_KEY)
            .bind(command.creation_key.0.as_bytes().as_slice())
            .fetch_optional(&mut *transaction)
            .await?
            .map(decode_canonical_publication)
            .transpose()
            .map_err(PublishNowLookupError::from_load)?;
        let Some(stored) = stored else {
            transaction.commit().await?;
            return Ok(None);
        };
        validate_publish_now_fingerprint(&stored, &command)?;
        let state = load_replayed_publish_now(&mut transaction, stored).await?;
        transaction.commit().await?;
        Ok(Some(state))
    }

    /// Replays a previously accepted scheduled approval before catalog resolution.
    pub(crate) async fn schedule_publication_replay(
        &self,
        command: LookupSchedulePublication,
    ) -> Result<Option<SchedulePublicationReplay>, SchedulePublicationLookupError> {
        let mut transaction = self.readers.begin().await?;
        let stored = sqlx::query_as::<_, CanonicalPublicationRow>(LOAD_CANONICAL_BY_CREATION_KEY)
            .bind(command.creation_key.0.as_bytes().as_slice())
            .fetch_optional(&mut *transaction)
            .await?
            .map(decode_canonical_publication)
            .transpose()
            .map_err(SchedulePublicationLookupError::from_load)?;
        let Some(stored) = stored else {
            transaction.commit().await?;
            return Ok(None);
        };
        validate_schedule_publication_fingerprint(&stored, &command)?;
        let scheduled = load_replayed_schedule_publication(&mut transaction, stored).await?;
        transaction.commit().await?;
        Ok(Some(scheduled))
    }

    /// Creates and claims one immediate canonical publication.
    pub(crate) async fn begin_publish_now(
        &self,
        command: BeginPublishNow,
    ) -> Result<PublishNowState, DatabaseMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.mutations
            .try_send(Mutation::BeginPublishNow {
                command,
                respond_to,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => DatabaseAdmissionError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => DatabaseAdmissionError::WriterClosed,
            })?;
        response
            .await
            .map_err(|_| DatabaseMutationError::Command(DatabaseCommandError::OutcomeUnknown))?
            .map_err(DatabaseMutationError::Command)
    }

    /// Persists one exact future publication approval.
    pub(crate) async fn schedule_publication(
        &self,
        command: SchedulePublication,
    ) -> Result<ScheduledPublication, DatabaseMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.mutations
            .try_send(Mutation::SchedulePublication {
                command,
                respond_to,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => DatabaseAdmissionError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => DatabaseAdmissionError::WriterClosed,
            })?;
        response
            .await
            .map_err(|_| DatabaseMutationError::Command(DatabaseCommandError::OutcomeUnknown))?
            .map_err(DatabaseMutationError::Command)
    }

    /// Returns the earliest exact revision waiting for scheduled activation.
    pub(crate) async fn next_scheduled_publication(
        &self,
    ) -> Result<Option<ScheduledPublication>, StartupSnapshotLoadError> {
        let rows = sqlx::query_as::<_, CanonicalPublicationRow>(LOAD_CANONICAL_PUBLICATIONS)
            .fetch_all(&self.readers)
            .await?;
        let mut scheduled = Vec::new();
        for row in rows {
            let stored = decode_canonical_publication(row)?;
            if let CanonicalPublicationStatus::Scheduled(publication) = stored.status {
                scheduled.push(ScheduledPublication {
                    publication_id: stored.publication_id,
                    publication,
                    creation_key: stored.creation_key,
                    content_digest: stored.content_digest,
                    accepted_preview_digest: stored.accepted_preview_digest,
                });
            }
        }
        scheduled.sort_by(|left, right| {
            left.publication
                .view()
                .scheduled_at
                .cmp(&right.publication.view().scheduled_at)
                .then_with(|| left.publication_id.cmp(&right.publication_id))
        });
        Ok(scheduled.into_iter().next())
    }

    /// Claims one due scheduled approval for canonical activation.
    pub(crate) async fn begin_scheduled_activation(
        &self,
        command: BeginScheduledActivation,
    ) -> Result<BegunPublication, DatabaseMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.mutations
            .try_send(Mutation::BeginScheduledActivation {
                command,
                respond_to,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => DatabaseAdmissionError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => DatabaseAdmissionError::WriterClosed,
            })?;
        response
            .await
            .map_err(|_| DatabaseMutationError::Command(DatabaseCommandError::OutcomeUnknown))?
            .map_err(DatabaseMutationError::Command)
    }

    /// Commits a snapshot-visible activation and releases its waiting jobs.
    pub(crate) async fn finish_publication(
        &self,
        command: FinishPublication,
    ) -> Result<FinishedPublication, DatabaseMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.mutations
            .try_send(Mutation::FinishPublication {
                command,
                respond_to,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => DatabaseAdmissionError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => DatabaseAdmissionError::WriterClosed,
            })?;
        response
            .await
            .map_err(|_| DatabaseMutationError::Command(DatabaseCommandError::OutcomeUnknown))?
            .map_err(DatabaseMutationError::Command)
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the WP4.3 admin read API will load durable target jobs"
        )
    )]
    pub(crate) async fn target_job(
        &self,
        publication_job_id: Uuid,
    ) -> Result<Option<StoredTargetJob>, TargetJobLoadError> {
        load_by_id(&self.readers, publication_job_id).await
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the WP4.1 admin mutation will submit this WP3.2 command"
        )
    )]
    pub(crate) async fn create_target_job(
        &self,
        command: CreateTargetJob,
    ) -> Result<StoredTargetJob, DatabaseMutationError> {
        let response = self.admit_create_target_job(command)?;
        response
            .await
            .map_err(|_| DatabaseMutationError::Command(DatabaseCommandError::OutcomeUnknown))?
            .map_err(DatabaseMutationError::Command)
    }

    pub(crate) fn admit_create_target_job(
        &self,
        command: CreateTargetJob,
    ) -> Result<oneshot::Receiver<CreateTargetJobResult>, DatabaseAdmissionError> {
        let (respond_to, response) = oneshot::channel();
        self.mutations
            .try_send(Mutation::CreateTargetJob {
                command,
                respond_to,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => DatabaseAdmissionError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => DatabaseAdmissionError::WriterClosed,
            })?;
        Ok(response)
    }
}

/// The durable inputs needed to build one canonical startup snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StartupSnapshotState {
    pub site: Option<SiteHead>,
    pub ledger: PublicLedgerProjection,
    pub activating: Vec<RecoverablePublicationActivation>,
    pub scheduled: Vec<ScheduledPublication>,
}

/// One exact activation that startup must reconcile before listener binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoverablePublicationActivation {
    pub publication_id: Uuid,
    pub publication: CanonicalPublication<canonical::Activating>,
    pub creation_key: Option<CommandIdempotencyKey>,
    pub content_digest: ContentTreeDigest,
    pub accepted_preview_digest: PreviewDigest,
    pub candidate_site_digest: SiteSnapshotDigest,
}

/// The current durable site head.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SiteHead {
    pub digest: SiteSnapshotDigest,
    pub version: u64,
}

/// One current content revision observed during startup compilation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ObservedPostRevision {
    pub stable_post_id: PostId,
    pub revision_digest: PostRevisionDigest,
    pub publication_status: DraftStatus,
    pub slug: PostSlug,
}

/// One guarded request to make a built snapshot the durable startup head.
pub(crate) struct InstallStartupSnapshot {
    pub expected: Option<SiteHead>,
    pub candidate_digest: SiteSnapshotDigest,
    pub activated_at: OffsetDateTime,
    pub source_commit: Option<SourceCommit>,
    pub posts: Vec<ObservedPostRevision>,
}

pub(crate) type InstallStartupSnapshotResult = Result<SiteHead, DatabaseCommandError>;

/// One validated preview candidate whose revisions must be retained for approval.
pub(crate) struct IndexContentCatalog {
    pub observed_at: OffsetDateTime,
    pub source_commit: Option<SourceCommit>,
    pub posts: Vec<ObservedPostRevision>,
}

pub(crate) type IndexContentCatalogResult = Result<(), DatabaseCommandError>;

/// One exact request to create and immediately claim a publication.
pub(crate) struct BeginPublishNow {
    pub creation_key: CommandIdempotencyKey,
    pub publication_id: Uuid,
    pub stable_post_id: PostId,
    pub pinned_post_digest: PostRevisionDigest,
    pub expected_revision: Option<PostRevisionDigest>,
    pub expected_site: SiteHead,
    pub source_commit: Option<SourceCommit>,
    pub content_digest: ContentTreeDigest,
    pub accepted_preview_digest: PreviewDigest,
    pub now: OffsetDateTime,
    pub candidate_site_digest: SiteSnapshotDigest,
}

/// One exact future approval to retain without changing the public snapshot.
pub(crate) struct SchedulePublication {
    pub creation_key: CommandIdempotencyKey,
    pub publication_id: Uuid,
    pub stable_post_id: PostId,
    pub pinned_post_digest: PostRevisionDigest,
    pub expected_revision: Option<PostRevisionDigest>,
    pub expected_site: SiteHead,
    pub source_commit: Option<SourceCommit>,
    pub content_digest: ContentTreeDigest,
    pub accepted_preview_digest: PreviewDigest,
    pub scheduled_at: OffsetDateTime,
}

/// One durable exact revision waiting for its activation time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScheduledPublication {
    pub publication_id: Uuid,
    pub publication: CanonicalPublication<canonical::Scheduled>,
    pub creation_key: Option<CommandIdempotencyKey>,
    pub content_digest: ContentTreeDigest,
    pub accepted_preview_digest: PreviewDigest,
}

/// A prior scheduled-approval command resolved before current-catalog selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SchedulePublicationReplay {
    Scheduled(ScheduledPublication),
    Published(CompletedPublication),
}

/// One due scheduled approval to claim against the current public site head.
pub(crate) struct BeginScheduledActivation {
    pub publication_id: Uuid,
    pub expected_publication_version: u64,
    pub expected_site: SiteHead,
    pub now: OffsetDateTime,
    pub candidate_site_digest: SiteSnapshotDigest,
}

/// The wire-level identity of a possible immediate-publication retry.
pub(crate) struct LookupPublishNow {
    pub creation_key: CommandIdempotencyKey,
    pub stable_post_id: PostId,
    pub expected_revision: Option<PostRevisionDigest>,
    pub accepted_preview_digest: PreviewDigest,
}

/// The wire-level identity of a possible scheduled-approval retry.
pub(crate) struct LookupSchedulePublication {
    pub creation_key: CommandIdempotencyKey,
    pub stable_post_id: PostId,
    pub expected_revision: Option<PostRevisionDigest>,
    pub accepted_preview_digest: PreviewDigest,
    pub scheduled_at: OffsetDateTime,
}

/// The durable state returned by an immediate-publication request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PublishNowState {
    Activating(BegunPublication),
    Published(CompletedPublication),
}

/// Historical result of one publication command, independent of its current canonical state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompletedPublication {
    pub publication_id: Uuid,
    pub stable_post_id: PostId,
    pub revision: PostRevisionDigest,
    pub accepted_preview_digest: PreviewDigest,
    pub published_at: OffsetDateTime,
    pub site: SiteHead,
}

/// A publication claimed for one exact candidate snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BegunPublication {
    pub publication_id: Uuid,
    pub publication: CanonicalPublication<canonical::Activating>,
    pub site: SiteHead,
    pub content_digest: ContentTreeDigest,
    pub accepted_preview_digest: PreviewDigest,
    pub candidate_site_digest: SiteSnapshotDigest,
}

/// Commits one candidate after the public snapshot has become visible.
pub(crate) struct FinishPublication {
    pub publication_id: Uuid,
    pub expected_publication_version: u64,
    pub expected_site: SiteHead,
    pub candidate_site_digest: SiteSnapshotDigest,
    pub slug: PostSlug,
}

/// A fully committed canonical publication and its activated site head.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FinishedPublication {
    pub publication_id: Uuid,
    pub publication: CanonicalPublication<canonical::Published>,
    pub site: SiteHead,
    pub accepted_preview_digest: PreviewDigest,
}

pub(crate) type BeginPublishNowResult = Result<PublishNowState, DatabaseCommandError>;
pub(crate) type SchedulePublicationResult = Result<ScheduledPublication, DatabaseCommandError>;
pub(crate) type BeginScheduledActivationResult = Result<BegunPublication, DatabaseCommandError>;
pub(crate) type FinishPublicationResult = Result<FinishedPublication, DatabaseCommandError>;

const LOAD_SITE_HEAD: &str = "SELECT \
    state.current_site_digest AS current_site_digest, \
    state.version AS state_version, \
    revision.site_revision_digest AS linked_site_digest, \
    revision.version AS revision_version, \
    revision.activated_at_ns AS activated_at_ns, \
    revision.source_commit AS source_commit, \
    (SELECT count(*) FROM site_revisions) AS site_revision_count \
    FROM (SELECT 1 AS singleton) AS seed \
    LEFT JOIN site_state AS state ON state.singleton = seed.singleton \
    LEFT JOIN site_revisions AS revision \
        ON revision.site_revision_digest = state.current_site_digest";

const LOAD_CANONICAL_PUBLICATIONS: &str = "SELECT \
    canonical.publication_id AS publication_id, \
    canonical.stable_post_id AS stable_post_id, \
    canonical.pinned_post_digest AS pinned_post_digest, \
    canonical.state AS state, \
    canonical.version AS version, \
    canonical.scheduled_at_ns AS scheduled_at_ns, \
    canonical.activation_at_ns AS activation_at_ns, \
    canonical.published_at_ns AS published_at_ns, \
    canonical.current_published_digest AS current_published_digest, \
    canonical.source_commit AS source_commit, \
    canonical.block_reason AS block_reason, \
    canonical.creation_key AS creation_key, \
    canonical.command_kind AS command_kind, \
    canonical.activation_site_digest AS activation_site_digest, \
    canonical.requested_revision_digest AS requested_revision_digest, \
    canonical.content_tree_digest AS content_tree_digest, \
    canonical.accepted_preview_digest AS accepted_preview_digest, \
    pinned.publication_status AS pinned_publication_status, \
    pinned.slug AS pinned_slug, \
    current.publication_status AS current_publication_status \
    FROM canonical_publications AS canonical \
    LEFT JOIN post_revisions AS pinned \
      ON pinned.stable_post_id = canonical.stable_post_id \
     AND pinned.revision_digest = canonical.pinned_post_digest \
    LEFT JOIN post_revisions AS current \
      ON current.stable_post_id = canonical.stable_post_id \
     AND current.revision_digest = canonical.current_published_digest \
    ORDER BY canonical.stable_post_id, canonical.publication_id";

const LOAD_CANONICAL_BY_CREATION_KEY: &str = "SELECT \
    canonical.publication_id AS publication_id, \
    canonical.stable_post_id AS stable_post_id, \
    canonical.pinned_post_digest AS pinned_post_digest, \
    canonical.state AS state, \
    canonical.version AS version, \
    canonical.scheduled_at_ns AS scheduled_at_ns, \
    canonical.activation_at_ns AS activation_at_ns, \
    canonical.published_at_ns AS published_at_ns, \
    canonical.current_published_digest AS current_published_digest, \
    canonical.source_commit AS source_commit, \
    canonical.block_reason AS block_reason, \
    canonical.creation_key AS creation_key, \
    canonical.command_kind AS command_kind, \
    canonical.activation_site_digest AS activation_site_digest, \
    canonical.requested_revision_digest AS requested_revision_digest, \
    canonical.content_tree_digest AS content_tree_digest, \
    canonical.accepted_preview_digest AS accepted_preview_digest, \
    pinned.publication_status AS pinned_publication_status, \
    pinned.slug AS pinned_slug, \
    current.publication_status AS current_publication_status \
    FROM canonical_publications AS canonical \
    LEFT JOIN post_revisions AS pinned \
      ON pinned.stable_post_id = canonical.stable_post_id \
     AND pinned.revision_digest = canonical.pinned_post_digest \
    LEFT JOIN post_revisions AS current \
      ON current.stable_post_id = canonical.stable_post_id \
     AND current.revision_digest = canonical.current_published_digest \
    WHERE canonical.creation_key = ?";

const LOAD_CANONICAL_BY_PUBLICATION_ID: &str = "SELECT \
    canonical.publication_id AS publication_id, \
    canonical.stable_post_id AS stable_post_id, \
    canonical.pinned_post_digest AS pinned_post_digest, \
    canonical.state AS state, \
    canonical.version AS version, \
    canonical.scheduled_at_ns AS scheduled_at_ns, \
    canonical.activation_at_ns AS activation_at_ns, \
    canonical.published_at_ns AS published_at_ns, \
    canonical.current_published_digest AS current_published_digest, \
    canonical.source_commit AS source_commit, \
    canonical.block_reason AS block_reason, \
    canonical.creation_key AS creation_key, \
    canonical.command_kind AS command_kind, \
    canonical.activation_site_digest AS activation_site_digest, \
    canonical.requested_revision_digest AS requested_revision_digest, \
    canonical.content_tree_digest AS content_tree_digest, \
    canonical.accepted_preview_digest AS accepted_preview_digest, \
    pinned.publication_status AS pinned_publication_status, \
    pinned.slug AS pinned_slug, \
    current.publication_status AS current_publication_status \
    FROM canonical_publications AS canonical \
    LEFT JOIN post_revisions AS pinned \
      ON pinned.stable_post_id = canonical.stable_post_id \
     AND pinned.revision_digest = canonical.pinned_post_digest \
    LEFT JOIN post_revisions AS current \
      ON current.stable_post_id = canonical.stable_post_id \
     AND current.revision_digest = canonical.current_published_digest \
    WHERE canonical.publication_id = ?";

#[derive(FromRow)]
struct SiteHeadRow {
    current_site_digest: Option<Vec<u8>>,
    state_version: Option<i64>,
    linked_site_digest: Option<Vec<u8>>,
    revision_version: Option<i64>,
    activated_at_ns: Option<i64>,
    source_commit: Option<Vec<u8>>,
    site_revision_count: i64,
}

#[derive(FromRow)]
struct CanonicalPublicationRow {
    publication_id: Vec<u8>,
    stable_post_id: Vec<u8>,
    pinned_post_digest: Vec<u8>,
    state: String,
    version: i64,
    scheduled_at_ns: i64,
    activation_at_ns: Option<i64>,
    published_at_ns: Option<i64>,
    current_published_digest: Option<Vec<u8>>,
    source_commit: Option<Vec<u8>>,
    block_reason: Option<String>,
    creation_key: Option<Vec<u8>>,
    command_kind: String,
    activation_site_digest: Option<Vec<u8>>,
    requested_revision_digest: Option<Vec<u8>>,
    content_tree_digest: Vec<u8>,
    accepted_preview_digest: Vec<u8>,
    pinned_publication_status: Option<String>,
    pinned_slug: Option<String>,
    current_publication_status: Option<String>,
}

async fn load_site_head<'executor>(
    executor: impl Executor<'executor, Database = Sqlite>,
) -> Result<Option<SiteHead>, StartupSnapshotLoadError> {
    SiteHeadRow::try_into_head(
        sqlx::query_as::<_, SiteHeadRow>(LOAD_SITE_HEAD)
            .fetch_one(executor)
            .await?,
    )
}

impl SiteHeadRow {
    fn try_into_head(self) -> Result<Option<SiteHead>, StartupSnapshotLoadError> {
        let Some(current_digest) = self.current_site_digest else {
            if self.site_revision_count == 0 {
                return Ok(None);
            }
            return Err(StartupSnapshotLoadError::MissingSiteHead);
        };
        let digest = site_digest(current_digest)?;
        let linked = self
            .linked_site_digest
            .ok_or(StartupSnapshotLoadError::MissingSiteRevision)
            .and_then(site_digest)?;
        if linked != digest {
            return Err(StartupSnapshotLoadError::MismatchedSiteRevision);
        }
        let state_version = positive_version(self.state_version)?;
        let revision_version = positive_version(self.revision_version)?;
        if revision_version > state_version {
            return Err(StartupSnapshotLoadError::MismatchedSiteVersion);
        }
        let activated_at_ns = self
            .activated_at_ns
            .ok_or(StartupSnapshotLoadError::InvalidSiteTimestamp)?;
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(activated_at_ns))
            .map_err(|_| StartupSnapshotLoadError::InvalidSiteTimestamp)?;
        if let Some(commit) = self.source_commit {
            decode_source_commit(commit).ok_or(StartupSnapshotLoadError::InvalidSourceCommit)?;
        }
        Ok(Some(SiteHead {
            digest,
            version: state_version,
        }))
    }
}

fn validate_reload_states(states: &[String]) -> Result<(), StartupSnapshotLoadError> {
    for state in states {
        match state.as_str() {
            "applying" => return Err(StartupSnapshotLoadError::UnreconciledReload),
            "applied" | "failed" => {}
            _ => return Err(StartupSnapshotLoadError::InvalidReloadState),
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationCommandKind {
    Immediate,
    Scheduled,
}

impl PublicationCommandKind {
    fn parse(value: &str) -> Result<Self, StartupSnapshotLoadError> {
        match value {
            "immediate" => Ok(Self::Immediate),
            "scheduled" => Ok(Self::Scheduled),
            _ => Err(StartupSnapshotLoadError::InvalidCommandKind),
        }
    }
}

struct StoredCanonicalPublication {
    publication_id: Uuid,
    creation_key: Option<CommandIdempotencyKey>,
    command_kind: PublicationCommandKind,
    activation_site_digest: Option<SiteSnapshotDigest>,
    requested_revision_digest: Option<PostRevisionDigest>,
    content_digest: ContentTreeDigest,
    accepted_preview_digest: PreviewDigest,
    pinned_slug: PostSlug,
    status: CanonicalPublicationStatus,
}

fn decode_canonical_publication(
    row: CanonicalPublicationRow,
) -> Result<StoredCanonicalPublication, StartupSnapshotLoadError> {
    let publication_id = Uuid::from_slice(&row.publication_id)
        .map_err(|_| StartupSnapshotLoadError::InvalidPublicationId)?;
    let command_kind = PublicationCommandKind::parse(&row.command_kind)?;
    let activation_site_digest = row.activation_site_digest.map(site_digest).transpose()?;
    let (stable_post_id, pinned_post_digest, pinned_slug) = decode_pinned_revision(
        row.stable_post_id,
        row.pinned_post_digest,
        row.pinned_publication_status.as_deref(),
        row.pinned_slug,
    )?;
    let (creation_key, requested_revision_digest) = decode_publish_now_identity(
        row.creation_key,
        row.requested_revision_digest,
        &pinned_post_digest,
    )?;
    let content_digest = content_tree_digest(row.content_tree_digest)?;
    let accepted_preview_digest = preview_digest(row.accepted_preview_digest)?;

    let current_published_digest = row.current_published_digest.map(post_digest).transpose()?;
    if current_published_digest.is_some() {
        require_publishable(row.current_publication_status.as_deref(), false)?;
    } else if row.current_publication_status.is_some() {
        return Err(StartupSnapshotLoadError::MismatchedCurrentRevision);
    }

    let source_commit = row
        .source_commit
        .map(|value| {
            decode_source_commit(value).ok_or(StartupSnapshotLoadError::InvalidSourceCommit)
        })
        .transpose()?;
    let block_reason = row
        .block_reason
        .map(|reason| match reason.as_str() {
            "revision_unavailable" => Ok(ActivationBlockReason::RevisionUnavailable),
            _ => Err(StartupSnapshotLoadError::InvalidBlockReason),
        })
        .transpose()?;
    let view = CanonicalPublicationView {
        state: canonical_state(&row.state)?,
        stable_post_id,
        pinned_post_digest,
        source_commit,
        scheduled_at: publication_timestamp(row.scheduled_at_ns)?,
        activation_started_at: row
            .activation_at_ns
            .map(publication_timestamp)
            .transpose()?,
        published_at: row.published_at_ns.map(publication_timestamp).transpose()?,
        current_published_digest,
        block_reason,
        version: u64::try_from(row.version)
            .map_err(|_| StartupSnapshotLoadError::InvalidPublicationVersion)?,
    };
    let status = CanonicalPublicationStatus::try_from(view)
        .map_err(StartupSnapshotLoadError::InvalidCanonicalPublication)?;
    Ok(StoredCanonicalPublication {
        publication_id,
        creation_key,
        command_kind,
        activation_site_digest,
        requested_revision_digest,
        content_digest,
        accepted_preview_digest,
        pinned_slug,
        status,
    })
}

fn decode_publish_now_identity(
    creation_key: Option<Vec<u8>>,
    requested_revision_digest: Option<Vec<u8>>,
    pinned_post_digest: &PostRevisionDigest,
) -> Result<(Option<CommandIdempotencyKey>, Option<PostRevisionDigest>), StartupSnapshotLoadError> {
    let creation_key = creation_key
        .map(|value| {
            Uuid::from_slice(&value)
                .map(CommandIdempotencyKey::new)
                .map_err(|_| StartupSnapshotLoadError::InvalidCreationKey)
        })
        .transpose()?;
    let requested_revision_digest = requested_revision_digest.map(post_digest).transpose()?;
    if requested_revision_digest.is_some() && creation_key.is_none() {
        return Err(StartupSnapshotLoadError::OrphanedRequestedRevision);
    }
    if requested_revision_digest
        .as_ref()
        .is_some_and(|requested| requested != pinned_post_digest)
    {
        return Err(StartupSnapshotLoadError::MismatchedRequestedRevision);
    }
    Ok((creation_key, requested_revision_digest))
}

fn decode_pinned_revision(
    stable_post_id: Vec<u8>,
    pinned_post_digest: Vec<u8>,
    publication_status: Option<&str>,
    slug: Option<String>,
) -> Result<(PostId, PostRevisionDigest, PostSlug), StartupSnapshotLoadError> {
    require_publishable(publication_status, true)?;
    let slug = slug.ok_or(StartupSnapshotLoadError::MissingPinnedRevision)?;
    Ok((
        decode_post_id(stable_post_id)?,
        post_digest(pinned_post_digest)?,
        PostSlug::parse(&slug).map_err(|_| StartupSnapshotLoadError::InvalidPinnedSlug)?,
    ))
}

fn canonical_view(status: &CanonicalPublicationStatus) -> &CanonicalPublicationView {
    match status {
        CanonicalPublicationStatus::Scheduled(publication) => publication.view(),
        CanonicalPublicationStatus::Activating(publication) => publication.view(),
        CanonicalPublicationStatus::Blocked(publication) => publication.view(),
        CanonicalPublicationStatus::Published(publication) => publication.view(),
        CanonicalPublicationStatus::Superseded(publication) => publication.view(),
        CanonicalPublicationStatus::Cancelled(publication) => publication.view(),
    }
}

fn publish_now_fingerprint_matches(
    stored: &StoredCanonicalPublication,
    post_id: &PostId,
    requested_revision: Option<&PostRevisionDigest>,
    accepted_preview_digest: &PreviewDigest,
) -> bool {
    let view = canonical_view(&stored.status);
    &view.stable_post_id == post_id
        && stored.requested_revision_digest.as_ref() == requested_revision
        && &stored.accepted_preview_digest == accepted_preview_digest
}

fn canonical_state(value: &str) -> Result<CanonicalState, StartupSnapshotLoadError> {
    match value {
        "scheduled" => Ok(CanonicalState::Scheduled),
        "activating" => Ok(CanonicalState::Activating),
        "blocked" => Ok(CanonicalState::Blocked),
        "published" => Ok(CanonicalState::Published),
        "superseded" => Ok(CanonicalState::Superseded),
        "cancelled" => Ok(CanonicalState::Cancelled),
        _ => Err(StartupSnapshotLoadError::InvalidCanonicalState),
    }
}

fn require_publishable(value: Option<&str>, pinned: bool) -> Result<(), StartupSnapshotLoadError> {
    match value {
        Some("publishable") => Ok(()),
        Some("draft") if pinned => Err(StartupSnapshotLoadError::DraftPinnedRevision),
        Some("draft") => Err(StartupSnapshotLoadError::DraftCurrentRevision),
        Some(_) => Err(StartupSnapshotLoadError::InvalidPublicationStatus),
        None if pinned => Err(StartupSnapshotLoadError::MissingPinnedRevision),
        None => Err(StartupSnapshotLoadError::MissingCurrentRevision),
    }
}

fn decode_post_id(value: Vec<u8>) -> Result<PostId, StartupSnapshotLoadError> {
    let uuid = Uuid::from_slice(&value).map_err(|_| StartupSnapshotLoadError::InvalidPostId)?;
    PostId::parse(&uuid.hyphenated().to_string())
        .map_err(|_| StartupSnapshotLoadError::InvalidPostId)
}

fn post_digest(value: Vec<u8>) -> Result<PostRevisionDigest, StartupSnapshotLoadError> {
    value
        .try_into()
        .map(PostRevisionDigest::from_bytes)
        .map_err(|_| StartupSnapshotLoadError::InvalidPostDigest)
}

fn content_tree_digest(value: Vec<u8>) -> Result<ContentTreeDigest, StartupSnapshotLoadError> {
    value
        .try_into()
        .map(ContentTreeDigest::from_bytes)
        .map_err(|_| StartupSnapshotLoadError::InvalidContentTreeDigest)
}

fn preview_digest(value: Vec<u8>) -> Result<PreviewDigest, StartupSnapshotLoadError> {
    value
        .try_into()
        .map(PreviewDigest::from_bytes)
        .map_err(|_| StartupSnapshotLoadError::InvalidAcceptedPreviewDigest)
}

fn site_digest(value: Vec<u8>) -> Result<SiteSnapshotDigest, StartupSnapshotLoadError> {
    value
        .try_into()
        .map(SiteSnapshotDigest::from_bytes)
        .map_err(|_| StartupSnapshotLoadError::InvalidSiteDigest)
}

fn positive_version(value: Option<i64>) -> Result<u64, StartupSnapshotLoadError> {
    value
        .and_then(|value| u64::try_from(value).ok())
        .filter(|version| *version > 0)
        .ok_or(StartupSnapshotLoadError::InvalidSiteVersion)
}

fn publication_timestamp(value: i64) -> Result<OffsetDateTime, StartupSnapshotLoadError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(value))
        .map(|timestamp| timestamp.to_offset(UtcOffset::UTC))
        .map_err(|_| StartupSnapshotLoadError::InvalidPublicationTimestamp)
}

fn decode_source_commit(value: Vec<u8>) -> Option<SourceCommit> {
    let prefix = match value.len() {
        20 => "git-sha1:",
        32 => "git-sha256:",
        _ => return None,
    };
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(prefix.len() + value.len() * 2);
    encoded.push_str(prefix);
    for byte in value {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    SourceCommit::parse(&encoded).ok()
}

#[derive(Debug, Error)]
pub(crate) enum StartupSnapshotLoadError {
    #[error("could not read the startup publication ledger")]
    Query(#[from] sqlx::Error),
    #[error("the current site head is missing")]
    MissingSiteHead,
    #[error("the current site revision is missing")]
    MissingSiteRevision,
    #[error("the current site revision digest is invalid")]
    InvalidSiteDigest,
    #[error("the current site head and revision digests differ")]
    MismatchedSiteRevision,
    #[error("the current site version is invalid")]
    InvalidSiteVersion,
    #[error("the current site revision version is newer than the site head")]
    MismatchedSiteVersion,
    #[error("the current site activation timestamp is invalid")]
    InvalidSiteTimestamp,
    #[error("a stored source commit is invalid")]
    InvalidSourceCommit,
    #[error("a reload operation has an invalid state")]
    InvalidReloadState,
    #[error("an applying reload must be reconciled before snapshot construction")]
    UnreconciledReload,
    #[error("more than one publication is activating")]
    MultipleActivations,
    #[error("an activating publication has no candidate site digest")]
    MissingActivationSiteDigest,
    #[error("a stored publication ID is invalid")]
    InvalidPublicationId,
    #[error("a stored publication creation key is invalid")]
    InvalidCreationKey,
    #[error("a stored publication command kind is invalid")]
    InvalidCommandKind,
    #[error("a stored post ID is invalid")]
    InvalidPostId,
    #[error("a stored post digest is invalid")]
    InvalidPostDigest,
    #[error("a stored content-tree digest is invalid")]
    InvalidContentTreeDigest,
    #[error("a stored accepted preview digest is invalid")]
    InvalidAcceptedPreviewDigest,
    #[error("a requested revision is stored without an immediate-publication key")]
    OrphanedRequestedRevision,
    #[error("a requested revision differs from the canonical pinned revision")]
    MismatchedRequestedRevision,
    #[error("a pinned post slug is invalid")]
    InvalidPinnedSlug,
    #[error("a canonical publication has an invalid state")]
    InvalidCanonicalState,
    #[error("a canonical publication has an invalid block reason")]
    InvalidBlockReason,
    #[error("a canonical publication has an invalid version")]
    InvalidPublicationVersion,
    #[error("a canonical publication has an invalid timestamp")]
    InvalidPublicationTimestamp,
    #[error("a referenced post revision is missing")]
    MissingPinnedRevision,
    #[error("a current published post revision is missing")]
    MissingCurrentRevision,
    #[error("a current post-revision join is inconsistent")]
    MismatchedCurrentRevision,
    #[error("a post revision has an invalid publication status")]
    InvalidPublicationStatus,
    #[error("a canonical publication pins a draft revision")]
    DraftPinnedRevision,
    #[error("a canonical publication exposes a draft revision")]
    DraftCurrentRevision,
    #[error("a canonical publication violates its domain invariants")]
    InvalidCanonicalPublication(#[source] RehydrationError),
    #[error("the public ledger contains duplicate published posts")]
    DuplicatePublishedPost,
}

#[derive(Debug, Error)]
pub(crate) enum PublishNowLookupError {
    #[error("could not read a prior immediate-publication request")]
    Query(#[from] sqlx::Error),
    #[error("the idempotency key is already bound to a different command")]
    IdempotencyConflict,
    #[error("the stored immediate-publication request is invalid")]
    InvalidStoredState,
}

impl PublishNowLookupError {
    fn from_load(error: StartupSnapshotLoadError) -> Self {
        match error {
            StartupSnapshotLoadError::Query(source) => Self::Query(source),
            _ => Self::InvalidStoredState,
        }
    }

    fn from_publication(error: PublicationMutationError) -> Self {
        match error {
            PublicationMutationError::Operation(source) => Self::Query(source),
            PublicationMutationError::Command(_) | PublicationMutationError::CorruptStoredState => {
                Self::InvalidStoredState
            }
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum SchedulePublicationLookupError {
    #[error("could not read a prior scheduled-publication request")]
    Query(#[from] sqlx::Error),
    #[error("the idempotency key is already bound to a different command")]
    IdempotencyConflict,
    #[error("the scheduled publication is currently activating")]
    ActivationInProgress,
    #[error("the stored scheduled-publication request is invalid")]
    InvalidStoredState,
}

impl SchedulePublicationLookupError {
    fn from_load(error: StartupSnapshotLoadError) -> Self {
        match error {
            StartupSnapshotLoadError::Query(source) => Self::Query(source),
            _ => Self::InvalidStoredState,
        }
    }

    fn from_publication(error: PublicationMutationError) -> Self {
        match error {
            PublicationMutationError::Operation(source) => Self::Query(source),
            PublicationMutationError::Command(_) | PublicationMutationError::CorruptStoredState => {
                Self::InvalidStoredState
            }
        }
    }
}

/// Distinguishes a retryable database mutation from domain and resource IDs.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CommandIdempotencyKey(Uuid);

impl CommandIdempotencyKey {
    pub(crate) const fn new(value: Uuid) -> Self {
        Self(value)
    }
}

/// One complete request to create a durable target job.
pub(crate) struct CreateTargetJob {
    pub idempotency_key: CommandIdempotencyKey,
    pub publication_job_id: Uuid,
    pub publication_id: Uuid,
    pub job: TargetJob<target::WaitingForCanonical>,
}

pub(crate) type CreateTargetJobResult = Result<StoredTargetJob, DatabaseCommandError>;

const LOAD_BY_ID: &str = "SELECT \
    job.publication_job_id AS publication_job_id, \
    job.publication_id AS publication_id, \
    job.state AS state, \
    job.target AS target, \
    canonical.stable_post_id AS stable_post_id, \
    canonical.pinned_post_digest AS pinned_post_digest, \
    job.scheduled_at_ns AS scheduled_at_ns, \
    job.payload_version AS payload_version, \
    job.payload_body AS payload_body, \
    job.payload_digest AS payload_digest, \
    job.version AS version \
    FROM publication_jobs AS job \
    JOIN canonical_publications AS canonical \
        ON canonical.publication_id = job.publication_id \
    WHERE job.publication_job_id = ?";

/// A target job that has passed both storage and domain validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredTargetJob {
    pub(crate) publication_job_id: Uuid,
    pub(crate) publication_id: Uuid,
    pub(crate) status: TargetJobStatus,
}

#[derive(FromRow)]
struct TargetJobRow {
    publication_job_id: Vec<u8>,
    publication_id: Vec<u8>,
    state: String,
    target: String,
    stable_post_id: Vec<u8>,
    pinned_post_digest: Vec<u8>,
    scheduled_at_ns: i64,
    payload_version: i64,
    payload_body: String,
    payload_digest: Vec<u8>,
    version: i64,
}

pub(super) async fn load_by_id<'executor>(
    executor: impl Executor<'executor, Database = Sqlite>,
    publication_job_id: Uuid,
) -> Result<Option<StoredTargetJob>, TargetJobLoadError> {
    sqlx::query_as::<_, TargetJobRow>(LOAD_BY_ID)
        .bind(publication_job_id.as_bytes().as_slice())
        .fetch_optional(executor)
        .await?
        .map(StoredTargetJob::try_from)
        .transpose()
}

pub(crate) async fn install_startup(
    transaction: &mut Transaction<'_, Sqlite>,
    mut command: InstallStartupSnapshot,
) -> Result<SiteHead, StartupSnapshotMutationError> {
    let activated_at_ns = validate_startup_command(&mut command)?;
    let actual = load_site_head(&mut **transaction)
        .await
        .map_err(StartupSnapshotMutationError::from_load)?;
    let unchanged = actual
        .as_ref()
        .filter(|head| head.digest == command.candidate_digest)
        .cloned();
    if unchanged.is_none() && actual != command.expected {
        return Err(StartupSnapshotMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }

    record_observed_posts(transaction, &command, activated_at_ns).await?;
    if let Some(head) = unchanged {
        return Ok(head);
    }

    let version = actual
        .as_ref()
        .map_or(Some(1), |head| head.version.checked_add(1))
        .ok_or(StartupSnapshotMutationError::Command(
            DatabaseCommandError::InvalidValue,
        ))?;
    retain_candidate_site_revision(
        transaction,
        &command,
        actual.as_ref(),
        version,
        activated_at_ns,
    )
    .await?;
    advance_site_head(
        transaction,
        actual.as_ref(),
        &command.candidate_digest,
        version,
    )
    .await?;

    Ok(SiteHead {
        digest: command.candidate_digest,
        version,
    })
}

/// Persists revision metadata for a validated private-preview candidate.
///
/// This operation deliberately does not alter the canonical publication ledger,
/// published routes, retained public snapshots, or the durable site head.
pub(crate) async fn index_content_catalog(
    transaction: &mut Transaction<'_, Sqlite>,
    command: IndexContentCatalog,
) -> Result<(), StartupSnapshotMutationError> {
    let mut snapshot = InstallStartupSnapshot {
        expected: None,
        candidate_digest: SiteSnapshotDigest::from_bytes([0; 32]),
        activated_at: command.observed_at,
        source_commit: command.source_commit,
        posts: command.posts,
    };
    let observed_at_ns = validate_startup_command(&mut snapshot)?;
    record_observed_posts(transaction, &snapshot, observed_at_ns).await
}

fn validate_startup_command(
    command: &mut InstallStartupSnapshot,
) -> Result<i64, StartupSnapshotMutationError> {
    if command.posts.len() > MAX_STARTUP_POST_REVISIONS {
        return Err(StartupSnapshotMutationError::Command(
            DatabaseCommandError::InvalidValue,
        ));
    }
    command.posts.sort_by(|left, right| {
        left.stable_post_id
            .cmp(&right.stable_post_id)
            .then_with(|| left.revision_digest.cmp(&right.revision_digest))
    });
    if command
        .posts
        .windows(2)
        .any(|pair| pair[0].stable_post_id == pair[1].stable_post_id)
    {
        return Err(StartupSnapshotMutationError::Command(
            DatabaseCommandError::InvalidValue,
        ));
    }
    i64::try_from(command.activated_at.unix_timestamp_nanos())
        .map_err(|_| StartupSnapshotMutationError::Command(DatabaseCommandError::InvalidValue))
}

async fn retain_candidate_site_revision(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &InstallStartupSnapshot,
    current: Option<&SiteHead>,
    version: u64,
    activated_at_ns: i64,
) -> Result<(), StartupSnapshotMutationError> {
    retain_site_revision(
        transaction,
        &command.candidate_digest,
        command.source_commit.as_ref(),
        current,
        version,
        activated_at_ns,
    )
    .await
}

async fn retain_site_revision(
    transaction: &mut Transaction<'_, Sqlite>,
    candidate: &SiteSnapshotDigest,
    source_commit: Option<&SourceCommit>,
    current: Option<&SiteHead>,
    version: u64,
    activated_at_ns: i64,
) -> Result<(), StartupSnapshotMutationError> {
    let stored_version = i64::try_from(version)
        .map_err(|_| StartupSnapshotMutationError::Command(DatabaseCommandError::InvalidValue))?;
    let retained: Option<(i64, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT version, source_commit FROM site_revisions WHERE site_revision_digest = ?",
    )
    .bind(candidate.as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(StartupSnapshotMutationError::Operation)?;
    if let Some((retained_version, source_commit)) = retained {
        let retained_version = u64::try_from(retained_version)
            .ok()
            .filter(|version| *version > 0)
            .ok_or(StartupSnapshotMutationError::CorruptStoredState)?;
        if !retained_revision_precedes_current(current, retained_version)
            || source_commit.is_some_and(|commit| decode_source_commit(commit).is_none())
        {
            return Err(StartupSnapshotMutationError::CorruptStoredState);
        }
    } else {
        sqlx::query(
            "INSERT INTO site_revisions (\
                    site_revision_digest, version, activated_at_ns, source_commit\
                 ) VALUES (?, ?, ?, ?)",
        )
        .bind(candidate.as_bytes().as_slice())
        .bind(stored_version)
        .bind(activated_at_ns)
        .bind(source_commit.map(SourceCommit::as_bytes))
        .execute(&mut **transaction)
        .await
        .map_err(StartupSnapshotMutationError::Operation)?;
    }
    Ok(())
}

fn retained_revision_precedes_current(current: Option<&SiteHead>, retained_version: u64) -> bool {
    current.is_some_and(|head| retained_version < head.version)
}

async fn advance_site_head(
    transaction: &mut Transaction<'_, Sqlite>,
    current: Option<&SiteHead>,
    candidate: &SiteSnapshotDigest,
    version: u64,
) -> Result<(), StartupSnapshotMutationError> {
    let stored_version = i64::try_from(version)
        .map_err(|_| StartupSnapshotMutationError::Command(DatabaseCommandError::InvalidValue))?;
    match current {
        None => {
            sqlx::query(
                "INSERT INTO site_state (singleton, current_site_digest, version) \
                 VALUES (1, ?, ?)",
            )
            .bind(candidate.as_bytes().as_slice())
            .bind(stored_version)
            .execute(&mut **transaction)
            .await
            .map_err(StartupSnapshotMutationError::Operation)?;
        }
        Some(expected) => {
            let expected_version = i64::try_from(expected.version).map_err(|_| {
                StartupSnapshotMutationError::Command(DatabaseCommandError::InvalidValue)
            })?;
            let updated = sqlx::query(
                "UPDATE site_state \
                 SET current_site_digest = ?, version = ? \
                 WHERE singleton = 1 AND current_site_digest = ? AND version = ?",
            )
            .bind(candidate.as_bytes().as_slice())
            .bind(stored_version)
            .bind(expected.digest.as_bytes().as_slice())
            .bind(expected_version)
            .execute(&mut **transaction)
            .await
            .map_err(StartupSnapshotMutationError::Operation)?;
            if updated.rows_affected() != 1 {
                return Err(StartupSnapshotMutationError::Command(
                    DatabaseCommandError::Rejected,
                ));
            }
        }
    }
    Ok(())
}

async fn record_observed_posts(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &InstallStartupSnapshot,
    first_observed_at_ns: i64,
) -> Result<(), StartupSnapshotMutationError> {
    for post in &command.posts {
        let stable_post_id = post.stable_post_id.as_uuid().into_bytes();
        let status = match post.publication_status {
            DraftStatus::Publishable => "publishable",
            DraftStatus::Draft => "draft",
        };
        let inserted = sqlx::query(
            "INSERT INTO post_revisions (\
                stable_post_id, revision_digest, publication_status, first_observed_at_ns, \
                slug, source_commit\
             ) VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(stable_post_id, revision_digest) DO NOTHING",
        )
        .bind(stable_post_id.as_slice())
        .bind(post.revision_digest.as_bytes().as_slice())
        .bind(status)
        .bind(first_observed_at_ns)
        .bind(post.slug.as_str())
        .bind(command.source_commit.as_ref().map(SourceCommit::as_bytes))
        .execute(&mut **transaction)
        .await
        .map_err(StartupSnapshotMutationError::Operation)?;
        if inserted.rows_affected() == 0 {
            let stored: Option<(String, String)> = sqlx::query_as(
                "SELECT slug, publication_status FROM post_revisions \
                 WHERE stable_post_id = ? AND revision_digest = ?",
            )
            .bind(stable_post_id.as_slice())
            .bind(post.revision_digest.as_bytes().as_slice())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(StartupSnapshotMutationError::Operation)?;
            if stored
                .as_ref()
                .map(|(slug, state)| (slug.as_str(), state.as_str()))
                != Some((post.slug.as_str(), status))
            {
                return Err(StartupSnapshotMutationError::CorruptStoredState);
            }
        }
    }
    Ok(())
}

pub(crate) enum StartupSnapshotMutationError {
    Command(DatabaseCommandError),
    Operation(sqlx::Error),
    CorruptStoredState,
}

impl StartupSnapshotMutationError {
    fn from_load(error: StartupSnapshotLoadError) -> Self {
        match error {
            StartupSnapshotLoadError::Query(source) => Self::Operation(source),
            _ => Self::CorruptStoredState,
        }
    }
}

pub(crate) async fn schedule_publication(
    transaction: &mut Transaction<'_, Sqlite>,
    command: SchedulePublication,
) -> Result<ScheduledPublication, PublicationMutationError> {
    if let Some(stored) = load_canonical_by_creation_key(transaction, command.creation_key).await? {
        if stored.command_kind != PublicationCommandKind::Scheduled
            || !publish_now_fingerprint_matches(
                &stored,
                &command.stable_post_id,
                command.expected_revision.as_ref(),
                &command.accepted_preview_digest,
            )
            || canonical_view(&stored.status).scheduled_at
                != command.scheduled_at.to_offset(UtcOffset::UTC)
        {
            return Err(PublicationMutationError::Command(
                DatabaseCommandError::IdempotencyConflict,
            ));
        }
        return match stored.status {
            CanonicalPublicationStatus::Scheduled(publication) => Ok(ScheduledPublication {
                publication_id: stored.publication_id,
                publication,
                creation_key: stored.creation_key,
                content_digest: stored.content_digest,
                accepted_preview_digest: stored.accepted_preview_digest,
            }),
            _ => Err(PublicationMutationError::Command(
                DatabaseCommandError::Rejected,
            )),
        };
    }

    let actual = load_site_head(&mut **transaction)
        .await
        .map_err(PublicationMutationError::from_load)?;
    if actual.as_ref() != Some(&command.expected_site)
        || command
            .expected_revision
            .as_ref()
            .is_some_and(|expected| expected != &command.pinned_post_digest)
    {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }
    require_publishable_revision(
        transaction,
        &command.stable_post_id,
        &command.pinned_post_digest,
    )
    .await?;
    require_no_pending_publication(transaction, &command.stable_post_id).await?;

    let publication = CanonicalPublication::schedule(
        command.stable_post_id.clone(),
        command.pinned_post_digest.clone(),
        command.source_commit.clone(),
        command.scheduled_at,
    );
    let view = publication.view();
    let scheduled_at_ns = i64::try_from(view.scheduled_at.unix_timestamp_nanos())
        .map_err(|_| PublicationMutationError::Command(DatabaseCommandError::InvalidValue))?;
    sqlx::query(
        "INSERT INTO canonical_publications (\
            publication_id, command_kind, stable_post_id, requested_revision_digest, pinned_post_digest, \
            state, version, scheduled_at_ns, source_commit, creation_key, content_tree_digest, \
            accepted_preview_digest\
         ) VALUES (?, 'scheduled', ?, ?, ?, 'scheduled', 1, ?, ?, ?, ?, ?)",
    )
    .bind(command.publication_id.as_bytes().as_slice())
    .bind(view.stable_post_id.as_uuid().as_bytes().as_slice())
    .bind(
        command
            .expected_revision
            .as_ref()
            .map(|digest| digest.as_bytes().as_slice()),
    )
    .bind(view.pinned_post_digest.as_bytes().as_slice())
    .bind(scheduled_at_ns)
    .bind(view.source_commit.as_ref().map(SourceCommit::as_bytes))
    .bind(command.creation_key.0.as_bytes().as_slice())
    .bind(command.content_digest.as_bytes().as_slice())
    .bind(command.accepted_preview_digest.as_bytes().as_slice())
    .execute(&mut **transaction)
    .await
    .map_err(PublicationMutationError::insert_sql)?;

    Ok(ScheduledPublication {
        publication_id: command.publication_id,
        publication,
        creation_key: Some(command.creation_key),
        content_digest: command.content_digest,
        accepted_preview_digest: command.accepted_preview_digest,
    })
}

pub(crate) async fn begin_scheduled_activation(
    transaction: &mut Transaction<'_, Sqlite>,
    command: BeginScheduledActivation,
) -> Result<BegunPublication, PublicationMutationError> {
    let stored = load_canonical_by_id(transaction, command.publication_id)
        .await?
        .ok_or(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ))?;
    let content_digest = stored.content_digest;
    let accepted_preview_digest = stored.accepted_preview_digest;
    let CanonicalPublicationStatus::Scheduled(publication) = stored.status else {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    };
    if publication.view().version != command.expected_publication_version {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }
    let actual = load_site_head(&mut **transaction)
        .await
        .map_err(PublicationMutationError::from_load)?;
    if actual.as_ref() != Some(&command.expected_site)
        || command.candidate_site_digest == command.expected_site.digest
    {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }
    let another_activation: bool = sqlx::query_scalar(
        "SELECT EXISTS(\
            SELECT 1 FROM canonical_publications \
            WHERE state = 'activating' AND publication_id != ?\
         )",
    )
    .bind(command.publication_id.as_bytes().as_slice())
    .fetch_one(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    if another_activation {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }
    let activating = publication
        .begin_activation(command.expected_publication_version, command.now)
        .map_err(|_| PublicationMutationError::Command(DatabaseCommandError::Rejected))?;
    let view = activating.view();
    let activation_at_ns = i64::try_from(
        view.activation_started_at
            .expect("an activating schedule has a timestamp")
            .unix_timestamp_nanos(),
    )
    .map_err(|_| PublicationMutationError::Command(DatabaseCommandError::InvalidValue))?;
    let version = i64::try_from(view.version)
        .map_err(|_| PublicationMutationError::Command(DatabaseCommandError::InvalidValue))?;
    let expected_version = i64::try_from(command.expected_publication_version)
        .map_err(|_| PublicationMutationError::Command(DatabaseCommandError::InvalidValue))?;
    let updated = sqlx::query(
        "UPDATE canonical_publications \
         SET state = 'activating', version = ?, activation_at_ns = ?, \
             activation_site_digest = ? \
         WHERE publication_id = ? AND state = 'scheduled' AND version = ? \
           AND scheduled_at_ns <= ?",
    )
    .bind(version)
    .bind(activation_at_ns)
    .bind(command.candidate_site_digest.as_bytes().as_slice())
    .bind(command.publication_id.as_bytes().as_slice())
    .bind(expected_version)
    .bind(activation_at_ns)
    .execute(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    if updated.rows_affected() != 1 {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }

    Ok(BegunPublication {
        publication_id: command.publication_id,
        publication: activating,
        site: command.expected_site,
        content_digest,
        accepted_preview_digest,
        candidate_site_digest: command.candidate_site_digest,
    })
}

pub(crate) async fn begin_publish_now(
    transaction: &mut Transaction<'_, Sqlite>,
    command: BeginPublishNow,
) -> Result<PublishNowState, PublicationMutationError> {
    if let Some(stored) = load_canonical_by_creation_key(transaction, command.creation_key).await? {
        return replay_publish_now(transaction, stored, &command).await;
    }

    let actual = load_site_head(&mut **transaction)
        .await
        .map_err(PublicationMutationError::from_load)?;
    if actual.as_ref() != Some(&command.expected_site)
        || command.candidate_site_digest == command.expected_site.digest
    {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }
    require_new_publication_candidate(transaction, &command).await?;

    let now_ns = i64::try_from(command.now.unix_timestamp_nanos())
        .map_err(|_| PublicationMutationError::Command(DatabaseCommandError::InvalidValue))?;
    let publication = CanonicalPublication::schedule(
        command.stable_post_id.clone(),
        command.pinned_post_digest.clone(),
        command.source_commit.clone(),
        command.now,
    )
    .begin_activation_now(1, command.now)
    .expect("a newly scheduled publication has version one");
    let view = publication.view();
    let publication_id = command.publication_id.into_bytes();
    let stable_post_id = view.stable_post_id.as_uuid().into_bytes();
    let version = i64::try_from(view.version)
        .map_err(|_| PublicationMutationError::Command(DatabaseCommandError::InvalidValue))?;

    sqlx::query(
        "INSERT INTO canonical_publications (\
            publication_id, command_kind, stable_post_id, requested_revision_digest, pinned_post_digest, \
            state, version, \
            scheduled_at_ns, activation_at_ns, source_commit, creation_key, \
            activation_site_digest, content_tree_digest, accepted_preview_digest\
         ) VALUES (?, 'immediate', ?, ?, ?, 'activating', ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(publication_id.as_slice())
    .bind(stable_post_id.as_slice())
    .bind(
        command
            .expected_revision
            .as_ref()
            .map(|digest| digest.as_bytes().as_slice()),
    )
    .bind(view.pinned_post_digest.as_bytes().as_slice())
    .bind(version)
    .bind(now_ns)
    .bind(now_ns)
    .bind(view.source_commit.as_ref().map(SourceCommit::as_bytes))
    .bind(command.creation_key.0.as_bytes().as_slice())
    .bind(command.candidate_site_digest.as_bytes().as_slice())
    .bind(command.content_digest.as_bytes().as_slice())
    .bind(command.accepted_preview_digest.as_bytes().as_slice())
    .execute(&mut **transaction)
    .await
    .map_err(PublicationMutationError::insert_sql)?;

    Ok(PublishNowState::Activating(BegunPublication {
        publication_id: command.publication_id,
        publication,
        site: command.expected_site,
        content_digest: command.content_digest,
        accepted_preview_digest: command.accepted_preview_digest,
        candidate_site_digest: command.candidate_site_digest,
    }))
}

async fn require_new_publication_candidate(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &BeginPublishNow,
) -> Result<(), PublicationMutationError> {
    let retained_candidate: bool = sqlx::query_scalar(
        "SELECT EXISTS(\
            SELECT 1 FROM site_revisions WHERE site_revision_digest = ?\
         )",
    )
    .bind(command.candidate_site_digest.as_bytes().as_slice())
    .fetch_one(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    if retained_candidate {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }

    require_publishable_revision(
        transaction,
        &command.stable_post_id,
        &command.pinned_post_digest,
    )
    .await?;

    let activating: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM canonical_publications WHERE state = 'activating')",
    )
    .fetch_one(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    if activating {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }

    require_no_pending_publication(transaction, &command.stable_post_id).await
}

async fn require_publishable_revision(
    transaction: &mut Transaction<'_, Sqlite>,
    post_id: &PostId,
    revision: &PostRevisionDigest,
) -> Result<(), PublicationMutationError> {
    let status: Option<String> = sqlx::query_scalar(
        "SELECT publication_status FROM post_revisions \
         WHERE stable_post_id = ? AND revision_digest = ?",
    )
    .bind(post_id.as_uuid().as_bytes().as_slice())
    .bind(revision.as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    match status.as_deref() {
        Some("publishable") => {}
        Some("draft") | None => {
            return Err(PublicationMutationError::Command(
                DatabaseCommandError::Rejected,
            ));
        }
        Some(_) => return Err(PublicationMutationError::CorruptStoredState),
    }

    Ok(())
}

async fn require_no_pending_publication(
    transaction: &mut Transaction<'_, Sqlite>,
    post_id: &PostId,
) -> Result<(), PublicationMutationError> {
    let active_for_post: bool = sqlx::query_scalar(
        "SELECT EXISTS(\
            SELECT 1 FROM canonical_publications \
            WHERE stable_post_id = ? \
              AND state IN ('scheduled', 'activating', 'blocked')\
         )",
    )
    .bind(post_id.as_uuid().as_bytes().as_slice())
    .fetch_one(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    if active_for_post {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }
    Ok(())
}

async fn load_canonical_by_creation_key(
    transaction: &mut Transaction<'_, Sqlite>,
    creation_key: CommandIdempotencyKey,
) -> Result<Option<StoredCanonicalPublication>, PublicationMutationError> {
    sqlx::query_as::<_, CanonicalPublicationRow>(LOAD_CANONICAL_BY_CREATION_KEY)
        .bind(creation_key.0.as_bytes().as_slice())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(PublicationMutationError::Operation)?
        .map(decode_canonical_publication)
        .transpose()
        .map_err(PublicationMutationError::from_load)
}

fn validate_publish_now_fingerprint(
    stored: &StoredCanonicalPublication,
    command: &LookupPublishNow,
) -> Result<(), PublishNowLookupError> {
    if stored.command_kind == PublicationCommandKind::Immediate
        && publish_now_fingerprint_matches(
            stored,
            &command.stable_post_id,
            command.expected_revision.as_ref(),
            &command.accepted_preview_digest,
        )
    {
        Ok(())
    } else {
        Err(PublishNowLookupError::IdempotencyConflict)
    }
}

fn validate_schedule_publication_fingerprint(
    stored: &StoredCanonicalPublication,
    command: &LookupSchedulePublication,
) -> Result<(), SchedulePublicationLookupError> {
    if stored.command_kind == PublicationCommandKind::Scheduled
        && publish_now_fingerprint_matches(
            stored,
            &command.stable_post_id,
            command.expected_revision.as_ref(),
            &command.accepted_preview_digest,
        )
        && canonical_view(&stored.status).scheduled_at
            == command.scheduled_at.to_offset(UtcOffset::UTC)
    {
        Ok(())
    } else {
        Err(SchedulePublicationLookupError::IdempotencyConflict)
    }
}

async fn load_replayed_schedule_publication(
    transaction: &mut Transaction<'_, Sqlite>,
    stored: StoredCanonicalPublication,
) -> Result<SchedulePublicationReplay, SchedulePublicationLookupError> {
    match stored.status {
        CanonicalPublicationStatus::Scheduled(publication) => {
            Ok(SchedulePublicationReplay::Scheduled(ScheduledPublication {
                publication_id: stored.publication_id,
                publication,
                creation_key: stored.creation_key,
                content_digest: stored.content_digest,
                accepted_preview_digest: stored.accepted_preview_digest,
            }))
        }
        CanonicalPublicationStatus::Activating(_) => {
            Err(SchedulePublicationLookupError::ActivationInProgress)
        }
        CanonicalPublicationStatus::Published(publication) => load_completed_publication(
            transaction,
            stored.publication_id,
            publication.view(),
            stored.activation_site_digest.as_ref(),
            stored.accepted_preview_digest,
        )
        .await
        .map(SchedulePublicationReplay::Published)
        .map_err(SchedulePublicationLookupError::from_publication),
        CanonicalPublicationStatus::Superseded(publication) => load_completed_publication(
            transaction,
            stored.publication_id,
            publication.view(),
            stored.activation_site_digest.as_ref(),
            stored.accepted_preview_digest,
        )
        .await
        .map(SchedulePublicationReplay::Published)
        .map_err(SchedulePublicationLookupError::from_publication),
        CanonicalPublicationStatus::Blocked(_) | CanonicalPublicationStatus::Cancelled(_) => {
            Err(SchedulePublicationLookupError::InvalidStoredState)
        }
    }
}

async fn load_replayed_publish_now(
    transaction: &mut Transaction<'_, Sqlite>,
    stored: StoredCanonicalPublication,
) -> Result<PublishNowState, PublishNowLookupError> {
    let candidate_site_digest = stored
        .activation_site_digest
        .ok_or(PublishNowLookupError::InvalidStoredState)?;
    let content_digest = stored.content_digest;
    let accepted_preview_digest = stored.accepted_preview_digest;
    match stored.status {
        CanonicalPublicationStatus::Activating(publication) => {
            let site = load_site_head(&mut **transaction)
                .await
                .map_err(PublishNowLookupError::from_load)?
                .ok_or(PublishNowLookupError::InvalidStoredState)?;
            Ok(PublishNowState::Activating(BegunPublication {
                publication_id: stored.publication_id,
                publication,
                site,
                content_digest,
                accepted_preview_digest,
                candidate_site_digest,
            }))
        }
        CanonicalPublicationStatus::Published(publication) => load_completed_publication(
            transaction,
            stored.publication_id,
            publication.view(),
            Some(&candidate_site_digest),
            accepted_preview_digest,
        )
        .await
        .map(PublishNowState::Published)
        .map_err(PublishNowLookupError::from_publication),
        CanonicalPublicationStatus::Superseded(publication) => load_completed_publication(
            transaction,
            stored.publication_id,
            publication.view(),
            Some(&candidate_site_digest),
            accepted_preview_digest,
        )
        .await
        .map(PublishNowState::Published)
        .map_err(PublishNowLookupError::from_publication),
        CanonicalPublicationStatus::Scheduled(_)
        | CanonicalPublicationStatus::Blocked(_)
        | CanonicalPublicationStatus::Cancelled(_) => {
            Err(PublishNowLookupError::InvalidStoredState)
        }
    }
}

async fn load_completed_publication(
    transaction: &mut Transaction<'_, Sqlite>,
    publication_id: Uuid,
    publication: &CanonicalPublicationView,
    activation_site_digest: Option<&SiteSnapshotDigest>,
    accepted_preview_digest: PreviewDigest,
) -> Result<CompletedPublication, PublicationMutationError> {
    let candidate_site_digest =
        activation_site_digest.ok_or(PublicationMutationError::CorruptStoredState)?;
    let published_at = publication
        .published_at
        .ok_or(PublicationMutationError::CorruptStoredState)?;
    if publication.current_published_digest.as_ref() != Some(&publication.pinned_post_digest) {
        return Err(PublicationMutationError::CorruptStoredState);
    }
    let site = load_retained_site_head(transaction, candidate_site_digest).await?;
    Ok(CompletedPublication {
        publication_id,
        stable_post_id: publication.stable_post_id.clone(),
        revision: publication.pinned_post_digest.clone(),
        accepted_preview_digest,
        published_at,
        site,
    })
}

async fn load_canonical_by_id(
    transaction: &mut Transaction<'_, Sqlite>,
    publication_id: Uuid,
) -> Result<Option<StoredCanonicalPublication>, PublicationMutationError> {
    sqlx::query_as::<_, CanonicalPublicationRow>(LOAD_CANONICAL_BY_PUBLICATION_ID)
        .bind(publication_id.as_bytes().as_slice())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(PublicationMutationError::Operation)?
        .map(decode_canonical_publication)
        .transpose()
        .map_err(PublicationMutationError::from_load)
}

async fn replay_publish_now(
    transaction: &mut Transaction<'_, Sqlite>,
    stored: StoredCanonicalPublication,
    command: &BeginPublishNow,
) -> Result<PublishNowState, PublicationMutationError> {
    if stored.command_kind != PublicationCommandKind::Immediate
        || !publish_now_fingerprint_matches(
            &stored,
            &command.stable_post_id,
            command.expected_revision.as_ref(),
            &command.accepted_preview_digest,
        )
    {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::IdempotencyConflict,
        ));
    }
    let candidate_site_digest = stored
        .activation_site_digest
        .ok_or(PublicationMutationError::CorruptStoredState)?;
    let content_digest = stored.content_digest;
    let accepted_preview_digest = stored.accepted_preview_digest;

    match stored.status {
        CanonicalPublicationStatus::Activating(publication) => {
            let site = load_site_head(&mut **transaction)
                .await
                .map_err(PublicationMutationError::from_load)?
                .ok_or(PublicationMutationError::CorruptStoredState)?;
            Ok(PublishNowState::Activating(BegunPublication {
                publication_id: stored.publication_id,
                publication,
                site,
                content_digest,
                accepted_preview_digest,
                candidate_site_digest,
            }))
        }
        CanonicalPublicationStatus::Published(publication) => load_completed_publication(
            transaction,
            stored.publication_id,
            publication.view(),
            Some(&candidate_site_digest),
            accepted_preview_digest,
        )
        .await
        .map(PublishNowState::Published),
        CanonicalPublicationStatus::Superseded(publication) => load_completed_publication(
            transaction,
            stored.publication_id,
            publication.view(),
            Some(&candidate_site_digest),
            accepted_preview_digest,
        )
        .await
        .map(PublishNowState::Published),
        CanonicalPublicationStatus::Scheduled(_)
        | CanonicalPublicationStatus::Blocked(_)
        | CanonicalPublicationStatus::Cancelled(_) => Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        )),
    }
}

pub(crate) async fn finish_publication(
    transaction: &mut Transaction<'_, Sqlite>,
    command: FinishPublication,
) -> Result<FinishedPublication, PublicationMutationError> {
    i64::try_from(command.expected_publication_version)
        .map_err(|_| PublicationMutationError::Command(DatabaseCommandError::InvalidValue))?;
    let stored = load_canonical_by_id(transaction, command.publication_id)
        .await?
        .ok_or(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ))?;
    if stored.activation_site_digest.as_ref() != Some(&command.candidate_site_digest)
        || stored.pinned_slug != command.slug
    {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }
    let accepted_preview_digest = stored.accepted_preview_digest;

    match stored.status {
        CanonicalPublicationStatus::Published(publication) => {
            finish_retry(
                transaction,
                stored.publication_id,
                publication,
                accepted_preview_digest,
                &command,
            )
            .await
        }
        CanonicalPublicationStatus::Activating(publication) => {
            finish_activation(
                transaction,
                stored.publication_id,
                publication,
                accepted_preview_digest,
                command,
            )
            .await
        }
        CanonicalPublicationStatus::Scheduled(_)
        | CanonicalPublicationStatus::Blocked(_)
        | CanonicalPublicationStatus::Superseded(_)
        | CanonicalPublicationStatus::Cancelled(_) => Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        )),
    }
}

async fn finish_retry(
    transaction: &mut Transaction<'_, Sqlite>,
    publication_id: Uuid,
    publication: CanonicalPublication<canonical::Published>,
    accepted_preview_digest: PreviewDigest,
    command: &FinishPublication,
) -> Result<FinishedPublication, PublicationMutationError> {
    if command.expected_publication_version.checked_add(1) != Some(publication.view().version) {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }
    require_claimed_route(transaction, &command.slug, publication.view()).await?;
    let site = load_retained_site_head(transaction, &command.candidate_site_digest).await?;
    Ok(FinishedPublication {
        publication_id,
        publication,
        site,
        accepted_preview_digest,
    })
}

async fn finish_activation(
    transaction: &mut Transaction<'_, Sqlite>,
    publication_id: Uuid,
    publication: CanonicalPublication<canonical::Activating>,
    accepted_preview_digest: PreviewDigest,
    command: FinishPublication,
) -> Result<FinishedPublication, PublicationMutationError> {
    if publication.view().version != command.expected_publication_version {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }
    let actual = load_site_head(&mut **transaction)
        .await
        .map_err(PublicationMutationError::from_load)?;
    if actual.as_ref() != Some(&command.expected_site) {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }

    let published = publication
        .commit_published(command.expected_publication_version)
        .expect("the stored publication version was checked");
    let published_at = published
        .view()
        .published_at
        .expect("a published publication has an activation timestamp");
    let published_at_ns = i64::try_from(published_at.unix_timestamp_nanos())
        .map_err(|_| PublicationMutationError::CorruptStoredState)?;
    let version =
        command
            .expected_site
            .version
            .checked_add(1)
            .ok_or(PublicationMutationError::Command(
                DatabaseCommandError::InvalidValue,
            ))?;

    retain_new_publication_site_revision(
        transaction,
        &command.candidate_site_digest,
        published.view().source_commit.as_ref(),
        version,
        published_at_ns,
    )
    .await?;
    supersede_current_publication(transaction, publication_id, published.view()).await?;
    advance_published_routes(transaction, published.view()).await?;
    claim_published_route(
        transaction,
        &command.slug,
        published.view(),
        published_at_ns,
    )
    .await?;
    persist_published(transaction, publication_id, &published, &command).await?;
    release_waiting_jobs(transaction, publication_id, &published).await?;
    advance_site_head(
        transaction,
        actual.as_ref(),
        &command.candidate_site_digest,
        version,
    )
    .await
    .map_err(PublicationMutationError::from_startup)?;

    Ok(FinishedPublication {
        publication_id,
        publication: published,
        site: SiteHead {
            digest: command.candidate_site_digest,
            version,
        },
        accepted_preview_digest,
    })
}

async fn supersede_current_publication(
    transaction: &mut Transaction<'_, Sqlite>,
    replacing_publication_id: Uuid,
    replacement: &CanonicalPublicationView,
) -> Result<(), PublicationMutationError> {
    let rows: Vec<(Vec<u8>, i64)> = sqlx::query_as(
        "SELECT publication_id, version FROM canonical_publications \
         WHERE stable_post_id = ? AND state = 'published' AND publication_id != ?",
    )
    .bind(replacement.stable_post_id.as_uuid().as_bytes().as_slice())
    .bind(replacing_publication_id.as_bytes().as_slice())
    .fetch_all(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    if rows.len() > 1 {
        return Err(PublicationMutationError::CorruptStoredState);
    }
    let Some((publication_id, version)) = rows.into_iter().next() else {
        return Ok(());
    };
    let version = u64::try_from(version)
        .ok()
        .filter(|version| *version >= 3)
        .ok_or(PublicationMutationError::CorruptStoredState)?;
    let next_version = version
        .checked_add(1)
        .and_then(|version| i64::try_from(version).ok())
        .ok_or(PublicationMutationError::Command(
            DatabaseCommandError::InvalidValue,
        ))?;
    let updated = sqlx::query(
        "UPDATE canonical_publications SET state = 'superseded', version = ? \
         WHERE publication_id = ? AND state = 'published' AND version = ?",
    )
    .bind(next_version)
    .bind(publication_id)
    .bind(i64::try_from(version).expect("validated publication version fits in i64"))
    .execute(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    if updated.rows_affected() != 1 {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }
    Ok(())
}

async fn advance_published_routes(
    transaction: &mut Transaction<'_, Sqlite>,
    replacement: &CanonicalPublicationView,
) -> Result<(), PublicationMutationError> {
    sqlx::query(
        "UPDATE published_routes SET revision_digest = ? \
         WHERE stable_post_id = ? AND kind = 'post'",
    )
    .bind(replacement.pinned_post_digest.as_bytes().as_slice())
    .bind(replacement.stable_post_id.as_uuid().as_bytes().as_slice())
    .execute(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    Ok(())
}

async fn retain_new_publication_site_revision(
    transaction: &mut Transaction<'_, Sqlite>,
    candidate: &SiteSnapshotDigest,
    source_commit: Option<&SourceCommit>,
    version: u64,
    activated_at_ns: i64,
) -> Result<(), PublicationMutationError> {
    let retained: bool = sqlx::query_scalar(
        "SELECT EXISTS(\
            SELECT 1 FROM site_revisions WHERE site_revision_digest = ?\
         )",
    )
    .bind(candidate.as_bytes().as_slice())
    .fetch_one(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    if retained {
        return Err(PublicationMutationError::CorruptStoredState);
    }
    let version = i64::try_from(version)
        .map_err(|_| PublicationMutationError::Command(DatabaseCommandError::InvalidValue))?;
    sqlx::query(
        "INSERT INTO site_revisions (\
            site_revision_digest, version, activated_at_ns, source_commit\
         ) VALUES (?, ?, ?, ?)",
    )
    .bind(candidate.as_bytes().as_slice())
    .bind(version)
    .bind(activated_at_ns)
    .bind(source_commit.map(SourceCommit::as_bytes))
    .execute(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    Ok(())
}

async fn persist_published(
    transaction: &mut Transaction<'_, Sqlite>,
    publication_id: Uuid,
    publication: &CanonicalPublication<canonical::Published>,
    command: &FinishPublication,
) -> Result<(), PublicationMutationError> {
    let view = publication.view();
    let version = i64::try_from(view.version)
        .map_err(|_| PublicationMutationError::Command(DatabaseCommandError::InvalidValue))?;
    let published_at_ns = i64::try_from(
        view.published_at
            .expect("a published publication has a timestamp")
            .unix_timestamp_nanos(),
    )
    .map_err(|_| PublicationMutationError::CorruptStoredState)?;
    let expected_version = i64::try_from(command.expected_publication_version)
        .map_err(|_| PublicationMutationError::Command(DatabaseCommandError::InvalidValue))?;
    let updated = sqlx::query(
        "UPDATE canonical_publications \
         SET state = 'published', version = ?, published_at_ns = ?, \
             current_published_digest = ?, block_reason = NULL \
         WHERE publication_id = ? AND state = 'activating' AND version = ? \
           AND activation_site_digest = ?",
    )
    .bind(version)
    .bind(published_at_ns)
    .bind(
        view.current_published_digest
            .as_ref()
            .expect("a published publication has a current digest")
            .as_bytes()
            .as_slice(),
    )
    .bind(publication_id.as_bytes().as_slice())
    .bind(expected_version)
    .bind(command.candidate_site_digest.as_bytes().as_slice())
    .execute(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    if updated.rows_affected() != 1 {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }
    Ok(())
}

async fn claim_published_route(
    transaction: &mut Transaction<'_, Sqlite>,
    slug: &PostSlug,
    publication: &CanonicalPublicationView,
    claimed_at_ns: i64,
) -> Result<(), PublicationMutationError> {
    let stable_post_id = publication.stable_post_id.as_uuid();
    let inserted = sqlx::query(
        "INSERT INTO published_routes (\
            route, stable_post_id, revision_digest, kind, claimed_at_ns\
         ) VALUES (?, ?, ?, 'post', ?) \
         ON CONFLICT(route) DO NOTHING",
    )
    .bind(slug.as_str())
    .bind(stable_post_id.as_bytes().as_slice())
    .bind(publication.pinned_post_digest.as_bytes().as_slice())
    .bind(claimed_at_ns)
    .execute(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    if inserted.rows_affected() == 0 {
        require_claimed_route(transaction, slug, publication).await?;
    }
    Ok(())
}

async fn require_claimed_route(
    transaction: &mut Transaction<'_, Sqlite>,
    slug: &PostSlug,
    publication: &CanonicalPublicationView,
) -> Result<(), PublicationMutationError> {
    let claimed: Option<(Vec<u8>, Vec<u8>, String)> = sqlx::query_as(
        "SELECT stable_post_id, revision_digest, kind FROM published_routes WHERE route = ?",
    )
    .bind(slug.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    let stable_post_id = publication.stable_post_id.as_uuid();
    let matches = claimed.is_some_and(|(post_id, digest, kind)| {
        post_id == stable_post_id.as_bytes().as_slice()
            && digest == publication.pinned_post_digest.as_bytes().as_slice()
            && kind == "post"
    });
    if !matches {
        return Err(PublicationMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    }
    Ok(())
}

async fn release_waiting_jobs(
    transaction: &mut Transaction<'_, Sqlite>,
    publication_id: Uuid,
    publication: &CanonicalPublication<canonical::Published>,
) -> Result<(), PublicationMutationError> {
    let rows = sqlx::query_as::<_, TargetJobRow>(
        "SELECT \
            job.publication_job_id AS publication_job_id, \
            job.publication_id AS publication_id, job.state AS state, job.target AS target, \
            canonical.stable_post_id AS stable_post_id, \
            canonical.pinned_post_digest AS pinned_post_digest, \
            job.scheduled_at_ns AS scheduled_at_ns, job.payload_version AS payload_version, \
            job.payload_body AS payload_body, job.payload_digest AS payload_digest, \
            job.version AS version \
         FROM publication_jobs AS job \
         JOIN canonical_publications AS canonical ON canonical.publication_id = job.publication_id \
         WHERE job.publication_id = ? ORDER BY job.publication_job_id",
    )
    .bind(publication_id.as_bytes().as_slice())
    .fetch_all(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    let now = publication
        .view()
        .published_at
        .expect("a published publication has a timestamp");
    for row in rows {
        let stored = StoredTargetJob::try_from(row)
            .map_err(|_| PublicationMutationError::CorruptStoredState)?;
        let TargetJobStatus::WaitingForCanonical(job) = stored.status else {
            return Err(PublicationMutationError::CorruptStoredState);
        };
        let expected_version = job.view().version;
        let released = job
            .release_after_canonical(expected_version, publication, now)
            .map_err(|_| PublicationMutationError::CorruptStoredState)?;
        let (state, version) = match released {
            ReleasedTargetJob::Scheduled(job) => ("scheduled", job.view().version),
            ReleasedTargetJob::Ready(job) => ("ready", job.view().version),
        };
        let version =
            i64::try_from(version).map_err(|_| PublicationMutationError::CorruptStoredState)?;
        let expected_version = i64::try_from(expected_version)
            .map_err(|_| PublicationMutationError::CorruptStoredState)?;
        let updated = sqlx::query(
            "UPDATE publication_jobs SET state = ?, version = ? \
             WHERE publication_job_id = ? AND state = 'waiting_for_canonical' AND version = ?",
        )
        .bind(state)
        .bind(version)
        .bind(stored.publication_job_id.as_bytes().as_slice())
        .bind(expected_version)
        .execute(&mut **transaction)
        .await
        .map_err(PublicationMutationError::Operation)?;
        if updated.rows_affected() != 1 {
            return Err(PublicationMutationError::Command(
                DatabaseCommandError::Rejected,
            ));
        }
    }
    Ok(())
}

async fn load_retained_site_head(
    transaction: &mut Transaction<'_, Sqlite>,
    digest: &SiteSnapshotDigest,
) -> Result<SiteHead, PublicationMutationError> {
    let row: Option<(i64, i64, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT version, activated_at_ns, source_commit FROM site_revisions \
         WHERE site_revision_digest = ?",
    )
    .bind(digest.as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(PublicationMutationError::Operation)?;
    let Some((version, activated_at_ns, source_commit)) = row else {
        return Err(PublicationMutationError::CorruptStoredState);
    };
    let version = u64::try_from(version)
        .ok()
        .filter(|version| *version > 0)
        .ok_or(PublicationMutationError::CorruptStoredState)?;
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(activated_at_ns))
        .map_err(|_| PublicationMutationError::CorruptStoredState)?;
    if source_commit.is_some_and(|commit| decode_source_commit(commit).is_none()) {
        return Err(PublicationMutationError::CorruptStoredState);
    }
    Ok(SiteHead {
        digest: digest.clone(),
        version,
    })
}

pub(crate) enum PublicationMutationError {
    Command(DatabaseCommandError),
    Operation(sqlx::Error),
    CorruptStoredState,
}

impl PublicationMutationError {
    fn from_load(error: StartupSnapshotLoadError) -> Self {
        match error {
            StartupSnapshotLoadError::Query(source) => Self::Operation(source),
            _ => Self::CorruptStoredState,
        }
    }

    fn from_startup(error: StartupSnapshotMutationError) -> Self {
        match error {
            StartupSnapshotMutationError::Command(error) => Self::Command(error),
            StartupSnapshotMutationError::Operation(source) => Self::Operation(source),
            StartupSnapshotMutationError::CorruptStoredState => Self::CorruptStoredState,
        }
    }

    fn insert_sql(source: sqlx::Error) -> Self {
        let is_rejection = source.as_database_error().is_some_and(|error| {
            matches!(
                error.kind(),
                ErrorKind::UniqueViolation
                    | ErrorKind::ForeignKeyViolation
                    | ErrorKind::NotNullViolation
                    | ErrorKind::CheckViolation
            )
        });
        if is_rejection {
            Self::Command(DatabaseCommandError::Rejected)
        } else {
            Self::Operation(source)
        }
    }
}

pub(crate) async fn create(
    transaction: &mut Transaction<'_, Sqlite>,
    command: CreateTargetJob,
) -> Result<StoredTargetJob, TargetJobMutationError> {
    let key = command.idempotency_key.0.into_bytes();
    let view = command.job.view();
    let scheduled_at_ns = i64::try_from(view.scheduled_at.unix_timestamp_nanos())
        .map_err(|_| TargetJobMutationError::Command(DatabaseCommandError::InvalidValue))?;
    let version = i64::try_from(view.version)
        .map_err(|_| TargetJobMutationError::Command(DatabaseCommandError::InvalidValue))?;
    let target = match view.target {
        DistributionTarget::X => "x",
    };
    let payload_digest = view.payload.digest();
    let publication_job_id = command.publication_job_id.into_bytes();
    let publication_id = command.publication_id.into_bytes();
    let stable_post_id = view.stable_post_id.as_uuid().into_bytes();

    // The SELECT makes canonical identity validation part of the insert.
    let inserted = sqlx::query(
        "INSERT INTO publication_jobs (\
            publication_job_id, publication_id, idempotency_key, state, target, version, \
            scheduled_at_ns, payload_version, payload_digest, payload_body\
         ) \
         SELECT ?, ?, ?, 'waiting_for_canonical', ?, ?, ?, ?, ?, ? \
         FROM canonical_publications AS canonical \
         WHERE canonical.publication_id = ? \
           AND canonical.stable_post_id = ? \
           AND canonical.pinned_post_digest = ? \
         ON CONFLICT(idempotency_key) DO NOTHING",
    )
    .bind(publication_job_id.as_slice())
    .bind(publication_id.as_slice())
    .bind(key.as_slice())
    .bind(target)
    .bind(version)
    .bind(scheduled_at_ns)
    .bind(i64::from(view.payload.version()))
    .bind(payload_digest.as_bytes().as_slice())
    .bind(view.payload.body())
    .bind(publication_id.as_slice())
    .bind(stable_post_id.as_slice())
    .bind(view.pinned_post_digest.as_bytes().as_slice())
    .execute(&mut **transaction)
    .await
    .map_err(TargetJobMutationError::insert_sql)?;

    if inserted.rows_affected() == 0 {
        return load_retry(transaction, &command).await;
    }

    Ok(StoredTargetJob {
        publication_job_id: command.publication_job_id,
        publication_id: command.publication_id,
        status: TargetJobStatus::WaitingForCanonical(command.job),
    })
}

async fn load_retry(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &CreateTargetJob,
) -> Result<StoredTargetJob, TargetJobMutationError> {
    let view = command.job.view();
    let scheduled_at_ns = i64::try_from(view.scheduled_at.unix_timestamp_nanos())
        .map_err(|_| TargetJobMutationError::Command(DatabaseCommandError::InvalidValue))?;
    let target = match view.target {
        DistributionTarget::X => "x",
    };
    let payload_digest = view.payload.digest();
    let stable_post_id = view.stable_post_id.as_uuid();

    // State and version may advance after creation, so they are not part of
    // the immutable idempotency projection.
    let prior = sqlx::query_as::<_, (Vec<u8>, i64)>(
        "SELECT job.publication_job_id, (\
            job.publication_job_id = ? \
            AND job.publication_id = ? \
            AND canonical.stable_post_id = ? \
            AND canonical.pinned_post_digest = ? \
            AND job.target = ? \
            AND job.scheduled_at_ns = ? \
            AND job.payload_version = ? \
            AND job.payload_digest = ? \
            AND job.payload_body = ?\
         ) AS matches_command \
         FROM publication_jobs AS job \
         JOIN canonical_publications AS canonical \
           ON canonical.publication_id = job.publication_id \
         WHERE job.idempotency_key = ?",
    )
    .bind(command.publication_job_id.as_bytes().as_slice())
    .bind(command.publication_id.as_bytes().as_slice())
    .bind(stable_post_id.as_bytes().as_slice())
    .bind(view.pinned_post_digest.as_bytes().as_slice())
    .bind(target)
    .bind(scheduled_at_ns)
    .bind(i64::from(view.payload.version()))
    .bind(payload_digest.as_bytes().as_slice())
    .bind(view.payload.body())
    .bind(command.idempotency_key.0.as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(TargetJobMutationError::Operation)?;

    let Some((prior_job_id, matches_command)) = prior else {
        return Err(TargetJobMutationError::Command(
            DatabaseCommandError::Rejected,
        ));
    };
    if matches_command != 1 {
        return Err(TargetJobMutationError::Command(
            DatabaseCommandError::IdempotencyConflict,
        ));
    }

    let prior_job_id =
        Uuid::from_slice(&prior_job_id).map_err(|_| TargetJobMutationError::CorruptStoredJob)?;
    load_by_id(&mut **transaction, prior_job_id)
        .await
        .map_err(|error| match error {
            TargetJobLoadError::Query(source) => TargetJobMutationError::Operation(source),
            _ => TargetJobMutationError::CorruptStoredJob,
        })?
        .ok_or(TargetJobMutationError::CorruptStoredJob)
}

pub(crate) enum TargetJobMutationError {
    Command(DatabaseCommandError),
    Operation(sqlx::Error),
    CorruptStoredJob,
}

impl TargetJobMutationError {
    fn insert_sql(source: sqlx::Error) -> Self {
        let is_rejection = source.as_database_error().is_some_and(|error| {
            matches!(
                error.kind(),
                ErrorKind::UniqueViolation
                    | ErrorKind::ForeignKeyViolation
                    | ErrorKind::NotNullViolation
                    | ErrorKind::CheckViolation
            )
        });
        if is_rejection {
            Self::Command(DatabaseCommandError::Rejected)
        } else {
            Self::Operation(source)
        }
    }
}

impl TryFrom<TargetJobRow> for StoredTargetJob {
    type Error = TargetJobLoadError;

    fn try_from(row: TargetJobRow) -> Result<Self, Self::Error> {
        let publication_job_id = Uuid::from_slice(&row.publication_job_id)
            .map_err(|_| TargetJobLoadError::InvalidPublicationJobId)?;
        let publication_id = Uuid::from_slice(&row.publication_id)
            .map_err(|_| TargetJobLoadError::InvalidPublicationId)?;
        let stable_post_id =
            Uuid::from_slice(&row.stable_post_id).map_err(|_| TargetJobLoadError::InvalidPostId)?;
        let stable_post_id = PostId::parse(&stable_post_id.hyphenated().to_string())
            .map_err(|_| TargetJobLoadError::InvalidPostId)?;
        let payload_version = u16::try_from(row.payload_version).map_err(|_| {
            TargetJobLoadError::PayloadVersionOutOfRange {
                value: row.payload_version,
            }
        })?;
        let version = u64::try_from(row.version)
            .map_err(|_| TargetJobLoadError::VersionOutOfRange { value: row.version })?;
        let pinned_post_digest = row
            .pinned_post_digest
            .try_into()
            .map(PostRevisionDigest::from_bytes)
            .map_err(|_| TargetJobLoadError::InvalidPinnedPostDigest)?;
        let payload_digest = row
            .payload_digest
            .try_into()
            .map(TargetPayloadDigest::from_bytes)
            .map_err(|_| TargetJobLoadError::InvalidPayloadDigest)?;
        let view = TargetJobView {
            state: parse_state(&row.state)?,
            target: parse_target(&row.target)?,
            stable_post_id,
            pinned_post_digest,
            scheduled_at: timestamp_from_nanos(i128::from(row.scheduled_at_ns))?,
            payload: TargetPayload::from_version(payload_version, row.payload_body)
                .map_err(TargetJobLoadError::InvalidPayload)?,
            payload_digest,
            version,
        };

        Ok(Self {
            publication_job_id,
            publication_id,
            status: TargetJobStatus::try_from(view)
                .map_err(TargetJobLoadError::InvalidDomainState)?,
        })
    }
}

fn parse_state(value: &str) -> Result<TargetJobState, TargetJobLoadError> {
    match value {
        "waiting_for_canonical" => Ok(TargetJobState::WaitingForCanonical),
        "scheduled" => Ok(TargetJobState::Scheduled),
        "ready" => Ok(TargetJobState::Ready),
        "running" => Ok(TargetJobState::Running),
        "succeeded" => Ok(TargetJobState::Succeeded),
        "failed" => Ok(TargetJobState::Failed),
        "outcome_unknown" => Ok(TargetJobState::OutcomeUnknown),
        "cancelled" => Ok(TargetJobState::Cancelled),
        _ => Err(TargetJobLoadError::InvalidState),
    }
}

fn parse_target(value: &str) -> Result<DistributionTarget, TargetJobLoadError> {
    match value {
        "x" => Ok(DistributionTarget::X),
        _ => Err(TargetJobLoadError::InvalidTarget),
    }
}

fn timestamp_from_nanos(nanoseconds: i128) -> Result<OffsetDateTime, TargetJobLoadError> {
    OffsetDateTime::from_unix_timestamp_nanos(nanoseconds)
        .map(|timestamp| timestamp.to_offset(UtcOffset::UTC))
        .map_err(|_| TargetJobLoadError::TimestampOutOfRange { nanoseconds })
}

#[derive(Debug, Error)]
pub(crate) enum TargetJobLoadError {
    #[error("could not read the target job")]
    Query(#[from] sqlx::Error),
    #[error("stored publication job ID is not a 16-byte UUID")]
    InvalidPublicationJobId,
    #[error("stored publication ID is not a 16-byte UUID")]
    InvalidPublicationId,
    #[error("stored target job state is not recognized")]
    InvalidState,
    #[error("stored distribution target is not recognized")]
    InvalidTarget,
    #[error("stored post ID is invalid")]
    InvalidPostId,
    #[error("stored pinned post digest is not 32 bytes")]
    InvalidPinnedPostDigest,
    #[error("stored payload version {value} is outside the unsigned 16-bit range")]
    PayloadVersionOutOfRange { value: i64 },
    #[error("stored target payload is invalid")]
    InvalidPayload(#[source] PayloadError),
    #[error("stored target payload digest is not 32 bytes")]
    InvalidPayloadDigest,
    #[error("stored target job timestamp {nanoseconds}ns is outside the supported range")]
    TimestampOutOfRange { nanoseconds: i128 },
    #[error("stored target job version {value} is outside the unsigned 64-bit range")]
    VersionOutOfRange { value: i64 },
    #[error("stored target job violates its domain invariants")]
    InvalidDomainState(#[source] RehydrationError),
}

#[cfg(test)]
mod tests {
    use sqlx::{
        Connection as _, Executor as _, SqliteConnection, SqlitePool, sqlite::SqlitePoolOptions,
    };

    use super::*;
    use crate::domain::distribution::CURRENT_PAYLOAD_VERSION;

    const JOB_ID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    const PUBLICATION_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const REVISION: &str =
        "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111";

    async fn startup_store() -> (PublicationStore, SqlitePool) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        for statement in [
            "CREATE TABLE site_revisions (\
                site_revision_digest BLOB PRIMARY KEY, version INTEGER NOT NULL UNIQUE, \
                activated_at_ns INTEGER NOT NULL, source_commit BLOB\
             ) STRICT",
            "CREATE TABLE site_state (\
                singleton INTEGER PRIMARY KEY, current_site_digest BLOB NOT NULL, \
                version INTEGER NOT NULL\
             ) STRICT",
            "CREATE TABLE post_revisions (\
                stable_post_id BLOB NOT NULL, revision_digest BLOB NOT NULL, \
                publication_status TEXT NOT NULL, first_observed_at_ns INTEGER NOT NULL, \
                slug TEXT NOT NULL, source_commit BLOB, \
                PRIMARY KEY (stable_post_id, revision_digest)\
             ) STRICT",
            "CREATE TABLE canonical_publications (\
                publication_id BLOB PRIMARY KEY, creation_key BLOB UNIQUE, \
                command_kind TEXT NOT NULL, \
                stable_post_id BLOB NOT NULL, \
                requested_revision_digest BLOB, \
                pinned_post_digest BLOB NOT NULL, state TEXT NOT NULL, version INTEGER NOT NULL, \
                scheduled_at_ns INTEGER NOT NULL, activation_at_ns INTEGER, \
                activation_site_digest BLOB, \
                published_at_ns INTEGER, current_published_digest BLOB, source_commit BLOB, \
                block_reason TEXT, \
                content_tree_digest BLOB NOT NULL CHECK (length(content_tree_digest) = 32), \
                accepted_preview_digest BLOB NOT NULL \
                    CHECK (length(accepted_preview_digest) = 32)\
             ) STRICT",
            "CREATE TABLE reload_operations (\
                reload_operation_id BLOB PRIMARY KEY, state TEXT NOT NULL\
             ) STRICT",
        ] {
            pool.execute(statement).await.unwrap();
        }
        let (mutations, _receiver) = mpsc::channel(1);
        (PublicationStore::new(pool.clone(), mutations), pool)
    }

    async fn insert_site_head(pool: &SqlitePool, digest: &[u8], version: i64) {
        sqlx::query(
            "INSERT INTO site_revisions (\
                site_revision_digest, version, activated_at_ns\
             ) VALUES (?, ?, 100)",
        )
        .bind(digest)
        .bind(version)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO site_state (singleton, current_site_digest, version) VALUES (1, ?, ?)",
        )
        .bind(digest)
        .bind(version)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_published_publication(pool: &SqlitePool, state: &str) {
        let post_id = uuid_bytes(POST_ID);
        let revision = [0x11_u8; 32];
        sqlx::query(
            "INSERT INTO post_revisions (\
                stable_post_id, revision_digest, publication_status, first_observed_at_ns, slug\
             ) VALUES (?, ?, 'publishable', 1, 'published-post')",
        )
        .bind(&post_id)
        .bind(revision.as_slice())
        .execute(pool)
        .await
        .unwrap();
        let (version, activation_at, published_at, current_digest) = match state {
            "published" => (
                3_i64,
                Some(200_i64),
                Some(200_i64),
                Some(revision.as_slice()),
            ),
            "activating" => (2_i64, Some(200_i64), None, None),
            _ => (1_i64, None, None, None),
        };
        let activation_site_digest =
            matches!(state, "activating" | "published").then_some([0x66_u8; 32]);
        let creation_key = matches!(state, "activating" | "published")
            .then(|| uuid_bytes("99999999-9999-4999-8999-999999999999"));
        let content_digest = [0x77_u8; 32];
        let accepted_preview_digest = [0x88_u8; 32];
        let command_kind = if state == "scheduled" {
            "scheduled"
        } else {
            "immediate"
        };
        sqlx::query(
            "INSERT INTO canonical_publications (\
                publication_id, creation_key, command_kind, stable_post_id, pinned_post_digest, state, version, \
                scheduled_at_ns, activation_at_ns, activation_site_digest, published_at_ns, \
                current_published_digest, content_tree_digest, accepted_preview_digest\
             ) VALUES (?, ?, ?, ?, ?, ?, ?, 100, ?, ?, ?, ?, ?, ?)",
        )
        .bind(uuid_bytes(PUBLICATION_ID))
        .bind(creation_key)
        .bind(command_kind)
        .bind(post_id)
        .bind(revision.as_slice())
        .bind(state)
        .bind(version)
        .bind(activation_at)
        .bind(activation_site_digest.map(|digest| digest.to_vec()))
        .bind(published_at)
        .bind(current_digest)
        .bind(content_digest.as_slice())
        .bind(accepted_preview_digest.as_slice())
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn startup_snapshot_state_accepts_a_pristine_database() {
        let (store, pool) = startup_store().await;

        let state = store.startup_snapshot_state().await.unwrap();

        assert_eq!(state.site, None);
        assert!(state.ledger.is_empty());
        assert!(state.activating.is_empty());
        pool.close().await;
    }

    #[tokio::test]
    async fn startup_snapshot_state_accepts_a_published_row_with_required_bindings() {
        let (store, pool) = startup_store().await;
        let site = [0x22_u8; 32];
        insert_site_head(&pool, &site, 1).await;
        insert_published_publication(&pool, "published").await;

        let state = store.startup_snapshot_state().await.unwrap();

        assert_eq!(state.site.unwrap().digest.as_bytes(), &site);
        assert_eq!(state.ledger.len(), 1);
        pool.close().await;
    }

    #[tokio::test]
    async fn superseded_immediate_replay_returns_the_historical_completion() {
        let (store, pool) = startup_store().await;
        insert_site_head(&pool, &[0x66_u8; 32], 1).await;
        insert_published_publication(&pool, "published").await;
        sqlx::query("UPDATE canonical_publications SET state = 'superseded', version = 4")
            .execute(&pool)
            .await
            .unwrap();

        let replay = store
            .publish_now_replay(LookupPublishNow {
                creation_key: CommandIdempotencyKey::new(
                    Uuid::parse_str("99999999-9999-4999-8999-999999999999").unwrap(),
                ),
                stable_post_id: PostId::parse(POST_ID).unwrap(),
                expected_revision: None,
                accepted_preview_digest: PreviewDigest::from_bytes([0x88; 32]),
            })
            .await
            .unwrap()
            .unwrap();
        let PublishNowState::Published(finished) = replay else {
            panic!("published creation key must replay its stored result");
        };
        assert_eq!(finished.revision.as_bytes(), &[0x11; 32]);
        assert_eq!(finished.site.digest.as_bytes(), &[0x66; 32]);
        assert_eq!(finished.accepted_preview_digest.as_bytes(), &[0x88; 32]);

        assert!(matches!(
            store
                .schedule_publication_replay(LookupSchedulePublication {
                    creation_key: CommandIdempotencyKey::new(
                        Uuid::parse_str("99999999-9999-4999-8999-999999999999").unwrap(),
                    ),
                    stable_post_id: PostId::parse(POST_ID).unwrap(),
                    expected_revision: None,
                    accepted_preview_digest: PreviewDigest::from_bytes([0x88; 32]),
                    scheduled_at: OffsetDateTime::from_unix_timestamp_nanos(100).unwrap(),
                })
                .await,
            Err(SchedulePublicationLookupError::IdempotencyConflict)
        ));

        let conflict = store
            .publish_now_replay(LookupPublishNow {
                creation_key: CommandIdempotencyKey::new(
                    Uuid::parse_str("99999999-9999-4999-8999-999999999999").unwrap(),
                ),
                stable_post_id: PostId::parse(POST_ID).unwrap(),
                expected_revision: None,
                accepted_preview_digest: PreviewDigest::from_bytes([0x89; 32]),
            })
            .await;
        assert!(matches!(
            conflict,
            Err(PublishNowLookupError::IdempotencyConflict)
        ));
        pool.close().await;
    }

    #[tokio::test]
    async fn scheduled_replay_returns_the_original_candidate_before_catalog_resolution() {
        let (store, pool) = startup_store().await;
        insert_published_publication(&pool, "scheduled").await;
        let creation_key = Uuid::parse_str("99999999-9999-4999-8999-999999999999").unwrap();
        sqlx::query("UPDATE canonical_publications SET creation_key = ?")
            .bind(creation_key.as_bytes().as_slice())
            .execute(&pool)
            .await
            .unwrap();
        let scheduled_at = OffsetDateTime::from_unix_timestamp_nanos(100).unwrap();
        let exact_lookup = || LookupSchedulePublication {
            creation_key: CommandIdempotencyKey::new(creation_key),
            stable_post_id: PostId::parse(POST_ID).unwrap(),
            expected_revision: None,
            accepted_preview_digest: PreviewDigest::from_bytes([0x88; 32]),
            scheduled_at,
        };

        assert!(matches!(
            store
                .publish_now_replay(LookupPublishNow {
                    creation_key: CommandIdempotencyKey::new(creation_key),
                    stable_post_id: PostId::parse(POST_ID).unwrap(),
                    expected_revision: None,
                    accepted_preview_digest: PreviewDigest::from_bytes([0x88; 32]),
                })
                .await,
            Err(PublishNowLookupError::IdempotencyConflict)
        ));

        assert!(
            store
                .schedule_publication_replay(LookupSchedulePublication {
                    creation_key: CommandIdempotencyKey::new(
                        Uuid::parse_str("88888888-8888-4888-8888-888888888888").unwrap(),
                    ),
                    ..exact_lookup()
                })
                .await
                .unwrap()
                .is_none()
        );

        let replayed = store
            .schedule_publication_replay(exact_lookup())
            .await
            .unwrap()
            .unwrap();
        let SchedulePublicationReplay::Scheduled(replayed) = replayed else {
            panic!("the retained approval must still be scheduled");
        };
        let current_server_content_digest = ContentTreeDigest::from_bytes([0x99; 32]);
        assert_eq!(replayed.content_digest.as_bytes(), &[0x77; 32]);
        assert_ne!(replayed.content_digest, current_server_content_digest);
        assert_eq!(replayed.accepted_preview_digest.as_bytes(), &[0x88; 32]);
        assert_eq!(replayed.publication.view().scheduled_at, scheduled_at);

        for conflict in [
            LookupSchedulePublication {
                stable_post_id: PostId::parse("22222222-2222-4222-8222-222222222222").unwrap(),
                ..exact_lookup()
            },
            LookupSchedulePublication {
                expected_revision: Some(PostRevisionDigest::from_bytes([0x11; 32])),
                ..exact_lookup()
            },
            LookupSchedulePublication {
                accepted_preview_digest: PreviewDigest::from_bytes([0x89; 32]),
                ..exact_lookup()
            },
            LookupSchedulePublication {
                scheduled_at: OffsetDateTime::from_unix_timestamp_nanos(101).unwrap(),
                ..exact_lookup()
            },
        ] {
            assert!(matches!(
                store.schedule_publication_replay(conflict).await,
                Err(SchedulePublicationLookupError::IdempotencyConflict)
            ));
        }

        sqlx::query(
            "UPDATE canonical_publications \
             SET state = 'activating', version = 2, activation_at_ns = 200, \
                 activation_site_digest = ?",
        )
        .bind([0x66_u8; 32].as_slice())
        .execute(&pool)
        .await
        .unwrap();
        assert!(matches!(
            store.schedule_publication_replay(exact_lookup()).await,
            Err(SchedulePublicationLookupError::ActivationInProgress)
        ));

        insert_site_head(&pool, &[0x66_u8; 32], 1).await;
        sqlx::query(
            "UPDATE canonical_publications \
             SET state = 'published', version = 3, published_at_ns = 200, \
                 current_published_digest = pinned_post_digest",
        )
        .execute(&pool)
        .await
        .unwrap();
        for state in ["published", "superseded"] {
            if state == "superseded" {
                sqlx::query(
                    "UPDATE canonical_publications SET state = 'superseded', version = 4",
                )
                .execute(&pool)
                .await
                .unwrap();
            }
            let replay = store
                .schedule_publication_replay(exact_lookup())
                .await
                .unwrap()
                .unwrap();
            let SchedulePublicationReplay::Published(completed) = replay else {
                panic!("a completed scheduled approval must replay as published");
            };
            assert_eq!(completed.publication_id, Uuid::parse_str(PUBLICATION_ID).unwrap());
            assert_eq!(completed.revision.as_bytes(), &[0x11; 32]);
            assert_eq!(completed.accepted_preview_digest.as_bytes(), &[0x88; 32]);
            assert_eq!(completed.published_at.unix_timestamp_nanos(), 200);
            assert_eq!(completed.site.digest.as_bytes(), &[0x66; 32]);
        }
        pool.close().await;
    }

    #[tokio::test]
    async fn startup_snapshot_state_returns_one_recoverable_activation() {
        let (store, pool) = startup_store().await;
        insert_site_head(&pool, &[0x33_u8; 32], 1).await;
        insert_published_publication(&pool, "activating").await;

        let state = store.startup_snapshot_state().await.unwrap();
        let activation = state.activating.first().unwrap();
        assert_eq!(
            activation.publication_id,
            Uuid::parse_str(PUBLICATION_ID).unwrap()
        );
        assert_eq!(activation.candidate_site_digest.as_bytes(), &[0x66; 32]);
        assert_eq!(activation.content_digest.as_bytes(), &[0x77; 32]);
        assert_eq!(activation.accepted_preview_digest.as_bytes(), &[0x88; 32]);
        assert_eq!(
            activation.publication.view().state,
            CanonicalState::Activating
        );
        assert!(activation.creation_key.is_some());

        sqlx::query("DELETE FROM canonical_publications")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO reload_operations (reload_operation_id, state) VALUES (?, 'applying')",
        )
        .bind([0x44_u8; 16].as_slice())
        .execute(&pool)
        .await
        .unwrap();
        assert!(matches!(
            store.startup_snapshot_state().await,
            Err(StartupSnapshotLoadError::UnreconciledReload)
        ));
        pool.close().await;
    }

    #[tokio::test]
    async fn canonical_schema_rejects_missing_or_malformed_required_bindings() {
        let (store, pool) = startup_store().await;
        insert_site_head(&pool, &[0x33_u8; 32], 1).await;
        insert_published_publication(&pool, "activating").await;

        assert!(
            sqlx::query("UPDATE canonical_publications SET content_tree_digest = NULL")
                .execute(&pool)
                .await
                .is_err()
        );

        assert!(
            sqlx::query("UPDATE canonical_publications SET content_tree_digest = ?")
                .bind([0x77_u8; 31].as_slice())
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("UPDATE canonical_publications SET accepted_preview_digest = NULL")
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("UPDATE canonical_publications SET accepted_preview_digest = ?")
                .bind([0x88_u8; 33].as_slice())
                .execute(&pool)
                .await
                .is_err()
        );
        assert_eq!(
            store.startup_snapshot_state().await.unwrap().activating[0]
                .accepted_preview_digest
                .as_bytes(),
            &[0x88; 32]
        );

        pool.execute("PRAGMA ignore_check_constraints = ON")
            .await
            .unwrap();
        sqlx::query("UPDATE canonical_publications SET accepted_preview_digest = ?")
            .bind([0x88_u8; 31].as_slice())
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            store.startup_snapshot_state().await,
            Err(StartupSnapshotLoadError::InvalidAcceptedPreviewDigest)
        ));
        sqlx::query("UPDATE canonical_publications SET accepted_preview_digest = ?")
            .bind([0x88_u8; 32].as_slice())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE canonical_publications SET content_tree_digest = ?")
            .bind([0x77_u8; 33].as_slice())
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            store.startup_snapshot_state().await,
            Err(StartupSnapshotLoadError::InvalidContentTreeDigest)
        ));
        pool.close().await;
    }

    #[tokio::test]
    async fn startup_snapshot_state_rejects_a_corrupt_request_fingerprint() {
        let (store, pool) = startup_store().await;
        insert_site_head(&pool, &[0x33_u8; 32], 1).await;
        insert_published_publication(&pool, "activating").await;

        sqlx::query("UPDATE canonical_publications SET requested_revision_digest = ?")
            .bind([0x22_u8; 32].as_slice())
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            store.startup_snapshot_state().await,
            Err(StartupSnapshotLoadError::MismatchedRequestedRevision)
        ));

        sqlx::query(
            "UPDATE canonical_publications \
             SET creation_key = NULL, requested_revision_digest = ?",
        )
        .bind([0x11_u8; 32].as_slice())
        .execute(&pool)
        .await
        .unwrap();
        assert!(matches!(
            store.startup_snapshot_state().await,
            Err(StartupSnapshotLoadError::OrphanedRequestedRevision)
        ));
        pool.close().await;
    }

    #[tokio::test]
    async fn startup_snapshot_state_rejects_ambiguous_or_incomplete_activation() {
        let (store, pool) = startup_store().await;
        insert_site_head(&pool, &[0x33_u8; 32], 1).await;
        insert_published_publication(&pool, "activating").await;
        sqlx::query("UPDATE canonical_publications SET activation_site_digest = NULL")
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            store.startup_snapshot_state().await,
            Err(StartupSnapshotLoadError::MissingActivationSiteDigest)
        ));

        sqlx::query("UPDATE canonical_publications SET activation_site_digest = ?")
            .bind([0x66_u8; 32].as_slice())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO canonical_publications (\
                publication_id, command_kind, stable_post_id, pinned_post_digest, state, version, \
                scheduled_at_ns, activation_at_ns, activation_site_digest, content_tree_digest, \
                accepted_preview_digest\
             ) SELECT ?, command_kind, stable_post_id, pinned_post_digest, state, version, \
                      scheduled_at_ns, activation_at_ns, activation_site_digest, \
                      content_tree_digest, accepted_preview_digest \
               FROM canonical_publications LIMIT 1",
        )
        .bind(
            Uuid::parse_str("88888888-8888-4888-8888-888888888888")
                .unwrap()
                .as_bytes()
                .as_slice(),
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(matches!(
            store.startup_snapshot_state().await,
            Err(StartupSnapshotLoadError::MultipleActivations)
        ));
        pool.close().await;
    }

    #[tokio::test]
    async fn startup_snapshot_state_rejects_an_invalid_site_digest() {
        let (store, pool) = startup_store().await;
        insert_site_head(&pool, &[0x55_u8; 31], 1).await;

        assert!(matches!(
            store.startup_snapshot_state().await,
            Err(StartupSnapshotLoadError::InvalidSiteDigest)
        ));
        pool.close().await;
    }

    #[test]
    fn startup_row_decoders_fail_closed() {
        for (stored, expected) in [
            ("scheduled", CanonicalState::Scheduled),
            ("activating", CanonicalState::Activating),
            ("blocked", CanonicalState::Blocked),
            ("published", CanonicalState::Published),
            ("cancelled", CanonicalState::Cancelled),
        ] {
            assert_eq!(canonical_state(stored).unwrap(), expected);
        }
        assert!(matches!(
            canonical_state("unknown"),
            Err(StartupSnapshotLoadError::InvalidCanonicalState)
        ));
        assert!(require_publishable(Some("publishable"), true).is_ok());
        assert!(matches!(
            require_publishable(Some("draft"), true),
            Err(StartupSnapshotLoadError::DraftPinnedRevision)
        ));
        assert!(matches!(
            require_publishable(Some("draft"), false),
            Err(StartupSnapshotLoadError::DraftCurrentRevision)
        ));
        assert!(matches!(
            require_publishable(Some("other"), false),
            Err(StartupSnapshotLoadError::InvalidPublicationStatus)
        ));
        assert!(matches!(
            require_publishable(None, true),
            Err(StartupSnapshotLoadError::MissingPinnedRevision)
        ));
        assert!(matches!(
            require_publishable(None, false),
            Err(StartupSnapshotLoadError::MissingCurrentRevision)
        ));
        assert!(validate_reload_states(&["applied".into(), "failed".into()]).is_ok());
        assert!(matches!(
            validate_reload_states(&["unknown".into()]),
            Err(StartupSnapshotLoadError::InvalidReloadState)
        ));
        assert!(decode_source_commit(vec![0xaa; 20]).is_some());
        assert!(decode_source_commit(vec![0xbb; 32]).is_some());
        assert!(decode_source_commit(vec![0xcc; 19]).is_none());

        let current = SiteHead {
            digest: SiteSnapshotDigest::from_bytes([0xdd; 32]),
            version: 2,
        };
        assert!(retained_revision_precedes_current(Some(&current), 1));
        assert!(!retained_revision_precedes_current(Some(&current), 2));
        assert!(!retained_revision_precedes_current(Some(&current), 3));
        assert!(!retained_revision_precedes_current(None, 1));
    }

    fn uuid_bytes(value: &str) -> Vec<u8> {
        Uuid::parse_str(value).unwrap().as_bytes().to_vec()
    }

    fn valid_row() -> TargetJobRow {
        let payload = TargetPayload::new("copy").unwrap();
        TargetJobRow {
            publication_job_id: uuid_bytes(JOB_ID),
            publication_id: uuid_bytes(PUBLICATION_ID),
            state: "waiting_for_canonical".into(),
            target: "x".into(),
            stable_post_id: uuid_bytes(POST_ID),
            pinned_post_digest: PostRevisionDigest::parse(REVISION)
                .unwrap()
                .as_bytes()
                .to_vec(),
            scheduled_at_ns: 1_234_567_890,
            payload_version: i64::from(payload.version()),
            payload_body: payload.body().into(),
            payload_digest: payload.digest().as_bytes().to_vec(),
            version: 1,
        }
    }

    #[tokio::test]
    async fn loads_and_validates_a_persisted_target_job() {
        let mut connection = SqliteConnection::connect("sqlite::memory:").await.unwrap();
        connection
            .execute(
                "CREATE TABLE canonical_publications (\
                    publication_id BLOB PRIMARY KEY, stable_post_id BLOB NOT NULL, \
                    pinned_post_digest BLOB NOT NULL\
                ) STRICT",
            )
            .await
            .unwrap();
        connection
            .execute(
                "CREATE TABLE publication_jobs (\
                    publication_job_id BLOB PRIMARY KEY, publication_id BLOB NOT NULL, \
                    state TEXT NOT NULL, target TEXT NOT NULL, scheduled_at_ns INTEGER NOT NULL, \
                    payload_version INTEGER NOT NULL, payload_body TEXT NOT NULL, \
                    payload_digest BLOB NOT NULL, version INTEGER NOT NULL\
                ) STRICT",
            )
            .await
            .unwrap();
        let row = valid_row();
        sqlx::query(
            "INSERT INTO canonical_publications (\
                publication_id, stable_post_id, pinned_post_digest\
             ) VALUES (?, ?, ?)",
        )
        .bind(&row.publication_id)
        .bind(&row.stable_post_id)
        .bind(&row.pinned_post_digest)
        .execute(&mut connection)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO publication_jobs (\
                publication_job_id, publication_id, state, target, scheduled_at_ns, \
                payload_version, payload_body, payload_digest, version\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.publication_job_id)
        .bind(&row.publication_id)
        .bind(&row.state)
        .bind(&row.target)
        .bind(row.scheduled_at_ns)
        .bind(row.payload_version)
        .bind(&row.payload_body)
        .bind(&row.payload_digest)
        .bind(row.version)
        .execute(&mut connection)
        .await
        .unwrap();

        let stored = load_by_id(&mut connection, Uuid::parse_str(JOB_ID).unwrap())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(stored.publication_job_id.to_string(), JOB_ID);
        assert_eq!(stored.publication_id.to_string(), PUBLICATION_ID);
        let TargetJobStatus::WaitingForCanonical(job) = stored.status else {
            panic!("expected a waiting target job");
        };
        let view = job.into_view();
        assert_eq!(view.state, TargetJobState::WaitingForCanonical);
        assert_eq!(view.scheduled_at.unix_timestamp_nanos(), 1_234_567_890);
        assert_eq!(view.payload.body(), "copy");

        connection.close().await.unwrap();
    }

    #[test]
    fn rejects_a_payload_body_that_does_not_match_its_digest() {
        let mut row = valid_row();
        row.payload_body = "different copy".into();

        assert!(matches!(
            StoredTargetJob::try_from(row),
            Err(TargetJobLoadError::InvalidDomainState(
                RehydrationError::TargetPayloadDigestMismatch
            ))
        ));
    }

    #[test]
    fn rejects_an_unsupported_payload_version() {
        let mut row = valid_row();
        row.payload_version = i64::from(CURRENT_PAYLOAD_VERSION) + 1;

        assert!(matches!(
            StoredTargetJob::try_from(row),
            Err(TargetJobLoadError::InvalidPayload(
                PayloadError::UnsupportedVersion { .. }
            ))
        ));
    }

    #[test]
    fn rejects_a_state_and_version_mismatch() {
        let mut row = valid_row();
        row.state = "running".into();

        assert!(matches!(
            StoredTargetJob::try_from(row),
            Err(TargetJobLoadError::InvalidDomainState(
                RehydrationError::TargetVersion {
                    state: TargetJobState::Running,
                    version: 1,
                    minimum: 3,
                }
            ))
        ));
    }

    #[test]
    fn rejects_an_unknown_persisted_state() {
        let mut row = valid_row();
        row.state = "paused".into();

        assert!(matches!(
            StoredTargetJob::try_from(row),
            Err(TargetJobLoadError::InvalidState)
        ));
    }

    #[test]
    fn decodes_every_persisted_state_name() {
        let states = [
            ("waiting_for_canonical", TargetJobState::WaitingForCanonical),
            ("scheduled", TargetJobState::Scheduled),
            ("ready", TargetJobState::Ready),
            ("running", TargetJobState::Running),
            ("succeeded", TargetJobState::Succeeded),
            ("failed", TargetJobState::Failed),
            ("outcome_unknown", TargetJobState::OutcomeUnknown),
            ("cancelled", TargetJobState::Cancelled),
        ];

        for (stored, expected) in states {
            assert_eq!(parse_state(stored).unwrap(), expected);
        }
    }

    #[test]
    fn rejects_numeric_values_before_casting() {
        let mut payload_version = valid_row();
        payload_version.payload_version = i64::from(u16::MAX) + 1;
        assert!(matches!(
            StoredTargetJob::try_from(payload_version),
            Err(TargetJobLoadError::PayloadVersionOutOfRange { .. })
        ));

        let mut version = valid_row();
        version.version = -1;
        assert!(matches!(
            StoredTargetJob::try_from(version),
            Err(TargetJobLoadError::VersionOutOfRange { value: -1 })
        ));
    }

    #[test]
    fn rejects_uuid_blobs_with_the_wrong_length() {
        let mut job_id = valid_row();
        job_id.publication_job_id.pop();
        assert!(matches!(
            StoredTargetJob::try_from(job_id),
            Err(TargetJobLoadError::InvalidPublicationJobId)
        ));

        let mut publication_id = valid_row();
        publication_id.publication_id.pop();
        assert!(matches!(
            StoredTargetJob::try_from(publication_id),
            Err(TargetJobLoadError::InvalidPublicationId)
        ));

        let mut post_id = valid_row();
        post_id.stable_post_id.pop();
        assert!(matches!(
            StoredTargetJob::try_from(post_id),
            Err(TargetJobLoadError::InvalidPostId)
        ));
    }

    #[test]
    fn rejects_digest_blobs_with_the_wrong_length() {
        let mut revision = valid_row();
        revision.pinned_post_digest.pop();
        assert!(matches!(
            StoredTargetJob::try_from(revision),
            Err(TargetJobLoadError::InvalidPinnedPostDigest)
        ));

        let mut payload = valid_row();
        payload.payload_digest.push(0);
        assert!(matches!(
            StoredTargetJob::try_from(payload),
            Err(TargetJobLoadError::InvalidPayloadDigest)
        ));
    }

    #[test]
    fn timestamp_conversion_is_checked() {
        assert!(matches!(
            timestamp_from_nanos(i128::MAX),
            Err(TargetJobLoadError::TimestampOutOfRange {
                nanoseconds: i128::MAX
            })
        ));
    }
}
