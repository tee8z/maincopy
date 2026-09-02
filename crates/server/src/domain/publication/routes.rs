use std::fmt;

use markdown_compiler::{PostSlug, PostTag, PublicationBaseUrl};
use serde::Serialize;

pub(crate) const RSS_FEED_PATH: &str = "/feed.xml";

/// A canonical absolute URL derived only from validated publication settings.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CanonicalSiteUrl(Box<str>);

impl CanonicalSiteUrl {
    pub(crate) fn for_path(base: &PublicationBaseUrl, path: &PublicPagePath) -> Self {
        let mut url = base.as_url().clone();
        url.set_path(path.as_str());
        url.set_query(None);
        url.set_fragment(None);
        Self(url.as_str().into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CanonicalSiteUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The closed path and identity vocabulary for public site output.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PublicPagePath(Box<str>);

impl PublicPagePath {
    pub(crate) fn index() -> Self {
        Self("/".into())
    }

    pub(crate) fn archive() -> Self {
        Self("/archive".into())
    }

    pub(crate) fn feed() -> Self {
        Self(RSS_FEED_PATH.into())
    }

    pub(crate) fn post(slug: &PostSlug) -> Self {
        Self(format!("/posts/{}", slug.as_str()).into_boxed_str())
    }

    pub(crate) fn tag(tag: &PostTag) -> Self {
        Self(format!("/tags/{}", tag.as_str()).into_boxed_str())
    }

    pub(crate) fn error_identity_marker(name: &'static str) -> Self {
        Self(format!("<{name}>").into_boxed_str())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}
