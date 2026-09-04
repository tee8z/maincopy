use std::{fmt, str::FromStr};

use maincopy_shared::source::{GIT_SHA1_SOURCE_COMMIT_PREFIX, GIT_SHA256_SOURCE_COMMIT_PREFIX};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceCommitAlgorithm {
    Sha1,
    Sha256,
}

impl SourceCommitAlgorithm {
    const fn byte_length(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceCommit {
    algorithm: SourceCommitAlgorithm,
    bytes: Box<[u8]>,
    encoded: Box<str>,
}

impl SourceCommit {
    pub fn parse(value: &str) -> Result<Self, SourceCommitParseError> {
        let (algorithm, hex) = if let Some(hex) = value.strip_prefix(GIT_SHA1_SOURCE_COMMIT_PREFIX)
        {
            (SourceCommitAlgorithm::Sha1, hex)
        } else if let Some(hex) = value.strip_prefix(GIT_SHA256_SOURCE_COMMIT_PREFIX) {
            (SourceCommitAlgorithm::Sha256, hex)
        } else {
            return Err(SourceCommitParseError::InvalidPrefix);
        };
        if hex.len() != algorithm.byte_length() * 2 {
            return Err(SourceCommitParseError::InvalidLength { algorithm });
        }
        if !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(SourceCommitParseError::InvalidEncoding { algorithm });
        }
        let mut bytes = Vec::with_capacity(algorithm.byte_length());
        for pair in hex.as_bytes().as_chunks::<2>().0 {
            let high = decode_nibble(pair[0])
                .ok_or(SourceCommitParseError::InvalidEncoding { algorithm })?;
            let low = decode_nibble(pair[1])
                .ok_or(SourceCommitParseError::InvalidEncoding { algorithm })?;
            bytes.push(high << 4 | low);
        }
        Ok(Self {
            algorithm,
            bytes: bytes.into_boxed_slice(),
            encoded: value.into(),
        })
    }

    pub(crate) fn from_git_hex(value: &str) -> Result<Self, SourceCommitParseError> {
        match value.len() {
            40 => Self::parse(&format!("{GIT_SHA1_SOURCE_COMMIT_PREFIX}{value}")),
            64 => Self::parse(&format!("{GIT_SHA256_SOURCE_COMMIT_PREFIX}{value}")),
            _ => Err(SourceCommitParseError::UnsupportedObjectFormat),
        }
    }

    pub const fn algorithm(&self) -> SourceCommitAlgorithm {
        self.algorithm
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn as_str(&self) -> &str {
        &self.encoded
    }
}

impl fmt::Display for SourceCommit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SourceCommit {
    type Err = SourceCommitParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for SourceCommit {
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

impl<'de> Deserialize<'de> for SourceCommit {
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

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SourceCommitParseError {
    #[error("source commit must start with git-sha1: or git-sha256:")]
    InvalidPrefix,
    #[error("{algorithm:?} source commit has the wrong encoded length")]
    InvalidLength { algorithm: SourceCommitAlgorithm },
    #[error("{algorithm:?} source commit must use lowercase hexadecimal")]
    InvalidEncoding { algorithm: SourceCommitAlgorithm },
    #[error("Git object format is not supported")]
    UnsupportedObjectFormat,
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
    fn source_commits_are_strict_and_algorithm_typed() {
        let sha1 = SourceCommit::parse(&format!("git-sha1:{}", "ab".repeat(20))).unwrap();
        assert_eq!(sha1.algorithm(), SourceCommitAlgorithm::Sha1);
        assert_eq!(sha1.as_bytes().len(), 20);

        let sha256 = SourceCommit::parse(&format!("git-sha256:{}", "cd".repeat(32))).unwrap();
        assert_eq!(sha256.algorithm(), SourceCommitAlgorithm::Sha256);
        assert_eq!(sha256.as_bytes().len(), 32);

        for invalid in [
            "ab".repeat(20),
            format!("git-sha1:{}", "AB".repeat(20)),
            format!("git-sha1:{}", "ab".repeat(19)),
            format!("git-sha256:{}", "gg".repeat(32)),
        ] {
            assert!(SourceCommit::parse(&invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn source_commit_serde_preserves_the_versioned_wire_value() {
        let value = format!("git-sha1:{}", "01".repeat(20));
        let commit = SourceCommit::parse(&value).unwrap();
        assert_eq!(serde_json::to_value(&commit).unwrap(), value);
        assert_eq!(
            serde_json::from_value::<SourceCommit>(serde_json::json!(value)).unwrap(),
            commit
        );
    }

    #[test]
    fn source_commit_algorithm_wire_names_are_stable() {
        for (value, expected) in [
            (
                serde_json::to_value(SourceCommitAlgorithm::Sha1).unwrap(),
                "sha1",
            ),
            (
                serde_json::to_value(SourceCommitAlgorithm::Sha256).unwrap(),
                "sha256",
            ),
        ] {
            assert_eq!(value, serde_json::json!(expected));
        }
    }
}
