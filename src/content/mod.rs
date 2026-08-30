//! Git-owned content types and compilation.

mod assets;
mod identity;
mod model;
mod parser;
mod path;
mod provenance;
pub(crate) mod render;
mod resolver;
mod tree;
mod validation;

pub use assets::{
    AssetRevisionReference, AuthoredMarkdownDestination, DigestedAsset, ExternalAssetOrigin,
    ExternalAssetOriginError, ExternalAssetUrl, ExternalAssetUrlError, MarkdownDestinationKind,
    MarkdownDestinationOrdinal, MarkdownSourceRange, ResolvedMarkdownDestination,
    ResolvedPostAssets, ResolvedSiteAssets, SnapshotAssetPath, SnapshotAssetPathError,
};
pub use identity::{
    AssetBindingTarget, AssetDigest, DigestKind, DigestParseError, PostRevisionDigest,
    PublishedPostRevision, RevisionIdentityError, SiteSnapshotDigest, digest_asset,
};
pub(crate) use identity::{PostRendererIdentity, SiteShellRendererIdentity};

pub use model::{
    AuthorName, AuthorSettings, DefaultPostTipPolicy, DistributionCopy, DistributionMode,
    DistributionSettings, DraftStatus, LogicalContentPath, MarkdownSource, PlainTextError,
    PostAlias, PostCollection, PostDescription, PostDocument, PostId, PostIdParseError,
    PostMetadata, PostSlug, PostSource, PostTag, PostTipPolicy, PostTitle, PrivacyPolicyRevision,
    PublicationBaseUrl, PublicationBaseUrlError, PublicationSettings, PublicationSource,
    PublicationTipSettings, RouteValueError, SiteDescription, SiteSettings, SiteTitle,
    SubscriptionSettings, TipAmount, TipAmountRange, ValidatedContent, XDistributionSettings,
};
pub use parser::validate_content;
pub use path::{LogicalAssetPath, LogicalTreePathError};
pub use provenance::{
    SourceCommit, SourceCommitAlgorithm, SourceCommitDiscovery, SourceCommitParseError,
    SourceCommitUnavailableReason, discover_source_commit,
};
pub use resolver::{
    AllowedOriginOrdinal, AssetReferenceLocation, AssetResolutionCode, AssetResolutionError,
    AssetResolutionErrors, AssetResolutionWarning, AssetResolutionWarningCode,
    AssetResolutionWarnings, ResolveContentAssetsError, ResolvedContentAssets, ResolvedLocalAsset,
    ResolvedLocalAssetLookupError, ResolvedLocalAssetStore, ResolvedPostAssetLookupError,
    ResolvedSiteAssetLookupError, resolve_content_assets,
};
pub use tree::{
    ContentDepthLimit, ContentEntryLimit, ContentFileByteLimit, ContentPathByteLimit,
    ContentTreeByteLimit, ContentTreeLimits, ContentTreeLimitsError, DiscoveredAsset,
    DiscoveredContentTree, DiscoveredPost, DiscoveredPublication, discover_content_tree,
};
pub use validation::{
    ContentValidationCode, ContentValidationError, ContentValidationErrors, FieldPath,
};

pub(crate) use model::{PublicationAssetSettings, UnresolvedAssetReference, UnresolvedHttpsOrigin};
pub(crate) use validation::{DiagnosticCollector, ValidationLocation};

#[cfg(test)]
mod tests;
