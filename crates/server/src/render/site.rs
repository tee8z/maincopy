use std::{
    collections::BTreeMap,
    fmt::{self, Write as _},
    sync::Arc,
};

use arc_swap::ArcSwap;
use markdown_compiler::identity::{
    PreviewDigestInput, PublishedPostIdentityInput, SiteShellOutputDigest, SiteShellOutputHasher,
    finalize_preview_digest, finalize_site_snapshot,
};
use markdown_compiler::{
    AssetRevisionReference, DefaultPostTipPolicy, DigestedAsset, DraftStatus, LogicalAssetPath,
    PostDescription, PostId, PostRevisionDigest, PostSlug, PostTag, PostTipPolicy, PostTitle,
    PreviewDigest, PublicationSettings, ResolvedLocalAssetStore, ResolvedPostAssets,
    RevisionIdentityError, SiteShellRendererIdentity, SiteSnapshotDigest,
};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use qrcode::{QrCode, types::Color};
use serde::Serialize;
use thiserror::Error;
use time::OffsetDateTime;

use crate::domain::profile::TipRecipientProjection;
use crate::domain::publication::{
    CanonicalSiteUrl, PublicLedgerProjection, PublicPagePath, PublishedPostRevision,
};
use crate::frontend_assets::FrontendAssetManifest;

use super::robots::{RenderedRobots, RobotsRenderError, render_robots};
use super::rss::{RenderedRssFeed, RssItem, RssRenderError, render_rss};
use super::sitemap::{RenderedSitemap, SitemapRenderError, render_sitemap};
use super::{ContentCatalog, GeneratedPostAsset, RenderedPost, SnapshotAssetPath};

const MAX_PAGE_BYTES: usize = 40 * 1024 * 1024;
const MAX_PUBLIC_ROUTES: usize = 50_000;
const MAX_RETAINED_HTML_BYTES: usize = 512 * 1024 * 1024;
// Index, archive, feed, robots, sitemap, and the two rendered fallback pages.
const FIXED_PUBLIC_ROUTES: usize = 7;

#[cfg(test)]
fn render_post_preview(
    catalog: &ContentCatalog,
    frontend: &'static FrontendAssetManifest,
    post_id: &PostId,
    preview_asset_endpoint: &str,
    published_at: Option<OffsetDateTime>,
) -> Result<Option<String>, SiteSnapshotBuildError> {
    render_bound_post_preview(
        catalog,
        frontend,
        post_id,
        None,
        preview_asset_endpoint,
        published_at,
    )
    .map(|preview| preview.map(|preview| preview.html))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PublicPostView {
    post_id: PostId,
    revision: PostRevisionDigest,
    title: PostTitle,
    slug: PostSlug,
    description: PostDescription,
    tags: Arc<[PostTag]>,
    authored_at: OffsetDateTime,
    updated_at: Option<OffsetDateTime>,
    published_at: OffsetDateTime,
    canonical_url: CanonicalSiteUrl,
    tips: PostTipPolicy,
}

impl PublicPostView {
    fn from_rendered(
        rendered: &RenderedPost,
        entry: &PublishedPostRevision,
        publication: &PublicationSettings,
    ) -> Self {
        let metadata = &rendered.document.metadata;
        let path = PublicPagePath::post(&metadata.slug);
        Self {
            post_id: metadata.id.clone(),
            revision: rendered.revision.clone(),
            title: metadata.title.clone(),
            slug: metadata.slug.clone(),
            description: metadata.description.clone(),
            tags: Arc::from(metadata.tags.as_slice()),
            authored_at: metadata.authored_at,
            updated_at: metadata.updated_at,
            published_at: entry.published_at,
            canonical_url: CanonicalSiteUrl::for_path(&publication.site.base_url, &path),
            tips: metadata.tips,
        }
    }

    fn public_path(&self) -> PublicPagePath {
        PublicPagePath::post(&self.slug)
    }
}

/// An opaque, candidate-bound site rendering capability.
///
/// It owns the exact catalog and binds the publication, site-asset policy,
/// frontend bundle, and public ledger used to produce its shell plan.
pub struct RenderedSiteShell {
    catalog: Arc<ContentCatalog>,
    frontend: &'static FrontendAssetManifest,
    ledger: PublicLedgerProjection,
    renderer: SiteShellRendererIdentity,
    posts: Arc<[PublicPostView]>,
    chronology: Arc<[usize]>,
    tags: BTreeMap<PostTag, Arc<[usize]>>,
    feed: RenderedRssFeed,
    robots: RenderedRobots,
    sitemap: RenderedSitemap,
    pre_injection_output: SiteShellOutputDigest,
    tip_recipient: Option<TipRecipientProjection>,
}

impl fmt::Debug for RenderedSiteShell {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RenderedSiteShell")
            .field("ledger_entries", &self.ledger.len())
            .field("posts", &self.posts.len())
            .field("tags", &self.tags.len())
            .field("feed_digest", &self.feed.digest)
            .field("robots_digest", &self.robots.digest)
            .field("sitemap_digest", &self.sitemap.digest)
            .finish_non_exhaustive()
    }
}

pub fn render_site_shell(
    catalog: Arc<ContentCatalog>,
    frontend: &'static FrontendAssetManifest,
    ledger: &PublicLedgerProjection,
) -> Result<RenderedSiteShell, SiteSnapshotBuildError> {
    frontend
        .validate()
        .map_err(|error| SiteSnapshotBuildError::frontend(error.to_string()))?;
    let posts = select_public_posts(&catalog, ledger)?;
    let chronology = chronology(&posts);
    let tags = tag_index(&posts, &chronology);
    validate_route_count(posts.len(), tags.len())?;
    let feed = render_public_feed(&catalog.publication, &posts, &chronology)?;
    let sitemap = render_public_sitemap(&catalog.publication, &posts, &tags)?;
    let robots = render_public_robots(&catalog.publication)?;

    let renderer = SiteShellRendererIdentity::new(*frontend.bundle_digest.as_bytes());
    let pre_injection_output = render_pre_injection_shell(
        &catalog.publication,
        frontend,
        &posts,
        &chronology,
        &tags,
        DiscoveryDocuments {
            feed: &feed,
            robots: &robots,
            sitemap: &sitemap,
        },
    )?;

    Ok(RenderedSiteShell {
        catalog,
        frontend,
        ledger: ledger.clone(),
        renderer,
        posts: posts.into(),
        chronology: chronology.into(),
        tags,
        feed,
        robots,
        sitemap,
        pre_injection_output,
        tip_recipient: None,
    })
}

impl RenderedSiteShell {
    pub(crate) fn bind_tip_recipient(mut self, recipient: Option<TipRecipientProjection>) -> Self {
        self.tip_recipient = recipient;
        self
    }
}

/// One rendered private document and the exact presentation binding it exposes for approval.
pub(crate) struct BoundPostPreview {
    pub(crate) html: String,
    pub(crate) digest: PreviewDigest,
    pub(crate) revision: PostRevisionDigest,
    pub(crate) canonical_url: CanonicalSiteUrl,
}

/// Renders and binds one current candidate without including its future activation time.
pub(crate) fn render_bound_post_preview(
    catalog: &ContentCatalog,
    frontend: &'static FrontendAssetManifest,
    post_id: &PostId,
    tip_recipient: Option<&TipRecipientProjection>,
    preview_asset_endpoint: &str,
    published_at: Option<OffsetDateTime>,
) -> Result<Option<BoundPostPreview>, SiteSnapshotBuildError> {
    let Some(rendered) = catalog.current_post(post_id) else {
        return Ok(None);
    };
    render_bound_preview(
        catalog,
        frontend,
        rendered,
        tip_recipient,
        catalog.local_assets.as_ref(),
        preview_asset_endpoint,
        published_at,
    )
    .map(Some)
}

/// Reproduces the approval binding for one exact retained post revision.
pub(crate) fn render_bound_post_revision_preview(
    catalog: &ContentCatalog,
    frontend: &'static FrontendAssetManifest,
    post_id: &PostId,
    revision: &PostRevisionDigest,
    tip_recipient: Option<&TipRecipientProjection>,
    preview_asset_endpoint: &str,
    published_at: Option<OffsetDateTime>,
) -> Result<Option<BoundPostPreview>, SiteSnapshotBuildError> {
    let Some((rendered, local_assets)) = catalog.get_with_local_assets(post_id, revision) else {
        return Ok(None);
    };
    render_bound_preview(
        catalog,
        frontend,
        rendered,
        tip_recipient,
        local_assets,
        preview_asset_endpoint,
        published_at,
    )
    .map(Some)
}

