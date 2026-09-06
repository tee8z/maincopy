use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Write as _},
    sync::Arc,
};

use arc_swap::ArcSwap;
use markdown_compiler::identity::{
    PreviewDigestInput, PublishedPostIdentityInput, SiteShellOutputDigest, SiteShellOutputHasher,
    finalize_preview_digest, finalize_site_snapshot,
};
use markdown_compiler::{
    AssetDigest, AssetRevisionReference, DefaultPostTipPolicy, DigestedAsset, DraftStatus,
    LogicalAssetPath, PostAlias, PostDescription, PostId, PostRevisionDigest, PostSlug, PostTag,
    PostTipPolicy, PostTitle, PreviewDigest, PublicationSettings, ResolvedLocalAssetStore,
    ResolvedPostAssets, ResolvedSiteAssets, RevisionIdentityError, SiteShellRendererIdentity,
    SiteSnapshotDigest,
};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use qrcode::{QrCode, types::Color};
use serde::Serialize;
use thiserror::Error;
use time::OffsetDateTime;

use crate::domain::profile::TipRecipientProjection;
use crate::domain::publication::{
    CanonicalSiteUrl, MAX_PUBLIC_ROUTES, PublicLedgerProjection, PublicPagePath,
    PublishedPostRevision, assets::AssetDelivery,
};
use crate::frontend_assets::FrontendAssetManifest;

use super::metadata::{
    MetadataRenderError, PostHeadMetadataInput, RenderedPostHeadMetadata, render_post_head_metadata,
};
use super::policy::{PublicResponsePolicy, REFERRER_POLICY};
use super::robots::{RenderedRobots, RobotsRenderError, render_robots};
use super::rss::{RenderedRssFeed, RssItem, RssRenderError, render_rss};
use super::sitemap::{RenderedSitemap, SitemapRenderError, render_sitemap};
use super::{ContentCatalog, RenderedPost, SnapshotAssetPath};

