//! Deterministic rendering capabilities consumed by immutable snapshots.

pub use crate::content::render::{
    BaselineMarkdownRenderer, CanonicalSiteUrl, CatalogBuildError, CatalogBuildErrorCode,
    CodeBlockOrdinal, ContentCatalog, GeneratedPostAsset, MarkdownRenderError,
    MarkdownRenderErrorCode, MarkdownRenderLocation, MermaidBlockOrdinal, MermaidPlaceholder,
    NavigationRejection, PublicLedgerProjection, PublicLedgerProjectionError,
    PublicLedgerProjectionErrorCode, RenderDestinationKind, RenderedPost, RenderedSiteShell,
    SiteSnapshot, SiteSnapshotBuildError, SiteSnapshotBuildErrorCode, SiteSnapshotBuilder,
    SiteSnapshotReader, SnapshotActivationError, SnapshotActivationErrorCode,
    SnapshotActivationOutcome, SnapshotPublicAsset, compile_content_catalog, render_site_shell,
};
