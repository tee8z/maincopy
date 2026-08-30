//! Deterministic rendering capabilities consumed by immutable snapshots.

mod catalog;
mod markdown;
mod site;

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
    CanonicalSiteUrl, PublicLedgerProjection, RenderedSiteShell, SiteSnapshot,
    SiteSnapshotBuildError, SiteSnapshotBuildErrorCode, SiteSnapshotReader, SnapshotPublicAsset,
    build_site_snapshot, render_site_shell,
};

pub(crate) use site::{SiteSnapshotActivator, snapshot_store};
