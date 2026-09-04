//! Offline managed-source setup and recovery operations.

use std::{
    ffi::OsString,
    fs::{self, File},
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
use maincopy_shared::auth::{AdminAuditEventId, UserId, UserRole, UserStatus};
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;
use tokio::{io::AsyncReadExt as _, process::Child};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    config::{HostConfigurationLoader, SourceConfigurationView},
    database::{
        self, DatabaseStore,
        store::{DatabaseCommandError, DatabaseMutationError},
    },
    domain::{
        auth::store::{AdminMutationKey, AuditPrincipalReference, MutationAuditContext},
        source::{
            ManagedSourceConfigurationInput,
            store::{PutSourceConfiguration, StoredSourceConfiguration},
        },
    },
    error::{ApplicationError, ProcessError, StartupStage},
    process_lock::{
        ProcessLock, ProcessLockError, open_existing_private_file, prepare_private_directory,
    },
};

const OWNER_PAGE_SIZE: u16 = 100;
const MAX_OWNER_SEARCH_PAGES: usize = 10_000;
const KEYGEN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PRIVATE_KEY_BYTES: u64 = 64 * 1024;
const MAX_PUBLIC_KEY_BYTES: u64 = 16 * 1024;

/// Persists the non-secret managed-source settings while the daemon is stopped.
pub(crate) async fn configure_source(
    config_path: PathBuf,
    request: ManagedSourceConfigurationInput,
    idempotency_key: Uuid,
) -> Result<(), ProcessError> {
    let host = HostConfigurationLoader::from_process_working_directory()?.load(&config_path)?;
    let host_view = host.view();
    let SourceConfigurationView::ManagedGit { credentials, .. } = host_view.source else {
        return Err(ProcessError::ManagedSourceDisabled);
    };
    if !credentials.contains_key(&request.credential_name) {
        return Err(ProcessError::SourceCredentialUnknown);
    }
    let _process_lock = acquire_process_lock(host_view.runtime_root)?;
    let database = match database::bootstrap(host_view.database).await {
        Ok(database) => database,
        Err(database::DatabaseStartupError::AlreadyOwned) => {
            return Err(ProcessError::AlreadyRunning);
        }
        Err(error) => {
            return Err(offline_failure(
                StartupStage::Database,
                "bootstrap the database for source setup",
                error,
            ));
        }
    };
    let (store, writer) = database.into_store(host_view.database.writer_queue_capacity.get());
    let shutdown = CancellationToken::new();
    let writer_task = tokio::spawn(writer.run(shutdown.clone()));

    let outcome = configure_with_store(&store, request, idempotency_key).await;
    shutdown.cancel();
    drop(store);
    let writer_outcome = writer_task.await;
    match writer_outcome {
        Ok(Ok(())) => outcome.map(|configuration| {
            tracing::info!(
                source_configuration_version = configuration.configuration.version.get(),
                "managed source configuration persisted"
            );
        }),
        Ok(Err(error)) => Err(offline_failure(
            StartupStage::Database,
            "stop the sole database writer after source setup",
            error,
        )),
        Err(error) => Err(offline_failure(
            StartupStage::Database,
            "join the sole database writer after source setup",
            error,
        )),
    }
}

/// Generates one dedicated Ed25519 deploy key without starting a listener.
pub(crate) async fn generate_source_key(
    config_path: PathBuf,
    private_key_file: PathBuf,
) -> Result<(), ProcessError> {
    let host = HostConfigurationLoader::from_process_working_directory()?.load(&config_path)?;
    let host_view = host.view();
    if !matches!(host_view.source, SourceConfigurationView::ManagedGit { .. }) {
        return Err(ProcessError::ManagedSourceDisabled);
    }
    let _process_lock = acquire_process_lock(host_view.runtime_root)?;
    let files = SourceKeyFiles::prepare(private_key_file)?;
    let (canonical_public_key, fingerprint) = files.generate().await?;

    let mut output = io::stdout().lock();
    writeln!(
        output,
        "\nMaincopy generated a managed-source deploy key.\n\n  Public key: {canonical_public_key}\n  Fingerprint: {fingerprint}\n\nInstall only the public key as a read-only deploy key.\n"
    )
    .and_then(|()| output.flush())
    .map_err(|error| {
        offline_failure(
            StartupStage::Source,
            "display the generated public source key",
            error,
        )
    })
}

