//! Deterministic rendering capabilities consumed by immutable snapshots.

mod asset_path;
mod catalog;
mod markdown;
mod rss;
mod site;

pub use asset_path::{SnapshotAssetPath, SnapshotAssetPathError};
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
    RenderedSiteShell, SiteSnapshot, SiteSnapshotBuildError, SiteSnapshotBuildErrorCode,
    SiteSnapshotReader, SnapshotPublicAsset, build_site_snapshot, render_site_shell,
};

pub(crate) use site::{
    SiteSnapshotActivator, render_bound_post_preview, render_bound_post_revision_preview,
    snapshot_store,
};
