//! Typed inventories for complete, encrypted Litestream file-replica checkpoints.

use super::{
    ArtifactIdentity, FileIdentity, HostConfigurationLoader, MAX_BINARY_BYTES, MAX_DATABASE_BYTES,
    MAX_MARKER_BYTES, RestoreError, RestoreSchema, artifact_inventory, checked_file,
    create_private, database, file_identity, reject_symlink_components, require_no_sidecars,
    require_private_file, restore_failure, sync_directory, verify_identities,
    verify_restore_content,
};
use crate::error::ProcessError;
use markdown_compiler::ContentTreeLimits;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};
use time::{OffsetDateTime, UtcOffset};

const MAX_LTX_FILES: usize = 4096;
const MAX_LTX_FILE_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const MAX_LTX_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Txid(u64);

impl<'de> Deserialize<'de> for Txid {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Box::<str>::deserialize(deserializer)?;
        if value.len() != 16
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(D::Error::custom(
                "transaction identity must contain 16 lowercase hexadecimal digits",
            ));
        }
        let value = u64::from_str_radix(&value, 16).map_err(D::Error::custom)?;
        if value == 0 {
            return Err(D::Error::custom("transaction identity must be positive"));
        }
        Ok(Self(value))
    }
}
impl Serialize for Txid {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{:016x}", self.0))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestorePlan {
    source: Box<str>,
    target_path: Box<str>,
    replica: FileReplica,
    min_txid: Txid,
    max_txid: Txid,
    files: Vec<RestorePlanFile>,
}
#[derive(Deserialize)]
enum FileReplica {
    #[serde(rename = "file")]
    File,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestorePlanFile {
    level: u8,
    name: Box<str>,
    min_txid: Txid,
    max_txid: Txid,
    size: u64,
    #[serde(with = "time::serde::rfc3339")]
    timestamp: OffsetDateTime,
}

impl RestorePlanFile {
    fn capture(
        self,
        root: &Path,
        remaining_bytes: &mut u64,
    ) -> Result<CheckpointFile, RestoreError> {
        validate_file_name(self.level, &self.name, self.min_txid, self.max_txid)?;
        if self.size == 0 || self.size > MAX_LTX_FILE_BYTES {
            return Err(RestoreError::Limit);
        }
        *remaining_bytes = remaining_bytes
            .checked_sub(self.size)
            .ok_or(RestoreError::Limit)?;
        let file = file_identity(
            &root.join(self.level.to_string()).join(self.name.as_ref()),
            self.size,
        )?;
        if file.bytes != self.size {
            return Err(RestoreError::Integrity);
        }
        Ok(CheckpointFile {
            level: self.level,
            name: self.name,
            min_txid: self.min_txid,
            max_txid: self.max_txid,
            timestamp: self.timestamp.to_offset(UtcOffset::UTC),
            file,
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReplicaCheckpoint {
    format: CheckpointFormat,
    #[serde(with = "time::serde::rfc3339")]
    captured_at: OffsetDateTime,
    schema: RestoreSchema,
    binary: FileIdentity,
    database: FileIdentity,
    min_txid: Txid,
    max_txid: Txid,
    files: Vec<CheckpointFile>,
    artifacts: Vec<ArtifactIdentity>,
}
#[derive(Deserialize, Serialize)]
enum CheckpointFormat {
    #[serde(rename = "maincopy-litestream-checkpoint-v1")]
    V1,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CheckpointFile {
    level: u8,
    name: Box<str>,
    min_txid: Txid,
    max_txid: Txid,
    #[serde(with = "time::serde::rfc3339")]
    timestamp: OffsetDateTime,
    file: FileIdentity,
}

pub(crate) async fn write_manifest(
    config_path: PathBuf,
    database_file: PathBuf,
    plan_file: PathBuf,
    ltx_root: PathBuf,
    artifact_root: PathBuf,
    output: PathBuf,
) -> Result<(), ProcessError> {
    let host = HostConfigurationLoader::from_process_working_directory()?.load(&config_path)?;
    let result = async {
        let inputs = (
            database_file.clone(),
            ltx_root.clone(),
            artifact_root.clone(),
        );
        let (manifest, database_identity) = tokio::task::spawn_blocking(move || {
            require_private_file(&inputs.0)?;
            require_no_sidecars(&inputs.0)?;
            let database_identity = file_identity(&inputs.0, MAX_DATABASE_BYTES)?;
            let plan: RestorePlan = serde_json::from_slice(&read_manifest(&plan_file)?)?;
            let manifest = ReplicaCheckpoint::from_plan(
                plan,
                &inputs.1,
                &inputs.2,
                database_identity.clone(),
            )?;
            Ok::<_, RestoreError>((manifest, database_identity))
        })
        .await??;
        verify_replayed_content(&database_file, &artifact_root, host.view().content_limits).await?;
        tokio::task::spawn_blocking(move || {
            if file_identity(&database_file, MAX_DATABASE_BYTES)? != database_identity {
                return Err(RestoreError::MarkerMismatch);
            }
            manifest.verify_artifacts(&artifact_root)?;
            manifest.verify_ltx(&ltx_root)?;
            manifest.write_new(&output)
        })
        .await??;
        Ok::<_, RestoreError>(())
    }
    .await;
    result.map_err(restore_failure)
}

/// Own the complete read-only inspection lifetime; no reader survives publication.
async fn verify_replayed_content(
    database_file: &Path,
    artifact_root: &Path,
    limits: ContentTreeLimits,
) -> Result<(), RestoreError> {
    let state_root = artifact_root
        .parent()
        .filter(|_| {
            artifact_root
                .file_name()
                .is_some_and(|name| name == super::ARTIFACT_DIRECTORY)
        })
        .ok_or(RestoreError::UnsafePath)?;
    let inspected = database::restore::inspect(database_file).await?;
    let content = async {
        verify_identities(&inspected.store).await?;
        verify_restore_content(&inspected.store, state_root, limits)
            .await
            .map_err(|error| RestoreError::Content(Box::new(error)))
    }
    .await;
    inspected.close().await;
    content
}

pub(crate) async fn verify_checkpoint(
    manifest_file: PathBuf,
    ltx_root: PathBuf,
    artifact_root: PathBuf,
) -> Result<(), ProcessError> {
    let manifest = tokio::task::spawn_blocking(move || {
        ReplicaCheckpoint::load(&manifest_file, &ltx_root, &artifact_root)
    })
    .await
    .map_err(|error| restore_failure(error.into()))?
    .map_err(restore_failure)?;
    #[derive(Serialize)]
    struct Verified {
        max_txid: Txid,
    }
    let output = serde_json::to_string(&Verified {
        max_txid: manifest.max_txid,
    })
    .map_err(|error| restore_failure(error.into()))?;
    println!("{output}");
    Ok(())
}

impl ReplicaCheckpoint {
    fn write_new(&self, output: &Path) -> Result<(), RestoreError> {
        let encoded = serde_json::to_vec(self)?;
        if encoded.len() as u64 > MAX_MARKER_BYTES {
            return Err(RestoreError::Limit);
        }
        reject_symlink_components(output).map_err(|_| RestoreError::UnsafePath)?;
        let mut file = create_private(output)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        sync_directory(output.parent().ok_or(RestoreError::UnsafePath)?)?;
        Ok(())
    }
    fn from_plan(
        plan: RestorePlan,
        ltx_root: &Path,
        artifact_root: &Path,
        database: FileIdentity,
    ) -> Result<Self, RestoreError> {
        if plan.source.len() > 4096 || plan.target_path.len() > 4096 {
            return Err(RestoreError::Limit);
        }
        match plan.replica {
            FileReplica::File => {}
        }
        if plan.files.is_empty() || plan.files.len() > MAX_LTX_FILES {
            return Err(RestoreError::Limit);
        }
        let mut remaining_bytes = MAX_LTX_TOTAL_BYTES;
        let files = plan
            .files
            .into_iter()
            .map(|entry| entry.capture(ltx_root, &mut remaining_bytes))
            .collect::<Result<_, RestoreError>>()?;
        let manifest = Self {
            format: CheckpointFormat::V1,
            captured_at: OffsetDateTime::now_utc(),
            schema: database::restore::binary_schema(),
            binary: file_identity(&std::env::current_exe()?, MAX_BINARY_BYTES)?,
            database,
            min_txid: plan.min_txid,
            max_txid: plan.max_txid,
            files,
            artifacts: artifact_inventory(artifact_root)?,
        };
        manifest.validate_plan()?;
        manifest.verify_ltx(ltx_root)?;
        Ok(manifest)
    }

    pub(super) fn load(
        path: &Path,
        ltx_root: &Path,
        artifact_root: &Path,
    ) -> Result<Self, RestoreError> {
        let manifest: Self = serde_json::from_slice(&read_manifest(path)?)?;
        manifest.validate_plan()?;
        manifest.verify_artifacts(artifact_root)?;
        manifest.verify_ltx(ltx_root)?;
        Ok(manifest)
    }

    pub(super) fn verify_database(&self, path: &Path) -> Result<FileIdentity, RestoreError> {
        if file_identity(path, MAX_DATABASE_BYTES)? != self.database {
            return Err(RestoreError::MarkerMismatch);
        }
        Ok(self.database.clone())
    }

    pub(super) fn verify_artifacts(&self, artifact_root: &Path) -> Result<(), RestoreError> {
        if self.captured_at.offset() != UtcOffset::UTC
            || self.schema != database::restore::binary_schema()
            || self.binary != file_identity(&std::env::current_exe()?, MAX_BINARY_BYTES)?
            || self.artifacts != artifact_inventory(artifact_root)?
        {
            return Err(RestoreError::MarkerMismatch);
        }
        Ok(())
    }

    fn validate_plan(&self) -> Result<(), RestoreError> {
        if self.files.is_empty()
            || self.files.len() > MAX_LTX_FILES
            || self.database.bytes == 0
            || self.database.bytes > MAX_DATABASE_BYTES
        {
            return Err(RestoreError::Limit);
        }
        if self.min_txid != Txid(1) {
            return Err(RestoreError::Integrity);
        }
        let mut previous_max = 0_u64;
        let mut total_bytes = 0_u64;
        for entry in &self.files {
            validate_file_name(entry.level, &entry.name, entry.min_txid, entry.max_txid)?;
            if entry.min_txid.0 > previous_max.saturating_add(1)
                || entry.max_txid.0 <= previous_max
                || entry.timestamp.offset() != UtcOffset::UTC
            {
                return Err(RestoreError::Integrity);
            }
            if entry.file.bytes == 0 || entry.file.bytes > MAX_LTX_FILE_BYTES {
                return Err(RestoreError::Limit);
            }
            total_bytes = total_bytes
                .checked_add(entry.file.bytes)
                .filter(|total| *total <= MAX_LTX_TOTAL_BYTES)
                .ok_or(RestoreError::Limit)?;
            previous_max = entry.max_txid.0;
        }
        if Txid(previous_max) != self.max_txid {
            return Err(RestoreError::Integrity);
        }
        Ok(())
    }

    fn verify_ltx(&self, root: &Path) -> Result<(), RestoreError> {
        let expected: BTreeSet<_> = self
            .files
            .iter()
            .map(|file| (file.level, file.name.to_string()))
            .collect();
        if ltx_inventory(root)? != expected {
            return Err(RestoreError::MarkerMismatch);
        }
        for entry in &self.files {
            let path = root.join(entry.level.to_string()).join(entry.name.as_ref());
            if file_identity(&path, MAX_LTX_FILE_BYTES)? != entry.file {
                return Err(RestoreError::MarkerMismatch);
            }
        }
        Ok(())
    }
}

fn read_manifest(path: &Path) -> Result<Vec<u8>, RestoreError> {
    require_private_file(path)?;
    let mut bytes = Vec::new();
    checked_file(path, MAX_MARKER_BYTES)?
        .take(MAX_MARKER_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MARKER_BYTES {
        return Err(RestoreError::Limit);
    }
    Ok(bytes)
}

fn validate_file_name(
    level: u8,
    name: &str,
    min_txid: Txid,
    max_txid: Txid,
) -> Result<(), RestoreError> {
    if level > 9
        || min_txid > max_txid
        || name != format!("{:016x}-{:016x}.ltx", min_txid.0, max_txid.0)
    {
        return Err(RestoreError::UnsafePath);
    }
    Ok(())
}

fn ltx_inventory(root: &Path) -> Result<BTreeSet<(u8, String)>, RestoreError> {
    reject_symlink_components(root).map_err(|_| RestoreError::UnsafePath)?;
    let mut inventory = BTreeSet::new();
    for level in fs::read_dir(root)? {
        let level = level?;
        let name = level.file_name();
        let name = name.to_str().ok_or(RestoreError::UnsafePath)?;
        if name.len() != 1 || !name.as_bytes()[0].is_ascii_digit() || !level.file_type()?.is_dir() {
            return Err(RestoreError::UnsafePath);
        }
        let number = name.as_bytes()[0] - b'0';
        for entry in fs::read_dir(level.path())? {
            let entry = entry?;
            if inventory.len() >= MAX_LTX_FILES {
                return Err(RestoreError::Limit);
            }
            if !entry.file_type()?.is_file() {
                return Err(RestoreError::UnsafePath);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| RestoreError::UnsafePath)?;
            inventory.insert((number, name));
        }
    }
    Ok(inventory)
}

#[cfg(test)]
mod tests {
    use super::super::prepare_private_directory;
    use super::*;
    use serde_json::json;

    fn fixture_database_identity() -> FileIdentity {
        FileIdentity {
            bytes: b"native replay bytes".len() as u64,
            digest: *blake3::hash(b"native replay bytes").as_bytes(),
        }
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, serde_json::Value) {
        let root = tempfile::tempdir().unwrap();
        let ltx = root.path().join("ltx");
        prepare_private_directory(&ltx.join("9")).unwrap();
        prepare_private_directory(&ltx.join("0")).unwrap();
        let artifacts = root.path().join("content-candidates");
        prepare_private_directory(&artifacts).unwrap();
        let plan = json!({"source":"file:/private/spool","target_path":"/private/recovered.db","replica":"file",
        "min_txid":"0000000000000001","max_txid":"0000000000000004",
        "files":[
            {"level":9,"name":"0000000000000001-0000000000000002.ltx","min_txid":"0000000000000001","max_txid":"0000000000000002","size":8,"timestamp":"2026-09-06T12:00:00Z"},
            {"level":0,"name":"0000000000000002-0000000000000004.ltx","min_txid":"0000000000000002","max_txid":"0000000000000004","size":8,"timestamp":"2026-09-06T12:00:01Z"}
        ]});
        for file in plan["files"].as_array().unwrap() {
            create_private(
                &ltx.join(file["level"].to_string())
                    .join(file["name"].as_str().unwrap()),
            )
            .unwrap()
            .write_all(b"LTXbytes")
            .unwrap();
        }
        let artifact = artifacts.join(format!("content-b3-v1-{}.candidate", "a".repeat(64)));
        create_private(&artifact)
            .unwrap()
            .write_all(b"retained archive")
            .unwrap();
        (root, ltx, artifacts, plan)
    }

    #[tokio::test]
    async fn checkpoint_commands_bind_exact_pinned_files_and_return_the_native_cutoff() {
        let (root, ltx, artifacts, plan) = fixture();
        let plan_file = root.path().join("plan.json");
        create_private(&plan_file)
            .unwrap()
            .write_all(&serde_json::to_vec(&plan).unwrap())
            .unwrap();
        let output = root.path().join("checkpoint.json");
        let parsed: RestorePlan =
            serde_json::from_slice(&read_manifest(&plan_file).unwrap()).unwrap();
        ReplicaCheckpoint::from_plan(parsed, &ltx, &artifacts, fixture_database_identity())
            .unwrap()
            .write_new(&output)
            .unwrap();
        let manifest = ReplicaCheckpoint::load(&output, &ltx, &artifacts).unwrap();
        assert_eq!(manifest.max_txid, Txid(4));
        assert_eq!(manifest.files.len(), 2);
        verify_checkpoint(output.clone(), ltx.clone(), artifacts.clone())
            .await
            .unwrap();
        let mut altered: serde_json::Value =
            serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
        assert_eq!(
            altered["files"][0]["file"]["digest"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        altered["binary"]["digest"] = json!("0".repeat(64));
        let wrong_binary = root.path().join("wrong-binary.json");
        create_private(&wrong_binary)
            .unwrap()
            .write_all(&serde_json::to_vec(&altered).unwrap())
            .unwrap();
        assert!(matches!(
            ReplicaCheckpoint::load(&wrong_binary, &ltx, &artifacts),
            Err(RestoreError::MarkerMismatch)
        ));
        fs::write(
            ltx.join("0/0000000000000002-0000000000000004.ltx"),
            b"changed!",
        )
        .unwrap();
        assert!(matches!(
            ReplicaCheckpoint::load(&output, &ltx, &artifacts),
            Err(RestoreError::MarkerMismatch)
        ));
        fs::write(
            ltx.join("0/0000000000000002-0000000000000004.ltx"),
            b"LTXbytes",
        )
        .unwrap();
        let extra = ltx.join("0/0000000000000005-0000000000000005.ltx");
        create_private(&extra)
            .unwrap()
            .write_all(b"LTXbytes")
            .unwrap();
        assert!(matches!(
            ReplicaCheckpoint::load(&output, &ltx, &artifacts),
            Err(RestoreError::MarkerMismatch)
        ));
        fs::remove_file(extra).unwrap();
        fs::remove_file(
            fs::read_dir(&artifacts)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(matches!(
            ReplicaCheckpoint::load(&output, &ltx, &artifacts),
            Err(RestoreError::MarkerMismatch)
        ));
    }

    #[test]
    fn replica_acceptance_rechecks_copied_database_and_candidate_bytes() {
        use super::super::RestoreManifest;
        let (root, ltx, artifacts, plan) = fixture();
        let manifest = ReplicaCheckpoint::from_plan(
            serde_json::from_value(plan).unwrap(),
            &ltx,
            &artifacts,
            fixture_database_identity(),
        )
        .unwrap();
        let manifest_path = root.path().join("checkpoint.json");
        create_private(&manifest_path)
            .unwrap()
            .write_all(&serde_json::to_vec(&manifest).unwrap())
            .unwrap();
        let database = root.path().join("native-replay.db");
        create_private(&database)
            .unwrap()
            .write_all(b"native replay bytes")
            .unwrap();
        fs::write(&database, b"a different otherwise valid cutoff").unwrap();
        assert!(matches!(
            RestoreManifest::Replica {
                path: manifest_path.clone(),
                ltx_root: ltx.clone()
            }
            .verify(&database, &artifacts),
            Err(RestoreError::MarkerMismatch)
        ));
        fs::write(&database, b"native replay bytes").unwrap();
        let verified = RestoreManifest::Replica {
            path: manifest_path,
            ltx_root: ltx,
        }
        .verify(&database, &artifacts)
        .unwrap();
        let copy = root.path().join("copied.db");
        fs::copy(&database, &copy).unwrap();
        verified.verify_copy(&copy, &artifacts).unwrap();
        fs::write(&copy, b"changed after verification").unwrap();
        assert!(matches!(
            verified.verify_copy(&copy, &artifacts),
            Err(RestoreError::MarkerMismatch)
        ));
        fs::copy(&database, &copy).unwrap();
        let artifact = fs::read_dir(&artifacts)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::write(artifact, b"changed after verification").unwrap();
        assert!(matches!(
            verified.verify_copy(&copy, &artifacts),
            Err(RestoreError::MarkerMismatch)
        ));
    }

    #[test]
    fn native_plan_rejects_traversal_gaps_invalid_bounds_and_unpinned_files() {
        let (_root, ltx, artifacts, plan) = fixture();
        for (field, value) in [
            ("name", json!("../../private-key")),
            ("level", json!(10)),
            ("min_txid", json!("0000000000000005")),
            ("max_txid", json!("0000000000000001")),
            ("size", json!(9)),
        ] {
            let mut altered = plan.clone();
            altered["files"][1][field] = value;
            let altered: RestorePlan = serde_json::from_value(altered).unwrap();
            assert!(
                ReplicaCheckpoint::from_plan(
                    altered,
                    &ltx,
                    &artifacts,
                    fixture_database_identity()
                )
                .is_err()
            );
        }
        let mut manifest = ReplicaCheckpoint::from_plan(
            serde_json::from_value(plan.clone()).unwrap(),
            &ltx,
            &artifacts,
            fixture_database_identity(),
        )
        .unwrap();
        manifest.files[1].min_txid = Txid(4);
        manifest.files[1].name = "0000000000000004-0000000000000004.ltx".into();
        assert!(matches!(
            manifest.validate_plan(),
            Err(RestoreError::Integrity)
        ));
        for value in [
            json!("0000000000000000"),
            json!("000000000000000A"),
            json!("1"),
        ] {
            let mut altered = plan.clone();
            altered["max_txid"] = value;
            assert!(serde_json::from_value::<RestorePlan>(altered).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_inventory_rejects_symlinks_before_reading_their_targets() {
        use std::os::unix::fs::symlink;
        let (root, ltx, artifacts, plan) = fixture();
        let target = root.path().join("secret");
        create_private(&target)
            .unwrap()
            .write_all(b"sensitive")
            .unwrap();
        let entry = ltx.join("0/0000000000000002-0000000000000004.ltx");
        fs::remove_file(&entry).unwrap();
        symlink(&target, &entry).unwrap();
        assert!(
            ReplicaCheckpoint::from_plan(
                serde_json::from_value(plan).unwrap(),
                &ltx,
                &artifacts,
                fixture_database_identity()
            )
            .is_err()
        );
        assert_eq!(fs::read(target).unwrap(), b"sensitive");
    }
}
