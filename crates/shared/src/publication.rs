//! Wire contracts for approving exact post revisions for publication.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

/// Versioned path for approving a post revision for publication.
pub const PUBLICATIONS_PATH: &str = "/api/admin/v1/publications";

/// Header that identifies retries of the same publication command.
pub const IDEMPOTENCY_KEY_HEADER: &str = "Idempotency-Key";

/// Header carrying the exact rendered private-preview identity.
pub const PREVIEW_DIGEST_HEADER: &str = "x-maincopy-preview-digest";

/// Header carrying the post revision used to render a private preview.
pub const POST_REVISION_HEADER: &str = "x-maincopy-post-revision";

/// Header carrying the managed content-tree identity used by a private preview.
pub const CONTENT_DIGEST_HEADER: &str = "x-maincopy-content-digest";

const PREVIEW_DIGEST_PREFIX: &str = "preview-b3-v1-";
const DIGEST_HEX_LENGTH: usize = 64;

/// Stable identity of one exact private post-preview representation.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[cfg_attr(
    feature = "schema",
    schema(value_type = String, pattern = r"^preview-b3-v1-[0-9a-f]{64}$")
)]
pub struct PreviewDigest(Box<str>);

impl PreviewDigest {
    /// Parses one canonical versioned preview digest.
    pub fn parse(value: &str) -> Result<Self, PreviewDigestParseError> {
        let encoded = value
            .strip_prefix(PREVIEW_DIGEST_PREFIX)
            .ok_or(PreviewDigestParseError::InvalidPrefix)?;
        if encoded.len() != DIGEST_HEX_LENGTH {
            return Err(PreviewDigestParseError::InvalidLength);
        }
        if !encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(PreviewDigestParseError::InvalidEncoding);
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PreviewDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PreviewDigest {
    type Err = PreviewDigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for PreviewDigest {
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

impl<'de> Deserialize<'de> for PreviewDigest {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        let value = Box::<str>::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewDigestParseError {
    InvalidPrefix,
    InvalidLength,
    InvalidEncoding,
}

impl fmt::Display for PreviewDigestParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPrefix => "preview_digest must start with preview-b3-v1-",
            Self::InvalidLength => "preview_digest must contain exactly 32 encoded bytes",
            Self::InvalidEncoding => "preview_digest must use lowercase hexadecimal",
        })
    }
}

impl std::error::Error for PreviewDigestParseError {}

/// Selects the exact post revision to publish immediately or at a scheduled time.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct PublishNowRequest {
    pub post_id: Uuid,
    /// Exact private preview reviewed by the operator.
    pub preview_digest: PreviewDigest,
    /// Exact revision precondition for the approval.
    ///
    /// A first publication can omit this precondition. Approval of an update
    /// requires it.
    pub expected_revision: Option<Box<str>>,
    /// Requested publication time. Omission requests immediate publication.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "time::serde::rfc3339::option::serialize",
        deserialize_with = "deserialize_optional_scheduled_for"
    )]
    pub scheduled_for: Option<OffsetDateTime>,
}

/// Durable state reached by a publication approval command.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum PublicationApprovalState {
    Scheduled,
    Published,
}

