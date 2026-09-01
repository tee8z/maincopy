//! Authored-content domain values and semantic rules.
//!
//! This module owns the valid authored model constructed by the compiler.

mod assets;
mod identity;
mod model;
mod path;
mod rules;

pub use assets::{
    AssetRevisionReference, DigestedAsset, ExternalAssetOrigin, ExternalAssetOriginError,
    ExternalAssetUrl, ExternalAssetUrlError,
};
pub use identity::{
    AssetDigest, DigestKind, DigestParseError, PostRevisionDigest, PreviewDigest,
    SiteSnapshotDigest,
};

pub use model::{
    AuthorName, AuthorSettings, DefaultPostTipPolicy, DraftStatus, LogicalContentPath,
    MarkdownSource, PlainTextError, PostAlias, PostCollection, PostDescription, PostDocument,
    PostId, PostIdParseError, PostMetadata, PostSlug, PostTag, PostTipPolicy, PostTitle,
    PublicationAssetSettings, PublicationBaseUrl, PublicationBaseUrlError, PublicationSettings,
    RouteValueError, SiteDescription, SiteSettings, SiteTitle, UnresolvedAssetReference,
    UnresolvedHttpsOrigin, ValidatedContent,
};
pub use path::{LogicalAssetPath, LogicalTreePathError};
pub(crate) use rules::{
    RouteConflict, RouteKind, classify_route_conflict, resolve_draft_status, timestamps_are_ordered,
};
