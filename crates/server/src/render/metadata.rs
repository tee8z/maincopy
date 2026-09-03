use std::fmt;

use markdown_compiler::{AuthorName, PostDescription, PostTag, PostTitle};
use maud::{Markup, PreEscaped, html};
use serde::Serialize;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::domain::publication::CanonicalSiteUrl;

/// Validated inputs for one post's Open Graph and JSON-LD metadata.
pub(crate) struct PostHeadMetadataInput<'metadata> {
    pub(crate) title: &'metadata PostTitle,
    pub(crate) description: &'metadata PostDescription,
    pub(crate) tags: &'metadata [PostTag],
    pub(crate) authored_at: OffsetDateTime,
    pub(crate) updated_at: Option<OffsetDateTime>,
    pub(crate) published_at: Option<OffsetDateTime>,
    pub(crate) canonical_url: &'metadata CanonicalSiteUrl,
    pub(crate) author: &'metadata AuthorName,
}

/// Exact safe metadata fragments used by the Maud page shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RenderedPostHeadMetadata {
    pub(crate) published_time: Option<Box<str>>,
    pub(crate) modified_time: Option<Box<str>>,
    json_ld: Box<str>,
}

impl RenderedPostHeadMetadata {
    /// The only trusted sink for serialized JSON-LD in an HTML script element.
    pub(crate) fn json_ld_script(&self) -> Markup {
        // Construction escapes every character that can end the script text node.
        html! {
            script type="application/ld+json" {
                (PreEscaped(self.json_ld.to_string()))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MetadataTimeField {
    Authored,
    Updated,
    Published,
}

impl fmt::Display for MetadataTimeField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Authored => "authored_at",
            Self::Updated => "updated_at",
            Self::Published => "published_at",
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum MetadataRenderError {
    #[error("{field} cannot be represented as an RFC 3339 metadata timestamp")]
    Timestamp {
        field: MetadataTimeField,
        #[source]
        source: time::error::Format,
    },
    #[error("BlogPosting JSON-LD serialization failed")]
    JsonLd(#[source] serde_json::Error),
}

pub(crate) fn render_post_head_metadata(
    input: PostHeadMetadataInput<'_>,
) -> Result<RenderedPostHeadMetadata, MetadataRenderError> {
    let authored_time = metadata_time(input.authored_at, MetadataTimeField::Authored)?;
    let modified_time = input
        .updated_at
        .map(|value| metadata_time(value, MetadataTimeField::Updated))
        .transpose()?;
    let published_time = input
        .published_at
        .map(|value| metadata_time(value, MetadataTimeField::Published))
        .transpose()?;

    let document = BlogPostingJsonLd {
        context: "https://schema.org",
        kind: "BlogPosting",
        headline: input.title.as_str(),
        description: input.description.as_str(),
        url: input.canonical_url.as_str(),
        main_entity_of_page: input.canonical_url.as_str(),
        date_created: &authored_time,
        date_published: published_time.as_deref(),
        date_modified: modified_time.as_deref(),
        author: BlogPostingAuthor {
            kind: "Person",
            name: input.author.as_str(),
        },
        keywords: input.tags,
    };
    let serialized = serde_json::to_string(&document).map_err(MetadataRenderError::JsonLd)?;
    let json_ld = escape_json_for_html_script(&serialized).into_boxed_str();

    Ok(RenderedPostHeadMetadata {
        published_time: published_time.map(String::into_boxed_str),
        modified_time: modified_time.map(String::into_boxed_str),
        json_ld,
    })
}

fn metadata_time(
    value: OffsetDateTime,
    field: MetadataTimeField,
) -> Result<String, MetadataRenderError> {
    value
        .format(&Rfc3339)
        .map_err(|source| MetadataRenderError::Timestamp { field, source })
}

/// Escapes JSON characters that can change an HTML script element's text boundary.
fn escape_json_for_html_script(json: &str) -> String {
    let mut output = String::with_capacity(json.len());
    for character in json.chars() {
        match character {
            '<' => output.push_str("\\u003c"),
            '>' => output.push_str("\\u003e"),
            '&' => output.push_str("\\u0026"),
            '\u{2028}' => output.push_str("\\u2028"),
            '\u{2029}' => output.push_str("\\u2029"),
            _ => output.push(character),
        }
    }
    output
}

#[derive(Serialize)]
struct BlogPostingJsonLd<'metadata> {
    #[serde(rename = "@context")]
    context: &'static str,
    #[serde(rename = "@type")]
    kind: &'static str,
    headline: &'metadata str,
    description: &'metadata str,
    url: &'metadata str,
    #[serde(rename = "mainEntityOfPage")]
    main_entity_of_page: &'metadata str,
    #[serde(rename = "dateCreated")]
    date_created: &'metadata str,
    #[serde(rename = "datePublished", skip_serializing_if = "Option::is_none")]
    date_published: Option<&'metadata str>,
    #[serde(rename = "dateModified", skip_serializing_if = "Option::is_none")]
    date_modified: Option<&'metadata str>,
    author: BlogPostingAuthor<'metadata>,
    keywords: &'metadata [PostTag],
}

#[derive(Serialize)]
struct BlogPostingAuthor<'metadata> {
    #[serde(rename = "@type")]
    kind: &'static str,
    name: &'metadata str,
}

#[cfg(test)]
mod tests {
    use markdown_compiler::PublicationBaseUrl;
    use serde_json::Value;
    use time::{Date, Month, Time};

    use super::*;
    use crate::domain::publication::PublicPagePath;

    fn canonical_url() -> CanonicalSiteUrl {
        CanonicalSiteUrl::for_path(
            &PublicationBaseUrl::parse("https://example.com/").unwrap(),
            &PublicPagePath::post(&markdown_compiler::PostSlug::parse("metadata").unwrap()),
        )
    }

    fn timestamp(value: &str) -> OffsetDateTime {
        OffsetDateTime::parse(value, &Rfc3339).unwrap()
    }

    #[test]
    fn renders_deterministic_blog_posting_with_exact_rfc3339_times() {
        let title = PostTitle::new("Metadata post").unwrap();
        let description = PostDescription::new("Structured description.").unwrap();
        let tags = [
            PostTag::parse("rust").unwrap(),
            PostTag::parse("web").unwrap(),
        ];
        let author = AuthorName::new("Maincopy Author").unwrap();
        let canonical_url = canonical_url();
        let input = || PostHeadMetadataInput {
            title: &title,
            description: &description,
            tags: &tags,
            authored_at: timestamp("2026-08-31T10:11:12-04:00"),
            updated_at: Some(timestamp("2026-09-01T11:12:13-04:00")),
            published_at: Some(timestamp("2026-09-02T15:16:17Z")),
            canonical_url: &canonical_url,
            author: &author,
        };

        let first = render_post_head_metadata(input()).unwrap();
        let second = render_post_head_metadata(input()).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            first.published_time.as_deref(),
            Some("2026-09-02T15:16:17Z")
        );
        assert_eq!(
            first.modified_time.as_deref(),
            Some("2026-09-01T11:12:13-04:00")
        );
        let value: Value = serde_json::from_str(&first.json_ld).unwrap();
        assert_eq!(value["@context"], "https://schema.org");
        assert_eq!(value["@type"], "BlogPosting");
        assert_eq!(value["headline"], "Metadata post");
        assert_eq!(value["description"], "Structured description.");
        assert_eq!(value["url"], "https://example.com/posts/metadata");
        assert_eq!(value["mainEntityOfPage"], value["url"]);
        assert_eq!(value["dateCreated"], "2026-08-31T10:11:12-04:00");
        assert_eq!(value["datePublished"], "2026-09-02T15:16:17Z");
        assert_eq!(value["dateModified"], "2026-09-01T11:12:13-04:00");
        assert_eq!(value["author"]["@type"], "Person");
        assert_eq!(value["author"]["name"], "Maincopy Author");
        assert_eq!(value["keywords"], serde_json::json!(["rust", "web"]));
    }

    #[test]
    fn unpublished_metadata_omits_publication_and_modification_times() {
        let title = PostTitle::new("Draft metadata").unwrap();
        let description = PostDescription::new("Not published yet.").unwrap();
        let author = AuthorName::new("Maincopy Author").unwrap();
        let canonical_url = canonical_url();
        let rendered = render_post_head_metadata(PostHeadMetadataInput {
            title: &title,
            description: &description,
            tags: &[],
            authored_at: timestamp("2026-08-31T10:11:12Z"),
            updated_at: None,
            published_at: None,
            canonical_url: &canonical_url,
            author: &author,
        })
        .unwrap();

        assert_eq!(rendered.published_time, None);
        assert_eq!(rendered.modified_time, None);
        let value: Value = serde_json::from_str(&rendered.json_ld).unwrap();
        assert!(value.get("datePublished").is_none());
        assert!(value.get("dateModified").is_none());
        assert_eq!(value["keywords"], serde_json::json!([]));
    }

    #[test]
    fn hostile_text_remains_json_data_and_cannot_close_the_script_element() {
        let title = PostTitle::new("</ScRiPt><script>alert(\"title\\\\path\")</script>").unwrap();
        let description =
            PostDescription::new("Ampersand & separators \u{2028} \u{2029} stay data.").unwrap();
        let author = AuthorName::new("Author <&>").unwrap();
        let canonical_url = canonical_url();
        let rendered = render_post_head_metadata(PostHeadMetadataInput {
            title: &title,
            description: &description,
            tags: &[],
            authored_at: timestamp("2026-08-31T10:11:12Z"),
            updated_at: None,
            published_at: None,
            canonical_url: &canonical_url,
            author: &author,
        })
        .unwrap();

        let script = rendered.json_ld_script().into_string();
        assert_eq!(
            script
                .matches("<script type=\"application/ld+json\">")
                .count(),
            1
        );
        assert_eq!(script.matches("</script>").count(), 1);
        let json = script
            .strip_prefix("<script type=\"application/ld+json\">")
            .unwrap()
            .strip_suffix("</script>")
            .unwrap();
        assert!(
            !json
                .chars()
                .any(|character| matches!(character, '<' | '>' | '&' | '\u{2028}' | '\u{2029}'))
        );
        assert!(!json.to_ascii_lowercase().contains("</script"));
        let value: Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["headline"], title.as_str());
        assert_eq!(value["description"], description.as_str());
        assert_eq!(value["author"]["name"], author.as_str());
    }

    #[test]
    fn unrepresentable_timestamp_reports_its_exact_field() {
        let title = PostTitle::new("Metadata post").unwrap();
        let description = PostDescription::new("Structured description.").unwrap();
        let author = AuthorName::new("Maincopy Author").unwrap();
        let canonical_url = canonical_url();
        let error = render_post_head_metadata(PostHeadMetadataInput {
            title: &title,
            description: &description,
            tags: &[],
            authored_at: Date::from_calendar_date(-1, Month::January, 1)
                .unwrap()
                .with_time(Time::MIDNIGHT)
                .assume_utc(),
            updated_at: None,
            published_at: None,
            canonical_url: &canonical_url,
            author: &author,
        })
        .unwrap_err();

        assert!(matches!(
            error,
            MetadataRenderError::Timestamp {
                field: MetadataTimeField::Authored,
                ..
            }
        ));
    }
}
