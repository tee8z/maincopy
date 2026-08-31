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
pub const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

macro_rules! frontend_enum_serde {
    ($enum:ident, $error:literal, { $($variant:ident => $wire:literal),+ $(,)? }) => {
        impl Serialize for $enum {
            fn serialize<SerializerType>(
                &self,
                serializer: SerializerType,
            ) -> Result<SerializerType::Ok, SerializerType::Error>
            where
                SerializerType: Serializer,
            {
                serializer.serialize_str(match self {
                    $(Self::$variant => $wire),+
                })
            }
        }

        impl<'de> Deserialize<'de> for $enum {
            fn deserialize<DeserializerType>(
                deserializer: DeserializerType,
            ) -> Result<Self, DeserializerType::Error>
            where
                DeserializerType: Deserializer<'de>,
            {
                match String::deserialize(deserializer)?.as_str() {
                    $($wire => Ok(Self::$variant)),+,
                    _ => Err(de::Error::custom($error)),
                }
            }
        }
    };
}

frontend_enum_serde!(FrontendAssetKind, "frontend asset kind is not recognized", {
    Css => "css",
    JavaScript => "java_script",
});
frontend_enum_serde!(FrontendAssetName, "frontend asset name is not recognized", {
    Stylesheet => "stylesheet",
    JavaScript => "java_script",
});

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