struct SourceKeyFiles {
    private_key: PathBuf,
    public_key: PathBuf,
    parent: PathBuf,
}

impl SourceKeyFiles {
    fn prepare(private_key: PathBuf) -> Result<Self, ProcessError> {
        let private_key = resolve_explicit_path(private_key)?;
        let public_key = public_key_path(&private_key)?;
        let parent = private_key
            .parent()
            .ok_or(ProcessError::IdentityCredentialInvalid)?
            .to_owned();
        prepare_private_directory(&parent).map_err(|error| {
            offline_failure(
                StartupStage::Source,
                "prepare the private source-key directory",
                error,
            )
        })?;
        require_absent(&private_key)?;
        require_absent(&public_key)?;
        Ok(Self {
            private_key,
            public_key,
            parent,
        })
    }

    async fn generate(&self) -> Result<(String, String), ProcessError> {
        let generated = match self.run_generator().await {
            Ok(()) => self.validate().await,
            Err(failure) => Err(failure),
        };
        match generated {
            Ok(identity) => Ok(identity),
            Err(failure) => Err(self.clean_up_failed_generation(failure)),
        }
    }

    async fn run_generator(&self) -> Result<(), ProcessError> {
        let mut child = tokio::process::Command::new(ssh_keygen_executable())
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .args(["-q", "-t", "ed25519", "-a", "100", "-N", ""])
            .arg("-C")
            .arg("maincopy managed source")
            .arg("-f")
            .arg(&self.private_key)
            .spawn()
            .map_err(|error| {
                offline_failure(
                    StartupStage::Source,
                    "start the source-key generator",
                    error,
                )
            })?;
        wait_for_key_generation(&mut child, KEYGEN_TIMEOUT).await
    }

    async fn validate(&self) -> Result<(String, String), ProcessError> {
        let private = open_existing_private_file(&self.private_key).map_err(|error| {
            offline_failure(
                StartupStage::Source,
                "validate the generated private source key",
                error,
            )
        })?;
        let private_metadata = private.metadata().map_err(|error| {
            offline_failure(
                StartupStage::Source,
                "inspect the generated private source key",
                error,
            )
        })?;
        if private_metadata.len() == 0 || private_metadata.len() > MAX_PRIVATE_KEY_BYTES {
            return Err(invalid_private_key());
        }
        private.sync_all().map_err(|error| {
            offline_failure(
                StartupStage::Source,
                "synchronize the generated private source key",
                error,
            )
        })?;
        let public_key = read_public_key(&self.public_key)?;
        let public_identity = parse_public_key(&public_key)?;
        let derived_public_key = derive_public_key(&self.private_key).await?;
        if derived_public_key != public_identity.0 {
            return Err(offline_failure(
                StartupStage::Source,
                "verify the generated source-key pair",
                SourceBootstrapError::KeyPairMismatch,
            ));
        }
        synchronize_directory(&self.parent, "synchronize the source-key directory")?;
        Ok(public_identity)
    }

    fn clean_up_failed_generation(&self, failure: ProcessError) -> ProcessError {
        let mut cleanup_error = None;
        for path in [&self.private_key, &self.public_key] {
            if let Err(error) = fs::remove_file(path)
                && error.kind() != io::ErrorKind::NotFound
                && cleanup_error.is_none()
            {
                cleanup_error = Some(error);
            }
        }
        if let Err(error) = File::open(&self.parent).and_then(|directory| directory.sync_all())
            && cleanup_error.is_none()
        {
            cleanup_error = Some(error);
        }
        match cleanup_error {
            None => failure,
            Some(error) => offline_failure(
                StartupStage::Source,
                "remove an incomplete generated source key",
                error,
            ),
        }
    }
}

async fn wait_for_key_generation(
    child: &mut tokio::process::Child,
    runtime_limit: Duration,
) -> Result<(), ProcessError> {
    match tokio::time::timeout(runtime_limit, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(_)) => Err(offline_failure(
            StartupStage::Source,
            "generate the dedicated source key",
            SourceBootstrapError::KeyGenerationFailed,
        )),
        Ok(Err(error)) => Err(offline_failure(
            StartupStage::Source,
            "wait for the source-key generator",
            error,
        )),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(offline_failure(
                StartupStage::Source,
                "bound the source-key generator runtime",
                SourceBootstrapError::KeyGenerationTimedOut,
            ))
        }
    }
}

