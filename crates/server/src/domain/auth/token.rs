use std::fmt;

use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use thiserror::Error;
use zeroize::Zeroize as _;

use super::identity::{decode_lower_hex, encode_lower_hex};

const TOKEN_BYTES: usize = 32;
#[cfg(test)]
const TOKEN_HEX_BYTES: usize = TOKEN_BYTES * 2;
const SESSION_TOKEN_PREFIX: &str = "mcs1_";
const CSRF_TOKEN_PREFIX: &str = "mcc1_";
const LOGIN_CHALLENGE_PREFIX: &str = "mcl1_";
const SESSION_DIGEST_CONTEXT: &[u8] = b"maincopy session token digest v1\0";
const CSRF_DIGEST_CONTEXT: &[u8] = b"maincopy csrf token digest v1\0";
const LOGIN_CHALLENGE_DIGEST_CONTEXT: &[u8] = b"maincopy login challenge digest v1\0";

macro_rules! opaque_token {
    ($name:ident, $digest:ident, $prefix:ident, $context:ident, $debug:literal) => {
        pub struct $name {
            bytes: [u8; TOKEN_BYTES],
            encoded: String,
        }

        impl $name {
            pub fn generate() -> Result<Self, TokenGenerationError> {
                let mut bytes = [0_u8; TOKEN_BYTES];
                getrandom::fill(&mut bytes).map_err(|_| TokenGenerationError)?;
                Ok(Self::from_bytes(bytes))
            }

            pub fn parse(value: &str) -> Result<Self, TokenParseError> {
                let hex = value.strip_prefix($prefix).ok_or(TokenParseError)?;
                let bytes = decode_lower_hex::<TOKEN_BYTES>(hex).ok_or(TokenParseError)?;
                Ok(Self {
                    bytes,
                    encoded: value.into(),
                })
            }

            pub fn expose_secret(&self) -> &str {
                &self.encoded
            }

            pub fn digest(&self) -> $digest {
                let mut hasher = Sha256::new();
                hasher.update($context);
                hasher.update(self.bytes);
                $digest(hasher.finalize().into())
            }

            fn from_bytes(bytes: [u8; TOKEN_BYTES]) -> Self {
                Self {
                    encoded: format!("{}{}", $prefix, encode_lower_hex(&bytes)),
                    bytes,
                }
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str($debug)
            }
        }

        impl Drop for $name {
            fn drop(&mut self) {
                self.bytes.zeroize();
                self.encoded.zeroize();
            }
        }

        #[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $digest([u8; TOKEN_BYTES]);

        impl $digest {
            #[cfg(test)]
            pub const fn from_bytes(bytes: [u8; TOKEN_BYTES]) -> Self {
                Self(bytes)
            }

            pub const fn as_bytes(&self) -> &[u8; TOKEN_BYTES] {
                &self.0
            }
        }

        impl fmt::Debug for $digest {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(concat!(stringify!($digest), "(<redacted>)"))
            }
        }
    };
}

opaque_token!(
    SessionToken,
    SessionTokenDigest,
    SESSION_TOKEN_PREFIX,
    SESSION_DIGEST_CONTEXT,
    "SessionToken(<redacted>)"
);
opaque_token!(
    CsrfToken,
    CsrfTokenDigest,
    CSRF_TOKEN_PREFIX,
    CSRF_DIGEST_CONTEXT,
    "CsrfToken(<redacted>)"
);
opaque_token!(
    LoginChallenge,
    LoginChallengeDigest,
    LOGIN_CHALLENGE_PREFIX,
    LOGIN_CHALLENGE_DIGEST_CONTEXT,
    "LoginChallenge(<redacted>)"
);

macro_rules! stored_digest {
    ($digest:ident) => {
        impl $digest {
            pub fn parse_bytes(bytes: &[u8]) -> Result<Self, TokenDigestParseError> {
                let bytes: [u8; TOKEN_BYTES] =
                    bytes.try_into().map_err(|_| TokenDigestParseError)?;
                Ok(Self(bytes))
            }

            pub fn ct_eq(&self, other: &Self) -> bool {
                bool::from(self.0.ct_eq(&other.0))
            }
        }
    };
}

stored_digest!(CsrfTokenDigest);
stored_digest!(LoginChallengeDigest);

#[cfg(test)]
impl SessionTokenDigest {
    pub fn parse_bytes(bytes: &[u8]) -> Result<Self, TokenDigestParseError> {
        let bytes: [u8; TOKEN_BYTES] = bytes.try_into().map_err(|_| TokenDigestParseError)?;
        Ok(Self(bytes))
    }

    pub fn ct_eq(&self, other: &Self) -> bool {
        bool::from(self.0.ct_eq(&other.0))
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("the operating system could not generate an authentication token")]
pub struct TokenGenerationError;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("the authentication token is not a canonical opaque token")]
pub struct TokenParseError;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("an authentication token digest must be exactly 32 bytes")]
pub struct TokenDigestParseError;

#[cfg(test)]
mod tests {
    use std::{borrow::Borrow, ops::Deref};

    use serde::{Serialize, de::DeserializeOwned};

