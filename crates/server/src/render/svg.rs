//! Strict validation and canonicalization for inline Mermaid SVG output.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
    num::NonZeroUsize,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use quick_xml::{
    Reader, XmlVersion,
    events::{BytesEnd, BytesStart, BytesText, Event, attributes::AttrError},
    writer::Writer,
};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use url::Url;

const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_ELEMENTS: usize = 20_000;
const MAX_ATTRIBUTES: usize = 200_000;
const MAX_ATTRIBUTES_PER_ELEMENT: usize = 32;
const MAX_DEPTH: usize = 64;
const MAX_IDS: usize = 20_000;
const MAX_ID_BYTES: usize = 256;
const MAX_REFERENCES: usize = 100_000;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_TEXT_NODE_BYTES: usize = 256 * 1024;
const MAX_PATH_BYTES: usize = 256 * 1024;
const MAX_EMBEDDED_IMAGE_BYTES: usize = 16 * 1024;
const MAX_NAVIGATION_URL_BYTES: usize = 2 * 1024;
const MAX_COORDINATE: f64 = 10_000_000.0;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const C4_PERSON_ICON_SHA256: [u8; 32] = [
    0xa4, 0x78, 0x95, 0x2c, 0x7c, 0x69, 0x3e, 0xf3, 0xcd, 0xef, 0x58, 0xbc, 0xd2, 0xdf, 0xac, 0x56,
    0x64, 0x70, 0x1a, 0x89, 0x83, 0xaa, 0x9c, 0x93, 0x53, 0x25, 0x79, 0x0e, 0xc0, 0x00, 0x15, 0x9c,
];
const C4_EXTERNAL_PERSON_ICON_SHA256: [u8; 32] = [
    0x39, 0x0c, 0x3f, 0x36, 0xe5, 0x90, 0x95, 0xc2, 0x7c, 0x36, 0x49, 0x34, 0x13, 0x64, 0x58, 0x3d,
    0x33, 0xdd, 0xa4, 0xb0, 0xe2, 0xc6, 0x94, 0x7b, 0xd8, 0x17, 0xf2, 0xa2, 0x52, 0xa1, 0x69, 0x3a,
];
const MIX_BLEND_MULTIPLY_CLASS: &str = "mc-mermaid-mix-blend-multiply";

/// SVG text that has crossed the renderer-specific allowlist boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SanitizedSvg(Box<str>);

impl SanitizedSvg {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Concrete sanitizer for output produced by the selected Mermaid renderer.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MermaidSvgSanitizer;

/// A deterministic DOM-ID namespace for one diagram within one post.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SvgScope {
    post: [u8; 8],
    block_ordinal: NonZeroUsize,
}

impl SvgScope {
    pub(crate) fn new(post_scope: &[u8], block_ordinal: NonZeroUsize) -> Self {
        let digest = blake3::hash(post_scope);
        let mut post = [0_u8; 8];
        post.copy_from_slice(&digest.as_bytes()[..8]);
        Self {
            post,
            block_ordinal,
        }
    }

    fn id_prefix(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut prefix = String::with_capacity(42);
        prefix.push_str("mc-mermaid-");
        for byte in self.post {
            prefix.push(char::from(HEX[usize::from(byte >> 4)]));
            prefix.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        prefix.push_str("-b");
        prefix.push_str(&self.block_ordinal.get().to_string());
        prefix.push_str("-i");
        prefix
    }
}

impl MermaidSvgSanitizer {
    /// Validates, namespaces references, and emits one deterministic SVG document.
    pub(crate) fn sanitize(input: &str, scope: SvgScope) -> Result<SanitizedSvg, SvgSanitizeError> {
        Self::sanitize_with_limits(input, scope, SvgLimits::production())
    }

