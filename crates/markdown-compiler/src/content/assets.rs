use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use url::Url;

use super::{AssetDigest, LogicalAssetPath};

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
}