async fn derive_public_key(private_key: &Path) -> Result<String, ProcessError> {
    let mut child = tokio::process::Command::new(ssh_keygen_executable())
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .args(["-y", "-f"])
        .arg(private_key)
        .spawn()
        .map_err(|error| {
            offline_failure(
                StartupStage::Source,
                "start private source-key verification",
                error,
            )
        })?;
    let output = collect_derived_public_key(&mut child, KEYGEN_TIMEOUT).await?;
    let source = std::str::from_utf8(&output).map_err(|_| invalid_private_key())?;
    parse_public_key(source)
        .map(|(canonical, _)| canonical)
        .map_err(|_| invalid_private_key())
}

async fn collect_derived_public_key(
    child: &mut Child,
    runtime_limit: Duration,
) -> Result<Vec<u8>, ProcessError> {
    let stdout = child.stdout.take().ok_or_else(invalid_private_key)?;
    let mut output = Vec::new();
    let completed = tokio::time::timeout(runtime_limit, async {
        let read_result = {
            let mut bounded = stdout.take(MAX_PUBLIC_KEY_BYTES + 1);
            let result = bounded.read_to_end(&mut output).await;
            drop(bounded);
            result
        };
        (read_result, child.wait().await)
    })
    .await;

    match completed {
        Ok((Ok(_), Ok(status)))
            if status.success() && output.len() as u64 <= MAX_PUBLIC_KEY_BYTES =>
        {
            Ok(output)
        }
        Ok((Err(error), _)) => Err(offline_failure(
            StartupStage::Source,
            "read the derived public source key",
            error,
        )),
        Ok((_, Err(error))) => Err(offline_failure(
            StartupStage::Source,
            "wait for private source-key verification",
            error,
        )),
        Ok((Ok(_), Ok(_))) => Err(invalid_private_key()),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(offline_failure(
                StartupStage::Source,
                "bound private source-key verification runtime",
                SourceBootstrapError::KeyValidationTimedOut,
            ))
        }
    }
}

async fn configure_with_store(
    store: &DatabaseStore,
    request: ManagedSourceConfigurationInput,
    idempotency_key: Uuid,
) -> Result<StoredSourceConfiguration, ProcessError> {
    let owner_user_id = enabled_owner(store).await?;
    store
        .source
        .put_configuration(PutSourceConfiguration {
            request,
            occurred_at: OffsetDateTime::now_utc(),
            audit: MutationAuditContext {
                audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
                principal: AuditPrincipalReference::Offline {
                    user_id: Some(owner_user_id),
                },
                request_id: None,
                idempotency_key: AdminMutationKey(idempotency_key),
            },
        })
        .await
        .map_err(map_source_configuration_error)
}

fn map_source_configuration_error(error: DatabaseMutationError) -> ProcessError {
    match error {
        DatabaseMutationError::Command(
            DatabaseCommandError::IdempotencyConflict | DatabaseCommandError::Rejected,
        ) => ProcessError::SourceConfigurationConflict,
        error => offline_failure(
            StartupStage::Source,
            "persist the managed source configuration",
            error,
        ),
    }
}

async fn enabled_owner(store: &DatabaseStore) -> Result<UserId, ProcessError> {
    let identity = store.auth.identity_state().await.map_err(|error| {
        offline_failure(
            StartupStage::Identity,
            "read identity state before source setup",
            error,
        )
    })?;
    if identity.bootstrap_required {
        return Err(ProcessError::SourceOwnerRequired);
    }

    let mut cursor = None;
    for _ in 0..MAX_OWNER_SEARCH_PAGES {
        let page = store
            .auth
            .users_page(cursor, OWNER_PAGE_SIZE)
            .await
            .map_err(|error| {
                offline_failure(
                    StartupStage::Identity,
                    "find an enabled owner for source setup",
                    error,
                )
            })?;
        if let Some(owner) = page.items.into_iter().find(|user| {
            user.status == UserStatus::Enabled && user.roles.contains(&UserRole::Owner)
        }) {
            return Ok(owner.user_id);
        }
        let Some(next) = page.next_cursor else {
            return Err(ProcessError::SourceOwnerRequired);
        };
        cursor = Some(next);
    }
    Err(offline_failure(
        StartupStage::Identity,
        "bound the owner search for source setup",
        SourceBootstrapError::OwnerSearchLimit,
    ))
}

