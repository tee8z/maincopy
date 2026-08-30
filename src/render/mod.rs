//! Deterministic rendering capabilities consumed by immutable snapshots.

pub use crate::content::render::{
    BaselineMarkdownRenderer, CatalogBuildError, CatalogBuildErrorCode, CodeBlockOrdinal,
    ContentCatalog, GeneratedPostAsset, MarkdownRenderError, MarkdownRenderErrorCode,
    MarkdownRenderLocation, MermaidBlockOrdinal, MermaidPlaceholder, NavigationRejection,
    RenderDestinationKind, RenderedPost, compile_content_catalog,
};
