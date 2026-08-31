use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

const ASSET_PREFIX: &str = "asset-b3-v1-";
const POST_PREFIX: &str = "post-b3-v1-";
const PREVIEW_PREFIX: &str = "preview-b3-v1-";
const SITE_PREFIX: &str = "site-b3-v1-";
const DIGEST_HEX_LENGTH: usize = 64;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestKind {
    Asset,
    PostRevision,
    Preview,
    SiteSnapshot,
}

impl DigestKind {
    const fn prefix(self) -> &'static str {
        match self {
            Self::Asset => ASSET_PREFIX,
            Self::PostRevision => POST_PREFIX,
            Self::Preview => PREVIEW_PREFIX,
            Self::SiteSnapshot => SITE_PREFIX,
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DigestParseError {
    #[error("{kind:?} digest must start with {expected}")]
    InvalidPrefix {
        kind: DigestKind,
        expected: &'static str,
    },
    #[error("{kind:?} digest must contain exactly 32 encoded bytes")]
    InvalidLength { kind: DigestKind },
    #[error("{kind:?} digest must use lowercase hexadecimal")]
    InvalidEncoding { kind: DigestKind },
}

macro_rules! public_digest_type {
    ($name:ident, $kind:expr) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name {
            bytes: [u8; 32],
            encoded: Box<str>,
        }

        impl $name {
            pub fn parse(value: &str) -> Result<Self, DigestParseError> {
                let bytes = parse_digest(value, $kind)?;
                Ok(Self::from_bytes(bytes))
            }

            pub fn as_str(&self) -> &str {
                &self.encoded
            }

            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.bytes
            }

            pub fn from_bytes(bytes: [u8; 32]) -> Self {
                let hash = blake3::Hash::from_bytes(bytes);
                let encoded = format!("{}{}", $kind.prefix(), hash.to_hex()).into_boxed_str();
                Self { bytes, encoded }
            }

            pub(crate) fn from_hash(hash: blake3::Hash) -> Self {
                Self::from_bytes(*hash.as_bytes())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = DigestParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
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

public_digest_type!(AssetDigest, DigestKind::Asset);
public_digest_type!(PostRevisionDigest, DigestKind::PostRevision);
public_digest_type!(PreviewDigest, DigestKind::Preview);
public_digest_type!(SiteSnapshotDigest, DigestKind::SiteSnapshot);

fn parse_digest(value: &str, kind: DigestKind) -> Result<[u8; 32], DigestParseError> {
    let Some(hex) = value.strip_prefix(kind.prefix()) else {
        return Err(DigestParseError::InvalidPrefix {
            kind,
            expected: kind.prefix(),
        });
    };
    if hex.len() != DIGEST_HEX_LENGTH {
        return Err(DigestParseError::InvalidLength { kind });
    }
    let mut bytes = [0_u8; 32];
    for (index, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high = decode_nibble(pair[0]).ok_or(DigestParseError::InvalidEncoding { kind })?;
        let low = decode_nibble(pair[1]).ok_or(DigestParseError::InvalidEncoding { kind })?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_digests_require_exact_versioned_lowercase_encodings() {
        for (valid, kind) in [
            (
                format!("asset-b3-v1-{}", "ab".repeat(32)),
                DigestKind::Asset,
            ),
            (
                format!("post-b3-v1-{}", "ab".repeat(32)),
                DigestKind::PostRevision,
            ),
            (
                format!("preview-b3-v1-{}", "ab".repeat(32)),
                DigestKind::Preview,
            ),
            (
                format!("site-b3-v1-{}", "ab".repeat(32)),
                DigestKind::SiteSnapshot,
            ),
        ] {
            let parsed = match kind {
                DigestKind::Asset => {
                    AssetDigest::parse(&valid).map(|value| value.as_str().to_owned())
                }
                DigestKind::PostRevision => {
                    PostRevisionDigest::parse(&valid).map(|value| value.as_str().to_owned())
                }
                DigestKind::Preview => {
                    PreviewDigest::parse(&valid).map(|value| value.as_str().to_owned())
                }
                DigestKind::SiteSnapshot => {
                    SiteSnapshotDigest::parse(&valid).map(|value| value.as_str().to_owned())
                }
            };
            assert_eq!(parsed.unwrap(), valid);
        }

        assert!(AssetDigest::parse(&format!("asset-b3-v1-{}", "AB".repeat(32))).is_err());
        assert!(PostRevisionDigest::parse(&format!("post-b3-v1-{}", "aa".repeat(31))).is_err());
        assert!(SiteSnapshotDigest::parse(&format!("post-b3-v1-{}", "aa".repeat(32))).is_err());
        assert!(AssetDigest::parse(&format!("asset-b3-v2-{}", "aa".repeat(32))).is_err());
        assert!(AssetDigest::parse(&format!("asset-sha256-v1-{}", "aa".repeat(32))).is_err());
        assert!(AssetDigest::parse(&format!("asset-b3-v1-{}", "gg".repeat(32))).is_err());

        let value = format!("post-b3-v1-{}", "12".repeat(32));
        let digest = PostRevisionDigest::parse(&value).unwrap();
        assert_eq!(PostRevisionDigest::from_bytes(*digest.as_bytes()), digest);
        assert_eq!(serde_json::to_value(&digest).unwrap(), value);
        assert_eq!(
            serde_json::from_value::<PostRevisionDigest>(serde_json::json!(value)).unwrap(),
            digest
        );
    }

    #[test]
    fn digest_kind_wire_names_are_stable() {
        for (value, expected) in [
            (serde_json::to_value(DigestKind::Asset).unwrap(), "asset"),
            (
                serde_json::to_value(DigestKind::PostRevision).unwrap(),
                "post_revision",
            ),
            (
                serde_json::to_value(DigestKind::Preview).unwrap(),
                "preview",
            ),
            (
                serde_json::to_value(DigestKind::SiteSnapshot).unwrap(),
                "site_snapshot",
            ),
        ] {
            assert_eq!(value, serde_json::json!(expected));
        }
    }
}
