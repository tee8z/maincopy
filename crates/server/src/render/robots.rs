use std::{fmt, sync::Arc};

use thiserror::Error;

use crate::domain::publication::CanonicalSiteUrl;

const MAX_ROBOTS_SITEMAP_URL_CHARACTERS: usize = 2_048;
const ROBOTS_DIGEST_CONTEXT: &str = "maincopy robots bytes v1";
const ROBOTS_DIGEST_PREFIX: &str = "robots-b3-v1-";

/// Exact immutable robots policy bytes and their strong presentation identity.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct RenderedRobots {
    pub(crate) body: Arc<str>,
    pub(crate) digest: RobotsDigest,
}

impl fmt::Debug for RenderedRobots {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RenderedRobots")
            .field("digest", &self.digest)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// Domain-separated identity of the exact serialized robots policy bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RobotsDigest([u8; 32]);

impl fmt::Display for RobotsDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{ROBOTS_DIGEST_PREFIX}{}",
            blake3::Hash::from_bytes(self.0).to_hex()
        )
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum RobotsRenderError {
    #[error(
        "robots sitemap URL {url} contains {characters} characters; sitemap URLs must contain fewer than {max_characters} characters"
    )]
    SitemapUrlTooLong {
        url: CanonicalSiteUrl,
        characters: usize,
        max_characters: usize,
    },
}

pub(crate) fn render_robots(
    sitemap_url: &CanonicalSiteUrl,
) -> Result<RenderedRobots, RobotsRenderError> {
    let characters = sitemap_url.as_str().chars().count();
    if characters >= MAX_ROBOTS_SITEMAP_URL_CHARACTERS {
        return Err(RobotsRenderError::SitemapUrlTooLong {
            url: sitemap_url.clone(),
            characters,
            max_characters: MAX_ROBOTS_SITEMAP_URL_CHARACTERS,
        });
    }

    let body: Arc<str> = format!(
        "User-agent: *\nAllow: /\n\nSitemap: {}\n",
        sitemap_url.as_str()
    )
    .into();
    let mut hasher = blake3::Hasher::new_derive_key(ROBOTS_DIGEST_CONTEXT);
    hasher.update(body.as_bytes());
    let digest = RobotsDigest(*hasher.finalize().as_bytes());

    Ok(RenderedRobots { body, digest })
}

#[cfg(test)]
mod tests {
    use markdown_compiler::PublicationBaseUrl;

    use super::*;
    use crate::domain::publication::PublicPagePath;

    fn sitemap_url(origin: &str) -> CanonicalSiteUrl {
        let base = PublicationBaseUrl::parse(origin).unwrap();
        CanonicalSiteUrl::for_path(&base, &PublicPagePath::sitemap())
    }

    #[test]
    fn renders_exact_allow_all_policy_with_absolute_sitemap_url() {
        let rendered = render_robots(&sitemap_url("https://example.com/")).unwrap();

        assert_eq!(
            rendered.body.as_ref(),
            concat!(
                "User-agent: *\n",
                "Allow: /\n",
                "\n",
                "Sitemap: https://example.com/sitemap.xml\n",
            )
        );
        assert!(!rendered.body.contains("Disallow"));
        assert!(rendered.body.ends_with('\n'));
    }

    #[test]
    fn digest_is_deterministic_domain_separated_and_binds_exact_bytes() {
        let example = sitemap_url("https://example.com/");
        let first = render_robots(&example).unwrap();
        let second = render_robots(&example).unwrap();

        assert_eq!(first, second);
        let encoded = first.digest.to_string();
        assert!(encoded.starts_with(ROBOTS_DIGEST_PREFIX));
        assert_eq!(encoded.len(), ROBOTS_DIGEST_PREFIX.len() + 64);

        let mut expected = blake3::Hasher::new_derive_key(ROBOTS_DIGEST_CONTEXT);
        expected.update(first.body.as_bytes());
        assert_eq!(
            encoded,
            format!("{ROBOTS_DIGEST_PREFIX}{}", expected.finalize().to_hex())
        );

        let changed = render_robots(&sitemap_url("https://other.example/")).unwrap();
        assert_ne!(changed.body, first.body);
        assert_ne!(changed.digest, first.digest);
    }

    #[test]
    fn sitemap_url_limit_accepts_2047_characters_and_rejects_2048() {
        let accepted_origin = format!("https://{}example.com/", "a.".repeat(1_008));
        let accepted = sitemap_url(&accepted_origin);
        assert_eq!(accepted.as_str().chars().count(), 2_047);
        render_robots(&accepted).unwrap();

        let rejected_origin = format!("https://{}bexample.com/", "a.".repeat(1_008));
        let rejected = sitemap_url(&rejected_origin);
        assert_eq!(rejected.as_str().chars().count(), 2_048);
        let error = render_robots(&rejected).unwrap_err();

        assert!(matches!(
            error,
            RobotsRenderError::SitemapUrlTooLong {
                url,
                characters: 2_048,
                max_characters: 2_048,
            } if url == rejected
        ));
    }
}
