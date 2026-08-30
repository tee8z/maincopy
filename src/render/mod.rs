//! Deterministic rendering capabilities consumed by immutable snapshots.

pub use crate::content::render::{
    CanonicalSiteUrl, CatalogBuildError, CatalogBuildErrorCode, CodeBlockOrdinal, ContentCatalog,
    MarkdownRenderError, MarkdownRenderErrorCode, MarkdownRenderLocation, MermaidBlockOrdinal,
    MermaidPlaceholder, NavigationRejection, PublicLedgerProjection, RenderDestinationKind,
    RenderedPost, RenderedSiteShell, SiteSnapshot, SiteSnapshotBuildError,
    SiteSnapshotBuildErrorCode, SiteSnapshotReader, SnapshotPublicAsset, build_site_snapshot,
    compile_content_catalog, render_markdown, render_site_shell,
};