/// Reports the exact pinned revision and durable approval state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct PublishNowResponse {
    pub publication_id: Uuid,
    pub post_id: Uuid,
    /// Exact private preview accepted by this approval.
    pub preview_digest: PreviewDigest,
    /// Exact post revision pinned by this approval.
    #[serde(deserialize_with = "deserialize_post_revision")]
    #[cfg_attr(feature = "schema", schema(pattern = r"^post-b3-v1-[0-9a-f]{64}$"))]
    pub revision: Box<str>,
    /// Whether the pinned revision is waiting for its time or is already public.
    pub state: PublicationApprovalState,
    /// Requested publication time when the approval was scheduled.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "time::serde::rfc3339::option::serialize",
        deserialize_with = "deserialize_optional_scheduled_for"
    )]
    pub scheduled_for: Option<OffsetDateTime>,
    /// Actual canonical publication time, absent until a scheduled approval activates.
    #[serde(
        serialize_with = "time::serde::rfc3339::option::serialize",
        deserialize_with = "deserialize_optional_published_at"
    )]
    pub published_at: Option<OffsetDateTime>,
    #[serde(deserialize_with = "deserialize_site_digest")]
    #[cfg_attr(feature = "schema", schema(pattern = r"^site-b3-v1-[0-9a-f]{64}$"))]
    pub site_digest: Box<str>,
    #[serde(deserialize_with = "deserialize_site_version")]
    #[cfg_attr(feature = "schema", schema(minimum = 1))]
    pub site_version: u64,
}

fn deserialize_post_revision<'de, D>(deserializer: D) -> Result<Box<str>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_digest(deserializer, "post-b3-v1-", "revision")
}

fn deserialize_site_digest<'de, D>(deserializer: D) -> Result<Box<str>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_digest(deserializer, "site-b3-v1-", "site_digest")
}

fn deserialize_digest<'de, D>(
    deserializer: D,
    prefix: &str,
    field: &str,
) -> Result<Box<str>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Box::<str>::deserialize(deserializer)?;
    let valid = value.strip_prefix(prefix).is_some_and(|encoded| {
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if valid {
        Ok(value)
    } else {
        Err(D::Error::custom(format_args!(
            "{field} must be {prefix} followed by 64 lowercase hexadecimal characters"
        )))
    }
}

fn deserialize_optional_scheduled_for<'de, D>(
    deserializer: D,
) -> Result<Option<OffsetDateTime>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_optional_utc_timestamp(deserializer, "scheduled_for")
}

fn deserialize_optional_published_at<'de, D>(
    deserializer: D,
) -> Result<Option<OffsetDateTime>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_optional_utc_timestamp(deserializer, "published_at")
}

fn deserialize_optional_utc_timestamp<'de, D>(
    deserializer: D,
    field: &str,
) -> Result<Option<OffsetDateTime>, D::Error>
where
    D: Deserializer<'de>,
{
    let timestamp = time::serde::rfc3339::option::deserialize(deserializer)?;
    match timestamp {
        Some(timestamp) if timestamp.offset() != UtcOffset::UTC => Err(D::Error::custom(
            format_args!("{field} must use the UTC offset"),
        )),
        timestamp => Ok(timestamp),
    }
}

