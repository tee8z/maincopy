use std::fmt;

use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHasher as _, PasswordVerifier as _, phc::PasswordHash},
};
use thiserror::Error;

pub const MIN_PASSWORD_SCALARS: usize = 15;
pub const MAX_PASSWORD_SCALARS: usize = 128;
pub const MAX_PASSWORD_BYTES: usize = 1024;
pub const MAX_PASSWORD_PHC_BYTES: usize = 256;

pub const ARGON2ID_MIN_MEMORY_KIB: u32 = 19_456;
pub const ARGON2ID_MIN_ITERATIONS: u32 = 2;
pub const ARGON2ID_MIN_PARALLELISM: u32 = 1;
pub const ARGON2ID_MAX_MEMORY_KIB: u32 = 262_144;
pub const ARGON2ID_MAX_ITERATIONS: u32 = 10;
pub const ARGON2ID_MAX_PARALLELISM: u32 = 4;

const PASSWORD_SALT_BYTES: usize = 16;
const PASSWORD_OUTPUT_BYTES: usize = 32;
const ARGON2_VERSION_19: u32 = 19;
const DUMMY_PASSWORD: &str = "maincopy-dummy-password";

/// Explicit Argon2id v19 parameters for password creation and verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Argon2idPolicy {
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
    params: Params,
}

impl Argon2idPolicy {
    /// The initial v1 policy. It is equal to the deployment-independent floor.
    pub fn v1() -> Self {
        Self::new(
            ARGON2ID_MIN_MEMORY_KIB,
            ARGON2ID_MIN_ITERATIONS,
            ARGON2ID_MIN_PARALLELISM,
        )
        .expect("the fixed v1 Argon2id policy is valid")
    }

    fn new(
        memory_kib: u32,
        iterations: u32,
        parallelism: u32,
    ) -> Result<Self, Argon2idPolicyError> {
        if memory_kib < ARGON2ID_MIN_MEMORY_KIB {
            return Err(Argon2idPolicyError::MemoryBelowFloor);
        }
        if memory_kib > ARGON2ID_MAX_MEMORY_KIB {
            return Err(Argon2idPolicyError::MemoryAboveCeiling);
        }
        if iterations < ARGON2ID_MIN_ITERATIONS {
            return Err(Argon2idPolicyError::IterationsBelowFloor);
        }
        if iterations > ARGON2ID_MAX_ITERATIONS {
            return Err(Argon2idPolicyError::IterationsAboveCeiling);
        }
        if parallelism < ARGON2ID_MIN_PARALLELISM {
            return Err(Argon2idPolicyError::ParallelismBelowFloor);
        }
        if parallelism > ARGON2ID_MAX_PARALLELISM {
            return Err(Argon2idPolicyError::ParallelismAboveCeiling);
        }

        let params = Params::new(
            memory_kib,
            iterations,
            parallelism,
            Some(PASSWORD_OUTPUT_BYTES),
        )
        .map_err(|_| Argon2idPolicyError::InvalidParameters)?;

        Ok(Self {
            memory_kib,
            iterations,
            parallelism,
            params,
        })
    }

    pub fn hash_password(
        &self,
        password: &str,
    ) -> Result<StoredPasswordHash, PasswordHashingError> {
        validate_password_input(password)?;
        let hash: PasswordHash = self
            .argon2()
            .hash_password(password.as_bytes())
            .map_err(|_| PasswordHashingError::CryptographicFailure)?;
        self.parse_hash(&hash.to_string())
            .map_err(|_| PasswordHashingError::CryptographicFailure)
    }

    /// Parses a stored PHC string without running Argon2.
    ///
    /// This boundary rejects unsupported or excessive parameters before a
    /// verification worker allocates attacker-influenced memory.
    pub fn parse_hash(&self, encoded: &str) -> Result<StoredPasswordHash, StoredPasswordHashError> {
        StoredPasswordHash::parse(encoded)
    }

    pub fn create_dummy_hash(&self) -> Result<DummyPasswordHash, PasswordHashingError> {
        self.hash_password(DUMMY_PASSWORD).map(DummyPasswordHash)
    }

