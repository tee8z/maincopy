use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    sync::Arc,
};

use pulldown_cmark::{Event, Options, Parser, Tag};
use serde::Serialize;
use thiserror::Error;

use super::{
    AssetRevisionReference, AuthoredMarkdownDestination, ContentValidationErrors, DigestedAsset,
    DiscoveredContentTree, ExternalAssetOrigin, ExternalAssetUrl, LogicalAssetPath,
    LogicalContentPath, MarkdownDestinationKind, MarkdownDestinationOrdinal, MarkdownSourceRange,
    PostDocument, PostRendererIdentity, PostRevisionDigest, PublicationSettings,
    ResolvedMarkdownDestination, ResolvedPostAssets, ResolvedSiteAssets, ValidatedContent,
    digest_asset,
    identity::{PostContentDigest, digest_post_content, digest_prepared_post_revision},
};

/// Parse and resolve one owned content tree without network I/O.
/// The result keeps each validated document paired with its approved assets.
pub fn prepare_content(
    tree: &DiscoveredContentTree,
) -> Result<PreparedContent, PrepareContentError> {
    let content = tree
        .validate()
        .map_err(PrepareContentError::InvalidContent)?;
    Resolver::new(tree, content).resolve()
}

#[derive(Debug, Error)]
pub enum PrepareContentError {
    #[error("the discovered content tree is not valid")]
    InvalidContent(#[source] ContentValidationErrors),
    #[error(transparent)]
    AssetResolution(#[from] AssetResolutionErrors),
}

/// An immutable, compiler-owned candidate with source-bound asset inputs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedContent {
    publication: PublicationSettings,
    site: ResolvedSiteAssets,
    posts: Vec<PreparedPost>,
    local_assets: ResolvedLocalAssetStore,
    warnings: AssetResolutionWarnings,
}

/// Read-only compiler inputs for composition in a renderer.
#[derive(Clone, Copy)]
pub struct PreparedContentView<'content> {
    pub publication: &'content PublicationSettings,
    pub site_assets: &'content ResolvedSiteAssets,
    pub posts: &'content [PreparedPost],
    pub local_assets: &'content ResolvedLocalAssetStore,
    pub warnings: &'content AssetResolutionWarnings,
}

impl PreparedContent {
    pub fn view(&self) -> PreparedContentView<'_> {
        PreparedContentView {
            publication: &self.publication,
            site_assets: &self.site,
            posts: &self.posts,
            local_assets: &self.local_assets,
            warnings: &self.warnings,
        }
    }
}

/// One validated document, its approved assets, and its cached content identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedPost {
    document: PostDocument,
    assets: ResolvedPostAssets,
    content_digest: PostContentDigest,
}

/// A borrowed post/asset pair that cannot mutate the prepared candidate.
#[derive(Clone, Copy)]
pub struct PreparedPostView<'post> {
    pub document: &'post PostDocument,
    pub assets: &'post ResolvedPostAssets,
}

impl PreparedPost {
    pub fn view(&self) -> PreparedPostView<'_> {
        PreparedPostView {
            document: &self.document,
            assets: &self.assets,
        }
    }

    /// Bind rendered bytes to the already validated source and asset identities.
    pub fn finalize_revision(
        &self,
        renderer: &PostRendererIdentity,
        pre_injection_article: &[u8],
    ) -> PostRevisionDigest {
        digest_prepared_post_revision(
            &self.content_digest,
            &self.assets,
            renderer,
            pre_injection_article,
        )
    }
}

/// Exact discovered bytes paired with the resolver-calculated identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedLocalAsset {
    pub asset: DigestedAsset,
    pub bytes: Arc<[u8]>,
}

/// Immutable local bytes keyed by their portable logical path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedLocalAssetStore {
    assets: BTreeMap<LogicalAssetPath, ResolvedLocalAsset>,
}

impl ResolvedLocalAssetStore {
    pub fn resolve(
        &self,
        reference: &DigestedAsset,
    ) -> Result<&ResolvedLocalAsset, ResolvedLocalAssetLookupError> {
        let Some(asset) = self.assets.get(&reference.path) else {
            return Err(ResolvedLocalAssetLookupError::Missing);
        };
        if asset.asset.path != reference.path || asset.asset.digest != reference.digest {
            return Err(ResolvedLocalAssetLookupError::DigestMismatch);
        }
        Ok(asset)
    }
}

/// Iterates resolved logical paths without bypassing reference-checked byte access.
impl<'assets> IntoIterator for &'assets ResolvedLocalAssetStore {
    type Item = &'assets LogicalAssetPath;
    type IntoIter =
        std::collections::btree_map::Keys<'assets, LogicalAssetPath, ResolvedLocalAsset>;