fn acquire_process_lock(runtime_root: &Path) -> Result<ProcessLock, ProcessError> {
    match ProcessLock::acquire(runtime_root) {
        Ok(lock) => Ok(lock),
        Err(ProcessLockError::AlreadyRunning) => Err(ProcessError::AlreadyRunning),
        Err(error) => Err(offline_failure(
            StartupStage::ProcessLock,
            "acquire the process lock for source setup",
            error,
        )),
    }
}

fn resolve_explicit_path(path: PathBuf) -> Result<PathBuf, ProcessError> {
    if path.as_os_str().is_empty() || path.file_name().is_none() {
        return Err(ProcessError::IdentityCredentialInvalid);
    }
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|working_directory| working_directory.join(path))
            .map_err(|error| {
                offline_failure(
                    StartupStage::Configuration,
                    "resolve the explicit source-key path",
                    error,
                )
            })
    }
}

fn public_key_path(private_key_file: &Path) -> Result<PathBuf, ProcessError> {
    let mut public_key = private_key_file.as_os_str().to_os_string();
    public_key.push(".pub");
    let public_key = PathBuf::from(public_key);
    if public_key == private_key_file {
        Err(ProcessError::IdentityCredentialInvalid)
    } else {
        Ok(public_key)
    }
}

fn require_absent(path: &Path) -> Result<(), ProcessError> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(offline_failure(
            StartupStage::Source,
            "refuse to overwrite an existing source-key file",
            SourceBootstrapError::KeyFileExists,
        )),
        Err(error) => Err(offline_failure(
            StartupStage::Source,
            "inspect the source-key output target",
            error,
        )),
    }
}

fn synchronize_directory(parent: &Path, context: &'static str) -> Result<(), ProcessError> {
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| offline_failure(StartupStage::Source, context, error))
}

fn ssh_keygen_executable() -> OsString {
    option_env!("MAINCOPY_SSH_KEYGEN").map_or_else(|| OsString::from("ssh-keygen"), OsString::from)
}

fn read_public_key(path: &Path) -> Result<String, ProcessError> {
    let file = File::open(path).map_err(|error| {
        offline_failure(
            StartupStage::Source,
            "open the generated public source key",
            error,
        )
    })?;
    let metadata = file.metadata().map_err(|error| {
        offline_failure(
            StartupStage::Source,
            "inspect the generated public source key",
            error,
        )
    })?;
    if !metadata.is_file() || metadata.len() > MAX_PUBLIC_KEY_BYTES {
        return Err(offline_failure(
            StartupStage::Source,
            "validate the generated public source key",
            SourceBootstrapError::PublicKeyInvalid,
        ));
    }
    let mut source = String::new();
    file.take(MAX_PUBLIC_KEY_BYTES + 1)
        .read_to_string(&mut source)
        .map_err(|error| {
            offline_failure(
                StartupStage::Source,
                "read the generated public source key",
                error,
            )
        })?;
    if source.len() as u64 > MAX_PUBLIC_KEY_BYTES {
        return Err(offline_failure(
            StartupStage::Source,
            "bound the generated public source key",
            SourceBootstrapError::PublicKeyInvalid,
        ));
    }
    Ok(source)
}

fn parse_public_key(source: &str) -> Result<(String, String), ProcessError> {
    let mut lines = source.lines();
    let line = lines.next().unwrap_or_default();
    if lines.any(|line| !line.is_empty()) {
        return Err(invalid_public_key());
    }
    let mut fields = line.split_ascii_whitespace();
    if fields.next() != Some("ssh-ed25519") {
        return Err(invalid_public_key());
    }
    let encoded = fields.next().ok_or_else(invalid_public_key)?;
    if fields.any(|field| field.chars().any(char::is_control)) {
        return Err(invalid_public_key());
    }
    let blob = STANDARD_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid_public_key())?;
    if !valid_ed25519_blob(&blob) {
        return Err(invalid_public_key());
    }
    let fingerprint = STANDARD_NO_PAD.encode(Sha256::digest(&blob));
    Ok((
        format!("ssh-ed25519 {encoded}"),
        format!("SHA256:{fingerprint}"),
    ))
}