fn deserialize_site_version<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let version = u64::deserialize(deserializer)?;
    if version > 0 {
        Ok(version)
    } else {
        Err(D::Error::custom("site_version must be greater than zero"))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const REVISION: &str =
        "post-b3-v1-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const PREVIEW_DIGEST: &str =
        "preview-b3-v1-123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0";
    const SITE_DIGEST: &str =
        "site-b3-v1-abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn published_response_value() -> serde_json::Value {
        json!({
            "publication_id": "018f2046-49b2-7c2a-9226-f81c87ab721d",
            "post_id": "123e4567-e89b-12d3-a456-426614174000",
            "preview_digest": PREVIEW_DIGEST,
            "revision": REVISION,
            "state": "published",
            "published_at": "1970-01-01T00:00:00Z",
            "site_digest": SITE_DIGEST,
            "site_version": 7
        })
    }

    fn scheduled_response_value() -> serde_json::Value {
        json!({
            "publication_id": "018f2046-49b2-7c2a-9226-f81c87ab721d",
            "post_id": "123e4567-e89b-12d3-a456-426614174000",
            "preview_digest": PREVIEW_DIGEST,
            "revision": REVISION,
            "state": "scheduled",
            "scheduled_for": "1970-01-02T00:00:00Z",
            "published_at": null,
            "site_digest": SITE_DIGEST,
            "site_version": 7
        })
    }

    #[test]
    fn preview_digest_is_a_strict_typed_string_contract() {
        let digest = PreviewDigest::parse(PREVIEW_DIGEST).unwrap();
        assert_eq!(digest.as_str(), PREVIEW_DIGEST);
        assert_eq!(digest.to_string(), PREVIEW_DIGEST);
        assert_eq!(
            serde_json::to_value(&digest).unwrap(),
            json!(PREVIEW_DIGEST)
        );
        assert_eq!(
            serde_json::from_value::<PreviewDigest>(json!(PREVIEW_DIGEST)).unwrap(),
            digest
        );

        for malformed in [
            &PREVIEW_DIGEST[1..],
            &PREVIEW_DIGEST[..PREVIEW_DIGEST.len() - 1],
            "preview-b3-v1-123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0",
            "preview-b3-v2-123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0",
        ] {
            assert!(PreviewDigest::parse(malformed).is_err(), "{malformed}");
            assert!(serde_json::from_value::<PreviewDigest>(json!(malformed)).is_err());
        }
    }

    #[test]
    fn publish_now_request_has_a_stable_bidirectional_wire_contract() {
        let request = PublishNowRequest {
            post_id: Uuid::from_u128(0x123e_4567_e89b_12d3_a456_4266_1417_4000),
            preview_digest: PreviewDigest::parse(PREVIEW_DIGEST).unwrap(),
            expected_revision: Some(REVISION.into()),
            scheduled_for: None,
        };

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value,
            json!({
                "post_id": "123e4567-e89b-12d3-a456-426614174000",
                "preview_digest": PREVIEW_DIGEST,
                "expected_revision": REVISION
            })
        );
        assert_eq!(
            serde_json::from_value::<PublishNowRequest>(value).unwrap(),
            request
        );
    }

    #[test]
    fn scheduled_request_has_a_stable_utc_wire_contract() {
        let scheduled_for = OffsetDateTime::from_unix_timestamp(86_400).unwrap();
        let request = PublishNowRequest {
            post_id: Uuid::from_u128(0x123e_4567_e89b_12d3_a456_4266_1417_4000),
            preview_digest: PreviewDigest::parse(PREVIEW_DIGEST).unwrap(),
            expected_revision: Some(REVISION.into()),
            scheduled_for: Some(scheduled_for),
        };

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value,
            json!({
                "post_id": "123e4567-e89b-12d3-a456-426614174000",
                "preview_digest": PREVIEW_DIGEST,
                "expected_revision": REVISION,
                "scheduled_for": "1970-01-02T00:00:00Z"
            })
        );
        assert_eq!(
            serde_json::from_value::<PublishNowRequest>(value).unwrap(),
            request
        );
    }

    #[test]
    fn request_rejects_a_non_utc_or_malformed_schedule() {
        for scheduled_for in [json!("1970-01-02T01:00:00+01:00"), json!("not-a-timestamp")] {
            let value = json!({
                "post_id": "123e4567-e89b-12d3-a456-426614174000",
                "preview_digest": PREVIEW_DIGEST,
                "expected_revision": REVISION,
                "scheduled_for": scheduled_for
            });

            assert!(serde_json::from_value::<PublishNowRequest>(value).is_err());
        }
    }

    #[test]
    fn request_revision_remains_server_validated() {
        let request: PublishNowRequest = serde_json::from_value(json!({
            "post_id": "123e4567-e89b-12d3-a456-426614174000",
            "preview_digest": PREVIEW_DIGEST,
            "expected_revision": "let-the-server-return-its-typed-error"
        }))
        .unwrap();

        assert_eq!(
            request.expected_revision.as_deref(),
            Some("let-the-server-return-its-typed-error")
        );
    }

    #[test]
    fn request_rejects_unknown_fields_that_could_drop_a_precondition() {
        let error = serde_json::from_value::<PublishNowRequest>(json!({
            "post_id": "123e4567-e89b-12d3-a456-426614174000",
            "preview_digest": PREVIEW_DIGEST,
            "expected_revison": REVISION
        }))
        .unwrap_err();

        assert!(error.to_string().contains("expected_revison"));
    }

    #[test]
    fn request_requires_the_reviewed_preview_digest() {
        let error = serde_json::from_value::<PublishNowRequest>(json!({
            "post_id": "123e4567-e89b-12d3-a456-426614174000",
            "expected_revision": REVISION
        }))
        .unwrap_err();

        assert!(error.to_string().contains("preview_digest"));
    }

    #[test]
    fn publish_now_response_has_a_stable_bidirectional_wire_contract() {
        let response = PublishNowResponse {
            publication_id: Uuid::from_u128(0x018f_2046_49b2_7c2a_9226_f81c_87ab_721d),
            post_id: Uuid::from_u128(0x123e_4567_e89b_12d3_a456_4266_1417_4000),
            preview_digest: PreviewDigest::parse(PREVIEW_DIGEST).unwrap(),
            revision: REVISION.into(),
            state: PublicationApprovalState::Published,
            scheduled_for: None,
            published_at: Some(OffsetDateTime::UNIX_EPOCH),
            site_digest: SITE_DIGEST.into(),
            site_version: 7,
        };

        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value, published_response_value());
        assert_eq!(
            serde_json::from_value::<PublishNowResponse>(value).unwrap(),
            response
        );
    }

    #[test]
    fn scheduled_response_exposes_the_pinned_revision_state_and_time() {
        let response = PublishNowResponse {
            publication_id: Uuid::from_u128(0x018f_2046_49b2_7c2a_9226_f81c_87ab_721d),
            post_id: Uuid::from_u128(0x123e_4567_e89b_12d3_a456_4266_1417_4000),
            preview_digest: PreviewDigest::parse(PREVIEW_DIGEST).unwrap(),
            revision: REVISION.into(),
            state: PublicationApprovalState::Scheduled,
            scheduled_for: Some(OffsetDateTime::from_unix_timestamp(86_400).unwrap()),
            published_at: None,
            site_digest: SITE_DIGEST.into(),
            site_version: 7,
        };

        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value, scheduled_response_value());
        assert_eq!(
            serde_json::from_value::<PublishNowResponse>(value).unwrap(),
            response
        );
    }

    #[test]
    fn approval_states_have_stable_wire_names() {
        for (state, name) in [
            (PublicationApprovalState::Scheduled, "scheduled"),
            (PublicationApprovalState::Published, "published"),
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), json!(name));
            assert_eq!(
                serde_json::from_value::<PublicationApprovalState>(json!(name)).unwrap(),
                state
            );
        }

        assert!(serde_json::from_value::<PublicationApprovalState>(json!("unknown")).is_err());
    }

    #[test]
    fn publish_now_response_rejects_malformed_success_fields() {
        let cases = [
            (
                "preview_digest",
                json!(PREVIEW_DIGEST.replacen("1234", "ABCD", 1)),
            ),
            ("revision", json!(&REVISION[..REVISION.len() - 1])),
            ("revision", json!(REVISION.replacen("abcdef", "ABCDEF", 1))),
            ("site_digest", json!(REVISION)),
            ("scheduled_for", json!("1970-01-02T01:00:00+01:00")),
            ("published_at", json!("1970-01-01T01:00:00+01:00")),
            ("site_version", json!(0)),
        ];

        for (field, malformed) in cases {
            let mut value = published_response_value();
            value[field] = malformed;

            let error = serde_json::from_value::<PublishNowResponse>(value).unwrap_err();
            assert!(error.to_string().contains(field), "{field}: {error}");
        }

        let mut missing_state = published_response_value();
        missing_state.as_object_mut().unwrap().remove("state");
        let error = serde_json::from_value::<PublishNowResponse>(missing_state).unwrap_err();
        assert!(error.to_string().contains("state"), "{error}");
    }
}