    fn into_iter(self) -> Self::IntoIter {
        self.assets.keys()
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedLocalAssetLookupError {
    #[error("the local asset is not present in this resolution")]
    Missing,
    #[error("the local asset identity does not match the resolved reference")]
    DigestMismatch,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AllowedOriginOrdinal(NonZeroUsize);

impl AllowedOriginOrdinal {
    pub const fn get(self) -> usize {
        self.0.get()
    }

    const fn new(value: NonZeroUsize) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssetReferenceLocation {
    ContentSource,
    AllowedOrigin {
        ordinal: AllowedOriginOrdinal,
    },
    PublicationFavicon,
    PublicationImage,
    PostPreviewImage,
    Markdown {
        ordinal: MarkdownDestinationOrdinal,
        destination_kind: MarkdownDestinationKind,
    },
    LocalAsset,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetResolutionCode {
    AllowedOriginInvalid,
    AllowedOriginDuplicated,
    AllowedOriginCountExceeded,
    LocalAssetDuplicated,
    LocalAssetPathInvalid,
    LocalAssetMissing,
    ExternalAssetUrlInvalid,
    ExternalAssetOriginNotAllowed,
    MarkdownDestinationCountExceeded,
}

#[derive(Clone, Debug, Eq, Error, PartialEq, Serialize)]
#[error("{path}: {location:?}: {message}")]
pub struct AssetResolutionError {
    pub path: LogicalContentPath,
    pub location: AssetReferenceLocation,
    pub code: AssetResolutionCode,
    pub message: Box<str>,
}

impl AssetResolutionError {
    fn new(
        path: LogicalContentPath,
        location: AssetReferenceLocation,
        code: AssetResolutionCode,
        message: impl Into<Box<str>>,
    ) -> Self {
        Self {
            path,
            location,
            code,
            message: message.into(),
        }
    }

    fn sort_key(&self) -> (&str, AssetReferenceLocation, AssetResolutionCode, &str) {
        (self.path.as_str(), self.location, self.code, &self.message)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("asset resolution failed with {} error(s)", .0.len())]
pub struct AssetResolutionErrors(Vec<AssetResolutionError>);

impl AssetResolutionErrors {
    pub fn errors(&self) -> &[AssetResolutionError] {
        &self.0
    }

    pub fn into_errors(self) -> Vec<AssetResolutionError> {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetResolutionWarningCode {
    ExternalUrlMayBeMutable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AssetResolutionWarning {
    pub path: LogicalContentPath,
    pub location: AssetReferenceLocation,
    pub code: AssetResolutionWarningCode,
    pub url: ExternalAssetUrl,
}

impl AssetResolutionWarning {
    fn sort_key(
        &self,
    ) -> (
        &str,
        AssetReferenceLocation,
        AssetResolutionWarningCode,
        &str,
    ) {
        (
            self.path.as_str(),
            self.location,
            self.code,
            self.url.as_str(),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetResolutionWarnings(Vec<AssetResolutionWarning>);

impl AssetResolutionWarnings {
    pub fn warnings(&self) -> &[AssetResolutionWarning] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

struct Resolver<'input> {
    tree: &'input DiscoveredContentTree,
    content: ValidatedContent,
    allowed_origins: Vec<ExternalAssetOrigin>,
    discovered_assets: BTreeMap<LogicalAssetPath, Arc<[u8]>>,
    resolved_local_assets: BTreeMap<LogicalAssetPath, ResolvedLocalAsset>,
    errors: Vec<AssetResolutionError>,
    warnings: Vec<AssetResolutionWarning>,
}

impl<'input> Resolver<'input> {
    fn new(tree: &'input DiscoveredContentTree, content: ValidatedContent) -> Self {
        Self {
            tree,
            content,
            allowed_origins: Vec::new(),
            discovered_assets: BTreeMap::new(),
            resolved_local_assets: BTreeMap::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn resolve(mut self) -> Result<PreparedContent, PrepareContentError> {
        self.index_local_assets();
        self.resolve_allowed_origins();

        let publication_path = self.tree.publication.path.clone();
        let favicon = self
            .content
            .publication
            .site
            .favicon
            .clone()
            .and_then(|favicon| {
                self.resolve_reference(
                    &publication_path,
                    AssetReferenceLocation::PublicationFavicon,
                    favicon.as_str(),
                )
            });

        let image = self
            .content
            .publication
            .site
            .image
            .clone()
            .and_then(|image| {
                self.resolve_reference(
                    &publication_path,
                    AssetReferenceLocation::PublicationImage,
                    image.as_str(),
                )
            });

        let site = ResolvedSiteAssets::new(
            &self.content.publication,
            favicon,
            image,
            self.allowed_origins.clone(),
            Vec::new(),
        );

        let mut posts = Vec::with_capacity(self.content.posts.len());
        for document in std::mem::take(&mut self.content.posts) {
            posts.push(self.resolve_post(document));
        }

        if !self.errors.is_empty() {
            self.errors
                .sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
            self.errors.dedup();
            return Err(PrepareContentError::AssetResolution(AssetResolutionErrors(
                self.errors,
            )));
        }

        self.warnings
            .sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
        self.warnings.dedup();
        Ok(PreparedContent {
            publication: self.content.publication,
            site,
            posts,
            local_assets: ResolvedLocalAssetStore {
                assets: self.resolved_local_assets,
            },
            warnings: AssetResolutionWarnings(self.warnings),
        })
    }

    fn index_local_assets(&mut self) {
        for asset in &self.tree.assets {
            let path = asset.path.clone();
            if self.discovered_assets.contains_key(&path) {
                self.errors.push(AssetResolutionError::new(
                    LogicalContentPath::new(path.as_str()),
                    AssetReferenceLocation::LocalAsset,
                    AssetResolutionCode::LocalAssetDuplicated,
                    format!("local asset path is duplicated: {path}"),
                ));
                continue;
            }
            self.discovered_assets
                .insert(path, Arc::clone(&asset.bytes));
        }
    }

    fn resolve_allowed_origins(&mut self) {
        let path = self.tree.publication.path.clone();
        let mut canonical_origins = BTreeSet::new();
        for (index, authored) in self
            .content
            .publication
            .assets
            .allowed_https_origins
            .iter()
            .enumerate()
        {
            let Some(ordinal) = index
                .checked_add(1)
                .and_then(NonZeroUsize::new)
                .map(AllowedOriginOrdinal::new)
            else {
                self.errors.push(AssetResolutionError::new(
                    path.clone(),
                    AssetReferenceLocation::ContentSource,
                    AssetResolutionCode::AllowedOriginCountExceeded,
                    "the allowlist contains more origins than this platform can address",
                ));
                break;
            };
            let location = AssetReferenceLocation::AllowedOrigin { ordinal };
            let origin = match ExternalAssetOrigin::parse(authored.as_str()) {
                Ok(origin) => origin,
                Err(error) => {
                    self.errors.push(AssetResolutionError::new(
                        path.clone(),
                        location,
                        AssetResolutionCode::AllowedOriginInvalid,
                        error.to_string(),
                    ));
                    continue;
                }
            };
            if !canonical_origins.insert(origin.as_str().to_owned()) {
                self.errors.push(AssetResolutionError::new(
                    path.clone(),
                    location,
                    AssetResolutionCode::AllowedOriginDuplicated,
                    format!("allowed HTTPS origin is duplicated after normalization: {origin}"),
                ));
                continue;
            }
            self.allowed_origins.push(origin);
        }
        self.allowed_origins
            .sort_by(|left, right| left.as_str().cmp(right.as_str()));
    }

    fn resolve_post(&mut self, document: PostDocument) -> PreparedPost {
        let path = document.path.clone();
        let image = document.metadata.image.as_ref().and_then(|image| {
            self.resolve_reference(
                &path,
                AssetReferenceLocation::PostPreviewImage,
                image.as_str(),
            )
        });

        let mut references = BTreeMap::new();
        let mut markdown_destinations = Vec::new();
        let mut destination_count = 0_usize;
        for (event, range) in
            Parser::new_ext(document.markdown.as_str(), Options::empty()).into_offset_iter()
        {
            let (kind, authored, is_asset) = match event {
                Event::Start(Tag::Image { dest_url, .. }) => {
                    (MarkdownDestinationKind::Image, dest_url, true)
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    let is_asset = self.is_markdown_file_reference(dest_url.as_ref());
                    (MarkdownDestinationKind::Download, dest_url, is_asset)
                }
                _ => continue,
            };

            let Some(next_count) = destination_count.checked_add(1) else {
                self.errors.push(AssetResolutionError::new(
                    path.clone(),
                    AssetReferenceLocation::ContentSource,
                    AssetResolutionCode::MarkdownDestinationCountExceeded,
                    "Markdown contains more destinations than this platform can address",
                ));
                break;
            };
            destination_count = next_count;
            let Some(non_zero_ordinal) = NonZeroUsize::new(destination_count) else {
                continue;
            };
            let ordinal = MarkdownDestinationOrdinal::new(non_zero_ordinal);
            if !is_asset {
                continue;
            }

            let location = AssetReferenceLocation::Markdown {
                ordinal,
                destination_kind: kind,
            };
            let Some(target) = self.resolve_reference(&path, location, authored.as_ref()) else {
                continue;
            };
            references
                .entry(reference_key(&target))
                .or_insert_with(|| target.clone());
            markdown_destinations.push(ResolvedMarkdownDestination::new(
                ordinal,
                MarkdownSourceRange::new(range),
                kind,
                AuthoredMarkdownDestination::new(authored.as_ref()),
                target,
            ));
        }

        let content_digest = digest_post_content(&document);
        let assets = ResolvedPostAssets::from_resolution(
            &document,
            &content_digest,
            &self.allowed_origins,
            image,
            references.into_values().collect(),
            markdown_destinations,
        );
        PreparedPost {
            document,
            assets,
            content_digest,
        }
    }

    fn is_markdown_file_reference(&self, value: &str) -> bool {
        if looks_like_local_asset_namespace(value) {
            return true;
        }
        self.has_allowed_external_origin(value)
    }

    fn resolve_reference(
        &mut self,
        path: &LogicalContentPath,
        location: AssetReferenceLocation,
        authored: &str,
    ) -> Option<AssetRevisionReference> {
        if looks_like_external_reference(authored) {
            return self.resolve_external(path, location, authored);
        }

        let logical_path = match LogicalAssetPath::parse(authored) {
            Ok(path) => path,
            Err(error) => {
                self.errors.push(AssetResolutionError::new(
                    path.clone(),
                    location,
                    AssetResolutionCode::LocalAssetPathInvalid,
                    format!("local asset reference `{authored}` is invalid: {error}"),
                ));
                return None;
            }
        };
        let Some(bytes) = self.discovered_assets.get(&logical_path).cloned() else {
            self.errors.push(AssetResolutionError::new(
                path.clone(),
                location,
                AssetResolutionCode::LocalAssetMissing,
                format!("local asset does not exist: {logical_path}"),
            ));
            return None;
        };
        let resolved = self
            .resolved_local_assets
            .entry(logical_path.clone())
            .or_insert_with(|| ResolvedLocalAsset {
                asset: DigestedAsset::new(logical_path, digest_asset(&bytes)),
                bytes,
            });
        Some(AssetRevisionReference::local(resolved.asset.clone()))
    }

    fn resolve_external(
        &mut self,
        path: &LogicalContentPath,
        location: AssetReferenceLocation,
        authored: &str,
    ) -> Option<AssetRevisionReference> {
        let url = match ExternalAssetUrl::parse(authored) {
            Ok(url) => url,
            Err(error) => {
                self.errors.push(AssetResolutionError::new(
                    path.clone(),
                    location,
                    AssetResolutionCode::ExternalAssetUrlInvalid,
                    error.to_string(),
                ));
                return None;
            }
        };
        if !self.is_allowed(&url) {
            self.errors.push(AssetResolutionError::new(
                path.clone(),
                location,
                AssetResolutionCode::ExternalAssetOriginNotAllowed,
                "external asset origin is not allowlisted",
            ));
            return None;
        }
        if !appears_immutable(&url) {
            self.warnings.push(AssetResolutionWarning {
                path: path.clone(),
                location,
                code: AssetResolutionWarningCode::ExternalUrlMayBeMutable,
                url: url.clone(),
            });
        }
        Some(AssetRevisionReference::external(url))
    }

    fn is_allowed(&self, url: &ExternalAssetUrl) -> bool {
        self.is_allowed_url(url.as_url())
    }

    fn has_allowed_external_origin(&self, authored: &str) -> bool {
        if let Ok(url) = url::Url::parse(authored) {
            return self.is_allowed_url(&url);
        }

        let Some(host) = raw_https_host(authored) else {
            return false;
        };
        self.allowed_origins
            .iter()
            .filter_map(|origin| origin.as_url().host_str())
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
    }

    fn is_allowed_url(&self, url: &url::Url) -> bool {
        self.allowed_origins.iter().any(|origin| {
            origin.as_url().scheme() == url.scheme()
                && origin.as_url().host() == url.host()
                && origin.as_url().port_or_known_default() == url.port_or_known_default()
        })
    }
}

fn reference_key(reference: &AssetRevisionReference) -> (u8, String) {
    let (kind, value) = reference.sort_key();
    (kind, value.to_owned())
}

fn looks_like_external_reference(value: &str) -> bool {
    if value.starts_with("//") || value.contains("://") {
        return true;
    }
    value.find(':').is_some_and(|colon| {
        value
            .find(['/', '\\'])
            .is_none_or(|separator| colon < separator)
    })
}

fn raw_https_host(value: &str) -> Option<&str> {
    let (scheme, remainder) = value.trim().split_once("://")?;
    if !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = remainder[..authority_end]
        .rsplit_once('@')
        .map_or(&remainder[..authority_end], |(_, host)| host);
    if authority.starts_with('[') {
        let end = authority.find(']')?;
        return Some(&authority[1..end]);
    }
    Some(
        authority
            .split_once(':')
            .map_or(authority, |(host, _)| host),
    )
    .filter(|host| !host.is_empty())
}

fn looks_like_local_asset_namespace(value: &str) -> bool {
    if looks_like_external_reference(value) {
        return false;
    }
    let value = value.trim();
    let lowercase = value.to_ascii_lowercase();
    if lowercase == "assets"
        || lowercase.starts_with("assets/")
        || lowercase.starts_with("assets\\")
        || lowercase.starts_with("assets%")
        || lowercase.starts_with("/assets/")
        || lowercase.starts_with("./assets/")
        || lowercase.starts_with("../assets/")
    {
        return true;
    }
    lowercase
        .split(['/', '\\'])
        .any(|component| component == "assets")
}

fn appears_immutable(url: &ExternalAssetUrl) -> bool {
    if url.as_url().query_pairs().any(|(key, value)| {
        !value.is_empty()
            && matches!(
                key.to_ascii_lowercase().as_ref(),
                "v" | "ver" | "version" | "rev" | "revision" | "hash" | "digest"
            )
    }) {
        return true;
    }

    url.as_url().path_segments().is_some_and(|segments| {
        segments.flat_map(version_tokens).any(|token| {
            let token = token.to_ascii_lowercase();
            token.strip_prefix('v').is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
            }) || (token.len() >= 8
                && token.bytes().all(|byte| byte.is_ascii_hexdigit())
                && token.bytes().any(|byte| byte.is_ascii_digit()))
        })
    })
}

fn version_tokens(segment: &str) -> impl Iterator<Item = &str> {
    segment.split(|character: char| !character.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::identity::finalize_post_revision;
    use crate::tree::{asset, post, publication};
    use crate::{ContentValidationCode, DraftStatus, PostCollection};

    const POST_ID: &str = "4f054633-2d09-4b05-97d0-c6f0011a5199";
    const SECOND_POST_ID: &str = "7d97b17a-686d-46f4-ad77-234f4973c69a";

    fn publication_source(favicon: Option<&str>, origins: &[&str]) -> String {
        let favicon = favicon
            .map(|value| format!("favicon = {value:?}\n"))
            .unwrap_or_default();
        let origins = origins
            .iter()
            .map(|origin| format!("{origin:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "[site]\n\
             title = \"Field Notes\"\n\
             base_url = \"https://blog.example.com/\"\n\
             description = \"Resolver tests.\"\n\
             {favicon}\n\
             [author]\n\
             name = \"Example Author\"\n\
             \n\
             [assets]\n\
             allowed_https_origins = [{origins}]\n"
        )
    }

    fn post_source(id: &str, slug: &str, image: Option<&str>, draft: bool, body: &str) -> String {
        let image = image
            .map(|value| format!("image = {value:?}\n"))
            .unwrap_or_default();
        format!(
            "+++\n\
             id = \"{id}\"\n\
             title = \"{slug}\"\n\
             slug = \"{slug}\"\n\
             authored_at = 2026-08-29T15:00:00-04:00\n\
             description = \"Resolver fixture.\"\n\
             {image}\
             draft = {draft}\n\
             +++\n\
             {body}"
        )
    }

    fn content_tree(
        publication_contents: String,
        posts: Vec<(&str, PostCollection, String)>,
        assets: Vec<(&str, &[u8])>,
    ) -> DiscoveredContentTree {
        DiscoveredContentTree::new(
            publication("publication.toml", publication_contents),
            posts
                .into_iter()
                .map(|(path, collection, source)| post(path, collection, source))
                .collect(),
            assets
                .into_iter()
                .map(|(path, bytes)| {
                    asset(
                        LogicalAssetPath::parse(path).expect("fixture asset path must parse"),
                        bytes.to_vec(),
                    )
                })
                .collect(),
            0,
        )
    }

    fn resolve(tree: &DiscoveredContentTree) -> PreparedContent {
        prepare_content(tree).expect("fixture content and assets must validate")
    }

    #[test]
    fn preparation_rejects_duplicate_posts_before_resolving_their_assets() {
        let tree = content_tree(
            publication_source(None, &[]),
            vec![
                (
                    "posts/first.md",
                    PostCollection::Posts,
                    post_source(
                        POST_ID,
                        "first",
                        None,
                        false,
                        "![missing](assets/missing.png)",
                    ),
                ),
                (
                    "posts/second.md",
                    PostCollection::Posts,
                    post_source(POST_ID, "second", None, false, "Second"),
                ),
            ],
            Vec::new(),
        );
        let PrepareContentError::InvalidContent(errors) = prepare_content(&tree).unwrap_err()
        else {
            panic!("invalid documents must fail before asset resolution")
        };
        assert_eq!(errors.errors().len(), 1);
        assert_eq!(
            errors.errors()[0].code,
            ContentValidationCode::DuplicatePostId
        );
    }

    #[test]
    fn prepared_pairs_and_revision_bytes_are_stable_across_discovery_order() {
        let mut tree = content_tree(
            publication_source(Some("assets/cover.png"), &["https://cdn.example.com"]),
            vec![
                (
                    "posts/z-assets.md",
                    PostCollection::Posts,
                    post_source(
                        POST_ID,
                        "assets",
                        Some("assets/cover.png"),
                        false,
                        "![z](https://cdn.example.com/z-v1.png)\n\
                         [file](assets/file.pdf)\n\
                         ![a](https://cdn.example.com/a-v1.png)\n\
                         ![cover](assets/cover.png)\n\
                         [repeated](assets/file.pdf)",
                    ),
                ),
                (
                    "drafts/a-empty.md",
                    PostCollection::Drafts,
                    post_source(SECOND_POST_ID, "empty", None, true, "No assets."),
                ),
            ],
            vec![("assets/file.pdf", b"file"), ("assets/cover.png", b"cover")],
        );
        let prepared = prepare_content(&tree).unwrap();
        tree.posts.reverse();
        tree.assets.reverse();
        assert_eq!(prepared, prepare_content(&tree).unwrap());
        drop(tree);

        let content = prepared.view();
        assert_eq!(
            content.posts[0].view().document.metadata.id.as_str(),
            SECOND_POST_ID
        );
        assert!(content.posts[0].view().assets.references.is_empty());
        assert_eq!(
            content.posts[1].view().document.metadata.id.as_str(),
            POST_ID
        );
        assert_eq!(content.posts[1].view().assets.references.len(), 4);
        let renderer = PostRendererIdentity::baseline();
        for post in content.posts {
            let inputs = post.view();
            let article = inputs.document.markdown.as_str().as_bytes();
            assert_eq!(
                post.finalize_revision(&renderer, article),
                finalize_post_revision(
                    inputs.document,
                    inputs.assets,
                    content.site_assets,
                    &renderer,
                    article,
                    &[]
                )
                .expect("prepared inputs must satisfy the checked identity API"),
            );
        }
    }

    #[test]
    fn resolves_site_post_and_occurrence_level_markdown_assets() {
        let body = "[ordinary](https://example.org/read)\n\
                    ![diagram](assets/images/diagram.png)\n\
                    [manual](assets/files/manual.pdf)\n\
                    [manual again](assets/files/manual.pdf)\n\
                    [CDN download](https://cdn.example.com/files/manual.pdf?version=2)";
        let tree = content_tree(
            publication_source(
                Some("assets/site/favicon.png"),
                &["HTTPS://CDN.EXAMPLE.COM:443"],
            ),
            vec![(
                "posts/resolver.md",
                PostCollection::Posts,
                post_source(
                    POST_ID,
                    "resolver",
                    Some("https://cdn.example.com/posts/cover-v1.webp"),
                    false,
                    body,
                ),
            )],
            vec![
                ("assets/site/favicon.png", b"favicon"),
                ("assets/images/diagram.png", b"diagram"),
                ("assets/files/manual.pdf", b"manual"),
            ],
        );

        let resolved = resolve(&tree);
        assert!(resolved.warnings.is_empty());
        assert_eq!(
            resolved.site.allowed_origins[0].as_str(),
            "https://cdn.example.com/"
        );
        let Some(AssetRevisionReference::Local(favicon)) = resolved.site.favicon.as_ref() else {
            panic!("favicon must resolve to a local asset")
        };
        assert_eq!(favicon.path.as_str(), "assets/site/favicon.png");
        assert_eq!(
            resolved
                .local_assets
                .resolve(favicon)
                .expect("favicon bytes must be retained")
                .bytes
                .as_ref(),
            b"favicon"
        );

        let post = &resolved.posts[0];
        let Some(AssetRevisionReference::External(image)) = post.assets.image.as_ref() else {
            panic!("preview must resolve to an external URL")
        };
        assert_eq!(
            image.as_str(),
            "https://cdn.example.com/posts/cover-v1.webp"
        );
        assert_eq!(post.assets.references.len(), 3);

        let occurrences = &post.assets.markdown_destinations;
        assert_eq!(occurrences.len(), 4);
        assert_eq!(
            occurrences
                .iter()
                .map(|occurrence| occurrence.ordinal.get())
                .collect::<Vec<_>>(),
            vec![2, 3, 4, 5]
        );
        assert_eq!(occurrences[0].kind, MarkdownDestinationKind::Image);
        assert_eq!(occurrences[1].kind, MarkdownDestinationKind::Download);
        assert_eq!(occurrences[1].authored.as_str(), "assets/files/manual.pdf");
        assert!(document_range(&tree, occurrences[0].source_range).starts_with("![diagram]"));
        assert_eq!(
            reference_key(&occurrences[1].target),
            reference_key(&occurrences[2].target)
        );
        assert_eq!(resolved.local_assets.assets.len(), 3);
    }

    #[test]
    fn aggregates_invalid_origins_urls_missing_files_and_namespace_traversal() {
        let tree = content_tree(
            publication_source(
                Some("assets/missing.ico"),
                &[
                    "https://cdn.example.com",
                    "HTTPS://CDN.EXAMPLE.COM:443/",
                    "http://insecure.example.com",
                ],
            ),
            vec![(
                "posts/errors.md",
                PostCollection::Posts,
                post_source(
                    POST_ID,
                    "errors",
                    Some("http://cdn.example.com/cover.png"),
                    false,
                    "![sibling](https://images.cdn.example.com/image-v1.png)\n\
                     ![bad port](https://cdn.example.com:invalid/image.png)\n\
                     [escape](../assets/secret.pdf)",
                ),
            )],
            Vec::new(),
        );
        let error = prepare_content(&tree).expect_err("assets must fail");
        let PrepareContentError::AssetResolution(errors) = error else {
            panic!("resolution diagnostics were expected")
        };
        let codes: BTreeSet<_> = errors.errors().iter().map(|error| error.code).collect();
        assert_eq!(
            codes,
            BTreeSet::from([
                AssetResolutionCode::AllowedOriginDuplicated,
                AssetResolutionCode::AllowedOriginInvalid,
                AssetResolutionCode::ExternalAssetOriginNotAllowed,
                AssetResolutionCode::ExternalAssetUrlInvalid,
                AssetResolutionCode::LocalAssetMissing,
                AssetResolutionCode::LocalAssetPathInvalid,
            ])
        );
        assert!(
            errors
                .errors()
                .windows(2)
                .all(|pair| { pair[0].sort_key() <= pair[1].sort_key() })
        );
    }

    #[test]
    fn rejects_credentials_fragments_and_malformed_ports_for_each_external_asset_surface() {
        for invalid in [
            "https://user@cdn.example.com/image.png",
            "https://cdn.example.com/image.png#section",
            "https://cdn.example.com:/image.png",
            "https://cdn.example.com:65536/image.png",
        ] {
            let tree = content_tree(
                publication_source(None, &["https://cdn.example.com"]),
                vec![(
                    "posts/invalid.md",
                    PostCollection::Posts,
                    post_source(POST_ID, "invalid", Some(invalid), false, "Body"),
                )],
                Vec::new(),
            );
            let PrepareContentError::AssetResolution(errors) =
                prepare_content(&tree).expect_err("URL must fail")
            else {
                panic!("resolution diagnostics were expected")
            };
            assert_eq!(errors.errors().len(), 1, "invalid value: {invalid}");
            assert_eq!(
                errors.errors()[0].code,
                AssetResolutionCode::ExternalAssetUrlInvalid,
                "invalid value: {invalid}"
            );
        }

        let tree = content_tree(
            publication_source(
                Some("https://user@cdn.example.com/favicon.png"),
                &["https://cdn.example.com"],
            ),
            vec![(
                "posts/downloads.md",
                PostCollection::Posts,
                post_source(
                    POST_ID,
                    "downloads",
                    None,
                    false,
                    "[fragment](https://cdn.example.com/file.pdf#page)\n\
                     [userinfo](https://token@cdn.example.com/file.pdf)\n\
                     [port](https://cdn.example.com:invalid/file.pdf)",
                ),
            )],
            Vec::new(),
        );
        let PrepareContentError::AssetResolution(errors) =
            prepare_content(&tree).expect_err("URLs must fail")
        else {
            panic!("resolution diagnostics were expected")
        };
        assert_eq!(errors.errors().len(), 4);
        assert!(errors.errors().iter().all(|error| {
            error.code == AssetResolutionCode::ExternalAssetUrlInvalid
                && !error.message.contains("token")
                && !error.message.contains("user")
        }));
    }

    #[test]
    fn resolver_rejects_raw_backslashes_before_whatwg_can_normalize_them() {
        let tree = content_tree(
            publication_source(
                Some("https://cdn.example.com\\favicon.png"),
                &["https://cdn.example.com"],
            ),
            vec![(
                "posts/raw-url.md",
                PostCollection::Posts,
                post_source(
                    POST_ID,
                    "raw-url",
                    Some("https://cdn.example.com\\cover.png"),
                    false,
                    "![backslash](https://cdn.example.com\\image.png)",
                ),
            )],
            Vec::new(),
        );
        let PrepareContentError::AssetResolution(errors) =
            prepare_content(&tree).expect_err("raw backslashes must fail")
        else {
            panic!("resolution diagnostics were expected")
        };
        assert_eq!(errors.errors().len(), 3);
        assert!(
            errors
                .errors()
                .iter()
                .all(|error| error.code == AssetResolutionCode::ExternalAssetUrlInvalid)
        );
    }

    #[test]
    fn ordinary_links_bypass_asset_policy_but_asset_namespace_attempts_do_not() {
        let ordinary_tree = content_tree(
            publication_source(None, &["https://cdn.example.com"]),
            vec![(
                "posts/links.md",
                PostCollection::Posts,
                post_source(
                    POST_ID,
                    "links",
                    None,
                    false,
                    "[site](http://example.org) \
                     [remote file](https://example.org/file.pdf) \
                     [different port](https://cdn.example.com:8443/file.pdf)",
                ),
            )],
            Vec::new(),
        );
        let ordinary = resolve(&ordinary_tree);
        assert!(ordinary.posts[0].assets.markdown_destinations.is_empty());

        for invalid in [
            "assets/../private.txt",
            "ASSETS/private.txt",
            "./assets/private.txt",
            "assets%2fprivate.txt",
        ] {
            let tree = content_tree(
                publication_source(None, &[]),
                vec![(
                    "posts/escape.md",
                    PostCollection::Posts,
                    post_source(
                        POST_ID,
                        "escape",
                        None,
                        false,
                        &format!("[file]({invalid})"),
                    ),
                )],
                Vec::new(),
            );
            let PrepareContentError::AssetResolution(errors) =
                prepare_content(&tree).expect_err("path must fail")
            else {
                panic!("resolution diagnostics were expected")
            };
            assert_eq!(
                errors.errors()[0].code,
                AssetResolutionCode::LocalAssetPathInvalid,
                "invalid value: {invalid}"
            );
        }
    }

    #[test]
    fn warnings_are_typed_deterministic_and_occurrence_scoped() {
        let first = content_tree(
            publication_source(
                Some("https://cdn.example.com/favicon.png"),
                &["https://cdn.example.com"],
            ),
            vec![
                (
                    "posts/zulu.md",
                    PostCollection::Posts,
                    post_source(
                        POST_ID,
                        "zulu",
                        None,
                        false,
                        "![mutable](https://cdn.example.com/image.png)",
                    ),
                ),
                (
                    "posts/alpha.md",
                    PostCollection::Posts,
                    post_source(
                        SECOND_POST_ID,
                        "alpha",
                        Some("https://cdn.example.com/cover.png"),
                        false,
                        "[versioned](https://cdn.example.com/manual.pdf?revision=three)",
                    ),
                ),
            ],
            Vec::new(),
        );
        let second = content_tree(
            publication_source(
                Some("https://cdn.example.com/favicon.png"),
                &["https://cdn.example.com"],
            ),
            first
                .posts
                .iter()
                .rev()
                .map(|post| (post.path.as_str(), post.collection, post.source.to_string()))
                .collect(),
            Vec::new(),
        );

        let first = resolve(&first);
        let second = resolve(&second);
        assert_eq!(first.warnings, second.warnings);
        assert_eq!(first.warnings.warnings().len(), 3);
        assert!(first.warnings.warnings().iter().all(|warning| {
            warning.code == AssetResolutionWarningCode::ExternalUrlMayBeMutable
        }));
        assert!(
            first
                .warnings
                .warnings()
                .iter()
                .any(|warning| matches!(warning.location, AssetReferenceLocation::Markdown { .. }))
        );
    }

    #[test]
    fn shared_local_assets_keep_occurrences_and_refresh_identity_when_bytes_change() {
        let make_tree = |bytes: &'static [u8]| {
            content_tree(
                publication_source(Some("assets/shared.png"), &[]),
                vec![(
                    "posts/local.md",
                    PostCollection::Posts,
                    post_source(
                        POST_ID,
                        "local",
                        Some("assets/shared.png"),
                        false,
                        "![local](assets/shared.png)\n[download](assets/shared.png)",
                    ),
                )],
                vec![("assets/shared.png", bytes)],
            )
        };
        let first = resolve(&make_tree(b"first"));
        let second = resolve(&make_tree(b"second"));
        assert_eq!(first.local_assets.assets.len(), 1);
        assert_eq!(first.posts[0].assets.image, first.site.favicon);
        let occurrences = &first.posts[0].assets.markdown_destinations;
        assert_eq!(occurrences.len(), 2);
        for occurrence in occurrences {
            assert_eq!(Some(&occurrence.target), first.site.favicon.as_ref());
        }
        let Some(AssetRevisionReference::Local(first_favicon)) = first.site.favicon.as_ref() else {
            panic!("local favicon was expected")
        };
        let Some(AssetRevisionReference::Local(second_favicon)) = second.site.favicon.as_ref()
        else {
            panic!("local favicon was expected")
        };
        assert_ne!(first_favicon.digest, second_favicon.digest);
        assert_eq!(
            first
                .local_assets
                .resolve(first_favicon)
                .expect("matching reference must resolve")
                .bytes
                .as_ref(),
            b"first"
        );
        assert_eq!(
            second.local_assets.resolve(first_favicon),
            Err(ResolvedLocalAssetLookupError::DigestMismatch)
        );
    }

    #[test]
    fn duplicate_discovered_asset_paths_are_rejected() {
        let publication_contents = publication_source(Some("assets/favicon.png"), &[]);
        let tree = DiscoveredContentTree::new(
            publication("publication.toml", publication_contents),
            vec![post(
                "posts/post.md",
                PostCollection::Posts,
                post_source(POST_ID, "post", None, false, "Body"),
            )],
            vec![
                asset(
                    LogicalAssetPath::parse("assets/favicon.png").unwrap(),
                    b"first".to_vec(),
                ),
                asset(
                    LogicalAssetPath::parse("assets/favicon.png").unwrap(),
                    b"second".to_vec(),
                ),
            ],
            0,
        );
        let PrepareContentError::AssetResolution(errors) =
            prepare_content(&tree).expect_err("duplicate asset must fail")
        else {
            panic!("resolution diagnostics were expected")
        };
        assert!(
            errors
                .errors()
                .iter()
                .any(|error| error.code == AssetResolutionCode::LocalAssetDuplicated)
        );
    }

    #[test]
    fn draft_assets_remain_scoped_to_their_source_bound_post() {
        let tree = content_tree(
            publication_source(None, &[]),
            vec![
                (
                    "posts/public.md",
                    PostCollection::Posts,
                    post_source(
                        POST_ID,
                        "public",
                        None,
                        false,
                        "![public](assets/public.png)",
                    ),
                ),
                (
                    "drafts/private.md",
                    PostCollection::Drafts,
                    post_source(
                        SECOND_POST_ID,
                        "private",
                        None,
                        true,
                        "![private](assets/private.png)",
                    ),
                ),
            ],
            vec![
                ("assets/public.png", b"public"),
                ("assets/private.png", b"private"),
            ],
        );
        let resolved = resolve(&tree);
        assert!(resolved.site.references.is_empty());
        let public = resolved
            .posts
            .iter()
            .find(|post| post.document.metadata.draft == DraftStatus::Publishable)
            .expect("publishable post must be present");
        let draft = resolved
            .posts
            .iter()
            .find(|post| post.document.metadata.draft == DraftStatus::Draft)
            .expect("draft post must be present");
        assert_eq!(public.assets.references.len(), 1);
        assert_eq!(draft.assets.references.len(), 1);
        assert_ne!(
            reference_key(&public.assets.references[0]),
            reference_key(&draft.assets.references[0])
        );
    }

    fn document_range(tree: &DiscoveredContentTree, range: MarkdownSourceRange) -> &str {
        let source = &tree.posts[0].source;
        let (_, markdown) = source
            .split_once("+++\n")
            .and_then(|(_, rest)| rest.split_once("+++\n"))
            .expect("fixture frontmatter delimiters must exist");
        &markdown[range.as_range()]
    }

    #[test]
    fn public_diagnostic_enums_have_exhaustive_stable_wire_values() {
        let ordinal = MarkdownDestinationOrdinal::new(NonZeroUsize::MIN);
        let allowed = AllowedOriginOrdinal::new(NonZeroUsize::MIN);
        let locations = [
            (
                AssetReferenceLocation::ContentSource,
                serde_json::json!({"kind": "content_source"}),
            ),
            (
                AssetReferenceLocation::AllowedOrigin { ordinal: allowed },
                serde_json::json!({"kind": "allowed_origin", "ordinal": 1}),
            ),
            (
                AssetReferenceLocation::PublicationFavicon,
                serde_json::json!({"kind": "publication_favicon"}),
            ),
            (
                AssetReferenceLocation::PostPreviewImage,
                serde_json::json!({"kind": "post_preview_image"}),
            ),
            (
                AssetReferenceLocation::Markdown {
                    ordinal,
                    destination_kind: MarkdownDestinationKind::Image,
                },
                serde_json::json!({
                    "kind": "markdown",
                    "ordinal": 1,
                    "destination_kind": "image"
                }),
            ),
            (
                AssetReferenceLocation::LocalAsset,
                serde_json::json!({"kind": "local_asset"}),
            ),
        ];
        for (value, expected) in locations {
            assert_eq!(serde_json::to_value(value).unwrap(), expected);
        }

        let codes = [
            (
                AssetResolutionCode::AllowedOriginInvalid,
                "allowed_origin_invalid",
            ),
            (
                AssetResolutionCode::AllowedOriginDuplicated,
                "allowed_origin_duplicated",
            ),
            (
                AssetResolutionCode::AllowedOriginCountExceeded,
                "allowed_origin_count_exceeded",
            ),
            (
                AssetResolutionCode::LocalAssetDuplicated,
                "local_asset_duplicated",
            ),
            (
                AssetResolutionCode::LocalAssetPathInvalid,
                "local_asset_path_invalid",
            ),
            (
                AssetResolutionCode::LocalAssetMissing,
                "local_asset_missing",
            ),
            (
                AssetResolutionCode::ExternalAssetUrlInvalid,
                "external_asset_url_invalid",
            ),
            (
                AssetResolutionCode::ExternalAssetOriginNotAllowed,
                "external_asset_origin_not_allowed",
            ),
            (
                AssetResolutionCode::MarkdownDestinationCountExceeded,
                "markdown_destination_count_exceeded",
            ),
        ];
        for (value, expected) in codes {
            assert_eq!(serde_json::to_value(value).unwrap(), expected);
        }

        for (value, expected) in [
            (MarkdownDestinationKind::Image, "image"),
            (MarkdownDestinationKind::Download, "download"),
        ] {
            assert_eq!(serde_json::to_value(value).unwrap(), expected);
        }
        assert_eq!(
            serde_json::to_value(AssetResolutionWarningCode::ExternalUrlMayBeMutable).unwrap(),
            "external_url_may_be_mutable"
        );
        assert_eq!(
            serde_json::to_value(ResolvedLocalAssetLookupError::Missing).unwrap(),
            "missing"
        );
        assert_eq!(
            serde_json::to_value(ResolvedLocalAssetLookupError::DigestMismatch).unwrap(),
            "digest_mismatch"
        );
    }
}
