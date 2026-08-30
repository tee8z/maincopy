use std::{fmt, num::NonZeroUsize, ops::Range};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use url::Url;

use super::identity::{
    AssetResolutionPolicyBinding, PostAssetSourceBinding, PublicationAssetSourceBinding,
    bind_asset_resolution_policy, bind_post_asset_source, bind_publication_asset_source,
};
use super::{AssetDigest, LogicalAssetPath, PostDocument, PublicationSettings, SiteSnapshotDigest};

macro_rules! canonical_url_wire {
    ($name:ident) => {
        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<SerializerType>(
                &self,
                serializer: SerializerType,
            ) -> Result<SerializerType::Ok, SerializerType::Error>
            where
                SerializerType: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<DeserializerType>(
                deserializer: DeserializerType,
            ) -> Result<Self, DeserializerType::Error>
            where
                DeserializerType: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(&value).map_err(de::Error::custom)
            }
        }
    };
}

/// A normalized external asset URL that is safe to include in revision input.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExternalAssetUrl {
    url: Url,
    canonical: String,
}

impl ExternalAssetUrl {
    pub fn parse(value: &str) -> Result<Self, ExternalAssetUrlError> {
        if has_forbidden_raw_url_input(value) {
            return Err(ExternalAssetUrlError);
        }
        let value = value.trim();
        let has_valid_raw_authority = has_valid_raw_authority(value);
        let mut url = Url::parse(value).map_err(|_| ExternalAssetUrlError)?;
        if url.scheme() != "https"
            || url.host().is_none()
            || !has_valid_raw_authority
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(ExternalAssetUrlError);
        }
        url.set_fragment(None);
        let canonical = url.as_str().to_owned();
        Ok(Self { url, canonical })
    }

    pub fn as_url(&self) -> &Url {
        &self.url
    }

    pub fn as_str(&self) -> &str {
        &self.canonical
    }
}

canonical_url_wire!(ExternalAssetUrl);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error(
    "external asset URL must be an absolute HTTPS URL without credentials, a fragment, controls, or backslashes"
)]
pub struct ExternalAssetUrlError;

/// A normalized HTTPS origin used by the effective asset allowlist.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExternalAssetOrigin {
    url: Url,
    canonical: String,
}

impl ExternalAssetOrigin {
    pub fn parse(value: &str) -> Result<Self, ExternalAssetOriginError> {
        if has_forbidden_raw_url_input(value) {
            return Err(ExternalAssetOriginError);
        }
        let value = value.trim();
        let has_valid_raw_authority = has_valid_raw_authority(value);
        let has_valid_raw_suffix = has_root_only_raw_suffix(value);
        let mut url = Url::parse(value).map_err(|_| ExternalAssetOriginError)?;
        if url.scheme() != "https"
            || url.host().is_none()
            || !has_valid_raw_authority
            || !has_valid_raw_suffix
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return Err(ExternalAssetOriginError);
        }
        url.set_path("/");
        let canonical = url.as_str().to_owned();
        Ok(Self { url, canonical })
    }

    pub fn as_url(&self) -> &Url {
        &self.url
    }

    pub fn as_str(&self) -> &str {
        &self.canonical
    }
}

canonical_url_wire!(ExternalAssetOrigin);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error(
    "asset origin must be an absolute HTTPS origin without credentials, path, query, fragment, controls, or backslashes"
)]
pub struct ExternalAssetOriginError;

/// A local asset path paired with the digest of its exact bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DigestedAsset {
    pub path: LogicalAssetPath,
    pub digest: AssetDigest,
}

impl DigestedAsset {
    pub const fn new(path: LogicalAssetPath, digest: AssetDigest) -> Self {
        Self { path, digest }
    }
}

/// A normalized local or external asset reference used by digest transcripts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetRevisionReference {
    Local(DigestedAsset),
    External(ExternalAssetUrl),
}

impl AssetRevisionReference {
    pub const fn local(asset: DigestedAsset) -> Self {
        Self::Local(asset)
    }

    pub const fn external(url: ExternalAssetUrl) -> Self {
        Self::External(url)
    }