fn render_bound_preview(
    catalog: &ContentCatalog,
    frontend: &'static FrontendAssetManifest,
    rendered: &RenderedPost,
    tip_recipient: Option<&TipRecipientProjection>,
    local_assets: &ResolvedLocalAssetStore,
    preview_asset_endpoint: &str,
    published_at: Option<OffsetDateTime>,
) -> Result<BoundPostPreview, SiteSnapshotBuildError> {
    frontend
        .validate()
        .map_err(|error| SiteSnapshotBuildError::frontend(error.to_string()))?;
    let post_id = &rendered.document.metadata.id;
    let article = rendered
        .project_for_preview(preview_asset_endpoint, &catalog.site_assets, local_assets)
        .map(ProjectedArticleHtml::new)
        .map_err(|error| {
            SiteSnapshotBuildError::post(
                SiteSnapshotBuildErrorCode::ArticleProjectionFailed,
                post_id.clone(),
                error.to_string(),
            )
        })?;
    let page = PostPageView::from_rendered(rendered, published_at);
    let tips_enabled = page.tips_enabled(&catalog.publication);
    let tip_handoff = if tips_enabled {
        tip_recipient.map(TipHandoff::new).transpose()?
    } else {
        None
    };
    let html = render_post(
        &catalog.publication,
        frontend,
        page,
        ArticleBody::Projected(&article),
        tip_handoff.as_ref(),
    )
    .into_string();
    validate_page_size(html.len())?;
    let pre_injection_shell = render_post(
        &catalog.publication,
        frontend,
        PostPageView::from_rendered(rendered, None),
        ArticleBody::Omitted,
        None,
    )
    .into_string();
    validate_page_size(pre_injection_shell.len())?;
    let canonical_url = CanonicalSiteUrl::for_path(
        &catalog.publication.site.base_url,
        &PublicPagePath::post(&rendered.document.metadata.slug),
    );
    let renderer = SiteShellRendererIdentity::new(*frontend.bundle_digest.as_bytes());
    let profile_projection = if tips_enabled {
        tip_recipient
            .map(TipRecipientProjection::identity_bytes)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let digest = finalize_preview_digest(PreviewDigestInput {
        publication: &catalog.publication,
        site_assets: &catalog.site_assets,
        post_id,
        post_revision: &rendered.revision,
        post_renderer: &rendered.renderer,
        article_identity_html: rendered.article.identity_html.as_bytes(),
        site_renderer: &renderer,
        pre_injection_post_shell: pre_injection_shell.as_bytes(),
        profile_projection: &profile_projection,
        canonical_url: canonical_url.as_str(),
    })
    .map_err(SiteSnapshotBuildError::identity)?;
    Ok(BoundPostPreview {
        html,
        digest,
        revision: rendered.revision.clone(),
        canonical_url,
    })
}

fn select_public_posts(
    catalog: &ContentCatalog,
    ledger: &PublicLedgerProjection,
) -> Result<Vec<PublicPostView>, SiteSnapshotBuildError> {
    let mut posts = Vec::with_capacity(ledger.len());
    for entry in ledger.published_posts() {
        let Some(rendered) = catalog.get(&entry.post_id, &entry.revision) else {
            return Err(SiteSnapshotBuildError::post(
                SiteSnapshotBuildErrorCode::RevisionUnavailable,
                entry.post_id.clone(),
                "the exact public-ledger revision is not available in this catalog",
            ));
        };
        if rendered.document.metadata.draft == DraftStatus::Draft {
            return Err(SiteSnapshotBuildError::post(
                SiteSnapshotBuildErrorCode::DraftSelected,
                entry.post_id.clone(),
                "the public ledger selected a draft revision",
            ));
        }
        posts.push(PublicPostView::from_rendered(
            rendered,
            entry,
            &catalog.publication,
        ));
    }
    Ok(posts)
}

fn chronology(posts: &[PublicPostView]) -> Vec<usize> {
    let mut chronology: Vec<_> = (0..posts.len()).collect();
    chronology.sort_by(|left, right| {
        posts[*right]
            .published_at
            .cmp(&posts[*left].published_at)
            .then_with(|| posts[*left].post_id.cmp(&posts[*right].post_id))
    });
    chronology
}

fn tag_index(posts: &[PublicPostView], chronology: &[usize]) -> BTreeMap<PostTag, Arc<[usize]>> {
    let mut tags: BTreeMap<PostTag, Vec<usize>> = BTreeMap::new();
    for post_index in chronology {
        for tag in &*posts[*post_index].tags {
            tags.entry(tag.clone()).or_default().push(*post_index);
        }
    }
    tags.into_iter()
        .map(|(tag, posts)| (tag, posts.into()))
        .collect()
}

fn render_public_feed(
    publication: &PublicationSettings,
    posts: &[PublicPostView],
    chronology: &[usize],
) -> Result<RenderedRssFeed, SiteSnapshotBuildError> {
    let feed_url = CanonicalSiteUrl::for_path(&publication.site.base_url, &PublicPagePath::feed());
    render_rss(
        publication,
        &feed_url,
        chronology.iter().map(|index| {
            let post = &posts[*index];
            RssItem {
                post_id: &post.post_id,
                title: &post.title,
                description: &post.description,
                canonical_url: &post.canonical_url,
                published_at: post.published_at,
            }
        }),
    )
    .map_err(SiteSnapshotBuildError::rss)
}

fn render_public_sitemap(
    publication: &PublicationSettings,
    posts: &[PublicPostView],
    tags: &BTreeMap<PostTag, Arc<[usize]>>,
) -> Result<RenderedSitemap, SiteSnapshotBuildError> {
    let locations: Vec<_> = std::iter::once(PublicPagePath::index())
        .chain(std::iter::once(PublicPagePath::archive()))
        .chain(posts.iter().map(PublicPostView::public_path))
        .chain(tags.keys().map(PublicPagePath::tag))
        .map(|path| CanonicalSiteUrl::for_path(&publication.site.base_url, &path))
        .collect();
    render_sitemap(&locations).map_err(SiteSnapshotBuildError::sitemap)
}

fn render_public_robots(
    publication: &PublicationSettings,
) -> Result<RenderedRobots, SiteSnapshotBuildError> {
    let sitemap_url =
        CanonicalSiteUrl::for_path(&publication.site.base_url, &PublicPagePath::sitemap());
    render_robots(&sitemap_url).map_err(SiteSnapshotBuildError::robots)
}

fn validate_route_count(posts: usize, tags: usize) -> Result<(), SiteSnapshotBuildError> {
    let routes = posts
        .checked_add(tags)
        .and_then(|count| count.checked_add(FIXED_PUBLIC_ROUTES))
        .ok_or_else(SiteSnapshotBuildError::route_limit)?;
    if routes > MAX_PUBLIC_ROUTES {
        return Err(SiteSnapshotBuildError::route_limit());
    }
    Ok(())
}

fn render_pre_injection_shell(
    publication: &PublicationSettings,
    frontend: &'static FrontendAssetManifest,
    posts: &[PublicPostView],
    chronology: &[usize],
    tags: &BTreeMap<PostTag, Arc<[usize]>>,
    discovery: DiscoveryDocuments<'_>,
) -> Result<SiteShellOutputDigest, SiteSnapshotBuildError> {
    let mut pages = BTreeMap::new();
    pages.insert(PublicPagePath::index(), PreInjectionPage::Index);
    pages.insert(PublicPagePath::archive(), PreInjectionPage::Archive);
    pages.insert(PublicPagePath::feed(), PreInjectionPage::Feed);
    pages.insert(PublicPagePath::robots(), PreInjectionPage::Robots);
    pages.insert(PublicPagePath::sitemap(), PreInjectionPage::Sitemap);
    for (index, post) in posts.iter().enumerate() {
        pages.insert(post.public_path(), PreInjectionPage::Post(index));
    }
    for tag in tags.keys() {
        pages.insert(PublicPagePath::tag(tag), PreInjectionPage::Tag(tag));
    }
    pages.insert(
        PublicPagePath::error_identity_marker("not-found"),
        PreInjectionPage::Error(PublicErrorPage::NotFound),
    );
    pages.insert(
        PublicPagePath::error_identity_marker("method-not-allowed"),
        PreInjectionPage::Error(PublicErrorPage::MethodNotAllowed),
    );

    let mut retained = RetainedHtmlBudget::new();
    let mut hasher = SiteShellOutputHasher::new(pages.len());
    for (path, page) in pages {
        match page {
            PreInjectionPage::Index => hash_pre_injection_html(
                &mut hasher,
                &mut retained,
                &path,
                render_index(publication, frontend, posts, chronology).into_string(),
            ),
            PreInjectionPage::Archive => hash_pre_injection_html(
                &mut hasher,
                &mut retained,
                &path,
                render_archive(publication, frontend, posts, chronology).into_string(),
            ),
            PreInjectionPage::Feed => {
                hasher.page(path.as_str(), discovery.feed.body.as_bytes());
                Ok(())
            }
            PreInjectionPage::Robots => {
                hasher.page(path.as_str(), discovery.robots.body.as_bytes());
                Ok(())
            }
            PreInjectionPage::Sitemap => {
                hasher.page(path.as_str(), discovery.sitemap.body.as_bytes());
                Ok(())
            }
            PreInjectionPage::Post(index) => hash_pre_injection_html(
                &mut hasher,
                &mut retained,
                &path,
                render_post(
                    publication,
                    frontend,
                    PostPageView::from_public(&posts[index]),
                    ArticleBody::Omitted,
                    None,
                )
                .into_string(),
            ),
            PreInjectionPage::Tag(tag) => hash_pre_injection_html(
                &mut hasher,
                &mut retained,
                &path,
                render_tag(
                    publication,
                    frontend,
                    tag,
                    posts,
                    tags.get(tag).map_or(&[], Arc::as_ref),
                )
                .into_string(),
            ),
            PreInjectionPage::Error(error) => hash_pre_injection_html(
                &mut hasher,
                &mut retained,
                &path,
                render_error(publication, frontend, error).into_string(),
            ),
        }?;
    }
    Ok(hasher.finish())
}

#[derive(Clone, Copy)]
struct DiscoveryDocuments<'documents> {
    feed: &'documents RenderedRssFeed,
    robots: &'documents RenderedRobots,
    sitemap: &'documents RenderedSitemap,
}

fn hash_pre_injection_html(
    hasher: &mut SiteShellOutputHasher,
    retained: &mut RetainedHtmlBudget,
    path: &PublicPagePath,
    html: String,
) -> Result<(), SiteSnapshotBuildError> {
    validate_page_size(html.len())?;
    retained.add(html.len())?;
    hasher.page(path.as_str(), html.as_bytes());
    Ok(())
}

#[derive(Clone, Copy)]
enum PreInjectionPage<'view> {
    Index,
    Archive,
    Feed,
    Robots,
    Sitemap,
    Post(usize),
    Tag(&'view PostTag),
    Error(PublicErrorPage),
}

pub fn build_site_snapshot(
    shell: RenderedSiteShell,
    ledger: &PublicLedgerProjection,
) -> Result<SiteSnapshot, SiteSnapshotBuildError> {
    validate_snapshot_shell(&shell, ledger)?;
    let public_posts: Vec<_> = ledger
        .published_posts()
        .map(|published| {
            PublishedPostIdentityInput::new(
                &published.post_id,
                &published.revision,
                published.published_at,
            )
        })
        .collect();
    let digest = finalize_site_snapshot(
        &shell.catalog.publication,
        &shell.catalog.site_assets,
        &shell.renderer,
        &shell.pre_injection_output,
        &public_posts,
    )
    .map_err(SiteSnapshotBuildError::identity)?;

    let mut retained = RetainedHtmlBudget::new();
    let tip_handoff = shell
        .tip_recipient
        .as_ref()
        .map(TipHandoff::new)
        .transpose()?;
    let pages = render_snapshot_pages(&shell, &digest, tip_handoff.as_ref(), &mut retained)?;
    let publication = &shell.catalog.publication;
    let not_found = rendered_error_page(
        publication,
        shell.frontend,
        PublicErrorPage::NotFound,
        &mut retained,
    )?;
    let method_not_allowed = rendered_error_page(
        publication,
        shell.frontend,
        PublicErrorPage::MethodNotAllowed,
        &mut retained,
    )?;
    let feed = shell.feed.clone();
    let robots = shell.robots.clone();
    let sitemap = shell.sitemap.clone();
    let assets = collect_public_assets(&shell, &digest)?;
    let presentation_digest = presentation_digest(
        &pages,
        &not_found,
        &method_not_allowed,
        &feed,
        &robots,
        &sitemap,
    );

    Ok(SiteSnapshot {
        digest,
        presentation_digest,
        feed,
        robots,
        sitemap,
        pages,
        not_found,
        method_not_allowed,
        assets,
        frontend: shell.frontend,
        retained_html_bytes: retained.used,
    })
}

fn validate_snapshot_shell(
    shell: &RenderedSiteShell,
    ledger: &PublicLedgerProjection,
) -> Result<(), SiteSnapshotBuildError> {
    if &shell.ledger != ledger {
        return Err(SiteSnapshotBuildError::new(
            SiteSnapshotBuildErrorCode::LedgerMismatch,
            None,
            "the site shell belongs to a different public-ledger projection",
        ));
    }
    shell
        .frontend
        .validate()
        .map_err(|error| SiteSnapshotBuildError::frontend(error.to_string()))?;
    if shell.renderer.frontend_bundle != *shell.frontend.bundle_digest.as_bytes() {
        return Err(SiteSnapshotBuildError::new(
            SiteSnapshotBuildErrorCode::FrontendMismatch,
            None,
            "the site shell belongs to a different frontend bundle",
        ));
    }

    let selected = select_public_posts(&shell.catalog, ledger)?;
    if selected.as_slice() != shell.posts.as_ref() {
        return Err(SiteSnapshotBuildError::new(
            SiteSnapshotBuildErrorCode::SourceBindingMismatch,
            None,
            "the site shell does not match its bound content catalog",
        ));
    }
    Ok(())
}

