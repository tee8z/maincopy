//! Wire contracts for listing posts loaded by the content compiler.

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

/// Versioned path for listing loaded post revisions.
pub const POSTS_PATH: &str = "/api/admin/v1/posts";

/// Canonical publication state of one loaded post revision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum PostPublicationState {
    Draft,
    Unpublished,
    /// `revision` is a newer previewable revision while the approved revision stays public.
    #[serde(rename = "unpublished_change")]
    UnpublishedChange,
    Published,
}

/// Summary of one post revision loaded from the Git-owned content tree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct PostSummary {
    pub post_id: Uuid,
    pub source_path: Box<str>,
    pub title: Box<str>,
    pub slug: Box<str>,
    #[serde(deserialize_with = "deserialize_post_revision")]
    #[cfg_attr(feature = "schema", schema(pattern = r"^post-b3-v1-[0-9a-f]{64}$"))]
    pub revision: Box<str>,
    pub publication_state: PostPublicationState,
    #[serde(
        serialize_with = "time::serde::rfc3339::option::serialize",
        deserialize_with = "deserialize_optional_utc_timestamp"
    )]
    pub published_at: Option<OffsetDateTime>,
}

/// One cursor-paginated page of posts from an immutable site revision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ListPostsResponse {
    #[serde(deserialize_with = "deserialize_content_digest")]
    #[cfg_attr(feature = "schema", schema(pattern = r"^content-b3-v1-[0-9a-f]{64}$"))]
    pub content_digest: Box<str>,
    #[serde(deserialize_with = "deserialize_site_digest")]
    #[cfg_attr(feature = "schema", schema(pattern = r"^site-b3-v1-[0-9a-f]{64}$"))]
    pub site_digest: Box<str>,
    #[serde(deserialize_with = "deserialize_site_version")]
    #[cfg_attr(feature = "schema", schema(minimum = 1))]
    pub site_version: u64,
    pub posts: Vec<PostSummary>,
    pub next_cursor: Option<Uuid>,
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

fn deserialize_content_digest<'de, D>(deserializer: D) -> Result<Box<str>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_digest(deserializer, "content-b3-v1-", "content_digest")
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

