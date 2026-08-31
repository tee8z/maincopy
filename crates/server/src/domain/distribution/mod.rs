//! Versioned, immutable payloads for outbound distribution targets.

use std::{fmt, str::FromStr};

use markdown_compiler::{PostId, PostRevisionDigest};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const CURRENT_PAYLOAD_VERSION: u16 = 1;
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
const TARGET_PAYLOAD_CONTEXT: &str = "maincopy target payload digest v1";
const TARGET_PAYLOAD_PREFIX: &str = "target-payload-b3-v1-";
const DIGEST_HEX_LENGTH: usize = 64;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistributionTarget {
    X,
}

impl DistributionTarget {
    const fn as_str(self) -> &'static str {
        match self {
            Self::X => "x",
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct TargetIdempotencyKey(String);

impl TargetIdempotencyKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TargetPayload {
    version: u16,
    body: String,
}

/// Stable identity of one versioned target payload.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TargetPayloadDigest {
    bytes: [u8; 32],
    encoded: Box<str>,
}

impl TargetPayloadDigest {
    pub fn parse(value: &str) -> Result<Self, TargetPayloadDigestParseError> {
        let hex = value
            .strip_prefix(TARGET_PAYLOAD_PREFIX)
            .ok_or(TargetPayloadDigestParseError)?;
        if hex.len() != DIGEST_HEX_LENGTH {
            return Err(TargetPayloadDigestParseError);
        }

        let mut bytes = [0_u8; 32];
        for (index, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let high = decode_nibble(pair[0]).ok_or(TargetPayloadDigestParseError)?;
            let low = decode_nibble(pair[1]).ok_or(TargetPayloadDigestParseError)?;
            bytes[index] = high << 4 | low;
        }

        Ok(Self::from_bytes(bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    pub(crate) const fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        let hash = blake3::Hash::from_bytes(bytes);
        Self {
            bytes,
            encoded: format!("{TARGET_PAYLOAD_PREFIX}{}", hash.to_hex()).into_boxed_str(),
        }
    }
}

const fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl fmt::Display for TargetPayloadDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for TargetPayloadDigest {
    type Err = TargetPayloadDigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for TargetPayloadDigest {
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

impl<'de> Deserialize<'de> for TargetPayloadDigest {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("target payload digest must use the canonical BLAKE3 encoding")]
pub struct TargetPayloadDigestParseError;

impl<'de> Deserialize<'de> for TargetPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WirePayload {
            version: u16,
            body: String,
        }

        let payload = WirePayload::deserialize(deserializer)?;
        Self::from_version(payload.version, payload.body).map_err(serde::de::Error::custom)
    }
}

impl TargetPayload {
    pub fn new(body: impl Into<String>) -> Result<Self, PayloadError> {
        Self::from_version(CURRENT_PAYLOAD_VERSION, body)
    }

    pub fn from_version(version: u16, body: impl Into<String>) -> Result<Self, PayloadError> {
        if version != CURRENT_PAYLOAD_VERSION {
            return Err(PayloadError::UnsupportedVersion { version });
        }

        let body = body.into();
        if body.len() > MAX_PAYLOAD_BYTES {
            return Err(PayloadError::TooLarge {
                bytes: body.len(),
                maximum: MAX_PAYLOAD_BYTES,
            });
        }

        Ok(Self { version, body })
    }

    pub const fn version(&self) -> u16 {
        self.version
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn digest(&self) -> TargetPayloadDigest {
        let mut transcript = blake3::Hasher::new_derive_key(TARGET_PAYLOAD_CONTEXT);
        transcript.update(&self.version.to_be_bytes());
        transcript.update(&(self.body.len() as u64).to_be_bytes());
        transcript.update(self.body.as_bytes());
        TargetPayloadDigest::from_bytes(*transcript.finalize().as_bytes())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum PayloadError {
    #[error("target payload version {version} is not supported")]
    UnsupportedVersion { version: u16 },
    #[error("target payload is {bytes} bytes; the maximum is {maximum}")]
    TooLarge { bytes: usize, maximum: usize },
}

/// Builds a stable, unambiguous key for one target delivery.
pub(crate) fn target_idempotency_key(
    stable_post_id: &PostId,
    revision_digest: &PostRevisionDigest,
    target: DistributionTarget,
) -> TargetIdempotencyKey {
    let stable_post_id = stable_post_id.as_str();
    let revision_digest = revision_digest.as_str();
    let target = target.as_str();
    TargetIdempotencyKey(format!(
        "{}:{stable_post_id}|{}:{revision_digest}|{}:{target}",
        stable_post_id.len(),
        revision_digest.len(),
        target.len(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const POST_A: &str = "11111111-1111-4111-8111-111111111111";
    const POST_B: &str = "22222222-2222-4222-8222-222222222222";
    const DIGEST_A: &str =
        "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111";
    const DIGEST_B: &str =
        "post-b3-v1-2222222222222222222222222222222222222222222222222222222222222222";

    #[test]
    fn rejects_an_unknown_payload_version() {
        assert_eq!(
            TargetPayload::from_version(CURRENT_PAYLOAD_VERSION + 1, "copy"),
            Err(PayloadError::UnsupportedVersion {
                version: CURRENT_PAYLOAD_VERSION + 1,
            })
        );
    }

    #[test]
    fn deserialization_cannot_bypass_version_validation() {
        let json = format!(
            r#"{{"version":{},"body":"copy"}}"#,
            CURRENT_PAYLOAD_VERSION + 1
        );
        assert!(serde_json::from_str::<TargetPayload>(&json).is_err());
    }

    #[test]
    fn rejects_an_oversized_payload() {
        let body = "x".repeat(MAX_PAYLOAD_BYTES + 1);
        assert_eq!(
            TargetPayload::new(body),
            Err(PayloadError::TooLarge {
                bytes: MAX_PAYLOAD_BYTES + 1,
                maximum: MAX_PAYLOAD_BYTES,
            })
        );
    }

    #[test]
    fn typed_identities_produce_distinct_idempotency_keys() {
        let post_a = PostId::parse(POST_A).unwrap();
        let post_b = PostId::parse(POST_B).unwrap();
        let digest_a = PostRevisionDigest::parse(DIGEST_A).unwrap();
        let digest_b = PostRevisionDigest::parse(DIGEST_B).unwrap();

        assert_ne!(
            target_idempotency_key(&post_a, &digest_a, DistributionTarget::X),
            target_idempotency_key(&post_b, &digest_a, DistributionTarget::X)
        );
        assert_ne!(
            target_idempotency_key(&post_a, &digest_a, DistributionTarget::X),
            target_idempotency_key(&post_a, &digest_b, DistributionTarget::X)
        );
    }

    #[test]
    fn deserialization_rejects_unknown_payload_fields() {
        let json = r#"{"version":1,"body":"copy","new_field":true}"#;
        assert!(serde_json::from_str::<TargetPayload>(json).is_err());
    }

    #[test]
    fn payload_digest_is_domain_separated_and_round_trips() {
        let payload = TargetPayload::new("copy").unwrap();
        let digest = payload.digest();

        assert_eq!(
            digest.as_str(),
            "target-payload-b3-v1-149abb216bf262f3259b06dd1e16e590\
             c49d208657753fe83b892adcab70c73c"
        );
        assert_eq!(TargetPayloadDigest::from_bytes(*digest.as_bytes()), digest);
        assert_eq!(TargetPayloadDigest::parse(digest.as_str()).unwrap(), digest);
        assert_eq!(
            serde_json::from_value::<TargetPayloadDigest>(serde_json::to_value(&digest).unwrap())
                .unwrap(),
            digest
        );
        assert_ne!(
            TargetPayload::new("different copy").unwrap().digest(),
            digest
        );
    }

    #[test]
    fn distribution_targets_have_stable_names() {
        assert_eq!(
            serde_json::to_value(DistributionTarget::X).unwrap(),
            serde_json::json!("x")
        );
        assert_eq!(
            serde_json::from_value::<DistributionTarget>(serde_json::json!("x")).unwrap(),
            DistributionTarget::X
        );
        assert!(serde_json::from_value::<DistributionTarget>(serde_json::json!("nostr")).is_err());
        assert_eq!(DistributionTarget::X.as_str(), "x");
    }
}
