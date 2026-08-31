use std::{fmt, str::FromStr};

use k256::schnorr::VerifyingKey;
use serde::{Deserialize, Serialize, de};
use thiserror::Error;

pub const MAX_USERNAME_BYTES: usize = 64;
const NOSTR_PUBLIC_KEY_BYTES: usize = 32;

/// A v1 login name whose database comparison is exact byte equality.
///
/// Usernames are already canonical when they enter this type. They are never
/// case-folded or normalized during lookup.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalUsername(Box<str>);

impl CanonicalUsername {
    pub fn parse(value: &str) -> Result<Self, CanonicalUsernameError> {
        if value.is_empty() {
            return Err(CanonicalUsernameError::Empty);
        }
        if value.len() > MAX_USERNAME_BYTES {
            return Err(CanonicalUsernameError::TooLong {
                actual: value.len(),
                maximum: MAX_USERNAME_BYTES,
            });
        }

        let bytes = value.as_bytes();
        if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
            return Err(CanonicalUsernameError::InvalidBoundary);
        }
        if !bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        }) {
            return Err(CanonicalUsernameError::InvalidCharacter);
        }

        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CanonicalUsername {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for CanonicalUsername {
    type Err = CanonicalUsernameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for CanonicalUsername {
    fn serialize<Serializer>(
        &self,
        serializer: Serializer,
    ) -> Result<Serializer::Ok, Serializer::Error>
    where
        Serializer: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CanonicalUsername {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CanonicalUsernameError {
    #[error("a username must not be empty")]
    Empty,
    #[error("the username is {actual} bytes; the maximum is {maximum}")]
    TooLong { actual: usize, maximum: usize },
    #[error("a username must start and end with an ASCII letter or digit")]
    InvalidBoundary,
    #[error("a username must contain only lowercase ASCII letters, digits, '-', '_', or '.'")]
    InvalidCharacter,
}

/// A canonical lowercase x-only secp256k1 public key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NostrPublicKey {
    bytes: [u8; NOSTR_PUBLIC_KEY_BYTES],
    encoded: Box<str>,
}

impl NostrPublicKey {
    pub fn parse(value: &str) -> Result<Self, NostrPublicKeyError> {
        let bytes = decode_lower_hex::<NOSTR_PUBLIC_KEY_BYTES>(value)
            .ok_or(NostrPublicKeyError::InvalidEncoding)?;
        VerifyingKey::from_bytes(&bytes).map_err(|_| NostrPublicKeyError::InvalidPoint)?;

        Ok(Self {
            bytes,
            encoded: value.into(),
        })
    }

    pub fn from_bytes(bytes: [u8; NOSTR_PUBLIC_KEY_BYTES]) -> Result<Self, NostrPublicKeyError> {
        VerifyingKey::from_bytes(&bytes).map_err(|_| NostrPublicKeyError::InvalidPoint)?;
        Ok(Self {
            encoded: encode_lower_hex(&bytes).into_boxed_str(),
            bytes,
        })
    }

    pub fn as_bytes(&self) -> &[u8; NOSTR_PUBLIC_KEY_BYTES] {
        &self.bytes
    }

    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    pub(crate) fn verifying_key(&self) -> VerifyingKey {
        // Construction validates the point, so reconstructing the key cannot
        // fail while this value exists.
        VerifyingKey::from_bytes(&self.bytes).expect("validated Nostr public key")
    }
}

impl fmt::Display for NostrPublicKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for NostrPublicKey {
    type Err = NostrPublicKeyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for NostrPublicKey {
    fn serialize<Serializer>(
        &self,
        serializer: Serializer,
    ) -> Result<Serializer::Ok, Serializer::Error>
    where
        Serializer: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for NostrPublicKey {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum NostrPublicKeyError {
    #[error("a Nostr public key must be exactly 32 bytes of lowercase hexadecimal")]
    InvalidEncoding,
    #[error("the Nostr public key is not a valid x-only secp256k1 point")]
    InvalidPoint,
}

pub(crate) fn decode_lower_hex<const LENGTH: usize>(value: &str) -> Option<[u8; LENGTH]> {
    if value.len() != LENGTH * 2 {
        return None;
    }

    let mut output = [0_u8; LENGTH];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high = decode_nibble(pair[0])?;
        let low = decode_nibble(pair[1])?;
        output[index] = high << 4 | low;
    }
    Some(output)
}

pub(crate) fn encode_lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

const fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_NOSTR_PUBLIC_KEY: &str =
        "63fe6318dc58583cfe16810f86dd09e18bfd76aabc24a0081ce2856f330504ed";

    #[test]
    fn username_comparison_is_exact_and_already_canonical() {
        let username = CanonicalUsername::parse("alice.writer-2").unwrap();
        assert_eq!(username.as_str(), "alice.writer-2");
        assert_eq!(username.to_string(), "alice.writer-2");
        assert_ne!(
            username,
            CanonicalUsername::parse("alice_writer-2").unwrap()
        );
    }

    #[test]
    fn invalid_usernames_are_rejected_without_normalization() {
        let cases = [
            ("", CanonicalUsernameError::Empty),
            ("Alice", CanonicalUsernameError::InvalidCharacter),
            ("álîçé", CanonicalUsernameError::InvalidBoundary),
            (" alice", CanonicalUsernameError::InvalidBoundary),
            ("alice ", CanonicalUsernameError::InvalidBoundary),
            ("-alice", CanonicalUsernameError::InvalidBoundary),
            ("alice.", CanonicalUsernameError::InvalidBoundary),
            ("alice+writer", CanonicalUsernameError::InvalidCharacter),
            ("alice/writer", CanonicalUsernameError::InvalidCharacter),
        ];

        for (value, expected) in cases {
            assert_eq!(CanonicalUsername::parse(value), Err(expected), "{value}");
        }

        let long = "a".repeat(MAX_USERNAME_BYTES + 1);
        assert_eq!(
            CanonicalUsername::parse(&long),
            Err(CanonicalUsernameError::TooLong {
                actual: MAX_USERNAME_BYTES + 1,
                maximum: MAX_USERNAME_BYTES,
            })
        );
    }

    #[test]
    fn username_serde_reuses_canonical_validation() {
        let username = CanonicalUsername::parse("publisher_1").unwrap();
        assert_eq!(serde_json::to_string(&username).unwrap(), "\"publisher_1\"");
        assert_eq!(
            serde_json::from_str::<CanonicalUsername>("\"publisher_1\"").unwrap(),
            username
        );
        assert!(serde_json::from_str::<CanonicalUsername>("\"Publisher_1\"").is_err());
    }

    #[test]
    fn canonical_nostr_public_key_round_trips() {
        let public_key = NostrPublicKey::parse(VALID_NOSTR_PUBLIC_KEY).unwrap();
        assert_eq!(public_key.as_str(), VALID_NOSTR_PUBLIC_KEY);
        assert_eq!(
            NostrPublicKey::from_bytes(*public_key.as_bytes()).unwrap(),
            public_key
        );
        assert_eq!(
            serde_json::from_str::<NostrPublicKey>(&serde_json::to_string(&public_key).unwrap())
                .unwrap(),
            public_key
        );
    }

    #[test]
    fn noncanonical_or_invalid_nostr_public_keys_fail_closed() {
        assert_eq!(
            NostrPublicKey::parse(&VALID_NOSTR_PUBLIC_KEY.to_uppercase()),
            Err(NostrPublicKeyError::InvalidEncoding)
        );
        assert_eq!(
            NostrPublicKey::parse(&VALID_NOSTR_PUBLIC_KEY[..62]),
            Err(NostrPublicKeyError::InvalidEncoding)
        );
        assert_eq!(
            NostrPublicKey::parse(&"ff".repeat(NOSTR_PUBLIC_KEY_BYTES)),
            Err(NostrPublicKeyError::InvalidPoint)
        );
    }
}
