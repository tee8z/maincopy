use std::{fmt, io, sync::Arc};

use markdown_compiler::{PostDescription, PostId, PostTitle, PublicationSettings};
use maud::html;
use quick_xml::{
    Writer,
    events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event},
};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc2822};

use crate::domain::publication::CanonicalSiteUrl;

const MAX_RSS_BYTES: usize = 40 * 1024 * 1024;
const FEED_DIGEST_CONTEXT: &str = "maincopy RSS feed bytes v1";
const FEED_DIGEST_PREFIX: &str = "feed-b3-v1-";

/// One already-selected public post projected into an RSS item.
pub(crate) struct RssItem<'a> {
    pub(crate) post_id: &'a PostId,
    pub(crate) title: &'a PostTitle,
    pub(crate) description: &'a PostDescription,
    pub(crate) canonical_url: &'a CanonicalSiteUrl,
    pub(crate) published_at: OffsetDateTime,
}

/// Exact immutable RSS bytes and their strong presentation identity.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct RenderedRssFeed {
    pub(crate) body: Arc<str>,
    pub(crate) digest: FeedDigest,
}

impl fmt::Debug for RenderedRssFeed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RenderedRssFeed")
            .field("digest", &self.digest)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// Domain-separated identity of the exact serialized feed bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct FeedDigest([u8; 32]);

impl fmt::Display for FeedDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{FEED_DIGEST_PREFIX}{}",
            blake3::Hash::from_bytes(self.0).to_hex()
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RssTextField {
    SiteTitle,
    SiteDescription,
    ChannelLink,
    FeedLink,
    ItemTitle,
    ItemDescription,
    ItemLink,
    ItemGuid,
}

impl fmt::Display for RssTextField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SiteTitle => "site title",
            Self::SiteDescription => "site description",
            Self::ChannelLink => "channel link",
            Self::FeedLink => "feed link",
            Self::ItemTitle => "item title",
            Self::ItemDescription => "item description",
            Self::ItemLink => "item link",
            Self::ItemGuid => "item GUID",
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum RssRenderError {
    #[error("RSS {field} contains XML 1.0-illegal character U+{code_point:04X}")]
    IllegalXmlCharacter {
        field: RssTextField,
        post_id: Option<PostId>,
        code_point: u32,
    },
    #[error("published timestamp for RSS item {post_id} cannot be represented as RFC 2822 UTC")]
    PublishedAtNotRepresentable {
        post_id: PostId,
        #[source]
        source: time::error::Format,
    },
    #[error("RSS feed exceeds the inclusive {max_bytes}-byte output limit")]
    OutputTooLarge { max_bytes: usize },
    #[error("RSS XML serialization produced non-UTF-8 output")]
    InvalidUtf8(#[source] std::string::FromUtf8Error),
}

pub(crate) fn render_rss<'a>(
    publication: &PublicationSettings,
    feed_url: &CanonicalSiteUrl,
    items: impl IntoIterator<Item = RssItem<'a>>,
) -> Result<RenderedRssFeed, RssRenderError> {
    render_rss_with_limit(publication, feed_url, items, MAX_RSS_BYTES)
}

