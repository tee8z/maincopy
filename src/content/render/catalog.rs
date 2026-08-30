use std::{collections::BTreeMap, fmt, sync::Arc};

use serde::Serialize;
use thiserror::Error;

#[cfg(test)]
use super::super::LogicalAssetPath;
use super::super::{
    LogicalContentPath, PostId, PostRevisionDigest, PublicationSettings, ResolvedContentAssets,
    ResolvedLocalAssetStore, ResolvedPostAssetLookupError, ResolvedSiteAssets, ValidatedContent,
};
#[cfg(test)]
use super::GeneratedPostAsset;
use super::{BaselineMarkdownRenderer, MarkdownRenderError, RenderedPost};

/// Compile one validated candidate into a self-contained immutable catalog.
pub fn compile_content_catalog(
    content: &ValidatedContent,
    assets: &ResolvedContentAssets,
) -> Result<ContentCatalog, CatalogBuildError> {
    if assets.posts().len() != content.posts().len() {
        return Err(CatalogBuildError::candidate_source_mismatch());
    }
    let site_assets = assets
        .site_assets_for(content.publication())
        .map_err(|error| CatalogBuildError::publication_assets(error.to_string()))?;
    let renderer = BaselineMarkdownRenderer;
    let mut revisions = BTreeMap::new();

    for document in content.posts() {
        let post_assets = assets
            .assets_for(document)
            .map_err(|error| CatalogBuildError::post_assets(document.path().clone(), error))?;
        let rendered = renderer
            .render(document, post_assets, site_assets)
            .map_err(CatalogBuildError::render)?;
        let key = CatalogKey {
            post_id: rendered.post_id().clone(),
            revision: rendered.revision().clone(),
        };
        if revisions.insert(key.clone(), Arc::new(rendered)).is_some() {
            return Err(CatalogBuildError::duplicate(key));
        }
    }
    validate_catalog_generated_assets(assets.local_assets(), revisions.values().map(Arc::as_ref))?;

    Ok(ContentCatalog {
        publication: content.publication().clone(),
        site_assets: site_assets.clone(),
        local_assets: assets.local_assets().clone(),
        revisions,
    })
}

/// Candidate-scoped rendered revisions and the exact local bytes they need.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentCatalog {
    publication: PublicationSettings,
    site_assets: ResolvedSiteAssets,
    local_assets: ResolvedLocalAssetStore,
    revisions: BTreeMap<CatalogKey, Arc<RenderedPost>>,
}

impl ContentCatalog {
    pub(super) const fn publication(&self) -> &PublicationSettings {
        &self.publication
    }

    pub(super) const fn site_assets(&self) -> &ResolvedSiteAssets {
        &self.site_assets
    }

    #[cfg(test)]
    pub(super) const fn local_assets(&self) -> &ResolvedLocalAssetStore {
        &self.local_assets
    }

    pub(super) const fn projection_scope(&self) -> CatalogProjectionScope<'_> {
        CatalogProjectionScope {
            site_assets: &self.site_assets,
            local_assets: &self.local_assets,
        }
    }

    pub fn len(&self) -> usize {
        self.revisions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.revisions.is_empty()
    }

    pub fn get(&self, post_id: &PostId, revision: &PostRevisionDigest) -> Option<&RenderedPost> {
        self.revisions
            .get(&CatalogKey {
                post_id: post_id.clone(),
                revision: revision.clone(),
            })
            .map(Arc::as_ref)
    }
}

#[derive(Clone, Copy)]
pub(super) struct CatalogProjectionScope<'catalog> {
    site_assets: &'catalog ResolvedSiteAssets,
    local_assets: &'catalog ResolvedLocalAssetStore,
}

