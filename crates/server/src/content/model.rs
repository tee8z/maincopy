use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct LogicalContentPath(String);

impl LogicalContentPath {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LogicalContentPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct PublicationSource<'source> {
    pub path: LogicalContentPath,
    pub contents: &'source str,
}

impl<'source> PublicationSource<'source> {
    pub fn new(path: impl Into<String>, contents: &'source str) -> Self {
        Self {
            path: LogicalContentPath::new(path),
            contents,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PostSource<'source> {
    pub path: LogicalContentPath,
    pub contents: &'source str,
    pub collection: PostCollection,
}

impl<'source> PostSource<'source> {
    pub fn in_posts(path: impl Into<String>, contents: &'source str) -> Self {
        Self {
            path: LogicalContentPath::new(path),
            contents,
            collection: PostCollection::Posts,
        }
    }

    pub fn in_drafts(path: impl Into<String>, contents: &'source str) -> Self {
        Self {
            path: LogicalContentPath::new(path),
            contents,
            collection: PostCollection::Drafts,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PostCollection {
    Posts,
    Drafts,
}

impl PostCollection {
    pub const fn directory(self) -> &'static str {
        match self {
            Self::Posts => "posts",
            Self::Drafts => "drafts",
        }
    }

    pub(crate) fn contains_path(self, path: &str) -> bool {
        path.strip_prefix(self.directory())
            .and_then(|remainder| remainder.strip_prefix('/'))
            .is_some_and(|remainder| !remainder.is_empty())
    }
}
