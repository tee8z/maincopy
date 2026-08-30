mod catalog;
mod markdown;
mod site;

pub use catalog::{
    CatalogBuildError, CatalogBuildErrorCode, ContentCatalog, compile_content_catalog,
};
pub use markdown::{
    BaselineMarkdownRenderer, CodeBlockOrdinal, GeneratedPostAsset, MarkdownRenderError,
    MarkdownRenderErrorCode, MarkdownRenderLocation, MermaidBlockOrdinal, MermaidPlaceholder,
    NavigationRejection, RenderDestinationKind, RenderedPost,
};
pub use site::{
    CanonicalSiteUrl, PublicLedgerProjection, PublicLedgerProjectionError,
    PublicLedgerProjectionErrorCode, RenderedSiteShell, SiteSnapshot, SiteSnapshotBuildError,
    SiteSnapshotBuildErrorCode, SiteSnapshotBuilder, SiteSnapshotReader, SnapshotActivationError,
    SnapshotActivationErrorCode, SnapshotActivationOutcome, SnapshotPublicAsset, render_site_shell,
};

#[allow(
    unused_imports,
    reason = "WP 1.5 gives the transition coordinator sole ownership of activation"
)]
pub(crate) use site::{SiteSnapshotActivator, snapshot_store};
