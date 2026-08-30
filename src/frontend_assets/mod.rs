//! Typed, content-addressed application assets embedded at build time.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

use crate::frontend_digest_contract::{
    FRONTEND_ASSET_PREFIX, FRONTEND_BUNDLE_PREFIX, FrontendDigestInput, frontend_asset_digest,
    frontend_bundle_digest,
};
pub use crate::frontend_digest_contract::{
    FrontendAssetKind, FrontendAssetName, FrontendAssetNameParseError,
};

const DIGEST_HEX_LENGTH: usize = 64;

impl Serialize for FrontendAssetKind {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        serializer.serialize_str(match self {
            Self::Css => "css",
            Self::JavaScript => "java_script",
        })
    }
}

impl<'de> Deserialize<'de> for FrontendAssetKind {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "css" => Ok(Self::Css),
            "java_script" => Ok(Self::JavaScript),
            _ => Err(de::Error::custom("frontend asset kind is not recognized")),
        }
    }
}

impl Serialize for FrontendAssetName {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        serializer.serialize_str(match self {
            Self::Stylesheet => "stylesheet",
            Self::JavaScript => "java_script",
        })
    }
}

impl<'de> Deserialize<'de> for FrontendAssetName {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "stylesheet" => Ok(Self::Stylesheet),
            "java_script" => Ok(Self::JavaScript),
            _ => Err(de::Error::custom("frontend asset name is not recognized")),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendDigestKind {
    Bundle,
    Asset,
}

impl FrontendDigestKind {
    const fn prefix(self) -> &'static str {
        match self {
            Self::Bundle => FRONTEND_BUNDLE_PREFIX,
            Self::Asset => FRONTEND_ASSET_PREFIX,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum FrontendDigestParseError {
    #[error("{kind:?} frontend digest must start with {expected}")]
    InvalidPrefix {
        kind: FrontendDigestKind,
        expected: &'static str,
    },
    #[error("{kind:?} frontend digest must contain exactly 32 encoded bytes")]
    InvalidLength { kind: FrontendDigestKind },
    #[error("{kind:?} frontend digest must use lowercase hexadecimal")]
    InvalidEncoding { kind: FrontendDigestKind },
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FrontendBundleDigest([u8; 32]);

impl FrontendBundleDigest {
    const fn from_generated(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn parse(value: &str) -> Result<Self, FrontendDigestParseError> {
        parse_digest(value, FrontendDigestKind::Bundle).map(Self)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for FrontendBundleDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        format_digest(FRONTEND_BUNDLE_PREFIX, &self.0, formatter)
    }
}

impl FromStr for FrontendBundleDigest {
    type Err = FrontendDigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for FrontendBundleDigest {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for FrontendBundleDigest {
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

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FrontendAssetDigest([u8; 32]);

impl FrontendAssetDigest {
    const fn from_generated(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn parse(value: &str) -> Result<Self, FrontendDigestParseError> {
        parse_digest(value, FrontendDigestKind::Asset).map(Self)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for FrontendAssetDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        format_digest(FRONTEND_ASSET_PREFIX, &self.0, formatter)
    }
}

impl FromStr for FrontendAssetDigest {
    type Err = FrontendDigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for FrontendAssetDigest {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for FrontendAssetDigest {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrontendAssetPath(&'static str);

impl FrontendAssetPath {
    const fn from_generated(value: &'static str) -> Self {
        Self(value)
    }

    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for FrontendAssetPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Serialize for FrontendAssetPath {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        serializer.serialize_str(self.0)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendAssetMime {
    Css,
    JavaScript,
}

impl FrontendAssetMime {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Css => "text/css; charset=utf-8",
            Self::JavaScript => "text/javascript; charset=utf-8",
        }
    }
}

impl fmt::Display for FrontendAssetMime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendCachePolicy {
    Immutable,
}

impl FrontendCachePolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Immutable => "public, max-age=31536000, immutable",
        }
    }
}

impl fmt::Display for FrontendCachePolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrontendAssetEtag(FrontendAssetDigest);

impl fmt::Display for FrontendAssetEtag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "\"{}\"", self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CssAsset {
    digest: FrontendAssetDigest,
    public_path: FrontendAssetPath,
    bytes: &'static [u8],
}

impl CssAsset {
    const fn from_generated(
        digest: FrontendAssetDigest,
        public_path: FrontendAssetPath,
        bytes: &'static [u8],
    ) -> Self {
        Self {
            digest,
            public_path,
            bytes,
        }
    }

    pub const fn name(&self) -> FrontendAssetName {
        FrontendAssetName::Stylesheet
    }

    pub const fn kind(&self) -> FrontendAssetKind {
        FrontendAssetKind::Css
    }

    pub const fn digest(&self) -> &FrontendAssetDigest {
        &self.digest
    }

    pub const fn public_path(&self) -> &FrontendAssetPath {
        &self.public_path
    }

    pub const fn mime(&self) -> FrontendAssetMime {
        FrontendAssetMime::Css
    }

    pub const fn cache_policy(&self) -> FrontendCachePolicy {
        FrontendCachePolicy::Immutable
    }

    pub const fn etag(&self) -> FrontendAssetEtag {
        FrontendAssetEtag(self.digest)
    }

    pub const fn bytes(&self) -> &'static [u8] {
        self.bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JavaScriptAsset {
    digest: FrontendAssetDigest,
    public_path: FrontendAssetPath,
    bytes: &'static [u8],
}

impl JavaScriptAsset {
    pub const fn name(&self) -> FrontendAssetName {
        FrontendAssetName::JavaScript
    }

    pub const fn kind(&self) -> FrontendAssetKind {
        FrontendAssetKind::JavaScript
    }

    pub const fn digest(&self) -> &FrontendAssetDigest {
        &self.digest
    }

    pub const fn public_path(&self) -> &FrontendAssetPath {
        &self.public_path
    }

    pub const fn mime(&self) -> FrontendAssetMime {
        FrontendAssetMime::JavaScript
    }

    pub const fn cache_policy(&self) -> FrontendCachePolicy {
        FrontendCachePolicy::Immutable
    }

    pub const fn etag(&self) -> FrontendAssetEtag {
        FrontendAssetEtag(self.digest)
    }

    pub const fn bytes(&self) -> &'static [u8] {
        self.bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrontendAsset<'asset> {
    Css(&'asset CssAsset),
    JavaScript(&'asset JavaScriptAsset),
}

impl<'asset> FrontendAsset<'asset> {
    pub const fn name(self) -> FrontendAssetName {
        match self {
            Self::Css(asset) => asset.name(),
            Self::JavaScript(asset) => asset.name(),
        }
    }

    pub const fn kind(self) -> FrontendAssetKind {
        match self {
            Self::Css(asset) => asset.kind(),
            Self::JavaScript(asset) => asset.kind(),
        }
    }

    pub const fn digest(self) -> &'asset FrontendAssetDigest {
        match self {
            Self::Css(asset) => asset.digest(),
            Self::JavaScript(asset) => asset.digest(),
        }
    }

    pub const fn public_path(self) -> &'asset FrontendAssetPath {
        match self {
            Self::Css(asset) => asset.public_path(),
            Self::JavaScript(asset) => asset.public_path(),
        }
    }

    pub const fn mime(self) -> FrontendAssetMime {
        match self {
            Self::Css(asset) => asset.mime(),
            Self::JavaScript(asset) => asset.mime(),
        }
    }

    pub const fn cache_policy(self) -> FrontendCachePolicy {
        match self {
            Self::Css(asset) => asset.cache_policy(),
            Self::JavaScript(asset) => asset.cache_policy(),
        }
    }

    pub const fn etag(self) -> FrontendAssetEtag {
        match self {
            Self::Css(asset) => asset.etag(),
            Self::JavaScript(asset) => asset.etag(),
        }
    }

    pub const fn bytes(self) -> &'asset [u8] {
        match self {
            Self::Css(asset) => asset.bytes(),
            Self::JavaScript(asset) => asset.bytes(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrontendAssetManifest {
    bundle_digest: FrontendBundleDigest,
    css: CssAsset,
    javascript: Option<JavaScriptAsset>,
}

impl FrontendAssetManifest {
    const fn from_generated(
        bundle_digest: FrontendBundleDigest,
        css: CssAsset,
        javascript: Option<JavaScriptAsset>,
    ) -> Self {
        Self {
            bundle_digest,
            css,
            javascript,
        }
    }

    pub const fn bundle_digest(&self) -> &FrontendBundleDigest {
        &self.bundle_digest
    }

    pub const fn css(&self) -> &CssAsset {
        &self.css
    }

    pub const fn javascript(&self) -> Option<&JavaScriptAsset> {
        self.javascript.as_ref()
    }

    pub fn lookup(
        &self,
        bundle: &FrontendBundleDigest,
        name: FrontendAssetName,
    ) -> Option<FrontendAsset<'_>> {
        if bundle != &self.bundle_digest {
            return None;
        }
        match name {
            FrontendAssetName::Stylesheet => Some(FrontendAsset::Css(&self.css)),
            FrontendAssetName::JavaScript => {
                self.javascript.as_ref().map(FrontendAsset::JavaScript)
            }
        }
    }

    pub fn validate(&self) -> Result<(), FrontendManifestError> {
        validate_asset(
            self.bundle_digest,
            self.css.name(),
            self.css.kind(),
            self.css.digest,
            self.css.public_path,
            self.css.bytes,
        )?;
        if let Some(javascript) = &self.javascript {
            validate_asset(
                self.bundle_digest,
                javascript.name(),
                javascript.kind(),
                javascript.digest,
                javascript.public_path,
                javascript.bytes,
            )?;
        }

        let calculated = if let Some(javascript) = &self.javascript {
            frontend_bundle_digest(&[
                FrontendDigestInput::new(FrontendAssetKind::Css, self.css.bytes),
                FrontendDigestInput::new(FrontendAssetKind::JavaScript, javascript.bytes),
            ])
        } else {
            frontend_bundle_digest(&[FrontendDigestInput::new(
                FrontendAssetKind::Css,
                self.css.bytes,
            )])
        }
        .map_err(|error| FrontendManifestError::DigestContract {
            message: error.to_string().into_boxed_str(),
        })?;
        if calculated != self.bundle_digest.0 {
            return Err(FrontendManifestError::BundleDigestMismatch);
        }
        Ok(())
    }
}

pub fn embedded_manifest() -> &'static FrontendAssetManifest {
    &GENERATED_FRONTEND_MANIFEST
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum FrontendManifestError {
    #[error("frontend {name} content digest does not match its embedded bytes")]
    AssetDigestMismatch { name: FrontendAssetName },
    #[error("frontend {name} path does not match its bundle digest and typed name")]
    AssetPathMismatch { name: FrontendAssetName },
    #[error("frontend bundle digest does not match its embedded assets")]
    BundleDigestMismatch,
    #[error("frontend digest contract failed: {message}")]
    DigestContract { message: Box<str> },
}

fn validate_asset(
    bundle: FrontendBundleDigest,
    name: FrontendAssetName,
    kind: FrontendAssetKind,
    digest: FrontendAssetDigest,
    public_path: FrontendAssetPath,
    bytes: &[u8],
) -> Result<(), FrontendManifestError> {
    if frontend_asset_digest(kind, bytes) != digest.0 {
        return Err(FrontendManifestError::AssetDigestMismatch { name });
    }
    let expected_path = format!("/app-assets/{bundle}/{name}");
    if public_path.as_str() != expected_path {
        return Err(FrontendManifestError::AssetPathMismatch { name });
    }
    Ok(())
}

fn parse_digest(
    value: &str,
    kind: FrontendDigestKind,
) -> Result<[u8; 32], FrontendDigestParseError> {
    let Some(hex) = value.strip_prefix(kind.prefix()) else {
        return Err(FrontendDigestParseError::InvalidPrefix {
            kind,
            expected: kind.prefix(),
        });
    };
    if hex.len() != DIGEST_HEX_LENGTH {
        return Err(FrontendDigestParseError::InvalidLength { kind });
    }
    if !hex
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(FrontendDigestParseError::InvalidEncoding { kind });
    }

    let mut bytes = [0_u8; 32];
    for (index, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high =
            decode_nibble(pair[0]).ok_or(FrontendDigestParseError::InvalidEncoding { kind })?;
        let low =
            decode_nibble(pair[1]).ok_or(FrontendDigestParseError::InvalidEncoding { kind })?;
        bytes[index] = high << 4 | low;
    }
    Ok(bytes)
}

const fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn format_digest(
    prefix: &str,
    bytes: &[u8; 32],
    formatter: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    formatter.write_str(prefix)?;
    for byte in bytes {
        write!(formatter, "{byte:02x}")?;
    }
    Ok(())
}

include!(concat!(env!("OUT_DIR"), "/frontend_manifest.rs"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_manifest_matches_every_embedded_byte_and_path() {
        let manifest = embedded_manifest();
        manifest.validate().unwrap();
        assert!(manifest.javascript().is_none());
        assert_eq!(manifest.css().name(), FrontendAssetName::Stylesheet);
        assert_eq!(manifest.css().kind(), FrontendAssetKind::Css);
        assert!(!manifest.css().bytes().is_empty());
        assert_eq!(
            manifest.css().public_path().as_str(),
            format!("/app-assets/{}/site.css", manifest.bundle_digest())
        );
    }

    #[test]
    fn exact_manifest_lookup_never_falls_back() {
        let manifest = embedded_manifest();
        assert!(matches!(
            manifest.lookup(manifest.bundle_digest(), FrontendAssetName::Stylesheet),
            Some(FrontendAsset::Css(_))
        ));
        assert!(
            manifest
                .lookup(manifest.bundle_digest(), FrontendAssetName::JavaScript)
                .is_none()
        );
        let different = FrontendBundleDigest([0x55; 32]);
        assert!(
            manifest
                .lookup(&different, FrontendAssetName::Stylesheet)
                .is_none()
        );
    }

    #[test]
    fn public_digest_encodings_are_full_strict_and_distinct() {
        let manifest = embedded_manifest();
        let bundle = manifest.bundle_digest().to_string();
        let asset = manifest.css().digest().to_string();
        assert_eq!(bundle.len(), FRONTEND_BUNDLE_PREFIX.len() + 64);
        assert_eq!(asset.len(), FRONTEND_ASSET_PREFIX.len() + 64);
        assert_ne!(bundle, asset);
        assert_eq!(
            FrontendBundleDigest::parse(&bundle).unwrap(),
            *manifest.bundle_digest()
        );
        assert_eq!(
            FrontendAssetDigest::parse(&asset).unwrap(),
            *manifest.css().digest()
        );
        assert!(FrontendBundleDigest::parse(&bundle.to_ascii_uppercase()).is_err());
        assert!(FrontendBundleDigest::parse(&asset).is_err());
    }

    #[test]
    fn header_metadata_is_typed_and_exact() {
        let css = embedded_manifest().css();
        assert_eq!(css.mime(), FrontendAssetMime::Css);
        assert_eq!(css.mime().as_str(), "text/css; charset=utf-8");
        assert_eq!(css.cache_policy(), FrontendCachePolicy::Immutable);
        assert_eq!(
            css.cache_policy().as_str(),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(css.etag().to_string(), format!("\"{}\"", css.digest()));
    }

    #[test]
    fn public_enum_wire_names_are_stable() {
        assert_eq!(serde_json::to_value(FrontendAssetKind::Css).unwrap(), "css");
        assert_eq!(
            serde_json::to_value(FrontendAssetKind::JavaScript).unwrap(),
            "java_script"
        );
        assert_eq!(
            serde_json::to_value(FrontendAssetName::Stylesheet).unwrap(),
            "stylesheet"
        );
        assert_eq!(
            serde_json::to_value(FrontendAssetName::JavaScript).unwrap(),
            "java_script"
        );
        assert_eq!(
            serde_json::to_value(FrontendAssetMime::JavaScript).unwrap(),
            "java_script"
        );
        assert_eq!(
            serde_json::to_value(FrontendCachePolicy::Immutable).unwrap(),
            "immutable"
        );
        assert_eq!(
            serde_json::from_value::<FrontendAssetName>("stylesheet".into()).unwrap(),
            FrontendAssetName::Stylesheet
        );
    }

    #[test]
    fn asset_name_parser_accepts_only_exact_public_names() {
        assert_eq!(
            FrontendAssetName::parse("site.css").unwrap(),
            FrontendAssetName::Stylesheet
        );
        for invalid in ["SITE.CSS", "site.js", "../site.css", "site.css/extra"] {
            if invalid == "site.js" {
                assert_eq!(
                    FrontendAssetName::parse(invalid).unwrap(),
                    FrontendAssetName::JavaScript
                );
            } else {
                assert!(FrontendAssetName::parse(invalid).is_err());
            }
        }
    }
}