    pub(crate) fn sort_key(&self) -> (u8, &str) {
        match self {
            Self::Local(asset) => (0, asset.path.as_str()),
            Self::External(url) => (1, url.as_str()),
        }
    }
}

/// The one-based position of a link or image destination in a Markdown event stream.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct MarkdownDestinationOrdinal(NonZeroUsize);

impl MarkdownDestinationOrdinal {
    pub const fn get(self) -> usize {
        self.0.get()
    }

    pub(crate) const fn new(value: NonZeroUsize) -> Self {
        Self(value)
    }
}

/// A half-open byte range in the exact Markdown source bound to a resolution.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct MarkdownSourceRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl MarkdownSourceRange {
    pub fn as_range(self) -> Range<usize> {
        self.start..self.end
    }

    pub(super) const fn new(range: Range<usize>) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkdownDestinationKind {
    Image,
    Download,
}

/// The destination value produced by the CommonMark parser for one occurrence.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AuthoredMarkdownDestination(Box<str>);

impl AuthoredMarkdownDestination {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn new(value: &str) -> Self {
        Self(value.into())
    }
}

/// One resolver-approved Markdown asset destination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedMarkdownDestination {
    pub(crate) ordinal: MarkdownDestinationOrdinal,
    pub(crate) source_range: MarkdownSourceRange,
    pub(crate) kind: MarkdownDestinationKind,
    pub(crate) authored: AuthoredMarkdownDestination,
    pub(crate) target: AssetRevisionReference,
}

impl ResolvedMarkdownDestination {
    pub(super) fn new(
        ordinal: MarkdownDestinationOrdinal,
        source_range: MarkdownSourceRange,
        kind: MarkdownDestinationKind,
        authored: AuthoredMarkdownDestination,
        target: AssetRevisionReference,
    ) -> Self {
        Self {
            ordinal,
            source_range,
            kind,
            authored,
            target,
        }
    }
}

/// Resolver-owned, complete asset inputs for one post revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPostAssets {
    pub(super) source_binding: PostAssetSourceBinding,
    pub(super) policy_binding: AssetResolutionPolicyBinding,
    pub(crate) image: Option<AssetRevisionReference>,
    pub(crate) references: Vec<AssetRevisionReference>,
    pub(crate) markdown_destinations: Vec<ResolvedMarkdownDestination>,
}

impl ResolvedPostAssets {
    #[cfg(test)]
    pub(crate) fn new(
        document: &PostDocument,
        image: Option<AssetRevisionReference>,
        references: Vec<AssetRevisionReference>,
    ) -> Self {
        Self::from_resolution(document, &[], image, references, Vec::new())
    }

    pub(super) fn from_resolution(
        document: &PostDocument,
        allowed_origins: &[ExternalAssetOrigin],
        image: Option<AssetRevisionReference>,
        references: Vec<AssetRevisionReference>,
        markdown_destinations: Vec<ResolvedMarkdownDestination>,
    ) -> Self {
        Self {
            source_binding: bind_post_asset_source(document),
            policy_binding: bind_asset_resolution_policy(allowed_origins),
            image,
            references,
            markdown_destinations,
        }
    }

    pub(crate) fn shares_policy_with(&self, site_assets: &ResolvedSiteAssets) -> bool {
        self.policy_binding == site_assets.policy_binding
    }
}

/// Resolver-owned, complete asset inputs for one site snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSiteAssets {
    pub(super) source_binding: PublicationAssetSourceBinding,
    pub(super) policy_binding: AssetResolutionPolicyBinding,
    pub(crate) favicon: Option<AssetRevisionReference>,
    pub(crate) allowed_origins: Vec<ExternalAssetOrigin>,
    pub(crate) references: Vec<AssetRevisionReference>,
}

impl ResolvedSiteAssets {
    pub(crate) fn new(
        publication: &PublicationSettings,
        favicon: Option<AssetRevisionReference>,
        allowed_origins: Vec<ExternalAssetOrigin>,
        references: Vec<AssetRevisionReference>,
    ) -> Self {
        Self {
            source_binding: bind_publication_asset_source(publication),
            policy_binding: bind_asset_resolution_policy(&allowed_origins),
            favicon,
            allowed_origins,
            references,
        }
    }
}

