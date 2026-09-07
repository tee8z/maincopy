//! Offline acceptance and one-use startup verification of restored state.

pub(crate) mod checkpoint;

use crate::{
    config::HostConfigurationLoader,
    database::{self, DatabaseStartupError},
    domain::{
        auth::store::{AuthApplyError, AuthCommandError, AuthLoadError},
        mail::{
            store::{CampaignApplyError, CampaignCommandError, CampaignLoadError},
            subscriber::{
                SubscriberCommandError,
                store::{SubscriberApplyError, SubscriberLoadError},
            },
        },
        profile::store::ProfileLoadError,
    },
    error::{ApplicationError, ProcessError, StartupStage},
    process_lock::{
        ProcessLock, ProcessLockError, prepare_private_directory, reject_symlink_components,
    },
    startup::verify_restore_content,
};
use maincopy_shared::source::valid_source_content_digest;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;
use uuid::Uuid;

const MAX_DATABASE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ARTIFACT_ENTRIES: usize = 4096;
const MAX_MARKER_BYTES: u64 = 2 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const ARTIFACT_DIRECTORY: &str = "content-candidates";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RestoreSchema {
    pub(crate) version: i64,
    #[serde(with = "digest_encoding")]
    pub(crate) digest: [u8; 32],
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct FileIdentity {
    bytes: u64,
    #[serde(with = "digest_encoding")]
    digest: [u8; 32],
}

mod digest_encoding {
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};
    pub(super) fn serialize<S: Serializer>(
        digest: &[u8; 32],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let encoded: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        serializer.serialize_str(&encoded)
    }
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<[u8; 32], D::Error> {
        let encoded = Box::<str>::deserialize(deserializer)?;
        if encoded.len() != 64
            || !encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(D::Error::custom(
                "digest must contain 64 lowercase hexadecimal digits",
            ));
        }
        let mut digest = [0_u8; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&encoded[index * 2..index * 2 + 2], 16)
                .map_err(D::Error::custom)?;
        }
        Ok(digest)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ArtifactIdentity {
    name: Box<str>,
    file: FileIdentity,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum RestoreFormat {
    #[serde(rename = "maincopy-restore-v1")]
    V1,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RestoreMarker {
    format: RestoreFormat,
    restore_id: Uuid,
    database: FileIdentity,
    schema: RestoreSchema,
    binary: FileIdentity,
    artifacts: Vec<ArtifactIdentity>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BackupManifest {
    format: BackupFormat,
    #[serde(with = "time::serde::rfc3339")]
    captured_at: time::OffsetDateTime,
    database: FileIdentity,
    schema: RestoreSchema,
    binary: FileIdentity,
    artifacts: Vec<ArtifactIdentity>,
}

#[derive(Debug, Deserialize, Serialize)]
enum BackupFormat {
    #[serde(rename = "maincopy-backup-v1")]
    V1,
}

#[derive(Debug, Error)]
pub(crate) enum RestoreError {
    #[error("offline restore acceptance could not finish truncating the SQLite WAL")]
    CheckpointIncomplete,
    #[error("restored subscriber state is invalid")]
    SubscriberLoad(#[from] SubscriberLoadError),
    #[error("restored subscriber eligibility discard was rejected")]
    Subscriber(SubscriberCommandError),
    #[error("restored campaign state is invalid")]
    CampaignLoad(#[from] CampaignLoadError),
    #[error("restored campaign quarantine was rejected")]
    Campaign(CampaignCommandError),
    #[error("the restore destination must be empty and use the configured state directory")]
    DestinationNotEmpty,
    #[error("restore input paths must be regular files in protected directories")]
    UnsafePath,
    #[error("restore input exceeds its byte or entry limit")]
    Limit,
    #[error("the replica must be one complete database without SQLite sidecar files")]
    Sidecars,
    #[error("the restored schema does not exactly match this binary")]
    SchemaMismatch,
    #[error("the restored ledger or artifact inventory failed integrity checks")]
    Integrity,
    #[error("restore acceptance is incomplete; normal startup cannot change this candidate")]
    PendingAcceptance,
    #[error("the restore marker does not match these database, artifact, or binary bytes")]
    MarkerMismatch,
    #[error("the restore process could not obtain exclusive ownership")]
    Ownership(#[from] ProcessLockError),
    #[error("restore filesystem access failed")]
    Io(#[from] std::io::Error),
    #[error("the restore marker is invalid")]
    Json(#[from] serde_json::Error),
    #[error("restored SQLite state could not be inspected")]
    Sql(#[from] sqlx::Error),
    #[error("restored database preflight failed")]
    Database(#[source] Box<DatabaseStartupError>),
    #[error("restored identities are invalid")]
    IdentityLoad(#[from] AuthLoadError),
    #[error("restored profile state is invalid")]
    Profile(#[from] ProfileLoadError),
    #[error("restored identity invalidation was rejected")]
    Identity(#[source] AuthCommandError),
    #[error("restored content does not reproduce the durable publication")]
    Content(#[source] Box<ProcessError>),
    #[error("the restore worker stopped unexpectedly")]
    Worker(#[from] tokio::task::JoinError),
}

impl From<DatabaseStartupError> for RestoreError {
    fn from(error: DatabaseStartupError) -> Self {
        Self::Database(Box::new(error))
    }
}
impl From<AuthApplyError> for RestoreError {
    fn from(error: AuthApplyError) -> Self {
        match error {
            AuthApplyError::Command(error) => Self::Identity(error),
            AuthApplyError::Operation(error) => Self::Sql(error),
            AuthApplyError::CorruptStoredState => Self::Integrity,
        }
    }
}

impl From<CampaignApplyError> for RestoreError {
    fn from(error: CampaignApplyError) -> Self {
        match error {
            CampaignApplyError::Command(error) => Self::Campaign(error),
            CampaignApplyError::Operation(error) => Self::Sql(error),
            CampaignApplyError::CorruptStoredState => Self::Integrity,
        }
    }
}

impl From<SubscriberApplyError> for RestoreError {
    fn from(error: SubscriberApplyError) -> Self {
        match error {
            SubscriberApplyError::Command(error) => Self::Subscriber(error),
            SubscriberApplyError::Operation(error) => Self::Sql(error),
            SubscriberApplyError::CorruptStoredState => Self::Integrity,
        }
    }
}

pub(crate) enum RestoreManifest {
    Bundle(PathBuf),
    Replica { path: PathBuf, ltx_root: PathBuf },
}

enum VerifiedManifest {
    Bundle(BackupManifest),
    Replica {
        checkpoint: checkpoint::ReplicaCheckpoint,
        database: FileIdentity,
    },
}

impl RestoreManifest {
    fn verify(self, database: &Path, artifacts: &Path) -> Result<VerifiedManifest, RestoreError> {
        match self {
            Self::Bundle(path) => {
                verify_backup_manifest(&path, database, artifacts).map(VerifiedManifest::Bundle)
            }
            Self::Replica { path, ltx_root } => {
                let checkpoint = checkpoint::ReplicaCheckpoint::load(&path, &ltx_root, artifacts)?;
                let database = checkpoint.verify_database(database)?;
                Ok(VerifiedManifest::Replica {
                    checkpoint,
                    database,
                })
            }
        }
    }
}
impl VerifiedManifest {
    fn verify_copy(&self, database: &Path, artifacts: &Path) -> Result<(), RestoreError> {
        match self {
            Self::Bundle(manifest) => verify_backup_inventory(manifest, database, artifacts),
            Self::Replica {
                checkpoint,
                database: expected,
            } => {
                if file_identity(database, MAX_DATABASE_BYTES)? != *expected {
                    return Err(RestoreError::MarkerMismatch);
                }
                checkpoint.verify_artifacts(artifacts)
            }
        }
    }
}

pub(crate) async fn restore(
    config_path: PathBuf,
    database_file: PathBuf,
    artifact_root: PathBuf,
    manifest: RestoreManifest,
) -> Result<(), ProcessError> {
    let host = HostConfigurationLoader::from_process_working_directory()?.load(&config_path)?;
    let view = host.view();
    let _ownership = ProcessLock::acquire(view.runtime_root)
        .map_err(|source| restore_failure(RestoreError::Ownership(source)))?;
    let state_root = view.state_root.to_path_buf();
    let destination = view.database.path.to_path_buf();
    if !destination.starts_with(view.state_root) || destination == view.state_root {
        return Err(restore_failure(RestoreError::UnsafePath));
    }
    let limits = view.content_limits;
    let restored = async {
        let staged_state = state_root.clone();
        let staged_database = destination.clone();
        tokio::task::spawn_blocking(move || {
            let manifest = manifest.verify(&database_file, &artifact_root)?;
            stage_candidate(
                &staged_state,
                &staged_database,
                &database_file,
                &artifact_root,
            )?;
            manifest.verify_copy(&staged_database, &staged_state.join(ARTIFACT_DIRECTORY))
        })
        .await??;
        let _database_ownership = database::restore::acquire_ownership(&destination)?;
        let inspected = database::restore::inspect(&destination).await?;
        let schema = inspected.schema.clone();
        if let Err(error) = verify_identities(&inspected.store).await {
            inspected.close().await;
            return Err(error);
        }
        let verified = verify_restore_content(&inspected.store, &state_root, limits)
            .await
            .map_err(|error| RestoreError::Content(Box::new(error)));
        inspected.close().await;
        verified?;
        let restore_id = Uuid::new_v4();
        database::restore::accept(&destination, restore_id).await?;
        tokio::task::spawn_blocking(move || {
            accept_marker(&state_root, &destination, schema, restore_id)
        })
        .await??;
        Ok::<Uuid, RestoreError>(restore_id)
    }
    .await
    .map_err(restore_failure)?;
    println!(
        "Accepted restored candidate {restored}. Browser sessions and agent credentials were revoked. Start this same binary with this host configuration."
    );
    Ok(())
}

/// The host encrypts stdout directly; this command never writes a plaintext bundle.
pub(crate) async fn export_backup(
    config_path: PathBuf,
    database_file: PathBuf,
) -> Result<(), ProcessError> {
    let host = HostConfigurationLoader::from_process_working_directory()?.load(&config_path)?;
    let view = host.view();
    let state_root = view.state_root.to_path_buf();
    let limits = view.content_limits;
    let result = async {
        require_private_file(&database_file)?;
        require_no_sidecars(&database_file)?;
        let inspected = database::restore::inspect(&database_file).await?;
        let verified = async {
            verify_identities(&inspected.store).await?;
            verify_restore_content(&inspected.store, &state_root, limits)
                .await
                .map_err(|error| RestoreError::Content(Box::new(error)))
        }
        .await;
        inspected.close().await;
        verified?;
        tokio::task::spawn_blocking(move || {
            let stdout = std::io::stdout();
            write_backup_bundle(
                &database_file,
                &state_root.join(ARTIFACT_DIRECTORY),
                stdout.lock(),
            )
        })
        .await?
    }
    .await;
    result.map_err(restore_failure)
}

fn write_backup_bundle(
    database: &Path,
    artifacts: &Path,
    output: impl Write,
) -> Result<(), RestoreError> {
    require_no_sidecars(database)?;
    let manifest = BackupManifest {
        format: BackupFormat::V1,
        captured_at: time::OffsetDateTime::now_utc(),
        database: file_identity(database, MAX_DATABASE_BYTES)?,
        schema: database::restore::binary_schema(),
        binary: file_identity(&std::env::current_exe()?, MAX_BINARY_BYTES)?,
        artifacts: artifact_inventory(artifacts)?,
    };
    let bytes = serde_json::to_vec(&manifest)?;
    if bytes.len() as u64 > MAX_MARKER_BYTES {
        return Err(RestoreError::Limit);
    }
    let mut archive = tar::Builder::new(output);
    append_archive_entry(
        &mut archive,
        "manifest.json",
        bytes.len() as u64,
        bytes.as_slice(),
    )?;
    append_verified_file(
        &mut archive,
        "database.sqlite3",
        database,
        &manifest.database,
    )?;
    for artifact in &manifest.artifacts {
        append_verified_file(
            &mut archive,
            &format!("content-candidates/{}", artifact.name),
            &artifacts.join(artifact.name.as_ref()),
            &artifact.file,
        )?;
    }
    archive.finish()?;
    archive.into_inner()?.flush()?;
    Ok(())
}

fn append_archive_entry(
    output: &mut tar::Builder<impl Write>,
    name: &str,
    bytes: u64,
    input: impl Read,
) -> Result<(), RestoreError> {
    let mut header = tar::Header::new_ustar();
    header.set_size(bytes);
    header.set_mode(0o600);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    output.append_data(&mut header, name, input)?;
    Ok(())
}

fn append_verified_file(
    output: &mut tar::Builder<impl Write>,
    name: &str,
    path: &Path,
    expected: &FileIdentity,
) -> Result<(), RestoreError> {
    let file = checked_file(path, expected.bytes)?;
    let mut input = DigestReader {
        input: file.take(expected.bytes + 1),
        hasher: blake3::Hasher::new(),
        bytes: 0,
    };
    append_archive_entry(output, name, expected.bytes, &mut input)?;
    if input.bytes != expected.bytes || input.hasher.finalize().as_bytes() != &expected.digest {
        return Err(RestoreError::Integrity);
    }
    Ok(())
}

/// Hash the exact bytes consumed by the standard archive writer.
struct DigestReader<Input> {
    input: Input,
    hasher: blake3::Hasher,
    bytes: u64,
}
impl<Input: Read> Read for DigestReader<Input> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let count = self.input.read(output)?;
        self.hasher.update(&output[..count]);
        self.bytes += count as u64;
        Ok(count)
    }
}

fn verify_backup_manifest(
    path: &Path,
    database: &Path,
    artifacts: &Path,
) -> Result<BackupManifest, RestoreError> {
    require_private_file(path)?;
    let mut bytes = Vec::new();
    checked_file(path, MAX_MARKER_BYTES)?
        .take(MAX_MARKER_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MARKER_BYTES {
        return Err(RestoreError::Limit);
    }
    let manifest: BackupManifest = serde_json::from_slice(&bytes)?;
    verify_backup_inventory(&manifest, database, artifacts)?;
    Ok(manifest)
}

fn verify_backup_inventory(
    manifest: &BackupManifest,
    database: &Path,
    artifacts: &Path,
) -> Result<(), RestoreError> {
    if manifest.captured_at.offset() != time::UtcOffset::UTC
        || manifest.schema != database::restore::binary_schema()
        || manifest.binary != file_identity(&std::env::current_exe()?, MAX_BINARY_BYTES)?
        || manifest.database != file_identity(database, MAX_DATABASE_BYTES)?
        || manifest.artifacts != artifact_inventory(artifacts)?
    {
        return Err(RestoreError::MarkerMismatch);
    }
    Ok(())
}

async fn verify_identities(store: &database::DatabaseStore) -> Result<(), RestoreError> {
    let identity = store.auth.identity_state().await?;
    if identity.bootstrap_required || identity.instance.is_none() {
        return Err(RestoreError::Integrity);
    }
    let mut cursor = None;
    let mut completed = false;
    for _ in 0..10000 {
        let page = store.auth.users_page(cursor, 100).await?;
        for user in page.items {
            store.auth.user_credentials(user.user_id).await?;
            store.profiles.profile(user.user_id).await?;
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            completed = true;
            break;
        }
    }
    if !completed {
        return Err(RestoreError::Limit);
    }
    let mut cursor = None;
    for _ in 0..10000 {
        let page = store.auth.agent_credentials_page(cursor, 100).await?;
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(());
        }
    }
    Err(RestoreError::Limit)
}

fn restore_failure(source: RestoreError) -> ProcessError {
    ApplicationError::Startup {
        stage: StartupStage::Database,
        operation: "accept restored operational state",
        source: Box::new(source),
    }
    .into()
}

/// Admitted empty state with a durable guard against premature startup.
struct RestoreDestination<'path> {
    state_root: &'path Path,
    database: &'path Path,
    database_parent: &'path Path,
}

impl<'path> RestoreDestination<'path> {
    fn admit(state_root: &'path Path, database: &'path Path) -> Result<Self, RestoreError> {
        reject_symlink_components(state_root).map_err(|_| RestoreError::UnsafePath)?;
        if !database.starts_with(state_root)
            || database
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(RestoreError::UnsafePath);
        }
        if state_root.try_exists()? && fs::read_dir(state_root)?.next().transpose()?.is_some() {
            return Err(RestoreError::DestinationNotEmpty);
        }
        let database_parent = database.parent().ok_or(RestoreError::UnsafePath)?;
        prepare_private_directory(state_root)?;
        prepare_private_directory(database_parent)?;
        create_private(&sidecar(database, ".restore-pending"))?.sync_all()?;
        sync_directory(database_parent)?;
        Ok(Self {
            state_root,
            database,
            database_parent,
        })
    }

    fn copy(
        self,
        database_file: &Path,
        artifact_root: &Path,
        inventory: Vec<ArtifactIdentity>,
    ) -> Result<(), RestoreError> {
        copy_private(database_file, self.database, MAX_DATABASE_BYTES)?;
        let target = self.state_root.join(ARTIFACT_DIRECTORY);
        prepare_private_directory(&target)?;
        for artifact in inventory {
            copy_private(
                &artifact_root.join(artifact.name.as_ref()),
                &target.join(artifact.name.as_ref()),
                MAX_ARTIFACT_BYTES,
            )?;
        }
        sync_directory(&target)?;
        sync_directory(self.database_parent)?;
        sync_directory(self.state_root)?;
        Ok(())
    }
}

fn stage_candidate(
    state_root: &Path,
    destination: &Path,
    database_file: &Path,
    artifact_root: &Path,
) -> Result<(), RestoreError> {
    require_private_file(database_file)?;
    require_no_sidecars(database_file)?;
    let inventory = artifact_inventory(artifact_root)?;
    RestoreDestination::admit(state_root, destination)?.copy(
        database_file,
        artifact_root,
        inventory,
    )
}

fn accept_marker(
    state_root: &Path,
    database: &Path,
    schema: RestoreSchema,
    restore_id: Uuid,
) -> Result<(), RestoreError> {
    require_no_sidecars(database)?;
    let marker = RestoreMarker {
        format: RestoreFormat::V1,
        restore_id,
        schema,
        database: file_identity(database, MAX_DATABASE_BYTES)?,
        binary: file_identity(&std::env::current_exe()?, MAX_BINARY_BYTES)?,
        artifacts: artifact_inventory(&state_root.join(ARTIFACT_DIRECTORY))?,
    };
    let bytes = serde_json::to_vec(&marker)?;
    if bytes.len() as u64 > MAX_MARKER_BYTES {
        return Err(RestoreError::Limit);
    }
    let mut file = create_private(&sidecar(database, ".restore.json"))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    let parent = database.parent().ok_or(RestoreError::UnsafePath)?;
    sync_directory(parent)?;
    fs::remove_file(sidecar(database, ".restore-pending"))?;
    sync_directory(parent)?;
    Ok(())
}

/// Offline identity/source maintenance must not consume first-start acceptance.
pub(crate) fn reject_unaccepted_candidate(database: &Path) -> Result<(), RestoreError> {
    if entry_exists(&sidecar(database, ".restore-pending"))?
        || entry_exists(&sidecar(database, ".restore.json"))?
    {
        return Err(RestoreError::PendingAcceptance);
    }
    Ok(())
}

pub(crate) async fn verify_startup_candidate(
    database: &Path,
    state_root: &Path,
) -> Result<(), RestoreError> {
    let database = database.to_path_buf();
    let state_root = state_root.to_path_buf();
    tokio::task::spawn_blocking(move || consume_marker(&database, &state_root)).await?
}

fn consume_marker(database: &Path, state_root: &Path) -> Result<(), RestoreError> {
    if entry_exists(&sidecar(database, ".restore-pending"))? {
        return Err(RestoreError::PendingAcceptance);
    }
    let path = sidecar(database, ".restore.json");
    if !entry_exists(&path)? {
        return Ok(());
    }
    let consumed = sidecar(database, ".restore-consumed");
    if entry_exists(&consumed)? {
        return Err(RestoreError::MarkerMismatch);
    }
    let _ownership = database::restore::acquire_ownership(database)?;
    // Another owner can finish consumption between the fast rejection and lock acquisition.
    if entry_exists(&consumed)? {
        return Err(RestoreError::MarkerMismatch);
    }
    let marker = RestoreMarker::read(&path)?;
    marker.verify(database, state_root)?;
    fs::rename(path, consumed)?;
    sync_directory(database.parent().ok_or(RestoreError::UnsafePath)?)?;
    Ok(())
}

impl RestoreMarker {
    fn read(path: &Path) -> Result<Self, RestoreError> {
        require_private_file(path)?;
        let mut bytes = Vec::new();
        checked_file(path, MAX_MARKER_BYTES)?
            .take(MAX_MARKER_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_MARKER_BYTES {
            return Err(RestoreError::Limit);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn verify(&self, database: &Path, state_root: &Path) -> Result<(), RestoreError> {
        require_private_file(database)?;
        require_no_sidecars(database)?;
        if self.database != file_identity(database, MAX_DATABASE_BYTES)?
            || self.binary != file_identity(&std::env::current_exe()?, MAX_BINARY_BYTES)?
            || self.schema != database::restore::binary_schema()
            || self.artifacts != artifact_inventory(&state_root.join(ARTIFACT_DIRECTORY))?
        {
            return Err(RestoreError::MarkerMismatch);
        }
        Ok(())
    }
}

fn artifact_inventory(root: &Path) -> Result<Vec<ArtifactIdentity>, RestoreError> {
    reject_symlink_components(root).map_err(|_| RestoreError::UnsafePath)?;
    let mut inventory = Vec::new();
    let mut total_bytes = 0_u64;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if inventory.len() >= MAX_ARTIFACT_ENTRIES {
            return Err(RestoreError::Limit);
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| RestoreError::UnsafePath)?;
        let digest = name
            .strip_suffix(".candidate")
            .ok_or(RestoreError::UnsafePath)?;
        if !valid_source_content_digest(digest) {
            return Err(RestoreError::UnsafePath);
        }
        let file = file_identity(&entry.path(), MAX_ARTIFACT_BYTES)?;
        total_bytes = total_bytes
            .checked_add(file.bytes)
            .filter(|total| *total <= MAX_ARTIFACT_BYTES)
            .ok_or(RestoreError::Limit)?;
        inventory.push(ArtifactIdentity {
            name: name.into_boxed_str(),
            file,
        });
    }
    inventory.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(inventory)
}

fn checked_file(path: &Path, limit: u64) -> Result<File, RestoreError> {
    reject_symlink_components(path).map_err(|_| RestoreError::UnsafePath)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(RestoreError::UnsafePath);
    }
    if metadata.len() > limit {
        return Err(RestoreError::Limit);
    }
    let file = open_input(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || opened.len() > limit || !same_file(&metadata, &opened) {
        return Err(RestoreError::UnsafePath);
    }
    Ok(file)
}

#[cfg(unix)]
fn open_input(path: &Path) -> Result<File, std::io::Error> {
    use rustix::fs::{Mode, OFlags, open};
    Ok(File::from(open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}
#[cfg(not(unix))]
fn open_input(path: &Path) -> Result<File, std::io::Error> {
    File::open(path)
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len()
}

fn require_private_file(path: &Path) -> Result<(), RestoreError> {
    let file = checked_file(path, MAX_DATABASE_BYTES)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = file.metadata()?;
        if metadata.nlink() != 1
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(RestoreError::UnsafePath);
        }
    }
    Ok(())
}

fn file_identity(path: &Path, limit: u64) -> Result<FileIdentity, RestoreError> {
    let mut file = checked_file(path, limit)?.take(limit + 1);
    let mut hasher = blake3::Hasher::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        if bytes > limit {
            return Err(RestoreError::Limit);
        }
        hasher.update(&buffer[..count]);
    }
    Ok(FileIdentity {
        bytes,
        digest: *hasher.finalize().as_bytes(),
    })
}

fn create_private(path: &Path) -> Result<File, RestoreError> {
    let mut options = OpenOptions::new();
    options.write(true).read(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn copy_private(source: &Path, destination: &Path, limit: u64) -> Result<(), RestoreError> {
    let mut input = checked_file(source, limit)?.take(limit + 1);
    let mut output = create_private(destination)?;
    if std::io::copy(&mut input, &mut output)? > limit {
        return Err(RestoreError::Limit);
    }
    output.sync_all()?;
    Ok(())
}

/// A dangling symlink is still an entry and must not bypass a restore guard.
fn entry_exists(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn sidecar(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    path.into()
}

fn require_no_sidecars(database: &Path) -> Result<(), RestoreError> {
    for suffix in ["-wal", "-shm", "-journal"] {
        if entry_exists(&sidecar(database, suffix))? {
            return Err(RestoreError::Sidecars);
        }
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), RestoreError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("replica.db");
        create_private(&source)
            .unwrap()
            .write_all(b"replica database")
            .unwrap();
        let artifacts = root.path().join("backup-artifacts");
        prepare_private_directory(&artifacts).unwrap();
        let artifact = artifacts.join(format!("content-b3-v1-{}.candidate", "a".repeat(64)));
        create_private(&artifact)
            .unwrap()
            .write_all(b"retained candidate")
            .unwrap();
        let state = root.path().join("restored-state");
        let database = state.join("database/maincopy.db");
        stage_candidate(&state, &database, &source, &artifacts).unwrap();
        (root, state, database)
    }

    #[test]
    fn a_streamed_bundle_binds_its_database_and_exact_artifact_inventory() {
        let (root, state, database) = staged();
        let mut output = Vec::new();
        write_backup_bundle(&database, &state.join(ARTIFACT_DIRECTORY), &mut output).unwrap();
        let recovered = root.path().join("unpacked");
        prepare_private_directory(&recovered).unwrap();
        tar::Archive::new(output.as_slice())
            .unpack(&recovered)
            .unwrap();
        let manifest = recovered.join("manifest.json");
        let copied_database = recovered.join("database.sqlite3");
        let copied_artifacts = recovered.join(ARTIFACT_DIRECTORY);
        verify_backup_manifest(&manifest, &copied_database, &copied_artifacts).unwrap();
        assert_eq!(
            fs::read(&database).unwrap(),
            fs::read(&copied_database).unwrap()
        );
        fs::write(&copied_database, b"a different snapshot").unwrap();
        assert!(matches!(
            verify_backup_manifest(&manifest, &copied_database, &copied_artifacts),
            Err(RestoreError::MarkerMismatch)
        ));
        fs::copy(&database, &copied_database).unwrap();
        let extra = copied_artifacts.join(format!("content-b3-v1-{}.candidate", "f".repeat(64)));
        create_private(&extra)
            .unwrap()
            .write_all(b"unmanifested archive")
            .unwrap();
        assert!(matches!(
            verify_backup_manifest(&manifest, &copied_database, &copied_artifacts),
            Err(RestoreError::MarkerMismatch)
        ));
    }

    #[test]
    fn staged_copy_must_still_match_the_manifest_verified_before_copy() {
        let (root, state, database) = staged();
        let mut output = Vec::new();
        write_backup_bundle(&database, &state.join(ARTIFACT_DIRECTORY), &mut output).unwrap();
        let source = root.path().join("source-bundle");
        prepare_private_directory(&source).unwrap();
        tar::Archive::new(output.as_slice())
            .unpack(&source)
            .unwrap();
        let source_database = source.join("database.sqlite3");
        let source_artifacts = source.join(ARTIFACT_DIRECTORY);
        let manifest_path = source.join("manifest.json");
        let mut manifest =
            verify_backup_manifest(&manifest_path, &source_database, &source_artifacts).unwrap();
        fs::write(&source_database, b"substituted after verification").unwrap();
        let copied_state = root.path().join("copied-state");
        let copied_database = copied_state.join("maincopy.db");
        stage_candidate(
            &copied_state,
            &copied_database,
            &source_database,
            &source_artifacts,
        )
        .unwrap();
        assert!(matches!(
            verify_backup_inventory(
                &manifest,
                &copied_database,
                &copied_state.join(ARTIFACT_DIRECTORY)
            ),
            Err(RestoreError::MarkerMismatch)
        ));
        assert!(matches!(
            reject_unaccepted_candidate(&copied_database),
            Err(RestoreError::PendingAcceptance)
        ));
        fs::copy(&database, &source_database).unwrap();
        manifest.binary.digest[0] ^= 1;
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(matches!(
            verify_backup_manifest(&manifest_path, &source_database, &source_artifacts),
            Err(RestoreError::MarkerMismatch)
        ));
    }

    #[test]
    fn a_pending_candidate_blocks_every_database_open_without_changing_its_bytes() {
        let (_root, state, database) = staged();
        let before = fs::read(&database).unwrap();
        assert!(matches!(
            reject_unaccepted_candidate(&database),
            Err(RestoreError::PendingAcceptance)
        ));
        assert!(matches!(
            consume_marker(&database, &state),
            Err(RestoreError::PendingAcceptance)
        ));
        assert_eq!(fs::read(&database).unwrap(), before);
        assert!(sidecar(&database, ".restore-pending").exists());
    }

    #[test]
    fn accepted_restore_is_consumed_once_and_offline_maintenance_cannot_consume_it() {
        let (_root, state, database) = staged();
        accept_marker(
            &state,
            &database,
            database::restore::binary_schema(),
            Uuid::new_v4(),
        )
        .unwrap();
        assert!(matches!(
            reject_unaccepted_candidate(&database),
            Err(RestoreError::PendingAcceptance)
        ));
        let marker = fs::read(sidecar(&database, ".restore.json")).unwrap();
        consume_marker(&database, &state).unwrap();
        assert!(!sidecar(&database, ".restore.json").exists());
        assert_eq!(
            fs::read(sidecar(&database, ".restore-consumed")).unwrap(),
            marker
        );
        consume_marker(&database, &state).unwrap();
        create_private(&sidecar(&database, ".restore.json"))
            .unwrap()
            .write_all(&marker)
            .unwrap();
        // A consumed receipt is terminal even when another process owns the database.
        // Rejection must not depend on acquiring that process's ownership lock.
        let _live_database = database::restore::acquire_ownership(&database).unwrap();
        let replay = consume_marker(&database, &state);
        assert!(
            matches!(replay, Err(RestoreError::MarkerMismatch)),
            "unexpected consumed-marker replay outcome: {replay:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_acceptance_entries_block_startup_and_offline_maintenance_without_changes() {
        use std::os::unix::fs::symlink;

        for suffix in [".restore-pending", ".restore.json"] {
            let (root, state, database) = staged();
            let before = fs::read(&database).unwrap();
            fs::remove_file(sidecar(&database, ".restore-pending")).unwrap();
            let marker = sidecar(&database, suffix);
            let target = root.path().join("absent-marker-target");
            symlink(&target, &marker).unwrap();
            assert!(matches!(
                reject_unaccepted_candidate(&database),
                Err(RestoreError::PendingAcceptance)
            ));
            let startup = consume_marker(&database, &state);
            match suffix {
                ".restore-pending" => {
                    assert!(matches!(startup, Err(RestoreError::PendingAcceptance)))
                }
                ".restore.json" => assert!(matches!(startup, Err(RestoreError::UnsafePath))),
                _ => unreachable!(),
            }
            assert_eq!(fs::read(&database).unwrap(), before);
            assert_eq!(fs::read_link(&marker).unwrap(), target);
            assert!(!entry_exists(&target).unwrap());
            assert!(!entry_exists(&sidecar(&database, ".restore-consumed")).unwrap());
        }
    }

    #[cfg(unix)]
    #[test]
    fn dangling_consumed_receipt_refuses_replay_without_replacing_either_entry() {
        use std::os::unix::fs::symlink;

        let (root, state, database) = staged();
        accept_marker(
            &state,
            &database,
            database::restore::binary_schema(),
            Uuid::new_v4(),
        )
        .unwrap();
        let before = fs::read(&database).unwrap();
        let marker = sidecar(&database, ".restore.json");
        let accepted = fs::read(&marker).unwrap();
        let consumed = sidecar(&database, ".restore-consumed");
        let target = root.path().join("absent-consumed-target");
        symlink(&target, &consumed).unwrap();
        assert!(matches!(
            consume_marker(&database, &state),
            Err(RestoreError::MarkerMismatch)
        ));
        assert_eq!(fs::read(&database).unwrap(), before);
        assert_eq!(fs::read(&marker).unwrap(), accepted);
        assert_eq!(fs::read_link(&consumed).unwrap(), target);
        assert!(!entry_exists(&target).unwrap());
    }

    #[test]
    fn inaccessible_acceptance_entries_are_errors_instead_of_absent_markers() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("regular-file");
        let before = b"a file cannot contain restore sidecars";
        fs::write(&parent, before).unwrap();
        let database = parent.join("maincopy.db");
        assert!(matches!(
            reject_unaccepted_candidate(&database),
            Err(RestoreError::Io(_))
        ));
        assert!(matches!(
            consume_marker(&database, root.path()),
            Err(RestoreError::Io(_))
        ));
        assert_eq!(fs::read(&parent).unwrap(), before);
    }

    #[test]
    fn acceptance_rejects_changed_database_artifacts_schema_binary_and_sidecars() {
        for mutation in 0..5 {
            let (_root, state, database) = staged();
            accept_marker(
                &state,
                &database,
                database::restore::binary_schema(),
                Uuid::new_v4(),
            )
            .unwrap();
            let marker_path = sidecar(&database, ".restore.json");
            match mutation {
                0 => fs::write(&database, b"changed database").unwrap(),
                1 => {
                    let artifact = fs::read_dir(state.join(ARTIFACT_DIRECTORY))
                        .unwrap()
                        .next()
                        .unwrap()
                        .unwrap()
                        .path();
                    fs::write(artifact, b"changed artifact").unwrap();
                }
                2 | 3 => {
                    let mut marker: RestoreMarker =
                        serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
                    if mutation == 2 {
                        marker.schema.version += 1;
                    } else {
                        marker.binary.digest[0] ^= 1;
                    }
                    fs::write(&marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
                }
                4 => {
                    create_private(&sidecar(&database, "-wal")).unwrap();
                }
                _ => unreachable!(),
            }
            let before = fs::read(&database).unwrap();
            assert!(consume_marker(&database, &state).is_err());
            assert!(marker_path.exists());
            assert!(!sidecar(&database, ".restore-consumed").exists());
            assert_eq!(fs::read(&database).unwrap(), before);
        }
    }

    #[test]
    fn staging_refuses_nonempty_destinations_and_sidecar_dependent_replicas() {
        let (root, state, database) = staged();
        let source = root.path().join("replica.db");
        let artifacts = root.path().join("backup-artifacts");
        let before = fs::read(&database).unwrap();
        assert!(matches!(
            stage_candidate(&state, &database, &source, &artifacts),
            Err(RestoreError::DestinationNotEmpty)
        ));
        assert_eq!(fs::read(&database).unwrap(), before);
        let empty = root.path().join("empty");
        create_private(&sidecar(&source, "-shm")).unwrap();
        assert!(matches!(
            stage_candidate(&empty, &empty.join("maincopy.db"), &source, &artifacts),
            Err(RestoreError::Sidecars)
        ));
        assert!(!empty.exists());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_sqlite_sidecars_prevent_staging_and_acceptance_without_changes() {
        use std::os::unix::fs::symlink;

        for suffix in ["-wal", "-shm", "-journal"] {
            let (root, state, database) = staged();
            accept_marker(
                &state,
                &database,
                database::restore::binary_schema(),
                Uuid::new_v4(),
            )
            .unwrap();
            let before = fs::read(&database).unwrap();
            let marker = sidecar(&database, ".restore.json");
            let accepted = fs::read(&marker).unwrap();
            let source = root.path().join("replica.db");
            let source_before = fs::read(&source).unwrap();
            let source_sidecar = sidecar(&source, suffix);
            let accepted_sidecar = sidecar(&database, suffix);
            let target = root.path().join("absent-sqlite-sidecar-target");
            symlink(&target, &source_sidecar).unwrap();
            symlink(&target, &accepted_sidecar).unwrap();
            let empty = root.path().join("empty-state");
            assert!(matches!(
                stage_candidate(
                    &empty,
                    &empty.join("maincopy.db"),
                    &source,
                    &root.path().join("backup-artifacts"),
                ),
                Err(RestoreError::Sidecars)
            ));
            assert!(matches!(
                consume_marker(&database, &state),
                Err(RestoreError::Sidecars)
            ));
            assert_eq!(fs::read(&database).unwrap(), before);
            assert_eq!(fs::read(&source).unwrap(), source_before);
            assert_eq!(fs::read(&marker).unwrap(), accepted);
            assert_eq!(fs::read_link(&source_sidecar).unwrap(), target);
            assert_eq!(fs::read_link(&accepted_sidecar).unwrap(), target);
            assert!(!entry_exists(&target).unwrap());
            assert!(!entry_exists(&empty).unwrap());
            assert!(!entry_exists(&sidecar(&database, ".restore-consumed")).unwrap());
        }
    }

    #[cfg(unix)]
    #[test]
    fn staging_rejects_shared_plaintext_replicas_and_symlinked_artifacts() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let (root, _, _) = staged();
        let source = root.path().join("replica.db");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            require_private_file(&source),
            Err(RestoreError::UnsafePath)
        ));
        let artifacts = root.path().join("linked-artifacts");
        prepare_private_directory(&artifacts).unwrap();
        symlink(
            &source,
            artifacts.join(format!("content-b3-v1-{}.candidate", "b".repeat(64))),
        )
        .unwrap();
        assert!(matches!(
            artifact_inventory(&artifacts),
            Err(RestoreError::UnsafePath)
        ));
    }
}