fn render_snapshot_pages(
    shell: &RenderedSiteShell,
    digest: &SiteSnapshotDigest,
    tip_handoff: Option<&TipHandoff<'_>>,
    retained: &mut RetainedHtmlBudget,
) -> Result<BTreeMap<PageRoute, RenderedPage>, SiteSnapshotBuildError> {
    let publication = &shell.catalog.publication;
    let mut pages = BTreeMap::new();

    insert_page(
        &mut pages,
        PageRoute::Index,
        render_index(publication, shell.frontend, &shell.posts, &shell.chronology).into_string(),
        publication,
        retained,
    )?;
    insert_page(
        &mut pages,
        PageRoute::Archive,
        render_archive(publication, shell.frontend, &shell.posts, &shell.chronology).into_string(),
        publication,
        retained,
    )?;

    for post in &*shell.posts {
        let (rendered, local_assets) = shell
            .catalog
            .get_with_local_assets(&post.post_id, &post.revision)
            .ok_or_else(|| {
                SiteSnapshotBuildError::post(
                    SiteSnapshotBuildErrorCode::RevisionUnavailable,
                    post.post_id.clone(),
                    "the bound post revision disappeared before snapshot projection",
                )
            })?;
        let article = rendered
            .project_for_snapshot(digest, &shell.catalog.site_assets, local_assets)
            .map(ProjectedArticleHtml::new)
            .map_err(|error| {
                SiteSnapshotBuildError::post(
                    SiteSnapshotBuildErrorCode::ArticleProjectionFailed,
                    post.post_id.clone(),
                    error.to_string(),
                )
            })?;
        insert_page(
            &mut pages,
            PageRoute::Post(post.slug.clone()),
            render_post(
                publication,
                shell.frontend,
                PostPageView::from_public(post),
                ArticleBody::Projected(&article),
                tip_handoff,
            )
            .into_string(),
            publication,
            retained,
        )?;
    }
    for (tag, indexes) in &shell.tags {
        insert_page(
            &mut pages,
            PageRoute::Tag(tag.clone()),
            render_tag(publication, shell.frontend, tag, &shell.posts, indexes).into_string(),
            publication,
            retained,
        )?;
    }
    Ok(pages)
}

fn insert_page(
    pages: &mut BTreeMap<PageRoute, RenderedPage>,
    route: PageRoute,
    html: String,
    publication: &PublicationSettings,
    retained: &mut RetainedHtmlBudget,
) -> Result<(), SiteSnapshotBuildError> {
    validate_page_size(html.len())?;
    retained.add(html.len())?;
    let path = route.public_path();
    let page = RenderedPage {
        html: html.into(),
        canonical_url: CanonicalSiteUrl::for_path(&publication.site.base_url, &path),
    };
    if pages.insert(route, page).is_some() {
        return Err(SiteSnapshotBuildError::new(
            SiteSnapshotBuildErrorCode::RouteCollision,
            None,
            "two public pages resolved to the same typed route",
        ));
    }
    Ok(())
}

fn rendered_error_page(
    publication: &PublicationSettings,
    frontend: &'static FrontendAssetManifest,
    error: PublicErrorPage,
    retained: &mut RetainedHtmlBudget,
) -> Result<RenderedPage, SiteSnapshotBuildError> {
    let html = render_error(publication, frontend, error).into_string();
    validate_page_size(html.len())?;
    retained.add(html.len())?;
    Ok(RenderedPage {
        html: html.into(),
        canonical_url: CanonicalSiteUrl::for_path(
            &publication.site.base_url,
            &PublicPagePath::index(),
        ),
    })
}

fn validate_page_size(bytes: usize) -> Result<(), SiteSnapshotBuildError> {
    if bytes > MAX_PAGE_BYTES {
        return Err(SiteSnapshotBuildError::new(
            SiteSnapshotBuildErrorCode::PageLimitExceeded,
            None,
            format!("rendered page exceeds the inclusive {MAX_PAGE_BYTES}-byte limit"),
        ));
    }
    Ok(())
}

struct RetainedHtmlBudget {
    used: usize,
}

impl RetainedHtmlBudget {
    const fn new() -> Self {
        Self { used: 0 }
    }

    fn add(&mut self, bytes: usize) -> Result<(), SiteSnapshotBuildError> {
        let Some(next) = self.used.checked_add(bytes) else {
            return Err(SiteSnapshotBuildError::retained_html_limit());
        };
        if next > MAX_RETAINED_HTML_BYTES {
            return Err(SiteSnapshotBuildError::retained_html_limit());
        }
        self.used = next;
        Ok(())
    }
}

fn collect_public_assets(
    shell: &RenderedSiteShell,
    digest: &SiteSnapshotDigest,
) -> Result<BTreeMap<SnapshotAssetPath, SnapshotPublicAsset>, SiteSnapshotBuildError> {
    let mut selected: BTreeMap<LogicalAssetPath, SelectedAsset> = BTreeMap::new();
    collect_site_global_assets(
        &mut selected,
        &shell.catalog.site_assets,
        &shell.catalog.local_assets,
    )?;

    for post in &*shell.posts {
        let (rendered, local_assets) = shell
            .catalog
            .get_with_local_assets(&post.post_id, &post.revision)
            .ok_or_else(|| {
                SiteSnapshotBuildError::post(
                    SiteSnapshotBuildErrorCode::RevisionUnavailable,
                    post.post_id.clone(),
                    "the selected post revision is unavailable while collecting assets",
                )
            })?;
        collect_selected_post_assets(
            &mut selected,
            &rendered.assets,
            &rendered.generated_assets,
            local_assets,
        )?;
    }

    materialize_public_assets(selected, digest)
}

fn collect_site_global_assets(
    selected: &mut BTreeMap<LogicalAssetPath, SelectedAsset>,
    site_assets: &markdown_compiler::ResolvedSiteAssets,
    local_assets: &ResolvedLocalAssetStore,
) -> Result<(), SiteSnapshotBuildError> {
    if let Some(AssetRevisionReference::Local(asset)) = &site_assets.favicon {
        insert_authored_asset(selected, asset, local_assets)?;
    }
    for reference in &site_assets.references {
        if let AssetRevisionReference::Local(asset) = reference {
            insert_authored_asset(selected, asset, local_assets)?;
        }
    }
    Ok(())
}

fn collect_selected_post_assets(
    selected: &mut BTreeMap<LogicalAssetPath, SelectedAsset>,
    assets: &ResolvedPostAssets,
    generated_assets: &[GeneratedPostAsset],
    store: &ResolvedLocalAssetStore,
) -> Result<(), SiteSnapshotBuildError> {
    if let Some(AssetRevisionReference::Local(asset)) = &assets.image {
        insert_authored_asset(selected, asset, store)?;
    }
    for reference in &assets.references {
        if let AssetRevisionReference::Local(asset) = reference {
            insert_authored_asset(selected, asset, store)?;
        }
    }
    for generated in generated_assets {
        insert_selected_asset(
            selected,
            generated.asset.clone(),
            Arc::clone(&generated.bytes),
        )?;
    }
    Ok(())
}

fn materialize_public_assets(
    selected: BTreeMap<LogicalAssetPath, SelectedAsset>,
    digest: &SiteSnapshotDigest,
) -> Result<BTreeMap<SnapshotAssetPath, SnapshotPublicAsset>, SiteSnapshotBuildError> {
    let mut assets = BTreeMap::new();
    for selected in selected.into_values() {
        let path = SnapshotAssetPath::new(digest, &selected.asset.path).map_err(|error| {
            SiteSnapshotBuildError::new(
                SiteSnapshotBuildErrorCode::AssetUnavailable,
                None,
                error.to_string(),
            )
        })?;
        let public = SnapshotPublicAsset {
            path: path.clone(),
            asset: selected.asset,
            bytes: selected.bytes,
        };
        if assets.insert(path, public).is_some() {
            return Err(SiteSnapshotBuildError::new(
                SiteSnapshotBuildErrorCode::AssetCollision,
                None,
                "two selected assets resolved to the same immutable public path",
            ));
        }
    }
    Ok(assets)
}

struct SelectedAsset {
    asset: DigestedAsset,
    bytes: Arc<[u8]>,
}

fn insert_authored_asset(
    selected: &mut BTreeMap<LogicalAssetPath, SelectedAsset>,
    asset: &DigestedAsset,
    store: &ResolvedLocalAssetStore,
) -> Result<(), SiteSnapshotBuildError> {
    let resolved = store.resolve(asset).map_err(|error| {
        SiteSnapshotBuildError::new(
            SiteSnapshotBuildErrorCode::AssetUnavailable,
            None,
            error.to_string(),
        )
    })?;
    insert_selected_asset(selected, asset.clone(), Arc::clone(&resolved.bytes))
}

fn insert_selected_asset(
    selected: &mut BTreeMap<LogicalAssetPath, SelectedAsset>,
    asset: DigestedAsset,
    bytes: Arc<[u8]>,
) -> Result<(), SiteSnapshotBuildError> {
    if let Some(existing) = selected.get(&asset.path) {
        if existing.asset.digest == asset.digest && existing.bytes == bytes {
            return Ok(());
        }
        return Err(SiteSnapshotBuildError::new(
            SiteSnapshotBuildErrorCode::AssetCollision,
            None,
            "selected assets disagree about the bytes at one logical path",
        ));
    }
    selected.insert(asset.path.clone(), SelectedAsset { asset, bytes });
    Ok(())
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum PageRoute {
    Index,
    Post(PostSlug),
    Tag(PostTag),
    Archive,
}

impl PageRoute {
    fn public_path(&self) -> PublicPagePath {
        match self {
            Self::Index => PublicPagePath::index(),
            Self::Post(slug) => PublicPagePath::post(slug),
            Self::Tag(tag) => PublicPagePath::tag(tag),
            Self::Archive => PublicPagePath::archive(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RenderedPage {
    html: Arc<str>,
    canonical_url: CanonicalSiteUrl,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PresentationDigest([u8; 32]);

impl fmt::Display for PresentationDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "presentation-b3-v1-{}",
            blake3::Hash::from_bytes(self.0).to_hex()
        )
    }
}

fn presentation_digest(
    pages: &BTreeMap<PageRoute, RenderedPage>,
    not_found: &RenderedPage,
    method_not_allowed: &RenderedPage,
    feed: &RenderedRssFeed,
    robots: &RenderedRobots,
    sitemap: &RenderedSitemap,
) -> PresentationDigest {
    let mut hasher = blake3::Hasher::new_derive_key("maincopy presentation snapshot v1");
    hash_presentation_part(&mut hasher, &(pages.len() as u64).to_be_bytes());
    for (route, page) in pages {
        hash_presentation_part(&mut hasher, route.public_path().as_str().as_bytes());
        hash_presentation_part(&mut hasher, page.html.as_bytes());
    }
    hash_presentation_part(&mut hasher, b"not-found");
    hash_presentation_part(&mut hasher, not_found.html.as_bytes());
    hash_presentation_part(&mut hasher, b"method-not-allowed");
    hash_presentation_part(&mut hasher, method_not_allowed.html.as_bytes());
    hash_presentation_part(&mut hasher, PublicPagePath::feed().as_str().as_bytes());
    hash_presentation_part(&mut hasher, feed.body.as_bytes());
    hash_presentation_part(&mut hasher, PublicPagePath::robots().as_str().as_bytes());
    hash_presentation_part(&mut hasher, robots.body.as_bytes());
    hash_presentation_part(&mut hasher, PublicPagePath::sitemap().as_str().as_bytes());
    hash_presentation_part(&mut hasher, sitemap.body.as_bytes());
    PresentationDigest(*hasher.finalize().as_bytes())
}

fn hash_presentation_part(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_be_bytes());
    hasher.update(value);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotPublicAsset {
    pub path: SnapshotAssetPath,
    pub asset: DigestedAsset,
    pub bytes: Arc<[u8]>,
}

/// Complete immutable request-facing state for one canonical publication.
pub struct SiteSnapshot {
    pub(crate) digest: SiteSnapshotDigest,
    pub(crate) presentation_digest: PresentationDigest,
    pub(crate) feed: RenderedRssFeed,
    pub(crate) robots: RenderedRobots,
    pub(crate) sitemap: RenderedSitemap,
    pages: BTreeMap<PageRoute, RenderedPage>,
    not_found: RenderedPage,
    method_not_allowed: RenderedPage,
    assets: BTreeMap<SnapshotAssetPath, SnapshotPublicAsset>,
    pub(crate) frontend: &'static FrontendAssetManifest,
    retained_html_bytes: usize,
}

impl fmt::Debug for SiteSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SiteSnapshot")
            .field("digest", &self.digest)
            .field("presentation_digest", &self.presentation_digest)
            .field("feed_digest", &self.feed.digest)
            .field("robots_digest", &self.robots.digest)
            .field("sitemap_digest", &self.sitemap.digest)
            .field("pages", &self.pages.len())
            .field("assets", &self.assets.len())
            .field("retained_html_bytes", &self.retained_html_bytes)
            .finish_non_exhaustive()
    }
}

impl SiteSnapshot {
    pub fn post_canonical_url(&self, slug: &PostSlug) -> Option<&CanonicalSiteUrl> {
        self.pages
            .get(&PageRoute::Post(slug.clone()))
            .map(|page| &page.canonical_url)
    }

    pub fn public_assets(&self) -> impl ExactSizeIterator<Item = &SnapshotPublicAsset> {
        self.assets.values()
    }

    pub(crate) fn index_page(&self) -> Arc<str> {
        self.pages
            .get(&PageRoute::Index)
            .map_or_else(|| Arc::from(""), |page| Arc::clone(&page.html))
    }

    pub(crate) fn post_page(&self, slug: &PostSlug) -> Option<Arc<str>> {
        self.pages
            .get(&PageRoute::Post(slug.clone()))
            .map(|page| Arc::clone(&page.html))
    }

    pub(crate) fn tag_page(&self, tag: &PostTag) -> Option<Arc<str>> {
        self.pages
            .get(&PageRoute::Tag(tag.clone()))
            .map(|page| Arc::clone(&page.html))
    }

    pub(crate) fn archive_page(&self) -> Arc<str> {
        self.pages
            .get(&PageRoute::Archive)
            .map_or_else(|| Arc::from(""), |page| Arc::clone(&page.html))
    }

    pub(crate) fn not_found_page(&self) -> Arc<str> {
        Arc::clone(&self.not_found.html)
    }

    pub(crate) fn method_not_allowed_page(&self) -> Arc<str> {
        Arc::clone(&self.method_not_allowed.html)
    }
}

/// Cloneable read-only access to the currently active immutable snapshot.
#[derive(Clone)]
pub struct SiteSnapshotReader {
    active: Arc<ArcSwap<SiteSnapshot>>,
}

impl fmt::Debug for SiteSnapshotReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SiteSnapshotReader")
            .finish_non_exhaustive()
    }
}