fn render_rss_with_limit<'a>(
    publication: &PublicationSettings,
    feed_url: &CanonicalSiteUrl,
    items: impl IntoIterator<Item = RssItem<'a>>,
    max_bytes: usize,
) -> Result<RenderedRssFeed, RssRenderError> {
    validate_xml_text(
        publication.site.title.as_str(),
        RssTextField::SiteTitle,
        None,
    )?;
    validate_xml_text(
        publication.site.description.as_str(),
        RssTextField::SiteDescription,
        None,
    )?;
    validate_xml_text(
        publication.site.base_url.as_str(),
        RssTextField::ChannelLink,
        None,
    )?;
    validate_xml_text(feed_url.as_str(), RssTextField::FeedLink, None)?;

    let mut writer = Writer::new_with_indent(BoundedOutput::new(max_bytes), b' ', 2);
    write_event(
        &mut writer,
        Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)),
        max_bytes,
    )?;

    let mut rss = BytesStart::new("rss");
    rss.push_attribute(("version", "2.0"));
    rss.push_attribute(("xmlns:atom", "http://www.w3.org/2005/Atom"));
    write_event(&mut writer, Event::Start(rss), max_bytes)?;
    write_event(
        &mut writer,
        Event::Start(BytesStart::new("channel")),
        max_bytes,
    )?;

    write_text_element(
        &mut writer,
        "title",
        publication.site.title.as_str(),
        max_bytes,
    )?;
    write_text_element(
        &mut writer,
        "link",
        publication.site.base_url.as_str(),
        max_bytes,
    )?;
    write_text_element(
        &mut writer,
        "description",
        publication.site.description.as_str(),
        max_bytes,
    )?;

    let mut self_link = BytesStart::new("atom:link");
    self_link.push_attribute(("href", feed_url.as_str()));
    self_link.push_attribute(("rel", "self"));
    self_link.push_attribute(("type", "application/rss+xml"));
    write_event(&mut writer, Event::Empty(self_link), max_bytes)?;

    for item in items {
        write_item(&mut writer, item, max_bytes)?;
    }

    write_event(&mut writer, Event::End(BytesEnd::new("channel")), max_bytes)?;
    write_event(&mut writer, Event::End(BytesEnd::new("rss")), max_bytes)?;

    let bytes = writer.into_inner().into_bytes();
    let digest = feed_digest(&bytes);
    let body = String::from_utf8(bytes)
        .map(Arc::<str>::from)
        .map_err(RssRenderError::InvalidUtf8)?;
    Ok(RenderedRssFeed { body, digest })
}

fn write_item(
    writer: &mut Writer<BoundedOutput>,
    item: RssItem<'_>,
    max_bytes: usize,
) -> Result<(), RssRenderError> {
    validate_item(&item)?;
    let published_at = item
        .published_at
        .to_offset(UtcOffset::UTC)
        .format(&Rfc2822)
        .map_err(|source| RssRenderError::PublishedAtNotRepresentable {
            post_id: item.post_id.clone(),
            source,
        })?;

    write_event(writer, Event::Start(BytesStart::new("item")), max_bytes)?;
    write_text_element(writer, "title", item.title.as_str(), max_bytes)?;
    write_text_element(writer, "link", item.canonical_url.as_str(), max_bytes)?;
    write_plain_text_description(writer, "description", item.description.as_str(), max_bytes)?;

    let mut guid = BytesStart::new("guid");
    guid.push_attribute(("isPermaLink", "false"));
    write_event(writer, Event::Start(guid), max_bytes)?;
    write_event(
        writer,
        Event::Text(BytesText::new(item.post_id.as_str())),
        max_bytes,
    )?;
    write_event(writer, Event::End(BytesEnd::new("guid")), max_bytes)?;

    write_text_element(writer, "pubDate", &published_at, max_bytes)?;
    write_event(writer, Event::End(BytesEnd::new("item")), max_bytes)?;
    Ok(())
}

fn validate_item(item: &RssItem<'_>) -> Result<(), RssRenderError> {
    for (value, field) in [
        (item.title.as_str(), RssTextField::ItemTitle),
        (item.description.as_str(), RssTextField::ItemDescription),
        (item.canonical_url.as_str(), RssTextField::ItemLink),
        (item.post_id.as_str(), RssTextField::ItemGuid),
    ] {
        validate_xml_text(value, field, Some(item.post_id))?;
    }
    Ok(())
}

fn validate_xml_text(
    value: &str,
    field: RssTextField,
    post_id: Option<&PostId>,
) -> Result<(), RssRenderError> {
    let Some(illegal) = value
        .chars()
        .find(|character| !is_xml_1_0_character(*character))
    else {
        return Ok(());
    };
    Err(RssRenderError::IllegalXmlCharacter {
        field,
        post_id: post_id.cloned(),
        code_point: illegal.into(),
    })
}

const fn is_xml_1_0_character(character: char) -> bool {
    matches!(
        character,
        '\u{0009}' | '\u{000A}' | '\u{000D}'
            | '\u{0020}'..='\u{D7FF}'
            | '\u{E000}'..='\u{FFFD}'
            | '\u{10000}'..='\u{10FFFF}'
    )
}