    fn sanitize_with_limits(
        input: &str,
        scope: SvgScope,
        limits: SvgLimits,
    ) -> Result<SanitizedSvg, SvgSanitizeError> {
        if input.len() > limits.input_bytes {
            return Err(SvgSanitizeError::InputTooLarge {
                limit: limits.input_bytes,
                actual: input.len(),
            });
        }
        let parsed = ParsedSvg::parse(input, limits)?;
        parsed.emit(scope, limits.output_bytes)
    }
}

#[derive(Clone, Copy, Debug)]
struct SvgLimits {
    input_bytes: usize,
    output_bytes: usize,
    elements: usize,
    attributes: usize,
    attributes_per_element: usize,
    depth: usize,
    ids: usize,
    references: usize,
    text_bytes: usize,
    text_node_bytes: usize,
}

impl SvgLimits {
    const fn production() -> Self {
        Self {
            input_bytes: MAX_INPUT_BYTES,
            output_bytes: MAX_OUTPUT_BYTES,
            elements: MAX_ELEMENTS,
            attributes: MAX_ATTRIBUTES,
            attributes_per_element: MAX_ATTRIBUTES_PER_ELEMENT,
            depth: MAX_DEPTH,
            ids: MAX_IDS,
            references: MAX_REFERENCES,
            text_bytes: MAX_TEXT_BYTES,
            text_node_bytes: MAX_TEXT_NODE_BYTES,
        }
    }
}

/// Stable classes for sanitizer failures at the renderer trust boundary.
#[derive(Debug, Error)]
pub(crate) enum SvgSanitizeError {
    #[error("renderer SVG is {actual} bytes; the inclusive limit is {limit}")]
    InputTooLarge { limit: usize, actual: usize },
    #[error("renderer SVG is malformed near byte {position}")]
    MalformedXml {
        position: u64,
        #[source]
        source: quick_xml::Error,
    },
    #[error("renderer SVG has malformed attributes near byte {position}")]
    MalformedAttribute {
        position: u64,
        #[source]
        source: AttrError,
    },
    #[error("renderer SVG contains forbidden XML construct {kind}")]
    ForbiddenDocumentConstruct { kind: &'static str },
    #[error("renderer SVG must contain exactly one root svg element")]
    InvalidRoot,
    #[error("renderer SVG nesting exceeds the inclusive limit of {limit}")]
    DepthExceeded { limit: usize },
    #[error("renderer SVG element count exceeds the inclusive limit of {limit}")]
    ElementLimitExceeded { limit: usize },
    #[error("renderer SVG attribute count exceeds the inclusive limit of {limit}")]
    AttributeLimitExceeded { limit: usize },
    #[error("renderer SVG text exceeds the configured limit")]
    TextLimitExceeded,
    #[error("sanitized SVG exceeds the inclusive output limit of {limit} bytes")]
    OutputTooLarge { limit: usize },
    #[error("sanitized SVG could not be represented as UTF-8")]
    InvalidOutputEncoding,
    #[error("renderer SVG element {element} is not allowed")]
    DisallowedElement { element: Box<str> },
    #[error("attribute {attribute} is not allowed on SVG element {element}")]
    DisallowedAttribute {
        element: &'static str,
        attribute: Box<str>,
    },
    #[error("invalid {attribute} value on SVG element {element}")]
    InvalidAttributeValue {
        element: &'static str,
        attribute: &'static str,
    },
    #[error("renderer SVG contains duplicate id {id}")]
    DuplicateId { id: Box<str> },
    #[error("renderer SVG contains too many ids; the inclusive limit is {limit}")]
    IdLimitExceeded { limit: usize },
    #[error("renderer SVG contains too many references; the inclusive limit is {limit}")]
    ReferenceLimitExceeded { limit: usize },
    #[error("renderer SVG references missing id {id}")]
    MissingReference { id: Box<str> },
    #[error("renderer SVG reference {attribute} targets the wrong element kind")]
    WrongReferenceTarget { attribute: &'static str },
    #[error("renderer SVG contains a remote or executable URL in {attribute}")]
    DisallowedUrl { attribute: &'static str },
    #[error("renderer SVG contains an invalid embedded PNG")]
    InvalidEmbeddedImage,
    #[error("renderer SVG contains text outside a text-bearing element")]
    InvalidTextPlacement,
    #[error("renderer SVG contains unsupported entity reference {entity}")]
    UnsupportedEntity { entity: Box<str> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SvgElement {
    Svg,
    Defs,
    G,
    A,
    Marker,
    LinearGradient,
    Stop,
    Symbol,
    Path,
    Rect,
    Circle,
    Ellipse,
    Line,
    Polygon,
    Polyline,
    Text,
    Tspan,
    Title,
    Image,
}

impl SvgElement {
    fn parse(name: &str) -> Result<Self, SvgSanitizeError> {
        match name {
            "svg" => Ok(Self::Svg),
            "defs" => Ok(Self::Defs),
            "g" => Ok(Self::G),
            "a" => Ok(Self::A),
            "marker" => Ok(Self::Marker),
            "linearGradient" => Ok(Self::LinearGradient),
            "stop" => Ok(Self::Stop),
            "symbol" => Ok(Self::Symbol),
            "path" => Ok(Self::Path),
            "rect" => Ok(Self::Rect),
            "circle" => Ok(Self::Circle),
            "ellipse" => Ok(Self::Ellipse),
            "line" => Ok(Self::Line),
            "polygon" => Ok(Self::Polygon),
            "polyline" => Ok(Self::Polyline),
            "text" => Ok(Self::Text),
            "tspan" => Ok(Self::Tspan),
            "title" => Ok(Self::Title),
            "image" => Ok(Self::Image),
            _ => Err(SvgSanitizeError::DisallowedElement {
                element: name.into(),
            }),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Svg => "svg",
            Self::Defs => "defs",
            Self::G => "g",
            Self::A => "a",
            Self::Marker => "marker",
            Self::LinearGradient => "linearGradient",
            Self::Stop => "stop",
            Self::Symbol => "symbol",
            Self::Path => "path",
            Self::Rect => "rect",
            Self::Circle => "circle",
            Self::Ellipse => "ellipse",
            Self::Line => "line",
            Self::Polygon => "polygon",
            Self::Polyline => "polyline",
            Self::Text => "text",
            Self::Tspan => "tspan",
            Self::Title => "title",
            Self::Image => "image",
        }
    }

    const fn accepts_text(self) -> bool {
        matches!(self, Self::Text | Self::Tspan | Self::Title)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SvgAttribute {
    AlignmentBaseline,
    Class,
    ClipRule,
    Cx,
    Cy,
    D,
    DataEdgeId,
    DataLabelKind,
    DominantBaseline,
    Dy,
    Fill,
    FillOpacity,
    FillRule,
    FontFamily,
    FontSize,
    FontStyle,
    FontWeight,
    GradientUnits,
    Height,
    Href,
    Id,
    LengthAdjust,
    MarkerEnd,
    MarkerHeight,
    MarkerStart,
    MarkerUnits,
    MarkerWidth,
    Offset,
    Opacity,
    Orient,
    Points,
    R,
    RefX,
    RefY,
    Rel,
    Rx,
    Ry,
    StopColor,
    Stroke,
    StrokeDasharray,
    StrokeLinecap,
    StrokeLinejoin,
    StrokeOpacity,
    StrokeWidth,
    Style,
    Target,
    TextAnchor,
    TextLength,
    Transform,
    ViewBox,
    Width,
    X,
    X1,
    X2,
    XlinkHref,
    Xmlns,
    XmlnsXlink,
    Y,
    Y1,
    Y2,
}

impl SvgAttribute {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "alignment-baseline" => Self::AlignmentBaseline,
            "class" => Self::Class,
            "clip-rule" => Self::ClipRule,
            "cx" => Self::Cx,
            "cy" => Self::Cy,
            "d" => Self::D,
            "data-edge-id" => Self::DataEdgeId,
            "data-label-kind" => Self::DataLabelKind,
            "dominant-baseline" => Self::DominantBaseline,
            "dy" => Self::Dy,
            "fill" => Self::Fill,
            "fill-opacity" => Self::FillOpacity,
            "fill-rule" => Self::FillRule,
            "font-family" => Self::FontFamily,
            "font-size" => Self::FontSize,
            "font-style" => Self::FontStyle,
            "font-weight" => Self::FontWeight,
            "gradientUnits" => Self::GradientUnits,
            "height" => Self::Height,
            "href" => Self::Href,
            "id" => Self::Id,
            "lengthAdjust" => Self::LengthAdjust,
            "marker-end" => Self::MarkerEnd,
            "markerHeight" => Self::MarkerHeight,
            "marker-start" => Self::MarkerStart,
            "markerUnits" => Self::MarkerUnits,
            "markerWidth" => Self::MarkerWidth,
            "offset" => Self::Offset,
            "opacity" => Self::Opacity,
            "orient" => Self::Orient,
            "points" => Self::Points,
            "r" => Self::R,
            "refX" => Self::RefX,
            "refY" => Self::RefY,
            "rel" => Self::Rel,
            "rx" => Self::Rx,
            "ry" => Self::Ry,
            "stop-color" => Self::StopColor,
            "stroke" => Self::Stroke,
            "stroke-dasharray" => Self::StrokeDasharray,
            "stroke-linecap" => Self::StrokeLinecap,
            "stroke-linejoin" => Self::StrokeLinejoin,
            "stroke-opacity" => Self::StrokeOpacity,
            "stroke-width" => Self::StrokeWidth,
            "style" => Self::Style,
            "target" => Self::Target,
            "text-anchor" => Self::TextAnchor,
            "textLength" => Self::TextLength,
            "transform" => Self::Transform,
            "viewBox" => Self::ViewBox,
            "width" => Self::Width,
            "x" => Self::X,
            "x1" => Self::X1,
            "x2" => Self::X2,
            "xlink:href" => Self::XlinkHref,
            "xmlns" => Self::Xmlns,
            "xmlns:xlink" => Self::XmlnsXlink,
            "y" => Self::Y,
            "y1" => Self::Y1,
            "y2" => Self::Y2,
            _ => return None,
        })
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::AlignmentBaseline => "alignment-baseline",
            Self::Class => "class",
            Self::ClipRule => "clip-rule",
            Self::Cx => "cx",
            Self::Cy => "cy",
            Self::D => "d",
            Self::DataEdgeId => "data-edge-id",
            Self::DataLabelKind => "data-label-kind",
            Self::DominantBaseline => "dominant-baseline",
            Self::Dy => "dy",
            Self::Fill => "fill",
            Self::FillOpacity => "fill-opacity",
            Self::FillRule => "fill-rule",
            Self::FontFamily => "font-family",
            Self::FontSize => "font-size",
            Self::FontStyle => "font-style",
            Self::FontWeight => "font-weight",
            Self::GradientUnits => "gradientUnits",
            Self::Height => "height",
            Self::Href => "href",
            Self::Id => "id",
            Self::LengthAdjust => "lengthAdjust",
            Self::MarkerEnd => "marker-end",
            Self::MarkerHeight => "markerHeight",
            Self::MarkerStart => "marker-start",
            Self::MarkerUnits => "markerUnits",
            Self::MarkerWidth => "markerWidth",
            Self::Offset => "offset",
            Self::Opacity => "opacity",
            Self::Orient => "orient",
            Self::Points => "points",
            Self::R => "r",
            Self::RefX => "refX",
            Self::RefY => "refY",
            Self::Rel => "rel",
            Self::Rx => "rx",
            Self::Ry => "ry",
            Self::StopColor => "stop-color",
            Self::Stroke => "stroke",
            Self::StrokeDasharray => "stroke-dasharray",
            Self::StrokeLinecap => "stroke-linecap",
            Self::StrokeLinejoin => "stroke-linejoin",
            Self::StrokeOpacity => "stroke-opacity",
            Self::StrokeWidth => "stroke-width",
            Self::Style => "style",
            Self::Target => "target",
            Self::TextAnchor => "text-anchor",
            Self::TextLength => "textLength",
            Self::Transform => "transform",
            Self::ViewBox => "viewBox",
            Self::Width => "width",
            Self::X => "x",
            Self::X1 => "x1",
            Self::X2 => "x2",
            Self::XlinkHref => "xlink:href",
            Self::Xmlns => "xmlns",
            Self::XmlnsXlink => "xmlns:xlink",
            Self::Y => "y",
            Self::Y1 => "y1",
            Self::Y2 => "y2",
        }
    }
}

#[derive(Debug)]
enum ParsedEvent {
    Start(ParsedElement),
    Empty(ParsedElement),
    End(SvgElement),
    Text(Box<str>),
}

#[derive(Debug)]
struct ParsedElement {
    kind: SvgElement,
    attributes: BTreeMap<SvgAttribute, Box<str>>,
}

struct ParsedSvg {
    events: Vec<ParsedEvent>,
    ids: BTreeMap<Box<str>, (Box<str>, SvgElement)>,
}

impl ParsedSvg {
    fn parse(input: &str, limits: SvgLimits) -> Result<Self, SvgSanitizeError> {
        let mut parser = SvgParser::new(input, limits);
        parser.parse()?;
        Ok(Self {
            events: parser.events,
            ids: parser.ids,
        })
    }

    fn emit(self, scope: SvgScope, output_limit: usize) -> Result<SanitizedSvg, SvgSanitizeError> {
        let replacements = self.replacements(scope);
        let mut writer = Writer::new(BoundedOutput::new(output_limit));
        for event in self.events {
            write_event(&mut writer, event, &self.ids, &replacements)?;
        }
        let bytes = writer.into_inner().bytes;
        let output =
            String::from_utf8(bytes).map_err(|_| SvgSanitizeError::InvalidOutputEncoding)?;
        Ok(SanitizedSvg(output.into_boxed_str()))
    }

    fn replacements(&self, scope: SvgScope) -> BTreeMap<Box<str>, Box<str>> {
        let prefix = scope.id_prefix();
        self.ids
            .keys()
            .enumerate()
            .map(|(index, original)| {
                (
                    original.clone(),
                    format!("{prefix}{}", index.saturating_add(1)).into_boxed_str(),
                )
            })
            .collect()
    }
}

struct SvgParser<'input> {
    reader: Reader<&'input [u8]>,
    limits: SvgLimits,
    events: Vec<ParsedEvent>,
    stack: Vec<SvgElement>,
    ids: BTreeMap<Box<str>, (Box<str>, SvgElement)>,
    element_count: usize,
    attribute_count: usize,
    text_bytes: usize,
    root_closed: bool,
    xlink_declared: bool,
    xlink_used: bool,
}

impl<'input> SvgParser<'input> {
    fn new(input: &'input str, limits: SvgLimits) -> Self {
        let mut reader = Reader::from_str(input);
        reader.config_mut().enable_all_checks(true);
        Self {
            reader,
            limits,
            events: Vec::new(),
            stack: Vec::new(),
            ids: BTreeMap::new(),
            element_count: 0,
            attribute_count: 0,
            text_bytes: 0,
            root_closed: false,
            xlink_declared: false,
            xlink_used: false,
        }
    }

    fn parse(&mut self) -> Result<(), SvgSanitizeError> {
        loop {
            let event =
                self.reader
                    .read_event()
                    .map_err(|source| SvgSanitizeError::MalformedXml {
                        position: self.reader.error_position(),
                        source,
                    })?;
            match event {
                Event::Start(start) => self.start(start, false)?,
                Event::Empty(start) => self.start(start, true)?,
                Event::End(end) => self.end(end.name().as_ref())?,
                Event::Text(text) => self.text(text.xml10_content().as_ref())?,
                Event::GeneralRef(reference) => self.reference(reference.as_ref())?,
                Event::Eof => break,
                Event::Decl(_) => return Err(forbidden("XML declaration")),
                Event::DocType(_) => return Err(forbidden("document type")),
                Event::PI(_) => return Err(forbidden("processing instruction")),
                Event::CData(_) => return Err(forbidden("CDATA")),
                Event::Comment(_) => return Err(forbidden("comment")),
            }
        }
        if !self.root_closed || !self.stack.is_empty() {
            return Err(SvgSanitizeError::InvalidRoot);
        }
        if self.xlink_used && !self.xlink_declared {
            return Err(SvgSanitizeError::InvalidRoot);
        }
        validate_references(&self.events, &self.ids, self.limits.references)
    }

    fn start(&mut self, start: BytesStart<'_>, empty: bool) -> Result<(), SvgSanitizeError> {
        if self.root_closed {
            return Err(SvgSanitizeError::InvalidRoot);
        }
        self.element_count = self.element_count.saturating_add(1);
        if self.element_count > self.limits.elements {
            return Err(SvgSanitizeError::ElementLimitExceeded {
                limit: self.limits.elements,
            });
        }
        let raw_name = start.name();
        let kind = SvgElement::parse(raw_name.as_ref())?;
        validate_parent(kind, self.stack.last().copied())?;
        if self.stack.is_empty() && kind != SvgElement::Svg {
            return Err(SvgSanitizeError::InvalidRoot);
        }
        if self.stack.len().saturating_add(1) > self.limits.depth {
            return Err(SvgSanitizeError::DepthExceeded {
                limit: self.limits.depth,
            });
        }
        let attributes = self.attributes(kind, &start)?;
        self.record_id(kind, &attributes)?;
        let parsed = ParsedElement { kind, attributes };
        if empty {
            self.events.push(ParsedEvent::Empty(parsed));
            if kind == SvgElement::Svg {
                self.root_closed = true;
            }
        } else {
            self.stack.push(kind);
            self.events.push(ParsedEvent::Start(parsed));
        }
        Ok(())
    }

    fn attributes(
        &mut self,
        element: SvgElement,
        start: &BytesStart<'_>,
    ) -> Result<BTreeMap<SvgAttribute, Box<str>>, SvgSanitizeError> {
        let mut values = BTreeMap::new();
        for (index, attribute) in start.attributes().enumerate() {
            if index >= self.limits.attributes_per_element {
                return Err(SvgSanitizeError::AttributeLimitExceeded {
                    limit: self.limits.attributes_per_element,
                });
            }
            self.attribute_count = self.attribute_count.saturating_add(1);
            if self.attribute_count > self.limits.attributes {
                return Err(SvgSanitizeError::AttributeLimitExceeded {
                    limit: self.limits.attributes,
                });
            }
            let attribute = attribute.map_err(|source| SvgSanitizeError::MalformedAttribute {
                position: self.reader.error_position(),
                source,
            })?;
            let raw_name = attribute.key.as_ref();
            let Some(kind) = SvgAttribute::parse(raw_name) else {
                return Err(SvgSanitizeError::DisallowedAttribute {
                    element: element.as_str(),
                    attribute: raw_name.into(),
                });
            };
            if !attribute_allowed(element, kind) {
                return Err(SvgSanitizeError::DisallowedAttribute {
                    element: element.as_str(),
                    attribute: raw_name.into(),
                });
            }
            let normalized = attribute
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|source| SvgSanitizeError::MalformedXml {
                    position: self.reader.error_position(),
                    source,
                })?;
            validate_attribute_value(element, kind, normalized.as_ref())?;
            self.xlink_declared |= element == SvgElement::Svg && kind == SvgAttribute::XmlnsXlink;
            self.xlink_used |= kind == SvgAttribute::XlinkHref;
            values.insert(kind, normalized.into_owned().into_boxed_str());
        }
        canonicalize_style(element, &mut values)?;
        if values.len() > self.limits.attributes_per_element {
            return Err(SvgSanitizeError::AttributeLimitExceeded {
                limit: self.limits.attributes_per_element,
            });
        }
        validate_attribute_relationships(element, &values)?;
        Ok(values)
    }

    fn record_id(
        &mut self,
        element: SvgElement,
        attributes: &BTreeMap<SvgAttribute, Box<str>>,
    ) -> Result<(), SvgSanitizeError> {
        let Some(id) = attributes.get(&SvgAttribute::Id) else {
            return Ok(());
        };
        if self.ids.len() >= self.limits.ids {
            return Err(SvgSanitizeError::IdLimitExceeded {
                limit: self.limits.ids,
            });
        }
        if self.ids.insert(id.clone(), (id.clone(), element)).is_some() {
            return Err(SvgSanitizeError::DuplicateId { id: id.clone() });
        }
        Ok(())
    }

    fn end(&mut self, raw_name: &str) -> Result<(), SvgSanitizeError> {
        let kind = SvgElement::parse(raw_name)?;
        if self.stack.pop() != Some(kind) {
            return Err(SvgSanitizeError::InvalidRoot);
        }
        self.events.push(ParsedEvent::End(kind));
        if self.stack.is_empty() {
            self.root_closed = true;
        }
        Ok(())
    }

    fn text(&mut self, value: &str) -> Result<(), SvgSanitizeError> {
        if self
            .stack
            .last()
            .is_some_and(|element| element.accepts_text())
        {
            self.push_text(value)
        } else if value.chars().all(char::is_whitespace) {
            Ok(())
        } else {
            Err(SvgSanitizeError::InvalidTextPlacement)
        }
    }

    fn reference(&mut self, value: &str) -> Result<(), SvgSanitizeError> {
        let decoded = match value {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "apos" => '\'',
            "quot" => '"',
            value if value.starts_with("#x") => decode_character_reference(&value[2..], 16)?,
            value if value.starts_with('#') => decode_character_reference(&value[1..], 10)?,
            _ => {
                return Err(SvgSanitizeError::UnsupportedEntity {
                    entity: value.into(),
                });
            }
        };
        if !self
            .stack
            .last()
            .is_some_and(|element| element.accepts_text())
        {
            return Err(SvgSanitizeError::InvalidTextPlacement);
        }
        self.push_text(&decoded.to_string())
    }

    fn push_text(&mut self, value: &str) -> Result<(), SvgSanitizeError> {
        if value.len() > self.limits.text_node_bytes {
            return Err(SvgSanitizeError::TextLimitExceeded);
        }
        self.text_bytes = self.text_bytes.saturating_add(value.len());
        if self.text_bytes > self.limits.text_bytes {
            return Err(SvgSanitizeError::TextLimitExceeded);
        }
        self.events.push(ParsedEvent::Text(value.into()));
        Ok(())
    }
}

fn forbidden(kind: &'static str) -> SvgSanitizeError {
    SvgSanitizeError::ForbiddenDocumentConstruct { kind }
}

fn decode_character_reference(value: &str, radix: u32) -> Result<char, SvgSanitizeError> {
    u32::from_str_radix(value, radix)
        .ok()
        .and_then(char::from_u32)
        .filter(|value| is_xml_character(*value))
        .ok_or_else(|| SvgSanitizeError::UnsupportedEntity {
            entity: format!("#{value}").into_boxed_str(),
        })
}

const fn is_xml_character(value: char) -> bool {
    matches!(value, '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

fn validate_parent(child: SvgElement, parent: Option<SvgElement>) -> Result<(), SvgSanitizeError> {
    let valid = match (child, parent) {
        (SvgElement::Svg, None) => true,
        (SvgElement::Svg, Some(_)) | (_, None) => false,
        (SvgElement::Marker | SvgElement::Symbol, Some(SvgElement::Defs)) => true,
        (SvgElement::Stop, Some(SvgElement::LinearGradient)) => true,
        (SvgElement::Tspan, Some(SvgElement::Text | SvgElement::Tspan)) => true,
        (SvgElement::Title, Some(SvgElement::A)) => true,
        (SvgElement::Defs, Some(SvgElement::Svg)) => true,
        (SvgElement::LinearGradient, Some(SvgElement::Defs | SvgElement::G)) => true,
        (SvgElement::G | SvgElement::A, Some(SvgElement::Svg | SvgElement::G | SvgElement::A)) => {
            true
        }
        (
            SvgElement::Path
            | SvgElement::Rect
            | SvgElement::Circle
            | SvgElement::Ellipse
            | SvgElement::Line
            | SvgElement::Polygon
            | SvgElement::Polyline
            | SvgElement::Text
            | SvgElement::Image,
            Some(
                SvgElement::Svg
                | SvgElement::G
                | SvgElement::A
                | SvgElement::Marker
                | SvgElement::Symbol,
            ),
        ) => true,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(SvgSanitizeError::InvalidRoot)
    }
}

fn attribute_allowed(element: SvgElement, attribute: SvgAttribute) -> bool {
    use SvgAttribute as A;
    use SvgElement as E;
    match element {
        E::Svg => matches!(
            attribute,
            A::Xmlns | A::XmlnsXlink | A::Width | A::Height | A::ViewBox | A::Style
        ),
        E::Defs | E::Title => false,
        E::G => matches!(
            attribute,
            A::Id
                | A::Class
                | A::DataEdgeId
                | A::DataLabelKind
                | A::Transform
                | A::X
                | A::Y
                | A::Fill
                | A::FontSize
                | A::StrokeOpacity
                | A::Style
        ),
        E::A => matches!(attribute, A::Href | A::XlinkHref | A::Target | A::Rel),
        E::Marker => matches!(
            attribute,
            A::Id
                | A::ViewBox
                | A::RefX
                | A::RefY
                | A::MarkerUnits
                | A::MarkerWidth
                | A::MarkerHeight
                | A::Orient
        ),
        E::LinearGradient => matches!(attribute, A::Id | A::GradientUnits | A::X1 | A::X2),
        E::Stop => matches!(attribute, A::Offset | A::StopColor),
        E::Symbol => matches!(
            attribute,
            A::Id | A::Width | A::Height | A::FillRule | A::ClipRule
        ),
        E::Path => matches!(
            attribute,
            A::Id
                | A::Class
                | A::DataEdgeId
                | A::D
                | A::Fill
                | A::FillOpacity
                | A::Stroke
                | A::StrokeWidth
                | A::StrokeOpacity
                | A::StrokeDasharray
                | A::StrokeLinecap
                | A::StrokeLinejoin
                | A::MarkerStart
                | A::MarkerEnd
                | A::Opacity
                | A::Transform
                | A::Style
        ),
        E::Rect => matches!(
            attribute,
            A::Id
                | A::Class
                | A::DataEdgeId
                | A::DataLabelKind
                | A::X
                | A::Y
                | A::Width
                | A::Height
                | A::Rx
                | A::Ry
                | A::Fill
                | A::FillOpacity
                | A::Stroke
                | A::StrokeWidth
                | A::StrokeOpacity
                | A::StrokeDasharray
                | A::StrokeLinecap
                | A::StrokeLinejoin
                | A::Opacity
        ),
        E::Circle => matches!(
            attribute,
            A::Cx
                | A::Cy
                | A::R
                | A::Fill
                | A::FillOpacity
                | A::Stroke
                | A::StrokeWidth
                | A::StrokeLinecap
                | A::StrokeLinejoin
        ),
        E::Ellipse => matches!(
            attribute,
            A::Cx
                | A::Cy
                | A::Rx
                | A::Ry
                | A::Fill
                | A::Stroke
                | A::StrokeWidth
                | A::StrokeLinecap
                | A::StrokeLinejoin
        ),
        E::Line => matches!(
            attribute,
            A::X1
                | A::Y1
                | A::X2
                | A::Y2
                | A::Fill
                | A::Stroke
                | A::StrokeWidth
                | A::StrokeOpacity
                | A::StrokeDasharray
                | A::StrokeLinecap
                | A::StrokeLinejoin
                | A::MarkerEnd
                | A::Style
        ),
        E::Polygon => matches!(
            attribute,
            A::Points | A::Fill | A::Stroke | A::StrokeWidth | A::StrokeLinecap | A::StrokeLinejoin
        ),
        E::Polyline => matches!(attribute, A::Points | A::Fill | A::Stroke | A::StrokeWidth),
        E::Text => matches!(
            attribute,
            A::X | A::Y
                | A::Dy
                | A::Transform
                | A::Fill
                | A::FontFamily
                | A::FontSize
                | A::FontStyle
                | A::FontWeight
                | A::TextAnchor
                | A::DominantBaseline
                | A::LengthAdjust
                | A::TextLength
                | A::Style
        ),
        E::Tspan => matches!(
            attribute,
            A::X | A::Dy | A::FontWeight | A::AlignmentBaseline
        ),
        E::Image => matches!(attribute, A::X | A::Y | A::Width | A::Height | A::XlinkHref),
    }
}

fn validate_attribute_value(
    element: SvgElement,
    attribute: SvgAttribute,
    value: &str,
) -> Result<(), SvgSanitizeError> {
    use SvgAttribute as A;
    let valid = match attribute {
        A::Xmlns => value == "http://www.w3.org/2000/svg",
        A::XmlnsXlink => value == "http://www.w3.org/1999/xlink",
        A::Id => valid_id(value),
        A::Class | A::DataEdgeId | A::DataLabelKind => valid_tokens(value),
        A::ViewBox => valid_view_box(value),
        A::D => valid_path(value),
        A::Points => valid_number_list(value, false),
        A::StrokeDasharray => valid_number_list(value, true),
        A::Transform => valid_transform(value),
        A::Fill | A::Stroke => valid_paint(value),
        A::StopColor => valid_color(value),
        A::FillOpacity | A::StrokeOpacity | A::Opacity => valid_range(value, 0.0, 1.0),
        A::Width
        | A::Height
        | A::R
        | A::Rx
        | A::Ry
        | A::MarkerWidth
        | A::MarkerHeight
        | A::TextLength => {
            valid_nonnegative_length(value, element == SvgElement::Svg && attribute == A::Width)
        }
        A::X | A::Y | A::X1 | A::X2 | A::Y1 | A::Y2 | A::Cx | A::Cy | A::RefX | A::RefY => {
            valid_number(value)
        }
        A::Dy => valid_length_with_unit(value, "em"),
        A::FontSize => valid_length_with_unit(value, "px"),
        A::Offset => valid_offset(value),
        A::MarkerUnits | A::GradientUnits => value == "userSpaceOnUse",
        A::Orient => matches!(value, "auto" | "auto-start-reverse"),
        A::StrokeLinecap => matches!(value, "butt" | "round" | "square"),
        A::StrokeLinejoin => matches!(value, "miter" | "round" | "bevel"),
        A::FillRule | A::ClipRule => matches!(value, "nonzero" | "evenodd"),
        A::TextAnchor => matches!(value, "start" | "middle" | "end"),
        A::DominantBaseline => value == "middle",
        A::AlignmentBaseline => value == "mathematical",
        A::LengthAdjust => value == "spacing",
        A::FontStyle => matches!(value, "normal" | "italic"),
        A::FontWeight => valid_font_weight(value),
        A::FontFamily => valid_font_family(value),
        A::MarkerStart | A::MarkerEnd => local_reference(value).is_some(),
        A::Href | A::XlinkHref => valid_link_or_image_url(element, value),
        A::Target => matches!(value, "_self" | "_blank"),
        A::Rel => value == "noopener noreferrer",
        A::Style => valid_style(element, value),
        A::StrokeWidth => valid_nonnegative_length_with_unit(value, "px"),
    };
    if valid {
        Ok(())
    } else if matches!(attribute, A::Href | A::XlinkHref) {
        Err(SvgSanitizeError::DisallowedUrl {
            attribute: attribute.as_str(),
        })
    } else {
        Err(SvgSanitizeError::InvalidAttributeValue {
            element: element.as_str(),
            attribute: attribute.as_str(),
        })
    }
}

fn validate_attribute_relationships(
    element: SvgElement,
    attributes: &BTreeMap<SvgAttribute, Box<str>>,
) -> Result<(), SvgSanitizeError> {
    use SvgAttribute as A;
    if element == SvgElement::Svg && !attributes.contains_key(&A::Xmlns) {
        return Err(invalid_value(element, A::Xmlns));
    }
    if element == SvgElement::A {
        let href = attributes.get(&A::Href);
        let xlink = attributes.get(&A::XlinkHref);
        if href.is_none() || href != xlink {
            return Err(invalid_value(element, A::Href));
        }
        match attributes.get(&A::Target).map(Box::as_ref) {
            Some("_blank")
                if attributes.get(&A::Rel).map(Box::as_ref) == Some("noopener noreferrer") => {}
            Some("_blank") => return Err(invalid_value(element, A::Rel)),
            _ if attributes.contains_key(&A::Rel) => return Err(invalid_value(element, A::Rel)),
            _ => {}
        }
    }
    if element == SvgElement::Image {
        let Some(value) = attributes.get(&A::XlinkHref) else {
            return Err(invalid_value(element, A::XlinkHref));
        };
        validate_embedded_png(value)?;
    }
    Ok(())
}

fn invalid_value(element: SvgElement, attribute: SvgAttribute) -> SvgSanitizeError {
    SvgSanitizeError::InvalidAttributeValue {
        element: element.as_str(),
        attribute: attribute.as_str(),
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && !value.chars().any(|character| {
            character.is_control() || character.is_whitespace() || "#()\"'".contains(character)
        })
}

fn valid_tokens(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b' '))
}

fn valid_number(value: &str) -> bool {
    parse_number(value).is_some_and(|number| number.abs() <= MAX_COORDINATE)
}

fn valid_nonnegative_length(value: &str, allow_percent: bool) -> bool {
    if allow_percent && value == "100%" {
        return true;
    }
    parse_number(value).is_some_and(|number| (0.0..=MAX_COORDINATE).contains(&number))
}

fn valid_length_with_unit(value: &str, unit: &str) -> bool {
    valid_number(value) || value.strip_suffix(unit).is_some_and(valid_number)
}

fn valid_nonnegative_length_with_unit(value: &str, unit: &str) -> bool {
    valid_nonnegative_length(value, false)
        || value
            .strip_suffix(unit)
            .is_some_and(|number| valid_nonnegative_length(number, false))
}

fn valid_range(value: &str, minimum: f64, maximum: f64) -> bool {
    parse_number(value).is_some_and(|number| (minimum..=maximum).contains(&number))
}

fn parse_number(value: &str) -> Option<f64> {
    if value.is_empty() || value.trim() != value || scan_number(value.as_bytes(), 0)? != value.len()
    {
        return None;
    }
    value
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

fn scan_number(bytes: &[u8], mut cursor: usize) -> Option<usize> {
    if matches!(bytes.get(cursor), Some(b'+' | b'-')) {
        cursor += 1;
    }
    let integer_start = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    let integer_digits = cursor > integer_start;
    let mut fractional_digits = false;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let fraction_start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        fractional_digits = cursor > fraction_start;
    }
    if !integer_digits && !fractional_digits {
        return None;
    }
    if matches!(bytes.get(cursor), Some(b'e' | b'E')) {
        cursor += 1;
        if matches!(bytes.get(cursor), Some(b'+' | b'-')) {
            cursor += 1;
        }
        let exponent_start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == exponent_start {
            return None;
        }
    }
    Some(cursor)
}

fn valid_view_box(value: &str) -> bool {
    number_list(value).is_some_and(|values| values.len() == 4 && values[2] > 0.0 && values[3] > 0.0)
}

fn valid_number_list(value: &str, nonnegative: bool) -> bool {
    number_list(value).is_some_and(|numbers| {
        numbers.len() >= 2 && (!nonnegative || numbers.iter().all(|number| *number >= 0.0))
    })
}

fn number_list(value: &str) -> Option<Vec<f64>> {
    let values = value
        .split(|character: char| character == ',' || character.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .map(parse_number)
        .collect::<Option<Vec<_>>>()?;
    if values.is_empty() || values.iter().any(|number| number.abs() > MAX_COORDINATE) {
        None
    } else {
        Some(values)
    }
}

fn valid_path(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_PATH_BYTES {
        return false;
    }
    let bytes = value.as_bytes();
    let mut cursor = 0;
    let mut saw_command = false;
    while cursor < bytes.len() {
        match bytes[cursor] {
            command if b"MmZzLlHhVvCcSsQqTtAa".contains(&command) => {
                saw_command = true;
                cursor += 1;
            }
            b',' | b' ' | b'\t' | b'\r' | b'\n' => cursor += 1,
            _ => {
                let Some(end) = scan_number(bytes, cursor) else {
                    return false;
                };
                if !parse_number(&value[cursor..end])
                    .is_some_and(|number| number.abs() <= MAX_COORDINATE)
                {
                    return false;
                }
                cursor = end;
            }
        }
    }
    saw_command
}

fn valid_transform(value: &str) -> bool {
    let mut remaining = value.trim();
    let mut found = false;
    while !remaining.is_empty() {
        let Some(open) = remaining.find('(') else {
            return false;
        };
        let name = remaining[..open].trim();
        if !matches!(name, "translate" | "scale" | "rotate") {
            return false;
        }
        let Some(close) = remaining[open + 1..].find(')') else {
            return false;
        };
        let end = open + 1 + close;
        let Some(arguments) = number_list(&remaining[open + 1..end]) else {
            return false;
        };
        let valid_count = match name {
            "translate" | "scale" => matches!(arguments.len(), 1 | 2),
            "rotate" => matches!(arguments.len(), 1 | 3),
            _ => false,
        };
        if !valid_count {
            return false;
        }
        found = true;
        remaining = remaining[end + 1..].trim_start();
    }
    found
}

fn valid_paint(value: &str) -> bool {
    valid_color(value) || local_reference(value).is_some()
}

fn valid_color(value: &str) -> bool {
    if matches!(
        value,
        "none" | "black" | "white" | "lightgrey" | "transparent"
    ) {
        return true;
    }
    if let Some(hex) = value.strip_prefix('#') {
        return matches!(hex.len(), 3 | 4 | 6 | 8)
            && hex.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    valid_functional_color(value)
}

fn valid_functional_color(value: &str) -> bool {
    for (name, expected) in [("rgb", 3), ("rgba", 4), ("hsl", 3), ("hsla", 4)] {
        let Some(arguments) = value
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('('))
            .and_then(|rest| rest.strip_suffix(')'))
        else {
            continue;
        };
        let parts = arguments.split(',').map(str::trim).collect::<Vec<_>>();
        if parts.len() != expected {
            return false;
        }
        return match name {
            "rgb" | "rgba" => valid_rgb(&parts),
            "hsl" | "hsla" => valid_hsl(&parts),
            _ => false,
        };
    }
    false
}

fn valid_rgb(parts: &[&str]) -> bool {
    parts[..3].iter().all(|part| valid_range(part, 0.0, 255.0))
        && (parts.len() == 3 || valid_range(parts[3], 0.0, 1.0))
}

fn valid_hsl(parts: &[&str]) -> bool {
    valid_range(parts[0], 0.0, 360.0)
        && parts[1..3].iter().all(|part| {
            part.strip_suffix('%')
                .is_some_and(|number| valid_range(number, 0.0, 100.0))
        })
        && (parts.len() == 3 || valid_range(parts[3], 0.0, 1.0))
}

fn valid_offset(value: &str) -> bool {
    value.strip_suffix('%').map_or_else(
        || valid_range(value, 0.0, 1.0),
        |number| valid_range(number, 0.0, 100.0),
    )
}

fn valid_font_weight(value: &str) -> bool {
    matches!(value, "normal" | "bold")
        || value
            .parse::<u16>()
            .is_ok_and(|weight| (100..=900).contains(&weight) && weight % 100 == 0)
}

fn valid_font_family(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.split(',').all(|family| {
            let family = family.trim();
            let family = family
                .strip_prefix('"')
                .and_then(|family| family.strip_suffix('"'))
                .or_else(|| {
                    family
                        .strip_prefix('\'')
                        .and_then(|family| family.strip_suffix('\''))
                })
                .unwrap_or(family);
            !family.is_empty()
                && family
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'-' | b'_'))
        })
}

fn valid_style(element: SvgElement, value: &str) -> bool {
    if value.len() > 256
        || value.contains(['\\', '{', '}', '@'])
        || value.to_ascii_lowercase().contains("url")
        || value.to_ascii_lowercase().contains("!important")
    {
        return false;
    }
    let mut seen = BTreeSet::new();
    value
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .all(|declaration| {
            let Some((name, value)) = declaration.split_once(':') else {
                return false;
            };
            let name = name.trim();
            let value = value.trim();
            seen.insert(name) && valid_style_declaration(element, name, value)
        })
        && !seen.is_empty()
}

fn valid_style_declaration(element: SvgElement, name: &str, value: &str) -> bool {
    match (element, name) {
        (SvgElement::Svg, "max-width") => value
            .strip_suffix("px")
            .is_some_and(|number| valid_nonnegative_length(number, false)),
        (SvgElement::Svg, "aspect-ratio") => parse_number(value).is_some_and(|number| number > 0.0),
        (SvgElement::G, "mix-blend-mode") => value == "multiply",
        (SvgElement::Path | SvgElement::Line, "fill") => value == "none",
        (SvgElement::Path | SvgElement::Line, "stroke-dasharray") => valid_number_list(value, true),
        (SvgElement::Text, "text-anchor") => matches!(value, "start" | "middle" | "end"),
        (SvgElement::Text, "font-size") => valid_length_with_unit(value, "px"),
        (SvgElement::Text, "font-weight") => valid_font_weight(value),
        (SvgElement::Text, "font-family") => valid_font_family(value),
        _ => false,
    }
}

fn canonicalize_style(
    element: SvgElement,
    attributes: &mut BTreeMap<SvgAttribute, Box<str>>,
) -> Result<(), SvgSanitizeError> {
    use SvgAttribute as A;

    let reserved_class_present = attributes
        .get(&A::Class)
        .is_some_and(|class| class.split_ascii_whitespace().any(is_mix_blend_class));
    let Some(style) = attributes.remove(&A::Style) else {
        if reserved_class_present {
            return Err(invalid_value(element, A::Class));
        }
        return Ok(());
    };
    if reserved_class_present && element != SvgElement::G {
        return Err(invalid_value(element, A::Class));
    }

    let mut max_width = None;
    let mut aspect_ratio = None;
    for declaration in style
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let Some((name, value)) = declaration.split_once(':') else {
            return Err(invalid_value(element, A::Style));
        };
        let value = value.trim();
        match (element, name.trim()) {
            (SvgElement::Svg, "max-width") => {
                max_width = Some(
                    value
                        .strip_suffix("px")
                        .ok_or_else(|| invalid_value(element, A::Style))?,
                );
            }
            (SvgElement::Svg, "aspect-ratio") => {
                aspect_ratio =
                    Some(parse_number(value).ok_or_else(|| invalid_value(element, A::Style))?);
            }
            (SvgElement::G, "mix-blend-mode") => add_mix_blend_class(element, attributes)?,
            (SvgElement::Path | SvgElement::Line, "fill") => {
                attributes.insert(A::Fill, value.into());
            }
            (SvgElement::Path | SvgElement::Line, "stroke-dasharray") => {
                attributes.insert(A::StrokeDasharray, value.into());
            }
            (SvgElement::Text, "text-anchor") => {
                attributes.insert(A::TextAnchor, value.into());
            }
            (SvgElement::Text, "font-size") => {
                attributes.insert(A::FontSize, value.into());
            }
            (SvgElement::Text, "font-weight") => {
                attributes.insert(A::FontWeight, value.into());
            }
            (SvgElement::Text, "font-family") => {
                attributes.insert(A::FontFamily, value.into());
            }
            _ => return Err(invalid_value(element, A::Style)),
        }
    }

