use std::{fmt, str::FromStr};

use blake3::Hasher;
use thiserror::Error;

use super::{DiscoveredAsset, DiscoveredContentTree, DiscoveredPost, PostCollection};

const CONTENT_TREE_CONTEXT: &str = "maincopy content tree digest v1";
const CONTENT_TREE_KIND: &[u8] = b"maincopy-content-tree";
const CONTENT_TREE_VERSION: u16 = 1;
const CONTENT_TREE_PREFIX: &str = "content-b3-v1-";

const PUBLICATION_SECTION: u8 = 0;
const POSTS_SECTION: u8 = 1;
const ASSETS_SECTION: u8 = 2;
const POSTS_COLLECTION: u8 = 0;
const DRAFTS_COLLECTION: u8 = 1;

/// Versioned identity of the exact managed inputs in one discovered content tree.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContentTreeDigest([u8; 32]);

impl ContentTreeDigest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn parse(value: &str) -> Result<Self, ContentTreeDigestParseError> {
        let encoded = value
            .strip_prefix(CONTENT_TREE_PREFIX)
            .ok_or(ContentTreeDigestParseError::InvalidPrefix)?;
        if encoded.len() != 64 {
            return Err(ContentTreeDigestParseError::InvalidLength);
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
            let high =
                decode_nibble(pair[0]).ok_or(ContentTreeDigestParseError::InvalidEncoding)?;
            let low = decode_nibble(pair[1]).ok_or(ContentTreeDigestParseError::InvalidEncoding)?;
            bytes[index] = high << 4 | low;
        }
        Ok(Self(bytes))
    }
}

impl FromStr for ContentTreeDigest {
    type Err = ContentTreeDigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ContentTreeDigestParseError {
    #[error("content digest must start with content-b3-v1-")]
    InvalidPrefix,
    #[error("content digest must contain exactly 32 encoded bytes")]
    InvalidLength,
    #[error("content digest must use lowercase hexadecimal")]
    InvalidEncoding,
}

impl fmt::Display for ContentTreeDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{CONTENT_TREE_PREFIX}{}",
            blake3::Hash::from_bytes(self.0).to_hex()
        )
    }
}

const fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl DiscoveredContentTree {
    /// Computes a deterministic token for the exact managed tree already in memory.
    pub fn digest(&self) -> ContentTreeDigest {
        let mut transcript = ContentTreeTranscript::new();

        transcript.tag(PUBLICATION_SECTION);
        transcript.string(self.publication.path.as_str());
        transcript.bytes(self.publication.source.as_bytes());

        transcript.tag(POSTS_SECTION);
        let mut posts: Vec<_> = self.posts.iter().collect();
        posts.sort_unstable_by(compare_posts);
        transcript.sequence_len(posts.len());
        for post in posts {
            transcript.string(post.path.as_str());
            transcript.tag(collection_tag(post.collection));
            transcript.bytes(post.source.as_bytes());
        }

        transcript.tag(ASSETS_SECTION);
        let mut assets: Vec<_> = self.assets.iter().collect();
        assets.sort_unstable_by(compare_assets);
        transcript.sequence_len(assets.len());
        for asset in assets {
            transcript.string(asset.path.as_str());
            transcript.bytes(&asset.bytes);
        }

        ContentTreeDigest(*transcript.finish().as_bytes())
    }
}

fn compare_posts(left: &&DiscoveredPost, right: &&DiscoveredPost) -> std::cmp::Ordering {
    left.path
        .cmp(&right.path)
        .then_with(|| collection_tag(left.collection).cmp(&collection_tag(right.collection)))
        .then_with(|| left.source.cmp(&right.source))
}

fn compare_assets(left: &&DiscoveredAsset, right: &&DiscoveredAsset) -> std::cmp::Ordering {
    left.path
        .cmp(&right.path)
        .then_with(|| left.bytes.as_ref().cmp(right.bytes.as_ref()))
}

const fn collection_tag(collection: PostCollection) -> u8 {
    match collection {
        PostCollection::Posts => POSTS_COLLECTION,
        PostCollection::Drafts => DRAFTS_COLLECTION,
    }
}

struct ContentTreeTranscript(Hasher);

impl ContentTreeTranscript {
    fn new() -> Self {
        let mut transcript = Self(Hasher::new_derive_key(CONTENT_TREE_CONTEXT));
        transcript.bytes(CONTENT_TREE_KIND);
        transcript.0.update(&CONTENT_TREE_VERSION.to_be_bytes());
        transcript
    }

    fn finish(self) -> blake3::Hash {
        self.0.finalize()
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.0.update(&(bytes.len() as u64).to_be_bytes());
        self.0.update(bytes);
    }

    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn sequence_len(&mut self, length: usize) {
        self.0.update(&(length as u64).to_be_bytes());
    }

