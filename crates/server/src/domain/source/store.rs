use std::fmt;

use maincopy_shared::{
    auth::AdminScope,
    source::{
        GitBranchName, ManagedSourceConfiguration, RepositoryContentSubdirectory,
        SourceConfigurationVersion, SourcePollInterval, SourceSyncAdmission, SourceSyncFailureCode,
        SourceSyncId, SourceSyncOutcome, SourceSyncRequestOrigin, SourceSyncStage,
        SshCredentialName, SshRemote, SshRemoteHost, SshRemotePort, SshRemoteUser,
        SshRepositoryPath,
    },
};
use markdown_compiler::ContentTreeDigest;
use sqlx::{FromRow, Sqlite, Transaction};
use thiserror::Error;
use time::{Duration, OffsetDateTime, UtcOffset};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::{
    database::store::{
        DatabaseAdmissionError, DatabaseCommandError, DatabaseMutationError, Mutation,
    },
    domain::{
        auth::store::{
            AdminMutationKey, AuditPrincipalReference, AuthApplyError, AuthCommandError,
            MutationAuditContext, append_success_audit, require_principal_scope,
        },
        publication::{
            SourceCommit,
            store::{
                IndexContentCatalog, ObservedPostRevision, StartupSnapshotMutationError,
                index_content_catalog,
            },
        },
    },
};

use super::ManagedSourceConfigurationInput;

const MAX_SOURCE_SYNC_PAGE_SIZE: usize = 100;
const MAX_RETAINED_TERMINAL_SOURCE_SYNCS: u32 = 4_096;
const MAX_RETAINED_SOURCE_SYNC_ALIASES: u32 = 4_096;
const PUT_CONFIGURATION_ACTION: &str = "source.configuration.put";
const MANUAL_SYNC_ACTION: &str = "source.sync.request";

/// Managed-source queries and mutations backed by the private database.
#[derive(Clone)]
pub(crate) struct SourceStore {
    readers: sqlx::SqlitePool,
    mutations: mpsc::Sender<Mutation>,
}

impl SourceStore {
    pub(crate) const fn new(readers: sqlx::SqlitePool, mutations: mpsc::Sender<Mutation>) -> Self {
        Self { readers, mutations }
    }

    pub(crate) async fn configuration(
        &self,
    ) -> Result<Option<StoredSourceConfiguration>, SourceLoadError> {
        let row = sqlx::query_as::<_, SourceConfigurationRow>(
            "SELECT ssh_user, ssh_host, ssh_port, repository_path, branch, \
                    content_subdirectory, credential_name, poll_interval_seconds, version, \
                    updated_at_ns, next_poll_at_ns \
             FROM source_configuration WHERE singleton = 1",
        )
        .fetch_optional(&self.readers)
        .await?;
        row.map(decode_configuration).transpose()
    }

    pub(crate) async fn installation(&self) -> Result<Option<InstalledSource>, SourceLoadError> {
        let row = sqlx::query_as::<_, SourceInstallationRow>(
            "SELECT configuration_version, source_commit, content_digest, source_sync_id, \
                    installed_at_ns \
             FROM source_installation WHERE singleton = 1",
        )
        .fetch_optional(&self.readers)
        .await?;
        row.map(decode_installation).transpose()
    }

    pub(crate) async fn sync(
        &self,
        source_sync_id: SourceSyncId,
    ) -> Result<Option<StoredSourceSync>, SourceLoadError> {
        let row = sqlx::query_as::<_, SourceSyncRow>(SOURCE_SYNC_COLUMNS_BY_ID)
            .bind(source_sync_id.as_uuid().as_bytes().as_slice())
            .fetch_optional(&self.readers)
            .await?;
        row.map(decode_sync).transpose()
    }

    pub(crate) async fn active_sync(&self) -> Result<Option<StoredSourceSync>, SourceLoadError> {
        let row = sqlx::query_as::<_, SourceSyncRow>(SOURCE_SYNC_COLUMNS_ACTIVE)
            .fetch_optional(&self.readers)
            .await?;
        row.map(decode_sync).transpose()
    }

    /// Reads the operator-facing source projection from one SQLite snapshot.
    pub(crate) async fn status(&self) -> Result<StoredSourceStatus, SourceLoadError> {
        let mut transaction = self.readers.begin().await?;
        let configuration = sqlx::query_as::<_, SourceConfigurationRow>(
            "SELECT ssh_user, ssh_host, ssh_port, repository_path, branch, \
                    content_subdirectory, credential_name, poll_interval_seconds, version, \
                    updated_at_ns, next_poll_at_ns \
             FROM source_configuration WHERE singleton = 1",
        )
        .fetch_optional(&mut *transaction)
        .await?
        .map(decode_configuration)
        .transpose()?;
        let installation = sqlx::query_as::<_, SourceInstallationRow>(
            "SELECT configuration_version, source_commit, content_digest, source_sync_id, \
                    installed_at_ns FROM source_installation WHERE singleton = 1",
        )
        .fetch_optional(&mut *transaction)
        .await?
        .map(decode_installation)
        .transpose()?;
        let active_sync = sqlx::query_as::<_, SourceSyncRow>(SOURCE_SYNC_COLUMNS_ACTIVE)
            .fetch_optional(&mut *transaction)
            .await?
            .map(decode_sync)
            .transpose()?;
        let latest_sync = sqlx::query_as::<_, SourceSyncRow>(SOURCE_SYNC_COLUMNS_LATEST)
            .fetch_optional(&mut *transaction)
            .await?
            .map(decode_sync)
            .transpose()?;
        transaction.commit().await?;
        Ok(StoredSourceStatus {
            configuration,
            installation,
            active_sync,
            latest_sync,
        })
    }

    pub(crate) async fn list_syncs(
        &self,
        after: Option<SourceSyncId>,
        limit: usize,
    ) -> Result<SourceSyncPage, SourceLoadError> {
        if !(1..=MAX_SOURCE_SYNC_PAGE_SIZE).contains(&limit) {
            return Err(SourceLoadError::InvalidPageLimit);
        }
        let rows = if let Some(after) = after {
            let cursor: Option<i64> = sqlx::query_scalar(
                "SELECT requested_at_ns FROM source_sync_operations WHERE source_sync_id = ?",
            )
            .bind(after.as_uuid().as_bytes().as_slice())
            .fetch_optional(&self.readers)
            .await?;
            let cursor = cursor.ok_or(SourceLoadError::CursorNotFound)?;
            sqlx::query_as::<_, SourceSyncRow>(SOURCE_SYNC_COLUMNS_PAGE_AFTER)
                .bind(cursor)
                .bind(cursor)
                .bind(after.as_uuid().as_bytes().as_slice())
                .bind(i64::try_from(limit + 1).expect("bounded page size fits SQLite"))
                .fetch_all(&self.readers)
                .await?
        } else {
            sqlx::query_as::<_, SourceSyncRow>(SOURCE_SYNC_COLUMNS_PAGE_FIRST)
                .bind(i64::try_from(limit + 1).expect("bounded page size fits SQLite"))
                .fetch_all(&self.readers)
                .await?
        };
        let mut syncs = rows
            .into_iter()
            .map(decode_sync)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = (syncs.len() > limit).then(|| syncs[limit - 1].source_sync_id);
        syncs.truncate(limit);
        Ok(SourceSyncPage { syncs, next_cursor })
    }

    pub(crate) async fn put_configuration(
        &self,
        command: PutSourceConfiguration,
    ) -> Result<StoredSourceConfiguration, DatabaseMutationError> {
        self.send(|respond_to| Mutation::PutSourceConfiguration {
            command,
            respond_to,
        })
        .await
    }

    pub(crate) async fn begin_sync(
        &self,
        command: BeginSourceSync,
    ) -> Result<BeginSourceSyncResult, DatabaseMutationError> {
        self.send(|respond_to| Mutation::BeginSourceSync {
            command,
            respond_to,
        })
        .await
    }

    pub(crate) async fn advance_sync(
        &self,
        command: AdvanceSourceSync,
    ) -> Result<StoredSourceSync, DatabaseMutationError> {
        self.send(|respond_to| Mutation::AdvanceSourceSync {
            command,
            respond_to,
        })
        .await
    }

    /// Atomically indexes the exact preview catalog, installs its source head,
    /// and terminalizes the owning source synchronization as applied.
    pub(crate) async fn apply_catalog(
        &self,
        command: ApplyManagedSourceCatalog,
    ) -> Result<StoredSourceSync, DatabaseMutationError> {
        self.send(|respond_to| Mutation::ApplyManagedSourceCatalog {
            command,
            respond_to,
        })
        .await
    }

    pub(crate) async fn finish_sync(
        &self,
        command: FinishSourceSync,
    ) -> Result<StoredSourceSync, DatabaseMutationError> {
        self.send(|respond_to| Mutation::FinishSourceSync {
            command,
            respond_to,
        })
        .await
    }

    pub(crate) async fn fail_interrupted_sync(
        &self,
        source_sync_id: SourceSyncId,
        expected_version: u64,
        completed_at: OffsetDateTime,
    ) -> Result<StoredSourceSync, DatabaseMutationError> {
        self.finish_sync(FinishSourceSync {
            source_sync_id,
            expected_version,
            completion: SourceSyncCompletion::Failed {
                code: SourceSyncFailureCode::Interrupted,
            },
            completed_at,
        })
        .await
    }

