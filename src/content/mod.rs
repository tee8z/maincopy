//! Git-owned content types and compilation.

mod assets;
mod identity;
mod model;
mod parser;
mod path;
mod provenance;
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
    AssetBindingTarget, AssetDigest, DigestKind, DigestParseError, FrontendBundleDigest,
    PostContentDigest, PostRendererIdentity, PostRendererVersion, PostRevisionDigest,
    PostRevisionInput, PreInjectionRenderedArticle, PreInjectionSiteShell, PublishedPostRevision,
    RevisionIdentityError, SanitizerVersion, SiteShellRendererIdentity, SiteShellRendererVersion,
    SiteSnapshotDigest, SiteSnapshotInput, digest_asset, digest_frontend_bundle,
    digest_post_content, digest_post_revision, digest_site_snapshot,
};

pub use model::{
    AuthorName, AuthorSettings, CodeRenderingMode, DefaultPostTipPolicy, DistributionCopy,
    DistributionMode, DistributionSettings, DraftStatus, LogicalContentPath, MarkdownDialect,
    MarkdownSource, MermaidRenderingMode, PlainTextError, PostAlias, PostCollection,
    PostDescription, PostDocument, PostId, PostIdParseError, PostMetadata, PostSlug, PostSource,
    PostTag, PostTipPolicy, PostTitle, PrivacyPolicyRevision, PublicationAssetSettings,
    PublicationBaseUrl, PublicationBaseUrlError, PublicationSettings, PublicationSource,
    PublicationTipSettings, RawHtmlPolicy, RendererSettings, RouteValueError, SiteDescription,
    SiteSettings, SiteTitle, SubscriptionSettings, TipAmount, TipAmountRange, ValidatedContent,
    XDistributionSettings,
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
    ResolvedPostAssetSet, resolve_content_assets,
};
pub use tree::{
    ContentByteCount, ContentDepthLimit, ContentEntryLimit, ContentFileByteLimit,
    ContentPathByteLimit, ContentTreeByteLimit, ContentTreeLimits, ContentTreeLimitsError,
    DiscoveredAsset, DiscoveredContentTree, DiscoveredPost, DiscoveredPublication,
    discover_content_tree,
};
pub use validation::{
    ContentValidationCode, ContentValidationError, ContentValidationErrors, FieldPath,
    ValidationLocation,
};

pub(crate) use model::{PostMetadataParts, UnresolvedAssetReference, UnresolvedHttpsOrigin};
pub(crate) use validation::DiagnosticCollector;

#[cfg(test)]
mod tests;