fn deserialize_optional_utc_timestamp<'de, D>(
    deserializer: D,
) -> Result<Option<OffsetDateTime>, D::Error>
where
    D: Deserializer<'de>,
{
    let timestamp = time::serde::rfc3339::option::deserialize(deserializer)?;
    match timestamp {
        Some(timestamp) if timestamp.offset() != UtcOffset::UTC => {
            Err(D::Error::custom("published_at must use the UTC offset"))
        }
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
    const SITE_DIGEST: &str =
        "site-b3-v1-abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    const CONTENT_DIGEST: &str =
        "content-b3-v1-abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn response_value() -> serde_json::Value {
        json!({
            "content_digest": CONTENT_DIGEST,
            "site_digest": SITE_DIGEST,
            "site_version": 7,
            "posts": [
                {
                    "post_id": "123e4567-e89b-12d3-a456-426614174000",
                    "source_path": "posts/sqlite.md",
                    "title": "SQLite Does Not Need a Network",
                    "slug": "sqlite-does-not-need-a-network",
                    "revision": REVISION,
                    "publication_state": "published",
                    "published_at": "1970-01-01T00:00:00Z"
                },
                {
                    "post_id": "018f2046-49b2-7c2a-9226-f81c87ab721d",
                    "source_path": "drafts/next.md",
                    "title": "Next post",
                    "slug": "next-post",
                    "revision": REVISION,
                    "publication_state": "draft",
                    "published_at": null
                }
            ],
            "next_cursor": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        })
    }

    #[test]
    fn posts_path_is_the_versioned_admin_resource() {
        assert_eq!(POSTS_PATH, "/api/admin/v1/posts");
    }

    #[test]
    fn list_posts_response_has_a_stable_bidirectional_wire_contract() {
        let response = ListPostsResponse {
            content_digest: CONTENT_DIGEST.into(),
            site_digest: SITE_DIGEST.into(),
            site_version: 7,
            posts: vec![
                PostSummary {
                    post_id: Uuid::from_u128(0x123e_4567_e89b_12d3_a456_4266_1417_4000),
                    source_path: "posts/sqlite.md".into(),
                    title: "SQLite Does Not Need a Network".into(),
                    slug: "sqlite-does-not-need-a-network".into(),
                    revision: REVISION.into(),
                    publication_state: PostPublicationState::Published,
                    published_at: Some(OffsetDateTime::UNIX_EPOCH),
                },
                PostSummary {
                    post_id: Uuid::from_u128(0x018f_2046_49b2_7c2a_9226_f81c_87ab_721d),
                    source_path: "drafts/next.md".into(),
                    title: "Next post".into(),
                    slug: "next-post".into(),
                    revision: REVISION.into(),
                    publication_state: PostPublicationState::Draft,
                    published_at: None,
                },
            ],
            next_cursor: Some(Uuid::from_u128(0xaaaa_aaaa_aaaa_4aaa_8aaa_aaaa_aaaa_aaaa)),
        };

        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value, response_value());
        assert_eq!(
            serde_json::from_value::<ListPostsResponse>(value).unwrap(),
            response
        );
    }

    #[test]
    fn publication_states_have_stable_wire_names() {
        for (state, name) in [
            (PostPublicationState::Draft, "draft"),
            (PostPublicationState::Unpublished, "unpublished"),
            (
                PostPublicationState::UnpublishedChange,
                "unpublished_change",
            ),
            (PostPublicationState::Published, "published"),
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), json!(name));
            assert_eq!(
                serde_json::from_value::<PostPublicationState>(json!(name)).unwrap(),
                state
            );
        }

        assert!(serde_json::from_value::<PostPublicationState>(json!("unknown")).is_err());
    }

    #[test]
    fn unpublished_change_marks_the_loaded_revision_as_preview_only() {
        let summary = PostSummary {
            post_id: Uuid::from_u128(0x123e_4567_e89b_12d3_a456_4266_1417_4000),
            source_path: "posts/sqlite.md".into(),
            title: "SQLite Does Not Need a Network".into(),
            slug: "sqlite-does-not-need-a-network".into(),
            revision: REVISION.into(),
            publication_state: PostPublicationState::UnpublishedChange,
            published_at: Some(OffsetDateTime::UNIX_EPOCH),
        };

        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(value["revision"], REVISION);
        assert_eq!(value["publication_state"], "unpublished_change");
        assert_eq!(value["published_at"], "1970-01-01T00:00:00Z");
        assert_eq!(
            serde_json::from_value::<PostSummary>(value).unwrap(),
            summary
        );
    }

    #[test]
    fn list_posts_response_rejects_malformed_typed_fields() {
        let cases = [
            ("revision", json!(&REVISION[..REVISION.len() - 1])),
            ("revision", json!(REVISION.replacen("abcdef", "ABCDEF", 1))),
            ("content_digest", json!(SITE_DIGEST)),
            ("site_digest", json!(REVISION)),
            ("published_at", json!("1970-01-01T01:00:00+01:00")),
            ("site_version", json!(0)),
        ];

        for (field, malformed) in cases {
            let mut value = response_value();
            match field {
                "revision" | "published_at" => value["posts"][0][field] = malformed,
                _ => value[field] = malformed,
            }

            let error = serde_json::from_value::<ListPostsResponse>(value).unwrap_err();
            assert!(error.to_string().contains(field), "{field}: {error}");
        }
    }

    #[test]
    fn list_posts_response_rejects_invalid_identifiers_and_states() {
        for (path, malformed) in [
            (&["posts", "0", "post_id"][..], json!("not-a-uuid")),
            (&["posts", "0", "publication_state"][..], json!("unknown")),
            (&["next_cursor"][..], json!("not-a-uuid")),
        ] {
            let mut value = response_value();
            let mut target = &mut value;
            for segment in path.iter().take(path.len() - 1) {
                target = if let Ok(index) = segment.parse::<usize>() {
                    &mut target[index]
                } else {
                    &mut target[*segment]
                };
            }
            target[path[path.len() - 1]] = malformed;

            assert!(serde_json::from_value::<ListPostsResponse>(value).is_err());
        }
    }
}