fn valid_ed25519_blob(blob: &[u8]) -> bool {
    const NAME: &[u8] = b"ssh-ed25519";
    blob.len() == 4 + NAME.len() + 4 + 32
        && blob.get(..4) == Some(&(NAME.len() as u32).to_be_bytes())
        && blob.get(4..4 + NAME.len()) == Some(NAME)
        && blob.get(4 + NAME.len()..8 + NAME.len()) == Some(&32_u32.to_be_bytes())
}

fn invalid_public_key() -> ProcessError {
    offline_failure(
        StartupStage::Source,
        "validate the generated public source key",
        SourceBootstrapError::PublicKeyInvalid,
    )
}

fn invalid_private_key() -> ProcessError {
    offline_failure(
        StartupStage::Source,
        "validate the generated private source key",
        SourceBootstrapError::PrivateKeyInvalid,
    )
}

fn offline_failure(
    stage: StartupStage,
    operation: &'static str,
    error: impl std::error::Error + Send + Sync + 'static,
) -> ProcessError {
    tracing::error!(%stage, operation, error = %error, "offline source operation failed");
    ApplicationError::Startup {
        stage,
        operation,
        source: Box::new(error),
    }
    .into()
}

#[derive(Debug, thiserror::Error)]
enum SourceBootstrapError {
    #[error("the bounded owner search was exhausted")]
    OwnerSearchLimit,
    #[error("a source-key output file already exists")]
    KeyFileExists,
    #[error("the source-key generator failed")]
    KeyGenerationFailed,
    #[error("the source-key generator exceeded its runtime limit")]
    KeyGenerationTimedOut,
    #[error("the generated private key is missing, invalid, or outside its byte limit")]
    PrivateKeyInvalid,
    #[error("the generated public key is not a canonical Ed25519 SSH key")]
    PublicKeyInvalid,
    #[error("the generated public key does not correspond to the private key")]
    KeyPairMismatch,
    #[error("private source-key verification exceeded its runtime limit")]
    KeyValidationTimedOut,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_lock::open_private_file;
    use base64::engine::general_purpose::STANDARD;

    const KEY_GENERATION_FIXTURE_ENV: &str = "MAINCOPY_TEST_KEY_GENERATION_FIXTURE";

    #[test]
    fn key_generation_wait_process_fixture() {
        match std::env::var(KEY_GENERATION_FIXTURE_ENV).as_deref() {
            Ok("failure") => panic!("key-generation failure fixture"),
            Ok("timeout") => loop {
                std::thread::park();
            },
            _ => {}
        }
    }

