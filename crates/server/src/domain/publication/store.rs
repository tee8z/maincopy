use sqlx::{Executor, FromRow, Sqlite, Transaction, error::ErrorKind};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::{
    content::{PostId, PostRevisionDigest},
    database::store::{
        DatabaseAdmissionError, DatabaseCommandError, DatabaseMutationError, Mutation,
    },
    domain::distribution::{DistributionTarget, PayloadError, TargetPayload, TargetPayloadDigest},
};

use super::{RehydrationError, TargetJob, TargetJobState, TargetJobStatus, TargetJobView, target};

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
    use sqlx::{Connection as _, Executor as _, SqliteConnection};

    use super::*;
    use crate::domain::distribution::CURRENT_PAYLOAD_VERSION;

    const JOB_ID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    const PUBLICATION_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const REVISION: &str =
        "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111";

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