    /// Performs exactly one real or dummy Argon2 verification for a bounded
    /// password attempt.
    pub fn verify_or_dummy(
        &self,
        password: &str,
        stored: Option<&StoredPasswordHash>,
        dummy: &DummyPasswordHash,
    ) -> Result<PasswordVerification, PasswordVerificationError> {
        validate_password_input(password)?;

        let selected = stored.unwrap_or(&dummy.0);
        let parsed = PasswordHash::new(selected.as_str())
            .map_err(|_| PasswordVerificationError::CredentialInvariant)?;
        let password_matches = self
            .argon2()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
        let verified = stored.is_some() && password_matches;

        if verified {
            Ok(PasswordVerification::Verified {
                needs_rehash: selected.is_weaker_than(self),
            })
        } else {
            Ok(PasswordVerification::Rejected)
        }
    }

    fn argon2(&self) -> Argon2<'static> {
        Argon2::new(Algorithm::Argon2id, Version::V0x13, self.params.clone())
    }
}

/// A validated Argon2id v19 PHC string suitable for persistence.
#[derive(Eq, PartialEq)]
pub struct StoredPasswordHash {
    encoded: Box<str>,
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
}

impl StoredPasswordHash {
    pub fn parse(encoded: &str) -> Result<Self, StoredPasswordHashError> {
        if encoded.len() > MAX_PASSWORD_PHC_BYTES {
            return Err(StoredPasswordHashError::TooLong {
                actual: encoded.len(),
                maximum: MAX_PASSWORD_PHC_BYTES,
            });
        }

        let hash = PasswordHash::new(encoded).map_err(|_| StoredPasswordHashError::Malformed)?;
        if hash.to_string() != encoded {
            return Err(StoredPasswordHashError::NonCanonical);
        }
        if hash.algorithm.as_str() != "argon2id" {
            return Err(StoredPasswordHashError::WrongAlgorithm);
        }
        if hash.version != Some(ARGON2_VERSION_19) {
            return Err(StoredPasswordHashError::WrongVersion);
        }
        if hash.params.iter().count() != 3 {
            return Err(StoredPasswordHashError::UnsupportedParameters);
        }

        let params =
            Params::try_from(&hash).map_err(|_| StoredPasswordHashError::UnsupportedParameters)?;
        validate_stored_costs(&params)?;

        let salt = hash.salt.ok_or(StoredPasswordHashError::MissingSalt)?;
        if salt.as_ref().len() != PASSWORD_SALT_BYTES {
            return Err(StoredPasswordHashError::WrongSaltLength);
        }
        let output = hash.hash.ok_or(StoredPasswordHashError::MissingOutput)?;
        if output.len() != PASSWORD_OUTPUT_BYTES {
            return Err(StoredPasswordHashError::WrongOutputLength);
        }

        Ok(Self {
            encoded: encoded.into(),
            memory_kib: params.m_cost(),
            iterations: params.t_cost(),
            parallelism: params.p_cost(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    fn is_weaker_than(&self, policy: &Argon2idPolicy) -> bool {
        self.memory_kib < policy.memory_kib
            || self.iterations < policy.iterations
            || self.parallelism < policy.parallelism
    }
}

impl fmt::Debug for StoredPasswordHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StoredPasswordHash(<redacted>)")
    }
}

pub struct DummyPasswordHash(StoredPasswordHash);

impl fmt::Debug for DummyPasswordHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DummyPasswordHash(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PasswordVerification {
    Rejected,
    Verified { needs_rehash: bool },
}

pub fn validate_password_input(password: &str) -> Result<(), PasswordInputError> {
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(PasswordInputError::TooManyBytes {
            actual: password.len(),
            maximum: MAX_PASSWORD_BYTES,
        });
    }

    let scalar_count = password.chars().count();
    if scalar_count < MIN_PASSWORD_SCALARS {
        return Err(PasswordInputError::TooFewScalars {
            actual: scalar_count,
            minimum: MIN_PASSWORD_SCALARS,
        });
    }
    if scalar_count > MAX_PASSWORD_SCALARS {
        return Err(PasswordInputError::TooManyScalars {
            actual: scalar_count,
            maximum: MAX_PASSWORD_SCALARS,
        });
    }
    Ok(())
}

fn validate_stored_costs(params: &Params) -> Result<(), StoredPasswordHashError> {
    if params.m_cost() < ARGON2ID_MIN_MEMORY_KIB
        || params.m_cost() > ARGON2ID_MAX_MEMORY_KIB
        || params.t_cost() < ARGON2ID_MIN_ITERATIONS
        || params.t_cost() > ARGON2ID_MAX_ITERATIONS
        || params.p_cost() < ARGON2ID_MIN_PARALLELISM
        || params.p_cost() > ARGON2ID_MAX_PARALLELISM
    {
        return Err(StoredPasswordHashError::CostOutsideLimits);
    }
    if params.output_len() != Some(PASSWORD_OUTPUT_BYTES)
        || !params.keyid().is_empty()
        || !params.data().is_empty()
    {
        return Err(StoredPasswordHashError::UnsupportedParameters);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum Argon2idPolicyError {
    #[error("Argon2id memory cost is below the supported floor")]
    MemoryBelowFloor,
    #[error("Argon2id memory cost exceeds the verification ceiling")]
    MemoryAboveCeiling,
    #[error("Argon2id iteration cost is below the supported floor")]
    IterationsBelowFloor,
    #[error("Argon2id iteration cost exceeds the verification ceiling")]
    IterationsAboveCeiling,
    #[error("Argon2id parallelism is below the supported floor")]
    ParallelismBelowFloor,
    #[error("Argon2id parallelism exceeds the verification ceiling")]
    ParallelismAboveCeiling,
    #[error("the Argon2id parameter combination is invalid")]
    InvalidParameters,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[expect(
    clippy::enum_variant_names,
    reason = "the repeated qualifiers make each password validation error unambiguous at call sites"
)]
pub enum PasswordInputError {
    #[error("the password has {actual} Unicode scalar values; the minimum is {minimum}")]
    TooFewScalars { actual: usize, minimum: usize },
    #[error("the password has {actual} Unicode scalar values; the maximum is {maximum}")]
    TooManyScalars { actual: usize, maximum: usize },
    #[error("the password is {actual} bytes; the maximum is {maximum}")]
    TooManyBytes { actual: usize, maximum: usize },
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PasswordHashingError {
    #[error(transparent)]
    InvalidPassword(#[from] PasswordInputError),
    #[error("password hashing failed")]
    CryptographicFailure,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PasswordVerificationError {
    #[error(transparent)]
    InvalidPassword(#[from] PasswordInputError),
    #[error("the selected password credential violated a validated invariant")]
    CredentialInvariant,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum StoredPasswordHashError {
    #[error("the password PHC string is {actual} bytes; the maximum is {maximum}")]
    TooLong { actual: usize, maximum: usize },
    #[error("the password PHC string is malformed")]
    Malformed,
    #[error("the password PHC string is not canonically encoded")]
    NonCanonical,
    #[error("the password PHC string does not use Argon2id")]
    WrongAlgorithm,
    #[error("the password PHC string does not use Argon2 version 19")]
    WrongVersion,
    #[error("the password PHC string has unsupported parameters")]
    UnsupportedParameters,
    #[error("the password PHC string has a cost outside verification limits")]
    CostOutsideLimits,
    #[error("the password PHC string has no salt")]
    MissingSalt,
    #[error("the password PHC string salt is not 16 bytes")]
    WrongSaltLength,
    #[error("the password PHC string has no output")]
    MissingOutput,
    #[error("the password PHC string output is not 32 bytes")]
    WrongOutputLength,
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWORD: &str = "correct horse battery staple";
    const WRONG_PASSWORD: &str = "incorrect-horse-battery-staple";

    #[test]
    fn v1_policy_is_explicit_and_at_the_supported_floor() {
        let policy = Argon2idPolicy::v1();
        assert_eq!(policy.memory_kib, ARGON2ID_MIN_MEMORY_KIB);
        assert_eq!(policy.iterations, ARGON2ID_MIN_ITERATIONS);
        assert_eq!(policy.parallelism, ARGON2ID_MIN_PARALLELISM);
    }

    #[test]
    fn policy_floors_and_resource_ceilings_are_enforced() {
        let cases = [
            (
                Argon2idPolicy::new(
                    ARGON2ID_MIN_MEMORY_KIB - 1,
                    ARGON2ID_MIN_ITERATIONS,
                    ARGON2ID_MIN_PARALLELISM,
                ),
                Argon2idPolicyError::MemoryBelowFloor,
            ),
            (
                Argon2idPolicy::new(
                    ARGON2ID_MAX_MEMORY_KIB + 1,
                    ARGON2ID_MIN_ITERATIONS,
                    ARGON2ID_MIN_PARALLELISM,
                ),
                Argon2idPolicyError::MemoryAboveCeiling,
            ),
            (
                Argon2idPolicy::new(
                    ARGON2ID_MIN_MEMORY_KIB,
                    ARGON2ID_MIN_ITERATIONS - 1,
                    ARGON2ID_MIN_PARALLELISM,
                ),
                Argon2idPolicyError::IterationsBelowFloor,
            ),
            (
                Argon2idPolicy::new(
                    ARGON2ID_MIN_MEMORY_KIB,
                    ARGON2ID_MAX_ITERATIONS + 1,
                    ARGON2ID_MIN_PARALLELISM,
                ),
                Argon2idPolicyError::IterationsAboveCeiling,
            ),
            (
                Argon2idPolicy::new(
                    ARGON2ID_MIN_MEMORY_KIB,
                    ARGON2ID_MIN_ITERATIONS,
                    ARGON2ID_MIN_PARALLELISM - 1,
                ),
                Argon2idPolicyError::ParallelismBelowFloor,
            ),
            (
                Argon2idPolicy::new(
                    ARGON2ID_MIN_MEMORY_KIB,
                    ARGON2ID_MIN_ITERATIONS,
                    ARGON2ID_MAX_PARALLELISM + 1,
                ),
                Argon2idPolicyError::ParallelismAboveCeiling,
            ),
        ];

        for (actual, expected) in cases {
            assert_eq!(actual, Err(expected));
        }
    }

    #[test]
    fn password_scalar_boundaries_are_inclusive_and_bytes_are_not_normalized() {
        let policy = Argon2idPolicy::v1();
        assert!(
            policy
                .hash_password(&"a".repeat(MIN_PASSWORD_SCALARS))
                .is_ok()
        );
        assert!(
            policy
                .hash_password(&"a".repeat(MAX_PASSWORD_SCALARS))
                .is_ok()
        );

        assert_eq!(
            policy.hash_password(&"a".repeat(MIN_PASSWORD_SCALARS - 1)),
            Err(PasswordHashingError::InvalidPassword(
                PasswordInputError::TooFewScalars {
                    actual: MIN_PASSWORD_SCALARS - 1,
                    minimum: MIN_PASSWORD_SCALARS,
                }
            ))
        );
        assert_eq!(
            policy.hash_password(&"a".repeat(MAX_PASSWORD_SCALARS + 1)),
            Err(PasswordHashingError::InvalidPassword(
                PasswordInputError::TooManyScalars {
                    actual: MAX_PASSWORD_SCALARS + 1,
                    maximum: MAX_PASSWORD_SCALARS,
                }
            ))
        );

        let oversized_bytes = "🦀".repeat((MAX_PASSWORD_BYTES / 4) + 1);
        assert_eq!(oversized_bytes.len(), MAX_PASSWORD_BYTES + 4);
        assert_eq!(
            policy.hash_password(&oversized_bytes),
            Err(PasswordHashingError::InvalidPassword(
                PasswordInputError::TooManyBytes {
                    actual: MAX_PASSWORD_BYTES + 4,
                    maximum: MAX_PASSWORD_BYTES,
                }
            ))
        );
    }

    #[test]
    fn hashes_use_unique_salts_and_exact_v1_phc_fields() {
        let policy = Argon2idPolicy::v1();
        let first = policy.hash_password(PASSWORD).unwrap();
        let second = policy.hash_password(PASSWORD).unwrap();
        assert_ne!(first, second);

        for hash in [&first, &second] {
            assert!(hash.as_str().starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
            assert_eq!(hash.memory_kib, ARGON2ID_MIN_MEMORY_KIB);
            assert_eq!(hash.iterations, ARGON2ID_MIN_ITERATIONS);
            assert_eq!(hash.parallelism, ARGON2ID_MIN_PARALLELISM);

            let parsed = PasswordHash::new(hash.as_str()).unwrap();
            assert_eq!(parsed.salt.unwrap().as_ref().len(), PASSWORD_SALT_BYTES);
            assert_eq!(parsed.hash.unwrap().len(), PASSWORD_OUTPUT_BYTES);
        }
    }

    #[test]
    fn real_and_dummy_verification_have_the_same_external_result_shape() {
        let policy = Argon2idPolicy::v1();
        let stored = policy.hash_password(PASSWORD).unwrap();
        let dummy = policy.create_dummy_hash().unwrap();

        let correct = policy
            .verify_or_dummy(PASSWORD, Some(&stored), &dummy)
            .unwrap();
        assert_eq!(
            correct,
            PasswordVerification::Verified {
                needs_rehash: false
            }
        );

        let wrong = policy
            .verify_or_dummy(WRONG_PASSWORD, Some(&stored), &dummy)
            .unwrap();
        assert_eq!(wrong, PasswordVerification::Rejected);

        let missing = policy
            .verify_or_dummy(DUMMY_PASSWORD, None, &dummy)
            .unwrap();
        assert_eq!(missing, PasswordVerification::Rejected);
    }

    #[test]
    fn password_bytes_are_not_unicode_normalized() {
        let policy = Argon2idPolicy::v1();
        let composed = "é".repeat(MIN_PASSWORD_SCALARS);
        let decomposed = "e\u{301}".repeat(MIN_PASSWORD_SCALARS);
        let stored = policy.hash_password(&composed).unwrap();
        let dummy = policy.create_dummy_hash().unwrap();

        assert!(matches!(
            policy
                .verify_or_dummy(&composed, Some(&stored), &dummy)
                .unwrap(),
            PasswordVerification::Verified { .. }
        ));
        assert_eq!(
            policy
                .verify_or_dummy(&decomposed, Some(&stored), &dummy)
                .unwrap(),
            PasswordVerification::Rejected
        );
    }

    #[test]
    fn a_stronger_active_policy_requests_rehash_only_after_success() {
        let old_policy = Argon2idPolicy::v1();
        let stored = old_policy.hash_password(PASSWORD).unwrap();
        let stronger = Argon2idPolicy::new(
            ARGON2ID_MIN_MEMORY_KIB + 1024,
            ARGON2ID_MIN_ITERATIONS,
            ARGON2ID_MIN_PARALLELISM,
        )
        .unwrap();
        let dummy = stronger.create_dummy_hash().unwrap();

        let correct = stronger
            .verify_or_dummy(PASSWORD, Some(&stored), &dummy)
            .unwrap();
        assert_eq!(
            correct,
            PasswordVerification::Verified { needs_rehash: true }
        );

        let wrong = stronger
            .verify_or_dummy(WRONG_PASSWORD, Some(&stored), &dummy)
            .unwrap();
        assert_eq!(wrong, PasswordVerification::Rejected);
    }

    #[test]
    fn malformed_or_excessive_phc_strings_fail_before_verification() {
        let policy = Argon2idPolicy::v1();
        let valid = policy.hash_password(PASSWORD).unwrap();
        let encoded = valid.as_str();

        let cases = [
            "not-phc".to_owned(),
            encoded.replacen("argon2id", "argon2i", 1),
            encoded.replacen("v=19", "v=16", 1),
            encoded.replacen("m=19456", "m=19455", 1),
            encoded.replacen("m=19456", "m=262145", 1),
            encoded.replacen("p=1", "p=1,keyid=YWJjZA", 1),
            "x".repeat(MAX_PASSWORD_PHC_BYTES + 1),
        ];

        for malformed in cases {
            assert!(policy.parse_hash(&malformed).is_err(), "{malformed}");
        }
    }

    #[test]
    fn password_hashes_and_dummy_hashes_are_redacted() {
        let policy = Argon2idPolicy::v1();
        let stored = policy.hash_password(PASSWORD).unwrap();
        let dummy = policy.create_dummy_hash().unwrap();
        assert_eq!(format!("{stored:?}"), "StoredPasswordHash(<redacted>)");
        assert_eq!(format!("{dummy:?}"), "DummyPasswordHash(<redacted>)");
    }
}
