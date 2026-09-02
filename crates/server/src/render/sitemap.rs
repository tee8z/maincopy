use std::{fmt, io, sync::Arc};

use quick_xml::{
    Writer,
    events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event},
};
use thiserror::Error;

use crate::domain::publication::CanonicalSiteUrl;

const MAX_SITEMAP_URLS: usize = 50_000;
const MAX_SITEMAP_LOCATION_CHARACTERS: usize = 2_048;
const MAX_SITEMAP_BYTES: usize = 40 * 1024 * 1024;
const SITEMAP_DIGEST_CONTEXT: &str = "maincopy sitemap bytes v1";
const SITEMAP_DIGEST_PREFIX: &str = "sitemap-b3-v1-";

/// Exact immutable sitemap bytes and their strong presentation identity.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct RenderedSitemap {
    pub(crate) body: Arc<str>,
    pub(crate) digest: SitemapDigest,
}

impl fmt::Debug for RenderedSitemap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RenderedSitemap")
            .field("digest", &self.digest)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// Domain-separated identity of the exact serialized sitemap bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SitemapDigest([u8; 32]);

impl fmt::Display for SitemapDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{SITEMAP_DIGEST_PREFIX}{}",
            blake3::Hash::from_bytes(self.0).to_hex()
        )
    }
}

#[derive(Debug, Error)]
pub(crate) enum SitemapRenderError {
    #[error("sitemap contains more than the allowed {max_urls} URLs")]
    TooManyUrls { max_urls: usize },
    #[error("sitemap contains duplicate canonical URL {url}")]
    DuplicateUrl { url: CanonicalSiteUrl },
    #[error(
        "sitemap URL {url} contains {characters} characters; locations must contain fewer than {max_characters} characters"
    )]
    LocationTooLong {
        url: CanonicalSiteUrl,
        characters: usize,
        max_characters: usize,
    },
    #[error("sitemap URL {url} contains XML 1.0-illegal character U+{code_point:04X}")]
    IllegalXmlCharacter {
        url: CanonicalSiteUrl,
        code_point: u32,
    },
    #[error("sitemap exceeds the inclusive {max_bytes}-byte output limit")]
    OutputTooLarge { max_bytes: usize },
    #[error("sitemap XML serialization produced non-UTF-8 output")]
    InvalidUtf8(#[source] std::string::FromUtf8Error),
}

pub(crate) fn render_sitemap(
    urls: &[CanonicalSiteUrl],
) -> Result<RenderedSitemap, SitemapRenderError> {
    render_sitemap_with_limits(urls, MAX_SITEMAP_URLS, MAX_SITEMAP_BYTES)
}

fn render_sitemap_with_limits(
    input_urls: &[CanonicalSiteUrl],
    max_urls: usize,
    max_bytes: usize,
) -> Result<RenderedSitemap, SitemapRenderError> {
    if input_urls.len() > max_urls {
        return Err(SitemapRenderError::TooManyUrls { max_urls });
    }

    let mut urls = input_urls.iter().collect::<Vec<_>>();
    urls.sort_unstable();
    if let Some(duplicate) = urls.windows(2).find(|pair| pair[0] == pair[1]) {
        return Err(SitemapRenderError::DuplicateUrl {
            url: (*duplicate[0]).clone(),
        });
    }
    for url in &urls {
        validate_location(url.as_str(), url)?;
    }

    let mut writer = Writer::new_with_indent(BoundedOutput::new(max_bytes), b' ', 2);
    write_event(
        &mut writer,
        Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)),
        max_bytes,
    )?;

    let mut urlset = BytesStart::new("urlset");
    urlset.push_attribute(("xmlns", "http://www.sitemaps.org/schemas/sitemap/0.9"));
    write_event(&mut writer, Event::Start(urlset), max_bytes)?;
    for url in urls {
        write_url(&mut writer, url, max_bytes)?;
    }
    write_event(&mut writer, Event::End(BytesEnd::new("urlset")), max_bytes)?;

    let bytes = writer.into_inner().into_bytes();
    let digest = sitemap_digest(&bytes);
    let body = String::from_utf8(bytes)
        .map(Arc::<str>::from)
        .map_err(SitemapRenderError::InvalidUtf8)?;
    Ok(RenderedSitemap { body, digest })
}

