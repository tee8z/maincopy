use std::{collections::BTreeMap, fmt, sync::Arc};

use arc_swap::ArcSwap;
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde::Serialize;
use thiserror::Error;
use time::OffsetDateTime;

use crate::frontend_assets::FrontendAssetManifest;

use super::{ContentCatalog, GeneratedPostAsset, RenderedPost};
use crate::content::identity::{
    SiteShellOutputDigest, SiteShellOutputHasher, finalize_site_snapshot,
};
use crate::content::{
    AssetRevisionReference, DigestedAsset, DraftStatus, LogicalAssetPath, PostDescription, PostId,
    PostRevisionDigest, PostSlug, PostTag, PostTitle, PublicationBaseUrl, PublicationSettings,
    PublishedPostRevision, ResolvedLocalAssetStore, ResolvedPostAssets, RevisionIdentityError,
    SiteShellRendererIdentity, SiteSnapshotDigest, SnapshotAssetPath,
};

const MAX_PAGE_BYTES: usize = 40 * 1024 * 1024;
const MAX_PUBLIC_ROUTES: usize = 50_000;
const MAX_RETAINED_HTML_BYTES: usize = 512 * 1024 * 1024;

/// An exact, storage-neutral view of revisions authorized for one public snapshot.
///
/// Persistence adapters and the snapshot-transition coordinator construct the
/// entries inside the crate. Public callers can explicitly request an empty
/// projection, but cannot infer publication from the content catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicLedgerProjection {
    entries: Arc<[PublishedPostRevision]>,
}

impl PublicLedgerProjection {
    pub fn empty() -> Self {
        Self {
            entries: Arc::from([]),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the SQLite projection adapter lands after the DB-neutral snapshot layer"
        )
    )]
    pub(crate) fn try_from_exact_entries(
        entries: impl IntoIterator<Item = PublishedPostRevision>,
    ) -> Result<Self, PublicLedgerProjectionError> {
        let mut entries: Vec<_> = entries.into_iter().collect();
        entries.sort_by(|left, right| left.post_id.cmp(&right.post_id));
        for pair in entries.windows(2) {
            if pair[0].post_id == pair[1].post_id {
                return Err(PublicLedgerProjectionError {
                    post_id: pair[0].post_id.clone(),
                });
            }
        }
        Ok(Self {
            entries: entries.into(),
        })
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("public ledger contains duplicate post {post_id}")]
pub(crate) struct PublicLedgerProjectionError {
    post_id: PostId,
}

/// A canonical absolute URL derived only from validated publication settings.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CanonicalSiteUrl(Box<str>);

