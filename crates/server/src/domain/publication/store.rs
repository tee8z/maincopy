use sqlx::{Executor, FromRow, Sqlite, Transaction, error::ErrorKind};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::{
    content::{
        DraftStatus, PostId, PostRevisionDigest, PostSlug, PublishedPostRevision,
        SiteSnapshotDigest, SourceCommit,
    },
    database::store::{
        DatabaseAdmissionError, DatabaseCommandError, DatabaseMutationError, Mutation,
    },
    domain::distribution::{DistributionTarget, PayloadError, TargetPayload, TargetPayloadDigest},
    render::PublicLedgerProjection,
};

use super::{
    ActivationBlockReason, CanonicalPublicationStatus, CanonicalPublicationView, CanonicalState,
    RehydrationError, TargetJob, TargetJobState, TargetJobStatus, TargetJobView, target,
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
        for row in rows {
            match decode_canonical_publication(row)? {
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
                CanonicalPublicationStatus::Activating(_) => {
                    return Err(StartupSnapshotLoadError::UnreconciledActivation);
                }
                CanonicalPublicationStatus::Scheduled(_)
                | CanonicalPublicationStatus::Blocked(_)
                | CanonicalPublicationStatus::Cancelled(_) => {}
            }
        }
        let ledger = PublicLedgerProjection::try_from_exact_entries(published)
            .map_err(|_| StartupSnapshotLoadError::DuplicatePublishedPost)?;
        transaction.commit().await?;
        Ok(StartupSnapshotState { site, ledger })
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
    pinned.publication_status AS pinned_publication_status, \
    current.publication_status AS current_publication_status \
    FROM canonical_publications AS canonical \
    LEFT JOIN post_revisions AS pinned \
      ON pinned.stable_post_id = canonical.stable_post_id \
     AND pinned.revision_digest = canonical.pinned_post_digest \
    LEFT JOIN post_revisions AS current \
      ON current.stable_post_id = canonical.stable_post_id \
     AND current.revision_digest = canonical.current_published_digest \
    ORDER BY canonical.stable_post_id, canonical.publication_id";

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
    pinned_publication_status: Option<String>,
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

fn decode_canonical_publication(
    row: CanonicalPublicationRow,
) -> Result<CanonicalPublicationStatus, StartupSnapshotLoadError> {
    Uuid::from_slice(&row.publication_id)
        .map_err(|_| StartupSnapshotLoadError::InvalidPublicationId)?;
    let stable_post_id = decode_post_id(row.stable_post_id)?;
    let pinned_post_digest = post_digest(row.pinned_post_digest)?;
    require_publishable(row.pinned_publication_status.as_deref(), true)?;

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
    CanonicalPublicationStatus::try_from(view)
        .map_err(StartupSnapshotLoadError::InvalidCanonicalPublication)
}

fn canonical_state(value: &str) -> Result<CanonicalState, StartupSnapshotLoadError> {
    match value {
        "scheduled" => Ok(CanonicalState::Scheduled),
        "activating" => Ok(CanonicalState::Activating),
        "blocked" => Ok(CanonicalState::Blocked),
        "published" => Ok(CanonicalState::Published),
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
    #[error("an activating publication must be reconciled before snapshot construction")]
    UnreconciledActivation,
    #[error("a stored publication ID is invalid")]
    InvalidPublicationId,
    #[error("a stored post ID is invalid")]
    InvalidPostId,
    #[error("a stored post digest is invalid")]
    InvalidPostDigest,
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

/// Distinguishes a retryable database mutation from domain and resource IDs.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CommandIdempotencyKey(Uuid);

impl CommandIdempotencyKey {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the WP4.1 admin mutation will create this WP3.2 key"
        )
    )]
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
    let stored_version = i64::try_from(version)
        .map_err(|_| StartupSnapshotMutationError::Command(DatabaseCommandError::InvalidValue))?;
    let retained: Option<(i64, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT version, source_commit FROM site_revisions WHERE site_revision_digest = ?",
    )
    .bind(command.candidate_digest.as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(StartupSnapshotMutationError::Operation)?;
    if let Some((retained_version, source_commit)) = retained {
        let retained_version = u64::try_from(retained_version)
            .ok()
            .filter(|version| *version > 0)
            .ok_or(StartupSnapshotMutationError::CorruptStoredState)?;
        if current.is_none_or(|head| retained_version > head.version)
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
        .bind(command.candidate_digest.as_bytes().as_slice())
        .bind(stored_version)
        .bind(activated_at_ns)
        .bind(command.source_commit.as_ref().map(SourceCommit::as_bytes))
        .execute(&mut **transaction)
        .await
        .map_err(StartupSnapshotMutationError::Operation)?;
    }
    Ok(())
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
                publication_id BLOB PRIMARY KEY, stable_post_id BLOB NOT NULL, \
                pinned_post_digest BLOB NOT NULL, state TEXT NOT NULL, version INTEGER NOT NULL, \
                scheduled_at_ns INTEGER NOT NULL, activation_at_ns INTEGER, \
                published_at_ns INTEGER, current_published_digest BLOB, source_commit BLOB, \
                block_reason TEXT\
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
        sqlx::query(
            "INSERT INTO canonical_publications (\
                publication_id, stable_post_id, pinned_post_digest, state, version, \
                scheduled_at_ns, activation_at_ns, published_at_ns, current_published_digest\
             ) VALUES (?, ?, ?, ?, ?, 100, ?, ?, ?)",
        )
        .bind(uuid_bytes(PUBLICATION_ID))
        .bind(post_id)
        .bind(revision.as_slice())
        .bind(state)
        .bind(version)
        .bind(activation_at)
        .bind(published_at)
        .bind(current_digest)
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
        pool.close().await;
    }

    #[tokio::test]
    async fn startup_snapshot_state_rehydrates_the_published_projection() {
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
    async fn startup_snapshot_state_rejects_unreconciled_rows() {
        let (store, pool) = startup_store().await;
        insert_site_head(&pool, &[0x33_u8; 32], 1).await;
        insert_published_publication(&pool, "activating").await;
        assert!(matches!(
            store.startup_snapshot_state().await,
            Err(StartupSnapshotLoadError::UnreconciledActivation)
        ));
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
