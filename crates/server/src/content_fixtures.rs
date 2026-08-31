use std::sync::Arc;

use markdown_compiler::{
    DiscoveredAsset, DiscoveredContentTree, DiscoveredPost, DiscoveredPublication,
    LogicalAssetPath, LogicalContentPath, PostCollection, PostDocument, PublicationSettings,
    ValidatedContent,
};

pub(crate) fn publication(path: impl Into<String>, source: String) -> DiscoveredPublication {
    DiscoveredPublication {
        path: LogicalContentPath::new(path),
        source: source.into_boxed_str(),
    }
}

pub(crate) fn post(
    path: impl Into<String>,
    collection: PostCollection,
    source: String,
) -> DiscoveredPost {
    DiscoveredPost {
        path: LogicalContentPath::new(path),
        collection,
        source: source.into_boxed_str(),
    }
}

pub(crate) fn asset(path: LogicalAssetPath, bytes: Vec<u8>) -> DiscoveredAsset {
    DiscoveredAsset {
        path,
        bytes: Arc::from(bytes),
    }
}

pub(crate) fn content_tree(
    publication: DiscoveredPublication,
    posts: Vec<DiscoveredPost>,
    assets: Vec<DiscoveredAsset>,
    total_bytes: u64,
) -> DiscoveredContentTree {
    DiscoveredContentTree {
        publication,
        posts,
        assets,
        total_bytes,
    }
}

pub(crate) fn validated_content(
    publication: PublicationSettings,
    posts: Vec<PostDocument>,
) -> ValidatedContent {
    ValidatedContent { publication, posts }
}
