//! Git-owned content types and compilation.

pub use crate::domain::content::{
    AuthorName, AuthorSettings, DefaultPostTipPolicy, DistributionCopy, DistributionMode,
    DistributionSettings, DraftStatus, MarkdownSource, PlainTextError, PostAlias, PostDescription,
    PostDocument, PostId, PostIdParseError, PostMetadata, PostSlug, PostTag, PostTipPolicy,
    PostTitle, PrivacyPolicyRevision, PublicationBaseUrl, PublicationBaseUrlError,
    PublicationSettings, PublicationTipSettings, RouteValueError, SiteDescription, SiteSettings,
    SiteTitle, SubscriptionSettings, TipAmount, TipAmountRange, ValidatedContent,
    XDistributionSettings,
};
pub(crate) use crate::domain::content::{
    PublicationAssetSettings, UnresolvedAssetReference, UnresolvedHttpsOrigin,
};

mod assets;
#[allow(dead_code)]
mod candidate_store;
pub(crate) mod identity;
mod model;
mod parser;
mod path;
mod provenance;
mod resolver;
pub(crate) mod tree;
mod tree_digest;
mod validation;

pub use assets::{
    AssetRevisionReference, AuthoredMarkdownDestination, DigestedAsset, ExternalAssetOrigin,
    ExternalAssetOriginError, ExternalAssetUrl, ExternalAssetUrlError, MarkdownDestinationKind,
    MarkdownDestinationOrdinal, MarkdownSourceRange, ResolvedMarkdownDestination,
    ResolvedPostAssets, ResolvedSiteAssets, SnapshotAssetPath, SnapshotAssetPathError,
};
#[allow(unused_imports)]
pub(crate) use candidate_store::{
    ContentCandidateStore, ContentCandidateStoreError, RetainedContentCandidate,
};
pub use identity::{
    AssetBindingTarget, AssetDigest, DigestKind, DigestParseError, PostRevisionDigest,
    PreviewDigest, RevisionIdentityError, SiteSnapshotDigest, digest_asset,
};
pub(crate) use identity::{
    PostRendererIdentity, PublishedPostIdentityInput, SiteShellRendererIdentity,
};

// Temporary compatibility export while callers move to the publication domain.
pub use crate::domain::publication::PublishedPostRevision;

pub use model::{LogicalContentPath, PostCollection, PostSource, PublicationSource};
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
pub use tree_digest::{ContentTreeDigest, ContentTreeDigestParseError};
pub use validation::{
    ContentValidationCode, ContentValidationError, ContentValidationErrors, FieldPath,
};

pub(crate) use validation::{DiagnosticCollector, ValidationLocation};

#[cfg(test)]
mod tests;
