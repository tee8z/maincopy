//! Deterministic rendering capabilities consumed by immutable snapshots.

mod asset_path;
mod catalog;
mod code;
mod diagram;
mod markdown;
mod metadata;
mod robots;
mod rss;
mod site;
mod sitemap;
mod svg;

pub(crate) use asset_path::SnapshotAssetPath;
pub(crate) use catalog::ContentCompiler;
pub use catalog::{
    CatalogBuildError, CatalogBuildErrorCode, ContentCatalog, compile_content_catalog,
};
pub(crate) use catalog::{CatalogRetentionError, PreviewAsset};
pub(crate) use markdown::GeneratedPostAsset;
pub use markdown::{
    CodeBlockOrdinal, MarkdownRenderError, MarkdownRenderErrorCode, MarkdownRenderLocation,
    NavigationRejection, RenderDestinationKind, RenderedPost, render_markdown,
};
pub use site::{
    RenderedSiteShell, SiteSnapshot, SiteSnapshotBuildError, SiteSnapshotBuildErrorCode,
    SiteSnapshotReader, build_site_snapshot, render_site_shell,
};

pub(crate) use site::{
    SiteSnapshotActivator, SnapshotPublicAsset, render_bound_post_preview,
    render_bound_post_revision_preview, snapshot_store,
};