impl CanonicalSiteUrl {
    fn for_path(base: &PublicationBaseUrl, path: &PublicPagePath) -> Self {
        let mut url = base.as_url().clone();
        url.set_path(path.as_str());
        url.set_query(None);
        url.set_fragment(None);
        Self(url.as_str().into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CanonicalSiteUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PublicPagePath(Box<str>);

impl PublicPagePath {
    fn index() -> Self {
        Self("/".into())
    }

    fn archive() -> Self {
        Self("/archive".into())
    }

    fn post(slug: &PostSlug) -> Self {
        Self(format!("/posts/{}", slug.as_str()).into_boxed_str())
    }

    fn tag(tag: &PostTag) -> Self {
        Self(format!("/tags/{}", tag.as_str()).into_boxed_str())
    }

    fn as_str(&self) -> &str {
        &self.0
    }
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
    pre_injection_output: SiteShellOutputDigest,
}

impl fmt::Debug for RenderedSiteShell {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RenderedSiteShell")
            .field("ledger_entries", &self.ledger.entries.len())
            .field("posts", &self.posts.len())
            .field("tags", &self.tags.len())
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

    let renderer = SiteShellRendererIdentity::new(frontend.bundle_digest);
    let pre_injection_output =
        render_pre_injection_shell(&catalog.publication, frontend, &posts, &chronology, &tags)?;

    Ok(RenderedSiteShell {
        catalog,
        frontend,
        ledger: ledger.clone(),
        renderer,
        posts: posts.into(),
        chronology: chronology.into(),
        tags,
        pre_injection_output,
    })
}

fn select_public_posts(
    catalog: &ContentCatalog,
    ledger: &PublicLedgerProjection,
) -> Result<Vec<PublicPostView>, SiteSnapshotBuildError> {
    let mut posts = Vec::with_capacity(ledger.entries.len());
    for entry in &*ledger.entries {
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

fn validate_route_count(posts: usize, tags: usize) -> Result<(), SiteSnapshotBuildError> {
    let routes = posts
        .checked_add(tags)
        .and_then(|count| count.checked_add(4))
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
) -> Result<SiteShellOutputDigest, SiteSnapshotBuildError> {
    let mut pages = BTreeMap::new();
    pages.insert(PublicPagePath::index(), PreInjectionPage::Index);
    pages.insert(PublicPagePath::archive(), PreInjectionPage::Archive);
    for (index, post) in posts.iter().enumerate() {
        pages.insert(post.public_path(), PreInjectionPage::Post(index));
    }
    for tag in tags.keys() {
        pages.insert(PublicPagePath::tag(tag), PreInjectionPage::Tag(tag));
    }
    pages.insert(
        PublicPagePath("<not-found>".into()),
        PreInjectionPage::Error(PublicErrorPage::NotFound),
    );
    pages.insert(
        PublicPagePath("<method-not-allowed>".into()),
        PreInjectionPage::Error(PublicErrorPage::MethodNotAllowed),
    );

    let mut retained = RetainedHtmlBudget::new();
    let mut hasher = SiteShellOutputHasher::new(pages.len());
    for (path, page) in pages {
        let page = match page {
            PreInjectionPage::Index => {
                render_index(publication, frontend, posts, chronology).into_string()
            }
            PreInjectionPage::Archive => {
                render_archive(publication, frontend, posts, chronology).into_string()
            }
            PreInjectionPage::Post(index) => {
                render_post(publication, frontend, &posts[index], ArticleBody::Omitted)
                    .into_string()
            }
            PreInjectionPage::Tag(tag) => render_tag(
                publication,
                frontend,
                tag,
                posts,
                tags.get(tag).map_or(&[], Arc::as_ref),
            )
            .into_string(),
            PreInjectionPage::Error(error) => {
                render_error(publication, frontend, error).into_string()
            }
        };
        validate_page_size(page.len())?;
        retained.add(page.len())?;
        hasher.page(path.as_str(), page.as_bytes());
    }
    Ok(hasher.finish())
}

#[derive(Clone, Copy)]
enum PreInjectionPage<'view> {
    Index,
    Archive,
    Post(usize),
    Tag(&'view PostTag),
    Error(PublicErrorPage),
}

pub fn build_site_snapshot(
    shell: RenderedSiteShell,
    ledger: &PublicLedgerProjection,
) -> Result<SiteSnapshot, SiteSnapshotBuildError> {
    validate_snapshot_shell(&shell, ledger)?;
    let digest = finalize_site_snapshot(
        &shell.catalog.publication,
        &shell.catalog.site_assets,
        &shell.renderer,
        &shell.pre_injection_output,
        &ledger.entries,
    )
    .map_err(SiteSnapshotBuildError::identity)?;

    let mut retained = RetainedHtmlBudget::new();
    let pages = render_snapshot_pages(&shell, &digest, &mut retained)?;
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
    let assets = collect_public_assets(&shell, &digest)?;

    Ok(SiteSnapshot {
        digest,
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
    if shell.renderer.frontend_bundle != shell.frontend.bundle_digest {
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
        let rendered = shell
            .catalog
            .get(&post.post_id, &post.revision)
            .ok_or_else(|| {
                SiteSnapshotBuildError::post(
                    SiteSnapshotBuildErrorCode::RevisionUnavailable,
                    post.post_id.clone(),
                    "the bound post revision disappeared before snapshot projection",
                )
            })?;
        let article = rendered
            .project_for_snapshot(
                digest,
                &shell.catalog.site_assets,
                &shell.catalog.local_assets,
            )
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
                post,
                ArticleBody::Projected(&article),
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
        let rendered = shell
            .catalog
            .get(&post.post_id, &post.revision)
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
            &shell.catalog.local_assets,
        )?;
    }

    materialize_public_assets(selected, digest)
}

fn collect_site_global_assets(
    selected: &mut BTreeMap<LogicalAssetPath, SelectedAsset>,
    site_assets: &crate::content::ResolvedSiteAssets,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotPublicAsset {
    pub path: SnapshotAssetPath,
    pub asset: DigestedAsset,
    pub bytes: Arc<[u8]>,
}

/// Complete immutable request-facing state for one canonical publication.
pub struct SiteSnapshot {
    pub(crate) digest: SiteSnapshotDigest,
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

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "WP 1.5 gives the transition coordinator sole ownership of activation"
    )
)]
pub(crate) struct SiteSnapshotActivator {
    active: Arc<ArcSwap<SiteSnapshot>>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "WP 1.5 gives the transition coordinator sole ownership of activation"
    )
)]
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
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "WP 1.5 gives the transition coordinator sole ownership of activation"
        )
    )]
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
        if current.digest == next.digest {
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

fn render_post(
    publication: &PublicationSettings,
    frontend: &'static FrontendAssetManifest,
    post: &PublicPostView,
    article: ArticleBody<'_>,
) -> Markup {
    let content = html! {
        article {
            header {
                h1 { (post.title.as_str()) }
                p { (post.description.as_str()) }
                p class="publication-time" {
                    "Published "
                    time datetime=(post.published_at.to_string()) {
                        (post.published_at.to_string())
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
                        @for tag in &*post.tags {
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
                link rel="stylesheet" href=(frontend.css.public_path);
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

    use super::*;
    use crate::{
        content::{
            DiscoveredContentTree, LogicalAssetPath, PostCollection, ResolvedPostAssets,
            ResolvedSiteAssets, digest_asset, resolve_content_assets,
            tree::{asset, post, publication},
        },
        frontend_assets::embedded_manifest,
        render::{compile_content_catalog, render_markdown},
    };

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
        let publication_source = "[site]\n\
             title = \"Site <unsafe> & title\"\n\
             base_url = \"https://blog.example.com/\"\n\
             description = \"A <careful> site.\"\n\
             favicon = \"assets/favicon.png\"\n\
             [author]\n\
             name = \"Author <unsafe>\"\n";
        let tree = DiscoveredContentTree::new(
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
                        "# First\n<script>alert('body')</script>\n![public](assets/public.png)\n",
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
                    b"public".to_vec(),
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
        assert_eq!(error.post_id.as_str(), FIRST_ID);
        assert!(PublicLedgerProjection::empty().entries.is_empty());
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

        let reversed = projection([
            entry(&fixture, SECOND_ID, 2_000),
            entry(&fixture, FIRST_ID, 1_000),
        ]);
        let rebuilt = build_snapshot(&fixture, &reversed).unwrap();
        assert_eq!(snapshot.digest, rebuilt.digest);
        assert_eq!(snapshot.index_page(), rebuilt.index_page());
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
        assert!(validate_route_count(MAX_PUBLIC_ROUTES - 4, 0).is_ok());
        assert_eq!(
            validate_route_count(MAX_PUBLIC_ROUTES - 3, 0)
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
                    } else if observed.digest == new_digest {
                        assert!(
                            observed
                                .post_page(&PostSlug::parse("first-post").unwrap())
                                .is_some()
                        );
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
