use std::{num::NonZeroUsize, ops::Range};

use serde::Serialize;

use super::identity::{
    AssetResolutionPolicyBinding, PostAssetSourceBinding, PublicationAssetSourceBinding,
    bind_asset_resolution_policy, bind_post_asset_source, bind_publication_asset_source,
};
use super::{AssetRevisionReference, ExternalAssetOrigin, PostDocument, PublicationSettings};

/// The one-based position of a link or image destination in a Markdown event stream.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct MarkdownDestinationOrdinal(NonZeroUsize);

impl MarkdownDestinationOrdinal {
    pub const fn get(self) -> usize {
        self.0.get()
    }

    pub const fn new(value: NonZeroUsize) -> Self {
        Self(value)
    }
}

/// A half-open byte range in the exact Markdown source bound to a resolution.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct MarkdownSourceRange {
    pub start: usize,
    pub end: usize,
}

impl MarkdownSourceRange {
    pub fn as_range(self) -> Range<usize> {
        self.start..self.end
    }

    pub(super) const fn new(range: Range<usize>) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkdownDestinationKind {
    Image,
    Download,
}

/// The destination value produced by the CommonMark parser for one occurrence.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AuthoredMarkdownDestination(Box<str>);

impl AuthoredMarkdownDestination {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn new(value: &str) -> Self {
        Self(value.into())
    }
}

/// One resolver-approved Markdown asset destination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedMarkdownDestination {
    pub ordinal: MarkdownDestinationOrdinal,
    pub source_range: MarkdownSourceRange,
    pub kind: MarkdownDestinationKind,
    pub authored: AuthoredMarkdownDestination,
    pub target: AssetRevisionReference,
}

impl ResolvedMarkdownDestination {
    pub(super) fn new(
        ordinal: MarkdownDestinationOrdinal,
        source_range: MarkdownSourceRange,
        kind: MarkdownDestinationKind,
        authored: AuthoredMarkdownDestination,
        target: AssetRevisionReference,
    ) -> Self {
        Self {
            ordinal,
            source_range,
            kind,
            authored,
            target,
        }
    }
}

/// Resolver-owned, complete asset inputs for one post revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPostAssets {
    pub(super) source_binding: PostAssetSourceBinding,
    pub(super) policy_binding: AssetResolutionPolicyBinding,
    pub image: Option<AssetRevisionReference>,
    pub references: Vec<AssetRevisionReference>,
    pub markdown_destinations: Vec<ResolvedMarkdownDestination>,
}

impl ResolvedPostAssets {
    pub fn new(
        document: &PostDocument,
        image: Option<AssetRevisionReference>,
        references: Vec<AssetRevisionReference>,
    ) -> Self {
        Self::from_resolution(document, &[], image, references, Vec::new())
    }

    pub(super) fn from_resolution(
        document: &PostDocument,
        allowed_origins: &[ExternalAssetOrigin],
        image: Option<AssetRevisionReference>,
        references: Vec<AssetRevisionReference>,
        markdown_destinations: Vec<ResolvedMarkdownDestination>,
    ) -> Self {
        Self {
            source_binding: bind_post_asset_source(document),
            policy_binding: bind_asset_resolution_policy(allowed_origins),
            image,
            references,
            markdown_destinations,
        }
    }

    pub fn shares_policy_with(&self, site_assets: &ResolvedSiteAssets) -> bool {
        self.policy_binding == site_assets.policy_binding
    }
}

/// Resolver-owned, complete asset inputs for one site snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSiteAssets {
    pub(super) source_binding: PublicationAssetSourceBinding,
    pub(super) policy_binding: AssetResolutionPolicyBinding,
    pub favicon: Option<AssetRevisionReference>,
    pub allowed_origins: Vec<ExternalAssetOrigin>,
    pub references: Vec<AssetRevisionReference>,
}

impl ResolvedSiteAssets {
    pub fn new(
        publication: &PublicationSettings,
        favicon: Option<AssetRevisionReference>,
        allowed_origins: Vec<ExternalAssetOrigin>,
        references: Vec<AssetRevisionReference>,
    ) -> Self {
        Self {
            source_binding: bind_publication_asset_source(publication),
            policy_binding: bind_asset_resolution_policy(&allowed_origins),
            favicon,
            allowed_origins,
            references,
        }
    }
}