    if let Some(max_width) = max_width {
        if attributes.get(&A::Width).map(Box::as_ref) != Some("100%")
            || attributes.contains_key(&A::Height)
        {
            return Err(invalid_value(element, A::Style));
        }
        attributes.insert(A::Width, max_width.into());
    }
    if let Some(aspect_ratio) = aspect_ratio {
        canonicalize_aspect_ratio(element, attributes, aspect_ratio)?;
    }
    Ok(())
}

fn is_mix_blend_class(token: &str) -> bool {
    token == MIX_BLEND_MULTIPLY_CLASS
}

fn add_mix_blend_class(
    element: SvgElement,
    attributes: &mut BTreeMap<SvgAttribute, Box<str>>,
) -> Result<(), SvgSanitizeError> {
    use SvgAttribute as A;

    let class = attributes.get(&A::Class).map_or_else(
        || MIX_BLEND_MULTIPLY_CLASS.to_owned(),
        |existing| {
            if existing.split_ascii_whitespace().any(is_mix_blend_class) {
                existing.to_string()
            } else {
                format!("{existing} {MIX_BLEND_MULTIPLY_CLASS}")
            }
        },
    );
    if !valid_tokens(&class) {
        return Err(invalid_value(element, A::Style));
    }
    attributes.insert(A::Class, class.into_boxed_str());
    Ok(())
}

fn canonicalize_aspect_ratio(
    element: SvgElement,
    attributes: &mut BTreeMap<SvgAttribute, Box<str>>,
    target_ratio: f64,
) -> Result<(), SvgSanitizeError> {
    use SvgAttribute as A;

    let Some(view_box) = attributes
        .get(&A::ViewBox)
        .and_then(|value| number_list(value))
    else {
        return Err(invalid_value(element, A::Style));
    };
    let [x, y, width, height] = view_box.as_slice() else {
        return Err(invalid_value(element, A::Style));
    };
    let (mut x, mut y, mut width, mut height) = (*x, *y, *width, *height);
    let current_ratio = width / height;
    if (current_ratio - target_ratio).abs()
        > f64::EPSILON * current_ratio.abs().max(target_ratio.abs()).max(1.0) * 8.0
    {
        if target_ratio > current_ratio {
            let expanded_width = height * target_ratio;
            x -= (expanded_width - width) / 2.0;
            width = expanded_width;
        } else {
            let expanded_height = width / target_ratio;
            y -= (expanded_height - height) / 2.0;
            height = expanded_height;
        }
    }
    let canonical = format!("{x} {y} {width} {height}");
    if !valid_view_box(&canonical) {
        return Err(invalid_value(element, A::Style));
    }
    attributes.insert(A::ViewBox, canonical.into_boxed_str());
    Ok(())
}

fn valid_link_or_image_url(element: SvgElement, value: &str) -> bool {
    match element {
        SvgElement::A => {
            local_reference(value).is_some()
                || valid_root_relative_url(value)
                || valid_https_url(value)
        }
        SvgElement::Image => value.starts_with("data:image/png;base64,"),
        _ => false,
    }
}

fn valid_https_url(value: &str) -> bool {
    if value.len() > MAX_NAVIGATION_URL_BYTES
        || value.trim() != value
        || value.contains('\\')
        || value.chars().any(char::is_control)
    {
        return false;
    }
    Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn valid_root_relative_url(value: &str) -> bool {
    value.len() <= MAX_NAVIGATION_URL_BYTES
        && value.starts_with('/')
        && !value.starts_with("//")
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
        && !value
            .split(['/', '?', '#'])
            .any(|component| matches!(component, "." | ".."))
}

fn validate_embedded_png(value: &str) -> Result<(), SvgSanitizeError> {
    let encoded = value
        .strip_prefix("data:image/png;base64,")
        .ok_or(SvgSanitizeError::InvalidEmbeddedImage)?;
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| SvgSanitizeError::InvalidEmbeddedImage)?;
    if decoded.len() > MAX_EMBEDDED_IMAGE_BYTES || !decoded.starts_with(PNG_SIGNATURE) {
        return Err(SvgSanitizeError::InvalidEmbeddedImage);
    }
    let digest: [u8; 32] = Sha256::digest(&decoded).into();
    if matches!(
        digest,
        C4_PERSON_ICON_SHA256 | C4_EXTERNAL_PERSON_ICON_SHA256
    ) {
        Ok(())
    } else {
        Err(SvgSanitizeError::InvalidEmbeddedImage)
    }
}

fn local_reference(value: &str) -> Option<&str> {
    value
        .strip_prefix("url(#")
        .and_then(|value| value.strip_suffix(')'))
        .or_else(|| value.strip_prefix('#'))
        .filter(|id| valid_id(id))
}

fn validate_references(
    events: &[ParsedEvent],
    ids: &BTreeMap<Box<str>, (Box<str>, SvgElement)>,
    limit: usize,
) -> Result<(), SvgSanitizeError> {
    let mut count = 0_usize;
    for element in events.iter().filter_map(parsed_element) {
        for (attribute, value) in &element.attributes {
            let Some(id) = reference_for(*attribute, value) else {
                continue;
            };
            count = count.saturating_add(1);
            if count > limit {
                return Err(SvgSanitizeError::ReferenceLimitExceeded { limit });
            }
            let Some((_, target)) = ids.get(id) else {
                return Err(SvgSanitizeError::MissingReference { id: id.into() });
            };
            validate_reference_target(*attribute, *target)?;
        }
    }
    Ok(())
}

fn parsed_element(event: &ParsedEvent) -> Option<&ParsedElement> {
    match event {
        ParsedEvent::Start(element) | ParsedEvent::Empty(element) => Some(element),
        ParsedEvent::End(_) | ParsedEvent::Text(_) => None,
    }
}

fn reference_for(attribute: SvgAttribute, value: &str) -> Option<&str> {
    match attribute {
        SvgAttribute::MarkerStart | SvgAttribute::MarkerEnd => local_reference(value),
        SvgAttribute::Fill | SvgAttribute::Stroke if value.starts_with("url(") => {
            local_reference(value)
        }
        SvgAttribute::Href | SvgAttribute::XlinkHref if value.starts_with('#') => {
            local_reference(value)
        }
        _ => None,
    }
}

fn validate_reference_target(
    attribute: SvgAttribute,
    target: SvgElement,
) -> Result<(), SvgSanitizeError> {
    let valid = match attribute {
        SvgAttribute::MarkerStart | SvgAttribute::MarkerEnd => target == SvgElement::Marker,
        SvgAttribute::Fill | SvgAttribute::Stroke => target == SvgElement::LinearGradient,
        SvgAttribute::Href | SvgAttribute::XlinkHref => true,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(SvgSanitizeError::WrongReferenceTarget {
            attribute: attribute.as_str(),
        })
    }
}

fn write_event(
    writer: &mut Writer<BoundedOutput>,
    event: ParsedEvent,
    ids: &BTreeMap<Box<str>, (Box<str>, SvgElement)>,
    replacements: &BTreeMap<Box<str>, Box<str>>,
) -> Result<(), SvgSanitizeError> {
    let result = match event {
        ParsedEvent::Start(element) => {
            writer.write_event(Event::Start(rewritten_start(element, ids, replacements)?))
        }
        ParsedEvent::Empty(element) => {
            writer.write_event(Event::Empty(rewritten_start(element, ids, replacements)?))
        }
        ParsedEvent::End(element) => {
            writer.write_event(Event::End(BytesEnd::new(element.as_str())))
        }
        ParsedEvent::Text(text) => writer.write_event(Event::Text(BytesText::new(&text))),
    };
    result.map_err(|_| SvgSanitizeError::OutputTooLarge {
        limit: writer.get_ref().limit,
    })
}

fn rewritten_start(
    element: ParsedElement,
    ids: &BTreeMap<Box<str>, (Box<str>, SvgElement)>,
    replacements: &BTreeMap<Box<str>, Box<str>>,
) -> Result<BytesStart<'static>, SvgSanitizeError> {
    let mut start = BytesStart::new(element.kind.as_str()).into_owned();
    for (attribute, value) in element.attributes {
        let rewritten = rewrite_value(attribute, &value, ids, replacements)?;
        start.push_attribute((attribute.as_str(), rewritten.as_ref()));
    }
    Ok(start)
}

