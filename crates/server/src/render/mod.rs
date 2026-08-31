//! Deterministic rendering capabilities consumed by immutable snapshots.

mod catalog;
mod markdown;
mod site;

pub(crate) use catalog::CatalogRetentionError;
pub use catalog::{
    CatalogBuildError, CatalogBuildErrorCode, ContentCatalog, compile_content_catalog,
};
pub(crate) use markdown::GeneratedPostAsset;
pub use markdown::{
    CodeBlockOrdinal, MarkdownRenderError, MarkdownRenderErrorCode, MarkdownRenderLocation,
    MermaidBlockOrdinal, MermaidPlaceholder, NavigationRejection, RenderDestinationKind,
    RenderedPost, render_markdown,
};
pub use site::{
    CanonicalSiteUrl, RenderedSiteShell, SiteSnapshot, SiteSnapshotBuildError,
    SiteSnapshotBuildErrorCode, SiteSnapshotReader, SnapshotPublicAsset, build_site_snapshot,
    render_site_shell,
};

// Temporary compatibility export while callers move to the owning domain.
pub use crate::domain::publication::PublicLedgerProjection;

pub(crate) use site::{
    SiteSnapshotActivator, render_bound_post_preview, render_bound_post_revision_preview,
    snapshot_store,
};