fn validate_location(value: &str, url: &CanonicalSiteUrl) -> Result<(), SitemapRenderError> {
    if let Some(illegal) = value
        .chars()
        .find(|character| !is_xml_1_0_character(*character))
    {
        return Err(SitemapRenderError::IllegalXmlCharacter {
            url: url.clone(),
            code_point: illegal.into(),
        });
    }

    let characters = value.chars().count();
    if characters >= MAX_SITEMAP_LOCATION_CHARACTERS {
        return Err(SitemapRenderError::LocationTooLong {
            url: url.clone(),
            characters,
            max_characters: MAX_SITEMAP_LOCATION_CHARACTERS,
        });
    }
    Ok(())
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

fn write_url(
    writer: &mut Writer<BoundedOutput>,
    url: &CanonicalSiteUrl,
    max_bytes: usize,
) -> Result<(), SitemapRenderError> {
    write_event(writer, Event::Start(BytesStart::new("url")), max_bytes)?;
    writer
        .create_element("loc")
        .write_text_content(BytesText::new(url.as_str()))
        .map_err(|_| SitemapRenderError::OutputTooLarge { max_bytes })?;
    write_event(writer, Event::End(BytesEnd::new("url")), max_bytes)
}

fn write_event<'a>(
    writer: &mut Writer<BoundedOutput>,
    event: Event<'a>,
    max_bytes: usize,
) -> Result<(), SitemapRenderError> {
    writer
        .write_event(event)
        .map_err(|_| SitemapRenderError::OutputTooLarge { max_bytes })
}

