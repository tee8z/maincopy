use std::{num::NonZeroUsize, ops::Range, sync::Arc};

use markdown_compiler::identity::finalize_post_revision;
use markdown_compiler::{
    AssetRevisionReference, DigestedAsset, LogicalAssetPath, LogicalContentPath,
    MarkdownDestinationKind, MarkdownDestinationOrdinal, PostDocument, PostRendererIdentity,
    PostRevisionDigest, ResolvedLocalAssetLookupError, ResolvedLocalAssetStore, ResolvedPostAssets,
    ResolvedSiteAssets, RevisionIdentityError, SiteSnapshotDigest, digest_asset,
};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde::Serialize;
use thiserror::Error;
use url::{Position, Url};

use super::SnapshotAssetPath;

const MAX_RENDERED_HTML_BYTES: usize = 32 * 1024 * 1024;
const MAX_MERMAID_SOURCE_BYTES: usize = 256 * 1024;
const MAX_MERMAID_BLOCKS: usize = 64;

/// Render one post with the closed V1 CommonMark pipeline.
pub fn render_markdown(
    document: &PostDocument,
    assets: &ResolvedPostAssets,
    site_assets: &ResolvedSiteAssets,
) -> Result<RenderedPost, MarkdownRenderError> {
    render_markdown_with_limits(document, assets, site_assets, RendererLimits::production())
}

fn render_markdown_with_limits(
    document: &PostDocument,
    assets: &ResolvedPostAssets,
    site_assets: &ResolvedSiteAssets,
    limits: RendererLimits,
) -> Result<RenderedPost, MarkdownRenderError> {
    let identity = PostRendererIdentity::baseline();
    let rendered = MarkdownEventRenderer::new(document, assets, site_assets, limits).render()?;
    let generated_assets: Vec<GeneratedPostAsset> = Vec::new();
    validate_generated_assets(document, assets, &generated_assets)?;
    let generated_identities = generated_assets
        .iter()
        .map(|generated| generated.asset.clone())
        .collect::<Vec<_>>();
    let revision = finalize_post_revision(
        document,
        assets,
        site_assets,
        &identity,
        rendered.article.identity_html.as_bytes(),
        &generated_identities,
        &document.metadata.distribution,
    )
    .map_err(|error| identity_error(document, error))?;

    Ok(RenderedPost {
        document: document.clone(),
        assets: assets.clone(),
        renderer: identity,
        article: rendered.article,
        mermaid: rendered.mermaid.into(),
        generated_assets: generated_assets.into(),
        revision,
    })
}

/// One fully rendered, identity-bound post revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedPost {
    pub(crate) document: PostDocument,
    pub(crate) assets: ResolvedPostAssets,
    pub(crate) renderer: PostRendererIdentity,
    pub(crate) article: RenderedArticle,
    pub(crate) mermaid: Arc<[MermaidPlaceholder]>,
    pub(crate) generated_assets: Arc<[GeneratedPostAsset]>,
    pub(crate) revision: PostRevisionDigest,
}

impl RenderedPost {
    pub(super) fn project_for_snapshot(
        &self,
        snapshot: &SiteSnapshotDigest,
        site_assets: &ResolvedSiteAssets,
        local_assets: &ResolvedLocalAssetStore,
    ) -> Result<String, MarkdownRenderError> {
        if !self.assets.shares_policy_with(site_assets) {
            return Err(MarkdownRenderError::new(
                &self.document,
                MarkdownRenderLocation::Document,
                MarkdownRenderErrorCode::AssetPolicyMismatch,
                "rendered post was approved under a different external-asset policy",
            ));
        }
        self.article.project_for_snapshot(
            &self.document.path,
            snapshot,
            local_assets,
            MAX_RENDERED_HTML_BYTES,
        )
    }

    pub(super) fn project_for_preview(
        &self,
        preview_asset_endpoint: &str,
        site_assets: &ResolvedSiteAssets,
        local_assets: &ResolvedLocalAssetStore,
    ) -> Result<String, MarkdownRenderError> {
        if !self.assets.shares_policy_with(site_assets) {
            return Err(MarkdownRenderError::new(
                &self.document,
                MarkdownRenderLocation::Document,
                MarkdownRenderErrorCode::AssetPolicyMismatch,
                "rendered post preview uses a different external-asset policy",
            ));
        }
        if !preview_asset_endpoint.starts_with('/')
            || preview_asset_endpoint.contains(['?', '#', '"'])
        {
            return Err(MarkdownRenderError::new(
                &self.document,
                MarkdownRenderLocation::Document,
                MarkdownRenderErrorCode::RevisionIdentityRejected,
                "preview asset endpoint is not a safe root-relative path",
            ));
        }
        self.article.project_with_local_asset_urls(
            &self.document.path,
            local_assets,
            MAX_RENDERED_HTML_BYTES,
            |asset| {
                Ok(format!(
                    "{preview_asset_endpoint}?path={}",
                    asset.path.as_str()
                ))
            },
        )
    }
}

/// A generated post asset whose digest was calculated from its owned bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GeneratedPostAsset {
    pub(super) asset: DigestedAsset,
    pub(super) bytes: Arc<[u8]>,
}

