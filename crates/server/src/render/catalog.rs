use std::{collections::BTreeMap, sync::Arc};

use markdown_compiler::{
    AssetRevisionReference, LogicalAssetPath, LogicalContentPath, PostId, PostRevisionDigest,
    PublicationSettings, ResolvedContentAssets, ResolvedLocalAssetLookupError,
    ResolvedLocalAssetStore, ResolvedPostAssetLookupError, ResolvedSiteAssets, ValidatedContent,
};
use serde::Serialize;
use thiserror::Error;

#[cfg(test)]
use super::GeneratedPostAsset;
use super::{
    MarkdownRenderError, RenderedPost,
    diagram::{DiagramRenderError, MermaidDiagramRenderer},
    markdown::render_markdown_with_diagrams,
};
#[cfg(test)]
use markdown_compiler::{DigestedAsset, ResolvedPostAssets};

/// Compile one validated candidate into a self-contained immutable catalog.
pub fn compile_content_catalog(
    content: &ValidatedContent,
    assets: &ResolvedContentAssets,
) -> Result<ContentCatalog, CatalogBuildError> {
    let compiler = ContentCompiler::discover().map_err(CatalogBuildError::compiler)?;
    compiler.compile(content, assets)
}

/// One application-owned compilation capability with shared renderer admission.
#[derive(Clone, Debug)]
pub(crate) struct ContentCompiler {
    diagrams: Arc<MermaidDiagramRenderer>,
}

impl ContentCompiler {
    pub(crate) fn discover() -> Result<Self, ContentCompilerInitializationError> {
        let diagrams = MermaidDiagramRenderer::discover()
            .map_err(ContentCompilerInitializationError::Diagram)?;
        Ok(Self {
            diagrams: Arc::new(diagrams),
        })
    }

