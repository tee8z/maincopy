//! Versioned, immutable payloads for outbound distribution targets.

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::content::{PostId, PostRevisionDigest};

pub const CURRENT_PAYLOAD_VERSION: u16 = 1;
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistributionTarget {
    X,
}

impl DistributionTarget {
    pub const fn as_str(self) -> &'static str {
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
    TargetIdempotencyKey(
        [stable_post_id, revision_digest, target]
            .into_iter()
            .map(|part| format!("{}:{part}", part.len()))
            .collect::<Vec<_>>()
            .join("|"),
    )
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