const MAX_PAGE_BYTES: usize = 40 * 1024 * 1024;
const MAX_RETAINED_HTML_BYTES: usize = 512 * 1024 * 1024;
const MAX_PUBLIC_ASSETS: usize = 50_000;
const MAX_RETAINED_ASSET_BYTES: usize = 512 * 1024 * 1024;
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
    aliases: Arc<[PostAlias]>,
    authored_at: OffsetDateTime,
    updated_at: Option<OffsetDateTime>,
    published_at: OffsetDateTime,
    canonical_url: Arc<CanonicalSiteUrl>,
    tips: PostTipPolicy,
    image: Option<AssetRevisionReference>,
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
            aliases: Arc::from(metadata.aliases.as_slice()),
            authored_at: metadata.authored_at,
            updated_at: metadata.updated_at,
            published_at: entry.published_at,
            canonical_url: Arc::new(CanonicalSiteUrl::for_path(
                &publication.site.base_url,
                &path,
            )),
            tips: metadata.tips,
            image: rendered.assets.image.clone(),
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
    post_navigation: Arc<[ChronologicalNeighbors]>,
    tags: BTreeMap<PostTag, Arc<[usize]>>,
    redirects: BTreeMap<PostAlias, Arc<CanonicalSiteUrl>>,
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
            .field("redirects", &self.redirects.len())
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
    let post_navigation = chronological_neighbors(posts.len(), &chronology);
    let tags = tag_index(&posts, &chronology);
    let alias_count = posts
        .iter()
        .try_fold(0_usize, |count, post| count.checked_add(post.aliases.len()));
    let alias_count = alias_count.ok_or_else(SiteSnapshotBuildError::route_limit)?;
    validate_route_count(posts.len(), tags.len(), alias_count)?;
    let redirects = alias_redirect_index(&posts)?;
    let feed = render_public_feed(&catalog.publication, &posts, &chronology)?;
    let sitemap = render_public_sitemap(&catalog.publication, &posts, &tags)?;
    let robots = render_public_robots(&catalog.publication)?;

    let renderer = SiteShellRendererIdentity::new(*frontend.bundle_digest.as_bytes());
    let pre_injection_output = render_pre_injection_shell(
        &PageRenderer::new(
            &catalog.publication,
            frontend,
            &catalog.site_assets,
            HeadAssetProjection::Identity,
        )?,
        PublicPagePlan {
            posts: &posts,
            chronology: &chronology,
            post_navigation: &post_navigation,
            tags: &tags,
            redirects: &redirects,
        },
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
        post_navigation: post_navigation.into(),
        tags,
        redirects,
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
    let canonical_url = CanonicalSiteUrl::for_path(
        &catalog.publication.site.base_url,
        &PublicPagePath::post(&rendered.document.metadata.slug),
    );
    let renderer = &PageRenderer::new(
        &catalog.publication,
        frontend,
        &catalog.site_assets,
        HeadAssetProjection::Preview(preview_asset_endpoint),
    )?;
    let page = PostPageView::from_rendered(rendered, published_at);
    let tips_enabled = page.tips_enabled(&catalog.publication);
    let tip_handoff = if tips_enabled {
        tip_recipient.map(TipHandoff::new).transpose()?
    } else {
        None
    };
    let html = render_post(
        renderer,
        page,
        &canonical_url,
        ArticleBody::Projected(&article),
        PostNavigation::default(),
        tip_handoff.as_ref(),
    )?
    .into_string();
    validate_page_size(html.len())?;
    let renderer = &PageRenderer::new(
        &catalog.publication,
        frontend,
        &catalog.site_assets,
        HeadAssetProjection::Identity,
    )?;
    let pre_injection_shell = render_post(
        renderer,
        PostPageView::from_rendered(rendered, None),
        &canonical_url,
        ArticleBody::Omitted,
        PostNavigation::default(),
        None,
    )?
    .into_string();
    validate_page_size(pre_injection_shell.len())?;
    let site_renderer = SiteShellRendererIdentity::new(*frontend.bundle_digest.as_bytes());
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
        site_renderer: &site_renderer,
        pre_injection_post_shell: pre_injection_shell.as_bytes(),
        response_policy: renderer.policy.content_security_policy.as_bytes(),
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
    let mut known_route_count = FIXED_PUBLIC_ROUTES;
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
        known_route_count = known_route_count
            .checked_add(1)
            .and_then(|count| count.checked_add(rendered.document.metadata.aliases.len()))
            .filter(|count| *count <= MAX_PUBLIC_ROUTES)
            .ok_or_else(SiteSnapshotBuildError::route_limit)?;
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ChronologicalNeighbors {
    previous: Option<usize>,
    next: Option<usize>,
}

fn chronological_neighbors(post_count: usize, chronology: &[usize]) -> Vec<ChronologicalNeighbors> {
    let mut neighbors = vec![ChronologicalNeighbors::default(); post_count];
    for (position, post_index) in chronology.iter().copied().enumerate() {
        neighbors[post_index] = ChronologicalNeighbors {
            next: position
                .checked_sub(1)
                .and_then(|next| chronology.get(next).copied()),
            previous: chronology.get(position + 1).copied(),
        };
    }
    neighbors
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

fn alias_redirect_index(
    posts: &[PublicPostView],
) -> Result<BTreeMap<PostAlias, Arc<CanonicalSiteUrl>>, SiteSnapshotBuildError> {
    let canonical_slugs: BTreeSet<_> = posts.iter().map(|post| post.slug.as_str()).collect();
    let mut redirects = BTreeMap::new();
    for post in posts {
        for alias in &*post.aliases {
            if canonical_slugs.contains(alias.as_str())
                || redirects
                    .insert(alias.clone(), Arc::clone(&post.canonical_url))
                    .is_some()
            {
                return Err(SiteSnapshotBuildError::post(
                    SiteSnapshotBuildErrorCode::RouteCollision,
                    post.post_id.clone(),
                    format!(
                        "published alias {} conflicts with another public post route",
                        alias.as_str()
                    ),
                ));
            }
        }
    }
    Ok(redirects)
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

fn validate_route_count(
    posts: usize,
    tags: usize,
    aliases: usize,
) -> Result<(), SiteSnapshotBuildError> {
    let routes = posts
        .checked_add(tags)
        .and_then(|count| count.checked_add(aliases))
        .and_then(|count| count.checked_add(FIXED_PUBLIC_ROUTES))
        .ok_or_else(SiteSnapshotBuildError::route_limit)?;
    if routes > MAX_PUBLIC_ROUTES {
        return Err(SiteSnapshotBuildError::route_limit());
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct PublicPagePlan<'plan> {
    posts: &'plan [PublicPostView],
    chronology: &'plan [usize],
    post_navigation: &'plan [ChronologicalNeighbors],
    tags: &'plan BTreeMap<PostTag, Arc<[usize]>>,
    redirects: &'plan BTreeMap<PostAlias, Arc<CanonicalSiteUrl>>,
}

fn render_pre_injection_shell(
    renderer: &PageRenderer<'_>,
    plan: PublicPagePlan<'_>,
    discovery: DiscoveryDocuments<'_>,
) -> Result<SiteShellOutputDigest, SiteSnapshotBuildError> {
    let mut pages = BTreeMap::new();
    pages.insert(PublicPagePath::index(), PreInjectionPage::Index);
    pages.insert(PublicPagePath::archive(), PreInjectionPage::Archive);
    pages.insert(PublicPagePath::feed(), PreInjectionPage::Feed);
    pages.insert(PublicPagePath::robots(), PreInjectionPage::Robots);
    pages.insert(PublicPagePath::sitemap(), PreInjectionPage::Sitemap);
    for (index, post) in plan.posts.iter().enumerate() {
        pages.insert(post.public_path(), PreInjectionPage::Post(index));
    }
    for tag in plan.tags.keys() {
        pages.insert(PublicPagePath::tag(tag), PreInjectionPage::Tag(tag));
    }
    for (alias, target) in plan.redirects {
        pages.insert(
            PublicPagePath::post_alias(alias),
            PreInjectionPage::Redirect(target.as_ref()),
        );
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
    let mut hasher = SiteShellOutputHasher::new(pages.len() + 2);
    hasher.page(
        "/maincopy-identity/public-response-policy",
        renderer.policy.content_security_policy.as_bytes(),
    );
    hasher.page(
        "/maincopy-identity/referrer-policy",
        REFERRER_POLICY.as_bytes(),
    );
    for (path, page) in pages {
        match page {
            PreInjectionPage::Index => hash_pre_injection_html(
                &mut hasher,
                &mut retained,
                &path,
                render_index(renderer, plan.posts, plan.chronology).into_string(),
            ),
            PreInjectionPage::Archive => hash_pre_injection_html(
                &mut hasher,
                &mut retained,
                &path,
                render_archive(renderer, plan.posts, plan.chronology).into_string(),
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
                    renderer,
                    PostPageView::from_public(&plan.posts[index]),
                    &plan.posts[index].canonical_url,
                    ArticleBody::Omitted,
                    PostNavigation::from_indexes(plan.posts, plan.post_navigation[index]),
                    None,
                )?
                .into_string(),
            ),
            PreInjectionPage::Tag(tag) => hash_pre_injection_html(
                &mut hasher,
                &mut retained,
                &path,
                render_tag(
                    renderer,
                    tag,
                    plan.posts,
                    plan.tags.get(tag).map_or(&[], Arc::as_ref),
                )
                .into_string(),
            ),
            PreInjectionPage::Redirect(target) => {
                hasher.page(path.as_str(), target.as_str().as_bytes());
                Ok(())
            }
            PreInjectionPage::Error(error) => hash_pre_injection_html(
                &mut hasher,
                &mut retained,
                &path,
                render_error(renderer, error).into_string(),
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
    Redirect(&'view CanonicalSiteUrl),
    Error(PublicErrorPage),
}

impl RenderedSiteShell {
    /// Consumes the exact inputs selected and validated when this shell was rendered.
    pub fn into_snapshot(self) -> Result<SiteSnapshot, SiteSnapshotBuildError> {
        let public_posts: Vec<_> = self
            .ledger
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
            &self.catalog.publication,
            &self.catalog.site_assets,
            &self.renderer,
            &self.pre_injection_output,
            &public_posts,
        )
        .map_err(SiteSnapshotBuildError::identity)?;

        let mut retained = RetainedHtmlBudget::new();
        let tip_handoff = self
            .tip_recipient
            .as_ref()
            .map(TipHandoff::new)
            .transpose()?;
        let pages = render_snapshot_pages(&self, &digest, tip_handoff.as_ref(), &mut retained)?;
        let renderer = &PageRenderer::new(
            &self.catalog.publication,
            self.frontend,
            &self.catalog.site_assets,
            HeadAssetProjection::Snapshot(&digest),
        )?;
        let not_found = rendered_error_page(renderer, PublicErrorPage::NotFound, &mut retained)?;
        let method_not_allowed =
            rendered_error_page(renderer, PublicErrorPage::MethodNotAllowed, &mut retained)?;
        let assets = collect_public_assets(&self, &digest)?;
        let feed = self.feed;
        let robots = self.robots;
        let sitemap = self.sitemap;
        let redirects = self.redirects;
        let presentation_digest = presentation_digest(
            &pages,
            &redirects,
            &not_found,
            &method_not_allowed,
            &feed,
            &robots,
            &sitemap,
        );

        let response_policy = renderer.policy.clone();
        Ok(SiteSnapshot {
            digest,
            presentation_digest,
            feed,
            robots,
            sitemap,
            pages,
            redirects,
            not_found,
            method_not_allowed,
            assets,
            response_policy,
            frontend: self.frontend,
            retained_html_bytes: retained.used,
        })
    }
}

fn render_snapshot_pages(
    shell: &RenderedSiteShell,
    digest: &SiteSnapshotDigest,
    tip_handoff: Option<&TipHandoff<'_>>,
    retained: &mut RetainedHtmlBudget,
) -> Result<BTreeMap<PageRoute, RenderedPage>, SiteSnapshotBuildError> {
    let publication = &shell.catalog.publication;
    let renderer = &PageRenderer::new(
        publication,
        shell.frontend,
        &shell.catalog.site_assets,
        HeadAssetProjection::Snapshot(digest),
    )?;
    let mut pages = BTreeMap::new();

    insert_page(
        &mut pages,
        PageRoute::Index,
        render_index(renderer, &shell.posts, &shell.chronology).into_string(),
        publication,
        retained,
    )?;
    insert_page(
        &mut pages,
        PageRoute::Archive,
        render_archive(renderer, &shell.posts, &shell.chronology).into_string(),
        publication,
        retained,
    )?;

    for (post_index, post) in shell.posts.iter().enumerate() {
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
                renderer,
                PostPageView::from_public(post),
                &post.canonical_url,
                ArticleBody::Projected(&article),
                PostNavigation::from_indexes(&shell.posts, shell.post_navigation[post_index]),
                tip_handoff,
            )?
            .into_string(),
            publication,
            retained,
        )?;
    }
    for (tag, indexes) in &shell.tags {
        insert_page(
            &mut pages,
            PageRoute::Tag(tag.clone()),
            render_tag(renderer, tag, &shell.posts, indexes).into_string(),
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
    renderer: &PageRenderer<'_>,
    error: PublicErrorPage,
    retained: &mut RetainedHtmlBudget,
) -> Result<RenderedPage, SiteSnapshotBuildError> {
    let publication = renderer.publication;
    let html = render_error(renderer, error).into_string();
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
    let mut selected = SelectedAssets::new();
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
        collect_selected_post_assets(&mut selected, &rendered.assets, local_assets)?;
    }

    materialize_public_assets(selected, digest)
}

fn collect_site_global_assets(
    selected: &mut SelectedAssets,
    site_assets: &ResolvedSiteAssets,
    local_assets: &ResolvedLocalAssetStore,
) -> Result<(), SiteSnapshotBuildError> {
    if let Some(AssetRevisionReference::Local(asset)) = &site_assets.favicon {
        insert_authored_asset(selected, asset, local_assets)?;
    }
    for reference in site_assets.image.iter().chain(&site_assets.references) {
        if let AssetRevisionReference::Local(asset) = reference {
            insert_authored_asset(selected, asset, local_assets)?;
        }
    }
    Ok(())
}

fn collect_selected_post_assets(
    selected: &mut SelectedAssets,
    assets: &ResolvedPostAssets,
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
    Ok(())
}

fn materialize_public_assets(
    selected: SelectedAssets,
    digest: &SiteSnapshotDigest,
) -> Result<BTreeMap<SnapshotAssetPath, SnapshotPublicAsset>, SiteSnapshotBuildError> {
    // One fixed snapshot digest and the map's unique logical paths make this
    // transformation injective, so collection cannot replace an earlier asset.
    selected
        .by_path
        .into_values()
        .map(|selected| {
            let delivery = AssetDelivery::for_authored(&selected.asset.path);
            let path = SnapshotAssetPath::new(digest, &selected.asset.path).map_err(|error| {
                SiteSnapshotBuildError::new(
                    SiteSnapshotBuildErrorCode::AssetUnavailable,
                    None,
                    error.to_string(),
                )
            })?;
            let public = SnapshotPublicAsset {
                digest: selected.asset.digest,
                bytes: selected.bytes,
                delivery,
            };
            Ok((path, public))
        })
        .collect()
}

struct SelectedAsset {
    asset: DigestedAsset,
    bytes: Arc<[u8]>,
}

struct SelectedAssets {
    by_path: BTreeMap<LogicalAssetPath, SelectedAsset>,
    retained_bytes: usize,
}

impl SelectedAssets {
    const fn new() -> Self {
        Self {
            by_path: BTreeMap::new(),
            retained_bytes: 0,
        }
    }

    fn insert(
        &mut self,
        asset: DigestedAsset,
        bytes: Arc<[u8]>,
    ) -> Result<(), SiteSnapshotBuildError> {
        if let Some(existing) = self.by_path.get(&asset.path) {
            if existing.asset == asset {
                return Ok(());
            }
            return Err(SiteSnapshotBuildError::new(
                SiteSnapshotBuildErrorCode::AssetCollision,
                None,
                "selected assets disagree at one logical path",
            ));
        }

        let retained_bytes =
            next_public_asset_bytes(self.by_path.len(), self.retained_bytes, bytes.len())?;
        self.by_path
            .insert(asset.path.clone(), SelectedAsset { asset, bytes });
        self.retained_bytes = retained_bytes;
        Ok(())
    }
}

fn next_public_asset_bytes(
    current_count: usize,
    current_bytes: usize,
    asset_bytes: usize,
) -> Result<usize, SiteSnapshotBuildError> {
    if current_count
        .checked_add(1)
        .is_none_or(|count| count > MAX_PUBLIC_ASSETS)
    {
        return Err(SiteSnapshotBuildError::public_asset_count_limit());
    }
    let Some(next_bytes) = current_bytes.checked_add(asset_bytes) else {
        return Err(SiteSnapshotBuildError::retained_asset_limit());
    };
    if next_bytes > MAX_RETAINED_ASSET_BYTES {
        return Err(SiteSnapshotBuildError::retained_asset_limit());
    }
    Ok(next_bytes)
}

fn insert_authored_asset(
    selected: &mut SelectedAssets,
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
    selected.insert(asset.clone(), Arc::clone(&resolved.bytes))
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
    redirects: &BTreeMap<PostAlias, Arc<CanonicalSiteUrl>>,
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
    hash_presentation_part(&mut hasher, &(redirects.len() as u64).to_be_bytes());
    for (alias, target) in redirects {
        hash_presentation_part(
            &mut hasher,
            PublicPagePath::post_alias(alias).as_str().as_bytes(),
        );
        hash_presentation_part(&mut hasher, target.as_str().as_bytes());
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
pub(crate) struct SnapshotPublicAsset {
    pub(crate) digest: AssetDigest,
    pub(crate) bytes: Arc<[u8]>,
    pub(crate) delivery: AssetDelivery,
}

/// Complete immutable request-facing state for one canonical publication.
pub struct SiteSnapshot {
    pub(crate) digest: SiteSnapshotDigest,
    pub(crate) presentation_digest: PresentationDigest,
    pub(crate) feed: RenderedRssFeed,
    pub(crate) robots: RenderedRobots,
    pub(crate) sitemap: RenderedSitemap,
    pages: BTreeMap<PageRoute, RenderedPage>,
    redirects: BTreeMap<PostAlias, Arc<CanonicalSiteUrl>>,
    not_found: RenderedPage,
    method_not_allowed: RenderedPage,
    assets: BTreeMap<SnapshotAssetPath, SnapshotPublicAsset>,
    pub(crate) frontend: &'static FrontendAssetManifest,
    pub(crate) response_policy: PublicResponsePolicy,
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
            .field("redirects", &self.redirects.len())
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

    pub(crate) fn public_asset(&self, path: &SnapshotAssetPath) -> Option<&SnapshotPublicAsset> {
        self.assets.get(path)
    }

    pub(crate) fn alias_target(&self, alias: &PostAlias) -> Option<&CanonicalSiteUrl> {
        self.redirects.get(alias).map(Arc::as_ref)
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
    ResponsePolicyInvalid,
    RevisionUnavailable,
    DraftSelected,
    RouteCollision,
    RouteLimitExceeded,
    PageLimitExceeded,
    RetainedHtmlLimitExceeded,
    AssetUnavailable,
    AssetCollision,
    PublicAssetCountLimitExceeded,
    RetainedAssetLimitExceeded,
    ArticleProjectionFailed,
    RssRenderFailed,
    RobotsRenderFailed,
    SitemapRenderFailed,
    MetadataRenderFailed,
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

    fn metadata(post_id: &PostId, error: MetadataRenderError) -> Self {
        Self::post(
            SiteSnapshotBuildErrorCode::MetadataRenderFailed,
            post_id.clone(),
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

    fn public_asset_count_limit() -> Self {
        Self::new(
            SiteSnapshotBuildErrorCode::PublicAssetCountLimitExceeded,
            None,
            format!("site exceeds the inclusive {MAX_PUBLIC_ASSETS}-asset limit"),
        )
    }

    fn retained_asset_limit() -> Self {
        Self::new(
            SiteSnapshotBuildErrorCode::RetainedAssetLimitExceeded,
            None,
            format!(
                "site exceeds the inclusive {MAX_RETAINED_ASSET_BYTES}-byte retained asset limit"
            ),
        )
    }
}

/// Local URLs use a stable marker while hashing to avoid a snapshot self-reference.
#[derive(Clone, Copy)]
enum HeadAssetProjection<'projection> {
    Identity,
    Snapshot(&'projection SiteSnapshotDigest),
    Preview(&'projection str),
}

struct PageRenderer<'render> {
    publication: &'render PublicationSettings,
    frontend: &'static FrontendAssetManifest,
    projection: HeadAssetProjection<'render>,
    favicon: Option<String>,
    image: Option<String>,
    policy: PublicResponsePolicy,
}

impl<'render> PageRenderer<'render> {
    fn new(
        publication: &'render PublicationSettings,
        frontend: &'static FrontendAssetManifest,
        assets: &ResolvedSiteAssets,
        projection: HeadAssetProjection<'render>,
    ) -> Result<Self, SiteSnapshotBuildError> {
        let policy =
            PublicResponsePolicy::new(&assets.allowed_origins, frontend).map_err(|error| {
                SiteSnapshotBuildError::new(
                    SiteSnapshotBuildErrorCode::ResponsePolicyInvalid,
                    None,
                    error.to_string(),
                )
            })?;
        let mut renderer = Self {
            publication,
            frontend,
            projection,
            favicon: None,
            image: None,
            policy,
        };
        renderer.favicon = assets
            .favicon
            .as_ref()
            .map(|asset| renderer.project_asset(asset))
            .transpose()?;
        renderer.image = renderer.project_metadata_image(assets.image.as_ref())?;
        Ok(renderer)
    }

    fn project_metadata_image(
        &self,
        asset: Option<&AssetRevisionReference>,
    ) -> Result<Option<String>, SiteSnapshotBuildError> {
        // Private local assets have no canonical public URL before release.
        // Their exact reference remains bound into the preview identity.
        match (self.projection, asset) {
            (HeadAssetProjection::Preview(_), Some(AssetRevisionReference::Local(_))) => Ok(None),
            (_, asset) => asset.map(|asset| self.project_asset(asset)).transpose(),
        }
    }

    fn project_asset(
        &self,
        asset: &AssetRevisionReference,
    ) -> Result<String, SiteSnapshotBuildError> {
        let asset = match asset {
            AssetRevisionReference::Local(asset) => asset,
            AssetRevisionReference::External(url) => return Ok(url.as_str().to_owned()),
        };
        let path = match self.projection {
            HeadAssetProjection::Identity => format!(
                "/assets/maincopy-snapshot-placeholder/{}",
                asset.path.as_str()
            ),
            HeadAssetProjection::Snapshot(digest) => SnapshotAssetPath::new(digest, &asset.path)
                .map_err(|error| {
                    SiteSnapshotBuildError::new(
                        SiteSnapshotBuildErrorCode::ArticleProjectionFailed,
                        None,
                        error.to_string(),
                    )
                })?
                .as_str()
                .to_owned(),
            HeadAssetProjection::Preview(endpoint) => {
                return Ok(format!("{endpoint}?path={}", asset.path.as_str()));
            }
        };
        Ok(format!(
            "{}{}",
            self.publication
                .site
                .base_url
                .as_str()
                .trim_end_matches('/'),
            path
        ))
    }
}

fn render_index(
    renderer: &PageRenderer<'_>,
    posts: &[PublicPostView],
    chronology: &[usize],
) -> Markup {
    let publication = renderer.publication;
    let canonical_url =
        CanonicalSiteUrl::for_path(&publication.site.base_url, &PublicPagePath::index());
    let content = html! {
        section class="maincopy-index" aria-labelledby="recent-posts-heading" {
            h1 id="recent-posts-heading" { "Recent posts" }
            @if chronology.is_empty() {
                p { "No posts have been published yet." }
            } @else {
                (render_post_list(posts, chronology))
            }
        }
    };
    render_layout(
        renderer,
        PageHead {
            image: renderer.image.as_deref(),
            context: PageContext::Index,
            title: publication.site.title.as_str(),
            description: publication.site.description.as_str(),
            canonical: Some(CanonicalPageHead {
                url: &canonical_url,
                kind: CanonicalPageKind::Website,
            }),
        },
        content,
    )
}

fn render_archive(
    renderer: &PageRenderer<'_>,
    posts: &[PublicPostView],
    chronology: &[usize],
) -> Markup {
    let publication = renderer.publication;
    let canonical_url =
        CanonicalSiteUrl::for_path(&publication.site.base_url, &PublicPagePath::archive());
    let description = format!(
        "Browse every published post from {}.",
        publication.site.title.as_str()
    );
    let content = html! {
        section class="maincopy-archive" aria-labelledby="archive-heading" {
            h1 id="archive-heading" { "Archive" }
            @if chronology.is_empty() {
                p { "No posts have been published yet." }
            } @else {
                (render_post_list(posts, chronology))
            }
        }
    };
    render_layout(
        renderer,
        PageHead {
            image: renderer.image.as_deref(),
            context: PageContext::Archive,
            title: "Archive",
            description: &description,
            canonical: Some(CanonicalPageHead {
                url: &canonical_url,
                kind: CanonicalPageKind::Website,
            }),
        },
        content,
    )
}

fn render_tag(
    renderer: &PageRenderer<'_>,
    tag: &PostTag,
    posts: &[PublicPostView],
    indexes: &[usize],
) -> Markup {
    let publication = renderer.publication;
    let title = format!("Posts tagged {}", tag.as_str());
    let description = format!(
        "Browse published posts tagged “{}” on {}.",
        tag.as_str(),
        publication.site.title.as_str()
    );
    let canonical_url =
        CanonicalSiteUrl::for_path(&publication.site.base_url, &PublicPagePath::tag(tag));
    let content = html! {
        section class="maincopy-tag" aria-labelledby="tag-heading" {
            h1 id="tag-heading" { "Posts tagged “" (tag.as_str()) "”" }
            (render_post_list(posts, indexes))
        }
    };
    render_layout(
        renderer,
        PageHead {
            image: renderer.image.as_deref(),
            context: PageContext::Tag,
            title: &title,
            description: &description,
            canonical: Some(CanonicalPageHead {
                url: &canonical_url,
                kind: CanonicalPageKind::Website,
            }),
        },
        content,
    )
}

#[derive(Clone, Copy)]
enum ArticleBody<'article> {
    Omitted,
    Projected(&'article ProjectedArticleHtml),
}

#[derive(Clone, Copy)]
struct PostPageView<'post> {
    post_id: &'post PostId,
    title: &'post PostTitle,
    description: &'post PostDescription,
    tags: &'post [PostTag],
    authored_at: OffsetDateTime,
    updated_at: Option<OffsetDateTime>,
    published_at: Option<OffsetDateTime>,
    tips: PostTipPolicy,
    image: Option<&'post AssetRevisionReference>,
}

impl<'post> PostPageView<'post> {
    fn from_public(post: &'post PublicPostView) -> Self {
        Self {
            post_id: &post.post_id,
            title: &post.title,
            description: &post.description,
            tags: &post.tags,
            authored_at: post.authored_at,
            updated_at: post.updated_at,
            published_at: Some(post.published_at),
            tips: post.tips,
            image: post.image.as_ref(),
        }
    }

    fn from_rendered(rendered: &'post RenderedPost, published_at: Option<OffsetDateTime>) -> Self {
        let metadata = &rendered.document.metadata;
        Self {
            post_id: &metadata.id,
            title: &metadata.title,
            description: &metadata.description,
            tags: &metadata.tags,
            authored_at: metadata.authored_at,
            updated_at: metadata.updated_at,
            published_at,
            tips: metadata.tips,
            image: rendered.assets.image.as_ref(),
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

#[derive(Clone, Copy, Default)]
struct PostNavigation<'post> {
    previous: Option<&'post PublicPostView>,
    next: Option<&'post PublicPostView>,
}

impl<'post> PostNavigation<'post> {
    fn from_indexes(posts: &'post [PublicPostView], indexes: ChronologicalNeighbors) -> Self {
        Self {
            previous: indexes.previous.map(|index| &posts[index]),
            next: indexes.next.map(|index| &posts[index]),
        }
    }

    const fn is_empty(self) -> bool {
        self.previous.is_none() && self.next.is_none()
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

fn render_post_navigation(navigation: PostNavigation<'_>) -> Markup {
    html! {
        @if !navigation.is_empty() {
            nav class="maincopy-post-navigation" aria-label="Post navigation" {
                @if let Some(previous) = navigation.previous {
                    a class="maincopy-post-navigation-link maincopy-post-navigation-previous"
                        href=(previous.public_path().as_str()) rel="prev" {
                        span class="maincopy-post-navigation-label" { "Previous post" }
                        span class="maincopy-post-navigation-title" { (previous.title.as_str()) }
                    }
                }
                @if let Some(next) = navigation.next {
                    a class="maincopy-post-navigation-link maincopy-post-navigation-next"
                        href=(next.public_path().as_str()) rel="next" {
                        span class="maincopy-post-navigation-label" { "Next post" }
                        span class="maincopy-post-navigation-title" { (next.title.as_str()) }
                    }
                }
            }
        }
    }
}

fn render_post(
    renderer: &PageRenderer<'_>,
    post: PostPageView<'_>,
    canonical_url: &CanonicalSiteUrl,
    article: ArticleBody<'_>,
    navigation: PostNavigation<'_>,
    tip_handoff: Option<&TipHandoff<'_>>,
) -> Result<Markup, SiteSnapshotBuildError> {
    let publication = renderer.publication;
    let image = renderer.project_metadata_image(post.image)?;
    let metadata = render_post_head_metadata(PostHeadMetadataInput {
        title: post.title,
        description: post.description,
        tags: post.tags,
        authored_at: post.authored_at,
        updated_at: post.updated_at,
        published_at: post.published_at,
        canonical_url,
        author: &publication.author.name,
        image: image.as_deref(),
    })
    .map_err(|error| SiteSnapshotBuildError::metadata(post.post_id, error))?;
    let tips_enabled = post.tips_enabled(publication);
    let content = html! {
        div class="maincopy-post-page" {
            article class="maincopy-post" {
                header class="maincopy-post-header" {
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
                        ul class="maincopy-post-tags" aria-label="Tags" {
                            @for tag in post.tags {
                                li {
                                    a href=(format!("/tags/{}", tag.as_str())) { (tag.as_str()) }
                                }
                            }
                        }
                    }
                }
                section class="maincopy-post-content" {
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
            (render_post_navigation(navigation))
        }
    };
    Ok(render_layout(
        renderer,
        PageHead {
            image: image.as_deref(),
            context: PageContext::Post,
            title: post.title.as_str(),
            description: post.description.as_str(),
            canonical: Some(CanonicalPageHead {
                url: canonical_url,
                kind: CanonicalPageKind::Article {
                    metadata: &metadata,
                    tags: post.tags,
                },
            }),
        },
        content,
    ))
}

fn render_post_list(posts: &[PublicPostView], indexes: &[usize]) -> Markup {
    html! {
        ol class="maincopy-post-list" {
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

fn render_error(renderer: &PageRenderer<'_>, error: PublicErrorPage) -> Markup {
    let (title, explanation) = match error {
        PublicErrorPage::NotFound => ("Page not found", "The requested page does not exist."),
        PublicErrorPage::MethodNotAllowed => (
            "Method not allowed",
            "The requested method is not available for this page.",
        ),
    };
    render_layout(
        renderer,
        PageHead {
            image: None,
            context: PageContext::Error,
            title,
            description: explanation,
            canonical: None,
        },
        html! {
            section class="maincopy-error-page" {
                h1 { (title) }
                p { (explanation) }
                p { a href="/" { "Return to the publication index" } }
            }
        },
    )
}

#[derive(Clone, Copy)]
struct PageHead<'head> {
    context: PageContext,
    title: &'head str,
    description: &'head str,
    canonical: Option<CanonicalPageHead<'head>>,
    image: Option<&'head str>,
}

#[derive(Clone, Copy)]
enum PageContext {
    Index,
    Archive,
    Tag,
    Post,
    Error,
}

impl PageContext {
    const fn body_class(self) -> &'static str {
        match self {
            Self::Index => "maincopy-site maincopy-page-index",
            Self::Archive => "maincopy-site maincopy-page-archive",
            Self::Tag => "maincopy-site maincopy-page-tag",
            Self::Post => "maincopy-site maincopy-page-post",
            Self::Error => "maincopy-site maincopy-page-error",
        }
    }
}

#[derive(Clone, Copy)]
struct CanonicalPageHead<'head> {
    url: &'head CanonicalSiteUrl,
    kind: CanonicalPageKind<'head>,
}

#[derive(Clone, Copy)]
enum CanonicalPageKind<'head> {
    Website,
    Article {
        metadata: &'head RenderedPostHeadMetadata,
        tags: &'head [PostTag],
    },
}

fn render_layout(renderer: &PageRenderer<'_>, head: PageHead<'_>, content: Markup) -> Markup {
    let publication = renderer.publication;
    let frontend = renderer.frontend;
    let site = &publication.site;
    let feed_url = CanonicalSiteUrl::for_path(&site.base_url, &PublicPagePath::feed());
    let feed_title = format!("{} RSS feed", site.title.as_str());
    let full_title = if head.title == site.title.as_str() {
        head.title.to_owned()
    } else {
        format!("{} — {}", head.title, site.title.as_str())
    };
    html! {
        (DOCTYPE)
        html lang="en" prefix="og: https://ogp.me/ns# article: https://ogp.me/ns/article#" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="description" content=(head.description);
                title { (full_title) }
                @if let Some(favicon) = &renderer.favicon {
                    link rel="icon" href=(favicon);
                }
                @if let Some(canonical) = head.canonical {
                    link rel="canonical" href=(canonical.url.as_str());
                    meta property="og:title" content=(head.title);
                    meta property="og:type" content=(match canonical.kind {
                        CanonicalPageKind::Website => "website",
                        CanonicalPageKind::Article { .. } => "article",
                    });
                    meta property="og:url" content=(canonical.url.as_str());
                    meta property="og:description" content=(head.description);
                    meta property="og:site_name" content=(site.title.as_str());
                    @if let Some(image) = head.image {
                        meta property="og:image" content=(image);
                    }
                    @if let CanonicalPageKind::Article { metadata, tags } = canonical.kind {
                        @if let Some(published_time) = &metadata.published_time {
                            meta property="article:published_time" content=(published_time);
                        }
                        @if let Some(modified_time) = &metadata.modified_time {
                            meta property="article:modified_time" content=(modified_time);
                        }
                        @for tag in tags {
                            meta property="article:tag" content=(tag.as_str());
                        }
                        (metadata.json_ld_script())
                    }
                }
                link rel="alternate" type="application/rss+xml"
                    title=(feed_title) href=(feed_url.as_str());
                link rel="stylesheet" href=(frontend.css.public_path);
                @if let Some(javascript) = &frontend.javascript {
                    script src=(javascript.public_path) integrity=[renderer.policy.script_integrity.as_deref()] defer {}
                }
            }
            body class=(head.context.body_class()) {
                header class="maincopy-site-header" {
                    a class="maincopy-site-title" href="/" { (site.title.as_str()) }
                    nav class="maincopy-site-navigation" aria-label="Primary navigation" {
                        ul {
                            li { a href="/" { "Home" } }
                            li { a href="/archive" { "Archive" } }
                        }
                    }
                }
                main class="maincopy-site-main" { (content) }
                footer class="maincopy-site-footer" {
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
    use crate::{frontend_assets::embedded_manifest, render::compile_content_catalog};
    use markdown_compiler::{
        LogicalAssetPath, PostCollection, ResolvedPostAssets, ResolvedSiteAssets, digest_asset,
        prepare_content,
    };

    use crate::content_fixtures::{asset, content_tree, post, publication};

    const FIRST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const SECOND_ID: &str = "22222222-2222-4222-8222-222222222222";
    const DRAFT_ID: &str = "33333333-3333-4333-8333-333333333333";

    struct Fixture {
        catalog: Arc<ContentCatalog>,
        revisions: BTreeMap<PostId, PostRevisionDigest>,
    }

    struct PostRoutes<'route> {
        slug: &'route str,
        aliases: &'route [&'route str],
    }

    fn post_source(
        id: &str,
        title: &str,
        routes: PostRoutes<'_>,
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
        let aliases = routes
            .aliases
            .iter()
            .map(|alias| format!("{alias:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let image = image.map_or_else(String::new, |path| format!("image = {path:?}\n"));
        let slug = routes.slug;
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
             aliases = [{aliases}]\n\
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
        fixture_with_routes(
            first_body,
            public_asset,
            "first-post",
            &["original-first-post"],
            "second-post",
            &["original-second-post"],
        )
    }

    fn fixture_with_routes(
        first_body: &str,
        public_asset: &[u8],
        first_slug: &str,
        first_aliases: &[&str],
        second_slug: &str,
        second_aliases: &[&str],
    ) -> Fixture {
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
                        PostRoutes {
                            slug: first_slug,
                            aliases: first_aliases,
                        },
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
                        PostRoutes {
                            slug: second_slug,
                            aliases: second_aliases,
                        },
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
                        PostRoutes {
                            slug: "draft-post",
                            aliases: &["draft-post-alias"],
                        },
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
        let content = prepare_content(&tree).unwrap();
        let catalog = Arc::new(compile_content_catalog(&content).unwrap());
        let revisions = catalog
            .rendered_posts()
            .map(|rendered| {
                (
                    rendered.document.metadata.id.clone(),
                    rendered.revision.clone(),
                )
            })
            .collect();
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
        shell.into_snapshot()
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
        let path = LogicalAssetPath::parse(path).unwrap();
        fixture
            .catalog
            .site_assets
            .favicon
            .iter()
            .chain(fixture.catalog.site_assets.references.iter())
            .chain(fixture.catalog.rendered_posts().flat_map(|post| {
                post.assets
                    .image
                    .iter()
                    .chain(post.assets.references.iter())
            }))
            .find_map(|reference| match reference {
                AssetRevisionReference::Local(asset) if asset.path == path => Some(asset.clone()),
                AssetRevisionReference::Local(_) | AssetRevisionReference::External(_) => None,
            })
            .expect("fixture asset reference must be present")
    }

    fn assert_core_page_metadata(
        page: &str,
        title: &str,
        description: &str,
        canonical_url: &str,
        object_type: &str,
    ) {
        assert_eq!(page.matches("<link rel=\"canonical\"").count(), 1);
        assert!(page.contains(&format!(
            "<link rel=\"canonical\" href=\"{canonical_url}\">"
        )));
        for property in [
            "og:title",
            "og:type",
            "og:url",
            "og:description",
            "og:site_name",
        ] {
            assert_eq!(
                page.matches(&format!("<meta property=\"{property}\""))
                    .count(),
                1,
                "unexpected {property} count in {page}"
            );
        }
        assert!(page.contains(&format!("<meta property=\"og:title\" content=\"{title}\">")));
        assert!(page.contains(&format!(
            "<meta property=\"og:type\" content=\"{object_type}\">"
        )));
        assert!(page.contains(&format!(
            "<meta property=\"og:url\" content=\"{canonical_url}\">"
        )));
        assert!(page.contains(&format!(
            "<meta property=\"og:description\" content=\"{description}\">"
        )));
        assert!(page.contains(
            "<meta property=\"og:site_name\" content=\"Site &lt;unsafe&gt; &amp; title\">"
        ));
    }

    fn post_json_ld(page: &str) -> serde_json::Value {
        const OPEN: &str = "<script type=\"application/ld+json\">";
        assert_eq!(page.matches(OPEN).count(), 1);
        let json = page
            .split_once(OPEN)
            .unwrap()
            .1
            .split_once("</script>")
            .unwrap()
            .0;
        assert!(!json.contains(['<', '>', '&']));
        serde_json::from_str(json).unwrap()
    }

    fn rendered_head(page: &str) -> &str {
        page.split_once("<head>")
            .unwrap()
            .1
            .split_once("</head>")
            .unwrap()
            .0
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
        assert!(draft.contains("maincopy-site-header"));
        assert!(draft.contains("<h1>Draft post</h1>"));
        assert!(draft.contains("<h1>Draft</h1>"));
        assert!(draft.contains(&format!("{asset_endpoint}?path=assets/draft.png")));
        assert!(!draft.contains("class=\"publication-time\""));
        assert_core_page_metadata(
            &draft,
            "Draft post",
            "Description &lt;unsafe&gt; &amp; text.",
            "https://blog.example.com/posts/draft-post",
            "article",
        );
        assert!(!draft.contains("property=\"article:published_time\""));
        assert!(post_json_ld(&draft).get("datePublished").is_none());

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
        assert!(published.contains(
            "<meta property=\"article:published_time\" content=\"1970-01-01T00:33:20Z\">"
        ));
        assert_eq!(
            post_json_ld(&published)["datePublished"],
            "1970-01-01T00:33:20Z"
        );
        let public =
            build_snapshot(&fixture, &projection([entry(&fixture, FIRST_ID, 2_000)])).unwrap();
        let public_post = public
            .post_page(&PostSlug::parse("first-post").unwrap())
            .unwrap();
        let public_prefix = format!("https://blog.example.com/assets/{}", public.digest);
        let image_url = format!("{public_prefix}/first-cover.png");
        assert_eq!(post_json_ld(&public_post)["image"], image_url);
        assert!(post_json_ld(&published).get("image").is_none());
        // Only the authenticated favicon projection and unreleased image metadata differ.
        let public_head = rendered_head(&public_post)
            .replace(
                &format!("{public_prefix}/favicon.png"),
                &format!("{asset_endpoint}?path=assets/favicon.png"),
            )
            .replace(
                &format!("<meta property=\"og:image\" content=\"{image_url}\">"),
                "",
            )
            .replace(&format!(",\"image\":\"{image_url}\""), "");
        assert_eq!(rendered_head(&published), public_head);

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
        let canonical_url = CanonicalSiteUrl::for_path(
            &publication.site.base_url,
            &PublicPagePath::post(&rendered.document.metadata.slug),
        );
        let projection = tip_projection(Some("Alice"), "alice@example.com");
        let handoff = TipHandoff::new(&projection).unwrap();
        let mut page = PostPageView::from_rendered(rendered, None);

        publication.tips = DefaultPostTipPolicy::Disabled;
        page.tips = PostTipPolicy::InheritPublication;
        let inherited_disabled = render_post(
            &PageRenderer::new(
                &publication,
                embedded_manifest(),
                &fixture.catalog.site_assets,
                HeadAssetProjection::Identity,
            )
            .unwrap(),
            page,
            &canonical_url,
            ArticleBody::Omitted,
            PostNavigation::default(),
            Some(&handoff),
        )
        .unwrap()
        .into_string();
        assert!(!inherited_disabled.contains("class=\"tip-cta\""));

        page.tips = PostTipPolicy::Enabled;
        let post_enabled = render_post(
            &PageRenderer::new(
                &publication,
                embedded_manifest(),
                &fixture.catalog.site_assets,
                HeadAssetProjection::Identity,
            )
            .unwrap(),
            page,
            &canonical_url,
            ArticleBody::Omitted,
            PostNavigation::default(),
            Some(&handoff),
        )
        .unwrap()
        .into_string();
        assert!(post_enabled.contains("class=\"tip-cta\""));

        publication.tips = DefaultPostTipPolicy::Enabled;
        page.tips = PostTipPolicy::Disabled;
        let post_disabled = render_post(
            &PageRenderer::new(
                &publication,
                embedded_manifest(),
                &fixture.catalog.site_assets,
                HeadAssetProjection::Identity,
            )
            .unwrap(),
            page,
            &canonical_url,
            ArticleBody::Omitted,
            PostNavigation::default(),
            Some(&handoff),
        )
        .unwrap()
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

        let paths: Vec<_> = [
            "assets/favicon.png",
            "assets/first-cover.png",
            "assets/public.png",
        ]
        .map(|path| {
            SnapshotAssetPath::new(&snapshot.digest, &LogicalAssetPath::parse(path).unwrap())
                .unwrap()
        })
        .into();
        assert_eq!(snapshot.assets.keys().cloned().collect::<Vec<_>>(), paths);
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
    fn canonical_pages_render_exact_core_open_graph_and_blog_posting_metadata() {
        let fixture = fixture();
        let ledger = projection([
            entry(&fixture, FIRST_ID, 2_000),
            entry(&fixture, SECOND_ID, 3_000),
        ]);
        let snapshot = build_snapshot(&fixture, &ledger).unwrap();

        assert_core_page_metadata(
            &snapshot.index_page(),
            "Site &lt;unsafe&gt; &amp; title",
            "A &lt;careful&gt; site.",
            "https://blog.example.com/",
            "website",
        );
        assert_core_page_metadata(
            &snapshot.archive_page(),
            "Archive",
            "Browse every published post from Site &lt;unsafe&gt; &amp; title.",
            "https://blog.example.com/archive",
            "website",
        );
        let tag_page = snapshot.tag_page(&PostTag::parse("rust").unwrap()).unwrap();
        assert_core_page_metadata(
            &tag_page,
            "Posts tagged rust",
            "Browse published posts tagged “rust” on Site &lt;unsafe&gt; &amp; title.",
            "https://blog.example.com/tags/rust",
            "website",
        );

        let first_page = snapshot
            .post_page(&PostSlug::parse("first-post").unwrap())
            .unwrap();
        assert_core_page_metadata(
            &first_page,
            "First &lt;script&gt;alert(1)&lt;/script&gt;",
            "Description &lt;unsafe&gt; &amp; text.",
            "https://blog.example.com/posts/first-post",
            "article",
        );
        assert!(first_page.contains(
            "<meta property=\"article:published_time\" content=\"1970-01-01T00:33:20Z\">"
        ));
        assert!(first_page.contains(
            "<meta property=\"article:modified_time\" content=\"2026-08-29T16:00:00-04:00\">"
        ));
        assert_eq!(first_page.matches("property=\"article:tag\"").count(), 1);
        assert!(first_page.contains("<meta property=\"article:tag\" content=\"rust\">"));

        let document = post_json_ld(&first_page);
        assert_eq!(document["@context"], "https://schema.org");
        assert_eq!(document["@type"], "BlogPosting");
        assert_eq!(document["headline"], "First <script>alert(1)</script>");
        assert_eq!(document["description"], "Description <unsafe> & text.");
        assert_eq!(document["url"], "https://blog.example.com/posts/first-post");
        assert_eq!(document["mainEntityOfPage"], document["url"]);
        assert_eq!(document["dateCreated"], "2026-08-29T15:00:00-04:00");
        assert_eq!(document["datePublished"], "1970-01-01T00:33:20Z");
        assert_eq!(document["dateModified"], "2026-08-29T16:00:00-04:00");
        assert_eq!(document["author"]["@type"], "Person");
        assert_eq!(document["author"]["name"], "Author <unsafe>");
        assert_eq!(document["keywords"], serde_json::json!(["rust"]));
        assert_eq!(
            document["image"],
            format!(
                "https://blog.example.com/assets/{}/first-cover.png",
                snapshot.digest
            )
        );

        let second_page = snapshot
            .post_page(&PostSlug::parse("second-post").unwrap())
            .unwrap();
        assert_eq!(second_page.matches("property=\"article:tag\"").count(), 2);
        assert!(second_page.contains("<meta property=\"article:tag\" content=\"rust\">"));
        assert!(second_page.contains("<meta property=\"article:tag\" content=\"sqlite\">"));
        assert_eq!(
            post_json_ld(&second_page)["keywords"],
            serde_json::json!(["rust", "sqlite"])
        );

        for error_page in [
            snapshot.not_found_page(),
            snapshot.method_not_allowed_page(),
        ] {
            assert!(!error_page.contains("<link rel=\"canonical\""));
            assert!(!error_page.contains("<meta property=\"og:"));
            assert!(!error_page.contains("application/ld+json"));
        }
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
            .public_asset(
                &SnapshotAssetPath::new(
                    &snapshot.digest,
                    &LogicalAssetPath::parse("assets/public.png").unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(retained_asset.bytes.as_ref(), b"retained public bytes");
    }

    #[test]
    fn mixed_retained_revisions_reject_alias_route_collisions_without_touching_the_active_snapshot()
    {
        for (second_slug, second_aliases) in [
            ("shared-route", &["current-second-alias"][..]),
            ("current-second", &["shared-route"][..]),
        ] {
            let prior = fixture_with_routes(
                "# Prior first\n",
                b"prior public bytes",
                "prior-first",
                &["shared-route"],
                "prior-second",
                &["prior-second-alias"],
            );
            let mut current = fixture_with_routes(
                "# Current first\n",
                b"current public bytes",
                "current-first",
                &["current-first-alias"],
                second_slug,
                second_aliases,
            );
            let ledger = projection([
                entry(&prior, FIRST_ID, 1_000),
                entry(&current, SECOND_ID, 2_000),
            ]);
            Arc::make_mut(&mut current.catalog)
                .retain_revisions_from(&prior.catalog, ledger.revision_keys())
                .unwrap();

            let active = build_snapshot(&current, &PublicLedgerProjection::empty()).unwrap();
            let (reader, _activator) = snapshot_store(active);
            let before = reader.load_full();
            let error =
                render_site_shell(Arc::clone(&current.catalog), embedded_manifest(), &ledger)
                    .unwrap_err();

            assert_eq!(error.code, SiteSnapshotBuildErrorCode::RouteCollision);
            assert!(Arc::ptr_eq(&before, &reader.load_full()));
        }
    }

    #[test]
    fn snapshot_activation_replaces_canonical_and_authored_alias_routes_together() {
        let prior = fixture_with_routes(
            "# Prior first\n",
            b"prior public bytes",
            "first-post",
            &["original-first-post"],
            "second-post",
            &["original-second-post"],
        );
        let current = fixture_with_routes(
            "# Current first\n",
            b"current public bytes",
            "renamed-first-post",
            &["first-post"],
            "second-post",
            &["original-second-post"],
        );
        let prior_ledger = projection([entry(&prior, FIRST_ID, 1_000)]);
        let current_ledger = projection([entry(&current, FIRST_ID, 1_000)]);
        let old = build_snapshot(&prior, &prior_ledger).unwrap();
        let old_digest = old.digest.clone();
        let next = build_snapshot(&current, &current_ledger).unwrap();
        let (reader, mut activator) = snapshot_store(old);

        let observed = reader.load_full();
        assert!(
            observed
                .post_page(&PostSlug::parse("first-post").unwrap())
                .is_some()
        );
        assert!(
            observed
                .alias_target(&PostAlias::parse("first-post").unwrap())
                .is_none()
        );

        assert_eq!(
            activator.activate(&old_digest, next).unwrap(),
            SnapshotActivationOutcome::Activated
        );
        let observed = reader.load_full();
        assert!(
            observed
                .post_page(&PostSlug::parse("first-post").unwrap())
                .is_none()
        );
        assert!(
            observed
                .post_page(&PostSlug::parse("renamed-first-post").unwrap())
                .is_some()
        );
        assert_eq!(
            observed
                .alias_target(&PostAlias::parse("first-post").unwrap())
                .unwrap()
                .as_str(),
            "https://blog.example.com/posts/renamed-first-post"
        );
        assert!(
            observed
                .alias_target(&PostAlias::parse("original-first-post").unwrap())
                .is_none()
        );
    }

    #[test]
    fn asset_collection_covers_each_source_dedupes_and_fails_closed() {
        let fixture = fixture();
        let favicon = catalog_asset(&fixture, "assets/favicon.png");
        let shared_reference = catalog_asset(&fixture, "assets/public.png");
        let site_assets = ResolvedSiteAssets::new(
            &fixture.catalog.publication,
            Some(AssetRevisionReference::local(favicon.clone())),
            None,
            Vec::new(),
            vec![AssetRevisionReference::local(shared_reference.clone())],
        );
        let mut selected = SelectedAssets::new();
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
        collect_selected_post_assets(&mut selected, &post_assets, &fixture.catalog.local_assets)
            .unwrap();

        assert_eq!(
            selected.by_path.len(),
            3,
            "the repeated post reference must dedupe"
        );
        let digest = SiteSnapshotDigest::parse(&format!("site-b3-v1-{}", "44".repeat(32))).unwrap();
        let public = materialize_public_assets(selected, &digest).unwrap();
        let paths: Vec<_> = [
            "assets/favicon.png",
            "assets/first-cover.png",
            "assets/public.png",
        ]
        .map(|path| {
            SnapshotAssetPath::new(&digest, &LogicalAssetPath::parse(path).unwrap()).unwrap()
        })
        .into();
        assert_eq!(public.keys().cloned().collect::<Vec<_>>(), paths);
        let authored_path = SnapshotAssetPath::new(
            &digest,
            &LogicalAssetPath::parse("assets/public.png").unwrap(),
        )
        .unwrap();
        let authored_png = public.get(&authored_path).unwrap();
        assert_eq!(
            authored_png.digest,
            catalog_asset(&fixture, "assets/public.png").digest
        );
        assert_eq!(authored_png.delivery.content_type(), "image/png");
        assert!(matches!(authored_png.delivery, AssetDelivery::Inline(_)));
        let missing = DigestedAsset::new(
            LogicalAssetPath::parse("assets/missing.png").unwrap(),
            digest_asset(b"missing"),
        );
        let error = insert_authored_asset(
            &mut SelectedAssets::new(),
            &missing,
            &fixture.catalog.local_assets,
        )
        .unwrap_err();
        assert_eq!(error.code, SiteSnapshotBuildErrorCode::AssetUnavailable);
        assert!(error.message.contains("not present"));

        let mismatched = DigestedAsset::new(favicon.path.clone(), digest_asset(b"changed"));
        let error = insert_authored_asset(
            &mut SelectedAssets::new(),
            &mismatched,
            &fixture.catalog.local_assets,
        )
        .unwrap_err();
        assert_eq!(error.code, SiteSnapshotBuildErrorCode::AssetUnavailable);
        assert!(error.message.contains("does not match"));

        let mut collision = SelectedAssets::new();
        insert_authored_asset(&mut collision, &favicon, &fixture.catalog.local_assets).unwrap();
        let conflicting = DigestedAsset::new(favicon.path.clone(), digest_asset(b"conflicting"));
        let error = collision
            .insert(conflicting, Arc::from(&b"conflicting"[..]))
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
    fn chronological_neighbors_cover_both_boundaries_and_the_middle() {
        assert_eq!(
            chronological_neighbors(3, &[2, 0, 1]),
            vec![
                ChronologicalNeighbors {
                    previous: Some(1),
                    next: Some(2),
                },
                ChronologicalNeighbors {
                    previous: None,
                    next: Some(0),
                },
                ChronologicalNeighbors {
                    previous: Some(0),
                    next: None,
                },
            ]
        );
    }

    #[test]
    fn public_post_navigation_uses_only_canonical_chronological_neighbors() {
        let fixture = fixture();
        let ledger = projection([
            entry(&fixture, FIRST_ID, 1_000),
            entry(&fixture, SECOND_ID, 2_000),
        ]);
        let snapshot = build_snapshot(&fixture, &ledger).unwrap();
        let first = snapshot
            .post_page(&PostSlug::parse("first-post").unwrap())
            .unwrap();
        let second = snapshot
            .post_page(&PostSlug::parse("second-post").unwrap())
            .unwrap();

        assert!(first.contains("class=\"maincopy-post-page\""));
        assert!(first.contains("maincopy-post-navigation-next"));
        assert!(first.contains("href=\"/posts/second-post\" rel=\"next\""));
        assert!(!first.contains("maincopy-post-navigation-previous"));
        assert!(!first.contains("draft-post"));

        assert!(second.contains("maincopy-post-navigation-previous"));
        assert!(second.contains("href=\"/posts/first-post\" rel=\"prev\""));
        assert!(second.contains("First &lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!second.contains("maincopy-post-navigation-next"));
        assert!(!second.contains("draft-post"));

        let preview = render_post_preview(
            &fixture.catalog,
            embedded_manifest(),
            &PostId::parse(DRAFT_ID).unwrap(),
            "/api/admin/v1/preview-assets/navigation",
            None,
        )
        .unwrap()
        .unwrap();
        assert!(preview.contains("class=\"maincopy-post-page\""));
        assert!(!preview.contains("maincopy-post-navigation"));
    }

    #[test]
    fn one_public_post_omits_empty_navigation() {
        let fixture = fixture();
        let ledger = projection([entry(&fixture, FIRST_ID, 2_000)]);
        let snapshot = build_snapshot(&fixture, &ledger).unwrap();
        let page = snapshot
            .post_page(&PostSlug::parse("first-post").unwrap())
            .unwrap();
        assert!(!page.contains("maincopy-post-navigation"));
    }

    #[test]
    fn site_shell_identity_binds_exact_discovery_document_representations() {
        let fixture = fixture();
        let ledger = projection([entry(&fixture, FIRST_ID, 2_000)]);
        let posts = select_public_posts(&fixture.catalog, &ledger).unwrap();
        let chronology = chronology(&posts);
        let post_navigation = chronological_neighbors(posts.len(), &chronology);
        let tags = tag_index(&posts, &chronology);
        let redirects = alias_redirect_index(&posts).unwrap();
        let feed = render_public_feed(&fixture.catalog.publication, &posts, &chronology).unwrap();
        let sitemap = render_public_sitemap(&fixture.catalog.publication, &posts, &tags).unwrap();
        let robots = render_public_robots(&fixture.catalog.publication).unwrap();
        let original = render_pre_injection_shell(
            &PageRenderer::new(
                &fixture.catalog.publication,
                embedded_manifest(),
                &fixture.catalog.site_assets,
                HeadAssetProjection::Identity,
            )
            .unwrap(),
            PublicPagePlan {
                posts: &posts,
                chronology: &chronology,
                post_navigation: &post_navigation,
                tags: &tags,
                redirects: &redirects,
            },
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
            &PageRenderer::new(
                &fixture.catalog.publication,
                embedded_manifest(),
                &fixture.catalog.site_assets,
                HeadAssetProjection::Identity,
            )
            .unwrap(),
            PublicPagePlan {
                posts: &posts,
                chronology: &chronology,
                post_navigation: &post_navigation,
                tags: &tags,
                redirects: &redirects,
            },
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
            &PageRenderer::new(
                &fixture.catalog.publication,
                embedded_manifest(),
                &fixture.catalog.site_assets,
                HeadAssetProjection::Identity,
            )
            .unwrap(),
            PublicPagePlan {
                posts: &posts,
                chronology: &chronology,
                post_navigation: &post_navigation,
                tags: &tags,
                redirects: &redirects,
            },
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
            &PageRenderer::new(
                &fixture.catalog.publication,
                embedded_manifest(),
                &fixture.catalog.site_assets,
                HeadAssetProjection::Identity,
            )
            .unwrap(),
            PublicPagePlan {
                posts: &posts,
                chronology: &chronology,
                post_navigation: &post_navigation,
                tags: &tags,
                redirects: &redirects,
            },
            DiscoveryDocuments {
                feed: &feed,
                robots: &robots,
                sitemap: &changed_sitemap,
            },
        )
        .unwrap();
        let mut changed_redirects = redirects.clone();
        changed_redirects.insert(
            PostAlias::parse("original-first-post").unwrap(),
            Arc::new(CanonicalSiteUrl::for_path(
                &fixture.catalog.publication.site.base_url,
                &PublicPagePath::post(&PostSlug::parse("changed-target").unwrap()),
            )),
        );
        let redirects_changed = render_pre_injection_shell(
            &PageRenderer::new(
                &fixture.catalog.publication,
                embedded_manifest(),
                &fixture.catalog.site_assets,
                HeadAssetProjection::Identity,
            )
            .unwrap(),
            PublicPagePlan {
                posts: &posts,
                chronology: &chronology,
                post_navigation: &post_navigation,
                tags: &tags,
                redirects: &changed_redirects,
            },
            DiscoveryDocuments {
                feed: &feed,
                robots: &robots,
                sitemap: &sitemap,
            },
        )
        .unwrap();

        assert_ne!(original, feed_changed);
        assert_ne!(original, robots_changed);
        assert_ne!(original, sitemap_changed);
        assert_ne!(original, redirects_changed);
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
        let mut changed_redirects = snapshot.redirects.clone();
        changed_redirects.insert(
            PostAlias::parse("original-first-post").unwrap(),
            Arc::new(CanonicalSiteUrl::for_path(
                &fixture.catalog.publication.site.base_url,
                &PublicPagePath::post(&PostSlug::parse("changed-target").unwrap()),
            )),
        );

        let robots_changed = presentation_digest(
            &snapshot.pages,
            &snapshot.redirects,
            &snapshot.not_found,
            &snapshot.method_not_allowed,
            &snapshot.feed,
            &changed_robots,
            &snapshot.sitemap,
        );
        let sitemap_changed = presentation_digest(
            &snapshot.pages,
            &snapshot.redirects,
            &snapshot.not_found,
            &snapshot.method_not_allowed,
            &snapshot.feed,
            &snapshot.robots,
            &changed_sitemap,
        );
        let redirects_changed = presentation_digest(
            &snapshot.pages,
            &changed_redirects,
            &snapshot.not_found,
            &snapshot.method_not_allowed,
            &snapshot.feed,
            &snapshot.robots,
            &snapshot.sitemap,
        );

        assert_ne!(snapshot.presentation_digest, robots_changed);
        assert_ne!(snapshot.presentation_digest, sitemap_changed);
        assert_ne!(snapshot.presentation_digest, redirects_changed);
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
            &next.redirects,
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
    fn snapshot_identity_and_presentation_preserve_canonical_bytes() {
        let fixture = fixture();
        let ledger = projection([
            entry(&fixture, FIRST_ID, 1_000),
            entry(&fixture, SECOND_ID, 2_000),
        ]);
        let snapshot = build_snapshot(&fixture, &ledger).unwrap();
        assert_eq!(
            snapshot.digest.to_string(),
            "site-b3-v1-d58571601459e2f96420e12d8d9e85b15181c181de9767199f45a8d7138e8b66"
        );
        assert_eq!(
            snapshot.presentation_digest,
            PresentationDigest([
                39, 95, 147, 214, 124, 121, 50, 143, 144, 211, 77, 97, 90, 187, 16, 237, 128, 32,
                116, 127, 146, 53, 247, 251, 54, 131, 33, 62, 98, 255, 165, 144,
            ])
        );
    }

    #[test]
    fn snapshot_uses_the_ledger_bound_before_the_caller_changes_it() {
        let fixture = fixture();
        let mut ledger = projection([entry(&fixture, FIRST_ID, 1_000)]);
        let expected = build_snapshot(&fixture, &ledger).unwrap();
        let shell =
            render_site_shell(Arc::clone(&fixture.catalog), embedded_manifest(), &ledger).unwrap();
        ledger = projection([entry(&fixture, SECOND_ID, 1_000)]);

        let snapshot = shell.into_snapshot().unwrap();
        assert_eq!(snapshot.digest, expected.digest);
        assert_eq!(snapshot.presentation_digest, expected.presentation_digest);
        assert_ne!(
            snapshot.digest,
            build_snapshot(&fixture, &ledger).unwrap().digest
        );
    }

    #[test]
    fn shell_and_snapshot_limits_are_inclusive() {
        assert!(validate_page_size(MAX_PAGE_BYTES).is_ok());
        assert_eq!(
            validate_page_size(MAX_PAGE_BYTES + 1).unwrap_err().code,
            SiteSnapshotBuildErrorCode::PageLimitExceeded
        );
        assert!(validate_route_count(0, 0, MAX_PUBLIC_ROUTES - FIXED_PUBLIC_ROUTES).is_ok());
        assert_eq!(
            validate_route_count(0, 0, MAX_PUBLIC_ROUTES - FIXED_PUBLIC_ROUTES + 1)
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

        assert_eq!(
            next_public_asset_bytes(MAX_PUBLIC_ASSETS - 1, 0, 0).unwrap(),
            0
        );
        assert_eq!(
            next_public_asset_bytes(MAX_PUBLIC_ASSETS, 0, 0)
                .unwrap_err()
                .code,
            SiteSnapshotBuildErrorCode::PublicAssetCountLimitExceeded
        );

        assert_eq!(
            next_public_asset_bytes(0, 0, MAX_RETAINED_ASSET_BYTES).unwrap(),
            MAX_RETAINED_ASSET_BYTES
        );
        assert_eq!(
            next_public_asset_bytes(1, MAX_RETAINED_ASSET_BYTES, 1)
                .unwrap_err()
                .code,
            SiteSnapshotBuildErrorCode::RetainedAssetLimitExceeded
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
        for (code, name) in [
            (
                SiteSnapshotBuildErrorCode::FrontendManifestInvalid,
                "frontend_manifest_invalid",
            ),
            (
                SiteSnapshotBuildErrorCode::RevisionUnavailable,
                "revision_unavailable",
            ),
            (SiteSnapshotBuildErrorCode::DraftSelected, "draft_selected"),
            (
                SiteSnapshotBuildErrorCode::RouteCollision,
                "route_collision",
            ),
            (
                SiteSnapshotBuildErrorCode::RouteLimitExceeded,
                "route_limit_exceeded",
            ),
            (
                SiteSnapshotBuildErrorCode::PageLimitExceeded,
                "page_limit_exceeded",
            ),
            (
                SiteSnapshotBuildErrorCode::RetainedHtmlLimitExceeded,
                "retained_html_limit_exceeded",
            ),
            (
                SiteSnapshotBuildErrorCode::AssetUnavailable,
                "asset_unavailable",
            ),
            (
                SiteSnapshotBuildErrorCode::AssetCollision,
                "asset_collision",
            ),
            (
                SiteSnapshotBuildErrorCode::PublicAssetCountLimitExceeded,
                "public_asset_count_limit_exceeded",
            ),
            (
                SiteSnapshotBuildErrorCode::RetainedAssetLimitExceeded,
                "retained_asset_limit_exceeded",
            ),
            (
                SiteSnapshotBuildErrorCode::ArticleProjectionFailed,
                "article_projection_failed",
            ),
            (
                SiteSnapshotBuildErrorCode::RssRenderFailed,
                "rss_render_failed",
            ),
            (
                SiteSnapshotBuildErrorCode::RobotsRenderFailed,
                "robots_render_failed",
            ),
            (
                SiteSnapshotBuildErrorCode::SitemapRenderFailed,
                "sitemap_render_failed",
            ),
            (
                SiteSnapshotBuildErrorCode::MetadataRenderFailed,
                "metadata_render_failed",
            ),
            (
                SiteSnapshotBuildErrorCode::QrCodeGenerationFailed,
                "qr_code_generation_failed",
            ),
            (
                SiteSnapshotBuildErrorCode::IdentityRejected,
                "identity_rejected",
            ),
        ] {
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
    fn image_fixture(favicon: &str, site_image: &str, article_image: &str) -> Fixture {
        let publication_source = format!(
            "[site]\ntitle = \"Images\"\nbase_url = \"https://blog.example.com/\"\ndescription = \"Image metadata.\"\nfavicon = {favicon:?}\nimage = {site_image:?}\n[author]\nname = \"Author\"\n[assets]\nallowed_https_origins = [\"https://cdn.example\"]\n"
        );
        let source = post_source(
            FIRST_ID,
            "Image post",
            PostRoutes {
                slug: "image-post",
                aliases: &[],
            },
            &["images"],
            Some(article_image),
            "Image metadata.",
            false,
        );
        let tree = content_tree(
            publication("publication.toml", publication_source),
            vec![post("posts/image.md", PostCollection::Posts, source)],
            ["favicon.png", "site.png", "article.png"]
                .into_iter()
                .map(|name| {
                    asset(
                        LogicalAssetPath::parse(&format!("assets/{name}")).unwrap(),
                        name.as_bytes().to_vec(),
                    )
                })
                .collect(),
            0,
        );
        let prepared = prepare_content(&tree).unwrap();
        let catalog = Arc::new(compile_content_catalog(&prepared).unwrap());
        let revisions = catalog
            .rendered_posts()
            .map(|post| (post.document.metadata.id.clone(), post.revision.clone()))
            .collect();
        Fixture { catalog, revisions }
    }

    #[test]
    fn local_favicon_site_and_article_images_use_canonical_snapshot_urls() {
        let fixture = image_fixture(
            "assets/favicon.png",
            "assets/site.png",
            "assets/article.png",
        );
        let snapshot =
            build_snapshot(&fixture, &projection([entry(&fixture, FIRST_ID, 2_000)])).unwrap();
        let prefix = format!("https://blog.example.com/assets/{}/", snapshot.digest);
        let favicon = format!("<link rel=\"icon\" href=\"{prefix}favicon.png\">");
        for page in [
            snapshot.index_page(),
            snapshot.archive_page(),
            snapshot
                .tag_page(&PostTag::parse("images").unwrap())
                .unwrap(),
        ] {
            assert!(page.contains(&favicon));
            assert!(page.contains(&format!(
                "<meta property=\"og:image\" content=\"{prefix}site.png\">"
            )));
        }
        let article = snapshot
            .post_page(&PostSlug::parse("image-post").unwrap())
            .unwrap();
        assert!(article.contains(&favicon));
        assert!(article.contains(&format!(
            "<meta property=\"og:image\" content=\"{prefix}article.png\">"
        )));
        assert_eq!(
            post_json_ld(&article)["image"],
            format!("{prefix}article.png")
        );
        for name in ["favicon.png", "site.png", "article.png"] {
            let path =
                SnapshotAssetPath::parse(&format!("/assets/{}/{name}", snapshot.digest)).unwrap();
            assert!(snapshot.public_asset(&path).is_some());
        }
        let preview = render_post_preview(
            &fixture.catalog,
            embedded_manifest(),
            &PostId::parse(FIRST_ID).unwrap(),
            "/api/admin/v1/preview-assets/example",
            None,
        )
        .unwrap()
        .unwrap();
        assert!(preview.contains("<link rel=\"icon\" href=\"/api/admin/v1/preview-assets/example?path=assets/favicon.png\">"));
        assert!(!preview.contains("property=\"og:image\""));
        assert!(post_json_ld(&preview).get("image").is_none());
        assert!(!preview.contains(&prefix));
    }

    #[test]
    fn external_image_metadata_uses_validated_urls_and_escaped_attributes() {
        let fixture = image_fixture(
            "https://cdn.example/icon.png",
            "https://cdn.example/site.png",
            "https://cdn.example/article.png?x=1&y=2",
        );
        let snapshot =
            build_snapshot(&fixture, &projection([entry(&fixture, FIRST_ID, 2_000)])).unwrap();
        assert!(
            snapshot
                .index_page()
                .contains("<link rel=\"icon\" href=\"https://cdn.example/icon.png\">")
        );
        assert!(
            snapshot
                .index_page()
                .contains("<meta property=\"og:image\" content=\"https://cdn.example/site.png\">")
        );
        let article = snapshot
            .post_page(&PostSlug::parse("image-post").unwrap())
            .unwrap();
        assert!(article.contains(
            "<meta property=\"og:image\" content=\"https://cdn.example/article.png?x=1&amp;y=2\">"
        ));
        assert_eq!(
            post_json_ld(&article)["image"],
            "https://cdn.example/article.png?x=1&y=2"
        );
        assert!(snapshot.assets.is_empty());
        let preview = render_post_preview(
            &fixture.catalog,
            embedded_manifest(),
            &PostId::parse(FIRST_ID).unwrap(),
            "/api/admin/v1/preview-assets/example",
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            post_json_ld(&preview)["image"],
            "https://cdn.example/article.png?x=1&y=2"
        );
    }

    #[test]
    fn site_image_changes_invalidate_snapshot_and_preview_approval_bindings() {
        let first = image_fixture(
            "assets/favicon.png",
            "assets/site.png",
            "assets/article.png",
        );
        let changed = image_fixture(
            "assets/favicon.png",
            "assets/favicon.png",
            "assets/article.png",
        );
        let first_snapshot =
            build_snapshot(&first, &projection([entry(&first, FIRST_ID, 2_000)])).unwrap();
        let changed_snapshot =
            build_snapshot(&changed, &projection([entry(&changed, FIRST_ID, 2_000)])).unwrap();
        assert_ne!(first_snapshot.digest, changed_snapshot.digest);
        let preview = |fixture: &Fixture| {
            render_bound_post_preview(
                &fixture.catalog,
                embedded_manifest(),
                &PostId::parse(FIRST_ID).unwrap(),
                None,
                "/api/admin/v1/preview-assets/example",
                None,
            )
            .unwrap()
            .unwrap()
        };
        assert_ne!(preview(&first).digest, preview(&changed).digest);
        assert_eq!(first.revisions, changed.revisions);
    }
    #[test]
    fn absent_site_image_does_not_substitute_the_favicon() {
        let fixture = fixture();
        let snapshot = build_snapshot(&fixture, &PublicLedgerProjection::empty()).unwrap();
        assert!(snapshot.index_page().contains("<link rel=\"icon\""));
        assert!(!snapshot.index_page().contains("property=\"og:image\""));
    }
}
