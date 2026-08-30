//! Wire contracts for immediate publication commands.

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

/// Versioned path for creating and immediately publishing a publication.
pub const PUBLICATIONS_PATH: &str = "/api/admin/v1/publications";

/// Header that identifies retries of the same publication command.
pub const IDEMPOTENCY_KEY_HEADER: &str = "Idempotency-Key";

/// Selects the post revision to publish immediately.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct PublishNowRequest {
    pub post_id: Uuid,
    pub expected_revision: Option<Box<str>>,
}

/// Reports the durable publication and site snapshot made visible by a command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct PublishNowResponse {
    pub publication_id: Uuid,
    pub post_id: Uuid,
    #[serde(deserialize_with = "deserialize_post_revision")]
    #[cfg_attr(feature = "schema", schema(pattern = r"^post-b3-v1-[0-9a-f]{64}$"))]
    pub revision: Box<str>,
    #[serde(
        serialize_with = "time::serde::rfc3339::serialize",
        deserialize_with = "deserialize_utc_timestamp"
    )]
    pub published_at: OffsetDateTime,
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

fn deserialize_utc_timestamp<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
where
    D: Deserializer<'de>,
{
    let timestamp = time::serde::rfc3339::deserialize(deserializer)?;
    if timestamp.offset() == UtcOffset::UTC {
        Ok(timestamp)
    } else {
        Err(D::Error::custom("published_at must use the UTC offset"))
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
    const SITE_DIGEST: &str =
        "site-b3-v1-abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn response_value() -> serde_json::Value {
        json!({
            "publication_id": "018f2046-49b2-7c2a-9226-f81c87ab721d",
            "post_id": "123e4567-e89b-12d3-a456-426614174000",
            "revision": REVISION,
            "published_at": "1970-01-01T00:00:00Z",
            "site_digest": SITE_DIGEST,
            "site_version": 7
        })
    }

    #[test]
    fn publish_now_request_has_a_stable_bidirectional_wire_contract() {
        let request = PublishNowRequest {
            post_id: Uuid::from_u128(0x123e_4567_e89b_12d3_a456_4266_1417_4000),
            expected_revision: Some(REVISION.into()),
        };

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value,
            json!({
                "post_id": "123e4567-e89b-12d3-a456-426614174000",
                "expected_revision": REVISION
            })
        );
        assert_eq!(
            serde_json::from_value::<PublishNowRequest>(value).unwrap(),
            request
        );
    }

    #[test]
    fn request_revision_remains_server_validated() {
        let request: PublishNowRequest = serde_json::from_value(json!({
            "post_id": "123e4567-e89b-12d3-a456-426614174000",
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
            "expected_revison": REVISION
        }))
        .unwrap_err();

        assert!(error.to_string().contains("expected_revison"));
    }

    #[test]
    fn publish_now_response_has_a_stable_bidirectional_wire_contract() {
        let response = PublishNowResponse {
            publication_id: Uuid::from_u128(0x018f_2046_49b2_7c2a_9226_f81c_87ab_721d),
            post_id: Uuid::from_u128(0x123e_4567_e89b_12d3_a456_4266_1417_4000),
            revision: REVISION.into(),
            published_at: OffsetDateTime::UNIX_EPOCH,
            site_digest: SITE_DIGEST.into(),
            site_version: 7,
        };

        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value, response_value());
        assert_eq!(
            serde_json::from_value::<PublishNowResponse>(value).unwrap(),
            response
        );
    }

    #[test]
    fn publish_now_response_rejects_malformed_success_fields() {
        let cases = [
            ("revision", json!(&REVISION[..REVISION.len() - 1])),
            ("revision", json!(REVISION.replacen("abcdef", "ABCDEF", 1))),
            ("site_digest", json!(REVISION)),
            ("published_at", json!("1970-01-01T01:00:00+01:00")),
            ("site_version", json!(0)),
        ];

        for (field, malformed) in cases {
            let mut value = response_value();
            value[field] = malformed;

            let error = serde_json::from_value::<PublishNowResponse>(value).unwrap_err();
            assert!(error.to_string().contains(field), "{field}: {error}");
        }
    }
}
