//! Markdown content parsing, validation, discovery, asset resolution, and identity.

mod assets;
mod candidate_store;
mod content;
pub mod identity;
mod model;
mod parser;
mod path;
mod resolver;
pub mod tree;
mod tree_digest;
mod validation;

pub use assets::{
    AuthoredMarkdownDestination, MarkdownDestinationKind, MarkdownDestinationOrdinal,
    MarkdownSourceRange, ResolvedMarkdownDestination, ResolvedPostAssets, ResolvedSiteAssets,
};
pub use candidate_store::{
    ContentCandidateStore, ContentCandidateStoreError, RetainedContentCandidate,
};
pub use content::{
    AssetDigest, AssetRevisionReference, AuthorName, AuthorSettings, DefaultPostTipPolicy,
    DigestKind, DigestParseError, DigestedAsset, DistributionCopy, DistributionMode,
    DistributionSettings, DraftStatus, ExternalAssetOrigin, ExternalAssetOriginError,
    ExternalAssetUrl, ExternalAssetUrlError, LogicalAssetPath, LogicalContentPath,
    LogicalTreePathError, MarkdownSource, PlainTextError, PostAlias, PostCollection,
    PostDescription, PostDocument, PostId, PostIdParseError, PostMetadata, PostRevisionDigest,
    PostSlug, PostTag, PostTipPolicy, PostTitle, PreviewDigest, PrivacyPolicyRevision,
    PublicationAssetSettings, PublicationBaseUrl, PublicationBaseUrlError, PublicationSettings,
    RouteValueError, SiteDescription, SiteSettings, SiteSnapshotDigest, SiteTitle,
    SubscriptionSettings, UnresolvedAssetReference, UnresolvedHttpsOrigin, ValidatedContent,
    XDistributionSettings,
};
pub use identity::{
    AssetBindingTarget, PostRendererIdentity, RevisionIdentityError, SiteShellRendererIdentity,
    digest_asset,
};
pub use model::{PostSource, PublicationSource};
pub use parser::{validate_content, validate_post_document, validate_post_document_bytes};
pub use resolver::{
    AllowedOriginOrdinal, AssetReferenceLocation, AssetResolutionCode, AssetResolutionError,
    AssetResolutionErrors, AssetResolutionWarning, AssetResolutionWarningCode,
    AssetResolutionWarnings, ResolveContentAssetsError, ResolvedContentAssets, ResolvedLocalAsset,
    ResolvedLocalAssetLookupError, ResolvedLocalAssetStore, ResolvedPostAssetLookupError,
    ResolvedPostAssetSet, ResolvedSiteAssetLookupError, resolve_content_assets,
};
pub use tree::{
    ContentDepthLimit, ContentEntryLimit, ContentFileByteLimit, ContentPathByteLimit,
    ContentTreeByteLimit, ContentTreeLimits, ContentTreeLimitsError, DiscoveredAsset,
    DiscoveredContentTree, DiscoveredPost, DiscoveredPublication, discover_content_tree,
};
pub use tree_digest::{ContentTreeDigest, ContentTreeDigestParseError};
pub use validation::{
    ContentValidationCode, ContentValidationError, ContentValidationErrors, FieldPath,
    ValidationLocation,
};

pub(crate) use validation::DiagnosticCollector;

#[cfg(test)]
mod tests;