fn write_text_element(
    writer: &mut Writer<BoundedOutput>,
    name: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), RssRenderError> {
    writer
        .create_element(name)
        .write_text_content(BytesText::new(value))
        .map(|_| ())
        .map_err(|_| RssRenderError::OutputTooLarge { max_bytes })
}

/// RSS descriptions carry HTML, so encode authored text as an HTML text node
/// before the XML writer applies the document-level escaping.
fn write_plain_text_description(
    writer: &mut Writer<BoundedOutput>,
    name: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), RssRenderError> {
    let html_text = html! { (value) }.into_string();
    writer
        .create_element(name)
        .write_text_content(BytesText::new(&html_text))
        .map(|_| ())
        .map_err(|_| RssRenderError::OutputTooLarge { max_bytes })
}

fn write_event<'a>(
    writer: &mut Writer<BoundedOutput>,
    event: Event<'a>,
    max_bytes: usize,
) -> Result<(), RssRenderError> {
    writer
        .write_event(event)
        .map_err(|_| RssRenderError::OutputTooLarge { max_bytes })
}

fn feed_digest(bytes: &[u8]) -> FeedDigest {
    let mut hasher = blake3::Hasher::new_derive_key(FEED_DIGEST_CONTEXT);
    hasher.update(bytes);
    FeedDigest(*hasher.finalize().as_bytes())
}

struct BoundedOutput {
    bytes: Vec<u8>,
    max_bytes: usize,
}