    use super::*;

    macro_rules! assert_not_impl {
        ($value:ty: $bound:path) => {
            const _: fn() = || {
                trait AmbiguousIfImpl<Marker> {
                    fn marker() {}
                }

                impl<Value: ?Sized> AmbiguousIfImpl<()> for Value {}
                impl<Value: ?Sized + $bound> AmbiguousIfImpl<u8> for Value {}

                let _ = <$value as AmbiguousIfImpl<_>>::marker;
            };
        };
    }

    assert_not_impl!(SessionToken: Copy);
    assert_not_impl!(SessionToken: Clone);
    assert_not_impl!(SessionToken: Serialize);
    assert_not_impl!(SessionToken: DeserializeOwned);
    assert_not_impl!(SessionToken: Deref);
    assert_not_impl!(SessionToken: AsRef<str>);
    assert_not_impl!(SessionToken: Borrow<str>);
    assert_not_impl!(CsrfToken: Copy);
    assert_not_impl!(CsrfToken: Clone);
    assert_not_impl!(CsrfToken: Serialize);
    assert_not_impl!(CsrfToken: DeserializeOwned);
    assert_not_impl!(LoginChallenge: Copy);
    assert_not_impl!(LoginChallenge: Clone);
    assert_not_impl!(LoginChallenge: Serialize);
    assert_not_impl!(LoginChallenge: DeserializeOwned);

    #[test]
    fn generated_tokens_have_independent_256_bit_values() {
        let session = SessionToken::generate().unwrap();
        let csrf = CsrfToken::generate().unwrap();
        let second_session = SessionToken::generate().unwrap();
        let challenge = LoginChallenge::generate().unwrap();

        assert_eq!(
            session.expose_secret().len(),
            SESSION_TOKEN_PREFIX.len() + TOKEN_HEX_BYTES
        );
        assert_eq!(
            csrf.expose_secret().len(),
            CSRF_TOKEN_PREFIX.len() + TOKEN_HEX_BYTES
        );
        assert_ne!(session.digest().as_bytes(), csrf.digest().as_bytes());
        assert_ne!(session.digest(), second_session.digest());
        assert_eq!(
            challenge.expose_secret().len(),
            LOGIN_CHALLENGE_PREFIX.len() + TOKEN_HEX_BYTES
        );
    }

    #[test]
    fn token_roles_have_distinct_encodings_and_digest_domains() {
        let bytes = [0xa5; TOKEN_BYTES];
        let session = SessionToken::from_bytes(bytes);
        let csrf = CsrfToken::from_bytes(bytes);
        let challenge = LoginChallenge::from_bytes(bytes);

        assert!(CsrfToken::parse(session.expose_secret()).is_err());
        assert!(SessionToken::parse(csrf.expose_secret()).is_err());
        assert!(LoginChallenge::parse(session.expose_secret()).is_err());
        assert!(SessionToken::parse(challenge.expose_secret()).is_err());
        assert_ne!(session.digest().as_bytes(), csrf.digest().as_bytes());
        assert_ne!(session.digest().as_bytes(), challenge.digest().as_bytes());
        assert_ne!(csrf.digest().as_bytes(), challenge.digest().as_bytes());
    }

    #[test]
    fn token_parsing_is_strict_and_round_trips() {
        let token = SessionToken::from_bytes([0x12; TOKEN_BYTES]);
        let encoded = token.expose_secret().to_owned();
        let parsed = SessionToken::parse(&encoded).unwrap();
        assert_eq!(parsed.digest(), token.digest());

        assert!(SessionToken::parse("").is_err());
        assert!(SessionToken::parse(&encoded.to_uppercase()).is_err());
        assert!(SessionToken::parse(&encoded[..encoded.len() - 1]).is_err());
        assert!(SessionToken::parse(&format!("{encoded}0")).is_err());
    }

    #[test]
    fn raw_tokens_are_redacted_and_digests_require_exact_width() {
        let token = SessionToken::from_bytes([0x77; TOKEN_BYTES]);
        assert_eq!(format!("{token:?}"), "SessionToken(<redacted>)");
        assert_eq!(
            format!("{:?}", token.digest()),
            "SessionTokenDigest(<redacted>)"
        );
        assert_eq!(
            SessionTokenDigest::parse_bytes(token.digest().as_bytes()).unwrap(),
            token.digest()
        );
        assert!(token.digest().ct_eq(&token.digest()));
        assert!(
            !token
                .digest()
                .ct_eq(&SessionTokenDigest::from_bytes([0; TOKEN_BYTES]))
        );
        assert!(SessionTokenDigest::parse_bytes(&[0_u8; TOKEN_BYTES - 1]).is_err());
        assert!(SessionTokenDigest::parse_bytes(&[0_u8; TOKEN_BYTES + 1]).is_err());

        let challenge = LoginChallenge::from_bytes([0x33; TOKEN_BYTES]);
        assert_eq!(format!("{challenge:?}"), "LoginChallenge(<redacted>)");
        assert_eq!(
            format!("{:?}", challenge.digest()),
            "LoginChallengeDigest(<redacted>)"
        );
    }
}