impl<'catalog> CatalogProjectionScope<'catalog> {
    pub(super) const fn site_assets(self) -> &'catalog ResolvedSiteAssets {
        self.site_assets
    }

    pub(super) const fn local_assets(self) -> &'catalog ResolvedLocalAssetStore {
        self.local_assets
    }

    #[cfg(test)]
    pub(super) const fn for_test(
        site_assets: &'catalog ResolvedSiteAssets,
        local_assets: &'catalog ResolvedLocalAssetStore,
    ) -> Self {
        Self {
            site_assets,
            local_assets,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CatalogKey {
    post_id: PostId,
    revision: PostRevisionDigest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogBuildErrorCode {
    CandidateSourceMismatch,
    PublicationAssetsUnavailable,
    PostAssetsUnavailable,
    PostRenderFailed,
    GeneratedAssetCollision,
    DuplicateRevision,
}

#[derive(Clone, Debug, Eq, Error, PartialEq, Serialize)]
#[error("{path}: {code:?}: {message}")]
pub struct CatalogBuildError {
    path: LogicalContentPath,
    code: CatalogBuildErrorCode,
    message: Box<str>,
}

impl CatalogBuildError {
    pub const fn path(&self) -> &LogicalContentPath {
        &self.path
    }

    pub const fn code(&self) -> CatalogBuildErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    fn post_assets(path: LogicalContentPath, error: ResolvedPostAssetLookupError) -> Self {
        Self {
            path,
            code: CatalogBuildErrorCode::PostAssetsUnavailable,
            message: error.to_string().into_boxed_str(),
        }
    }

    fn publication_assets(message: String) -> Self {
        Self {
            path: LogicalContentPath::new("publication.toml"),
            code: CatalogBuildErrorCode::PublicationAssetsUnavailable,
            message: message.into_boxed_str(),
        }
    }

    fn candidate_source_mismatch() -> Self {
        Self {
            path: LogicalContentPath::new("<content-catalog>"),
            code: CatalogBuildErrorCode::CandidateSourceMismatch,
            message: "resolved content assets and validated content contain different post sets"
                .into(),
        }
    }

    fn render(error: MarkdownRenderError) -> Self {
        Self {
            path: error.path().clone(),
            code: CatalogBuildErrorCode::PostRenderFailed,
            message: error.to_string().into_boxed_str(),
        }
    }

    fn duplicate(key: CatalogKey) -> Self {
        Self {
            path: LogicalContentPath::new("<content-catalog>"),
            code: CatalogBuildErrorCode::DuplicateRevision,
            message: format!(
                "post {} contains duplicate rendered revision {}",
                key.post_id, key.revision
            )
            .into_boxed_str(),
        }
    }

    fn generated_asset_collision(path: LogicalContentPath, asset: &str) -> Self {
        Self {
            path,
            code: CatalogBuildErrorCode::GeneratedAssetCollision,
            message: format!(
                "generated asset path duplicates or ASCII-case-collides with another asset: {asset}"
            )
            .into_boxed_str(),
        }
    }
}

impl fmt::Display for CatalogBuildErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

fn validate_catalog_generated_assets<'post>(
    local_assets: &ResolvedLocalAssetStore,
    rendered_posts: impl Iterator<Item = &'post RenderedPost>,
) -> Result<(), CatalogBuildError> {
    let mut paths = BTreeMap::new();
    for path in local_assets.paths() {
        paths.insert(path.as_str().to_ascii_lowercase(), path.as_str().to_owned());
    }
    for rendered in rendered_posts {
        for generated in rendered.generated_assets() {
            let path = generated.asset().path().as_str();
            if paths
                .insert(path.to_ascii_lowercase(), path.to_owned())
                .is_some()
            {
                return Err(CatalogBuildError::generated_asset_collision(
                    rendered.document().path().clone(),
                    path,
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn validate_generated_path_sets<'asset, 'path>(
    authored: impl Iterator<Item = &'asset LogicalAssetPath>,
    generated: impl Iterator<Item = (&'path LogicalContentPath, &'asset GeneratedPostAsset)>,
) -> Result<(), CatalogBuildError> {
    let mut paths = BTreeMap::new();
    for path in authored {
        paths.insert(path.as_str().to_ascii_lowercase(), path.as_str().to_owned());
    }
    for (post_path, asset) in generated {
        let path = asset.asset().path().as_str();
        if paths
            .insert(path.to_ascii_lowercase(), path.to_owned())
            .is_some()
        {
            return Err(CatalogBuildError::generated_asset_collision(
                post_path.clone(),
                path,
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::tree::{asset, post, publication};
    use crate::content::{
        DiscoveredContentTree, LogicalAssetPath, PostCollection, SiteSnapshotDigest,
        resolve_content_assets,
    };

    const FIRST_ID: &str = "4f054633-2d09-4b05-97d0-c6f0011a5199";
    const SECOND_ID: &str = "7d97b17a-686d-46f4-ad77-234f4973c69a";

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
             description = \"Catalog tests.\"\n\
             [author]\n\
             name = \"Example Author\"\n\
             [assets]\n\
             allowed_https_origins = [{origins}]\n"
        )
    }

    fn post_source(id: &str, slug: &str, body: &str, draft: bool) -> String {
        format!(
            "+++\n\
             id = {id:?}\n\
             title = {slug:?}\n\
             slug = {slug:?}\n\
             authored_at = 2026-08-29T15:00:00-04:00\n\
             description = \"Catalog fixture.\"\n\
             draft = {draft}\n\
             +++\n\
             {body}"
        )
    }

    fn tree(title: &str, origins: &[&str], include_second: bool) -> DiscoveredContentTree {
        let mut posts = vec![post(
            "drafts/first.md",
            PostCollection::Drafts,
            post_source(FIRST_ID, "first-post", "![cover](assets/cover.png)\n", true),
        )];
        if include_second {
            posts.push(post(
                "posts/second.md",
                PostCollection::Posts,
                post_source(
                    SECOND_ID,
                    "second-post",
                    "[file](assets/second.pdf)\n",
                    false,
                ),
            ));
        }
        let mut assets = vec![asset(
            LogicalAssetPath::parse("assets/cover.png").unwrap(),
            b"cover".to_vec(),
        )];
        if include_second {
            assets.push(asset(
                LogicalAssetPath::parse("assets/second.pdf").unwrap(),
                b"second".to_vec(),
            ));
        }
        DiscoveredContentTree::new(
            publication("publication.toml", publication_source(title, origins)),
            posts,
            assets,
            0,
        )
    }

    fn compile(title: &str, origins: &[&str]) -> ContentCatalog {
        let tree = tree(title, origins, false);
        let content = tree.validate().unwrap();
        let assets = resolve_content_assets(&tree, &content).unwrap();
        compile_content_catalog(&content, &assets).unwrap()
    }

    #[test]
    fn catalog_owns_drafts_rendered_revisions_and_exact_local_bytes() {
        let catalog = compile("Catalog", &[]);
        assert_eq!(catalog.len(), 1);
        assert!(!catalog.is_empty());
        assert_eq!(catalog.publication().site().title().as_str(), "Catalog");
        assert!(catalog.site_assets().allowed_origins().is_empty());
        let path = LogicalAssetPath::parse("assets/cover.png").unwrap();
        assert_eq!(catalog.local_assets().get(&path).unwrap().bytes(), b"cover");

        let (key, rendered) = catalog.revisions.iter().next().unwrap();
        assert_eq!(
            rendered.document().metadata().draft(),
            crate::content::DraftStatus::Draft
        );
        assert!(
            catalog
                .get(rendered.post_id(), rendered.revision())
                .is_some()
        );
        let wrong = PostRevisionDigest::parse(&format!("post-b3-v1-{}", "22".repeat(32))).unwrap();
        assert!(catalog.get(&key.post_id, &wrong).is_none());
        let wrong_id = PostId::parse(SECOND_ID).unwrap();
        assert!(catalog.get(&wrong_id, &key.revision).is_none());
    }

    #[test]
    fn catalog_rejects_publication_asset_cross_wiring_before_rendering() {
        let source = tree("Source", &[], false);
        let source_content = source.validate().unwrap();
        let source_assets = resolve_content_assets(&source, &source_content).unwrap();
        let other = tree("Different publication", &[], false);
        let other_content = other.validate().unwrap();

        let error = compile_content_catalog(&other_content, &source_assets).unwrap_err();
        assert_eq!(
            error.code(),
            CatalogBuildErrorCode::PublicationAssetsUnavailable
        );
    }

    #[test]
    fn catalog_rejects_asset_bundle_with_ignored_posts_and_bytes() {
        let source = tree("Catalog", &[], true);
        let full_content = source.validate().unwrap();
        let full_assets = resolve_content_assets(&source, &full_content).unwrap();
        assert!(
            full_assets
                .local_assets()
                .get(&LogicalAssetPath::parse("assets/second.pdf").unwrap())
                .is_some()
        );
        let subset = ValidatedContent::new(
            full_content.publication().clone(),
            vec![full_content.posts()[0].clone()],
        );

        let error = compile_content_catalog(&subset, &full_assets).unwrap_err();
        assert_eq!(error.code(), CatalogBuildErrorCode::CandidateSourceMismatch);
    }

    #[test]
    fn catalog_rejects_post_source_cross_wiring() {
        let source = tree("Catalog", &[], false);
        let source_content = source.validate().unwrap();
        let source_assets = resolve_content_assets(&source, &source_content).unwrap();

        let changed = DiscoveredContentTree::new(
            publication("publication.toml", publication_source("Catalog", &[])),
            vec![post(
                "drafts/first.md",
                PostCollection::Drafts,
                post_source(FIRST_ID, "first-post", "Changed source.\n", true),
            )],
            vec![],
            0,
        );
        let changed_content = changed.validate().unwrap();
        let error = compile_content_catalog(&changed_content, &source_assets).unwrap_err();
        assert_eq!(error.code(), CatalogBuildErrorCode::PostAssetsUnavailable);
    }

    #[test]
    fn equal_public_keys_from_different_policies_remain_candidate_scoped() {
        let old = compile(
            "Catalog",
            &["https://cdn.example.com", "https://unused.example.com"],
        );
        let current = compile("Catalog", &["https://cdn.example.com"]);
        let old_post = old.revisions.values().next().unwrap();
        let current_post = current.revisions.values().next().unwrap();
        assert_eq!(old_post.revision(), current_post.revision());

        let snapshot =
            SiteSnapshotDigest::parse(&format!("site-b3-v1-{}", "11".repeat(32))).unwrap();
        let error = old_post
            .project_for_snapshot(&snapshot, current.projection_scope())
            .unwrap_err();
        assert_eq!(
            error.code(),
            super::super::MarkdownRenderErrorCode::AssetPolicyMismatch
        );
        current_post
            .project_for_snapshot(&snapshot, current.projection_scope())
            .unwrap();
    }

    #[test]
    fn catalog_error_wire_contract_is_stable() {
        let source = tree("Catalog", &[], true);
        let full_content = source.validate().unwrap();
        let full_assets = resolve_content_assets(&source, &full_content).unwrap();
        let subset = ValidatedContent::new(
            full_content.publication().clone(),
            vec![full_content.posts()[0].clone()],
        );
        let error = compile_content_catalog(&subset, &full_assets).unwrap_err();
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            serde_json::json!({
                "path": "<content-catalog>",
                "code": "candidate_source_mismatch",
                "message": "resolved content assets and validated content contain different post sets"
            })
        );
    }

    #[test]
    fn generated_paths_cannot_collide_with_authored_or_other_generated_assets() {
        let authored = LogicalAssetPath::parse("assets/diagram.svg").unwrap();
        let first_path = LogicalContentPath::new("posts/first.md");
        let second_path = LogicalContentPath::new("posts/second.md");
        let authored_collision = GeneratedPostAsset::from_owned_bytes(
            LogicalAssetPath::parse("assets/DIAGRAM.svg").unwrap(),
            Arc::from(&b"generated"[..]),
        );
        let error = validate_generated_path_sets(
            std::iter::once(&authored),
            std::iter::once((&first_path, &authored_collision)),
        )
        .unwrap_err();
        assert_eq!(error.code(), CatalogBuildErrorCode::GeneratedAssetCollision);

        let first = GeneratedPostAsset::from_owned_bytes(
            LogicalAssetPath::parse("assets/generated.svg").unwrap(),
            Arc::from(&b"first"[..]),
        );
        let second = GeneratedPostAsset::from_owned_bytes(
            LogicalAssetPath::parse("assets/GENERATED.svg").unwrap(),
            Arc::from(&b"second"[..]),
        );
        let error = validate_generated_path_sets(
            std::iter::empty(),
            [(&first_path, &first), (&second_path, &second)].into_iter(),
        )
        .unwrap_err();
        assert_eq!(error.code(), CatalogBuildErrorCode::GeneratedAssetCollision);
    }

    #[test]
    fn every_catalog_error_code_has_a_stable_wire_value() {
        for (value, expected) in [
            (
                CatalogBuildErrorCode::CandidateSourceMismatch,
                "candidate_source_mismatch",
            ),
            (
                CatalogBuildErrorCode::PublicationAssetsUnavailable,
                "publication_assets_unavailable",
            ),
            (
                CatalogBuildErrorCode::PostAssetsUnavailable,
                "post_assets_unavailable",
            ),
            (
                CatalogBuildErrorCode::PostRenderFailed,
                "post_render_failed",
            ),
            (
                CatalogBuildErrorCode::GeneratedAssetCollision,
                "generated_asset_collision",
            ),
            (
                CatalogBuildErrorCode::DuplicateRevision,
                "duplicate_revision",
            ),
        ] {
            assert_eq!(serde_json::to_value(value).unwrap(), expected);
        }
    }
}