    pub(crate) fn compile(
        &self,
        content: &ValidatedContent,
        assets: &ResolvedContentAssets,
    ) -> Result<ContentCatalog, CatalogBuildError> {
        compile_content_catalog_with(content, assets, |document, post_assets, site_assets| {
            render_markdown_with_diagrams(document, post_assets, site_assets, &self.diagrams)
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum ContentCompilerInitializationError {
    #[error("initialize the supervised Mermaid renderer")]
    Diagram(#[source] DiagramRenderError),
}

fn compile_content_catalog_with(
    content: &ValidatedContent,
    assets: &ResolvedContentAssets,
    mut render: impl FnMut(
        &markdown_compiler::PostDocument,
        &markdown_compiler::ResolvedPostAssets,
        &ResolvedSiteAssets,
    ) -> Result<RenderedPost, MarkdownRenderError>,
) -> Result<ContentCatalog, CatalogBuildError> {
    if assets.posts.len() != content.posts.len() {
        return Err(CatalogBuildError::candidate_source_mismatch());
    }
    let site_assets = assets
        .site_assets_for(&content.publication)
        .map_err(|error| CatalogBuildError::publication_assets(error.to_string()))?;
    let local_assets = Arc::new(assets.local_assets.clone());
    let mut current_revisions = BTreeMap::new();
    let mut revisions = BTreeMap::new();

    for document in &content.posts {
        let post_assets = assets
            .assets_for(document)
            .map_err(|error| CatalogBuildError::post_assets(document.path.clone(), error))?;
        let rendered =
            render(document, post_assets, site_assets).map_err(CatalogBuildError::render)?;
        let key = (
            rendered.document.metadata.id.clone(),
            rendered.revision.clone(),
        );
        if current_revisions
            .insert(key.0.clone(), key.1.clone())
            .is_some()
        {
            return Err(CatalogBuildError::duplicate(key));
        }
        let revision = CatalogRevision {
            rendered: Arc::new(rendered),
            local_assets: Arc::clone(&local_assets),
        };
        if revisions.insert(key.clone(), revision).is_some() {
            return Err(CatalogBuildError::duplicate(key));
        }
    }
    validate_catalog_generated_assets(
        &local_assets,
        revisions
            .values()
            .map(|revision| revision.rendered.as_ref()),
    )?;

    Ok(ContentCatalog {
        publication: content.publication.clone(),
        site_assets: site_assets.clone(),
        local_assets,
        current_revisions,
        revisions,
    })
}

/// Current candidate revisions plus exact retained historical render inputs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentCatalog {
    pub(super) publication: PublicationSettings,
    pub(super) site_assets: ResolvedSiteAssets,
    pub(super) local_assets: Arc<ResolvedLocalAssetStore>,
    current_revisions: BTreeMap<PostId, PostRevisionDigest>,
    revisions: BTreeMap<(PostId, PostRevisionDigest), CatalogRevision>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CatalogRevision {
    rendered: Arc<RenderedPost>,
    local_assets: Arc<ResolvedLocalAssetStore>,
}

pub(crate) enum PreviewAsset {
    Authored(Arc<[u8]>),
    RendererGenerated(Arc<[u8]>),
}

impl ContentCatalog {
    pub fn get(&self, post_id: &PostId, revision: &PostRevisionDigest) -> Option<&RenderedPost> {
        self.revisions
            .get(&(post_id.clone(), revision.clone()))
            .map(|revision| revision.rendered.as_ref())
    }

    pub(super) fn get_with_local_assets(
        &self,
        post_id: &PostId,
        revision: &PostRevisionDigest,
    ) -> Option<(&RenderedPost, &ResolvedLocalAssetStore)> {
        self.revisions
            .get(&(post_id.clone(), revision.clone()))
            .map(|revision| (revision.rendered.as_ref(), revision.local_assets.as_ref()))
    }

    /// Finds the one revision supplied by this candidate content tree for a post.
    pub(crate) fn current_post(&self, post_id: &PostId) -> Option<&RenderedPost> {
        let revision = self.current_revisions.get(post_id)?;
        self.get(post_id, revision)
    }

    /// Returns only the revisions supplied by the current candidate content tree.
    pub(crate) fn rendered_posts(&self) -> impl ExactSizeIterator<Item = &RenderedPost> {
        self.current_revisions.iter().map(|(post_id, revision)| {
            self.get(post_id, revision)
                .expect("every current catalog key has a retained rendered revision")
        })
    }

    /// Resolves one referenced asset from the current candidate without exposing retained history.
    pub(crate) fn current_preview_asset(
        &self,
        path: &LogicalAssetPath,
    ) -> Result<Option<PreviewAsset>, ResolvedLocalAssetLookupError> {
        let authored = self
            .site_assets
            .favicon
            .iter()
            .chain(self.site_assets.references.iter())
            .chain(self.rendered_posts().flat_map(|post| {
                post.assets
                    .image
                    .iter()
                    .chain(post.assets.references.iter())
            }))
            .find_map(|reference| match reference {
                AssetRevisionReference::Local(asset) if &asset.path == path => Some(asset),
                AssetRevisionReference::Local(_) | AssetRevisionReference::External(_) => None,
            });
        if let Some(reference) = authored {
            let resolved = self.local_assets.resolve(reference)?;
            return Ok(Some(PreviewAsset::Authored(Arc::clone(&resolved.bytes))));
        }

        Ok(self
            .rendered_posts()
            .flat_map(|post| post.generated_assets.iter())
            .find(|generated| &generated.asset.path == path)
            .map(|generated| PreviewAsset::RendererGenerated(Arc::clone(&generated.bytes))))
    }

    /// Atomically retains an explicit set of exact historical revision inputs.
    pub(crate) fn retain_revisions_from(
        &mut self,
        prior: &Self,
        revisions: impl IntoIterator<Item = (PostId, PostRevisionDigest)>,
    ) -> Result<(), CatalogRetentionError> {
        let mut retained = BTreeMap::new();
        for key in revisions {
            if self.revisions.contains_key(&key) {
                continue;
            }
            let revision =
                prior
                    .revisions
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| CatalogRetentionError {
                        post_id: key.0.clone(),
                        revision: key.1.clone(),
                    })?;
            retained.insert(key, revision);
        }
        self.revisions.extend(retained);
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("prior catalog does not retain post {post_id} revision {revision}")]
pub(crate) struct CatalogRetentionError {
    post_id: PostId,
    revision: PostRevisionDigest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogBuildErrorCode {
    ContentCompilerUnavailable,
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
    pub path: LogicalContentPath,
    pub code: CatalogBuildErrorCode,
    pub message: Box<str>,
}

impl CatalogBuildError {
    fn compiler(error: ContentCompilerInitializationError) -> Self {
        Self {
            path: LogicalContentPath::new("<content-catalog>"),
            code: CatalogBuildErrorCode::ContentCompilerUnavailable,
            message: error.to_string().into_boxed_str(),
        }
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
            path: error.path.clone(),
            code: CatalogBuildErrorCode::PostRenderFailed,
            message: error.to_string().into_boxed_str(),
        }
    }

    fn duplicate(key: (PostId, PostRevisionDigest)) -> Self {
        Self {
            path: LogicalContentPath::new("<content-catalog>"),
            code: CatalogBuildErrorCode::DuplicateRevision,
            message: format!(
                "post {} contains duplicate rendered revision {}",
                key.0, key.1
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

fn validate_catalog_generated_assets<'post>(
    local_assets: &ResolvedLocalAssetStore,
    rendered_posts: impl Iterator<Item = &'post RenderedPost>,
) -> Result<(), CatalogBuildError> {
    let mut paths = BTreeMap::new();
    for path in local_assets {
        paths.insert(path.as_str().to_ascii_lowercase(), path.as_str().to_owned());
    }
    for rendered in rendered_posts {
        for generated in &*rendered.generated_assets {
            let path = generated.asset.path.as_str();
            if paths
                .insert(path.to_ascii_lowercase(), path.to_owned())
                .is_some()
            {
                return Err(CatalogBuildError::generated_asset_collision(
                    rendered.document.path.clone(),
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
        let path = asset.asset.path.as_str();
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
    use crate::domain::publication::PublicLedgerProjection;
    use crate::domain::publication::PublishedPostRevision;
    use crate::frontend_assets::embedded_manifest;
    use crate::render::{
        SiteSnapshotBuildErrorCode, build_site_snapshot, render_site_shell, snapshot_store,
    };
    use markdown_compiler::{
        DiscoveredContentTree, LogicalAssetPath, PostCollection, SiteSnapshotDigest,
        resolve_content_assets,
    };
    use time::{Date, Month, OffsetDateTime, Time};

    use crate::content_fixtures::{asset, content_tree, post, publication, validated_content};

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
        content_tree(
            publication("publication.toml", publication_source(title, origins)),
            posts,
            assets,
            0,
        )
    }

    fn compile_tree(tree: DiscoveredContentTree) -> ContentCatalog {
        let content = tree.validate().unwrap();
        let assets = resolve_content_assets(&tree, &content).unwrap();
        compile_content_catalog(&content, &assets).unwrap()
    }

    fn compile(title: &str, origins: &[&str]) -> ContentCatalog {
        compile_tree(tree(title, origins, false))
    }

    #[test]
    fn compiler_clones_share_renderer_admission() {
        let compiler = ContentCompiler::discover().expect("test renderer path must resolve");
        let cloned = compiler.clone();

        assert!(Arc::ptr_eq(&compiler.diagrams, &cloned.diagrams));
    }

    fn local_asset_reference<'assets>(
        assets: &'assets ResolvedPostAssets,
        path: &LogicalAssetPath,
    ) -> &'assets DigestedAsset {
        assets
            .image
            .iter()
            .chain(assets.references.iter())
            .find_map(|reference| match reference {
                AssetRevisionReference::Local(asset) if &asset.path == path => Some(asset),
                AssetRevisionReference::Local(_) | AssetRevisionReference::External(_) => None,
            })
            .expect("fixture reference must be present")
    }

    fn resolved_bytes<'assets>(
        assets: &'assets ResolvedLocalAssetStore,
        reference: &DigestedAsset,
    ) -> &'assets [u8] {
        assets
            .resolve(reference)
            .expect("fixture reference must resolve")
            .bytes
            .as_ref()
    }

    #[test]
    fn catalog_owns_drafts_rendered_revisions_and_exact_local_bytes() {
        let catalog = compile("Catalog", &[]);
        assert_eq!(catalog.revisions.len(), 1);
        assert!(!catalog.revisions.is_empty());
        assert_eq!(catalog.publication.site.title.as_str(), "Catalog");
        assert!(catalog.site_assets.allowed_origins.is_empty());
        let (key, revision) = catalog.revisions.iter().next().unwrap();
        let rendered = revision.rendered.as_ref();
        let path = LogicalAssetPath::parse("assets/cover.png").unwrap();
        let cover = local_asset_reference(&rendered.assets, &path);
        assert_eq!(resolved_bytes(&catalog.local_assets, cover), b"cover");
        assert_eq!(
            rendered.document.metadata.draft,
            markdown_compiler::DraftStatus::Draft
        );
        assert!(
            catalog
                .get(&rendered.document.metadata.id, &rendered.revision)
                .is_some()
        );
        let wrong = PostRevisionDigest::parse(&format!("post-b3-v1-{}", "22".repeat(32))).unwrap();
        assert!(catalog.get(&key.0, &wrong).is_none());
        let wrong_id = PostId::parse(SECOND_ID).unwrap();
        assert!(catalog.get(&wrong_id, &key.1).is_none());
    }

    #[test]
    fn retained_revision_keeps_its_exact_render_and_local_asset_store() {
        fn candidate(body: &str, cover: &[u8]) -> ContentCatalog {
            compile_tree(content_tree(
                publication("publication.toml", publication_source("Catalog", &[])),
                vec![post(
                    "posts/first.md",
                    PostCollection::Posts,
                    post_source(FIRST_ID, "first-post", body, false),
                )],
                vec![asset(
                    LogicalAssetPath::parse("assets/cover.png").unwrap(),
                    cover.to_vec(),
                )],
                0,
            ))
        }

        let prior = candidate("Old body.\n\n![cover](assets/cover.png)\n", b"old cover");
        let mut current = candidate(
            "Current body.\n\n![cover](assets/cover.png)\n",
            b"current cover",
        );
        let post_id = PostId::parse(FIRST_ID).unwrap();
        let prior_revision = prior.current_post(&post_id).unwrap().revision.clone();
        let current_revision = current.current_post(&post_id).unwrap().revision.clone();
        assert_ne!(prior_revision, current_revision);
        let ledger = PublicLedgerProjection::try_from_exact_entries([PublishedPostRevision::new(
            post_id.clone(),
            prior_revision.clone(),
            time::OffsetDateTime::from_unix_timestamp(1_000).unwrap(),
        )])
        .unwrap();

        current
            .retain_revisions_from(&prior, ledger.revision_keys())
            .unwrap();

        assert_eq!(current.revisions.len(), 2);
        assert_eq!(current.rendered_posts().count(), 1);
        assert_eq!(
            current.current_post(&post_id).unwrap().revision,
            current_revision
        );
        let (retained, retained_assets) = current
            .get_with_local_assets(&post_id, &prior_revision)
            .unwrap();
        assert!(retained.article.identity_html.contains("Old body."));
        assert!(!retained.article.identity_html.contains("Current body."));
        let cover = LogicalAssetPath::parse("assets/cover.png").unwrap();
        let retained_cover = local_asset_reference(&retained.assets, &cover);
        assert_eq!(
            resolved_bytes(retained_assets, retained_cover),
            b"old cover"
        );
        let current_rendered = current.current_post(&post_id).unwrap();
        let current_cover = local_asset_reference(&current_rendered.assets, &cover);
        assert_eq!(
            resolved_bytes(&current.local_assets, current_cover),
            b"current cover"
        );
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
            error.code,
            CatalogBuildErrorCode::PublicationAssetsUnavailable
        );
    }

    #[test]
    fn catalog_rejects_asset_bundle_with_ignored_posts_and_bytes() {
        let source = tree("Catalog", &[], true);
        let full_content = source.validate().unwrap();
        let full_assets = resolve_content_assets(&source, &full_content).unwrap();
        let second = LogicalAssetPath::parse("assets/second.pdf").unwrap();
        let second_document = full_content
            .posts
            .iter()
            .find(|document| document.metadata.id.as_str() == SECOND_ID)
            .unwrap();
        let second_assets = full_assets.assets_for(second_document).unwrap();
        let second_reference = local_asset_reference(second_assets, &second);
        assert_eq!(
            resolved_bytes(&full_assets.local_assets, second_reference),
            b"second"
        );
        let subset = validated_content(
            full_content.publication.clone(),
            vec![full_content.posts[0].clone()],
        );

        let error = compile_content_catalog(&subset, &full_assets).unwrap_err();
        assert_eq!(error.code, CatalogBuildErrorCode::CandidateSourceMismatch);
    }

    #[test]
    fn catalog_rejects_post_source_cross_wiring() {
        let source = tree("Catalog", &[], false);
        let source_content = source.validate().unwrap();
        let source_assets = resolve_content_assets(&source, &source_content).unwrap();

        let changed = content_tree(
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
        assert_eq!(error.code, CatalogBuildErrorCode::PostAssetsUnavailable);
    }

    #[test]
    fn equal_public_keys_from_different_policies_remain_candidate_scoped() {
        let old = compile(
            "Catalog",
            &["https://cdn.example.com", "https://unused.example.com"],
        );
        let current = compile("Catalog", &["https://cdn.example.com"]);
        let old_post = old.revisions.values().next().unwrap().rendered.as_ref();
        let current_post = current.revisions.values().next().unwrap().rendered.as_ref();
        assert_eq!(old_post.revision, current_post.revision);

        let snapshot =
            SiteSnapshotDigest::parse(&format!("site-b3-v1-{}", "11".repeat(32))).unwrap();
        let error = old_post
            .project_for_snapshot(&snapshot, &current.site_assets, &current.local_assets)
            .unwrap_err();
        assert_eq!(
            error.code,
            super::super::MarkdownRenderErrorCode::AssetPolicyMismatch
        );
        current_post
            .project_for_snapshot(&snapshot, &current.site_assets, &current.local_assets)
            .unwrap();
    }

    #[test]
    fn catalog_error_wire_contract_is_stable() {
        let source = tree("Catalog", &[], true);
        let full_content = source.validate().unwrap();
        let full_assets = resolve_content_assets(&source, &full_content).unwrap();
        let subset = validated_content(
            full_content.publication.clone(),
            vec![full_content.posts[0].clone()],
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
        assert_eq!(error.code, CatalogBuildErrorCode::GeneratedAssetCollision);

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
        assert_eq!(error.code, CatalogBuildErrorCode::GeneratedAssetCollision);
    }

    #[test]
    fn preview_asset_lookup_uses_current_authored_and_generated_bytes_only() {
        let mut catalog = compile_tree(tree("Catalog", &[], false));
        let authored = LogicalAssetPath::parse("assets/cover.png").unwrap();
        let PreviewAsset::Authored(bytes) = catalog
            .current_preview_asset(&authored)
            .unwrap()
            .expect("referenced authored asset must resolve")
        else {
            panic!("authored asset must retain authored provenance")
        };
        assert_eq!(bytes.as_ref(), b"cover");

        let generated_path = LogicalAssetPath::parse("assets/generated.svg").unwrap();
        let generated = GeneratedPostAsset::from_owned_bytes(
            generated_path.clone(),
            Arc::from(&b"generated preview"[..]),
        );
        let current_key = catalog.current_revisions.first_key_value().unwrap();
        let current_key = (current_key.0.clone(), current_key.1.clone());
        Arc::make_mut(&mut catalog.revisions.get_mut(&current_key).unwrap().rendered)
            .generated_assets = Arc::from([generated]);

        let PreviewAsset::RendererGenerated(bytes) = catalog
            .current_preview_asset(&generated_path)
            .unwrap()
            .expect("current renderer asset must resolve")
        else {
            panic!("renderer asset must retain generated provenance")
        };
        assert_eq!(bytes.as_ref(), b"generated preview");
        assert!(
            catalog
                .current_preview_asset(&LogicalAssetPath::parse("assets/not-present.png").unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn metadata_failure_rejects_the_candidate_without_replacing_the_active_snapshot() {
        let mut catalog = compile_tree(tree("Catalog", &[], true));
        let post_id = PostId::parse(SECOND_ID).unwrap();
        let revision = catalog.current_post(&post_id).unwrap().revision.clone();
        let ledger = PublicLedgerProjection::try_from_exact_entries([PublishedPostRevision::new(
            post_id.clone(),
            revision.clone(),
            OffsetDateTime::from_unix_timestamp(2_000).unwrap(),
        )])
        .unwrap();
        let valid_shell =
            render_site_shell(Arc::new(catalog.clone()), embedded_manifest(), &ledger).unwrap();
        let active = build_site_snapshot(valid_shell, &ledger).unwrap();
        let (reader, _activator) = snapshot_store(active);
        let before = reader.load_full();

        let retained = catalog
            .revisions
            .get_mut(&(post_id.clone(), revision))
            .unwrap();
        Arc::make_mut(&mut retained.rendered)
            .document
            .metadata
            .authored_at = Date::from_calendar_date(-1, Month::January, 1)
            .unwrap()
            .with_time(Time::MIDNIGHT)
            .assume_utc();
        let error = render_site_shell(Arc::new(catalog), embedded_manifest(), &ledger).unwrap_err();

        assert_eq!(error.code, SiteSnapshotBuildErrorCode::MetadataRenderFailed);
        assert_eq!(error.post_id.as_ref(), Some(&post_id));
        assert!(Arc::ptr_eq(&before, &reader.load_full()));
    }

    #[test]
    fn every_catalog_error_code_has_a_stable_wire_value() {
        for (value, expected) in [
            (
                CatalogBuildErrorCode::ContentCompilerUnavailable,
                "content_compiler_unavailable",
            ),
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