macro_rules! frontend_digest {
    ($digest:ident, $kind:ident, $prefix:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $digest([u8; 32]);

        impl $digest {
            const fn from_generated(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            pub fn parse(value: &str) -> Result<Self, FrontendDigestParseError> {
                parse_digest(value, FrontendDigestKind::$kind).map(Self)
            }

            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl fmt::Display for $digest {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                format_digest($prefix, &self.0, formatter)
            }
        }

        impl FromStr for $digest {
            type Err = FrontendDigestParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl Serialize for $digest {
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

        impl<'de> Deserialize<'de> for $digest {
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

frontend_digest!(FrontendBundleDigest, Bundle, FRONTEND_BUNDLE_PREFIX);
frontend_digest!(FrontendAssetDigest, Asset, FRONTEND_ASSET_PREFIX);

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct FrontendAsset {
    pub kind: FrontendAssetKind,
    pub digest: FrontendAssetDigest,
    pub public_path: &'static str,
    pub bytes: &'static [u8],
}

impl FrontendAsset {
    pub const fn name(&self) -> FrontendAssetName {
        match self.kind {
            FrontendAssetKind::Css => FrontendAssetName::Stylesheet,
            FrontendAssetKind::JavaScript => FrontendAssetName::JavaScript,
        }
    }

    pub const fn mime(&self) -> &'static str {
        match self.kind {
            FrontendAssetKind::Css => "text/css; charset=utf-8",
            FrontendAssetKind::JavaScript => "text/javascript; charset=utf-8",
        }
    }

    pub fn etag(&self) -> String {
        format!("\"{}\"", self.digest)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct FrontendAssetManifest {
    pub bundle_digest: FrontendBundleDigest,
    pub css: FrontendAsset,
    pub javascript: Option<FrontendAsset>,
}

impl FrontendAssetManifest {
    pub fn lookup(
        &self,
        bundle: &FrontendBundleDigest,
        name: FrontendAssetName,
    ) -> Option<&FrontendAsset> {
        if bundle != &self.bundle_digest {
            return None;
        }
        match name {
            FrontendAssetName::Stylesheet => Some(&self.css),
            FrontendAssetName::JavaScript => self.javascript.as_ref(),
        }
    }

    pub fn validate(&self) -> Result<(), FrontendManifestError> {
        validate_asset(self.bundle_digest, &self.css)?;
        if let Some(javascript) = &self.javascript {
            validate_asset(self.bundle_digest, javascript)?;
        }

        let calculated = if let Some(javascript) = &self.javascript {
            frontend_bundle_digest(&[
                FrontendDigestInput {
                    kind: self.css.kind,
                    bytes: self.css.bytes,
                },
                FrontendDigestInput {
                    kind: javascript.kind,
                    bytes: javascript.bytes,
                },
            ])
        } else {
            frontend_bundle_digest(&[FrontendDigestInput {
                kind: self.css.kind,
                bytes: self.css.bytes,
            }])
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
    asset: &FrontendAsset,
) -> Result<(), FrontendManifestError> {
    if frontend_asset_digest(asset.kind, asset.bytes) != asset.digest.0 {
        return Err(FrontendManifestError::AssetDigestMismatch { name: asset.name() });
    }
    let expected_path = format!("/app-assets/{bundle}/{}", asset.name());
    if asset.public_path != expected_path {
        return Err(FrontendManifestError::AssetPathMismatch { name: asset.name() });
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
        assert_eq!(manifest.css.name(), FrontendAssetName::Stylesheet);
        assert_eq!(manifest.css.kind, FrontendAssetKind::Css);
        assert!(!manifest.css.bytes.is_empty());
        assert_eq!(
            manifest.css.public_path,
            format!("/app-assets/{}/site.css", manifest.bundle_digest)
        );
        let javascript = manifest.javascript.as_ref().unwrap();
        assert_eq!(javascript.name(), FrontendAssetName::JavaScript);
        assert_eq!(javascript.kind, FrontendAssetKind::JavaScript);
        assert!(!javascript.bytes.is_empty());
        assert_eq!(
            javascript.public_path,
            format!("/app-assets/{}/site.js", manifest.bundle_digest)
        );
    }

    #[test]
    fn exact_manifest_lookup_never_falls_back() {
        let manifest = embedded_manifest();
        assert_eq!(
            manifest.lookup(&manifest.bundle_digest, FrontendAssetName::Stylesheet),
            Some(&manifest.css)
        );
        assert_eq!(
            manifest.lookup(&manifest.bundle_digest, FrontendAssetName::JavaScript),
            manifest.javascript.as_ref()
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
        let bundle = manifest.bundle_digest.to_string();
        let asset = manifest.css.digest.to_string();
        assert_eq!(bundle.len(), FRONTEND_BUNDLE_PREFIX.len() + 64);
        assert_eq!(asset.len(), FRONTEND_ASSET_PREFIX.len() + 64);
        assert_ne!(bundle, asset);
        assert_eq!(
            FrontendBundleDigest::parse(&bundle).unwrap(),
            manifest.bundle_digest
        );
        assert_eq!(
            FrontendAssetDigest::parse(&asset).unwrap(),
            manifest.css.digest
        );
        assert_eq!(
            serde_json::to_value(manifest.bundle_digest).unwrap(),
            bundle
        );
        assert_eq!(serde_json::to_value(manifest.css.digest).unwrap(), asset);
        assert_eq!(
            serde_json::from_value::<FrontendBundleDigest>(bundle.clone().into()).unwrap(),
            manifest.bundle_digest
        );
        assert_eq!(
            serde_json::from_value::<FrontendAssetDigest>(asset.clone().into()).unwrap(),
            manifest.css.digest
        );
        assert!(FrontendBundleDigest::parse(&bundle.to_ascii_uppercase()).is_err());
        assert!(FrontendBundleDigest::parse(&asset).is_err());
    }

    #[test]
    fn header_metadata_is_typed_and_exact() {
        let css = &embedded_manifest().css;
        assert_eq!(css.mime(), "text/css; charset=utf-8");
        assert_eq!(
            IMMUTABLE_CACHE_CONTROL,
            "public, max-age=31536000, immutable"
        );
        assert_eq!(css.etag(), format!("\"{}\"", css.digest));
    }

    #[test]
    fn optional_javascript_manifest_keeps_javascript_semantics() {
        let css_bytes: &'static [u8] = b"body{color:black}";
        let javascript_bytes: &'static [u8] = b"console.log('maincopy')";
        let bundle_digest = FrontendBundleDigest(
            frontend_bundle_digest(&[
                FrontendDigestInput {
                    kind: FrontendAssetKind::Css,
                    bytes: css_bytes,
                },
                FrontendDigestInput {
                    kind: FrontendAssetKind::JavaScript,
                    bytes: javascript_bytes,
                },
            ])
            .unwrap(),
        );
        let css_path = Box::leak(format!("/app-assets/{bundle_digest}/site.css").into_boxed_str());
        let javascript_path =
            Box::leak(format!("/app-assets/{bundle_digest}/site.js").into_boxed_str());
        let css = FrontendAsset {
            kind: FrontendAssetKind::Css,
            digest: FrontendAssetDigest(frontend_asset_digest(FrontendAssetKind::Css, css_bytes)),
            public_path: css_path,
            bytes: css_bytes,
        };
        let javascript = FrontendAsset {
            kind: FrontendAssetKind::JavaScript,
            digest: FrontendAssetDigest(frontend_asset_digest(
                FrontendAssetKind::JavaScript,
                javascript_bytes,
            )),
            public_path: javascript_path,
            bytes: javascript_bytes,
        };

        let invalid_css = FrontendAssetManifest {
            bundle_digest,
            css: javascript.clone(),
            javascript: None,
        };
        assert!(matches!(
            invalid_css.validate(),
            Err(FrontendManifestError::DigestContract { message })
                if message.as_ref() == "frontend bundle must start with one CSS asset"
        ));
        let invalid_javascript = FrontendAssetManifest {
            bundle_digest,
            css: css.clone(),
            javascript: Some(css.clone()),
        };
        assert!(matches!(
            invalid_javascript.validate(),
            Err(FrontendManifestError::DigestContract { message })
                if message.as_ref()
                    == "frontend bundle assets must be unique and ordered by their typed kind"
        ));

        let manifest = FrontendAssetManifest {
            bundle_digest,
            css,
            javascript: Some(javascript.clone()),
        };

        manifest.validate().unwrap();
        let asset = manifest
            .lookup(&bundle_digest, FrontendAssetName::JavaScript)
            .unwrap();

        assert_eq!(asset.name(), FrontendAssetName::JavaScript);
        assert_eq!(asset.kind, FrontendAssetKind::JavaScript);
        assert_eq!(asset.digest, javascript.digest);
        assert_eq!(asset.public_path, javascript.public_path);
        assert_eq!(asset.mime(), "text/javascript; charset=utf-8");
        assert_eq!(asset.etag(), javascript.etag());
        assert_eq!(asset.bytes, javascript_bytes);
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