impl BoundedOutput {
    const fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl io::Write for BoundedOutput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(buffer.len())
            .is_none_or(|length| length > self.max_bytes)
        {
            return Err(io::Error::other("RSS output limit reached"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use markdown_compiler::{
        AuthorName, AuthorSettings, DefaultPostTipPolicy, PostSlug, PublicationAssetSettings,
        PublicationBaseUrl, SiteDescription, SiteSettings, SiteTitle,
    };
    use quick_xml::{Reader, escape::unescape, events::Event};
    use time::format_description::well_known::Rfc3339;

    use super::*;
    use crate::domain::publication::PublicPagePath;

    const FIRST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const SECOND_ID: &str = "22222222-2222-4222-8222-222222222222";

    fn publication() -> PublicationSettings {
        PublicationSettings {
            site: SiteSettings {
                title: SiteTitle::new("Main & <Copy> — 東京").unwrap(),
                base_url: PublicationBaseUrl::parse("https://example.com/").unwrap(),
                description: SiteDescription::new("Notes, diagrams & careful prose.").unwrap(),
                favicon: None,
            },
            author: AuthorSettings {
                name: AuthorName::new("Example Author").unwrap(),
            },
            assets: PublicationAssetSettings::default(),
            tips: DefaultPostTipPolicy::Disabled,
        }
    }

    fn date(value: &str) -> OffsetDateTime {
        OffsetDateTime::parse(value, &Rfc3339).unwrap()
    }

    fn post_url(publication: &PublicationSettings, slug: &str) -> CanonicalSiteUrl {
        CanonicalSiteUrl::for_path(
            &publication.site.base_url,
            &PublicPagePath::post(&PostSlug::parse(slug).unwrap()),
        )
    }

    fn feed_url(publication: &PublicationSettings) -> CanonicalSiteUrl {
        CanonicalSiteUrl::for_path(&publication.site.base_url, &PublicPagePath::feed())
    }

    fn assert_well_formed(xml: &str) {
        let mut reader = Reader::from_str(xml);
        loop {
            match reader.read_event() {
                Ok(Event::Eof) => return,
                Ok(_) => {}
                Err(error) => panic!(
                    "RSS must be well-formed XML at byte {}: {error}",
                    reader.error_position()
                ),
            }
        }
    }

    fn parsed_element_text(xml: &str, element: &str) -> Vec<String> {
        let mut reader = Reader::from_str(xml);
        let mut values = Vec::new();
        loop {
            match reader.read_event().unwrap() {
                Event::Start(start) if start.name().as_ref() == element => {
                    let raw = reader.read_text(start.name()).unwrap();
                    values.push(unescape(raw.as_ref()).unwrap().into_owned());
                }
                Event::Eof => return values,
                _ => {}
            }
        }
    }

    #[test]
    fn renders_rss_contract_with_escaped_unicode_in_supplied_order() {
        let publication = publication();
        let feed_url = feed_url(&publication);
        let first_id = PostId::parse(FIRST_ID).unwrap();
        let second_id = PostId::parse(SECOND_ID).unwrap();
        let first_title = PostTitle::new("A & <B> \"quoted\" — café").unwrap();
        let second_title = PostTitle::new("Second").unwrap();
        let first_description =
            PostDescription::new("Plain text: 5 < 7 & emoji 🚀. <script>no</script> &lt;b&gt;")
                .unwrap();
        let second_description = PostDescription::new("Another summary.").unwrap();
        let first_url = post_url(&publication, "first");
        let second_url = post_url(&publication, "second");

        let rendered = render_rss(
            &publication,
            &feed_url,
            [
                RssItem {
                    post_id: &first_id,
                    title: &first_title,
                    description: &first_description,
                    canonical_url: &first_url,
                    published_at: date("2026-09-02T09:45:00-04:00"),
                },
                RssItem {
                    post_id: &second_id,
                    title: &second_title,
                    description: &second_description,
                    canonical_url: &second_url,
                    published_at: date("2026-08-31T12:00:00Z"),
                },
            ],
        )
        .unwrap();

        assert_well_formed(&rendered.body);
        assert!(
            rendered
                .body
                .starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>")
        );
        assert!(
            rendered
                .body
                .contains("<rss version=\"2.0\" xmlns:atom=\"http://www.w3.org/2005/Atom\">")
        );
        assert!(
            rendered
                .body
                .contains("<title>Main &amp; &lt;Copy&gt; — 東京</title>")
        );
        assert!(rendered.body.contains("<link>https://example.com/</link>"));
        assert!(
            rendered
                .body
                .contains("<description>Notes, diagrams &amp; careful prose.</description>")
        );
        assert!(rendered.body.contains(
            "<atom:link href=\"https://example.com/feed.xml\" rel=\"self\" type=\"application/rss+xml\"/>"
        ));
        assert!(
            rendered
                .body
                .contains("<title>A &amp; &lt;B&gt; &quot;quoted&quot; — café</title>")
        );
        assert!(
            rendered
                .body
                .contains("&amp;lt;script&amp;gt;no&amp;lt;/script&amp;gt;")
        );
        assert_eq!(
            parsed_element_text(&rendered.body, "description"),
            [
                "Notes, diagrams & careful prose.",
                "Plain text: 5 &lt; 7 &amp; emoji 🚀. &lt;script&gt;no&lt;/script&gt; &amp;lt;b&amp;gt;",
                "Another summary.",
            ]
        );
        assert!(
            rendered
                .body
                .contains(&format!("<guid isPermaLink=\"false\">{FIRST_ID}</guid>"))
        );
        assert!(
            rendered
                .body
                .contains("<pubDate>Wed, 02 Sep 2026 13:45:00 +0000</pubDate>")
        );
        assert!(
            rendered.body.find(FIRST_ID).unwrap() < rendered.body.find(SECOND_ID).unwrap(),
            "items must retain caller-supplied order"
        );
    }

    #[test]
    fn empty_feed_is_valid_and_contains_no_item_or_wall_clock_metadata() {
        let publication = publication();
        let rendered = render_rss(&publication, &feed_url(&publication), []).unwrap();

        assert_well_formed(&rendered.body);
        assert!(!rendered.body.contains("<item>"));
        assert!(!rendered.body.contains("lastBuildDate"));
    }

    #[test]
    fn rejects_xml_1_0_illegal_item_characters_with_field_and_identity() {
        let publication = publication();
        let post_id = PostId::parse(FIRST_ID).unwrap();
        let title = PostTitle::new("Illegal ￾").unwrap();
        let description = PostDescription::new("Summary").unwrap();
        let post_url = post_url(&publication, "illegal");

        let error = render_rss(
            &publication,
            &feed_url(&publication),
            [RssItem {
                post_id: &post_id,
                title: &title,
                description: &description,
                canonical_url: &post_url,
                published_at: date("2026-09-02T13:45:00Z"),
            }],
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RssRenderError::IllegalXmlCharacter {
                field: RssTextField::ItemTitle,
                post_id: Some(found),
                code_point: 0xFFFE,
            } if found == post_id
        ));
    }

    #[test]
    fn rejects_dates_that_rfc_2822_cannot_represent() {
        let publication = publication();
        let post_id = PostId::parse(FIRST_ID).unwrap();
        let title = PostTitle::new("Old post").unwrap();
        let description = PostDescription::new("Summary").unwrap();
        let post_url = post_url(&publication, "old-post");

        let error = render_rss(
            &publication,
            &feed_url(&publication),
            [RssItem {
                post_id: &post_id,
                title: &title,
                description: &description,
                canonical_url: &post_url,
                published_at: date("1899-12-31T23:59:59Z"),
            }],
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RssRenderError::PublishedAtNotRepresentable { post_id: found, .. }
                if found == post_id
        ));
    }

    #[test]
    fn feed_digest_is_deterministic_domain_separated_and_binds_exact_bytes() {
        let publication = publication();
        let feed_url = feed_url(&publication);
        let first = render_rss(&publication, &feed_url, []).unwrap();
        let second = render_rss(&publication, &feed_url, []).unwrap();

        assert_eq!(first, second);
        let encoded = first.digest.to_string();
        assert!(encoded.starts_with(FEED_DIGEST_PREFIX));
        assert_eq!(encoded.len(), FEED_DIGEST_PREFIX.len() + 64);

        let mut expected = blake3::Hasher::new_derive_key(FEED_DIGEST_CONTEXT);
        expected.update(first.body.as_bytes());
        assert_eq!(
            encoded,
            format!("{FEED_DIGEST_PREFIX}{}", expected.finalize().to_hex())
        );

        let changed = feed_digest(format!("{}\n", first.body).as_bytes());
        assert_ne!(changed, first.digest);
    }

    #[test]
    fn slug_change_preserves_guid_while_changing_link_and_feed_digest() {
        let publication = publication();
        let feed_url = feed_url(&publication);
        let post_id = PostId::parse(FIRST_ID).unwrap();
        let title = PostTitle::new("Stable identity").unwrap();
        let description = PostDescription::new("The route may change.").unwrap();
        let original_url = post_url(&publication, "original-slug");
        let renamed_url = post_url(&publication, "renamed-slug");
        let published_at = date("2026-09-02T13:45:00Z");

        let original = render_rss(
            &publication,
            &feed_url,
            [RssItem {
                post_id: &post_id,
                title: &title,
                description: &description,
                canonical_url: &original_url,
                published_at,
            }],
        )
        .unwrap();
        let renamed = render_rss(
            &publication,
            &feed_url,
            [RssItem {
                post_id: &post_id,
                title: &title,
                description: &description,
                canonical_url: &renamed_url,
                published_at,
            }],
        )
        .unwrap();

        let stable_guid = format!("<guid isPermaLink=\"false\">{FIRST_ID}</guid>");
        assert!(original.body.contains(&stable_guid));
        assert!(renamed.body.contains(&stable_guid));
        assert!(
            original
                .body
                .contains("<link>https://example.com/posts/original-slug</link>")
        );
        assert!(
            renamed
                .body
                .contains("<link>https://example.com/posts/renamed-slug</link>")
        );
        assert_ne!(original.digest, renamed.digest);
    }

    #[test]
    fn bounded_output_accepts_its_inclusive_limit_and_rejects_the_next_byte() {
        assert_eq!(MAX_RSS_BYTES, 40 * 1024 * 1024);
        let mut output = BoundedOutput::new(4);

        output.write_all(b"1234").unwrap();
        assert_eq!(output.bytes, b"1234");
        assert!(output.write_all(b"5").is_err());
        assert_eq!(output.bytes, b"1234");
    }

    #[test]
    fn renderer_maps_the_exact_output_boundary_to_a_typed_error() {
        let publication = publication();
        let feed_url = feed_url(&publication);
        let baseline = render_rss(&publication, &feed_url, []).unwrap();
        let exact =
            render_rss_with_limit(&publication, &feed_url, [], baseline.body.len()).unwrap();
        assert_eq!(exact, baseline);

        let max_bytes = baseline.body.len() - 1;
        assert!(matches!(
            render_rss_with_limit(&publication, &feed_url, [], max_bytes),
            Err(RssRenderError::OutputTooLarge { max_bytes: found }) if found == max_bytes
        ));
    }
}