fn rewrite_value(
    attribute: SvgAttribute,
    value: &str,
    ids: &BTreeMap<Box<str>, (Box<str>, SvgElement)>,
    replacements: &BTreeMap<Box<str>, Box<str>>,
) -> Result<Box<str>, SvgSanitizeError> {
    if attribute == SvgAttribute::Id {
        return replacements
            .get(value)
            .cloned()
            .ok_or_else(|| SvgSanitizeError::MissingReference { id: value.into() });
    }
    let Some(original) = reference_for(attribute, value) else {
        return Ok(value.into());
    };
    let Some((_, target)) = ids.get(original) else {
        return Err(SvgSanitizeError::MissingReference {
            id: original.into(),
        });
    };
    validate_reference_target(attribute, *target)?;
    let replacement =
        replacements
            .get(original)
            .ok_or_else(|| SvgSanitizeError::MissingReference {
                id: original.into(),
            })?;
    if matches!(attribute, SvgAttribute::Href | SvgAttribute::XlinkHref) {
        Ok(format!("#{replacement}").into_boxed_str())
    } else {
        Ok(format!("url(#{replacement})").into_boxed_str())
    }
}

struct BoundedOutput {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedOutput {
    const fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
}

impl Write for BoundedOutput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(buffer.len())
            .is_none_or(|next| next > self.limit)
        {
            return Err(io::Error::other("sanitized SVG output limit exceeded"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_rs_renderer::{RenderOptions, render_strict};

    fn sanitize(input: &str) -> Result<SanitizedSvg, SvgSanitizeError> {
        MermaidSvgSanitizer::sanitize(
            input,
            SvgScope::new(b"post-fixture", NonZeroUsize::new(2).unwrap()),
        )
    }

    #[test]
    fn selected_renderer_and_repository_mermaid_corpus_match_the_svg_policy() {
        let diagrams = [include_str!(
            "../../tests/fixtures/mermaid/selected-corpus.md"
        )]
        .into_iter()
        .flat_map(mermaid_fences)
        .collect::<Vec<_>>();
        assert_eq!(diagrams.len(), 10, "update the reviewed corpus count");

        for (index, source) in diagrams.iter().enumerate() {
            let mut options = RenderOptions::default();
            options.layout.fast_text_metrics = true;
            let raw = render_strict(source, options)
                .unwrap_or_else(|error| panic!("diagram {} must render: {error}", index + 1));
            let sanitized = MermaidSvgSanitizer::sanitize(
                &raw,
                SvgScope::new(b"repository-corpus", NonZeroUsize::new(index + 1).unwrap()),
            )
            .unwrap_or_else(|error| {
                panic!("diagram {} must satisfy the SVG policy: {error}", index + 1)
            });
            assert!(
                !sanitized.as_str().contains(" style="),
                "diagram {} retained an inline style",
                index + 1
            );
        }
    }

    fn mermaid_fences(markdown: &str) -> Vec<String> {
        let mut diagrams = Vec::new();
        let mut source = None;
        for line in markdown.split_inclusive('\n') {
            match (&mut source, line.trim_end_matches(['\r', '\n'])) {
                (None, "```mermaid") => source = Some(String::new()),
                (Some(_), "```") => diagrams.push(source.take().unwrap()),
                (Some(source), _) => source.push_str(line),
                (None, _) => {}
            }
        }
        assert!(source.is_none(), "Mermaid fence must be closed");
        diagrams
    }

    #[test]
    fn valid_renderer_svg_is_canonical_and_references_are_namespaced() {
        let input = concat!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="40" viewBox="0 0 100 40">"#,
            r##"<defs><marker id="arrow-0" viewBox="0 0 10 10" refX="5" refY="5" markerUnits="userSpaceOnUse" markerWidth="8" markerHeight="8" orient="auto"><path d="M 0 0 L 10 5 L 0 10 z" fill="#333" stroke="#333" stroke-width="1"/></marker></defs>"##,
            r##"<path id="edge-0" d="M 0 20 L 90 20" fill="none" stroke="#333" stroke-width="2" marker-end="url(#arrow-0)"/><text x="50" y="15" text-anchor="middle">A &amp; B</text></svg>"##,
        );
        let output = sanitize(input).unwrap();
        let prefix = SvgScope::new(b"post-fixture", NonZeroUsize::new(2).unwrap()).id_prefix();
        assert!(output.as_str().contains(&format!(r#"id="{prefix}1""#)));
        assert!(output.as_str().contains(&format!(r#"id="{prefix}2""#)));
        assert!(
            output
                .as_str()
                .contains(&format!(r#"marker-end="url(#{prefix}1)""#))
        );
        assert!(output.as_str().contains("A &amp; B"));
        assert_eq!(output, sanitize(input).unwrap());
    }

    #[test]
    fn dangerous_elements_and_attributes_are_rejected() {
        for input in [
            r#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><foreignObject/></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><rect onload="alert(1)"/></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><style>@import url(https://evil.test)</style></svg>"#,
        ] {
            assert!(sanitize(input).is_err(), "accepted {input}");
        }
    }

    #[test]
    fn remote_javascript_and_css_urls_are_rejected() {
        for input in [
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><a href="javascript:alert(1)" xlink:href="javascript:alert(1)"><text>x</text></a></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="url(https://tracker.test/x)"/></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><g style="mix-blend-mode:multiply;background:url(https://tracker.test)"></g></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><image xlink:href="https://tracker.test/x.png"/></svg>"#,
        ] {
            assert!(sanitize(input).is_err(), "accepted {input}");
        }
    }

    #[test]
    fn selected_renderer_https_links_remain_safe_navigation() {
        let mut options = RenderOptions::default();
        options.layout.fast_text_metrics = true;
        let raw = render_strict(
            "flowchart LR\nA-->B\nclick A \"https://example.com/read?q=1&x=2\"\n",
            options,
        )
        .unwrap();
        let output = sanitize(&raw).unwrap();
        assert!(
            output
                .as_str()
                .contains("https://example.com/read?q=1&amp;x=2")
        );
    }

    #[test]
    fn safe_renderer_inline_styles_become_csp_safe_attributes_and_classes() {
        let input = concat!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="100%" viewBox="0 0 100 50" style="max-width: 100px; aspect-ratio: 2;">"##,
            r##"<g class="link" style="mix-blend-mode: multiply;"><path d="M0 0L1 1" fill="#fff" stroke-dasharray="1, 1" style="fill: none; stroke-dasharray: 0, 0;"/>"##,
            r##"<line x1="0" y1="0" x2="1" y2="1" fill="#fff" stroke-dasharray="1, 1" style="fill: none; stroke-dasharray: 2, 3;"/>"##,
            r#"<text x="1" y="1" text-anchor="start" font-size="8" font-weight="normal" font-family="serif" style="text-anchor: middle; font-size: 14px; font-weight: bold; font-family: Open Sans,sans-serif">x</text></g></svg>"#,
        );
        let output = sanitize(input).unwrap();
        assert!(!output.as_str().contains(" style="));
        assert!(output.as_str().contains(r#"width="100""#));
        assert!(output.as_str().contains(r#"viewBox="0 0 100 50""#));
        assert!(
            output
                .as_str()
                .contains(r#"class="link mc-mermaid-mix-blend-multiply""#)
        );
        assert!(output.as_str().contains(r#"fill="none""#));
        assert!(output.as_str().contains(r#"stroke-dasharray="0, 0""#));
        assert!(output.as_str().contains(r#"stroke-dasharray="2, 3""#));
        assert!(
            output
                .as_str()
                .contains(r#"font-family="Open Sans,sans-serif""#)
        );
        assert!(output.as_str().contains(r#"font-size="14px""#));
        assert!(output.as_str().contains(r#"font-weight="bold""#));
        assert!(output.as_str().contains(r#"text-anchor="middle""#));
        assert!(sanitize(&input.replace("multiply", "screen")).is_err());
    }

    #[test]
    fn aspect_ratio_expands_view_box_without_cropping() {
        let wide = sanitize(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100" viewBox="0 0 100 100" style="aspect-ratio: 2;"><rect x="0" y="0" width="100" height="100"/></svg>"#,
        )
        .unwrap();
        assert!(!wide.as_str().contains(" style="));
        assert!(wide.as_str().contains(r#"viewBox="-50 0 200 100""#));

        let tall = sanitize(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100" viewBox="0 0 100 100" style="aspect-ratio: 0.5;"><rect x="0" y="0" width="100" height="100"/></svg>"#,
        )
        .unwrap();
        assert!(!tall.as_str().contains(" style="));
        assert!(tall.as_str().contains(r#"viewBox="0 -50 100 200""#));
    }

    #[test]
    fn style_canonicalization_rejects_ambiguous_layout_and_reserved_class() {
        for input in [
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="90" viewBox="0 0 100 50" style="max-width: 100px;"/>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" height="50" viewBox="0 0 100 50" style="max-width: 100px;"/>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50" style="aspect-ratio: 2;"/>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><g class="mc-mermaid-mix-blend-multiply"/></svg>"#,
        ] {
            assert!(
                sanitize(input).is_err(),
                "accepted ambiguous style: {input}"
            );
        }
    }

    #[test]
    fn hsl_colors_require_bounded_channels_percentages_and_alpha() {
        for color in [
            "hsl(0, 0%, 0%)",
            "hsl(360, 100%, 100%)",
            "hsla(240.000, 99.5%, 50.25%, 0)",
            "hsla(120, 50%, 25%, 1)",
        ] {
            let input =
                format!(r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="{color}"/></svg>"#);
            assert!(sanitize(&input).is_ok(), "rejected safe color {color}");
        }

        for color in [
            "hsl(-0.1, 50%, 50%)",
            "hsl(360.1, 50%, 50%)",
            "hsl(120, 50, 50%)",
            "hsl(120, -0.1%, 50%)",
            "hsl(120, 100.1%, 50%)",
            "hsl(120, 50%, -0.1%)",
            "hsl(120, 50%, 100.1%)",
            "hsla(120, 50%, 50%, -0.1)",
            "hsla(120, 50%, 50%, 1.1)",
            "hsla(120, 50%, 50%, NaN)",
            "hsl(120, 50%, 50%, 0.5)",
            "hsla(120, 50%, 50%)",
        ] {
            let input =
                format!(r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="{color}"/></svg>"#);
            assert!(sanitize(&input).is_err(), "accepted unsafe color {color}");
        }
    }

    #[test]
    fn malformed_xml_duplicate_ids_and_missing_references_are_rejected() {
        for input in [
            r#"<svg xmlns="http://www.w3.org/2000/svg"><g></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" width="2"/>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><g id="same"/><path id="same" d="M0 0"/></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0" marker-end="url(#missing)"/></svg>"#,
        ] {
            assert!(sanitize(input).is_err(), "accepted {input}");
        }
    }

    #[test]
    fn nonfinite_malformed_and_executable_values_are_rejected() {
        for input in [
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 NaN 10"/>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0<script>"/></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="expression(alert(1))"/></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><g transform="matrix(1 0 0 1 0 0)"/></svg>"#,
        ] {
            assert!(sanitize(input).is_err(), "accepted {input}");
        }
    }

    #[test]
    fn declarations_dtd_cdata_comments_and_extra_roots_are_rejected() {
        for input in [
            r#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"/>"#,
            r#"<!DOCTYPE svg [<!ENTITY x "boom">]><svg xmlns="http://www.w3.org/2000/svg"/>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><text><![CDATA[x]]></text></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><!--x--></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"/><svg xmlns="http://www.w3.org/2000/svg"/>"#,
        ] {
            assert!(sanitize(input).is_err(), "accepted {input}");
        }
    }

    #[test]
    fn only_pinned_renderer_pngs_cross_the_inline_image_boundary() {
        for (source, expected_digest) in [
            (
                "C4Context\n  Person(admin, \"Admin\")\n",
                C4_PERSON_ICON_SHA256,
            ),
            (
                "C4Context\n  Person_Ext(visitor, \"Visitor\")\n",
                C4_EXTERNAL_PERSON_ICON_SHA256,
            ),
        ] {
            let mut options = RenderOptions::default();
            options.layout.fast_text_metrics = true;
            let c4 = render_strict(source, options).unwrap();
            let encoded = c4
                .split_once("data:image/png;base64,")
                .and_then(|(_, suffix)| suffix.split_once('"'))
                .map(|(encoded, _)| encoded)
                .expect("the selected C4 person shape must embed its pinned icon");
            let icon = STANDARD.decode(encoded).unwrap();

            let actual_digest: [u8; 32] = Sha256::digest(icon).into();
            assert_eq!(actual_digest, expected_digest);
            assert!(sanitize(&c4).is_ok());
        }

        let png = STANDARD.encode(PNG_SIGNATURE);
        let arbitrary = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><image x="0" y="0" width="1" height="1" xlink:href="data:image/png;base64,{png}"/></svg>"#
        );
        assert!(sanitize(&arbitrary).is_err());
        assert!(sanitize(&arbitrary.replace("image/png", "image/svg+xml")).is_err());
        let oversized = STANDARD.encode(vec![0_u8; MAX_EMBEDDED_IMAGE_BYTES + 1]);
        assert!(sanitize(&arbitrary.replace(&png, &oversized)).is_err());
    }

    #[test]
    fn configured_input_depth_and_output_limits_fail_closed() {
        let mut limits = SvgLimits::production();
        limits.input_bytes = 10;
        assert!(matches!(
            MermaidSvgSanitizer::sanitize_with_limits(
                r#"<svg xmlns="http://www.w3.org/2000/svg"/>"#,
                SvgScope::new(b"post", NonZeroUsize::MIN),
                limits,
            ),
            Err(SvgSanitizeError::InputTooLarge { .. })
        ));

        limits = SvgLimits::production();
        limits.depth = 1;
        assert!(matches!(
            MermaidSvgSanitizer::sanitize_with_limits(
                r#"<svg xmlns="http://www.w3.org/2000/svg"><g/></svg>"#,
                SvgScope::new(b"post", NonZeroUsize::MIN),
                limits,
            ),
            Err(SvgSanitizeError::DepthExceeded { .. })
        ));

        limits = SvgLimits::production();
        limits.attributes_per_element = 1;
        assert!(matches!(
            MermaidSvgSanitizer::sanitize_with_limits(
                r#"<svg xmlns="http://www.w3.org/2000/svg"><text style="text-anchor: middle; font-size: 14px; font-weight: bold; font-family: sans-serif;">x</text></svg>"#,
                SvgScope::new(b"post", NonZeroUsize::MIN),
                limits,
            ),
            Err(SvgSanitizeError::AttributeLimitExceeded { limit: 1 })
        ));

        limits = SvgLimits::production();
        limits.output_bytes = 10;
        assert!(matches!(
            MermaidSvgSanitizer::sanitize_with_limits(
                r#"<svg xmlns="http://www.w3.org/2000/svg"/>"#,
                SvgScope::new(b"post", NonZeroUsize::MIN),
                limits,
            ),
            Err(SvgSanitizeError::OutputTooLarge { .. })
        ));
    }
}