    fn tag(&mut self, tag: u8) {
        self.0.update(&[tag]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{asset, post, publication};
    use crate::{LogicalAssetPath, LogicalContentPath};

    fn fixture() -> DiscoveredContentTree {
        let posts = vec![
            post(
                "posts/first.md",
                PostCollection::Posts,
                "first post".to_owned(),
            ),
            post(
                "drafts/second.md",
                PostCollection::Drafts,
                "second post".to_owned(),
            ),
        ];
        let assets = vec![
            asset(
                LogicalAssetPath::parse("assets/first.bin").unwrap(),
                b"first asset".to_vec(),
            ),
            asset(
                LogicalAssetPath::parse("assets/second.bin").unwrap(),
                b"second asset".to_vec(),
            ),
        ];
        DiscoveredContentTree::new(
            publication("publication.toml", "publication settings".to_owned()),
            posts,
            assets,
            64,
        )
    }

    #[test]
    fn identical_logical_trees_have_one_order_independent_digest() {
        let first = fixture();
        let mut reordered = first.clone();
        reordered.posts.reverse();
        reordered.assets.reverse();
        reordered.total_bytes = u64::MAX;

        assert_eq!(first.digest(), first.clone().digest());
        assert_eq!(first.digest(), reordered.digest());
    }

    #[test]
    fn digest_binds_publication_path_and_exact_bytes() {
        let original = fixture().digest();
        let mut changed_path = fixture();
        changed_path.publication.path = LogicalContentPath::new("settings/publication.toml");
        let mut changed_bytes = fixture();
        changed_bytes.publication.source = "publication settings\n".into();

        assert_ne!(original, changed_path.digest());
        assert_ne!(original, changed_bytes.digest());
    }

    #[test]
    fn digest_binds_post_path_collection_exact_bytes_and_cardinality() {
        let original = fixture().digest();
        let mut changed_path = fixture();
        changed_path.posts[0].path = LogicalContentPath::new("posts/renamed.md");
        let mut changed_collection = fixture();
        changed_collection.posts[0].collection = PostCollection::Drafts;
        let mut changed_bytes = fixture();
        changed_bytes.posts[0].source = "first post\n".into();
        let mut removed = fixture();
        removed.posts.pop();

        for changed in [changed_path, changed_collection, changed_bytes, removed] {
            assert_ne!(original, changed.digest());
        }
    }

    #[test]
    fn digest_binds_asset_path_exact_bytes_and_cardinality() {
        let original = fixture().digest();
        let mut changed_path = fixture();
        changed_path.assets[0].path = LogicalAssetPath::parse("assets/renamed.bin").unwrap();
        let mut changed_bytes = fixture();
        changed_bytes.assets[0].bytes = std::sync::Arc::from(b"first asset\n".as_slice());
        let mut removed = fixture();
        removed.assets.pop();

        for changed in [changed_path, changed_bytes, removed] {
            assert_ne!(original, changed.digest());
        }
    }

    #[test]
    fn length_framing_prevents_adjacent_field_ambiguity() {
        let first = DiscoveredContentTree::new(
            publication("ab", "c".to_owned()),
            Vec::new(),
            Vec::new(),
            3,
        );
        let second = DiscoveredContentTree::new(
            publication("a", "bc".to_owned()),
            Vec::new(),
            Vec::new(),
            3,
        );

        assert_ne!(first.digest(), second.digest());
    }

    #[test]
    fn display_uses_the_versioned_content_digest_format() {
        let digest = fixture().digest();
        let encoded = digest.to_string();
        let hex = encoded.strip_prefix(CONTENT_TREE_PREFIX).unwrap();

        assert_eq!(digest.as_bytes().len(), 32);
        assert_eq!(hex.len(), 64);
        assert!(
            hex.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert_eq!(ContentTreeDigest::parse(&encoded).unwrap(), digest);
        assert_eq!(encoded.parse::<ContentTreeDigest>().unwrap(), digest);
        assert_eq!(
            ContentTreeDigest::parse(&encoded.to_uppercase()),
            Err(ContentTreeDigestParseError::InvalidPrefix)
        );
        assert_eq!(
            ContentTreeDigest::parse(&encoded[..encoded.len() - 1]),
            Err(ContentTreeDigestParseError::InvalidLength)
        );
        assert_eq!(
            ContentTreeDigest::parse(&format!("{CONTENT_TREE_PREFIX}{}", "gg".repeat(32))),
            Err(ContentTreeDigestParseError::InvalidEncoding)
        );
    }
}
