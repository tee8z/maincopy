//! Mermaid rendering and SVG trust-boundary composition.

use std::num::NonZeroUsize;

use maincopy_diagram_renderer::client::{
    MermaidRenderError, MermaidRenderErrorCode, MermaidRenderer,
};
use markdown_compiler::PostId;
use thiserror::Error;

use super::svg::{MermaidSvgSanitizer, SanitizedSvg, SvgSanitizeError, SvgScope};

/// The packaged Mermaid renderer followed by Maincopy's strict SVG sanitizer.
#[derive(Debug)]
pub(super) struct MermaidDiagramRenderer {
    renderer: MermaidRenderer,
}

impl MermaidDiagramRenderer {
    pub(super) fn discover() -> Result<Self, DiagramRenderError> {
        MermaidRenderer::discover()
            .map(|renderer| Self { renderer })
            .map_err(DiagramRenderError::Renderer)
    }

    pub(super) fn render(
        &self,
        source: &str,
        post_id: &PostId,
        block_ordinal: NonZeroUsize,
    ) -> Result<SanitizedSvg, DiagramRenderError> {
        let raw = self
            .renderer
            .render(source)
            .map_err(DiagramRenderError::Renderer)?;
        MermaidSvgSanitizer::sanitize(
            raw.as_str(),
            SvgScope::new(post_id.as_uuid().as_bytes(), block_ordinal),
        )
        .map_err(DiagramRenderError::Sanitizer)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DiagramRenderErrorCode {
    Unavailable,
    InvalidDiagram,
    ResourceLimit,
    TimedOut,
    InvalidOutput,
    UnsafeSvg,
    Internal,
}

#[derive(Debug, Error)]
pub(crate) enum DiagramRenderError {
    #[error("Mermaid rendering failed")]
    Renderer(#[source] MermaidRenderError),
    #[error("Mermaid renderer output failed the SVG security policy")]
    Sanitizer(#[source] SvgSanitizeError),
}

impl DiagramRenderError {
    pub(super) const fn code(&self) -> DiagramRenderErrorCode {
        match self {
            Self::Renderer(error) => match error.code() {
                MermaidRenderErrorCode::Unavailable => DiagramRenderErrorCode::Unavailable,
                MermaidRenderErrorCode::InvalidDiagram => DiagramRenderErrorCode::InvalidDiagram,
                MermaidRenderErrorCode::ResourceLimit => DiagramRenderErrorCode::ResourceLimit,
                MermaidRenderErrorCode::TimedOut => DiagramRenderErrorCode::TimedOut,
                MermaidRenderErrorCode::InvalidOutput => DiagramRenderErrorCode::InvalidOutput,
                MermaidRenderErrorCode::Internal => DiagramRenderErrorCode::Internal,
            },
            Self::Sanitizer(_) => DiagramRenderErrorCode::UnsafeSvg,
        }
    }
}