/// The public, immutable path of a content asset in one site snapshot.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SnapshotAssetPath {
    public: String,
    storage_relative: String,
}

impl SnapshotAssetPath {
    pub(crate) fn new(
        snapshot: &SiteSnapshotDigest,
        asset: &LogicalAssetPath,
    ) -> Result<Self, SnapshotAssetPathError> {
        let Some(relative) = asset.as_str().strip_prefix("assets/") else {
            return Err(SnapshotAssetPathError);
        };
        if relative.is_empty() {
            return Err(SnapshotAssetPathError);
        }
        let storage_relative = format!("{}/{relative}", snapshot.as_str());
        Ok(Self {
            public: format!("/assets/{storage_relative}"),
            storage_relative,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.public
    }

    pub fn storage_relative(&self) -> &str {
        &self.storage_relative
    }
}

impl fmt::Display for SnapshotAssetPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for SnapshotAssetPath {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("logical asset path is outside the assets namespace")]
pub struct SnapshotAssetPathError;

fn has_valid_raw_authority(value: &str) -> bool {
    let Some((_, remainder)) = value.split_once("://") else {
        return false;
    };
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    !authority.is_empty() && !authority.contains('@') && !authority.ends_with(':')
}

fn has_forbidden_raw_url_input(value: &str) -> bool {
    value.contains('\\') || value.chars().any(char::is_control)
}

fn has_root_only_raw_suffix(value: &str) -> bool {
    value.split_once("://").is_some_and(|(_, remainder)| {
        let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
        matches!(&remainder[authority_end..], "" | "/")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_asset_urls_are_normalized_and_bounded() {
        let url = ExternalAssetUrl::parse("https://EXAMPLE.com:443/image.png?version=1").unwrap();
        assert_eq!(url.as_str(), "https://example.com/image.png?version=1");

        for invalid in [
            "http://example.com/image.png",
            "https://user@example.com/image.png",
            "https://@example.com/image.png",
            "HTTPS://@example.com/image.png",
            "https://example.com:/image.png",
            "https://example.com:invalid/image.png",
            "https://example.com:65536/image.png",
            "https://example.com/image.png#fragment",
            "https://example.com\\evil.png",
            "https://exa\nmple.com/image.png",
            "https://exa\tmple.com/image.png",
            "/image.png",
        ] {
            assert_eq!(ExternalAssetUrl::parse(invalid), Err(ExternalAssetUrlError));
            assert!(
                serde_json::from_value::<ExternalAssetUrl>(serde_json::json!(invalid)).is_err()
            );
        }
    }

    #[test]
    fn external_asset_origins_have_an_exact_normalized_boundary() {
        let origin = ExternalAssetOrigin::parse("HTTPS://EXAMPLE.com:443").unwrap();
        assert_eq!(origin.as_str(), "https://example.com/");

        for invalid in [
            "http://example.com",
            "https://@example.com",
            "https://example.com:",
            "https://example.com:invalid",
            "https://example.com:65536",
            "https://example.com/path",
            "https://example.com/foo/..",
            "https://example.com/%2e",
            "https://example.com/?query=1",
            "https://example.com/#fragment",
            "https://example.com\\path",
            "https://exa\nmple.com",
            "https://exa\tmple.com",
        ] {
            assert_eq!(
                ExternalAssetOrigin::parse(invalid),
                Err(ExternalAssetOriginError)
            );
        }
    }

    #[test]
    fn immutable_asset_paths_are_snapshot_scoped() {
        let snapshot = SiteSnapshotDigest::parse(&format!("site-b3-v1-{}", "11".repeat(32)))
            .expect("fixture digest must parse");
        let asset =
            LogicalAssetPath::parse("assets/images/cover.webp").expect("fixture path must parse");

        assert_eq!(
            SnapshotAssetPath::new(&snapshot, &asset).unwrap().as_str(),
            format!("/assets/{snapshot}/images/cover.webp")
        );
        assert_eq!(
            SnapshotAssetPath::new(&snapshot, &asset)
                .unwrap()
                .storage_relative(),
            format!("{snapshot}/images/cover.webp")
        );
    }
}
