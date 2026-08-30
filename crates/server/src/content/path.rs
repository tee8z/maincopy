use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct LogicalAssetPath(String);

impl LogicalAssetPath {
    pub fn parse(value: &str) -> Result<Self, LogicalTreePathError> {
        let path = PortableLogicalPath::parse(value, usize::MAX)?;
        Self::from_portable(path)
    }

    pub(crate) fn from_portable(path: PortableLogicalPath) -> Result<Self, LogicalTreePathError> {
        let mut components = path.as_str().split('/');
        if components.next() != Some("assets") || components.next().is_none() {
            return Err(LogicalTreePathError::WrongAssetNamespace);
        }
        Ok(Self(path.0))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LogicalAssetPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Error, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalTreePathError {
    #[error("logical path must not be empty")]
    Empty,
    #[error("logical path must be relative")]
    Absolute,
    #[error("logical path contains an unsupported component")]
    UnsupportedComponent,
    #[error("logical path contains an encoded or platform-specific separator")]
    EncodedTraversal,
    #[error("logical path exceeds its byte limit")]
    TooLong,
    #[error("logical asset path must start with assets/ and name an entry")]
    WrongAssetNamespace,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PortableLogicalPath(pub(crate) String);

impl PortableLogicalPath {
    pub(crate) fn parse(value: &str, max_bytes: usize) -> Result<Self, LogicalTreePathError> {
        if value.is_empty() {
            return Err(LogicalTreePathError::Empty);
        }
        if value.len() > max_bytes {
            return Err(LogicalTreePathError::TooLong);
        }
        if value.starts_with('/') || value.starts_with('\\') {
            return Err(LogicalTreePathError::Absolute);
        }
        if value.contains(['%', '\\']) {
            return Err(LogicalTreePathError::EncodedTraversal);
        }
        if !value.split('/').all(is_portable_component) {
            return Err(LogicalTreePathError::UnsupportedComponent);
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn case_collision_key(&self) -> String {
        self.0.to_ascii_lowercase()
    }
}

fn is_portable_component(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
