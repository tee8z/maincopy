use std::sync::Arc;

use maincopy_shared::auth_api::SecretString;
use thiserror::Error;
use tokio::{sync::Semaphore, task};

use crate::domain::auth::{
    Argon2idPolicy, DummyPasswordHash, PasswordHashingError, PasswordVerification,
    PasswordVerificationError, StoredPasswordHash,
};

const DEFAULT_MAX_IN_FLIGHT: usize = 4;
const DEFAULT_MAX_PENDING: usize = 64;

/// Bounded blocking-work admission for every Argon2 operation.
#[derive(Clone)]
pub(crate) struct PasswordExecutor {
    policy: Arc<Argon2idPolicy>,
    dummy: Arc<DummyPasswordHash>,
    in_flight: Arc<Semaphore>,
    pending: Arc<Semaphore>,
}

impl PasswordExecutor {
    pub(crate) async fn new(policy: Argon2idPolicy) -> Result<Self, PasswordExecutorError> {
        Self::with_limits(policy, DEFAULT_MAX_IN_FLIGHT, DEFAULT_MAX_PENDING).await
    }

    async fn with_limits(
        policy: Argon2idPolicy,
        max_in_flight: usize,
        max_pending: usize,
    ) -> Result<Self, PasswordExecutorError> {
        if max_in_flight == 0 || max_pending < max_in_flight {
            return Err(PasswordExecutorError::InvalidLimits);
        }
        let policy = Arc::new(policy);
        let dummy_policy = Arc::clone(&policy);
        let dummy = task::spawn_blocking(move || dummy_policy.create_dummy_hash())
            .await
            .map_err(PasswordExecutorError::WorkerFailed)?
            .map_err(PasswordExecutorError::Hashing)?;
        Ok(Self {
            policy,
            dummy: Arc::new(dummy),
            in_flight: Arc::new(Semaphore::new(max_in_flight)),
            pending: Arc::new(Semaphore::new(max_pending)),
        })
    }

    pub(crate) async fn hash_password(
        &self,
        password: SecretString,
    ) -> Result<StoredPasswordHash, PasswordExecutorError> {
        let permit = self.admit().await?;
        let policy = Arc::clone(&self.policy);
        task::spawn_blocking(move || {
            let _permit = permit;
            policy.hash_password(password.expose_secret())
        })
        .await
        .map_err(PasswordExecutorError::WorkerFailed)?
        .map_err(PasswordExecutorError::Hashing)
    }

    pub(crate) async fn verify_password(
        &self,
        password: SecretString,
        stored: Option<StoredPasswordHash>,
    ) -> Result<PasswordVerification, PasswordExecutorError> {
        let permit = self.admit().await?;
        let policy = Arc::clone(&self.policy);
        let dummy = Arc::clone(&self.dummy);
        task::spawn_blocking(move || {
            let _permit = permit;
            policy.verify_or_dummy(password.expose_secret(), stored.as_ref(), &dummy)
        })
        .await
        .map_err(PasswordExecutorError::WorkerFailed)?
        .map_err(PasswordExecutorError::Verification)
    }

    async fn admit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, PasswordExecutorError> {
        let pending = Arc::clone(&self.pending)
            .try_acquire_owned()
            .map_err(|_| PasswordExecutorError::Busy)?;
        let in_flight = Arc::clone(&self.in_flight)
            .acquire_owned()
            .await
            .map_err(|_| PasswordExecutorError::Closed)?;
        drop(pending);
        Ok(in_flight)
    }
}

#[derive(Debug, Error)]
pub(crate) enum PasswordExecutorError {
    #[error("password verification capacity is temporarily exhausted")]
    Busy,
    #[error("password verification is shutting down")]
    Closed,
    #[error("password worker limits are invalid")]
    InvalidLimits,
    #[error("a password worker stopped unexpectedly")]
    WorkerFailed(#[source] task::JoinError),
    #[error(transparent)]
    Hashing(PasswordHashingError),
    #[error(transparent)]
    Verification(PasswordVerificationError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn password(value: &str) -> SecretString {
        SecretString::new(value)
    }

    #[tokio::test]
    async fn hashes_and_verifies_through_the_bounded_worker_pool() {
        let executor = PasswordExecutor::with_limits(Argon2idPolicy::v1(), 1, 1)
            .await
            .unwrap();
        let first_stored = executor
            .hash_password(password("a valid long password"))
            .await
            .unwrap();
        assert_eq!(
            executor
                .verify_password(password("a valid long password"), Some(first_stored))
                .await
                .unwrap(),
            PasswordVerification::Verified {
                needs_rehash: false
            }
        );
        let second_stored = executor
            .hash_password(password("a valid long password"))
            .await
            .unwrap();
        assert_eq!(
            executor
                .verify_password(password("a different password"), Some(second_stored))
                .await
                .unwrap(),
            PasswordVerification::Rejected
        );
        assert_eq!(
            executor
                .verify_password(password("a valid long password"), None)
                .await
                .unwrap(),
            PasswordVerification::Rejected
        );
    }
}