fn sitemap_digest(bytes: &[u8]) -> SitemapDigest {
    let mut hasher = blake3::Hasher::new_derive_key(SITEMAP_DIGEST_CONTEXT);
    hasher.update(bytes);
    SitemapDigest(*hasher.finalize().as_bytes())
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
            return Err(io::Error::other("sitemap output limit reached"));
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

    use markdown_compiler::{PostSlug, PublicationBaseUrl};
    use quick_xml::{Reader, escape::unescape, events::Event};

    use super::*;
    use crate::domain::publication::PublicPagePath;

    fn canonical_url(path: &PublicPagePath) -> CanonicalSiteUrl {
        let base = PublicationBaseUrl::parse("https://example.com/").unwrap();
        CanonicalSiteUrl::for_path(&base, path)
    }

    fn post_url(slug: &str) -> CanonicalSiteUrl {
        canonical_url(&PublicPagePath::post(&PostSlug::parse(slug).unwrap()))
    }

    fn assert_well_formed(xml: &str) {
        let mut reader = Reader::from_str(xml);
        loop {
            match reader.read_event() {
                Ok(Event::Eof) => return,
                Ok(_) => {}
                Err(error) => panic!(
                    "sitemap must be well-formed XML at byte {}: {error}",
                    reader.error_position()
                ),
            }
        }
    }

    fn parsed_locations(xml: &str) -> Vec<String> {
        let mut reader = Reader::from_str(xml);
        let mut locations = Vec::new();
        loop {
            match reader.read_event().unwrap() {
                Event::Start(start) if start.name().as_ref() == "loc" => {
                    let raw = reader.read_text(start.name()).unwrap();
                    locations.push(unescape(raw.as_ref()).unwrap().into_owned());
                }
                Event::Eof => return locations,
                _ => {}
            }
        }
    }

    #[test]
    fn renders_only_sorted_canonical_locations_in_the_sitemap_namespace() {
        let root = canonical_url(&PublicPagePath::index());
        let alpha = post_url("alpha");
        let zulu = post_url("zulu");

        let rendered = render_sitemap(&[zulu, root, alpha]).unwrap();

        assert_well_formed(&rendered.body);
        assert_eq!(
            rendered.body.as_ref(),
            concat!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
                "<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
                "  <url>\n",
                "    <loc>https://example.com/</loc>\n",
                "  </url>\n",
                "  <url>\n",
                "    <loc>https://example.com/posts/alpha</loc>\n",
                "  </url>\n",
                "  <url>\n",
                "    <loc>https://example.com/posts/zulu</loc>\n",
                "  </url>\n",
                "</urlset>"
            )
        );
        assert_eq!(
            parsed_locations(&rendered.body),
            [
                "https://example.com/",
                "https://example.com/posts/alpha",
                "https://example.com/posts/zulu",
            ]
        );
        assert_eq!(rendered.body.matches("<url>").count(), 3);
        for optional_tag in ["lastmod", "changefreq", "priority"] {
            assert!(!rendered.body.contains(optional_tag));
        }
    }

    #[test]
    fn empty_input_serializes_without_url_or_wall_clock_metadata() {
        let rendered = render_sitemap(&[]).unwrap();

        assert_well_formed(&rendered.body);
        assert!(parsed_locations(&rendered.body).is_empty());
        assert!(!rendered.body.contains("<url>"));
        assert!(!rendered.body.contains("lastmod"));
    }

    #[test]
    fn rejects_duplicates_after_deterministic_sorting_with_url_context() {
        let alpha = post_url("alpha");
        let zulu = post_url("zulu");

        let error = render_sitemap(&[zulu.clone(), alpha, zulu.clone()]).unwrap_err();

        assert!(matches!(
            error,
            SitemapRenderError::DuplicateUrl { url } if url == zulu
        ));
    }

    #[test]
    fn location_limit_counts_characters_and_is_exclusive_at_2048() {
        let context = post_url("length-context");

        assert!(validate_location(&"é".repeat(2_047), &context).is_ok());
        let error = validate_location(&"é".repeat(2_048), &context).unwrap_err();

        assert!(matches!(
            error,
            SitemapRenderError::LocationTooLong {
                url,
                characters: 2_048,
                max_characters: 2_048,
            } if url == context
        ));
    }

    #[test]
    fn rejects_xml_1_0_illegal_characters_with_url_context() {
        let context = post_url("xml-context");
        let value = format!("https://example.com/posts/illegal{}", '\u{FFFE}');

        let error = validate_location(&value, &context).unwrap_err();

        assert!(matches!(
            error,
            SitemapRenderError::IllegalXmlCharacter {
                url,
                code_point: 0xFFFE,
            } if url == context
        ));
    }

    #[test]
    fn configured_url_count_limit_is_inclusive_and_rejects_the_next_url() {
        assert_eq!(MAX_SITEMAP_URLS, 50_000);
        let alpha = post_url("alpha");
        let beta = post_url("beta");
        let gamma = post_url("gamma");

        let rendered =
            render_sitemap_with_limits(&[beta.clone(), alpha.clone()], 2, MAX_SITEMAP_BYTES)
                .unwrap();
        assert_eq!(parsed_locations(&rendered.body).len(), 2);

        assert!(matches!(
            render_sitemap_with_limits(&[gamma, beta, alpha], 2, MAX_SITEMAP_BYTES),
            Err(SitemapRenderError::TooManyUrls { max_urls: 2 })
        ));
    }

    #[test]
    fn public_url_count_limit_accepts_50000_and_rejects_50001() {
        let mut urls: Vec<_> = (0..MAX_SITEMAP_URLS)
            .map(|index| post_url(&format!("post-{index}")))
            .collect();

        let rendered = render_sitemap(&urls).unwrap();
        assert_eq!(parsed_locations(&rendered.body).len(), MAX_SITEMAP_URLS);

        urls.push(post_url("post-overflow"));
        assert!(matches!(
            render_sitemap(&urls),
            Err(SitemapRenderError::TooManyUrls {
                max_urls: MAX_SITEMAP_URLS
            })
        ));
    }

    #[test]
    fn sitemap_digest_is_deterministic_domain_separated_and_binds_exact_bytes() {
        let url = post_url("digest");
        let first = render_sitemap(std::slice::from_ref(&url)).unwrap();
        let second = render_sitemap(std::slice::from_ref(&url)).unwrap();

        assert_eq!(first, second);
        let encoded = first.digest.to_string();
        assert!(encoded.starts_with(SITEMAP_DIGEST_PREFIX));
        assert_eq!(encoded.len(), SITEMAP_DIGEST_PREFIX.len() + 64);

        let mut expected = blake3::Hasher::new_derive_key(SITEMAP_DIGEST_CONTEXT);
        expected.update(first.body.as_bytes());
        assert_eq!(
            encoded,
            format!("{SITEMAP_DIGEST_PREFIX}{}", expected.finalize().to_hex())
        );

        let changed = sitemap_digest(format!("{}\n", first.body).as_bytes());
        assert_ne!(changed, first.digest);
    }

    #[test]
    fn bounded_output_accepts_its_inclusive_limit_and_rejects_the_next_byte() {
        assert_eq!(MAX_SITEMAP_BYTES, 40 * 1024 * 1024);
        let mut output = BoundedOutput::new(4);

        output.write_all(b"1234").unwrap();
        assert_eq!(output.bytes, b"1234");
        assert!(output.write_all(b"5").is_err());
        assert_eq!(output.bytes, b"1234");
    }

    #[test]
    fn renderer_maps_the_exact_output_boundary_to_a_typed_error() {
        let url = post_url("output-boundary");
        let baseline = render_sitemap(std::slice::from_ref(&url)).unwrap();
        let exact =
            render_sitemap_with_limits(std::slice::from_ref(&url), 1, baseline.body.len()).unwrap();
        assert_eq!(exact, baseline);

        let max_bytes = baseline.body.len() - 1;
        assert!(matches!(
            render_sitemap_with_limits(std::slice::from_ref(&url), 1, max_bytes),
            Err(SitemapRenderError::OutputTooLarge { max_bytes: found })
                if found == max_bytes
        ));
    }
}