    async fn send<Output>(
        &self,
        mutation: impl FnOnce(oneshot::Sender<Result<Output, DatabaseCommandError>>) -> Mutation,
    ) -> Result<Output, DatabaseMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.mutations
            .try_send(mutation(respond_to))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => DatabaseAdmissionError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => DatabaseAdmissionError::WriterClosed,
            })?;
        response
            .await
            .map_err(|_| DatabaseMutationError::Command(DatabaseCommandError::OutcomeUnknown))?
            .map_err(DatabaseMutationError::Command)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredSourceConfiguration {
    pub(crate) configuration: ManagedSourceConfiguration,
    pub(crate) next_poll_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InstalledSource {
    pub(crate) configuration_version: SourceConfigurationVersion,
    pub(crate) source_commit: SourceCommit,
    pub(crate) content_digest: ContentTreeDigest,
    pub(crate) source_sync_id: SourceSyncId,
    pub(crate) installed_at: OffsetDateTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredSourceSync {
    pub(crate) source_sync_id: SourceSyncId,
    pub(crate) configuration_version: SourceConfigurationVersion,
    pub(crate) request_origin: SourceSyncRequestOrigin,
    pub(crate) stage: SourceSyncStage,
    pub(crate) outcome: Option<SourceSyncOutcome>,
    pub(crate) source_commit: Option<SourceCommit>,
    pub(crate) content_digest: Option<ContentTreeDigest>,
    pub(crate) failure_code: Option<SourceSyncFailureCode>,
    pub(crate) version: u64,
    pub(crate) requested_at: OffsetDateTime,
    pub(crate) updated_at: OffsetDateTime,
    pub(crate) finished_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceSyncPage {
    pub(crate) syncs: Vec<StoredSourceSync>,
    pub(crate) next_cursor: Option<SourceSyncId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredSourceStatus {
    pub(crate) configuration: Option<StoredSourceConfiguration>,
    pub(crate) installation: Option<InstalledSource>,
    pub(crate) active_sync: Option<StoredSourceSync>,
    pub(crate) latest_sync: Option<StoredSourceSync>,
}

pub(crate) struct PutSourceConfiguration {
    pub(crate) request: ManagedSourceConfigurationInput,
    pub(crate) occurred_at: OffsetDateTime,
    pub(crate) audit: MutationAuditContext,
}

pub(crate) enum SourceSyncRequest {
    Startup,
    Poll,
    Manual { audit: MutationAuditContext },
}

pub(crate) struct BeginSourceSync {
    pub(crate) proposed_source_sync_id: SourceSyncId,
    pub(crate) expected_configuration_version: SourceConfigurationVersion,
    pub(crate) requested_at: OffsetDateTime,
    pub(crate) request: SourceSyncRequest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BeginSourceSyncResult {
    pub(crate) admission: SourceSyncAdmission,
    pub(crate) sync: StoredSourceSync,
}

pub(crate) enum SourceSyncProgress {
    Fetching,
    ResolvingCommit,
    PreparingCandidate {
        source_commit: SourceCommit,
    },
    Compiling {
        source_commit: SourceCommit,
    },
    Reloading {
        source_commit: SourceCommit,
        content_digest: ContentTreeDigest,
    },
}

pub(crate) struct AdvanceSourceSync {
    pub(crate) source_sync_id: SourceSyncId,
    pub(crate) expected_version: u64,
    pub(crate) progress: SourceSyncProgress,
    pub(crate) updated_at: OffsetDateTime,
}

pub(crate) struct ApplyManagedSourceCatalog {
    pub(crate) source_sync_id: SourceSyncId,
    pub(crate) expected_sync_version: u64,
    pub(crate) source_commit: SourceCommit,
    pub(crate) content_digest: ContentTreeDigest,
    pub(crate) observed_posts: Vec<ObservedPostRevision>,
    pub(crate) completed_at: OffsetDateTime,
}

pub(crate) enum SourceSyncCompletion {
    NoChange { source_commit: SourceCommit },
    Failed { code: SourceSyncFailureCode },
    Cancelled,
}

pub(crate) struct FinishSourceSync {
    pub(crate) source_sync_id: SourceSyncId,
    pub(crate) expected_version: u64,
    pub(crate) completion: SourceSyncCompletion,
    pub(crate) completed_at: OffsetDateTime,
}

#[derive(Debug, Error)]
pub(crate) enum SourceLoadError {
    #[error("managed-source persistence query failed")]
    Query(#[from] sqlx::Error),
    #[error("stored managed-source state is invalid: {field}")]
    Corrupt { field: &'static str },
    #[error("source synchronization page limit must be between 1 and 100")]
    InvalidPageLimit,
    #[error("source synchronization cursor does not exist")]
    CursorNotFound,
}

pub(crate) enum SourceApplyError {
    Command(DatabaseCommandError),
    Operation(sqlx::Error),
    CorruptStoredState,
}

impl From<sqlx::Error> for SourceApplyError {
    fn from(source: sqlx::Error) -> Self {
        Self::Operation(source)
    }
}

impl From<DatabaseCommandError> for SourceApplyError {
    fn from(error: DatabaseCommandError) -> Self {
        Self::Command(error)
    }
}

const SOURCE_SYNC_COLUMNS_BY_ID: &str = "SELECT source_sync_id, configuration_version, request_origin, stage, outcome, \
            source_commit, content_digest, failure_code, version, requested_at_ns, \
            updated_at_ns, finished_at_ns \
     FROM source_sync_operations WHERE source_sync_id = ?";
const SOURCE_SYNC_COLUMNS_ACTIVE: &str = "SELECT source_sync_id, configuration_version, request_origin, stage, outcome, \
            source_commit, content_digest, failure_code, version, requested_at_ns, \
            updated_at_ns, finished_at_ns \
     FROM source_sync_operations WHERE outcome IS NULL";
const SOURCE_SYNC_COLUMNS_LATEST: &str = "SELECT source_sync_id, configuration_version, request_origin, stage, outcome, \
            source_commit, content_digest, failure_code, version, requested_at_ns, \
            updated_at_ns, finished_at_ns \
     FROM source_sync_operations \
     ORDER BY requested_at_ns DESC, source_sync_id DESC LIMIT 1";
const SOURCE_SYNC_COLUMNS_PAGE_FIRST: &str = "SELECT source_sync_id, configuration_version, request_origin, stage, outcome, \
            source_commit, content_digest, failure_code, version, requested_at_ns, \
            updated_at_ns, finished_at_ns \
     FROM source_sync_operations \
     ORDER BY requested_at_ns DESC, source_sync_id DESC LIMIT ?";
const SOURCE_SYNC_COLUMNS_PAGE_AFTER: &str = "SELECT source_sync_id, configuration_version, request_origin, stage, outcome, \
            source_commit, content_digest, failure_code, version, requested_at_ns, \
            updated_at_ns, finished_at_ns \
     FROM source_sync_operations \
     WHERE requested_at_ns < ? OR (requested_at_ns = ? AND source_sync_id < ?) \
     ORDER BY requested_at_ns DESC, source_sync_id DESC LIMIT ?";

#[derive(FromRow)]
struct SourceConfigurationRow {
    ssh_user: String,
    ssh_host: String,
    ssh_port: i64,
    repository_path: String,
    branch: String,
    content_subdirectory: String,
    credential_name: String,
    poll_interval_seconds: i64,
    version: i64,
    updated_at_ns: i64,
    next_poll_at_ns: Option<i64>,
}

#[derive(FromRow)]
struct SourceInstallationRow {
    configuration_version: i64,
    source_commit: Vec<u8>,
    content_digest: Vec<u8>,
    source_sync_id: Vec<u8>,
    installed_at_ns: i64,
}

#[derive(FromRow)]
struct SourceSyncRow {
    source_sync_id: Vec<u8>,
    configuration_version: i64,
    request_origin: String,
    stage: String,
    outcome: Option<String>,
    source_commit: Option<Vec<u8>>,
    content_digest: Option<Vec<u8>>,
    failure_code: Option<String>,
    version: i64,
    requested_at_ns: i64,
    updated_at_ns: i64,
    finished_at_ns: Option<i64>,
}

#[derive(FromRow)]
struct MutationReceiptRow {
    command_fingerprint: Vec<u8>,
    result_version: Option<i64>,
    source_sync_id: Option<Vec<u8>>,
    principal_kind: String,
    actor_user_id: Option<Vec<u8>>,
    session_id: Option<Vec<u8>>,
    agent_credential_id: Option<Vec<u8>>,
    action: String,
}

fn decode_configuration(
    row: SourceConfigurationRow,
) -> Result<StoredSourceConfiguration, SourceLoadError> {
    let port = u16::try_from(row.ssh_port)
        .ok()
        .and_then(SshRemotePort::new)
        .ok_or(SourceLoadError::Corrupt { field: "SSH port" })?;
    let poll_interval = u64::try_from(row.poll_interval_seconds)
        .ok()
        .and_then(SourcePollInterval::from_seconds)
        .ok_or(SourceLoadError::Corrupt {
            field: "poll interval",
        })?;
    let version = configuration_version(row.version)?;
    let updated_at = source_timestamp(row.updated_at_ns, "configuration timestamp")?;
    let next_poll_at = row
        .next_poll_at_ns
        .map(|value| source_timestamp(value, "next poll timestamp"))
        .transpose()?;
    Ok(StoredSourceConfiguration {
        configuration: ManagedSourceConfiguration {
            remote: SshRemote {
                user: SshRemoteUser::parse(&row.ssh_user)
                    .map_err(|_| SourceLoadError::Corrupt { field: "SSH user" })?,
                host: SshRemoteHost::parse(&row.ssh_host)
                    .map_err(|_| SourceLoadError::Corrupt { field: "SSH host" })?,
                port,
                repository_path: SshRepositoryPath::parse(&row.repository_path).map_err(|_| {
                    SourceLoadError::Corrupt {
                        field: "repository path",
                    }
                })?,
            },
            branch: GitBranchName::parse(&row.branch)
                .map_err(|_| SourceLoadError::Corrupt { field: "branch" })?,
            content_subdirectory: RepositoryContentSubdirectory::parse(&row.content_subdirectory)
                .map_err(|_| SourceLoadError::Corrupt {
                field: "content subdirectory",
            })?,
            credential_name: SshCredentialName::parse(&row.credential_name).map_err(|_| {
                SourceLoadError::Corrupt {
                    field: "credential name",
                }
            })?,
            poll_interval_seconds: poll_interval,
            version,
            updated_at,
        },
        next_poll_at,
    })
}

fn decode_installation(row: SourceInstallationRow) -> Result<InstalledSource, SourceLoadError> {
    Ok(InstalledSource {
        configuration_version: configuration_version(row.configuration_version)?,
        source_commit: decode_source_commit(&row.source_commit).ok_or(
            SourceLoadError::Corrupt {
                field: "installed source commit",
            },
        )?,
        content_digest: decode_content_digest(row.content_digest)?,
        source_sync_id: decode_source_sync_id(&row.source_sync_id)?,
        installed_at: source_timestamp(row.installed_at_ns, "installation timestamp")?,
    })
}

fn decode_sync(row: SourceSyncRow) -> Result<StoredSourceSync, SourceLoadError> {
    let source_sync_id = decode_source_sync_id(&row.source_sync_id)?;
    let configuration_version = configuration_version(row.configuration_version)?;
    let request_origin =
        SourceSyncRequestOrigin::parse(&row.request_origin).ok_or(SourceLoadError::Corrupt {
            field: "sync request origin",
        })?;
    let stage = SourceSyncStage::parse(&row.stage).ok_or(SourceLoadError::Corrupt {
        field: "sync stage",
    })?;
    let outcome = match row.outcome.as_deref() {
        Some(value) => Some(
            SourceSyncOutcome::parse(value).ok_or(SourceLoadError::Corrupt {
                field: "sync outcome",
            })?,
        ),
        None => None,
    };
    let source_commit = match row.source_commit.as_deref() {
        Some(value) => Some(decode_source_commit(value).ok_or(SourceLoadError::Corrupt {
            field: "sync source commit",
        })?),
        None => None,
    };
    let content_digest = row.content_digest.map(decode_content_digest).transpose()?;
    let failure_code = match row.failure_code.as_deref() {
        Some(value) => Some(SourceSyncFailureCode::parse(value).ok_or(
            SourceLoadError::Corrupt {
                field: "sync failure code",
            },
        )?),
        None => None,
    };
    let version = positive_u64(row.version, "sync version")?;
    let requested_at = source_timestamp(row.requested_at_ns, "sync request timestamp")?;
    let updated_at = source_timestamp(row.updated_at_ns, "sync update timestamp")?;
    let finished_at = row
        .finished_at_ns
        .map(|value| source_timestamp(value, "sync finish timestamp"))
        .transpose()?;
    if updated_at < requested_at || finished_at.is_some_and(|value| value < updated_at) {
        return Err(SourceLoadError::Corrupt {
            field: "sync timestamp ordering",
        });
    }
    validate_sync_shape(
        stage,
        outcome,
        source_commit.as_ref(),
        content_digest.as_ref(),
        failure_code,
        finished_at,
    )?;
    Ok(StoredSourceSync {
        source_sync_id,
        configuration_version,
        request_origin,
        stage,
        outcome,
        source_commit,
        content_digest,
        failure_code,
        version,
        requested_at,
        updated_at,
        finished_at,
    })
}

fn validate_sync_shape(
    stage: SourceSyncStage,
    outcome: Option<SourceSyncOutcome>,
    source_commit: Option<&SourceCommit>,
    content_digest: Option<&ContentTreeDigest>,
    failure_code: Option<SourceSyncFailureCode>,
    finished_at: Option<OffsetDateTime>,
) -> Result<(), SourceLoadError> {
    if outcome.is_some() != finished_at.is_some()
        || (outcome == Some(SourceSyncOutcome::Failed)) != failure_code.is_some()
    {
        return Err(SourceLoadError::Corrupt {
            field: "sync terminal state",
        });
    }
    match outcome {
        Some(SourceSyncOutcome::Applied) => {
            if stage != SourceSyncStage::Reloading
                || source_commit.is_none()
                || content_digest.is_none()
            {
                return Err(SourceLoadError::Corrupt {
                    field: "applied sync result",
                });
            }
        }
        Some(SourceSyncOutcome::NoChange) => {
            if stage != SourceSyncStage::ResolvingCommit
                || source_commit.is_none()
                || content_digest.is_none()
            {
                return Err(SourceLoadError::Corrupt {
                    field: "no-change sync result",
                });
            }
        }
        Some(SourceSyncOutcome::Failed | SourceSyncOutcome::Cancelled) => {}
        None => match stage {
            SourceSyncStage::Queued
            | SourceSyncStage::Fetching
            | SourceSyncStage::ResolvingCommit
                if source_commit.is_some() || content_digest.is_some() =>
            {
                return Err(SourceLoadError::Corrupt {
                    field: "early sync provenance",
                });
            }
            SourceSyncStage::PreparingCandidate | SourceSyncStage::Compiling
                if source_commit.is_none() || content_digest.is_some() =>
            {
                return Err(SourceLoadError::Corrupt {
                    field: "candidate sync provenance",
                });
            }
            SourceSyncStage::Reloading if source_commit.is_none() || content_digest.is_none() => {
                return Err(SourceLoadError::Corrupt {
                    field: "reload sync provenance",
                });
            }
            _ => {}
        },
    }
    Ok(())
}

fn decode_source_sync_id(value: &[u8]) -> Result<SourceSyncId, SourceLoadError> {
    let identifier = Uuid::from_slice(value).map_err(|_| SourceLoadError::Corrupt {
        field: "source sync identifier",
    })?;
    Ok(SourceSyncId::from_uuid(identifier))
}

fn decode_source_commit(value: &[u8]) -> Option<SourceCommit> {
    let prefix = match value.len() {
        20 => "git-sha1:",
        32 => "git-sha256:",
        _ => return None,
    };
    let mut encoded = String::with_capacity(prefix.len() + value.len() * 2);
    encoded.push_str(prefix);
    for byte in value {
        use fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    SourceCommit::parse(&encoded).ok()
}

fn decode_content_digest(value: Vec<u8>) -> Result<ContentTreeDigest, SourceLoadError> {
    value
        .try_into()
        .map(ContentTreeDigest::from_bytes)
        .map_err(|_| SourceLoadError::Corrupt {
            field: "content digest",
        })
}

fn configuration_version(value: i64) -> Result<SourceConfigurationVersion, SourceLoadError> {
    u64::try_from(value)
        .ok()
        .and_then(SourceConfigurationVersion::new)
        .ok_or(SourceLoadError::Corrupt {
            field: "configuration version",
        })
}

fn positive_u64(value: i64, field: &'static str) -> Result<u64, SourceLoadError> {
    u64::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or(SourceLoadError::Corrupt { field })
}

fn source_timestamp(value: i64, field: &'static str) -> Result<OffsetDateTime, SourceLoadError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(value))
        .map(|value| value.to_offset(UtcOffset::UTC))
        .map_err(|_| SourceLoadError::Corrupt { field })
}

pub(crate) async fn put_configuration(
    transaction: &mut Transaction<'_, Sqlite>,
    command: PutSourceConfiguration,
) -> Result<StoredSourceConfiguration, SourceApplyError> {
    let fingerprint = configuration_fingerprint(&command.request);
    if let Some(replayed) =
        replay_configuration_mutation(transaction, &command.audit, fingerprint).await?
    {
        return Ok(replayed);
    }
    map_auth_result(
        require_principal_scope(
            transaction,
            &command.audit.principal,
            AdminScope::SourceManage,
            command.occurred_at,
        )
        .await,
    )?;

    let current = load_configuration(transaction).await?;
    if current
        .as_ref()
        .is_some_and(|current| command.occurred_at < current.configuration.updated_at)
    {
        return Err(DatabaseCommandError::Rejected.into());
    }
    let next_version = match (current.as_ref(), command.request.expected_version) {
        (None, None) => SourceConfigurationVersion::new(1)
            .expect("the initial source configuration version is valid"),
        (Some(current), Some(expected)) if current.configuration.version == expected => {
            SourceConfigurationVersion::new(
                expected
                    .get()
                    .checked_add(1)
                    .ok_or(DatabaseCommandError::InvalidValue)?,
            )
            .ok_or(DatabaseCommandError::InvalidValue)?
        }
        _ => return Err(DatabaseCommandError::Rejected.into()),
    };
    let updated_at_ns = command_timestamp(command.occurred_at)?;
    let request = command.request;
    let version_i64 = version_i64(next_version.get())?;
    sqlx::query(
        "INSERT INTO source_configuration_revisions (\
            version, ssh_user, ssh_host, ssh_port, repository_path, branch, \
            content_subdirectory, credential_name, poll_interval_seconds, updated_at_ns\
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(version_i64)
    .bind(request.remote.user.as_str())
    .bind(request.remote.host.as_str())
    .bind(i64::from(request.remote.port.get()))
    .bind(request.remote.repository_path.as_str())
    .bind(request.branch.as_str())
    .bind(request.content_subdirectory.as_str())
    .bind(request.credential_name.as_str())
    .bind(version_i64_from_u64(
        request.poll_interval_seconds.seconds(),
    )?)
    .bind(updated_at_ns)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO source_configuration (\
            singleton, ssh_user, ssh_host, ssh_port, repository_path, branch, \
            content_subdirectory, credential_name, poll_interval_seconds, version, \
            updated_at_ns, next_poll_at_ns\
         ) VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL) \
         ON CONFLICT(singleton) DO UPDATE SET \
            ssh_user = excluded.ssh_user, ssh_host = excluded.ssh_host, \
            ssh_port = excluded.ssh_port, repository_path = excluded.repository_path, \
            branch = excluded.branch, content_subdirectory = excluded.content_subdirectory, \
            credential_name = excluded.credential_name, \
            poll_interval_seconds = excluded.poll_interval_seconds, version = excluded.version, \
            updated_at_ns = excluded.updated_at_ns, next_poll_at_ns = NULL",
    )
    .bind(request.remote.user.as_str())
    .bind(request.remote.host.as_str())
    .bind(i64::from(request.remote.port.get()))
    .bind(request.remote.repository_path.as_str())
    .bind(request.branch.as_str())
    .bind(request.content_subdirectory.as_str())
    .bind(request.credential_name.as_str())
    .bind(version_i64_from_u64(
        request.poll_interval_seconds.seconds(),
    )?)
    .bind(version_i64)
    .bind(updated_at_ns)
    .execute(&mut **transaction)
    .await?;
    map_auth_result(
        append_success_audit(
            transaction,
            &command.audit,
            command.occurred_at,
            PUT_CONFIGURATION_ACTION,
        )
        .await,
    )?;
    sqlx::query(
        "INSERT INTO source_configuration_mutation_receipts (\
            idempotency_key, audit_event_id, command_fingerprint, result_version, completed_at_ns\
         ) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(command.audit.idempotency_key.0.as_bytes().as_slice())
    .bind(command.audit.audit_event_id.as_uuid().as_bytes().as_slice())
    .bind(fingerprint.as_slice())
    .bind(version_i64)
    .bind(updated_at_ns)
    .execute(&mut **transaction)
    .await?;

    Ok(StoredSourceConfiguration {
        configuration: ManagedSourceConfiguration {
            remote: request.remote,
            branch: request.branch,
            content_subdirectory: request.content_subdirectory,
            credential_name: request.credential_name,
            poll_interval_seconds: request.poll_interval_seconds,
            version: next_version,
            updated_at: command.occurred_at.to_offset(UtcOffset::UTC),
        },
        next_poll_at: None,
    })
}

pub(crate) async fn begin_sync(
    transaction: &mut Transaction<'_, Sqlite>,
    command: BeginSourceSync,
) -> Result<BeginSourceSyncResult, SourceApplyError> {
    let (manual_fingerprint, request_origin) =
        match prepare_sync_request(transaction, &command).await? {
            PreparedSyncRequest::Replayed(sync) => {
                return Ok(BeginSourceSyncResult {
                    admission: SourceSyncAdmission::Replayed,
                    sync,
                });
            }
            PreparedSyncRequest::Startup => (None, SourceSyncRequestOrigin::Startup),
            PreparedSyncRequest::Poll => (None, SourceSyncRequestOrigin::Poll),
            PreparedSyncRequest::Manual { audit, fingerprint } => {
                (Some((audit, fingerprint)), SourceSyncRequestOrigin::Manual)
            }
        };

    let configuration = load_configuration(transaction)
        .await?
        .ok_or(DatabaseCommandError::Rejected)?;
    if configuration.configuration.version != command.expected_configuration_version {
        return Err(DatabaseCommandError::Rejected.into());
    }

    let existing = load_active_sync(transaction).await?;
    let (admission, sync) = if let Some(sync) = existing {
        if sync.configuration_version != command.expected_configuration_version {
            return Err(DatabaseCommandError::Rejected.into());
        }
        (SourceSyncAdmission::Coalesced, sync)
    } else {
        let requested_at = command.requested_at.to_offset(UtcOffset::UTC);
        let requested_at_ns = command_timestamp(requested_at)?;
        let identifier_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(\
                SELECT 1 FROM source_sync_operations WHERE source_sync_id = ?\
             )",
        )
        .bind(
            command
                .proposed_source_sync_id
                .as_uuid()
                .as_bytes()
                .as_slice(),
        )
        .fetch_one(&mut **transaction)
        .await?;
        if identifier_exists {
            return Err(DatabaseCommandError::Rejected.into());
        }
        sqlx::query(
            "INSERT INTO source_sync_operations (\
                source_sync_id, configuration_version, request_origin, stage, outcome, \
                source_commit, content_digest, failure_code, version, \
                requested_at_ns, updated_at_ns, finished_at_ns\
             ) VALUES (?, ?, ?, 'queued', NULL, NULL, NULL, NULL, 1, ?, ?, NULL)",
        )
        .bind(
            command
                .proposed_source_sync_id
                .as_uuid()
                .as_bytes()
                .as_slice(),
        )
        .bind(version_i64(command.expected_configuration_version.get())?)
        .bind(request_origin.as_str())
        .bind(requested_at_ns)
        .bind(requested_at_ns)
        .execute(&mut **transaction)
        .await?;
        (
            SourceSyncAdmission::Created,
            StoredSourceSync {
                source_sync_id: command.proposed_source_sync_id,
                configuration_version: command.expected_configuration_version,
                request_origin,
                stage: SourceSyncStage::Queued,
                outcome: None,
                source_commit: None,
                content_digest: None,
                failure_code: None,
                version: 1,
                requested_at,
                updated_at: requested_at,
                finished_at: None,
            },
        )
    };

    if let Some((audit, fingerprint)) = manual_fingerprint {
        map_auth_result(
            append_success_audit(transaction, audit, command.requested_at, MANUAL_SYNC_ACTION)
                .await,
        )?;
        sqlx::query(
            "INSERT INTO source_sync_idempotency_aliases (\
                idempotency_key, audit_event_id, command_fingerprint, source_sync_id, \
                requested_at_ns\
             ) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(audit.idempotency_key.0.as_bytes().as_slice())
        .bind(audit.audit_event_id.as_uuid().as_bytes().as_slice())
        .bind(fingerprint.as_slice())
        .bind(sync.source_sync_id.as_uuid().as_bytes().as_slice())
        .bind(command_timestamp(command.requested_at)?)
        .execute(&mut **transaction)
        .await?;
        prune_source_sync_aliases(transaction, MAX_RETAINED_SOURCE_SYNC_ALIASES).await?;
    }
    Ok(BeginSourceSyncResult { admission, sync })
}

enum PreparedSyncRequest<'a> {
    Replayed(StoredSourceSync),
    Startup,
    Poll,
    Manual {
        audit: &'a MutationAuditContext,
        fingerprint: [u8; 32],
    },
}

async fn prepare_sync_request<'a>(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &'a BeginSourceSync,
) -> Result<PreparedSyncRequest<'a>, SourceApplyError> {
    let audit = match &command.request {
        SourceSyncRequest::Startup => return Ok(PreparedSyncRequest::Startup),
        SourceSyncRequest::Poll => return Ok(PreparedSyncRequest::Poll),
        SourceSyncRequest::Manual { audit } => audit,
    };

    let fingerprint = manual_sync_fingerprint(command.expected_configuration_version);
    if let Some(sync) = replay_manual_sync(transaction, audit, fingerprint).await? {
        return Ok(PreparedSyncRequest::Replayed(sync));
    }
    map_auth_result(
        require_principal_scope(
            transaction,
            &audit.principal,
            AdminScope::SourceSync,
            command.requested_at,
        )
        .await,
    )?;
    Ok(PreparedSyncRequest::Manual { audit, fingerprint })
}

pub(crate) async fn advance_sync(
    transaction: &mut Transaction<'_, Sqlite>,
    command: AdvanceSourceSync,
) -> Result<StoredSourceSync, SourceApplyError> {
    let current = required_sync(transaction, command.source_sync_id).await?;
    require_active_version(&current, command.expected_version)?;
    if command.updated_at < current.updated_at
        || !valid_stage_transition(current.stage, &command.progress)
    {
        return Err(DatabaseCommandError::Rejected.into());
    }
    let next_version = checked_next_version(current.version)?;
    let (stage, source_commit, content_digest) = match command.progress {
        SourceSyncProgress::Fetching => (SourceSyncStage::Fetching, None, None),
        SourceSyncProgress::ResolvingCommit => (SourceSyncStage::ResolvingCommit, None, None),
        SourceSyncProgress::PreparingCandidate { source_commit } => (
            SourceSyncStage::PreparingCandidate,
            Some(source_commit),
            None,
        ),
        SourceSyncProgress::Compiling { source_commit } => {
            (SourceSyncStage::Compiling, Some(source_commit), None)
        }
        SourceSyncProgress::Reloading {
            source_commit,
            content_digest,
        } => (
            SourceSyncStage::Reloading,
            Some(source_commit),
            Some(content_digest),
        ),
    };
    let result = sqlx::query(
        "UPDATE source_sync_operations SET stage = ?, source_commit = ?, content_digest = ?, \
            version = ?, updated_at_ns = ? \
         WHERE source_sync_id = ? AND version = ? AND outcome IS NULL",
    )
    .bind(stage.as_str())
    .bind(source_commit.as_ref().map(SourceCommit::as_bytes))
    .bind(
        content_digest
            .as_ref()
            .map(|digest| digest.as_bytes().as_slice()),
    )
    .bind(version_i64(next_version)?)
    .bind(command_timestamp(command.updated_at)?)
    .bind(command.source_sync_id.as_uuid().as_bytes().as_slice())
    .bind(version_i64(current.version)?)
    .execute(&mut **transaction)
    .await?;
    require_one_row(result.rows_affected())?;
    Ok(StoredSourceSync {
        stage,
        source_commit,
        content_digest,
        version: next_version,
        updated_at: command.updated_at.to_offset(UtcOffset::UTC),
        ..current
    })
}

pub(crate) async fn apply_catalog(
    transaction: &mut Transaction<'_, Sqlite>,
    command: ApplyManagedSourceCatalog,
) -> Result<StoredSourceSync, SourceApplyError> {
    let current = required_sync(transaction, command.source_sync_id).await?;
    require_active_version(&current, command.expected_sync_version)?;
    if current.stage != SourceSyncStage::Reloading
        || current.source_commit.as_ref() != Some(&command.source_commit)
        || current.content_digest.as_ref() != Some(&command.content_digest)
        || command.completed_at < current.updated_at
    {
        return Err(DatabaseCommandError::Rejected.into());
    }
    require_current_configuration_version(transaction, current.configuration_version).await?;
    index_content_catalog(
        transaction,
        IndexContentCatalog {
            observed_at: command.completed_at,
            source_commit: Some(command.source_commit.clone()),
            posts: command.observed_posts,
        },
    )
    .await
    .map_err(map_publication_error)?;

    let completed_at = command.completed_at.to_offset(UtcOffset::UTC);
    let completed_at_ns = command_timestamp(completed_at)?;
    sqlx::query(
        "INSERT INTO source_installation (\
            singleton, configuration_version, source_commit, content_digest, source_sync_id, \
            installed_at_ns\
         ) VALUES (1, ?, ?, ?, ?, ?) \
         ON CONFLICT(singleton) DO UPDATE SET \
            configuration_version = excluded.configuration_version, \
            source_commit = excluded.source_commit, content_digest = excluded.content_digest, \
            source_sync_id = excluded.source_sync_id, installed_at_ns = excluded.installed_at_ns",
    )
    .bind(version_i64(current.configuration_version.get())?)
    .bind(command.source_commit.as_bytes())
    .bind(command.content_digest.as_bytes().as_slice())
    .bind(command.source_sync_id.as_uuid().as_bytes().as_slice())
    .bind(completed_at_ns)
    .execute(&mut **transaction)
    .await?;
    let next_version = checked_next_version(current.version)?;
    terminalize_sync(
        transaction,
        &current,
        TerminalSyncUpdate {
            next_version,
            outcome: SourceSyncOutcome::Applied,
            source_commit: Some(&command.source_commit),
            content_digest: Some(&command.content_digest),
            failure_code: None,
            completed_at_ns,
        },
    )
    .await?;
    schedule_next_poll(transaction, current.configuration_version, completed_at).await?;
    Ok(StoredSourceSync {
        outcome: Some(SourceSyncOutcome::Applied),
        source_commit: Some(command.source_commit),
        content_digest: Some(command.content_digest),
        failure_code: None,
        version: next_version,
        updated_at: completed_at,
        finished_at: Some(completed_at),
        ..current
    })
}

pub(crate) async fn finish_sync(
    transaction: &mut Transaction<'_, Sqlite>,
    command: FinishSourceSync,
) -> Result<StoredSourceSync, SourceApplyError> {
    let current = required_sync(transaction, command.source_sync_id).await?;
    require_active_version(&current, command.expected_version)?;
    if command.completed_at < current.updated_at {
        return Err(DatabaseCommandError::Rejected.into());
    }
    let completed_at = command.completed_at.to_offset(UtcOffset::UTC);
    let completed_at_ns = command_timestamp(completed_at)?;
    let next_version = checked_next_version(current.version)?;

    let (outcome, source_commit, content_digest, failure_code) = match command.completion {
        SourceSyncCompletion::NoChange { source_commit } => {
            if current.stage != SourceSyncStage::ResolvingCommit {
                return Err(DatabaseCommandError::Rejected.into());
            }
            require_current_configuration_version(transaction, current.configuration_version)
                .await?;
            let installed = load_installation(transaction)
                .await?
                .ok_or(DatabaseCommandError::Rejected)?;
            if installed.configuration_version != current.configuration_version
                || installed.source_commit != source_commit
            {
                return Err(DatabaseCommandError::Rejected.into());
            }
            sqlx::query(
                "UPDATE source_installation SET configuration_version = ?, \
                        source_sync_id = ?, installed_at_ns = ? WHERE singleton = 1",
            )
            .bind(version_i64(current.configuration_version.get())?)
            .bind(command.source_sync_id.as_uuid().as_bytes().as_slice())
            .bind(completed_at_ns)
            .execute(&mut **transaction)
            .await?;
            (
                SourceSyncOutcome::NoChange,
                Some(source_commit),
                Some(installed.content_digest),
                None,
            )
        }
        SourceSyncCompletion::Failed { code } => (
            SourceSyncOutcome::Failed,
            current.source_commit.clone(),
            current.content_digest.clone(),
            Some(code),
        ),
        SourceSyncCompletion::Cancelled => (
            SourceSyncOutcome::Cancelled,
            current.source_commit.clone(),
            current.content_digest.clone(),
            None,
        ),
    };
    terminalize_sync(
        transaction,
        &current,
        TerminalSyncUpdate {
            next_version,
            outcome,
            source_commit: source_commit.as_ref(),
            content_digest: content_digest.as_ref(),
            failure_code,
            completed_at_ns,
        },
    )
    .await?;
    schedule_next_poll(transaction, current.configuration_version, completed_at).await?;
    Ok(StoredSourceSync {
        outcome: Some(outcome),
        source_commit,
        content_digest,
        failure_code,
        version: next_version,
        updated_at: completed_at,
        finished_at: Some(completed_at),
        ..current
    })
}

async fn load_configuration(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<Option<StoredSourceConfiguration>, SourceApplyError> {
    sqlx::query_as::<_, SourceConfigurationRow>(
        "SELECT ssh_user, ssh_host, ssh_port, repository_path, branch, \
                content_subdirectory, credential_name, poll_interval_seconds, version, \
                updated_at_ns, next_poll_at_ns \
         FROM source_configuration WHERE singleton = 1",
    )
    .fetch_optional(&mut **transaction)
    .await?
    .map(decode_configuration)
    .transpose()
    .map_err(|_| SourceApplyError::CorruptStoredState)
}

async fn load_configuration_revision(
    transaction: &mut Transaction<'_, Sqlite>,
    version: i64,
) -> Result<Option<StoredSourceConfiguration>, SourceApplyError> {
    sqlx::query_as::<_, SourceConfigurationRow>(
        "SELECT ssh_user, ssh_host, ssh_port, repository_path, branch, \
                content_subdirectory, credential_name, poll_interval_seconds, version, \
                updated_at_ns, NULL AS next_poll_at_ns \
         FROM source_configuration_revisions WHERE version = ?",
    )
    .bind(version)
    .fetch_optional(&mut **transaction)
    .await?
    .map(decode_configuration)
    .transpose()
    .map_err(|_| SourceApplyError::CorruptStoredState)
}

async fn load_installation(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<Option<InstalledSource>, SourceApplyError> {
    sqlx::query_as::<_, SourceInstallationRow>(
        "SELECT configuration_version, source_commit, content_digest, source_sync_id, \
                installed_at_ns FROM source_installation WHERE singleton = 1",
    )
    .fetch_optional(&mut **transaction)
    .await?
    .map(decode_installation)
    .transpose()
    .map_err(|_| SourceApplyError::CorruptStoredState)
}

async fn load_active_sync(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<Option<StoredSourceSync>, SourceApplyError> {
    sqlx::query_as::<_, SourceSyncRow>(SOURCE_SYNC_COLUMNS_ACTIVE)
        .fetch_optional(&mut **transaction)
        .await?
        .map(decode_sync)
        .transpose()
        .map_err(|_| SourceApplyError::CorruptStoredState)
}

async fn required_sync(
    transaction: &mut Transaction<'_, Sqlite>,
    source_sync_id: SourceSyncId,
) -> Result<StoredSourceSync, SourceApplyError> {
    let row = sqlx::query_as::<_, SourceSyncRow>(SOURCE_SYNC_COLUMNS_BY_ID)
        .bind(source_sync_id.as_uuid().as_bytes().as_slice())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(DatabaseCommandError::Rejected)?;
    decode_sync(row).map_err(|_| SourceApplyError::CorruptStoredState)
}

async fn require_current_configuration_version(
    transaction: &mut Transaction<'_, Sqlite>,
    expected: SourceConfigurationVersion,
) -> Result<(), SourceApplyError> {
    let actual: Option<i64> =
        sqlx::query_scalar("SELECT version FROM source_configuration WHERE singleton = 1")
            .fetch_optional(&mut **transaction)
            .await?;
    if actual == Some(version_i64(expected.get())?) {
        Ok(())
    } else {
        Err(DatabaseCommandError::Rejected.into())
    }
}

fn require_active_version(
    sync: &StoredSourceSync,
    expected_version: u64,
) -> Result<(), SourceApplyError> {
    if sync.outcome.is_none() && sync.version == expected_version {
        Ok(())
    } else {
        Err(DatabaseCommandError::Rejected.into())
    }
}

fn valid_stage_transition(current: SourceSyncStage, progress: &SourceSyncProgress) -> bool {
    matches!(
        (current, progress),
        (SourceSyncStage::Queued, SourceSyncProgress::Fetching)
            | (
                SourceSyncStage::Fetching,
                SourceSyncProgress::ResolvingCommit
            )
            | (
                SourceSyncStage::ResolvingCommit,
                SourceSyncProgress::PreparingCandidate { .. }
            )
            | (
                SourceSyncStage::PreparingCandidate,
                SourceSyncProgress::Compiling { .. }
            )
            | (
                SourceSyncStage::Compiling,
                SourceSyncProgress::Reloading { .. }
            )
    )
}

struct TerminalSyncUpdate<'value> {
    next_version: u64,
    outcome: SourceSyncOutcome,
    source_commit: Option<&'value SourceCommit>,
    content_digest: Option<&'value ContentTreeDigest>,
    failure_code: Option<SourceSyncFailureCode>,
    completed_at_ns: i64,
}

async fn terminalize_sync(
    transaction: &mut Transaction<'_, Sqlite>,
    current: &StoredSourceSync,
    update: TerminalSyncUpdate<'_>,
) -> Result<(), SourceApplyError> {
    let result = sqlx::query(
        "UPDATE source_sync_operations SET outcome = ?, source_commit = ?, content_digest = ?, \
            failure_code = ?, version = ?, updated_at_ns = ?, finished_at_ns = ? \
         WHERE source_sync_id = ? AND version = ? AND outcome IS NULL",
    )
    .bind(update.outcome.as_str())
    .bind(update.source_commit.map(SourceCommit::as_bytes))
    .bind(
        update
            .content_digest
            .map(|digest| digest.as_bytes().as_slice()),
    )
    .bind(update.failure_code.map(SourceSyncFailureCode::as_str))
    .bind(version_i64(update.next_version)?)
    .bind(update.completed_at_ns)
    .bind(update.completed_at_ns)
    .bind(current.source_sync_id.as_uuid().as_bytes().as_slice())
    .bind(version_i64(current.version)?)
    .execute(&mut **transaction)
    .await?;
    require_one_row(result.rows_affected())?;
    prune_source_sync_history(transaction, MAX_RETAINED_TERMINAL_SOURCE_SYNCS).await
}

async fn prune_source_sync_history(
    transaction: &mut Transaction<'_, Sqlite>,
    retained_terminal_count: u32,
) -> Result<(), SourceApplyError> {
    let retained_terminal_count = i64::from(retained_terminal_count);
    // Manual aliases have their own bounded replay window. Preserve the
    // operation behind each retained alias so unrelated poll traffic cannot
    // shorten that window.
    sqlx::query(
        "DELETE FROM source_sync_operations \
         WHERE source_sync_id IN (\
             SELECT source_sync_id FROM source_sync_operations \
             WHERE outcome IS NOT NULL \
             ORDER BY requested_at_ns DESC, source_sync_id DESC \
             LIMIT -1 OFFSET ?\
         ) \
         AND source_sync_id NOT IN (\
             SELECT source_sync_id FROM source_installation\
         ) \
         AND source_sync_id NOT IN (\
             SELECT source_sync_id FROM source_sync_idempotency_aliases\
         )",
    )
    .bind(retained_terminal_count)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn prune_source_sync_aliases(
    transaction: &mut Transaction<'_, Sqlite>,
    retained_alias_count: u32,
) -> Result<(), SourceApplyError> {
    sqlx::query(
        "DELETE FROM source_sync_idempotency_aliases \
         WHERE idempotency_key IN (\
             SELECT idempotency_key FROM source_sync_idempotency_aliases \
             ORDER BY requested_at_ns DESC, idempotency_key DESC \
             LIMIT -1 OFFSET ?\
         )",
    )
    .bind(i64::from(retained_alias_count))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn schedule_next_poll(
    transaction: &mut Transaction<'_, Sqlite>,
    configuration_version: SourceConfigurationVersion,
    completed_at: OffsetDateTime,
) -> Result<(), SourceApplyError> {
    let interval: Option<i64> = sqlx::query_scalar(
        "SELECT poll_interval_seconds FROM source_configuration \
         WHERE singleton = 1 AND version = ?",
    )
    .bind(version_i64(configuration_version.get())?)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(interval) = interval else {
        return Ok(());
    };
    let interval = u64::try_from(interval)
        .ok()
        .and_then(SourcePollInterval::from_seconds)
        .ok_or(SourceApplyError::CorruptStoredState)?;
    let interval =
        i64::try_from(interval.seconds()).map_err(|_| SourceApplyError::CorruptStoredState)?;
    let next_poll_at = completed_at
        .checked_add(Duration::seconds(interval))
        .ok_or(DatabaseCommandError::InvalidValue)?;
    sqlx::query(
        "UPDATE source_configuration SET next_poll_at_ns = ? \
         WHERE singleton = 1 AND version = ?",
    )
    .bind(command_timestamp(next_poll_at)?)
    .bind(version_i64(configuration_version.get())?)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn replay_configuration_mutation(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: &MutationAuditContext,
    fingerprint: [u8; 32],
) -> Result<Option<StoredSourceConfiguration>, SourceApplyError> {
    let row = sqlx::query_as::<_, MutationReceiptRow>(
        "SELECT receipt.command_fingerprint, receipt.result_version, \
                NULL AS source_sync_id, audit.principal_kind, audit.actor_user_id, \
                audit.session_id, audit.agent_credential_id, audit.action \
         FROM source_configuration_mutation_receipts AS receipt \
         JOIN admin_audit_events AS audit ON audit.audit_event_id = receipt.audit_event_id \
         WHERE receipt.idempotency_key = ?",
    )
    .bind(audit.idempotency_key.0.as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        reject_claimed_idempotency_key(transaction, audit.idempotency_key).await?;
        return Ok(None);
    };
    if row.command_fingerprint.as_slice() != fingerprint
        || row.action != PUT_CONFIGURATION_ACTION
        || !stored_principal_matches(&row, &audit.principal)
        || row.source_sync_id.is_some()
    {
        return Err(DatabaseCommandError::IdempotencyConflict.into());
    }
    let version = row
        .result_version
        .ok_or(SourceApplyError::CorruptStoredState)?;
    load_configuration_revision(transaction, version)
        .await?
        .ok_or(SourceApplyError::CorruptStoredState)
        .map(Some)
}

async fn replay_manual_sync(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: &MutationAuditContext,
    fingerprint: [u8; 32],
) -> Result<Option<StoredSourceSync>, SourceApplyError> {
    let row = sqlx::query_as::<_, MutationReceiptRow>(
        "SELECT alias.command_fingerprint, NULL AS result_version, alias.source_sync_id, \
                audit.principal_kind, audit.actor_user_id, audit.session_id, \
                audit.agent_credential_id, audit.action \
         FROM source_sync_idempotency_aliases AS alias \
         JOIN admin_audit_events AS audit ON audit.audit_event_id = alias.audit_event_id \
         WHERE alias.idempotency_key = ?",
    )
    .bind(audit.idempotency_key.0.as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        reject_claimed_idempotency_key(transaction, audit.idempotency_key).await?;
        return Ok(None);
    };
    if row.command_fingerprint.as_slice() != fingerprint
        || row.action != MANUAL_SYNC_ACTION
        || !stored_principal_matches(&row, &audit.principal)
        || row.result_version.is_some()
    {
        return Err(DatabaseCommandError::IdempotencyConflict.into());
    }
    let identifier = row
        .source_sync_id
        .as_deref()
        .ok_or(SourceApplyError::CorruptStoredState)
        .and_then(|value| {
            decode_source_sync_id(value).map_err(|_| SourceApplyError::CorruptStoredState)
        })?;
    required_sync(transaction, identifier).await.map(Some)
}

async fn reject_claimed_idempotency_key(
    transaction: &mut Transaction<'_, Sqlite>,
    idempotency_key: AdminMutationKey,
) -> Result<(), SourceApplyError> {
    let claimed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM admin_audit_events WHERE idempotency_key = ?)",
    )
    .bind(idempotency_key.0.as_bytes().as_slice())
    .fetch_one(&mut **transaction)
    .await?;
    if claimed {
        Err(DatabaseCommandError::IdempotencyConflict.into())
    } else {
        Ok(())
    }
}

fn stored_principal_matches(row: &MutationReceiptRow, principal: &AuditPrincipalReference) -> bool {
    let bytes_match = |stored: Option<&Vec<u8>>, expected: Option<&[u8]>| match (stored, expected) {
        (Some(stored), Some(expected)) => stored.as_slice() == expected,
        (None, None) => true,
        _ => false,
    };
    match principal {
        AuditPrincipalReference::BrowserSession {
            user_id,
            session_id,
        } => {
            row.principal_kind == "browser_session"
                && bytes_match(
                    row.actor_user_id.as_ref(),
                    Some(user_id.as_uuid().as_bytes()),
                )
                && bytes_match(
                    row.session_id.as_ref(),
                    Some(session_id.as_uuid().as_bytes()),
                )
                && row.agent_credential_id.is_none()
        }
        AuditPrincipalReference::AgentCredential {
            user_id,
            credential_id,
        } => {
            row.principal_kind == "agent_credential"
                && bytes_match(
                    row.actor_user_id.as_ref(),
                    Some(user_id.as_uuid().as_bytes()),
                )
                && row.session_id.is_none()
                && bytes_match(
                    row.agent_credential_id.as_ref(),
                    Some(credential_id.as_uuid().as_bytes()),
                )
        }
        AuditPrincipalReference::Offline { user_id } => {
            row.principal_kind == "offline"
                && bytes_match(
                    row.actor_user_id.as_ref(),
                    user_id
                        .as_ref()
                        .map(|value| value.as_uuid().as_bytes().as_slice()),
                )
                && row.session_id.is_none()
                && row.agent_credential_id.is_none()
        }
        AuditPrincipalReference::Unauthenticated => {
            row.principal_kind == "unauthenticated"
                && row.actor_user_id.is_none()
                && row.session_id.is_none()
                && row.agent_credential_id.is_none()
        }
    }
}

fn configuration_fingerprint(request: &ManagedSourceConfigurationInput) -> [u8; 32] {
    let mut builder = SourceFingerprint::new(PUT_CONFIGURATION_ACTION);
    builder.field(request.remote.user.as_str().as_bytes());
    builder.field(request.remote.host.as_str().as_bytes());
    builder.field(&request.remote.port.get().to_be_bytes());
    builder.field(request.remote.repository_path.as_str().as_bytes());
    builder.field(request.branch.as_str().as_bytes());
    builder.field(request.content_subdirectory.as_str().as_bytes());
    builder.field(request.credential_name.as_str().as_bytes());
    builder.field(&request.poll_interval_seconds.seconds().to_be_bytes());
    match request.expected_version {
        Some(version) => {
            builder.field(&[1]);
            builder.field(&version.get().to_be_bytes());
        }
        None => builder.field(&[0]),
    }
    builder.finish()
}

fn manual_sync_fingerprint(version: SourceConfigurationVersion) -> [u8; 32] {
    let mut builder = SourceFingerprint::new(MANUAL_SYNC_ACTION);
    builder.field(&version.get().to_be_bytes());
    builder.finish()
}

struct SourceFingerprint(blake3::Hasher);

impl SourceFingerprint {
    fn new(action: &'static str) -> Self {
        let mut builder = Self(blake3::Hasher::new());
        builder.field(action.as_bytes());
        builder
    }

    fn field(&mut self, value: &[u8]) {
        self.0.update(&(value.len() as u64).to_be_bytes());
        self.0.update(value);
    }

    fn finish(self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
    }
}

fn map_auth_result<Output>(
    result: Result<Output, AuthApplyError>,
) -> Result<Output, SourceApplyError> {
    result.map_err(|error| match error {
        AuthApplyError::Command(AuthCommandError::IdempotencyConflict) => {
            SourceApplyError::Command(DatabaseCommandError::IdempotencyConflict)
        }
        AuthApplyError::Command(AuthCommandError::InvalidValue) => {
            SourceApplyError::Command(DatabaseCommandError::InvalidValue)
        }
        AuthApplyError::Command(_) => SourceApplyError::Command(DatabaseCommandError::Rejected),
        AuthApplyError::Operation(source) => SourceApplyError::Operation(source),
        AuthApplyError::CorruptStoredState => SourceApplyError::CorruptStoredState,
    })
}

fn map_publication_error(error: StartupSnapshotMutationError) -> SourceApplyError {
    match error {
        StartupSnapshotMutationError::Command(error) => SourceApplyError::Command(error),
        StartupSnapshotMutationError::Operation(source) => SourceApplyError::Operation(source),
        StartupSnapshotMutationError::CorruptStoredState => SourceApplyError::CorruptStoredState,
    }
}

fn checked_next_version(value: u64) -> Result<u64, SourceApplyError> {
    value
        .checked_add(1)
        .filter(|value| i64::try_from(*value).is_ok())
        .ok_or_else(|| DatabaseCommandError::InvalidValue.into())
}

fn version_i64(value: u64) -> Result<i64, SourceApplyError> {
    i64::try_from(value).map_err(|_| DatabaseCommandError::InvalidValue.into())
}

fn version_i64_from_u64(value: u64) -> Result<i64, SourceApplyError> {
    version_i64(value)
}

fn command_timestamp(value: OffsetDateTime) -> Result<i64, SourceApplyError> {
    i64::try_from(value.unix_timestamp_nanos())
        .map_err(|_| DatabaseCommandError::InvalidValue.into())
}

fn require_one_row(rows: u64) -> Result<(), SourceApplyError> {
    if rows == 1 {
        Ok(())
    } else {
        Err(DatabaseCommandError::Rejected.into())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use maincopy_shared::auth::{
        AdminAuditEventId, AdminSessionId, AgentCredentialId, InstanceId, UserId,
    };
    use markdown_compiler::{DraftStatus, PostId, PostRevisionDigest, PostSlug};
    use sqlx::{Connection as _, SqliteConnection, sqlite::SqliteConnectOptions};
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        config::{
            DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
            DatabaseWriterQueueCapacity,
        },
        database::{self, store::DatabaseStore},
        domain::auth::{
            NostrPublicKey,
            store::{BootstrapIdentity, ConfiguredLoginProviders, NewHumanCredential},
        },
    };

    const OWNER_KEY: &str = "63fe6318dc58583cfe16810f86dd09e18bfd76aabc24a0081ce2856f330504ed";

    struct Harness {
        _root: tempfile::TempDir,
        path: PathBuf,
        store: DatabaseStore,
        shutdown: CancellationToken,
        writer: JoinHandle<()>,
        owner: UserId,
    }

    impl Harness {
        async fn start() -> Self {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("state/maincopy.db");
            let database = database::bootstrap(database_configuration(&path))
                .await
                .unwrap();
            let (store, writer) = database.into_store(32);
            let shutdown = CancellationToken::new();
            let task_shutdown = shutdown.clone();
            let writer = tokio::spawn(async move {
                writer.run(task_shutdown).await.unwrap();
            });
            let owner = UserId::from_uuid(Uuid::from_u128(2));
            store
                .auth
                .bootstrap_identity(BootstrapIdentity {
                    instance_id: InstanceId::from_uuid(Uuid::from_u128(1)),
                    owner_user_id: owner,
                    credential: NewHumanCredential::Nostr {
                        public_key: NostrPublicKey::parse(OWNER_KEY).unwrap(),
                    },
                    configured_providers: ConfiguredLoginProviders::new(false, true).unwrap(),
                    occurred_at: at(1),
                    audit_event_id: AdminAuditEventId::from_uuid(Uuid::from_u128(3)),
                })
                .await
                .unwrap();
            Self {
                _root: root,
                path,
                store,
                shutdown,
                writer,
                owner,
            }
        }

        async fn stop(self) -> (tempfile::TempDir, PathBuf) {
            let Self {
                _root: root,
                path,
                store,
                shutdown,
                writer,
                owner: _,
            } = self;
            drop(store);
            shutdown.cancel();
            writer.await.unwrap();
            (root, path)
        }
    }

    fn database_configuration(path: &Path) -> DatabaseConfigurationView<'_> {
        DatabaseConfigurationView {
            path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1_000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(32).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        }
    }

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    fn audit(owner: UserId, value: u128) -> MutationAuditContext {
        MutationAuditContext {
            audit_event_id: AdminAuditEventId::from_uuid(Uuid::from_u128(1_000 + value)),
            principal: AuditPrincipalReference::Offline {
                user_id: Some(owner),
            },
            request_id: Some(Uuid::from_u128(2_000 + value)),
            idempotency_key: AdminMutationKey(Uuid::from_u128(3_000 + value)),
        }
    }

    fn configuration_request(
        expected_version: Option<SourceConfigurationVersion>,
        branch: &str,
    ) -> ManagedSourceConfigurationInput {
        ManagedSourceConfigurationInput {
            remote: SshRemote {
                user: SshRemoteUser::parse("git").unwrap(),
                host: SshRemoteHost::parse("git.example.test").unwrap(),
                port: SshRemotePort::new(22).unwrap(),
                repository_path: SshRepositoryPath::parse("publisher/site.git").unwrap(),
            },
            branch: GitBranchName::parse(branch).unwrap(),
            content_subdirectory: RepositoryContentSubdirectory::parse("content").unwrap(),
            credential_name: SshCredentialName::parse("production").unwrap(),
            poll_interval_seconds: SourcePollInterval::from_seconds(60).unwrap(),
            expected_version,
        }
    }

    fn begin(
        id: u128,
        version: SourceConfigurationVersion,
        requested_at: i64,
        request: SourceSyncRequest,
    ) -> BeginSourceSync {
        BeginSourceSync {
            proposed_source_sync_id: SourceSyncId::from_uuid(Uuid::from_u128(id)),
            expected_configuration_version: version,
            requested_at: at(requested_at),
            request,
        }
    }

    fn commit(byte: u8) -> SourceCommit {
        SourceCommit::parse(&format!("git-sha1:{}", format!("{byte:02x}").repeat(20))).unwrap()
    }

    fn assert_corrupt_sync_shape(
        result: Result<(), SourceLoadError>,
        expected_field: &'static str,
    ) {
        match result {
            Err(SourceLoadError::Corrupt { field }) => assert_eq!(field, expected_field),
            Err(error) => panic!("expected corrupt sync shape, got {error}"),
            Ok(()) => panic!("expected corrupt sync shape"),
        }
    }

    async fn advance(
        store: &SourceStore,
        current: StoredSourceSync,
        progress: SourceSyncProgress,
        second: i64,
    ) -> StoredSourceSync {
        store
            .advance_sync(AdvanceSourceSync {
                source_sync_id: current.source_sync_id,
                expected_version: current.version,
                progress,
                updated_at: at(second),
            })
            .await
            .unwrap()
    }

    async fn install_sync(
        store: &SourceStore,
        id: u128,
        configuration_version: SourceConfigurationVersion,
        requested_at: i64,
    ) -> StoredSourceSync {
        let mut sync = store
            .begin_sync(begin(
                id,
                configuration_version,
                requested_at,
                SourceSyncRequest::Poll,
            ))
            .await
            .unwrap()
            .sync;
        sync = advance(store, sync, SourceSyncProgress::Fetching, requested_at + 1).await;
        sync = advance(
            store,
            sync,
            SourceSyncProgress::ResolvingCommit,
            requested_at + 2,
        )
        .await;
        let source_commit = commit(0x44);
        sync = advance(
            store,
            sync,
            SourceSyncProgress::PreparingCandidate {
                source_commit: source_commit.clone(),
            },
            requested_at + 3,
        )
        .await;
        sync = advance(
            store,
            sync,
            SourceSyncProgress::Compiling {
                source_commit: source_commit.clone(),
            },
            requested_at + 4,
        )
        .await;
        let content_digest = ContentTreeDigest::from_bytes([0x55; 32]);
        sync = advance(
            store,
            sync,
            SourceSyncProgress::Reloading {
                source_commit: source_commit.clone(),
                content_digest: content_digest.clone(),
            },
            requested_at + 5,
        )
        .await;
        store
            .apply_catalog(ApplyManagedSourceCatalog {
                source_sync_id: sync.source_sync_id,
                expected_sync_version: sync.version,
                source_commit,
                content_digest,
                observed_posts: Vec::new(),
                completed_at: at(requested_at + 6),
            })
            .await
            .unwrap()
    }

    async fn fail_sync(
        store: &SourceStore,
        id: u128,
        configuration_version: SourceConfigurationVersion,
        requested_at: i64,
        request: SourceSyncRequest,
    ) -> StoredSourceSync {
        let sync = store
            .begin_sync(begin(id, configuration_version, requested_at, request))
            .await
            .unwrap()
            .sync;
        store
            .finish_sync(FinishSourceSync {
                source_sync_id: sync.source_sync_id,
                expected_version: sync.version,
                completion: SourceSyncCompletion::Failed {
                    code: SourceSyncFailureCode::Internal,
                },
                completed_at: at(requested_at + 1),
            })
            .await
            .unwrap()
    }

    #[test]
    fn stored_sync_shapes_enforce_terminal_metadata_and_stage_provenance() {
        let source_commit = commit(0x44);
        let content_digest = ContentTreeDigest::from_bytes([0x55; 32]);
        let finished_at = at(10);
        let validate = |stage: SourceSyncStage,
                        outcome: Option<SourceSyncOutcome>,
                        has_commit: bool,
                        has_digest: bool,
                        failure_code: Option<SourceSyncFailureCode>,
                        is_finished: bool| {
            validate_sync_shape(
                stage,
                outcome,
                has_commit.then_some(&source_commit),
                has_digest.then_some(&content_digest),
                failure_code,
                is_finished.then_some(finished_at),
            )
        };

        for (stage, outcome, has_commit, has_digest, failure_code, is_finished) in [
            (SourceSyncStage::Queued, None, false, false, None, false),
            (SourceSyncStage::Fetching, None, false, false, None, false),
            (
                SourceSyncStage::ResolvingCommit,
                None,
                false,
                false,
                None,
                false,
            ),
            (
                SourceSyncStage::PreparingCandidate,
                None,
                true,
                false,
                None,
                false,
            ),
            (SourceSyncStage::Compiling, None, true, false, None, false),
            (SourceSyncStage::Reloading, None, true, true, None, false),
            (
                SourceSyncStage::Reloading,
                Some(SourceSyncOutcome::Applied),
                true,
                true,
                None,
                true,
            ),
            (
                SourceSyncStage::ResolvingCommit,
                Some(SourceSyncOutcome::NoChange),
                true,
                true,
                None,
                true,
            ),
            (
                SourceSyncStage::Fetching,
                Some(SourceSyncOutcome::Failed),
                false,
                false,
                Some(SourceSyncFailureCode::FetchFailed),
                true,
            ),
            (
                SourceSyncStage::Compiling,
                Some(SourceSyncOutcome::Cancelled),
                true,
                false,
                None,
                true,
            ),
        ] {
            assert!(
                validate(
                    stage,
                    outcome,
                    has_commit,
                    has_digest,
                    failure_code,
                    is_finished,
                )
                .is_ok()
            );
        }

        for ((stage, outcome, has_commit, has_digest, failure_code, is_finished), expected_field) in [
            (
                (
                    SourceSyncStage::Fetching,
                    Some(SourceSyncOutcome::Failed),
                    false,
                    false,
                    Some(SourceSyncFailureCode::FetchFailed),
                    false,
                ),
                "sync terminal state",
            ),
            (
                (SourceSyncStage::Fetching, None, false, false, None, true),
                "sync terminal state",
            ),
            (
                (
                    SourceSyncStage::Fetching,
                    Some(SourceSyncOutcome::Cancelled),
                    false,
                    false,
                    Some(SourceSyncFailureCode::Internal),
                    true,
                ),
                "sync terminal state",
            ),
            (
                (
                    SourceSyncStage::Reloading,
                    Some(SourceSyncOutcome::Applied),
                    false,
                    true,
                    None,
                    true,
                ),
                "applied sync result",
            ),
            (
                (
                    SourceSyncStage::Reloading,
                    Some(SourceSyncOutcome::Applied),
                    true,
                    false,
                    None,
                    true,
                ),
                "applied sync result",
            ),
            (
                (
                    SourceSyncStage::Compiling,
                    Some(SourceSyncOutcome::Applied),
                    true,
                    true,
                    None,
                    true,
                ),
                "applied sync result",
            ),
            (
                (
                    SourceSyncStage::ResolvingCommit,
                    Some(SourceSyncOutcome::NoChange),
                    false,
                    true,
                    None,
                    true,
                ),
                "no-change sync result",
            ),
            (
                (
                    SourceSyncStage::ResolvingCommit,
                    Some(SourceSyncOutcome::NoChange),
                    true,
                    false,
                    None,
                    true,
                ),
                "no-change sync result",
            ),
            (
                (
                    SourceSyncStage::Fetching,
                    Some(SourceSyncOutcome::NoChange),
                    true,
                    true,
                    None,
                    true,
                ),
                "no-change sync result",
            ),
            (
                (SourceSyncStage::Queued, None, true, false, None, false),
                "early sync provenance",
            ),
            (
                (SourceSyncStage::Fetching, None, false, true, None, false),
                "early sync provenance",
            ),
            (
                (
                    SourceSyncStage::PreparingCandidate,
                    None,
                    false,
                    false,
                    None,
                    false,
                ),
                "candidate sync provenance",
            ),
            (
                (SourceSyncStage::Compiling, None, true, true, None, false),
                "candidate sync provenance",
            ),
            (
                (SourceSyncStage::Reloading, None, false, true, None, false),
                "reload sync provenance",
            ),
            (
                (SourceSyncStage::Reloading, None, true, false, None, false),
                "reload sync provenance",
            ),
        ] {
            assert_corrupt_sync_shape(
                validate(
                    stage,
                    outcome,
                    has_commit,
                    has_digest,
                    failure_code,
                    is_finished,
                ),
                expected_field,
            );
        }
    }

    #[test]
    fn idempotency_receipts_match_only_the_exact_stored_principal_shape() {
        let user_id = UserId::from_uuid(Uuid::from_u128(40));
        let other_user_id = UserId::from_uuid(Uuid::from_u128(41));
        let session_id = AdminSessionId::from_uuid(Uuid::from_u128(42));
        let credential_id = AgentCredentialId::from_uuid(Uuid::from_u128(43));
        let bytes = |identifier: &Uuid| Some(identifier.as_bytes().to_vec());
        let row = |principal_kind: &str,
                   actor_user_id: Option<Vec<u8>>,
                   session_id: Option<Vec<u8>>,
                   agent_credential_id: Option<Vec<u8>>| MutationReceiptRow {
            command_fingerprint: vec![0; 32],
            result_version: None,
            source_sync_id: None,
            principal_kind: principal_kind.to_owned(),
            actor_user_id,
            session_id,
            agent_credential_id,
            action: MANUAL_SYNC_ACTION.to_owned(),
        };

        let browser = AuditPrincipalReference::BrowserSession {
            user_id,
            session_id,
        };
        assert!(stored_principal_matches(
            &row(
                "browser_session",
                bytes(user_id.as_uuid()),
                bytes(session_id.as_uuid()),
                None,
            ),
            &browser,
        ));
        assert!(!stored_principal_matches(
            &row(
                "browser_session",
                bytes(other_user_id.as_uuid()),
                bytes(session_id.as_uuid()),
                None,
            ),
            &browser,
        ));
        assert!(!stored_principal_matches(
            &row("browser_session", bytes(user_id.as_uuid()), None, None,),
            &browser,
        ));
        assert!(!stored_principal_matches(
            &row(
                "browser_session",
                bytes(user_id.as_uuid()),
                bytes(session_id.as_uuid()),
                bytes(credential_id.as_uuid()),
            ),
            &browser,
        ));

        let agent = AuditPrincipalReference::AgentCredential {
            user_id,
            credential_id,
        };
        assert!(stored_principal_matches(
            &row(
                "agent_credential",
                bytes(user_id.as_uuid()),
                None,
                bytes(credential_id.as_uuid()),
            ),
            &agent,
        ));
        assert!(!stored_principal_matches(
            &row(
                "browser_session",
                bytes(user_id.as_uuid()),
                None,
                bytes(credential_id.as_uuid()),
            ),
            &agent,
        ));
        assert!(!stored_principal_matches(
            &row(
                "agent_credential",
                bytes(user_id.as_uuid()),
                bytes(session_id.as_uuid()),
                bytes(credential_id.as_uuid()),
            ),
            &agent,
        ));
        assert!(!stored_principal_matches(
            &row("agent_credential", bytes(user_id.as_uuid()), None, None,),
            &agent,
        ));

        let offline_owner = AuditPrincipalReference::Offline {
            user_id: Some(user_id),
        };
        assert!(stored_principal_matches(
            &row("offline", bytes(user_id.as_uuid()), None, None),
            &offline_owner,
        ));
        assert!(!stored_principal_matches(
            &row("offline", None, None, None),
            &offline_owner,
        ));
        assert!(!stored_principal_matches(
            &row(
                "offline",
                bytes(user_id.as_uuid()),
                bytes(session_id.as_uuid()),
                None,
            ),
            &offline_owner,
        ));

        let offline_system = AuditPrincipalReference::Offline { user_id: None };
        assert!(stored_principal_matches(
            &row("offline", None, None, None),
            &offline_system,
        ));
        assert!(!stored_principal_matches(
            &row("offline", bytes(user_id.as_uuid()), None, None),
            &offline_system,
        ));

        let unauthenticated = AuditPrincipalReference::Unauthenticated;
        assert!(stored_principal_matches(
            &row("unauthenticated", None, None, None),
            &unauthenticated,
        ));
        assert!(!stored_principal_matches(
            &row("offline", None, None, None),
            &unauthenticated,
        ));
        assert!(!stored_principal_matches(
            &row("unauthenticated", Some(vec![0; 15]), None, None,),
            &unauthenticated,
        ));
    }

    #[tokio::test]
    async fn source_configuration_and_sync_state_are_atomic_replayable_and_recoverable() {
        let harness = Harness::start().await;
        let first_audit = audit(harness.owner, 1);
        let first_request = configuration_request(None, "main");
        let first = harness
            .store
            .source
            .put_configuration(PutSourceConfiguration {
                request: first_request.clone(),
                occurred_at: at(10),
                audit: first_audit.clone(),
            })
            .await
            .unwrap();
        let version_one = first.configuration.version;
        assert_eq!(version_one.get(), 1);

        let second = harness
            .store
            .source
            .put_configuration(PutSourceConfiguration {
                request: configuration_request(Some(version_one), "stable"),
                occurred_at: at(11),
                audit: audit(harness.owner, 2),
            })
            .await
            .unwrap();
        let version_two = second.configuration.version;
        assert_eq!(version_two.get(), 2);
        let replayed_first = harness
            .store
            .source
            .put_configuration(PutSourceConfiguration {
                request: first_request,
                occurred_at: at(99),
                audit: first_audit,
            })
            .await
            .unwrap();
        assert_eq!(replayed_first, first);
        assert_eq!(
            harness.store.source.configuration().await.unwrap().unwrap(),
            second
        );

        let created = harness
            .store
            .source
            .begin_sync(begin(10, version_two, 20, SourceSyncRequest::Startup))
            .await
            .unwrap();
        assert_eq!(created.admission, SourceSyncAdmission::Created);
        let manual_audit = audit(harness.owner, 3);
        let coalesced = harness
            .store
            .source
            .begin_sync(begin(
                11,
                version_two,
                21,
                SourceSyncRequest::Manual {
                    audit: manual_audit.clone(),
                },
            ))
            .await
            .unwrap();
        assert_eq!(coalesced.admission, SourceSyncAdmission::Coalesced);
        assert_eq!(coalesced.sync.source_sync_id, created.sync.source_sync_id);
        let replayed = harness
            .store
            .source
            .begin_sync(begin(
                12,
                version_two,
                22,
                SourceSyncRequest::Manual {
                    audit: manual_audit,
                },
            ))
            .await
            .unwrap();
        assert_eq!(replayed.admission, SourceSyncAdmission::Replayed);
        assert_eq!(replayed.sync.source_sync_id, created.sync.source_sync_id);

        let mut sync = advance(
            &harness.store.source,
            created.sync,
            SourceSyncProgress::Fetching,
            23,
        )
        .await;
        sync = advance(
            &harness.store.source,
            sync,
            SourceSyncProgress::ResolvingCommit,
            24,
        )
        .await;
        let source_commit = commit(0x44);
        sync = advance(
            &harness.store.source,
            sync,
            SourceSyncProgress::PreparingCandidate {
                source_commit: source_commit.clone(),
            },
            25,
        )
        .await;
        sync = advance(
            &harness.store.source,
            sync,
            SourceSyncProgress::Compiling {
                source_commit: source_commit.clone(),
            },
            26,
        )
        .await;
        let digest = ContentTreeDigest::from_bytes([0x55; 32]);
        sync = advance(
            &harness.store.source,
            sync,
            SourceSyncProgress::Reloading {
                source_commit: source_commit.clone(),
                content_digest: digest.clone(),
            },
            27,
        )
        .await;

        let post = ObservedPostRevision {
            stable_post_id: PostId::parse("11111111-1111-4111-8111-111111111111").unwrap(),
            revision_digest: PostRevisionDigest::from_bytes([0x66; 32]),
            publication_status: DraftStatus::Publishable,
            slug: PostSlug::parse("first").unwrap(),
        };
        let rejected = harness
            .store
            .source
            .apply_catalog(ApplyManagedSourceCatalog {
                source_sync_id: sync.source_sync_id,
                expected_sync_version: sync.version,
                source_commit: source_commit.clone(),
                content_digest: digest.clone(),
                observed_posts: vec![post.clone(), post.clone()],
                completed_at: at(28),
            })
            .await;
        assert_eq!(
            rejected,
            Err(DatabaseMutationError::Command(
                DatabaseCommandError::InvalidValue
            ))
        );
        assert!(harness.store.source.installation().await.unwrap().is_none());
        assert_eq!(
            harness.store.source.active_sync().await.unwrap().unwrap(),
            sync
        );

        let applied = harness
            .store
            .source
            .apply_catalog(ApplyManagedSourceCatalog {
                source_sync_id: sync.source_sync_id,
                expected_sync_version: sync.version,
                source_commit: source_commit.clone(),
                content_digest: digest.clone(),
                observed_posts: vec![post],
                completed_at: at(29),
            })
            .await
            .unwrap();
        assert_eq!(applied.outcome, Some(SourceSyncOutcome::Applied));
        let installed = harness.store.source.installation().await.unwrap().unwrap();
        assert_eq!(installed.source_commit, source_commit);
        assert_eq!(installed.content_digest, digest);
        assert!(harness.store.source.active_sync().await.unwrap().is_none());
        assert_eq!(
            harness
                .store
                .source
                .configuration()
                .await
                .unwrap()
                .unwrap()
                .next_poll_at,
            Some(at(89))
        );

        let interrupted = harness
            .store
            .source
            .begin_sync(begin(13, version_two, 90, SourceSyncRequest::Poll))
            .await
            .unwrap()
            .sync;
        let interrupted = harness
            .store
            .source
            .fail_interrupted_sync(interrupted.source_sync_id, interrupted.version, at(91))
            .await
            .unwrap();
        assert_eq!(interrupted.outcome, Some(SourceSyncOutcome::Failed));
        assert_eq!(
            interrupted.failure_code,
            Some(SourceSyncFailureCode::Interrupted)
        );
        assert!(harness.store.source.active_sync().await.unwrap().is_none());

        let page = harness.store.source.list_syncs(None, 1).await.unwrap();
        assert_eq!(page.syncs.len(), 1);
        assert!(page.next_cursor.is_some());
        assert_eq!(
            harness.store.source.status().await.unwrap().latest_sync,
            Some(interrupted)
        );

        let (_root, path) = harness.stop().await;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .read_only(true);
        let mut connection = sqlx::SqliteConnection::connect_with(&options)
            .await
            .unwrap();
        let observed_posts: i64 = sqlx::query_scalar("SELECT count(*) FROM post_revisions")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        assert_eq!(observed_posts, 1);
        connection.close().await.unwrap();
    }

    #[tokio::test]
    async fn poll_history_cannot_evict_an_operation_with_a_retained_manual_alias() {
        let harness = Harness::start().await;
        let configuration_version = harness
            .store
            .source
            .put_configuration(PutSourceConfiguration {
                request: configuration_request(None, "main"),
                occurred_at: at(10),
                audit: audit(harness.owner, 20),
            })
            .await
            .unwrap()
            .configuration
            .version;
        let installed = install_sync(&harness.store.source, 100, configuration_version, 20).await;

        let oldest_manual_audit = audit(harness.owner, 21);
        let oldest_manual = fail_sync(
            &harness.store.source,
            101,
            configuration_version,
            100,
            SourceSyncRequest::Manual {
                audit: oldest_manual_audit.clone(),
            },
        )
        .await;
        let evicted_poll = fail_sync(
            &harness.store.source,
            102,
            configuration_version,
            110,
            SourceSyncRequest::Poll,
        )
        .await;
        let retained_poll_one = fail_sync(
            &harness.store.source,
            103,
            configuration_version,
            120,
            SourceSyncRequest::Poll,
        )
        .await;
        let retained_poll_two = fail_sync(
            &harness.store.source,
            104,
            configuration_version,
            130,
            SourceSyncRequest::Poll,
        )
        .await;
        let active = harness
            .store
            .source
            .begin_sync(begin(
                105,
                configuration_version,
                140,
                SourceSyncRequest::Poll,
            ))
            .await
            .unwrap()
            .sync;

        let (root, path) = harness.stop().await;
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        let mut transaction = connection.begin().await.unwrap();
        assert!(prune_source_sync_history(&mut transaction, 2).await.is_ok());

        let operation_ids: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT source_sync_id FROM source_sync_operations ORDER BY requested_at_ns",
        )
        .fetch_all(&mut *transaction)
        .await
        .unwrap();
        let expected_operation_ids = [
            &installed,
            &oldest_manual,
            &retained_poll_one,
            &retained_poll_two,
            &active,
        ]
        .map(|sync| sync.source_sync_id.as_uuid().as_bytes().to_vec())
        .to_vec();
        assert_eq!(operation_ids, expected_operation_ids);
        let oldest_manual_alias_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM source_sync_idempotency_aliases WHERE audit_event_id = ?",
        )
        .bind(
            oldest_manual_audit
                .audit_event_id
                .as_uuid()
                .as_bytes()
                .as_slice(),
        )
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
        assert_eq!(oldest_manual_alias_count, 1);
        let oldest_manual_audit_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM admin_audit_events WHERE audit_event_id = ?")
                .bind(
                    oldest_manual_audit
                        .audit_event_id
                        .as_uuid()
                        .as_bytes()
                        .as_slice(),
                )
                .fetch_one(&mut *transaction)
                .await
                .unwrap();
        assert_eq!(oldest_manual_audit_count, 1);
        transaction.commit().await.unwrap();
        connection.close().await.unwrap();

        let database = database::bootstrap(database_configuration(&path))
            .await
            .unwrap();
        let (store, writer) = database.into_store(32);
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let writer = tokio::spawn(async move {
            writer.run(task_shutdown).await.unwrap();
        });
        let retry_id = SourceSyncId::from_uuid(Uuid::from_u128(106));
        let replay = store
            .source
            .begin_sync(BeginSourceSync {
                proposed_source_sync_id: retry_id,
                expected_configuration_version: configuration_version,
                requested_at: at(150),
                request: SourceSyncRequest::Manual {
                    audit: oldest_manual_audit,
                },
            })
            .await
            .unwrap();
        assert_eq!(replay.admission, SourceSyncAdmission::Replayed);
        assert_eq!(replay.sync, oldest_manual);
        assert!(store.source.sync(retry_id).await.unwrap().is_none());
        assert_eq!(
            store
                .source
                .sync(oldest_manual.source_sync_id)
                .await
                .unwrap(),
            Some(oldest_manual)
        );
        assert!(
            store
                .source
                .sync(evicted_poll.source_sync_id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .source
                .installation()
                .await
                .unwrap()
                .unwrap()
                .source_sync_id,
            installed.source_sync_id
        );
        assert_eq!(store.source.active_sync().await.unwrap(), Some(active));

        drop(store);
        shutdown.cancel();
        writer.await.unwrap();
        drop(root);
    }

    #[tokio::test]
    async fn alias_replay_window_bounds_keys_that_coalesce_onto_one_active_sync() {
        let harness = Harness::start().await;
        let owner = harness.owner;
        let configuration_version = harness
            .store
            .source
            .put_configuration(PutSourceConfiguration {
                request: configuration_request(None, "main"),
                occurred_at: at(10),
                audit: audit(owner, 30),
            })
            .await
            .unwrap()
            .configuration
            .version;
        let active = harness
            .store
            .source
            .begin_sync(begin(
                200,
                configuration_version,
                20,
                SourceSyncRequest::Poll,
            ))
            .await
            .unwrap()
            .sync;
        let oldest_audit = audit(owner, 31);
        for (id, second, request_audit) in [
            (201, 21, oldest_audit.clone()),
            (202, 22, audit(owner, 32)),
            (203, 23, audit(owner, 33)),
        ] {
            let admitted = harness
                .store
                .source
                .begin_sync(begin(
                    id,
                    configuration_version,
                    second,
                    SourceSyncRequest::Manual {
                        audit: request_audit,
                    },
                ))
                .await
                .unwrap();
            assert_eq!(admitted.admission, SourceSyncAdmission::Coalesced);
            assert_eq!(admitted.sync.source_sync_id, active.source_sync_id);
        }

        let (root, path) = harness.stop().await;
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        let mut transaction = connection.begin().await.unwrap();
        assert!(prune_source_sync_aliases(&mut transaction, 2).await.is_ok());
        let alias_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM source_sync_idempotency_aliases")
                .fetch_one(&mut *transaction)
                .await
                .unwrap();
        assert_eq!(alias_count, 2);
        let oldest_alias_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(\
                SELECT 1 FROM source_sync_idempotency_aliases WHERE idempotency_key = ?\
             )",
        )
        .bind(oldest_audit.idempotency_key.0.as_bytes().as_slice())
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
        assert!(!oldest_alias_exists);
        let audit_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM admin_audit_events WHERE action = ? AND outcome = 'succeeded'",
        )
        .bind(MANUAL_SYNC_ACTION)
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
        assert_eq!(audit_count, 3);
        transaction.commit().await.unwrap();
        connection.close().await.unwrap();

        let database = database::bootstrap(database_configuration(&path))
            .await
            .unwrap();
        let (store, writer) = database.into_store(32);
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let writer = tokio::spawn(async move {
            writer.run(task_shutdown).await.unwrap();
        });
        let retry = store
            .source
            .begin_sync(begin(
                204,
                configuration_version,
                24,
                SourceSyncRequest::Manual {
                    audit: oldest_audit,
                },
            ))
            .await;
        assert_eq!(
            retry,
            Err(DatabaseMutationError::Command(
                DatabaseCommandError::IdempotencyConflict
            ))
        );
        assert_eq!(store.source.active_sync().await.unwrap(), Some(active));

        drop(store);
        shutdown.cancel();
        writer.await.unwrap();
        drop(root);
    }
}
