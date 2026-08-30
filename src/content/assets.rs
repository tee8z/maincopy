use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use url::Url;

use super::identity::{
    PostAssetSourceBinding, PublicationAssetSourceBinding, bind_post_asset_source,
    bind_publication_asset_source,
};
use super::{AssetDigest, LogicalAssetPath, PostDocument, PublicationSettings, SiteSnapshotDigest};

/// A normalized external asset URL that is safe to include in revision input.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExternalAssetUrl {
    url: Url,
    canonical: String,
}

impl ExternalAssetUrl {
    pub fn parse(value: &str) -> Result<Self, ExternalAssetUrlError> {
        let value = value.trim();
        let has_valid_raw_authority = value.split_once("://").is_some_and(|(_, remainder)| {
            let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
            !remainder[..authority_end].contains('@')
        });
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

impl fmt::Display for ExternalAssetUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for ExternalAssetUrl {
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

impl<'de> Deserialize<'de> for ExternalAssetUrl {
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

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("external asset URL must be an absolute HTTPS URL without credentials or a fragment")]
pub struct ExternalAssetUrlError;

/// A normalized HTTPS origin used by the effective asset allowlist.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExternalAssetOrigin {
    url: Url,
    canonical: String,
}

impl ExternalAssetOrigin {
    pub fn parse(value: &str) -> Result<Self, ExternalAssetOriginError> {
        let value = value.trim();
        let has_valid_raw_authority = value.split_once("://").is_some_and(|(_, remainder)| {
            let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
            !remainder[..authority_end].contains('@')
        });
        let mut url = Url::parse(value).map_err(|_| ExternalAssetOriginError)?;
        if url.scheme() != "https"
            || url.host().is_none()
            || !has_valid_raw_authority
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

impl fmt::Display for ExternalAssetOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for ExternalAssetOrigin {
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

impl<'de> Deserialize<'de> for ExternalAssetOrigin {
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

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error(
    "asset origin must be an absolute HTTPS origin without credentials, path, query, or fragment"
)]
pub struct ExternalAssetOriginError;

/// A local asset path paired with the digest of its exact bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DigestedAsset {
    path: LogicalAssetPath,
    digest: AssetDigest,
}

impl DigestedAsset {
    pub const fn new(path: LogicalAssetPath, digest: AssetDigest) -> Self {
        Self { path, digest }
    }

    pub const fn path(&self) -> &LogicalAssetPath {
        &self.path
    }

    pub const fn digest(&self) -> &AssetDigest {
        &self.digest
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
            Self::Local(asset) => (0, asset.path().as_str()),
            Self::External(url) => (1, url.as_str()),
        }
    }
}

/// Resolver-owned, complete asset inputs for one post revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPostAssets {
    source_binding: PostAssetSourceBinding,
    image: Option<AssetRevisionReference>,
    references: Vec<AssetRevisionReference>,
}

impl ResolvedPostAssets {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the asset resolver becomes the sole constructor in WP 1.6"
        )
    )]
    pub(super) fn new(
        document: &PostDocument,
        image: Option<AssetRevisionReference>,
        references: Vec<AssetRevisionReference>,
    ) -> Self {
        Self {
            source_binding: bind_post_asset_source(document),
            image,
            references,
        }
    }

    pub(super) const fn source_binding(&self) -> &PostAssetSourceBinding {
        &self.source_binding
    }

    pub const fn image(&self) -> Option<&AssetRevisionReference> {
        self.image.as_ref()
    }

    pub fn references(&self) -> &[AssetRevisionReference] {
        &self.references
    }
}

/// Resolver-owned, complete asset inputs for one site snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSiteAssets {
    source_binding: PublicationAssetSourceBinding,
    favicon: Option<AssetRevisionReference>,
    allowed_origins: Vec<ExternalAssetOrigin>,
    references: Vec<AssetRevisionReference>,
}

impl ResolvedSiteAssets {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the asset resolver becomes the sole constructor in WP 1.6"
        )
    )]
    pub(super) fn new(
        publication: &PublicationSettings,
        favicon: Option<AssetRevisionReference>,
        allowed_origins: Vec<ExternalAssetOrigin>,
        references: Vec<AssetRevisionReference>,
    ) -> Self {
        Self {
            source_binding: bind_publication_asset_source(publication),
            favicon,
            allowed_origins,
            references,
        }
    }

    pub(super) const fn source_binding(&self) -> &PublicationAssetSourceBinding {
        &self.source_binding
    }

    pub const fn favicon(&self) -> Option<&AssetRevisionReference> {
        self.favicon.as_ref()
    }

    pub fn allowed_origins(&self) -> &[ExternalAssetOrigin] {
        &self.allowed_origins
    }

    pub fn references(&self) -> &[AssetRevisionReference] {
        &self.references
    }
}

/// The public, immutable path of a content asset in one site snapshot.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SnapshotAssetPath {
    public: String,
    storage_relative: String,
}

impl SnapshotAssetPath {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the public snapshot manifest becomes the sole caller in WP 1.4"
        )
    )]
    pub(super) fn new(
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
            "https://example.com/image.png#fragment",
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
            "https://example.com/path",
            "https://example.com/?query=1",
            "https://example.com/#fragment",
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
