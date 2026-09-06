use std::{collections::BTreeMap, sync::Arc};

use markdown_compiler::{
    AssetRevisionReference, LogicalAssetPath, LogicalContentPath, PostId, PostRevisionDigest,
    PreparedContent, PublicationSettings, ResolvedLocalAssetLookupError, ResolvedLocalAssetStore,
    ResolvedSiteAssets,
};
use serde::Serialize;
use thiserror::Error;

use super::{
    MarkdownRenderError, RenderedPost,
    diagram::{DiagramRenderError, MermaidDiagramRenderer},
    markdown::render_prepared_post,
};
#[cfg(test)]
use markdown_compiler::{DigestedAsset, ResolvedPostAssets};

/// Compile one validated candidate into a self-contained immutable catalog.
pub fn compile_content_catalog(
    content: &PreparedContent,
) -> Result<ContentCatalog, CatalogBuildError> {
    let compiler = ContentCompiler::discover().map_err(CatalogBuildError::compiler)?;
    compiler.compile(content)
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
        content: &PreparedContent,
    ) -> Result<ContentCatalog, CatalogBuildError> {
        let content = content.view();
        let local_assets = Arc::new(content.local_assets.clone());
        let mut current_revisions = BTreeMap::new();
        let mut revisions = BTreeMap::new();

        for post in content.posts {
            let rendered = render_prepared_post(post, content.site_assets, &self.diagrams)
                .map_err(CatalogBuildError::render)?;
            let key = (
                rendered.document.metadata.id.clone(),
                rendered.revision.clone(),
            );
            // Preparation guarantees one document per post ID.
            current_revisions.insert(key.0.clone(), key.1.clone());
            revisions.insert(
                key,
                CatalogRevision {
                    rendered: Arc::new(rendered),
                    local_assets: Arc::clone(&local_assets),
                },
            );
        }

        Ok(ContentCatalog {
            publication: content.publication.clone(),
            site_assets: content.site_assets.clone(),
            local_assets,
            current_revisions,
            revisions,
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum ContentCompilerInitializationError {
    #[error("initialize the supervised Mermaid renderer")]
    Diagram(#[source] DiagramRenderError),
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
    ) -> Result<Option<Arc<[u8]>>, ResolvedLocalAssetLookupError> {
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
        let Some(reference) = authored else {
            return Ok(None);
        };
        let resolved = self.local_assets.resolve(reference)?;
        Ok(Some(Arc::clone(&resolved.bytes)))
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
    PostRenderFailed,
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

    fn render(error: MarkdownRenderError) -> Self {
        Self {
            path: error.path.clone(),
            code: CatalogBuildErrorCode::PostRenderFailed,
            message: error.to_string().into_boxed_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::publication::PublicLedgerProjection;
    use crate::domain::publication::PublishedPostRevision;
    use crate::frontend_assets::embedded_manifest;
    use crate::render::{SiteSnapshotBuildErrorCode, render_site_shell, snapshot_store};
    use markdown_compiler::{
        DiscoveredContentTree, LogicalAssetPath, PostCollection, SiteSnapshotDigest,
        prepare_content,
    };
    use time::{Date, Month, OffsetDateTime, Time};

    use crate::content_fixtures::{asset, content_tree, post, publication};

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
        let content = prepare_content(&tree).unwrap();
        compile_content_catalog(&content).unwrap()
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
    fn preview_asset_lookup_returns_only_referenced_authored_bytes() {
        let catalog = compile_tree(tree("Catalog", &[], false));
        let authored = LogicalAssetPath::parse("assets/cover.png").unwrap();
        let bytes = catalog
            .current_preview_asset(&authored)
            .unwrap()
            .expect("referenced authored asset must resolve");
        assert_eq!(bytes.as_ref(), b"cover");
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
        let active = valid_shell.into_snapshot().unwrap();
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
                CatalogBuildErrorCode::PostRenderFailed,
                "post_render_failed",
            ),
        ] {
            assert_eq!(serde_json::to_value(value).unwrap(), expected);
        }
    }
}
