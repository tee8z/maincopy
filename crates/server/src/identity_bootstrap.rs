use std::{
    io::{self, IsTerminal as _, Read as _, Write as _},
    path::PathBuf,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use maincopy_shared::{
    auth::{AdminAuditEventId, InstanceId, UserId},
    auth_api::SecretString,
};
use subtle::ConstantTimeEq as _;
use time::OffsetDateTime;
use tokio::task::spawn_blocking;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroize as _;

use crate::{
    cli::BootstrapCredential,
    config::HostConfigurationLoader,
    database::{self, DatabaseStore},
    domain::auth::{
        Argon2idPolicy, CanonicalUsername, MAX_PASSWORD_BYTES, PasswordHashingError,
        store::{
            AuthCommandError, AuthLoadError, AuthMutationError, BootstrapIdentity,
            BootstrapIdentityResult, ConfiguredLoginProviders, NewHumanCredential,
        },
    },
    error::{ApplicationError, ProcessError, StartupStage},
    password_executor::{PasswordExecutor, PasswordExecutorError},
    process_lock::{ProcessLock, ProcessLockError},
};

const INITIAL_PASSWORD_POLICY_VERSION: u32 = 1;
const GENERATED_OWNER_USERNAME: &str = "owner";
const GENERATED_OWNER_PASSWORD_PREFIX: &str = "mcp1_";
const GENERATED_OWNER_PASSWORD_ENTROPY_BYTES: usize = 32;

struct GeneratedOwnerPassword(SecretString);

impl GeneratedOwnerPassword {
    fn generate() -> Result<Self, GeneratedOwnerBootstrapError> {
        let mut entropy = [0_u8; GENERATED_OWNER_PASSWORD_ENTROPY_BYTES];
        if getrandom::fill(&mut entropy).is_err() {
            entropy.zeroize();
            return Err(GeneratedOwnerBootstrapError::PasswordGeneration);
        }
        Ok(Self::from_entropy(entropy))
    }

    fn from_entropy(mut entropy: [u8; GENERATED_OWNER_PASSWORD_ENTROPY_BYTES]) -> Self {
        let mut password = String::with_capacity(48);
        password.push_str(GENERATED_OWNER_PASSWORD_PREFIX);
        URL_SAFE_NO_PAD.encode_string(entropy, &mut password);
        entropy.zeroize();
        Self(SecretString::new(password.into_boxed_str()))
    }

    fn copy_for_hashing(&self) -> SecretString {
        SecretString::new(self.0.expose_secret())
    }
}

/// Creates the first owner from an instance-unique password when a daemon is
/// started against fresh state.
pub(crate) async fn bootstrap_generated_owner<Output>(
    store: &DatabaseStore,
    mut output: Output,
) -> Result<bool, GeneratedOwnerBootstrapError>
where
    Output: io::Write + Send,
{
    let identity = store
        .auth
        .identity_state()
        .await
        .map_err(GeneratedOwnerBootstrapError::IdentityState)?;
    if !identity.bootstrap_required {
        return Ok(false);
    }

    let password = GeneratedOwnerPassword::generate()?;
    let password_executor = PasswordExecutor::new(Argon2idPolicy::v1())
        .await
        .map_err(GeneratedOwnerBootstrapError::Password)?;
    let password_hash = password_executor
        .hash_password(password.copy_for_hashing())
        .await
        .map_err(GeneratedOwnerBootstrapError::Password)?;

    write_generated_owner_credential(&mut output, &password)
        .map_err(GeneratedOwnerBootstrapError::Output)?;
    output
        .flush()
        .map_err(GeneratedOwnerBootstrapError::Output)?;

    let result = persist_owner_identity(
        store,
        NewHumanCredential::Password {
            username: CanonicalUsername::parse(GENERATED_OWNER_USERNAME)
                .expect("the generated owner username is canonical"),
            password_hash,
            policy_version: INITIAL_PASSWORD_POLICY_VERSION,
        },
        ConfiguredLoginProviders::new(true, true)
            .expect("password and Nostr form a valid login-provider set"),
    )
    .await
    .map_err(GeneratedOwnerBootstrapError::Mutation)?;

    tracing::info!(
        instance_id = %result.instance.instance_id,
        owner_user_id = %result.owner_user_id,
        "generated the initial owner identity"
    );
    Ok(true)
}

fn write_generated_owner_credential(
    output: &mut impl io::Write,
    password: &GeneratedOwnerPassword,
) -> io::Result<()> {
    writeln!(
        output,
        "\nMaincopy generated the initial owner credential.\n\n  Username: {GENERATED_OWNER_USERNAME}\n  Password: {}\n\nSave this password now. Maincopy will not display it after identity setup.\n",
        password.0.expose_secret()
    )
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum GeneratedOwnerBootstrapError {
    #[error("the durable identity state could not be read")]
    IdentityState(#[source] AuthLoadError),
    #[error("a secure initial owner password could not be generated")]
    PasswordGeneration,
    #[error("the initial owner password could not be prepared")]
    Password(#[source] PasswordExecutorError),
    #[error("the generated owner credential could not be displayed")]
    Output(#[source] io::Error),
    #[error("the generated owner identity could not be persisted")]
    Mutation(#[source] AuthMutationError),
}

/// Creates the instance identity and first owner without starting any server runtime.
pub(crate) async fn bootstrap_owner(
    config_path: PathBuf,
    credential: BootstrapCredential,
) -> Result<(), ProcessError> {
    let host = HostConfigurationLoader::from_process_working_directory()?.load(&config_path)?;
    let host_view = host.view();
    let _process_lock = match ProcessLock::acquire(host_view.runtime_root) {
        Ok(process_lock) => process_lock,
        Err(ProcessLockError::AlreadyRunning) => return Err(ProcessError::AlreadyRunning),
        Err(error) => {
            return Err(offline_failure(
                StartupStage::ProcessLock,
                "acquire the process lock",
                error,
            ));
        }
    };

    let database = match database::bootstrap(host_view.database).await {
        Ok(database) => database,
        Err(database::DatabaseStartupError::AlreadyOwned) => {
            return Err(ProcessError::AlreadyRunning);
        }
        Err(error) => {
            return Err(offline_failure(
                StartupStage::Database,
                "bootstrap the database",
                error,
            ));
        }
    };
    let (store, writer) = database.into_store(host_view.database.writer_queue_capacity.get());
    let writer_shutdown = CancellationToken::new();
    let writer_task = tokio::spawn(writer.run(writer_shutdown.clone()));

    let outcome = bootstrap_with_store(&store, credential).await;
    writer_shutdown.cancel();
    drop(store);
    let writer_outcome = writer_task.await;

    match writer_outcome {
        Ok(Ok(())) => outcome,
        Ok(Err(error)) => Err(offline_failure(
            StartupStage::Database,
            "stop the sole database writer",
            error,
        )),
        Err(error) => Err(offline_failure(
            StartupStage::Database,
            "join the sole database writer",
            error,
        )),
    }
}

async fn bootstrap_with_store(
    store: &DatabaseStore,
    credential: BootstrapCredential,
) -> Result<(), ProcessError> {
    let identity = store.auth.identity_state().await.map_err(|error| {
        offline_failure(
            StartupStage::Identity,
            "read the durable identity state",
            error,
        )
    })?;
    if !identity.bootstrap_required {
        return Err(ProcessError::IdentityAlreadyBootstrapped);
    }

    let (credential, configured_providers) = match credential {
        BootstrapCredential::Password { username } => {
            let password = spawn_blocking(read_password_secret)
                .await
                .map_err(|error| {
                    offline_failure(
                        StartupStage::Identity,
                        "join the secure password reader",
                        error,
                    )
                })?
                .map_err(map_password_read_error)?;
            let executor = PasswordExecutor::new(Argon2idPolicy::v1())
                .await
                .map_err(|error| {
                    offline_failure(StartupStage::Identity, "initialize password hashing", error)
                })?;
            let password_hash = executor
                .hash_password(password)
                .await
                .map_err(map_password_hashing_error)?;
            let providers = ConfiguredLoginProviders::new(true, false)
                .expect("the password bootstrap provider set is nonempty");
            (
                NewHumanCredential::Password {
                    username,
                    password_hash,
                    policy_version: INITIAL_PASSWORD_POLICY_VERSION,
                },
                providers,
            )
        }
        BootstrapCredential::Nostr { public_key } => {
            let providers = ConfiguredLoginProviders::new(false, true)
                .expect("the Nostr bootstrap provider set is nonempty");
            (NewHumanCredential::Nostr { public_key }, providers)
        }
    };

    let result = persist_owner_identity(store, credential, configured_providers)
        .await
        .map_err(map_bootstrap_mutation_error)?;

    tracing::info!(
        instance_id = %result.instance.instance_id,
        owner_user_id = %result.owner_user_id,
        "identity bootstrap completed"
    );
    Ok(())
}

async fn persist_owner_identity(
    store: &DatabaseStore,
    credential: NewHumanCredential,
    configured_providers: ConfiguredLoginProviders,
) -> Result<BootstrapIdentityResult, AuthMutationError> {
    store
        .auth
        .bootstrap_identity(BootstrapIdentity {
            instance_id: InstanceId::from_uuid(Uuid::new_v4()),
            owner_user_id: UserId::from_uuid(Uuid::new_v4()),
            credential,
            configured_providers,
            occurred_at: OffsetDateTime::now_utc(),
            audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
        })
        .await
}

fn read_password_secret() -> Result<SecretString, PasswordReadError> {
    let standard_input = io::stdin();
    if standard_input.is_terminal() {
        let password =
            SecretString::new(rpassword::prompt_password("Owner password: ")?.into_boxed_str());
        let confirmation = SecretString::new(
            rpassword::prompt_password("Confirm owner password: ")?.into_boxed_str(),
        );
        confirm_password(password, confirmation)
    } else {
        let maximum_input =
            u64::try_from(MAX_PASSWORD_BYTES).expect("the password byte limit fits in u64") + 2;
        let config = rpassword::ConfigBuilder::new()
            .input_reader(standard_input.take(maximum_input))
            .output_discard()
            .build();
        let password = rpassword::read_password_with_config(config)?;
        Ok(SecretString::new(password.into_boxed_str()))
    }
}

fn confirm_password(
    password: SecretString,
    confirmation: SecretString,
) -> Result<SecretString, PasswordReadError> {
    if bool::from(
        password
            .expose_secret()
            .as_bytes()
            .ct_eq(confirmation.expose_secret().as_bytes()),
    ) {
        Ok(password)
    } else {
        Err(PasswordReadError::ConfirmationMismatch)
    }
}

fn map_password_read_error(error: PasswordReadError) -> ProcessError {
    match error {
        PasswordReadError::ConfirmationMismatch => ProcessError::IdentityCredentialInvalid,
        PasswordReadError::Io(error) => {
            offline_failure(StartupStage::Identity, "read the owner password", error)
        }
    }
}

fn map_password_hashing_error(error: PasswordExecutorError) -> ProcessError {
    match error {
        PasswordExecutorError::Hashing(PasswordHashingError::InvalidPassword(_)) => {
            ProcessError::IdentityCredentialInvalid
        }
        error => offline_failure(StartupStage::Identity, "hash the owner password", error),
    }
}

fn map_bootstrap_mutation_error(error: AuthMutationError) -> ProcessError {
    if matches!(
        error,
        AuthMutationError::Command(AuthCommandError::AlreadyBootstrapped)
    ) {
        ProcessError::IdentityAlreadyBootstrapped
    } else {
        offline_failure(
            StartupStage::Identity,
            "persist the instance identity and first owner",
            error,
        )
    }
}

fn offline_failure(
    stage: StartupStage,
    operation: &'static str,
    error: impl std::error::Error + Send + Sync + 'static,
) -> ProcessError {
    tracing::error!(%stage, operation, error = %error, "offline operation failed");
    ApplicationError::Startup {
        stage,
        operation,
        source: Box::new(error),
    }
    .into()
}

#[derive(Debug, thiserror::Error)]
enum PasswordReadError {
    #[error("the owner password could not be read")]
    Io(#[from] io::Error),
    #[error("the owner password confirmation did not match")]
    ConfirmationMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::auth::{MIN_PASSWORD_SCALARS, PasswordInputError};

    #[test]
    fn bootstrap_mutation_conflict_maps_to_the_conflict_exit_path() {
        assert!(matches!(
            map_bootstrap_mutation_error(AuthMutationError::Command(
                AuthCommandError::AlreadyBootstrapped
            )),
            ProcessError::IdentityAlreadyBootstrapped
        ));
    }

    #[test]
    fn mismatched_confirmation_is_redacted_and_maps_to_validation() {
        let password = SecretString::new("correct horse battery staple");
        let confirmation = SecretString::new("correct horse battery staples");
        let error = confirm_password(password, confirmation).unwrap_err();

        assert_eq!(
            error.to_string(),
            "the owner password confirmation did not match"
        );
        assert!(matches!(
            map_password_read_error(error),
            ProcessError::IdentityCredentialInvalid
        ));
    }

    #[test]
    fn weak_password_errors_are_redacted_and_map_to_validation() {
        let error = PasswordExecutorError::Hashing(PasswordHashingError::InvalidPassword(
            PasswordInputError::TooFewScalars {
                actual: 5,
                minimum: MIN_PASSWORD_SCALARS,
            },
        ));

        assert!(matches!(
            map_password_hashing_error(error),
            ProcessError::IdentityCredentialInvalid
        ));
    }

    #[test]
    fn generated_owner_password_is_strong_copyable_and_shown_once() {
        let password =
            GeneratedOwnerPassword::from_entropy([0x5a_u8; GENERATED_OWNER_PASSWORD_ENTROPY_BYTES]);
        let exposed = password.0.expose_secret();

        assert!(exposed.starts_with(GENERATED_OWNER_PASSWORD_PREFIX));
        assert_eq!(exposed.chars().count(), 48);
        assert!(exposed.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
        }));
        Argon2idPolicy::v1().hash_password(exposed).unwrap();

        let mut output = Vec::new();
        write_generated_owner_credential(&mut output, &password).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Username: owner"));
        assert_eq!(output.matches(exposed).count(), 1);
        assert!(output.contains("will not display it after identity setup"));
    }
}