impl SiteSnapshotReader {
    pub fn from_snapshot(snapshot: SiteSnapshot) -> Self {
        Self {
            active: Arc::new(ArcSwap::from_pointee(snapshot)),
        }
    }

    pub fn load_full(&self) -> Arc<SiteSnapshot> {
        self.active.load_full()
    }
}

pub(crate) struct SiteSnapshotActivator {
    active: Arc<ArcSwap<SiteSnapshot>>,
}

pub(crate) fn snapshot_store(initial: SiteSnapshot) -> (SiteSnapshotReader, SiteSnapshotActivator) {
    let active = Arc::new(ArcSwap::from_pointee(initial));
    (
        SiteSnapshotReader {
            active: Arc::clone(&active),
        },
        SiteSnapshotActivator { active },
    )
}

impl SiteSnapshotActivator {
    pub(crate) fn activate(
        &mut self,
        expected: &SiteSnapshotDigest,
        next: SiteSnapshot,
    ) -> Result<SnapshotActivationOutcome, SnapshotActivationError> {
        let current = self.active.load_full();
        if &current.digest != expected {
            return Err(SnapshotActivationError {
                expected: expected.clone(),
                actual: current.digest.clone(),
            });
        }
        if current.digest == next.digest && current.presentation_digest == next.presentation_digest
        {
            return Ok(SnapshotActivationOutcome::AlreadyActive);
        }
        self.active.store(Arc::new(next));
        Ok(SnapshotActivationOutcome::Activated)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SnapshotActivationOutcome {
    Activated,
    AlreadyActive,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("expected active snapshot {expected}, found {actual}")]
pub(crate) struct SnapshotActivationError {
    expected: SiteSnapshotDigest,
    actual: SiteSnapshotDigest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SiteSnapshotBuildErrorCode {
    FrontendManifestInvalid,
    FrontendMismatch,
    LedgerMismatch,
    RevisionUnavailable,
    DraftSelected,
    SourceBindingMismatch,
    RouteCollision,
    RouteLimitExceeded,
    PageLimitExceeded,
    RetainedHtmlLimitExceeded,
    AssetUnavailable,
    AssetCollision,
    ArticleProjectionFailed,
    RssRenderFailed,
    RobotsRenderFailed,
    SitemapRenderFailed,
    QrCodeGenerationFailed,
    IdentityRejected,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{code:?}: {message}")]
pub struct SiteSnapshotBuildError {
    pub code: SiteSnapshotBuildErrorCode,
    pub post_id: Option<PostId>,
    pub message: Box<str>,
}

impl SiteSnapshotBuildError {
    fn new(
        code: SiteSnapshotBuildErrorCode,
        post_id: Option<PostId>,
        message: impl Into<Box<str>>,
    ) -> Self {
        Self {
            code,
            post_id,
            message: message.into(),
        }
    }

    fn post(
        code: SiteSnapshotBuildErrorCode,
        post_id: PostId,
        message: impl Into<Box<str>>,
    ) -> Self {
        Self::new(code, Some(post_id), message)
    }

    fn frontend(message: impl Into<Box<str>>) -> Self {
        Self::new(
            SiteSnapshotBuildErrorCode::FrontendManifestInvalid,
            None,
            message,
        )
    }

    fn identity(error: RevisionIdentityError) -> Self {
        Self::new(
            SiteSnapshotBuildErrorCode::IdentityRejected,
            None,
            error.to_string(),
        )
    }

    fn qr_code(message: impl Into<Box<str>>) -> Self {
        Self::new(
            SiteSnapshotBuildErrorCode::QrCodeGenerationFailed,
            None,
            message,
        )
    }

    fn rss(error: RssRenderError) -> Self {
        let post_id = match &error {
            RssRenderError::IllegalXmlCharacter { post_id, .. } => post_id.clone(),
            RssRenderError::PublishedAtNotRepresentable { post_id, .. } => Some(post_id.clone()),
            RssRenderError::OutputTooLarge { .. } | RssRenderError::InvalidUtf8(_) => None,
        };
        Self::new(
            SiteSnapshotBuildErrorCode::RssRenderFailed,
            post_id,
            error.to_string(),
        )
    }

    fn sitemap(error: SitemapRenderError) -> Self {
        Self::new(
            SiteSnapshotBuildErrorCode::SitemapRenderFailed,
            None,
            error.to_string(),
        )
    }

    fn robots(error: RobotsRenderError) -> Self {
        Self::new(
            SiteSnapshotBuildErrorCode::RobotsRenderFailed,
            None,
            error.to_string(),
        )
    }

    fn route_limit() -> Self {
        Self::new(
            SiteSnapshotBuildErrorCode::RouteLimitExceeded,
            None,
            format!("site exceeds the inclusive {MAX_PUBLIC_ROUTES}-route limit"),
        )
    }

    fn retained_html_limit() -> Self {
        Self::new(
            SiteSnapshotBuildErrorCode::RetainedHtmlLimitExceeded,
            None,
            format!(
                "site exceeds the inclusive {MAX_RETAINED_HTML_BYTES}-byte retained HTML limit"
            ),
        )
    }
}

fn render_index(
    publication: &PublicationSettings,
    frontend: &'static FrontendAssetManifest,
    posts: &[PublicPostView],
    chronology: &[usize],
) -> Markup {
    let content = html! {
        section aria-labelledby="recent-posts-heading" {
            h1 id="recent-posts-heading" { "Recent posts" }
            @if chronology.is_empty() {
                p { "No posts have been published yet." }
            } @else {
                (render_post_list(posts, chronology))
            }
        }
    };
    render_layout(
        publication,
        frontend,
        publication.site.title.as_str(),
        content,
    )
}

fn render_archive(
    publication: &PublicationSettings,
    frontend: &'static FrontendAssetManifest,
    posts: &[PublicPostView],
    chronology: &[usize],
) -> Markup {
    let content = html! {
        section aria-labelledby="archive-heading" {
            h1 id="archive-heading" { "Archive" }
            @if chronology.is_empty() {
                p { "No posts have been published yet." }
            } @else {
                (render_post_list(posts, chronology))
            }
        }
    };
    render_layout(publication, frontend, "Archive", content)
}

fn render_tag(
    publication: &PublicationSettings,
    frontend: &'static FrontendAssetManifest,
    tag: &PostTag,
    posts: &[PublicPostView],
    indexes: &[usize],
) -> Markup {
    let title = format!("Posts tagged {}", tag.as_str());
    let content = html! {
        section aria-labelledby="tag-heading" {
            h1 id="tag-heading" { "Posts tagged “" (tag.as_str()) "”" }
            (render_post_list(posts, indexes))
        }
    };
    render_layout(publication, frontend, &title, content)
}

#[derive(Clone, Copy)]
enum ArticleBody<'article> {
    Omitted,
    Projected(&'article ProjectedArticleHtml),
}

#[derive(Clone, Copy)]
struct PostPageView<'post> {
    title: &'post PostTitle,
    description: &'post PostDescription,
    tags: &'post [PostTag],
    authored_at: OffsetDateTime,
    updated_at: Option<OffsetDateTime>,
    published_at: Option<OffsetDateTime>,
    tips: PostTipPolicy,
}

impl<'post> PostPageView<'post> {
    fn from_public(post: &'post PublicPostView) -> Self {
        Self {
            title: &post.title,
            description: &post.description,
            tags: &post.tags,
            authored_at: post.authored_at,
            updated_at: post.updated_at,
            published_at: Some(post.published_at),
            tips: post.tips,
        }
    }

    fn from_rendered(rendered: &'post RenderedPost, published_at: Option<OffsetDateTime>) -> Self {
        let metadata = &rendered.document.metadata;
        Self {
            title: &metadata.title,
            description: &metadata.description,
            tags: &metadata.tags,
            authored_at: metadata.authored_at,
            updated_at: metadata.updated_at,
            published_at,
            tips: metadata.tips,
        }
    }

    fn tips_enabled(self, publication: &PublicationSettings) -> bool {
        match self.tips {
            PostTipPolicy::Enabled => true,
            PostTipPolicy::Disabled => false,
            PostTipPolicy::InheritPublication => match publication.tips {
                DefaultPostTipPolicy::Enabled => true,
                DefaultPostTipPolicy::Disabled => false,
            },
        }
    }
}

struct TipHandoff<'recipient> {
    recipient: &'recipient TipRecipientProjection,
    qr: Markup,
}

impl<'recipient> TipHandoff<'recipient> {
    fn new(recipient: &'recipient TipRecipientProjection) -> Result<Self, SiteSnapshotBuildError> {
        let view = recipient.as_view();
        let code = QrCode::new(view.lnurl.as_bytes())
            .map_err(|error| SiteSnapshotBuildError::qr_code(error.to_string()))?;
        Ok(Self {
            recipient,
            qr: render_tip_qr(&code, view.address, view.lnurl),
        })
    }
}

fn render_tip_qr(code: &QrCode, address: &str, lnurl: &str) -> Markup {
    const QUIET_ZONE_MODULES: usize = 4;

    let dimension = code.width() + 2 * QUIET_ZONE_MODULES;
    let mut path = String::new();
    for y in 0..code.width() {
        for x in 0..code.width() {
            if code[(x, y)] == Color::Dark {
                let _ = write!(
                    path,
                    "M{} {}h1v1h-1z",
                    x + QUIET_ZONE_MODULES,
                    y + QUIET_ZONE_MODULES
                );
            }
        }
    }
    let label = format!("QR code for tipping {address} with Lightning");
    html! {
        svg class="tip-qr" xmlns="http://www.w3.org/2000/svg"
            viewBox=(format!("0 0 {dimension} {dimension}")) role="img"
            aria-label=(label) data-lnurl=(lnurl) {
            rect width="100%" height="100%" fill="white" {}
            path d=(path) fill="black" {}
        }
    }
}

fn render_tip_cta(handoff: &TipHandoff<'_>) -> Markup {
    let view = handoff.recipient.as_view();
    let recipient = view.display_name.unwrap_or(view.address);
    html! {
        aside class="tip-cta" aria-labelledby="tip-heading" {
            h2 id="tip-heading" { "Enjoyed this article?" }
            p { "Send a tip to " (recipient) "." }
            p {
                a class="tip-action" href=(view.wallet_link) { "Tip with Lightning" }
            }
            p class="tip-recipient" {
                "Lightning Address: " code { (view.address) }
                " "
                button type="button" class="tip-copy" hidden
                    data-copy-lightning-address=(view.address) { "Copy" }
            }
            (handoff.qr.clone())
            p { "Your wallet will ask for the amount and apply the recipient service's limits." }
            p { "Tips are voluntary and are handled by your wallet and the recipient's Lightning service." }
        }
    }
}

fn render_post(
    publication: &PublicationSettings,
    frontend: &'static FrontendAssetManifest,
    post: PostPageView<'_>,
    article: ArticleBody<'_>,
    tip_handoff: Option<&TipHandoff<'_>>,
) -> Markup {
    let tips_enabled = post.tips_enabled(publication);
    let content = html! {
        article {
            header {
                h1 { (post.title.as_str()) }
                p { (post.description.as_str()) }
                @if let Some(published_at) = post.published_at {
                    p class="publication-time" {
                        "Published "
                        time datetime=(published_at.to_string()) {
                            (published_at.to_string())
                        }
                    }
                }
                p class="author-time" {
                    "Authored "
                    time datetime=(post.authored_at.to_string()) {
                        (post.authored_at.to_string())
                    }
                    @if let Some(updated_at) = post.updated_at {
                        " · Updated "
                        time datetime=(updated_at.to_string()) { (updated_at.to_string()) }
                    }
                }
                @if !post.tags.is_empty() {
                    ul class="post-tags" aria-label="Tags" {
                        @for tag in post.tags {
                            li {
                                a href=(format!("/tags/{}", tag.as_str())) { (tag.as_str()) }
                            }
                        }
                    }
                }
            }
            section class="post-content" {
                @if let ArticleBody::Projected(article) = article {
                    (trusted_article_markup(article))
                }
            }
            @if tips_enabled {
                @if let Some(tip_handoff) = tip_handoff {
                    (render_tip_cta(tip_handoff))
                }
            }
        }
    };
    render_layout(publication, frontend, post.title.as_str(), content)
}

fn render_post_list(posts: &[PublicPostView], indexes: &[usize]) -> Markup {
    html! {
        ol class="post-list" {
            @for index in indexes {
                @let post = &posts[*index];
                li {
                    article {
                        h2 {
                            a href=(post.public_path().as_str()) { (post.title.as_str()) }
                        }
                        p { (post.description.as_str()) }
                        time datetime=(post.published_at.to_string()) {
                            (post.published_at.to_string())
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum PublicErrorPage {
    NotFound,
    MethodNotAllowed,
}

fn render_error(
    publication: &PublicationSettings,
    frontend: &'static FrontendAssetManifest,
    error: PublicErrorPage,
) -> Markup {
    let (title, explanation) = match error {
        PublicErrorPage::NotFound => ("Page not found", "The requested page does not exist."),
        PublicErrorPage::MethodNotAllowed => (
            "Method not allowed",
            "The requested method is not available for this page.",
        ),
    };
    render_layout(
        publication,
        frontend,
        title,
        html! {
            section class="error-page" {
                h1 { (title) }
                p { (explanation) }
                p { a href="/" { "Return to the publication index" } }
            }
        },
    )
}

fn render_layout(
    publication: &PublicationSettings,
    frontend: &'static FrontendAssetManifest,
    page_title: &str,
    content: Markup,
) -> Markup {
    let site = &publication.site;
    let feed_url = CanonicalSiteUrl::for_path(&site.base_url, &PublicPagePath::feed());
    let feed_title = format!("{} RSS feed", site.title.as_str());
    let full_title = if page_title == site.title.as_str() {
        page_title.to_owned()
    } else {
        format!("{page_title} — {}", site.title.as_str())
    };
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="description" content=(site.description.as_str());
                title { (full_title) }
                link rel="alternate" type="application/rss+xml"
                    title=(feed_title) href=(feed_url.as_str());
                link rel="stylesheet" href=(frontend.css.public_path);
                @if let Some(javascript) = &frontend.javascript {
                    script src=(javascript.public_path) defer {}
                }
            }
            body {
                header class="site-header" {
                    a class="site-title" href="/" { (site.title.as_str()) }
                    nav aria-label="Primary navigation" {
                        ul {
                            li { a href="/" { "Home" } }
                            li { a href="/archive" { "Archive" } }
                        }
                    }
                }
                main { (content) }
                footer {
                    p { "Written by " (publication.author.name.as_str()) }
                }
            }
        }
    }
}

struct ProjectedArticleHtml(Box<str>);

impl ProjectedArticleHtml {
    fn new(value: String) -> Self {
        Self(value.into_boxed_str())
    }
}

/// The sole trusted-HTML sink for rendered Markdown in the Maud shell.
fn trusted_article_markup(article: &ProjectedArticleHtml) -> Markup {
    PreEscaped(article.0.to_string())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Barrier, thread};

    use maincopy_shared::profile::{LightningAddress, ProfileDisplayName};

    use super::*;
    use crate::{
        frontend_assets::embedded_manifest,
        render::{compile_content_catalog, render_markdown},
    };
    use markdown_compiler::{
        LogicalAssetPath, PostCollection, ResolvedPostAssets, ResolvedSiteAssets, digest_asset,
        resolve_content_assets,
    };

    use crate::content_fixtures::{asset, content_tree, post, publication};

    const FIRST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const SECOND_ID: &str = "22222222-2222-4222-8222-222222222222";
    const DRAFT_ID: &str = "33333333-3333-4333-8333-333333333333";

    struct Fixture {
        catalog: Arc<ContentCatalog>,
        revisions: BTreeMap<PostId, PostRevisionDigest>,
    }

    fn post_source(
        id: &str,
        title: &str,
        slug: &str,
        tags: &[&str],
        image: Option<&str>,
        body: &str,
        draft: bool,
    ) -> String {
        let tags = tags
            .iter()
            .map(|tag| format!("{tag:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let image = image.map_or_else(String::new, |path| format!("image = {path:?}\n"));
        format!(
            "+++\n\
             id = {id:?}\n\
             title = {title:?}\n\
             slug = {slug:?}\n\
             authored_at = 2026-08-29T15:00:00-04:00\n\
             updated_at = 2026-08-29T16:00:00-04:00\n\
             description = \"Description <unsafe> & text.\"\n\
             {image}\
             tags = [{tags}]\n\
             draft = {draft}\n\
             +++\n\
             {body}"
        )
    }

    fn fixture() -> Fixture {
        fixture_with_first(
            "# First\n<script>alert('body')</script>\n![public](assets/public.png)\n",
            b"public",
        )
    }

    fn fixture_with_first(first_body: &str, public_asset: &[u8]) -> Fixture {
        let publication_source = "[site]\n\
             title = \"Site <unsafe> & title\"\n\
             base_url = \"https://blog.example.com/\"\n\
             description = \"A <careful> site.\"\n\
             favicon = \"assets/favicon.png\"\n\
             [author]\n\
             name = \"Author <unsafe>\"\n";
        let tree = content_tree(
            publication("publication.toml", publication_source.to_owned()),
            vec![
                post(
                    "posts/first.md",
                    PostCollection::Posts,
                    post_source(
                        FIRST_ID,
                        "First <script>alert(1)</script>",
                        "first-post",
                        &["rust"],
                        Some("assets/first-cover.png"),
                        first_body,
                        false,
                    ),
                ),
                post(
                    "posts/second.md",
                    PostCollection::Posts,
                    post_source(
                        SECOND_ID,
                        "Second post",
                        "second-post",
                        &["rust", "sqlite"],
                        Some("assets/second-cover.png"),
                        "# Second\n![private](assets/private.png)\n",
                        false,
                    ),
                ),
                post(
                    "drafts/draft.md",
                    PostCollection::Drafts,
                    post_source(
                        DRAFT_ID,
                        "Draft post",
                        "draft-post",
                        &["draft-tag"],
                        Some("assets/draft-cover.png"),
                        "# Draft\n![draft](assets/draft.png)\n",
                        true,
                    ),
                ),
            ],
            vec![
                asset(
                    LogicalAssetPath::parse("assets/favicon.png").unwrap(),
                    b"favicon".to_vec(),
                ),
                asset(
                    LogicalAssetPath::parse("assets/public.png").unwrap(),
                    public_asset.to_vec(),
                ),
                asset(
                    LogicalAssetPath::parse("assets/first-cover.png").unwrap(),
                    b"first cover".to_vec(),
                ),
                asset(
                    LogicalAssetPath::parse("assets/private.png").unwrap(),
                    b"private".to_vec(),
                ),
                asset(
                    LogicalAssetPath::parse("assets/second-cover.png").unwrap(),
                    b"second cover".to_vec(),
                ),
                asset(
                    LogicalAssetPath::parse("assets/draft.png").unwrap(),
                    b"draft".to_vec(),
                ),
                asset(
                    LogicalAssetPath::parse("assets/draft-cover.png").unwrap(),
                    b"draft cover".to_vec(),
                ),
            ],
            0,
        );
        let content = tree.validate().unwrap();
        let assets = resolve_content_assets(&tree, &content).unwrap();
        let mut revisions = BTreeMap::new();
        for document in &content.posts {
            let rendered = render_markdown(
                document,
                assets.assets_for(document).unwrap(),
                assets.site_assets_for(&content.publication).unwrap(),
            )
            .unwrap();
            revisions.insert(
                rendered.document.metadata.id.clone(),
                rendered.revision.clone(),
            );
        }
        let catalog = Arc::new(compile_content_catalog(&content, &assets).unwrap());
        Fixture { catalog, revisions }
    }

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    fn entry(fixture: &Fixture, id: &str, published_at: i64) -> PublishedPostRevision {
        let post_id = PostId::parse(id).unwrap();
        PublishedPostRevision::new(
            post_id.clone(),
            fixture.revisions[&post_id].clone(),
            at(published_at),
        )
    }

    fn projection(
        entries: impl IntoIterator<Item = PublishedPostRevision>,
    ) -> PublicLedgerProjection {
        PublicLedgerProjection::try_from_exact_entries(entries).unwrap()
    }

    fn build_snapshot(
        fixture: &Fixture,
        ledger: &PublicLedgerProjection,
    ) -> Result<SiteSnapshot, SiteSnapshotBuildError> {
        let shell = render_site_shell(Arc::clone(&fixture.catalog), embedded_manifest(), ledger)?;
        build_site_snapshot(shell, ledger)
    }

    fn preview_asset_endpoint() -> String {
        format!(
            "/api/admin/v1/preview-assets/content-b3-v1-{}",
            "88".repeat(32)
        )
    }

    fn tip_projection(display_name: Option<&str>, address: &str) -> TipRecipientProjection {
        TipRecipientProjection::from_validated_profile(
            display_name.map(|value| ProfileDisplayName::parse(value).unwrap()),
            LightningAddress::parse(address).unwrap(),
        )
        .unwrap()
    }

    fn catalog_asset(fixture: &Fixture, path: &str) -> DigestedAsset {
        fixture
            .catalog
            .local_assets
            .assets
            .get(&LogicalAssetPath::parse(path).unwrap())
            .unwrap()
            .asset
            .clone()
    }

    #[test]
    fn projection_rejects_duplicate_post_ids() {
        let fixture = fixture();
        let first = entry(&fixture, FIRST_ID, 1_000);
        let error =
            PublicLedgerProjection::try_from_exact_entries([first.clone(), first]).unwrap_err();
        assert_eq!(error.post_id().as_str(), FIRST_ID);
        assert!(PublicLedgerProjection::empty().is_empty());
    }

    #[test]
    fn published_entry_is_inserted_in_exact_post_id_order() {
        let fixture = fixture();
        let original = projection([
            entry(&fixture, DRAFT_ID, 3_000),
            entry(&fixture, FIRST_ID, 1_000),
        ]);

        let published = original
            .with_published(entry(&fixture, SECOND_ID, 2_000))
            .unwrap();

        let post_ids: Vec<_> = published
            .published_posts()
            .map(|entry| entry.post_id.as_str())
            .collect();
        assert_eq!(post_ids, [FIRST_ID, SECOND_ID, DRAFT_ID]);
        assert_eq!(
            original
                .published_posts()
                .map(|entry| entry.post_id.as_str())
                .collect::<Vec<_>>(),
            [FIRST_ID, DRAFT_ID]
        );
    }

    #[test]
    fn publishing_rejects_an_existing_post_id() {
        let fixture = fixture();
        let ledger = projection([entry(&fixture, FIRST_ID, 1_000)]);

        let error = ledger
            .with_published(entry(&fixture, FIRST_ID, 2_000))
            .unwrap_err();

        assert_eq!(error.post_id().as_str(), FIRST_ID);
    }

    #[test]
    fn candidate_preview_uses_the_production_shell_for_every_publication_state() {
        let fixture = fixture();
        let asset_endpoint = preview_asset_endpoint();

        let draft = render_post_preview(
            &fixture.catalog,
            embedded_manifest(),
            &PostId::parse(DRAFT_ID).unwrap(),
            &asset_endpoint,
            None,
        )
        .unwrap()
        .unwrap();
        assert!(draft.starts_with("<!DOCTYPE html>"));
        assert!(draft.contains("class=\"site-header\""));
        assert!(draft.contains("<h1>Draft post</h1>"));
        assert!(draft.contains("<h1>Draft</h1>"));
        assert!(draft.contains(&format!("{asset_endpoint}?path=assets/draft.png")));
        assert!(!draft.contains("class=\"publication-time\""));

        let unpublished = render_post_preview(
            &fixture.catalog,
            embedded_manifest(),
            &PostId::parse(SECOND_ID).unwrap(),
            &asset_endpoint,
            None,
        )
        .unwrap()
        .unwrap();
        assert!(unpublished.contains("<h1>Second post</h1>"));
        assert!(unpublished.contains("<h1>Second</h1>"));
        assert!(!unpublished.contains("class=\"publication-time\""));

        let published = render_post_preview(
            &fixture.catalog,
            embedded_manifest(),
            &PostId::parse(FIRST_ID).unwrap(),
            &asset_endpoint,
            Some(at(2_000)),
        )
        .unwrap()
        .unwrap();
        assert!(published.contains("class=\"publication-time\""));
        assert!(published.contains("1970-01-01 0:33:20.0 +00:00:00"));

        assert!(
            render_post_preview(
                &fixture.catalog,
                embedded_manifest(),
                &PostId::parse("44444444-4444-4444-8444-444444444444").unwrap(),
                &asset_endpoint,
                None,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn preview_binding_excludes_activation_and_private_asset_transport_metadata() {
        let fixture = fixture();
        let post_id = PostId::parse(FIRST_ID).unwrap();
        let first = render_bound_post_preview(
            &fixture.catalog,
            embedded_manifest(),
            &post_id,
            None,
            "/api/admin/v1/preview-assets/first-candidate",
            None,
        )
        .unwrap()
        .unwrap();
        let second = render_bound_post_preview(
            &fixture.catalog,
            embedded_manifest(),
            &post_id,
            None,
            "/api/admin/v1/preview-assets/second-candidate",
            Some(at(9_000)),
        )
        .unwrap()
        .unwrap();

        assert_ne!(first.html, second.html);
        assert_eq!(first.digest, second.digest);
        assert_eq!(first.revision, fixture.revisions[&post_id]);
        assert_eq!(
            first.canonical_url.as_str(),
            "https://blog.example.com/posts/first-post"
        );
    }

    #[test]
    fn tip_handoff_renders_exact_accessible_copy_and_escapes_the_display_name() {
        let projection = tip_projection(Some("Alice <Writer> & Company"), "alice@example.com");
        let handoff = TipHandoff::new(&projection).unwrap();
        let html = render_tip_cta(&handoff).into_string();

        assert!(html.contains("<h2 id=\"tip-heading\">Enjoyed this article?</h2>"));
        assert!(html.contains("Send a tip to Alice &lt;Writer&gt; &amp; Company."));
        assert!(html.contains(">Tip with Lightning</a>"));
        assert!(html.contains("Lightning Address: <code>alice@example.com</code>"));
        assert!(html.contains(
            "<button type=\"button\" class=\"tip-copy\" hidden data-copy-lightning-address=\"alice@example.com\">Copy</button>"
        ));
        assert!(html.contains(
            "Your wallet will ask for the amount and apply the recipient service's limits."
        ));
        assert!(html.contains("Tips are voluntary"));
        assert!(html.contains("role=\"img\""));
        assert!(
            html.contains("aria-label=\"QR code for tipping alice@example.com with Lightning\"")
        );
        assert!(!html.contains("<form"));
        assert!(!html.contains("<input"));
        assert!(!html.contains("invoice"));
        assert!(!html.contains("payment status"));
        assert!(!html.contains("success"));
    }

    #[test]
    fn tip_handoff_falls_back_to_the_address_and_qr_matches_the_wallet_payload() {
        let projection = tip_projection(None, "alice@example.com");
        let view = projection.as_view();
        let first = TipHandoff::new(&projection).unwrap();
        let second = TipHandoff::new(&projection).unwrap();
        let html = render_tip_cta(&first).into_string();

        assert!(html.contains("Send a tip to alice@example.com."));
        assert!(html.contains(&format!("href=\"{}\"", view.wallet_link)));
        assert!(html.contains(&format!("data-lnurl=\"{}\"", view.lnurl)));
        assert_eq!(first.qr.0, second.qr.0);
        let code = QrCode::new(view.lnurl.as_bytes()).unwrap();
        let mut dark_modules = 0;
        for y in 0..code.width() {
            for x in 0..code.width() {
                if code[(x, y)] == Color::Dark {
                    dark_modules += 1;
                    assert!(
                        first
                            .qr
                            .0
                            .contains(&format!("M{} {}h1v1h-1z", x + 4, y + 4))
                    );
                }
            }
        }
        assert_eq!(first.qr.0.matches('z').count(), dark_modules);
    }

    #[test]
    fn authored_tip_policy_controls_the_profile_handoff() {
        let fixture = fixture();
        let rendered = fixture
            .catalog
            .current_post(&PostId::parse(FIRST_ID).unwrap())
            .unwrap();
        let mut publication = fixture.catalog.publication.clone();
        let projection = tip_projection(Some("Alice"), "alice@example.com");
        let handoff = TipHandoff::new(&projection).unwrap();
        let mut page = PostPageView::from_rendered(rendered, None);

        publication.tips = DefaultPostTipPolicy::Disabled;
        page.tips = PostTipPolicy::InheritPublication;
        let inherited_disabled = render_post(
            &publication,
            embedded_manifest(),
            page,
            ArticleBody::Omitted,
            Some(&handoff),
        )
        .into_string();
        assert!(!inherited_disabled.contains("class=\"tip-cta\""));

        page.tips = PostTipPolicy::Enabled;
        let post_enabled = render_post(
            &publication,
            embedded_manifest(),
            page,
            ArticleBody::Omitted,
            Some(&handoff),
        )
        .into_string();
        assert!(post_enabled.contains("class=\"tip-cta\""));

        publication.tips = DefaultPostTipPolicy::Enabled;
        page.tips = PostTipPolicy::Disabled;
        let post_disabled = render_post(
            &publication,
            embedded_manifest(),
            page,
            ArticleBody::Omitted,
            Some(&handoff),
        )
        .into_string();
        assert!(!post_disabled.contains("class=\"tip-cta\""));
    }

    #[test]
    fn exact_public_selection_controls_pages_chronology_and_reachable_assets() {
        let fixture = fixture();
        let ledger = projection([entry(&fixture, FIRST_ID, 2_000)]);
        let snapshot = build_snapshot(&fixture, &ledger).unwrap();

        let first_slug = PostSlug::parse("first-post").unwrap();
        let second_slug = PostSlug::parse("second-post").unwrap();
        let draft_slug = PostSlug::parse("draft-post").unwrap();
        let page = snapshot.post_page(&first_slug).unwrap();
        assert!(page.contains("First &lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(page.contains("&lt;script&gt;alert(\'body\')&lt;/script&gt;"));
        assert!(!page.contains("<script>"));
        assert!(page.contains("1970-01-01 0:33:20.0 +00:00:00"));
        assert!(snapshot.post_page(&second_slug).is_none());
        assert!(snapshot.post_page(&draft_slug).is_none());
        assert!(
            snapshot
                .tag_page(&PostTag::parse("rust").unwrap())
                .is_some()
        );
        assert!(
            snapshot
                .tag_page(&PostTag::parse("draft-tag").unwrap())
                .is_none()
        );
        assert_eq!(
            snapshot.post_canonical_url(&first_slug).unwrap().as_str(),
            "https://blog.example.com/posts/first-post"
        );

        let paths: Vec<_> = snapshot
            .public_assets()
            .map(|asset| asset.asset.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "assets/favicon.png",
                "assets/first-cover.png",
                "assets/public.png"
            ]
        );
        assert!(!snapshot.index_page().contains("Second post"));
        assert!(!snapshot.index_page().contains("Draft post"));
        assert!(snapshot.feed.body.contains(FIRST_ID));
        assert!(
            snapshot
                .feed
                .body
                .contains("https://blog.example.com/posts/first-post")
        );
        assert!(!snapshot.feed.body.contains(SECOND_ID));
        assert!(!snapshot.feed.body.contains(DRAFT_ID));
        assert_eq!(
            snapshot.robots.body.as_ref(),
            concat!(
                "User-agent: *\n",
                "Allow: /\n",
                "\n",
                "Sitemap: https://blog.example.com/sitemap.xml\n",
            )
        );
        assert!(
            snapshot
                .sitemap
                .body
                .contains("https://blog.example.com/posts/first-post")
        );
        assert!(
            snapshot
                .sitemap
                .body
                .contains("https://blog.example.com/tags/rust")
        );
        assert!(!snapshot.sitemap.body.contains("second-post"));
        assert!(!snapshot.sitemap.body.contains("draft-post"));
        assert!(!snapshot.sitemap.body.contains("draft-tag"));
    }

    #[test]
    fn rss_failure_rejects_the_candidate_without_changing_the_active_snapshot() {
        let valid = fixture();
        let ledger = projection([entry(&valid, FIRST_ID, 2_000)]);
        let active = build_snapshot(&valid, &ledger).unwrap();
        let (reader, _activator) = snapshot_store(active);
        let before = reader.load_full();

        let mut invalid = fixture();
        Arc::make_mut(&mut invalid.catalog).publication.site.title =
            markdown_compiler::SiteTitle::new("Invalid RSS \u{fffe}").unwrap();
        let error = build_snapshot(&invalid, &ledger).unwrap_err();

        assert_eq!(error.code, SiteSnapshotBuildErrorCode::RssRenderFailed);
        assert_eq!(error.post_id, None);
        assert!(Arc::ptr_eq(&before, &reader.load_full()));
    }

    #[test]
    fn sitemap_failure_rejects_the_candidate_without_changing_the_active_snapshot() {
        let valid = fixture();
        let ledger = projection([entry(&valid, FIRST_ID, 2_000)]);
        let active = build_snapshot(&valid, &ledger).unwrap();
        let (reader, _activator) = snapshot_store(active);
        let before = reader.load_full();

        let oversized_origin = format!("https://{}example.com/", "a.".repeat(1_024));
        let base_url = markdown_compiler::PublicationBaseUrl::parse(&oversized_origin).unwrap();
        assert!(base_url.as_str().chars().count() >= 2_048);
        let mut invalid = fixture();
        Arc::make_mut(&mut invalid.catalog)
            .publication
            .site
            .base_url = base_url;
        let error = build_snapshot(&invalid, &ledger).unwrap_err();

        assert_eq!(error.code, SiteSnapshotBuildErrorCode::SitemapRenderFailed);
        assert_eq!(error.post_id, None);
        assert!(Arc::ptr_eq(&before, &reader.load_full()));
    }

    #[test]
    fn robots_failure_rejects_the_candidate_without_changing_the_active_snapshot() {
        let valid = fixture();
        let ledger = PublicLedgerProjection::empty();
        let active = build_snapshot(&valid, &ledger).unwrap();
        let (reader, _activator) = snapshot_store(active);
        let before = reader.load_full();

        let oversized_origin = format!("https://{}bexample.com/", "a.".repeat(1_008));
        let base_url = markdown_compiler::PublicationBaseUrl::parse(&oversized_origin).unwrap();
        assert_eq!(
            CanonicalSiteUrl::for_path(&base_url, &PublicPagePath::sitemap())
                .as_str()
                .chars()
                .count(),
            2_048
        );
        let mut invalid = fixture();
        Arc::make_mut(&mut invalid.catalog)
            .publication
            .site
            .base_url = base_url;
        let error = build_snapshot(&invalid, &ledger).unwrap_err();

        assert_eq!(error.code, SiteSnapshotBuildErrorCode::RobotsRenderFailed);
        assert_eq!(error.post_id, None);
        assert!(Arc::ptr_eq(&before, &reader.load_full()));
    }

    #[test]
    fn mixed_ledger_projects_retained_body_and_assets_with_current_revisions() {
        let prior = fixture_with_first(
            "# Retained body\n![public](assets/public.png)\n",
            b"retained public bytes",
        );
        let mut current = fixture_with_first(
            "# Current unpublished body\n![public](assets/public.png)\n",
            b"current public bytes",
        );
        let retained_first = entry(&prior, FIRST_ID, 1_000);
        let current_second = entry(&current, SECOND_ID, 2_000);
        let ledger = projection([retained_first, current_second]);
        Arc::make_mut(&mut current.catalog)
            .retain_revisions_from(&prior.catalog, ledger.revision_keys())
            .unwrap();

        let snapshot = build_snapshot(&current, &ledger).unwrap();
        let first = snapshot
            .post_page(&PostSlug::parse("first-post").unwrap())
            .unwrap();
        assert!(first.contains("Retained body"));
        assert!(!first.contains("Current unpublished body"));
        let second = snapshot
            .post_page(&PostSlug::parse("second-post").unwrap())
            .unwrap();
        assert!(second.contains("Second"));
        let retained_asset = snapshot
            .public_assets()
            .find(|asset| asset.asset.path.as_str() == "assets/public.png")
            .unwrap();
        assert_eq!(retained_asset.bytes.as_ref(), b"retained public bytes");
    }

    #[test]
    fn asset_collection_covers_each_source_dedupes_and_fails_closed() {
        let fixture = fixture();
        let favicon = catalog_asset(&fixture, "assets/favicon.png");
        let shared_reference = catalog_asset(&fixture, "assets/public.png");
        let site_assets = ResolvedSiteAssets::new(
            &fixture.catalog.publication,
            Some(AssetRevisionReference::local(favicon.clone())),
            Vec::new(),
            vec![AssetRevisionReference::local(shared_reference.clone())],
        );
        let mut selected = BTreeMap::new();
        collect_site_global_assets(&mut selected, &site_assets, &fixture.catalog.local_assets)
            .unwrap();

        let first_id = PostId::parse(FIRST_ID).unwrap();
        let first = fixture
            .catalog
            .get(&first_id, &fixture.revisions[&first_id])
            .unwrap();
        let first_cover = catalog_asset(&fixture, "assets/first-cover.png");
        let post_assets = ResolvedPostAssets::new(
            &first.document,
            Some(AssetRevisionReference::local(first_cover)),
            vec![AssetRevisionReference::local(shared_reference)],
        );
        let generated = GeneratedPostAsset::from_owned_bytes(
            LogicalAssetPath::parse("assets/generated.svg").unwrap(),
            Arc::from(&b"generated"[..]),
        );
        collect_selected_post_assets(
            &mut selected,
            &post_assets,
            std::slice::from_ref(&generated),
            &fixture.catalog.local_assets,
        )
        .unwrap();

        assert_eq!(selected.len(), 4, "the repeated post reference must dedupe");
        let digest = SiteSnapshotDigest::parse(&format!("site-b3-v1-{}", "44".repeat(32))).unwrap();
        let public = materialize_public_assets(selected, &digest).unwrap();
        let paths: Vec<_> = public
            .values()
            .map(|asset| asset.asset.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "assets/favicon.png",
                "assets/first-cover.png",
                "assets/generated.svg",
                "assets/public.png",
            ]
        );
        assert!(
            public
                .values()
                .any(|asset| asset.asset == generated.asset && asset.bytes == generated.bytes)
        );

        let missing = DigestedAsset::new(
            LogicalAssetPath::parse("assets/missing.png").unwrap(),
            digest_asset(b"missing"),
        );
        let error = insert_authored_asset(
            &mut BTreeMap::new(),
            &missing,
            &fixture.catalog.local_assets,
        )
        .unwrap_err();
        assert_eq!(error.code, SiteSnapshotBuildErrorCode::AssetUnavailable);
        assert!(error.message.contains("not present"));

        let mismatched = DigestedAsset::new(favicon.path.clone(), digest_asset(b"changed"));
        let error = insert_authored_asset(
            &mut BTreeMap::new(),
            &mismatched,
            &fixture.catalog.local_assets,
        )
        .unwrap_err();
        assert_eq!(error.code, SiteSnapshotBuildErrorCode::AssetUnavailable);
        assert!(error.message.contains("does not match"));

        let mut collision = BTreeMap::new();
        insert_selected_asset(
            &mut collision,
            generated.asset.clone(),
            Arc::clone(&generated.bytes),
        )
        .unwrap();
        let conflicting =
            DigestedAsset::new(generated.asset.path.clone(), digest_asset(b"conflicting"));
        let error =
            insert_selected_asset(&mut collision, conflicting, Arc::from(&b"conflicting"[..]))
                .unwrap_err();
        assert_eq!(error.code, SiteSnapshotBuildErrorCode::AssetCollision);
    }

    #[test]
    fn chronology_uses_ledger_time_then_stable_post_id() {
        let fixture = fixture();
        let ledger = projection([
            entry(&fixture, FIRST_ID, 1_000),
            entry(&fixture, SECOND_ID, 2_000),
        ]);
        let snapshot = build_snapshot(&fixture, &ledger).unwrap();
        let index = snapshot.index_page();
        assert!(index.find("Second post").unwrap() < index.find("First &lt;script&gt;").unwrap());
        assert!(
            snapshot.feed.body.find(SECOND_ID).unwrap()
                < snapshot.feed.body.find(FIRST_ID).unwrap()
        );

        let reversed = projection([
            entry(&fixture, SECOND_ID, 2_000),
            entry(&fixture, FIRST_ID, 1_000),
        ]);
        let rebuilt = build_snapshot(&fixture, &reversed).unwrap();
        assert_eq!(snapshot.digest, rebuilt.digest);
        assert_eq!(snapshot.index_page(), rebuilt.index_page());
        assert_eq!(snapshot.feed.body, rebuilt.feed.body);
        assert_eq!(snapshot.feed.digest, rebuilt.feed.digest);
        assert_eq!(snapshot.robots.body, rebuilt.robots.body);
        assert_eq!(snapshot.robots.digest, rebuilt.robots.digest);
        assert_eq!(snapshot.sitemap.body, rebuilt.sitemap.body);
        assert_eq!(snapshot.sitemap.digest, rebuilt.sitemap.digest);

        let tied = projection([
            entry(&fixture, SECOND_ID, 3_000),
            entry(&fixture, FIRST_ID, 3_000),
        ]);
        let tied = build_snapshot(&fixture, &tied).unwrap();
        assert!(
            tied.index_page().find("First &lt;script&gt;").unwrap()
                < tied.index_page().find("Second post").unwrap()
        );
        assert!(tied.feed.body.find(FIRST_ID).unwrap() < tied.feed.body.find(SECOND_ID).unwrap());
    }

    #[test]
    fn site_shell_identity_binds_exact_discovery_document_representations() {
        let fixture = fixture();
        let ledger = projection([entry(&fixture, FIRST_ID, 2_000)]);
        let posts = select_public_posts(&fixture.catalog, &ledger).unwrap();
        let chronology = chronology(&posts);
        let tags = tag_index(&posts, &chronology);
        let feed = render_public_feed(&fixture.catalog.publication, &posts, &chronology).unwrap();
        let sitemap = render_public_sitemap(&fixture.catalog.publication, &posts, &tags).unwrap();
        let robots = render_public_robots(&fixture.catalog.publication).unwrap();
        let original = render_pre_injection_shell(
            &fixture.catalog.publication,
            embedded_manifest(),
            &posts,
            &chronology,
            &tags,
            DiscoveryDocuments {
                feed: &feed,
                robots: &robots,
                sitemap: &sitemap,
            },
        )
        .unwrap();
        let mut changed_feed = feed.clone();
        changed_feed.body = format!("{}\n", feed.body).into();
        let feed_changed = render_pre_injection_shell(
            &fixture.catalog.publication,
            embedded_manifest(),
            &posts,
            &chronology,
            &tags,
            DiscoveryDocuments {
                feed: &changed_feed,
                robots: &robots,
                sitemap: &sitemap,
            },
        )
        .unwrap();
        let mut changed_robots = robots.clone();
        changed_robots.body = format!("{}\n", robots.body).into();
        let robots_changed = render_pre_injection_shell(
            &fixture.catalog.publication,
            embedded_manifest(),
            &posts,
            &chronology,
            &tags,
            DiscoveryDocuments {
                feed: &feed,
                robots: &changed_robots,
                sitemap: &sitemap,
            },
        )
        .unwrap();
        let mut changed_sitemap = sitemap.clone();
        changed_sitemap.body = format!("{}\n", sitemap.body).into();
        let sitemap_changed = render_pre_injection_shell(
            &fixture.catalog.publication,
            embedded_manifest(),
            &posts,
            &chronology,
            &tags,
            DiscoveryDocuments {
                feed: &feed,
                robots: &robots,
                sitemap: &changed_sitemap,
            },
        )
        .unwrap();

        assert_ne!(original, feed_changed);
        assert_ne!(original, robots_changed);
        assert_ne!(original, sitemap_changed);
    }

    #[test]
    fn presentation_identity_binds_exact_discovery_document_representations() {
        let fixture = fixture();
        let ledger = projection([entry(&fixture, FIRST_ID, 2_000)]);
        let snapshot = build_snapshot(&fixture, &ledger).unwrap();
        let mut changed_robots = snapshot.robots.clone();
        changed_robots.body = format!("{}\n", snapshot.robots.body).into();
        let mut changed_sitemap = snapshot.sitemap.clone();
        changed_sitemap.body = format!("{}\n", snapshot.sitemap.body).into();

        let robots_changed = presentation_digest(
            &snapshot.pages,
            &snapshot.not_found,
            &snapshot.method_not_allowed,
            &snapshot.feed,
            &changed_robots,
            &snapshot.sitemap,
        );
        let sitemap_changed = presentation_digest(
            &snapshot.pages,
            &snapshot.not_found,
            &snapshot.method_not_allowed,
            &snapshot.feed,
            &snapshot.robots,
            &changed_sitemap,
        );

        assert_ne!(snapshot.presentation_digest, robots_changed);
        assert_ne!(snapshot.presentation_digest, sitemap_changed);
    }

    #[test]
    fn profile_only_presentation_change_activates_without_changing_content_identity() {
        let fixture = fixture();
        let post_id = PostId::parse(FIRST_ID).unwrap();
        let ledger = projection([entry(&fixture, FIRST_ID, 2_000)]);
        let old = build_snapshot(&fixture, &ledger).unwrap();
        let content_digest = old.digest.clone();
        let old_presentation = old.presentation_digest;
        let old_feed_body = Arc::clone(&old.feed.body);
        let old_feed_digest = old.feed.digest;
        let old_robots_body = Arc::clone(&old.robots.body);
        let old_robots_digest = old.robots.digest;
        let old_sitemap_body = Arc::clone(&old.sitemap.body);
        let old_sitemap_digest = old.sitemap.digest;
        let revision = fixture.revisions[&post_id].clone();
        let mut next = build_snapshot(&fixture, &ledger).unwrap();
        let projection = tip_projection(Some("Alice"), "alice@example.com");
        let handoff = TipHandoff::new(&projection).unwrap();
        let page = next
            .pages
            .get_mut(&PageRoute::Post(PostSlug::parse("first-post").unwrap()))
            .unwrap();
        let mut html = page.html.to_string();
        html.push_str(&render_tip_cta(&handoff).into_string());
        page.html = html.into();
        next.presentation_digest = presentation_digest(
            &next.pages,
            &next.not_found,
            &next.method_not_allowed,
            &next.feed,
            &next.robots,
            &next.sitemap,
        );

        assert_eq!(next.digest, content_digest);
        assert_eq!(fixture.revisions[&post_id], revision);
        assert_ne!(next.presentation_digest, old_presentation);
        assert_eq!(next.feed.body, old_feed_body);
        assert_eq!(next.feed.digest, old_feed_digest);
        assert_eq!(next.robots.body, old_robots_body);
        assert_eq!(next.robots.digest, old_robots_digest);
        assert_eq!(next.sitemap.body, old_sitemap_body);
        assert_eq!(next.sitemap.digest, old_sitemap_digest);

        let (reader, mut activator) = snapshot_store(old);
        assert_eq!(
            activator.activate(&content_digest, next).unwrap(),
            SnapshotActivationOutcome::Activated
        );
        assert_ne!(reader.load_full().presentation_digest, old_presentation);
        assert_eq!(reader.load_full().digest, content_digest);
    }

    #[test]
    fn missing_and_draft_revisions_fail_closed() {
        let fixture = fixture();
        let missing_id = PostId::parse(FIRST_ID).unwrap();
        let missing = projection([PublishedPostRevision::new(
            missing_id,
            PostRevisionDigest::parse(&format!("post-b3-v1-{}", "55".repeat(32))).unwrap(),
            at(1_000),
        )]);
        assert_eq!(
            render_site_shell(Arc::clone(&fixture.catalog), embedded_manifest(), &missing)
                .unwrap_err()
                .code,
            SiteSnapshotBuildErrorCode::RevisionUnavailable
        );

        let draft = projection([entry(&fixture, DRAFT_ID, 1_000)]);
        assert_eq!(
            render_site_shell(Arc::clone(&fixture.catalog), embedded_manifest(), &draft)
                .unwrap_err()
                .code,
            SiteSnapshotBuildErrorCode::DraftSelected
        );
    }

    #[test]
    fn shell_cannot_be_cross_wired_to_another_ledger() {
        let fixture = fixture();
        let first = projection([entry(&fixture, FIRST_ID, 1_000)]);
        let second = projection([entry(&fixture, SECOND_ID, 1_000)]);
        let shell =
            render_site_shell(Arc::clone(&fixture.catalog), embedded_manifest(), &first).unwrap();
        let error = build_site_snapshot(shell, &second).unwrap_err();
        assert_eq!(error.code, SiteSnapshotBuildErrorCode::LedgerMismatch);
    }

    #[test]
    fn shell_and_snapshot_limits_are_inclusive() {
        assert!(validate_page_size(MAX_PAGE_BYTES).is_ok());
        assert_eq!(
            validate_page_size(MAX_PAGE_BYTES + 1).unwrap_err().code,
            SiteSnapshotBuildErrorCode::PageLimitExceeded
        );
        assert!(validate_route_count(MAX_PUBLIC_ROUTES - FIXED_PUBLIC_ROUTES, 0).is_ok());
        assert_eq!(
            validate_route_count(MAX_PUBLIC_ROUTES - FIXED_PUBLIC_ROUTES + 1, 0)
                .unwrap_err()
                .code,
            SiteSnapshotBuildErrorCode::RouteLimitExceeded
        );
        let mut retained = RetainedHtmlBudget::new();
        retained.add(MAX_RETAINED_HTML_BYTES).unwrap();
        assert_eq!(
            retained.add(1).unwrap_err().code,
            SiteSnapshotBuildErrorCode::RetainedHtmlLimitExceeded
        );
    }

    #[test]
    fn activation_checks_expected_digest_and_readers_never_observe_mixed_snapshots() {
        let fixture = fixture();
        let empty = PublicLedgerProjection::empty();
        let old = build_snapshot(&fixture, &empty).unwrap();
        let old_digest = old.digest.clone();
        let first = projection([entry(&fixture, FIRST_ID, 1_000)]);
        let new = build_snapshot(&fixture, &first).unwrap();
        let new_digest = new.digest.clone();
        let (reader, mut activator) = snapshot_store(old);

        let wrong = SiteSnapshotDigest::parse(&format!("site-b3-v1-{}", "77".repeat(32))).unwrap();
        let replacement = build_snapshot(&fixture, &first).unwrap();
        let error = activator.activate(&wrong, replacement).unwrap_err();
        assert_eq!(error.expected, wrong);
        assert_eq!(error.actual, old_digest);
        assert_eq!(&reader.load_full().digest, &old_digest);

        let barrier = Arc::new(Barrier::new(5));
        let mut readers = Vec::new();
        for _ in 0..4 {
            let reader = reader.clone();
            let barrier = Arc::clone(&barrier);
            let old_digest = old_digest.clone();
            let new_digest = new_digest.clone();
            readers.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..2_000 {
                    let observed = reader.load_full();
                    if observed.digest == old_digest {
                        assert!(
                            observed
                                .post_page(&PostSlug::parse("first-post").unwrap())
                                .is_none()
                        );
                        assert!(!observed.feed.body.contains(FIRST_ID));
                        assert!(!observed.sitemap.body.contains("first-post"));
                    } else if observed.digest == new_digest {
                        assert!(
                            observed
                                .post_page(&PostSlug::parse("first-post").unwrap())
                                .is_some()
                        );
                        assert!(observed.feed.body.contains(FIRST_ID));
                        assert!(observed.sitemap.body.contains("first-post"));
                    } else {
                        panic!("reader observed an unknown snapshot");
                    }
                }
            }));
        }
        barrier.wait();
        assert_eq!(
            activator.activate(&old_digest, new).unwrap(),
            SnapshotActivationOutcome::Activated
        );
        for reader in readers {
            reader.join().unwrap();
        }
        assert_eq!(&reader.load_full().digest, &new_digest);
        let same = build_snapshot(&fixture, &first).unwrap();
        assert_eq!(
            activator.activate(&new_digest, same).unwrap(),
            SnapshotActivationOutcome::AlreadyActive
        );
    }

    #[test]
    fn activation_installs_a_new_content_identity_even_when_rendered_bytes_match() {
        let fixture = fixture();
        let ledger = PublicLedgerProjection::empty();
        let old = build_snapshot(&fixture, &ledger).unwrap();
        let old_digest = old.digest.clone();
        let old_presentation = old.presentation_digest;
        let mut next = build_snapshot(&fixture, &ledger).unwrap();
        let new_digest =
            SiteSnapshotDigest::parse(&format!("site-b3-v1-{}", "66".repeat(32))).unwrap();
        next.digest = new_digest.clone();

        let (reader, mut activator) = snapshot_store(old);
        assert_eq!(next.presentation_digest, old_presentation);
        assert_eq!(
            activator.activate(&old_digest, next).unwrap(),
            SnapshotActivationOutcome::Activated
        );
        assert_eq!(reader.load_full().digest, new_digest);
    }

    #[test]
    fn all_public_error_and_activation_enum_wire_names_are_stable() {
        let build_codes = [
            SiteSnapshotBuildErrorCode::FrontendManifestInvalid,
            SiteSnapshotBuildErrorCode::FrontendMismatch,
            SiteSnapshotBuildErrorCode::LedgerMismatch,
            SiteSnapshotBuildErrorCode::RevisionUnavailable,
            SiteSnapshotBuildErrorCode::DraftSelected,
            SiteSnapshotBuildErrorCode::SourceBindingMismatch,
            SiteSnapshotBuildErrorCode::RouteCollision,
            SiteSnapshotBuildErrorCode::RouteLimitExceeded,
            SiteSnapshotBuildErrorCode::PageLimitExceeded,
            SiteSnapshotBuildErrorCode::RetainedHtmlLimitExceeded,
            SiteSnapshotBuildErrorCode::AssetUnavailable,
            SiteSnapshotBuildErrorCode::AssetCollision,
            SiteSnapshotBuildErrorCode::ArticleProjectionFailed,
            SiteSnapshotBuildErrorCode::RssRenderFailed,
            SiteSnapshotBuildErrorCode::RobotsRenderFailed,
            SiteSnapshotBuildErrorCode::SitemapRenderFailed,
            SiteSnapshotBuildErrorCode::QrCodeGenerationFailed,
            SiteSnapshotBuildErrorCode::IdentityRejected,
        ];
        let names = [
            "frontend_manifest_invalid",
            "frontend_mismatch",
            "ledger_mismatch",
            "revision_unavailable",
            "draft_selected",
            "source_binding_mismatch",
            "route_collision",
            "route_limit_exceeded",
            "page_limit_exceeded",
            "retained_html_limit_exceeded",
            "asset_unavailable",
            "asset_collision",
            "article_projection_failed",
            "rss_render_failed",
            "robots_render_failed",
            "sitemap_render_failed",
            "qr_code_generation_failed",
            "identity_rejected",
        ];
        for (code, name) in build_codes.into_iter().zip(names) {
            assert_eq!(serde_json::to_value(code).unwrap(), name);
        }
        assert_eq!(
            serde_json::to_value(SnapshotActivationOutcome::Activated).unwrap(),
            "activated"
        );
        assert_eq!(
            serde_json::to_value(SnapshotActivationOutcome::AlreadyActive).unwrap(),
            "already_active"
        );
    }
}
