use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::Path,
    sync::Arc,
};

use serde::Serialize;
use thiserror::Error;

pub(crate) use super::path::PortableLogicalPath;
use super::{
    ContentValidationCode, ContentValidationError, ContentValidationErrors, LogicalAssetPath,
    LogicalContentPath, LogicalTreePathError, PostCollection, PostSource, PublicationSource,
    ValidatedContent, validate_content,
};

#[cfg(target_os = "linux")]
mod linux;

#[cfg(test)]
mod tests;

const DEFAULT_PUBLICATION_BYTES: u64 = 256 * 1024;
const DEFAULT_POST_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_ASSET_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_TREE_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_ENTRIES: usize = 10_000;
const DEFAULT_DEPTH: usize = 16;
const DEFAULT_PATH_BYTES: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ContentFileByteLimit(NonZeroU64);

impl ContentFileByteLimit {
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) if value.get() < u64::MAX => Some(Self(value)),
            _ => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }

    fn default_value(value: u64) -> Self {
        Self(NonZeroU64::new(value).unwrap_or(NonZeroU64::MIN))
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ContentTreeByteLimit(NonZeroU64);

impl ContentTreeByteLimit {
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) if value.get() < u64::MAX => Some(Self(value)),
            _ => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }

    fn default_value(value: u64) -> Self {
        Self(NonZeroU64::new(value).unwrap_or(NonZeroU64::MIN))
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ContentEntryLimit(NonZeroUsize);

impl ContentEntryLimit {
    pub const fn new(value: usize) -> Option<Self> {
        match NonZeroUsize::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }

    fn default_value(value: usize) -> Self {
        Self(NonZeroUsize::new(value).unwrap_or(NonZeroUsize::MIN))
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ContentDepthLimit(NonZeroUsize);

impl ContentDepthLimit {
    pub const fn new(value: usize) -> Option<Self> {
        match NonZeroUsize::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }

    fn default_value(value: usize) -> Self {
        Self(NonZeroUsize::new(value).unwrap_or(NonZeroUsize::MIN))
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ContentPathByteLimit(NonZeroUsize);

impl ContentPathByteLimit {
    pub const fn new(value: usize) -> Option<Self> {
        match NonZeroUsize::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }

    fn default_value(value: usize) -> Self {
        Self(NonZeroUsize::new(value).unwrap_or(NonZeroUsize::MIN))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContentTreeLimits {
    publication_file_bytes: ContentFileByteLimit,
    post_file_bytes: ContentFileByteLimit,
    asset_file_bytes: ContentFileByteLimit,
    total_tree_bytes: ContentTreeByteLimit,
    entries: ContentEntryLimit,
    depth: ContentDepthLimit,
    path_bytes: ContentPathByteLimit,
}

impl ContentTreeLimits {
    pub fn new(
        publication_file_bytes: ContentFileByteLimit,
        post_file_bytes: ContentFileByteLimit,
        asset_file_bytes: ContentFileByteLimit,
        total_tree_bytes: ContentTreeByteLimit,
        entries: ContentEntryLimit,
        depth: ContentDepthLimit,
        path_bytes: ContentPathByteLimit,
    ) -> Result<Self, ContentTreeLimitsError> {
        if [publication_file_bytes, post_file_bytes, asset_file_bytes]
            .into_iter()
            .any(|limit| limit.get() > total_tree_bytes.get())
        {
            return Err(ContentTreeLimitsError);
        }
        Ok(Self {
            publication_file_bytes,
            post_file_bytes,
            asset_file_bytes,
            total_tree_bytes,
            entries,
            depth,
            path_bytes,
        })
    }

    pub const fn publication_file_bytes(self) -> ContentFileByteLimit {
        self.publication_file_bytes
    }

    pub const fn post_file_bytes(self) -> ContentFileByteLimit {
        self.post_file_bytes
    }

    pub const fn asset_file_bytes(self) -> ContentFileByteLimit {
        self.asset_file_bytes
    }

    pub const fn total_tree_bytes(self) -> ContentTreeByteLimit {
        self.total_tree_bytes
    }

    pub const fn entries(self) -> ContentEntryLimit {
        self.entries
    }

    pub const fn depth(self) -> ContentDepthLimit {
        self.depth
    }

    pub const fn path_bytes(self) -> ContentPathByteLimit {
        self.path_bytes
    }

    pub(crate) const fn file_limit(self, kind: ContentFileKind) -> ContentFileByteLimit {
        match kind {
            ContentFileKind::Publication => self.publication_file_bytes,
            ContentFileKind::Post(_) => self.post_file_bytes,
            ContentFileKind::Asset => self.asset_file_bytes,
        }
    }
}

impl Default for ContentTreeLimits {
    fn default() -> Self {
        Self {
            publication_file_bytes: ContentFileByteLimit::default_value(DEFAULT_PUBLICATION_BYTES),
            post_file_bytes: ContentFileByteLimit::default_value(DEFAULT_POST_BYTES),
            asset_file_bytes: ContentFileByteLimit::default_value(DEFAULT_ASSET_BYTES),
            total_tree_bytes: ContentTreeByteLimit::default_value(DEFAULT_TREE_BYTES),
            entries: ContentEntryLimit::default_value(DEFAULT_ENTRIES),
            depth: ContentDepthLimit::default_value(DEFAULT_DEPTH),
            path_bytes: ContentPathByteLimit::default_value(DEFAULT_PATH_BYTES),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("each file limit must not exceed the complete tree limit")]
pub struct ContentTreeLimitsError;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ContentByteCount(u64);

impl ContentByteCount {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredPublication {
    path: LogicalContentPath,
    source: Box<str>,
}

impl DiscoveredPublication {
    pub const fn path(&self) -> &LogicalContentPath {
        &self.path
    }

    pub fn source(&self) -> &str {
        &self.source
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredPost {
    path: LogicalContentPath,
    collection: PostCollection,
    source: Box<str>,
}

impl DiscoveredPost {
    pub const fn path(&self) -> &LogicalContentPath {
        &self.path
    }

    pub const fn collection(&self) -> PostCollection {
        self.collection
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    fn as_post_source(&self) -> PostSource<'_> {
        PostSource::in_collection(self.path.as_str(), self.source(), self.collection)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredAsset {
    path: LogicalAssetPath,
    bytes: Arc<[u8]>,
}

impl DiscoveredAsset {
    pub const fn path(&self) -> &LogicalAssetPath {
        &self.path
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn owned_bytes(&self) -> Arc<[u8]> {
        Arc::clone(&self.bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredContentTree {
    publication: DiscoveredPublication,
    posts: Vec<DiscoveredPost>,
    assets: Vec<DiscoveredAsset>,
    total_bytes: ContentByteCount,
}

impl DiscoveredContentTree {
    pub const fn publication(&self) -> &DiscoveredPublication {
        &self.publication
    }

    pub fn posts(&self) -> &[DiscoveredPost] {
        &self.posts
    }

    pub fn assets(&self) -> &[DiscoveredAsset] {
        &self.assets
    }

    pub const fn total_bytes(&self) -> ContentByteCount {
        self.total_bytes
    }

    pub fn validate(&self) -> Result<ValidatedContent, ContentValidationErrors> {
        validate_content(
            PublicationSource::new(self.publication.path.as_str(), self.publication.source()),
            self.posts.iter().map(DiscoveredPost::as_post_source),
        )
    }

    pub(crate) fn new(
        publication: DiscoveredPublication,
        posts: Vec<DiscoveredPost>,
        assets: Vec<DiscoveredAsset>,
        total_bytes: u64,
    ) -> Self {
        Self {
            publication,
            posts,
            assets,
            total_bytes: ContentByteCount(total_bytes),
        }
    }
}

pub fn discover_content_tree(
    root: &Path,
    limits: ContentTreeLimits,
) -> Result<DiscoveredContentTree, ContentValidationErrors> {
    discover_content_tree_with_hooks(root, limits, || {}, |_| {}, |_| {})
}

#[cfg(test)]
fn discover_content_tree_with_hook(
    root: &Path,
    limits: ContentTreeLimits,
    before_read: impl FnOnce(),
) -> Result<DiscoveredContentTree, ContentValidationErrors> {
    discover_content_tree_with_hooks(root, limits, before_read, |_| {}, |_| {})
}

#[cfg(target_os = "linux")]
fn discover_content_tree_with_hooks(
    root: &Path,
    limits: ContentTreeLimits,
    before_read: impl FnOnce(),
    before_file_read: impl FnMut(&str),
    after_file_read: impl FnMut(&str),
) -> Result<DiscoveredContentTree, ContentValidationErrors> {
    linux::discover(root, limits, before_read, before_file_read, after_file_read)
}

#[cfg(not(target_os = "linux"))]
fn discover_content_tree_with_hooks(
    _root: &Path,
    _limits: ContentTreeLimits,
    _before_read: impl FnOnce(),
    _before_file_read: impl FnMut(&str),
    _after_file_read: impl FnMut(&str),
) -> Result<DiscoveredContentTree, ContentValidationErrors> {
    let mut diagnostics = super::DiagnosticCollector::default();
    diagnostics.push(ContentValidationError::new(
        LogicalContentPath::new("<content-root>"),
        "$path",
        ContentValidationCode::ContentPlatformUnsupported,
        "safe content discovery requires the supported Linux filesystem boundary",
    ));
    Err(diagnostics.finish())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContentFileKind {
    Publication,
    Post(PostCollection),
    Asset,
}

pub(crate) fn tree_error(
    path: impl Into<String>,
    code: ContentValidationCode,
    message: impl Into<String>,
) -> ContentValidationError {
    ContentValidationError::new(LogicalContentPath::new(path), "$path", code, message)
}

pub(crate) fn publication(path: impl Into<String>, source: String) -> DiscoveredPublication {
    DiscoveredPublication {
        path: LogicalContentPath::new(path),
        source: source.into_boxed_str(),
    }
}

pub(crate) fn post(
    path: impl Into<String>,
    collection: PostCollection,
    source: String,
) -> DiscoveredPost {
    DiscoveredPost {
        path: LogicalContentPath::new(path),
        collection,
        source: source.into_boxed_str(),
    }
}

pub(crate) fn asset(path: LogicalAssetPath, bytes: Vec<u8>) -> DiscoveredAsset {
    DiscoveredAsset {
        path,
        bytes: Arc::from(bytes),
    }
}