    fn key_generation_fixture(mode: &str) -> tokio::process::Child {
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "source_bootstrap::tests::key_generation_wait_process_fixture",
            ])
            .env(KEY_GENERATION_FIXTURE_ENV, mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn public_key_fixture(public_byte: u8) -> String {
        let mut blob = Vec::new();
        blob.extend_from_slice(&11_u32.to_be_bytes());
        blob.extend_from_slice(b"ssh-ed25519");
        blob.extend_from_slice(&32_u32.to_be_bytes());
        blob.extend_from_slice(&[public_byte; 32]);
        format!("ssh-ed25519 {}\n", STANDARD.encode(blob))
    }

    #[tokio::test]
    async fn key_generation_wait_accepts_success_and_reports_failure() {
        let mut successful = key_generation_fixture("success");
        assert!(
            wait_for_key_generation(&mut successful, Duration::from_secs(5))
                .await
                .is_ok()
        );

        let mut failed = key_generation_fixture("failure");
        let error = wait_for_key_generation(&mut failed, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("source key"));
    }

    #[tokio::test]
    async fn key_generation_wait_terminates_and_reaps_a_hung_generator() {
        let mut child = key_generation_fixture("timeout");
        let error = wait_for_key_generation(&mut child, Duration::from_millis(50))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("runtime limit"));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn failed_key_generation_removes_partial_outputs_and_preserves_the_failure() {
        let root = tempfile::tempdir().unwrap();
        let files = SourceKeyFiles {
            private_key: root.path().join("source-key"),
            public_key: root.path().join("source-key.pub"),
            parent: root.path().to_owned(),
        };
        fs::write(&files.private_key, "partial private key").unwrap();
        fs::write(&files.public_key, "partial public key").unwrap();

        let failure = files.clean_up_failed_generation(ProcessError::IdentityCredentialInvalid);

        assert!(matches!(failure, ProcessError::IdentityCredentialInvalid));
        assert!(!files.private_key.exists());
        assert!(!files.public_key.exists());
    }

    #[test]
    fn failed_key_generation_reports_cleanup_failure() {
        let root = tempfile::tempdir().unwrap();
        let files = SourceKeyFiles {
            private_key: root.path().join("source-key"),
            public_key: root.path().join("source-key.pub"),
            parent: root.path().to_owned(),
        };
        fs::create_dir(&files.private_key).unwrap();

        let failure = files.clean_up_failed_generation(ProcessError::IdentityCredentialInvalid);

        assert!(matches!(failure, ProcessError::Application(_)));
        assert!(files.private_key.is_dir());
    }

    #[tokio::test]
    async fn generated_key_validation_does_not_create_a_missing_private_key() {
        let root = tempfile::tempdir().unwrap();
        let files = SourceKeyFiles {
            private_key: root.path().join("source-key"),
            public_key: root.path().join("source-key.pub"),
            parent: root.path().to_owned(),
        };
        fs::write(&files.public_key, public_key_fixture(0x42)).unwrap();

        assert!(files.validate().await.is_err());
        assert!(!files.private_key.exists());
    }

    #[tokio::test]
    async fn generated_key_validation_rejects_empty_and_oversized_private_outputs() {
        let root = tempfile::tempdir().unwrap();
        let files = SourceKeyFiles {
            private_key: root.path().join("source-key"),
            public_key: root.path().join("source-key.pub"),
            parent: root.path().to_owned(),
        };
        fs::write(&files.public_key, public_key_fixture(0x42)).unwrap();
        let private = open_private_file(&files.private_key).unwrap();

        assert!(files.validate().await.is_err());
        private.set_len(MAX_PRIVATE_KEY_BYTES + 1).unwrap();
        assert!(files.validate().await.is_err());
    }

    #[tokio::test]
    async fn generated_key_validation_rejects_invalid_private_material() {
        let root = tempfile::tempdir().unwrap();
        let files = SourceKeyFiles {
            private_key: root.path().join("source-key"),
            public_key: root.path().join("source-key.pub"),
            parent: root.path().to_owned(),
        };
        fs::write(&files.public_key, public_key_fixture(0x42)).unwrap();
        let mut private = open_private_file(&files.private_key).unwrap();
        private.write_all(b"not an OpenSSH private key").unwrap();
        private.sync_all().unwrap();

        assert!(files.validate().await.is_err());
    }

    #[tokio::test]
    async fn generated_key_validation_rejects_a_mismatched_public_key() {
        let root = tempfile::tempdir().unwrap();
        let files = SourceKeyFiles::prepare(root.path().join("keys/source-key")).unwrap();
        files.run_generator().await.unwrap();
        fs::write(&files.public_key, public_key_fixture(0x42)).unwrap();

        let error = files.validate().await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("verify the generated source-key pair")
        );
    }

    #[test]
    fn generated_public_key_parser_returns_only_canonical_public_material() {
        let public_key_fixture = public_key_fixture(0x42);
        let encoded = public_key_fixture.split_ascii_whitespace().nth(1).unwrap();

        let (public_key, fingerprint) =
            parse_public_key(&format!("ssh-ed25519 {encoded} ignored-comment\n")).unwrap();

        assert_eq!(public_key, format!("ssh-ed25519 {encoded}"));
        assert!(fingerprint.starts_with("SHA256:"));
        assert!(!public_key.contains("ignored-comment"));
    }

    #[test]
    fn public_key_parser_rejects_other_key_types_and_trailing_material() {
        for invalid in [
            "ssh-rsa AAAA comment\n",
            "ssh-ed25519 not-base64\n",
            "ssh-ed25519 AAAA\nsecond-key AAAA\n",
        ] {
            assert!(parse_public_key(invalid).is_err(), "accepted {invalid:?}");
        }
    }
}
