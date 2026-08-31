//! Authentication identities and cryptographic proof primitives.

mod identity;
mod nip98;
mod password;
pub(crate) mod store;
mod token;

pub(crate) use identity::{CanonicalUsername, NostrPublicKey};
#[cfg(test)]
pub(crate) use nip98::NIP98_EVENT_KIND;
pub(crate) use nip98::{
    MAX_NIP98_EVENT_BYTES, NIP98_FRESHNESS_SECONDS, Nip98EventId, Nip98Payload, Nip98Request,
    verify_nip98_event,
};
pub(crate) use password::{
    Argon2idPolicy, DummyPasswordHash, MAX_PASSWORD_BYTES, PasswordHashingError,
    PasswordVerification, PasswordVerificationError, StoredPasswordHash,
};
#[cfg(test)]
pub(crate) use password::{MIN_PASSWORD_SCALARS, PasswordInputError};
pub(crate) use token::{
    CsrfToken, CsrfTokenDigest, LoginChallenge, LoginChallengeDigest, SessionToken,
    SessionTokenDigest, TokenGenerationError,
};