impl GeneratedPostAsset {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Slice 6 is the first production producer of generated renderer assets"
        )
    )]
    pub(super) fn from_owned_bytes(path: LogicalAssetPath, bytes: Arc<[u8]>) -> Self {
        let asset = DigestedAsset::new(path, digest_asset(&bytes));
        Self { asset, bytes }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct MermaidBlockOrdinal(NonZeroUsize);

impl MermaidBlockOrdinal {
    pub const fn get(self) -> usize {
        self.0.get()
    }

    fn from_index(index: usize) -> Option<Self> {
        index.checked_add(1).and_then(NonZeroUsize::new).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CodeBlockOrdinal(NonZeroUsize);

impl CodeBlockOrdinal {
    pub const fn get(self) -> usize {
        self.0.get()
    }

    fn new(value: NonZeroUsize) -> Self {
        Self(value)
    }
}

/// Escaped inline placeholder input retained for Slice 6 diagram rendering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MermaidPlaceholder {
    pub ordinal: MermaidBlockOrdinal,
    pub source: Arc<str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderDestinationKind {
    Link,
    Image,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MarkdownRenderLocation {
    Document,
    Destination {
        ordinal: MarkdownDestinationOrdinal,
        destination_kind: RenderDestinationKind,
    },
    CodeBlock {
        ordinal: CodeBlockOrdinal,
    },
    GeneratedAsset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigationRejection {
    Empty,
    Whitespace,
    ForbiddenCharacter,
    ProtocolRelative,
    UnsupportedScheme,
    MissingAuthority,
    Credentials,
    Traversal,
    NotRootRelative,
    Malformed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkdownRenderErrorCode {
    DestinationCountExceeded,
    AssetOccurrenceMissing,
    AssetOccurrenceMismatch,
    AssetOccurrenceUnused,
    NavigationRejected,
    MermaidBlockCountExceeded,
    MermaidBlockTooLarge,
    RenderedHtmlTooLarge,
    UnsupportedCommonMarkEvent,
    MalformedCommonMarkEvents,
    GeneratedAssetCollision,
    AssetPolicyMismatch,
    LocalAssetMissing,
    LocalAssetDigestMismatch,
    RevisionIdentityRejected,
}

#[derive(Clone, Debug, Eq, Error, PartialEq, Serialize)]
#[error("{path}: {code:?}: {message}")]
pub struct MarkdownRenderError {
    pub path: LogicalContentPath,
    pub location: MarkdownRenderLocation,
    pub code: MarkdownRenderErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigation_rejection: Option<NavigationRejection>,
    pub message: Box<str>,
}

impl MarkdownRenderError {
    fn new(
        document: &PostDocument,
        location: MarkdownRenderLocation,
        code: MarkdownRenderErrorCode,
        message: impl Into<Box<str>>,
    ) -> Self {
        Self {
            path: document.path.clone(),
            location,
            code,
            navigation_rejection: None,
            message: message.into(),
        }
    }

    fn navigation(
        document: &PostDocument,
        ordinal: MarkdownDestinationOrdinal,
        reason: NavigationRejection,
    ) -> Self {
        Self {
            path: document.path.clone(),
            location: MarkdownRenderLocation::Destination {
                ordinal,
                destination_kind: RenderDestinationKind::Link,
            },
            code: MarkdownRenderErrorCode::NavigationRejected,
            navigation_rejection: Some(reason),
            message: format!("link destination was rejected: {reason:?}").into_boxed_str(),
        }
    }
}

#[derive(Clone, Copy)]
struct RendererLimits {
    rendered_html_bytes: usize,
    mermaid_source_bytes: usize,
    mermaid_blocks: usize,
}

impl RendererLimits {
    const fn production() -> Self {
        Self {
            rendered_html_bytes: MAX_RENDERED_HTML_BYTES,
            mermaid_source_bytes: MAX_MERMAID_SOURCE_BYTES,
            mermaid_blocks: MAX_MERMAID_BLOCKS,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RenderedArticle {
    pub(crate) identity_html: Arc<str>,
    plan: Arc<[ArticleChunk]>,
}

impl RenderedArticle {
    fn project_for_snapshot(
        &self,
        path: &LogicalContentPath,
        snapshot: &SiteSnapshotDigest,
        local_assets: &ResolvedLocalAssetStore,
        limit: usize,
    ) -> Result<String, MarkdownRenderError> {
        self.project_with_local_asset_urls(path, local_assets, limit, |asset| {
            SnapshotAssetPath::new(snapshot, &asset.path)
                .map(|projected| projected.as_str().to_owned())
                .map_err(|error| MarkdownRenderError {
                    path: path.clone(),
                    location: MarkdownRenderLocation::Document,
                    code: MarkdownRenderErrorCode::RevisionIdentityRejected,
                    navigation_rejection: None,
                    message: error.to_string().into_boxed_str(),
                })
        })
    }

    fn project_with_local_asset_urls(
        &self,
        path: &LogicalContentPath,
        local_assets: &ResolvedLocalAssetStore,
        limit: usize,
        mut project: impl FnMut(&DigestedAsset) -> Result<String, MarkdownRenderError>,
    ) -> Result<String, MarkdownRenderError> {
        for chunk in &*self.plan {
            if let ArticleChunk::LocalAsset(asset) = chunk {
                local_assets
                    .resolve(asset)
                    .map_err(|error| local_asset_projection_error(path, error))?;
            }
        }
        let mut output = String::with_capacity(self.identity_html.len());
        for chunk in &*self.plan {
            let value = match chunk {
                ArticleChunk::Literal(value) => value.as_ref(),
                ArticleChunk::LocalAsset(asset) => {
                    let projected = project(asset)?;
                    if output
                        .len()
                        .checked_add(projected.len())
                        .is_none_or(|size| size > limit)
                    {
                        return Err(rendered_html_limit_error(path));
                    }
                    output.push_str(&projected);
                    continue;
                }
            };
            if output
                .len()
                .checked_add(value.len())
                .is_none_or(|size| size > limit)
            {
                return Err(rendered_html_limit_error(path));
            }
            output.push_str(value);
        }
        Ok(output)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ArticleChunk {
    Literal(Arc<str>),
    LocalAsset(DigestedAsset),
}

struct RenderResult {
    article: RenderedArticle,
    mermaid: Vec<MermaidPlaceholder>,
}

struct MarkdownEventRenderer<'input> {
    document: &'input PostDocument,
    assets: &'input ResolvedPostAssets,
    site_assets: &'input ResolvedSiteAssets,
    events: Vec<(Event<'input>, Range<usize>)>,
    cursor: usize,
    destination_count: usize,
    approved_cursor: usize,
    code_block_count: usize,
    mermaid: Vec<MermaidPlaceholder>,
    writer: ArticleWriter,
    limits: RendererLimits,
}

impl<'input> MarkdownEventRenderer<'input> {
    fn new(
        document: &'input PostDocument,
        assets: &'input ResolvedPostAssets,
        site_assets: &'input ResolvedSiteAssets,
        limits: RendererLimits,
    ) -> Self {
        let events = Parser::new_ext(document.markdown.as_str(), Options::empty())
            .into_offset_iter()
            .collect();
        Self {
            document,
            assets,
            site_assets,
            events,
            cursor: 0,
            destination_count: 0,
            approved_cursor: 0,
            code_block_count: 0,
            mermaid: Vec::new(),
            writer: ArticleWriter::new(limits.rendered_html_bytes),
            limits,
        }
    }

    fn render(mut self) -> Result<RenderResult, MarkdownRenderError> {
        while self.cursor < self.events.len() {
            let (event, range) = self.events[self.cursor].clone();
            self.cursor += 1;
            self.render_event(event, range)?;
        }
        if let Some(unused) = self.assets.markdown_destinations.get(self.approved_cursor) {
            return Err(self.error_at_destination(
                unused.ordinal,
                destination_kind(unused.kind),
                MarkdownRenderErrorCode::AssetOccurrenceUnused,
                "resolver-approved asset occurrence was not consumed by the renderer",
            ));
        }
        Ok(RenderResult {
            article: self.writer.finish(),
            mermaid: self.mermaid,
        })
    }

    fn render_event(
        &mut self,
        event: Event<'input>,
        range: Range<usize>,
    ) -> Result<(), MarkdownRenderError> {
        match event {
            Event::Start(tag) => self.render_start(tag, range),
            Event::End(tag) => self.render_end(tag),
            Event::Text(text) => self.write_escaped_body(&text),
            Event::Code(text) => {
                self.write("<code>")?;
                self.write_escaped_body(&text)?;
                self.write("</code>")
            }
            Event::Html(html) | Event::InlineHtml(html) => self.write_escaped_body(&html),
            Event::SoftBreak => self.write("\n"),
            Event::HardBreak => self.write("<br />\n"),
            Event::Rule => {
                if !self.writer.end_newline {
                    self.write("\n")?;
                }
                self.write("<hr />\n")
            }
            Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_) => Err(self.unsupported("optional CommonMark event")),
        }
    }

    fn render_start(
        &mut self,
        tag: Tag<'input>,
        range: Range<usize>,
    ) -> Result<(), MarkdownRenderError> {
        match tag {
            Tag::Paragraph => self.write_block_start("<p>"),
            Tag::Heading {
                level,
                id: _,
                classes: _,
                attrs: _,
            } => self.write_block_start(&format!("<{level}>")),
            Tag::BlockQuote(None) => self.write_block_start("<blockquote>\n"),
            Tag::CodeBlock(kind) => self.render_code_block(kind),
            Tag::List(Some(1)) => self.write_block_start("<ol>\n"),
            Tag::List(Some(start)) => {
                if !self.writer.end_newline {
                    self.write("\n")?;
                }
                self.write("<ol start=\"")?;
                self.write(&start.to_string())?;
                self.write("\">\n")
            }
            Tag::List(None) => self.write_block_start("<ul>\n"),
            Tag::Item => self.write_block_start("<li>"),
            Tag::Emphasis => self.write("<em>"),
            Tag::Strong => self.write("<strong>"),
            Tag::Link {
                link_type: _,
                dest_url,
                title,
                id: _,
            } => self.render_link(dest_url.as_ref(), title.as_ref(), range),
            Tag::Image {
                link_type: _,
                dest_url,
                title,
                id: _,
            } => self.render_image(dest_url.as_ref(), title.as_ref(), range),
            Tag::HtmlBlock => Ok(()),
            Tag::BlockQuote(Some(_))
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::Strikethrough
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => Err(self.unsupported("optional CommonMark tag")),
        }
    }

    fn render_end(&mut self, tag: TagEnd) -> Result<(), MarkdownRenderError> {
        match tag {
            TagEnd::Paragraph => self.write("</p>\n"),
            TagEnd::Heading(level) => self.write(&format!("</{level}>\n")),
            TagEnd::BlockQuote(None) => self.write("</blockquote>\n"),
            TagEnd::List(true) => self.write("</ol>\n"),
            TagEnd::List(false) => self.write("</ul>\n"),
            TagEnd::Item => self.write("</li>\n"),
            TagEnd::Emphasis => self.write("</em>"),
            TagEnd::Strong => self.write("</strong>"),
            TagEnd::Link => self.write("</a>"),
            TagEnd::HtmlBlock => Ok(()),
            TagEnd::CodeBlock | TagEnd::Image => Err(self.malformed()),
            TagEnd::BlockQuote(Some(_))
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => Err(self.unsupported("optional CommonMark end tag")),
        }
    }

    fn render_code_block(
        &mut self,
        kind: CodeBlockKind<'input>,
    ) -> Result<(), MarkdownRenderError> {
        self.code_block_count = self
            .code_block_count
            .checked_add(1)
            .ok_or_else(|| self.malformed())?;
        let ordinal = CodeBlockOrdinal::new(
            NonZeroUsize::new(self.code_block_count).ok_or_else(|| self.malformed())?,
        );
        let source = self.collect_code_block()?;
        if matches!(&kind, CodeBlockKind::Fenced(info) if info.as_ref() == "mermaid") {
            if self.mermaid.len() >= self.limits.mermaid_blocks {
                return Err(MarkdownRenderError::new(
                    self.document,
                    MarkdownRenderLocation::CodeBlock { ordinal },
                    MarkdownRenderErrorCode::MermaidBlockCountExceeded,
                    "post contains more Mermaid blocks than the configured limit",
                ));
            }
            if source.len() > self.limits.mermaid_source_bytes {
                return Err(MarkdownRenderError::new(
                    self.document,
                    MarkdownRenderLocation::CodeBlock { ordinal },
                    MarkdownRenderErrorCode::MermaidBlockTooLarge,
                    "Mermaid source exceeds the configured byte limit",
                ));
            }
            let Some(ordinal) = MermaidBlockOrdinal::from_index(self.mermaid.len()) else {
                return Err(self.malformed());
            };
            self.write_block_start(
                "<div class=\"mermaid-placeholder\" data-maincopy-mermaid=\"v1\" data-block=\"",
            )?;
            self.write(&ordinal.get().to_string())?;
            self.write("\"><pre><code>")?;
            self.write_escaped_body(&source)?;
            self.write("</code></pre></div>\n")?;
            self.mermaid.push(MermaidPlaceholder {
                ordinal,
                source: Arc::from(source),
            });
        } else {
            self.write_block_start("<pre><code>")?;
            self.write_escaped_body(&source)?;
            self.write("</code></pre>\n")?;
        }
        Ok(())
    }

    fn collect_code_block(&mut self) -> Result<String, MarkdownRenderError> {
        let mut source = String::new();
        while self.cursor < self.events.len() {
            let (event, _) = self.events[self.cursor].clone();
            self.cursor += 1;
            match event {
                Event::Text(text) => source.push_str(&text),
                Event::End(TagEnd::CodeBlock) => return Ok(source),
                _ => return Err(self.malformed()),
            }
        }
        Err(self.malformed())
    }

    fn render_link(
        &mut self,
        authored: &str,
        title: &str,
        range: Range<usize>,
    ) -> Result<(), MarkdownRenderError> {
        let target = self.validate_destination(
            RenderDestinationKind::Link,
            MarkdownDestinationKind::Download,
            authored,
            range,
            false,
        )?;
        self.write("<a href=\"")?;
        self.write_destination(&target)?;
        if !title.is_empty() {
            self.write("\" title=\"")?;
            self.write_escaped_attribute(title)?;
        }
        self.write("\">")
    }

    fn render_image(
        &mut self,
        authored: &str,
        title: &str,
        range: Range<usize>,
    ) -> Result<(), MarkdownRenderError> {
        let target = self.validate_destination(
            RenderDestinationKind::Image,
            MarkdownDestinationKind::Image,
            authored,
            range,
            true,
        )?;
        let alt = self.collect_image_alt()?;
        self.write("<img src=\"")?;
        self.write_destination(&target)?;
        self.write("\" alt=\"")?;
        self.write_escaped_attribute(&alt)?;
        if !title.is_empty() {
            self.write("\" title=\"")?;
            self.write_escaped_attribute(title)?;
        }
        self.write("\" />")
    }

    fn collect_image_alt(&mut self) -> Result<String, MarkdownRenderError> {
        let mut output = String::new();
        let mut depth = 0_usize;
        while self.cursor < self.events.len() {
            let (event, range) = self.events[self.cursor].clone();
            self.cursor += 1;
            match event {
                Event::End(TagEnd::Image) if depth == 0 => return Ok(output),
                Event::Start(Tag::Image { dest_url, .. }) => {
                    let _ = self.validate_destination(
                        RenderDestinationKind::Image,
                        MarkdownDestinationKind::Image,
                        dest_url.as_ref(),
                        range,
                        true,
                    )?;
                    self.enter_image_alt_nesting(&mut depth)?;
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    let _ = self.validate_destination(
                        RenderDestinationKind::Link,
                        MarkdownDestinationKind::Download,
                        dest_url.as_ref(),
                        range,
                        false,
                    )?;
                    self.enter_image_alt_nesting(&mut depth)?;
                }
                Event::Start(_) => {
                    self.enter_image_alt_nesting(&mut depth)?;
                }
                Event::End(_) if depth > 0 => depth -= 1,
                Event::End(_) => return Err(self.malformed()),
                Event::Text(text)
                | Event::Code(text)
                | Event::Html(text)
                | Event::InlineHtml(text) => output.push_str(&text),
                Event::SoftBreak | Event::HardBreak | Event::Rule => output.push(' '),
                Event::InlineMath(text) | Event::DisplayMath(text) => output.push_str(&text),
                Event::FootnoteReference(name) => {
                    output.push('[');
                    output.push_str(&name);
                    output.push(']');
                }
                Event::TaskListMarker(checked) => {
                    output.push_str(if checked { "[x]" } else { "[ ]" });
                }
            }
        }
        Err(self.malformed())
    }

    fn enter_image_alt_nesting(&self, depth: &mut usize) -> Result<(), MarkdownRenderError> {
        *depth = depth.checked_add(1).ok_or_else(|| self.malformed())?;
        Ok(())
    }

    fn validate_destination(
        &mut self,
        render_kind: RenderDestinationKind,
        asset_kind: MarkdownDestinationKind,
        authored: &str,
        range: Range<usize>,
        asset_required: bool,
    ) -> Result<ValidatedDestination, MarkdownRenderError> {
        self.destination_count = self.destination_count.checked_add(1).ok_or_else(|| {
            MarkdownRenderError::new(
                self.document,
                MarkdownRenderLocation::Document,
                MarkdownRenderErrorCode::DestinationCountExceeded,
                "Markdown contains more destinations than this platform can address",
            )
        })?;
        let ordinal = MarkdownDestinationOrdinal::new(
            NonZeroUsize::new(self.destination_count).ok_or_else(|| self.malformed())?,
        );
        let approved = self.assets.markdown_destinations.get(self.approved_cursor);
        if let Some(approved) = approved.filter(|approved| approved.ordinal == ordinal) {
            if approved.kind != asset_kind
                || approved.source_range.start != range.start
                || approved.source_range.end != range.end
                || approved.authored.as_str() != authored
            {
                return Err(self.error_at_destination(
                    ordinal,
                    render_kind,
                    MarkdownRenderErrorCode::AssetOccurrenceMismatch,
                    "Markdown destination does not match its resolver-approved occurrence",
                ));
            }
            self.approved_cursor += 1;
            return Ok(ValidatedDestination::Asset(approved.target.clone()));
        }
        if approved.is_some_and(|approved| approved.ordinal.get() < ordinal.get()) {
            return Err(self.error_at_destination(
                ordinal,
                render_kind,
                MarkdownRenderErrorCode::AssetOccurrenceUnused,
                "renderer advanced past a resolver-approved asset occurrence",
            ));
        }
        if asset_required || self.looks_like_asset(authored) {
            return Err(self.error_at_destination(
                ordinal,
                render_kind,
                MarkdownRenderErrorCode::AssetOccurrenceMissing,
                "asset destination has no matching resolver-approved occurrence",
            ));
        }
        normalize_navigation(authored)
            .map(ValidatedDestination::Navigation)
            .map_err(|reason| MarkdownRenderError::navigation(self.document, ordinal, reason))
    }

    fn looks_like_asset(&self, authored: &str) -> bool {
        if LogicalAssetPath::parse(authored).is_ok() {
            return true;
        }
        let Ok(url) = Url::parse(authored) else {
            return false;
        };
        self.site_assets.allowed_origins.iter().any(|origin| {
            origin.as_url().scheme() == url.scheme()
                && origin.as_url().host() == url.host()
                && origin.as_url().port_or_known_default() == url.port_or_known_default()
        })
    }

    fn write_destination(
        &mut self,
        destination: &ValidatedDestination,
    ) -> Result<(), MarkdownRenderError> {
        match destination {
            ValidatedDestination::Navigation(value) => self.write_escaped_attribute(value),
            ValidatedDestination::Asset(AssetRevisionReference::External(url)) => {
                self.write_escaped_attribute(url.as_str())
            }
            ValidatedDestination::Asset(AssetRevisionReference::Local(asset)) => self
                .writer
                .write_local_asset(asset.clone())
                .map_err(|_| rendered_html_limit_error(&self.document.path)),
        }
    }

    fn write_block_start(&mut self, value: &str) -> Result<(), MarkdownRenderError> {
        if !self.writer.end_newline {
            self.write("\n")?;
        }
        self.write(value)
    }

    fn write(&mut self, value: &str) -> Result<(), MarkdownRenderError> {
        self.writer
            .write(value)
            .map_err(|_| rendered_html_limit_error(&self.document.path))
    }

    fn write_escaped_body(&mut self, value: &str) -> Result<(), MarkdownRenderError> {
        for piece in Escaped::body(value) {
            self.write(piece)?;
        }
        Ok(())
    }

    fn write_escaped_attribute(&mut self, value: &str) -> Result<(), MarkdownRenderError> {
        for piece in Escaped::attribute(value) {
            self.write(piece)?;
        }
        Ok(())
    }

    fn error_at_destination(
        &self,
        ordinal: MarkdownDestinationOrdinal,
        destination_kind: RenderDestinationKind,
        code: MarkdownRenderErrorCode,
        message: &'static str,
    ) -> MarkdownRenderError {
        MarkdownRenderError::new(
            self.document,
            MarkdownRenderLocation::Destination {
                ordinal,
                destination_kind,
            },
            code,
            message,
        )
    }

    fn unsupported(&self, event: &'static str) -> MarkdownRenderError {
        MarkdownRenderError::new(
            self.document,
            MarkdownRenderLocation::Document,
            MarkdownRenderErrorCode::UnsupportedCommonMarkEvent,
            format!("the strict CommonMark parser produced an unsupported {event}"),
        )
    }

    fn malformed(&self) -> MarkdownRenderError {
        MarkdownRenderError::new(
            self.document,
            MarkdownRenderLocation::Document,
            MarkdownRenderErrorCode::MalformedCommonMarkEvents,
            "the CommonMark parser produced an unbalanced event stream",
        )
    }
}

enum ValidatedDestination {
    Navigation(String),
    Asset(AssetRevisionReference),
}

struct ArticleWriter {
    identity_html: String,
    literal: String,
    plan: Vec<ArticleChunk>,
    limit: usize,
    end_newline: bool,
}

impl ArticleWriter {
    fn new(limit: usize) -> Self {
        Self {
            identity_html: String::new(),
            literal: String::new(),
            plan: Vec::new(),
            limit,
            end_newline: true,
        }
    }

    fn write(&mut self, value: &str) -> Result<(), RenderedHtmlLimit> {
        self.reserve(value.len())?;
        self.identity_html.push_str(value);
        self.literal.push_str(value);
        if !value.is_empty() {
            self.end_newline = value.ends_with('\n');
        }
        Ok(())
    }

    fn write_local_asset(&mut self, asset: DigestedAsset) -> Result<(), RenderedHtmlLimit> {
        let path = asset.path.as_str();
        let path_len = path.len();
        let path_ends_newline = path.ends_with('\n');
        self.reserve(path_len)?;
        self.identity_html.push_str(path);
        self.flush_literal();
        self.plan.push(ArticleChunk::LocalAsset(asset));
        if path_len > 0 {
            self.end_newline = path_ends_newline;
        }
        Ok(())
    }

    fn reserve(&self, additional: usize) -> Result<(), RenderedHtmlLimit> {
        if self
            .identity_html
            .len()
            .checked_add(additional)
            .is_some_and(|size| size <= self.limit)
        {
            Ok(())
        } else {
            Err(RenderedHtmlLimit)
        }
    }

    fn flush_literal(&mut self) {
        if !self.literal.is_empty() {
            self.plan
                .push(ArticleChunk::Literal(Arc::from(self.literal.as_str())));
            self.literal.clear();
        }
    }

    fn finish(mut self) -> RenderedArticle {
        self.flush_literal();
        RenderedArticle {
            identity_html: Arc::from(self.identity_html),
            plan: self.plan.into(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RenderedHtmlLimit;

enum Escaped<'input> {
    Body(EscapePieces<'input>),
    Attribute(EscapePieces<'input>),
}

impl<'input> Escaped<'input> {
    fn body(value: &'input str) -> Self {
        Self::Body(EscapePieces::new(value, false))
    }

    fn attribute(value: &'input str) -> Self {
        Self::Attribute(EscapePieces::new(value, true))
    }
}

impl<'input> Iterator for Escaped<'input> {
    type Item = &'input str;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Body(pieces) | Self::Attribute(pieces) => pieces.next(),
        }
    }
}

struct EscapePieces<'input> {
    remainder: &'input str,
    pending: Option<&'static str>,
    attribute: bool,
}

impl<'input> EscapePieces<'input> {
    const fn new(value: &'input str, attribute: bool) -> Self {
        Self {
            remainder: value,
            pending: None,
            attribute,
        }
    }
}

impl<'input> Iterator for EscapePieces<'input> {
    type Item = &'input str;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(entity) = self.pending.take() {
            // The entities are static and therefore live for at least `'input`.
            return Some(entity);
        }
        let position = self
            .remainder
            .char_indices()
            .find_map(|(index, character)| {
                escape_entity(character, self.attribute)
                    .map(|entity| (index, character.len_utf8(), entity))
            });
        let Some((index, width, entity)) = position else {
            return (!self.remainder.is_empty()).then(|| std::mem::take(&mut self.remainder));
        };
        if index == 0 {
            self.remainder = &self.remainder[width..];
            return Some(entity);
        }
        let literal = &self.remainder[..index];
        self.remainder = &self.remainder[index + width..];
        self.pending = Some(entity);
        Some(literal)
    }
}

const fn escape_entity(character: char, attribute: bool) -> Option<&'static str> {
    match character {
        '&' => Some("&amp;"),
        '<' => Some("&lt;"),
        '>' => Some("&gt;"),
        '"' if attribute => Some("&quot;"),
        _ => None,
    }
}

fn normalize_navigation(value: &str) -> Result<String, NavigationRejection> {
    validate_navigation_text(value)?;
    if value.starts_with('/') {
        normalize_root_relative_navigation(value)
    } else {
        normalize_https_navigation(value)
    }
}

fn validate_navigation_text(value: &str) -> Result<(), NavigationRejection> {
    if value.is_empty() {
        return Err(NavigationRejection::Empty);
    }
    if value.trim() != value {
        return Err(NavigationRejection::Whitespace);
    }
    if value.contains('\\') || value.chars().any(char::is_control) {
        return Err(NavigationRejection::ForbiddenCharacter);
    }
    if value.starts_with("//") {
        return Err(NavigationRejection::ProtocolRelative);
    }
    Ok(())
}

fn normalize_root_relative_navigation(value: &str) -> Result<String, NavigationRejection> {
    reject_traversal(raw_root_relative_path(value))?;
    let base =
        Url::parse("https://maincopy.invalid/").map_err(|_| NavigationRejection::Malformed)?;
    let parsed = base
        .join(value)
        .map_err(|_| NavigationRejection::Malformed)?;
    if parsed.origin() != base.origin()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(NavigationRejection::Malformed);
    }
    Ok(parsed[Position::BeforePath..].to_owned())
}

fn normalize_https_navigation(value: &str) -> Result<String, NavigationRejection> {
    validate_raw_https_navigation(value)?;
    let parsed = Url::parse(value).map_err(|_| NavigationRejection::Malformed)?;
    if parsed.scheme() != "https" {
        return Err(NavigationRejection::UnsupportedScheme);
    }
    if parsed.host().is_none() {
        return Err(NavigationRejection::MissingAuthority);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(NavigationRejection::Credentials);
    }
    Ok(parsed.into())
}

fn validate_raw_https_navigation(value: &str) -> Result<(), NavigationRejection> {
    let Some((raw_scheme, remainder)) = value.split_once("://") else {
        return Err(NavigationRejection::NotRootRelative);
    };
    if !raw_scheme.eq_ignore_ascii_case("https") {
        return Err(NavigationRejection::UnsupportedScheme);
    }
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    if authority.is_empty() || authority.ends_with(':') {
        return Err(NavigationRejection::MissingAuthority);
    }
    if authority.contains('@') {
        return Err(NavigationRejection::Credentials);
    }
    reject_traversal(
        remainder[authority_end..]
            .split(['?', '#'])
            .next()
            .unwrap_or(""),
    )
}

fn raw_root_relative_path(value: &str) -> &str {
    value.split(['?', '#']).next().unwrap_or("")
}

fn reject_traversal(path: &str) -> Result<(), NavigationRejection> {
    for component in path.split('/') {
        if matches!(component, "." | "..") || has_encoded_path_control(component) {
            return Err(NavigationRejection::Traversal);
        }
    }
    Ok(())
}

fn has_encoded_path_control(component: &str) -> bool {
    let bytes = component.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let Some(encoded) = bytes
                .get(index + 1..index + 3)
                .and_then(|value| decode_hex_pair(value.try_into().ok()?))
            else {
                return true;
            };
            if matches!(encoded, b'.' | b'/' | b'\\' | b'%') {
                return true;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    false
}

fn decode_hex_pair(pair: &[u8; 2]) -> Option<u8> {
    let high = hex_value(pair[0])?;
    let low = hex_value(pair[1])?;
    Some((high << 4) | low)
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn destination_kind(kind: MarkdownDestinationKind) -> RenderDestinationKind {
    match kind {
        MarkdownDestinationKind::Image => RenderDestinationKind::Image,
        MarkdownDestinationKind::Download => RenderDestinationKind::Link,
    }
}

fn validate_generated_assets(
    document: &PostDocument,
    authored: &ResolvedPostAssets,
    generated: &[GeneratedPostAsset],
) -> Result<(), MarkdownRenderError> {
    let mut paths = std::collections::BTreeSet::new();
    if let Some(AssetRevisionReference::Local(asset)) = &authored.image {
        paths.insert(&asset.path);
    }
    for reference in &authored.references {
        if let AssetRevisionReference::Local(asset) = reference {
            paths.insert(&asset.path);
        }
    }
    for asset in generated {
        if !paths.insert(&asset.asset.path) {
            return Err(MarkdownRenderError::new(
                document,
                MarkdownRenderLocation::GeneratedAsset,
                MarkdownRenderErrorCode::GeneratedAssetCollision,
                format!(
                    "generated asset path collides with another post asset: {}",
                    asset.asset.path
                ),
            ));
        }
    }
    Ok(())
}

fn identity_error(document: &PostDocument, error: RevisionIdentityError) -> MarkdownRenderError {
    MarkdownRenderError::new(
        document,
        MarkdownRenderLocation::Document,
        MarkdownRenderErrorCode::RevisionIdentityRejected,
        error.to_string(),
    )
}

fn local_asset_projection_error(
    path: &LogicalContentPath,
    error: ResolvedLocalAssetLookupError,
) -> MarkdownRenderError {
    let code = match error {
        ResolvedLocalAssetLookupError::Missing => MarkdownRenderErrorCode::LocalAssetMissing,
        ResolvedLocalAssetLookupError::DigestMismatch => {
            MarkdownRenderErrorCode::LocalAssetDigestMismatch
        }
    };
    MarkdownRenderError {
        path: path.clone(),
        location: MarkdownRenderLocation::Document,
        code,
        navigation_rejection: None,
        message: error.to_string().into_boxed_str(),
    }
}

fn rendered_html_limit_error(path: &LogicalContentPath) -> MarkdownRenderError {
    MarkdownRenderError {
        path: path.clone(),
        location: MarkdownRenderLocation::Document,
        code: MarkdownRenderErrorCode::RenderedHtmlTooLarge,
        navigation_rejection: None,
        message: "rendered article HTML exceeds the configured byte limit".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use markdown_compiler::{
        PostCollection, ResolvedContentAssets, ValidatedContent, resolve_content_assets,
    };

    use crate::content_fixtures::{asset, content_tree, post, publication};

    const POST_ID: &str = "4f054633-2d09-4b05-97d0-c6f0011a5199";

    fn publication_source(title: &str, origins: &[&str]) -> String {
        let origins = origins
            .iter()
            .map(|origin| format!("{origin:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "[site]\n\
             title = {title:?}\n\
             base_url = \"https://blog.example.com/\"\n\
             description = \"Renderer tests.\"\n\
             [author]\n\
             name = \"Example Author\"\n\
             [assets]\n\
             allowed_https_origins = [{origins}]\n"
        )
    }

    fn post_source(body: &str, draft: bool) -> String {
        format!(
            "+++\n\
             id = \"{POST_ID}\"\n\
             title = \"Rendered Post\"\n\
             slug = \"rendered-post\"\n\
             authored_at = 2026-08-29T15:00:00-04:00\n\
             description = \"Renderer fixture.\"\n\
             draft = {draft}\n\
             +++\n\
             {body}"
        )
    }

    fn candidate(
        title: &str,
        origins: &[&str],
        body: &str,
        draft: bool,
        asset_paths: &[&str],
    ) -> (ValidatedContent, ResolvedContentAssets) {
        let tree = content_tree(
            publication("publication.toml", publication_source(title, origins)),
            vec![post(
                if draft {
                    "drafts/rendered.md"
                } else {
                    "posts/rendered.md"
                },
                if draft {
                    PostCollection::Drafts
                } else {
                    PostCollection::Posts
                },
                post_source(body, draft),
            )],
            asset_paths
                .iter()
                .map(|path| {
                    asset(
                        LogicalAssetPath::parse(path).expect("fixture path must parse"),
                        format!("bytes:{path}").into_bytes(),
                    )
                })
                .collect(),
            0,
        );
        let content = tree.validate().expect("fixture content must validate");
        let assets = resolve_content_assets(&tree, &content).expect("fixture assets must resolve");
        (content, assets)
    }

    fn candidate_with_local_bytes(
        body: &str,
        path: Option<&str>,
        bytes: &[u8],
    ) -> (ValidatedContent, ResolvedContentAssets) {
        let discovered_assets = path
            .map(|path| {
                vec![asset(
                    LogicalAssetPath::parse(path).expect("fixture path must parse"),
                    bytes.to_vec(),
                )]
            })
            .unwrap_or_default();
        let tree = content_tree(
            publication("publication.toml", publication_source("Renderer", &[])),
            vec![post(
                "posts/rendered.md",
                PostCollection::Posts,
                post_source(body, false),
            )],
            discovered_assets,
            0,
        );
        let content = tree.validate().expect("fixture content must validate");
        let assets = resolve_content_assets(&tree, &content).expect("fixture assets must resolve");
        (content, assets)
    }

    fn render(origins: &[&str], body: &str, asset_paths: &[&str]) -> RenderedPost {
        let (content, assets) = candidate("Renderer", origins, body, false, asset_paths);
        render_markdown(
            &content.posts[0],
            assets.assets_for(&content.posts[0]).unwrap(),
            &assets.site,
        )
        .expect("fixture must render")
    }

    fn render_error(body: &str) -> MarkdownRenderError {
        let (content, assets) = candidate("Renderer", &[], body, false, &[]);
        render_markdown(
            &content.posts[0],
            assets.assets_for(&content.posts[0]).unwrap(),
            &assets.site,
        )
        .expect_err("fixture must be rejected")
    }

    fn snapshot() -> SiteSnapshotDigest {
        SiteSnapshotDigest::parse(&format!("site-b3-v1-{}", "11".repeat(32))).unwrap()
    }

    fn collect_and_escape_image_alt_events(
        events: Vec<Event<'static>>,
    ) -> Result<(String, String), MarkdownRenderError> {
        let (content, assets) = candidate("Renderer", &[], "body", false, &[]);
        let document = &content.posts[0];
        let mut renderer = MarkdownEventRenderer::new(
            document,
            assets.assets_for(document).unwrap(),
            &assets.site,
            RendererLimits::production(),
        );
        renderer.events = events.into_iter().map(|event| (event, 0..0)).collect();
        let alt = renderer.collect_image_alt()?;
        renderer.write_escaped_attribute(&alt)?;
        Ok((alt, renderer.writer.identity_html))
    }

    #[test]
    fn exact_commonmark_html_and_typed_asset_projection_are_deterministic() {
        let body = "# Heading\n\nRaw <b>& text</b>.\n\n[route](/notes?x=1&y=2)\n\n![diagram](assets/images/diagram.png \"Diagram\")\n\n[manual](assets/files/manual.pdf)\n\n```rust\n<a>&\n```\n\n```mermaid\ngraph TD\nA-->B\n```\n";
        let first = render(
            &[],
            body,
            &["assets/images/diagram.png", "assets/files/manual.pdf"],
        );
        let second = render(
            &[],
            body,
            &["assets/files/manual.pdf", "assets/images/diagram.png"],
        );
        let expected = "<h1>Heading</h1>\n\
                        <p>Raw &lt;b&gt;&amp; text&lt;/b&gt;.</p>\n\
                        <p><a href=\"/notes?x=1&amp;y=2\">route</a></p>\n\
                        <p><img src=\"assets/images/diagram.png\" alt=\"diagram\" title=\"Diagram\" /></p>\n\
                        <p><a href=\"assets/files/manual.pdf\">manual</a></p>\n\
                        <pre><code>&lt;a&gt;&amp;\n</code></pre>\n\
                        <div class=\"mermaid-placeholder\" data-maincopy-mermaid=\"v1\" data-block=\"1\"><pre><code>graph TD\nA--&gt;B\n</code></pre></div>\n";
        assert_eq!(first.article.identity_html.as_ref(), expected);
        assert_eq!(second.article.identity_html.as_ref(), expected);
        assert_eq!(first.revision, second.revision);
        assert_eq!(first.mermaid.len(), 1);
        assert_eq!(first.mermaid[0].source.as_ref(), "graph TD\nA-->B\n");
        assert!(first.generated_assets.is_empty());

        let (_, projection_assets) = candidate(
            "Renderer",
            &[],
            body,
            false,
            &["assets/images/diagram.png", "assets/files/manual.pdf"],
        );
        let projected = first
            .project_for_snapshot(
                &snapshot(),
                &projection_assets.site,
                &projection_assets.local_assets,
            )
            .unwrap();
        assert!(projected.contains(&format!("/assets/{}/images/diagram.png", snapshot())));
        assert!(!projected.contains("assets/images/diagram.png\""));
    }

    #[test]
    fn raw_html_is_visible_text_in_blocks_inline_and_image_alt_text() {
        let rendered = render(
            &[],
            "<script>alert(1)</script>\n\n<!-- comment -->\n\n<style>x</style>\n\n<svg><script>x</script></svg>\n\n![<b>alt</b>](assets/image.png)\n",
            &["assets/image.png"],
        );
        let html = &rendered.article.identity_html;
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<!--"));
        assert!(!html.contains("<style>"));
        assert!(!html.contains("<svg>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("alt=\"&lt;b&gt;alt&lt;/b&gt;\""));
    }

    #[test]
    fn image_alt_collects_nested_destinations_and_all_textual_event_forms() {
        let rendered = render(
            &[],
            "![outer *em* [link](/inside) ![nested](assets/nested.png) `code`<b>raw</b>  \nnext](assets/outer.png)\n",
            &["assets/outer.png", "assets/nested.png"],
        );
        assert!(
            rendered
                .article
                .identity_html
                .contains("alt=\"outer em link nested code&lt;b&gt;raw&lt;/b&gt; next\"")
        );

        let (alt, escaped) = collect_and_escape_image_alt_events(vec![
            Event::Start(Tag::Emphasis),
            Event::Text("text".into()),
            Event::Code("code".into()),
            Event::Html("<block>&".into()),
            Event::InlineHtml("<inline>".into()),
            Event::SoftBreak,
            Event::HardBreak,
            Event::Rule,
            Event::InlineMath("inline-math".into()),
            Event::DisplayMath("display-math".into()),
            Event::FootnoteReference("note".into()),
            Event::TaskListMarker(true),
            Event::TaskListMarker(false),
            Event::End(TagEnd::Emphasis),
            Event::End(TagEnd::Image),
        ])
        .unwrap();
        assert_eq!(
            alt,
            "textcode<block>&<inline>   inline-mathdisplay-math[note][x][ ]"
        );
        assert_eq!(
            escaped,
            "textcode&lt;block&gt;&amp;&lt;inline&gt;   inline-mathdisplay-math[note][x][ ]"
        );
    }

    #[test]
    fn image_alt_rejects_unbalanced_events_and_nesting_overflow() {
        for events in [
            vec![Event::End(TagEnd::Strong), Event::End(TagEnd::Image)],
            vec![Event::Start(Tag::Emphasis), Event::End(TagEnd::Image)],
            vec![Event::Text("unterminated".into())],
        ] {
            let error = collect_and_escape_image_alt_events(events).unwrap_err();
            assert_eq!(
                error.code,
                MarkdownRenderErrorCode::MalformedCommonMarkEvents
            );
        }

        let (content, assets) = candidate("Renderer", &[], "body", false, &[]);
        let document = &content.posts[0];
        let renderer = MarkdownEventRenderer::new(
            document,
            assets.assets_for(document).unwrap(),
            &assets.site,
            RendererLimits::production(),
        );
        let mut depth = usize::MAX;
        let error = renderer.enter_image_alt_nesting(&mut depth).unwrap_err();
        assert_eq!(
            error.code,
            MarkdownRenderErrorCode::MalformedCommonMarkEvents
        );
        assert_eq!(depth, usize::MAX);
    }

    #[test]
    fn all_non_mermaid_fence_labels_render_identically_without_authored_classes() {
        let expected = render(&[], "```\n<a>&\n```\n", &[]);
        for label in ["text", "ascii", "rust", "Mermaid", "mermaid trailing"] {
            let rendered = render(&[], &format!("```{label}\n<a>&\n```\n"), &[]);
            assert_eq!(
                rendered.article.identity_html,
                expected.article.identity_html
            );
            assert!(rendered.mermaid.is_empty());
            assert!(!rendered.article.identity_html.contains("language-"));
        }
        let commonmark_decoded = render(&[], "```merm&#x61;id\na-->b\n```\n", &[]);
        assert_eq!(commonmark_decoded.mermaid.len(), 1);
    }

    #[test]
    fn unsafe_navigation_variants_fail_closed_after_commonmark_unescaping() {
        let rejected = [
            (
                "[bad](javascript:alert)",
                NavigationRejection::NotRootRelative,
            ),
            (
                "[bad](jav&#x61;script:alert)",
                NavigationRejection::NotRootRelative,
            ),
            (
                "[bad](mailto:x@example.com)",
                NavigationRejection::NotRootRelative,
            ),
            (
                "[bad](//example.com/path)",
                NavigationRejection::ProtocolRelative,
            ),
            (
                "[bad](https://user@example.com/path)",
                NavigationRejection::Credentials,
            ),
            (
                "[bad](https:example.com)",
                NavigationRejection::NotRootRelative,
            ),
            (
                "[bad](https:///host)",
                NavigationRejection::MissingAuthority,
            ),
            ("[bad](/a/../secret)", NavigationRejection::Traversal),
            ("[bad](/a/%2e%2e/secret)", NavigationRejection::Traversal),
            (
                "[bad](/a/%252e%252e/secret)",
                NavigationRejection::Traversal,
            ),
        ];
        for (body, reason) in rejected {
            let error = render_error(body);
            assert_eq!(
                error.code,
                MarkdownRenderErrorCode::NavigationRejected,
                "{body}"
            );
            assert_eq!(error.navigation_rejection, Some(reason), "{body}");
        }
    }

    #[test]
    fn absolute_https_root_relative_reference_and_autolinks_are_rebuilt() {
        let rendered = render(
            &[],
            "[root](/a/b?q=1#part) [absolute](HTTPS://EXAMPLE.COM:443/a?q=1&x=2) [reference][target] <https://example.net/read>\n\n[target]: /reference\n",
            &[],
        );
        assert_eq!(
            rendered.article.identity_html.as_ref(),
            "<p><a href=\"/a/b?q=1#part\">root</a> <a href=\"https://example.com/a?q=1&amp;x=2\">absolute</a> <a href=\"/reference\">reference</a> <a href=\"https://example.net/read\">https://example.net/read</a></p>\n"
        );

        let error = render_error("<person@example.com>");
        assert_eq!(error.code, MarkdownRenderErrorCode::NavigationRejected);
    }

    #[test]
    fn resolver_occurrences_are_unique_exact_and_exhaustive() {
        let body = "[safe](/safe)\n\n![image](assets/image.png)\n\n[file](assets/file.pdf)\n";
        let (content, assets) = candidate(
            "Renderer",
            &[],
            body,
            false,
            &["assets/image.png", "assets/file.pdf"],
        );
        let approved = assets.assets_for(&content.posts[0]).unwrap();

        let (reordered, _) = candidate(
            "Renderer",
            &[],
            "![image](assets/image.png)\n\n[safe](/safe)\n\n[file](assets/file.pdf)\n",
            false,
            &["assets/image.png", "assets/file.pdf"],
        );
        let error = render_markdown(&reordered.posts[0], approved, &assets.site).unwrap_err();
        assert_eq!(error.code, MarkdownRenderErrorCode::AssetOccurrenceMissing);

        let (shifted, _) = candidate(
            "Renderer",
            &[],
            "[safe](/safe)\n\n \n![image](assets/image.png)\n\n[file](assets/file.pdf)\n",
            false,
            &["assets/image.png", "assets/file.pdf"],
        );
        let error = render_markdown(&shifted.posts[0], approved, &assets.site).unwrap_err();
        assert_eq!(error.code, MarkdownRenderErrorCode::AssetOccurrenceMismatch);

        let (removed, _) = candidate("Renderer", &[], "[safe](/safe)\n", false, &[]);
        let error = render_markdown(&removed.posts[0], approved, &assets.site).unwrap_err();
        assert_eq!(error.code, MarkdownRenderErrorCode::AssetOccurrenceUnused);
    }

    #[test]
    fn renderer_limits_are_inclusive_and_fail_one_byte_or_block_past() {
        let (content, assets) = candidate("Renderer", &[], "```mermaid\nabc\n```\n", false, &[]);
        let document = &content.posts[0];
        let post_assets = assets.assets_for(document).unwrap();
        let base = RendererLimits {
            rendered_html_bytes: 1_024,
            mermaid_source_bytes: 4,
            mermaid_blocks: 1,
        };
        render_markdown_with_limits(document, post_assets, &assets.site, base).unwrap();
        let error = render_markdown_with_limits(
            document,
            post_assets,
            &assets.site,
            RendererLimits {
                mermaid_source_bytes: 3,
                ..base
            },
        )
        .unwrap_err();
        assert_eq!(error.code, MarkdownRenderErrorCode::MermaidBlockTooLarge);

        let (two_content, two_assets) = candidate(
            "Renderer",
            &[],
            "```mermaid\na\n```\n\n```mermaid\nb\n```\n",
            false,
            &[],
        );
        let error = render_markdown_with_limits(
            &two_content.posts[0],
            two_assets.assets_for(&two_content.posts[0]).unwrap(),
            &two_assets.site,
            base,
        )
        .unwrap_err();
        assert_eq!(
            error.code,
            MarkdownRenderErrorCode::MermaidBlockCountExceeded
        );

        let mut writer = ArticleWriter::new(3);
        assert!(writer.write("abc").is_ok());
        assert!(writer.write("d").is_err());
        let (small_content, small_assets) = candidate("Renderer", &[], "a", false, &[]);
        let error = render_markdown_with_limits(
            &small_content.posts[0],
            small_assets.assets_for(&small_content.posts[0]).unwrap(),
            &small_assets.site,
            RendererLimits {
                rendered_html_bytes: 7,
                ..base
            },
        )
        .unwrap_err();
        assert_eq!(error.code, MarkdownRenderErrorCode::RenderedHtmlTooLarge);
    }

    #[test]
    fn stale_policy_product_cannot_project_even_when_public_revision_is_unchanged() {
        let body = "![cover](https://cdn.example.com/cover-v1.png)\n";
        let (old_content, old_assets) = candidate(
            "Renderer",
            &["https://cdn.example.com", "https://unused.example.com"],
            body,
            false,
            &[],
        );
        let old = render_markdown(
            &old_content.posts[0],
            old_assets.assets_for(&old_content.posts[0]).unwrap(),
            &old_assets.site,
        )
        .unwrap();
        let (new_content, new_assets) =
            candidate("Renderer", &["https://cdn.example.com"], body, false, &[]);
        let fresh = render_markdown(
            &new_content.posts[0],
            new_assets.assets_for(&new_content.posts[0]).unwrap(),
            &new_assets.site,
        )
        .unwrap();
        assert_eq!(old.revision, fresh.revision);
        let error = old
            .project_for_snapshot(&snapshot(), &new_assets.site, &new_assets.local_assets)
            .unwrap_err();
        assert_eq!(error.code, MarkdownRenderErrorCode::AssetPolicyMismatch);
        assert!(
            fresh
                .project_for_snapshot(&snapshot(), &new_assets.site, &new_assets.local_assets,)
                .unwrap()
                .contains("https://cdn.example.com/cover-v1.png")
        );
    }

    #[test]
    fn projection_requires_the_candidate_store_to_match_every_local_asset_digest() {
        let body = "![cover](assets/cover.png)\n";
        let (old_content, old_assets) =
            candidate_with_local_bytes(body, Some("assets/cover.png"), b"old bytes");
        let old = render_markdown(
            &old_content.posts[0],
            old_assets.assets_for(&old_content.posts[0]).unwrap(),
            &old_assets.site,
        )
        .unwrap();
        let (_, changed_assets) =
            candidate_with_local_bytes(body, Some("assets/cover.png"), b"changed bytes");
        let error = old
            .project_for_snapshot(
                &snapshot(),
                &changed_assets.site,
                &changed_assets.local_assets,
            )
            .unwrap_err();
        assert_eq!(
            error.code,
            MarkdownRenderErrorCode::LocalAssetDigestMismatch
        );

        let (_, missing_assets) = candidate_with_local_bytes("No asset.\n", None, b"");
        let error = old
            .project_for_snapshot(
                &snapshot(),
                &missing_assets.site,
                &missing_assets.local_assets,
            )
            .unwrap_err();
        assert_eq!(error.code, MarkdownRenderErrorCode::LocalAssetMissing);

        assert!(
            old.project_for_snapshot(&snapshot(), &old_assets.site, &old_assets.local_assets,)
                .unwrap()
                .contains(&format!("/assets/{}/cover.png", snapshot()))
        );
    }

    #[test]
    fn generated_assets_own_bytes_digest_them_and_cannot_collide() {
        let (content, assets) = candidate(
            "Renderer",
            &[],
            "![image](assets/image.png)",
            false,
            &["assets/image.png"],
        );
        let bytes: Arc<[u8]> = Arc::from(&b"generated"[..]);
        let generated = GeneratedPostAsset::from_owned_bytes(
            LogicalAssetPath::parse("assets/image.png").unwrap(),
            Arc::clone(&bytes),
        );
        assert_eq!(generated.asset.digest, digest_asset(&bytes));
        assert_eq!(generated.bytes.as_ref(), bytes.as_ref());
        let error = validate_generated_assets(
            &content.posts[0],
            assets.assets_for(&content.posts[0]).unwrap(),
            &[generated],
        )
        .unwrap_err();
        assert_eq!(error.code, MarkdownRenderErrorCode::GeneratedAssetCollision);
    }

    #[test]
    fn render_error_wire_contract_uses_typed_codes_and_ordinals() {
        let error = render_error("[bad](//example.com)");
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            serde_json::json!({
                "path": "posts/rendered.md",
                "location": {
                    "kind": "destination",
                    "ordinal": 1,
                    "destination_kind": "link"
                },
                "code": "navigation_rejected",
                "navigation_rejection": "protocol_relative",
                "message": "link destination was rejected: ProtocolRelative"
            })
        );
    }

    #[test]
    fn renderer_wire_types_and_ordinals_have_stable_contracts() {
        let destination_ordinal = MarkdownDestinationOrdinal::new(NonZeroUsize::new(2).unwrap());
        let code_ordinal = CodeBlockOrdinal::new(NonZeroUsize::new(3).unwrap());
        let mermaid_ordinal = MermaidBlockOrdinal::from_index(3).unwrap();
        assert_eq!(serde_json::to_value(destination_ordinal).unwrap(), 2);
        assert_eq!(serde_json::to_value(code_ordinal).unwrap(), 3);
        assert_eq!(serde_json::to_value(mermaid_ordinal).unwrap(), 4);

        for (value, expected) in [
            (RenderDestinationKind::Link, "link"),
            (RenderDestinationKind::Image, "image"),
        ] {
            assert_eq!(serde_json::to_value(value).unwrap(), expected);
        }

        assert_eq!(
            serde_json::to_value(MarkdownRenderLocation::Document).unwrap(),
            serde_json::json!({ "kind": "document" })
        );
        assert_eq!(
            serde_json::to_value(MarkdownRenderLocation::Destination {
                ordinal: destination_ordinal,
                destination_kind: RenderDestinationKind::Image,
            })
            .unwrap(),
            serde_json::json!({
                "kind": "destination",
                "ordinal": 2,
                "destination_kind": "image"
            })
        );
        assert_eq!(
            serde_json::to_value(MarkdownRenderLocation::CodeBlock {
                ordinal: code_ordinal,
            })
            .unwrap(),
            serde_json::json!({ "kind": "code_block", "ordinal": 3 })
        );
        assert_eq!(
            serde_json::to_value(MarkdownRenderLocation::GeneratedAsset).unwrap(),
            serde_json::json!({ "kind": "generated_asset" })
        );

        for (value, expected) in [
            (NavigationRejection::Empty, "empty"),
            (NavigationRejection::Whitespace, "whitespace"),
            (
                NavigationRejection::ForbiddenCharacter,
                "forbidden_character",
            ),
            (NavigationRejection::ProtocolRelative, "protocol_relative"),
            (NavigationRejection::UnsupportedScheme, "unsupported_scheme"),
            (NavigationRejection::MissingAuthority, "missing_authority"),
            (NavigationRejection::Credentials, "credentials"),
            (NavigationRejection::Traversal, "traversal"),
            (NavigationRejection::NotRootRelative, "not_root_relative"),
            (NavigationRejection::Malformed, "malformed"),
        ] {
            assert_eq!(serde_json::to_value(value).unwrap(), expected);
        }

        for (value, expected) in [
            (
                MarkdownRenderErrorCode::DestinationCountExceeded,
                "destination_count_exceeded",
            ),
            (
                MarkdownRenderErrorCode::AssetOccurrenceMissing,
                "asset_occurrence_missing",
            ),
            (
                MarkdownRenderErrorCode::AssetOccurrenceMismatch,
                "asset_occurrence_mismatch",
            ),
            (
                MarkdownRenderErrorCode::AssetOccurrenceUnused,
                "asset_occurrence_unused",
            ),
            (
                MarkdownRenderErrorCode::NavigationRejected,
                "navigation_rejected",
            ),
            (
                MarkdownRenderErrorCode::MermaidBlockCountExceeded,
                "mermaid_block_count_exceeded",
            ),
            (
                MarkdownRenderErrorCode::MermaidBlockTooLarge,
                "mermaid_block_too_large",
            ),
            (
                MarkdownRenderErrorCode::RenderedHtmlTooLarge,
                "rendered_html_too_large",
            ),
            (
                MarkdownRenderErrorCode::UnsupportedCommonMarkEvent,
                "unsupported_common_mark_event",
            ),
            (
                MarkdownRenderErrorCode::MalformedCommonMarkEvents,
                "malformed_common_mark_events",
            ),
            (
                MarkdownRenderErrorCode::GeneratedAssetCollision,
                "generated_asset_collision",
            ),
            (
                MarkdownRenderErrorCode::AssetPolicyMismatch,
                "asset_policy_mismatch",
            ),
            (
                MarkdownRenderErrorCode::LocalAssetMissing,
                "local_asset_missing",
            ),
            (
                MarkdownRenderErrorCode::LocalAssetDigestMismatch,
                "local_asset_digest_mismatch",
            ),
            (
                MarkdownRenderErrorCode::RevisionIdentityRejected,
                "revision_identity_rejected",
            ),
        ] {
            assert_eq!(serde_json::to_value(value).unwrap(), expected);
        }
    }
}
