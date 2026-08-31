use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use thiserror::Error;
use uuid::Uuid;

use super::{
    ContentTreeDigest, ContentTreeLimits, DiscoveredAsset, DiscoveredContentTree, DiscoveredPost,
    DiscoveredPublication, LogicalAssetPath, LogicalContentPath, PostCollection,
    tree::PortableLogicalPath,
};

const STORE_DIRECTORY: &str = "content-candidates";
const CANDIDATE_SUFFIX: &str = ".candidate";
const STAGING_PREFIX: &str = ".candidate-stage-";
const STAGING_SUFFIX: &str = ".tmp";
const DIGEST_PREFIX: &str = "content-b3-v1-";

const ARCHIVE_MAGIC: &[u8; 17] = b"MAINCOPYCANDIDATE";
const ARCHIVE_VERSION: u16 = 1;
const ARCHIVE_HEADER_BYTES: u64 = ARCHIVE_MAGIC.len() as u64 + 2 + 32 + 8;
const PUBLICATION_RECORD_OVERHEAD: u64 = 4 + 8;
const POST_RECORD_OVERHEAD: u64 = 1 + 4 + 8;
const ASSET_RECORD_OVERHEAD: u64 = 4 + 8;
const SEQUENCE_LENGTH_BYTES: u64 = 4;

const POSTS_COLLECTION: u8 = 0;
const DRAFTS_COLLECTION: u8 = 1;

fn prepare_private_directory(path: &Path) -> io::Result<()> {
    reject_symlink_components(path)?;
    create_private_directory(path)?;
    validate_private_directory(path)
}

fn reject_symlink_components(path: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "private candidate-store paths cannot contain symbolic links",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).create(path)
}

#[cfg(unix)]
fn validate_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private candidate-store directory ownership or permissions are unsafe",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private candidate-store path is not a directory",
        ))
    }
}

#[derive(Clone, Debug)]
pub struct ContentCandidateStore {
    root: PathBuf,
    limits: ContentTreeLimits,
}

#[derive(Debug, Eq, PartialEq)]
pub struct RetainedContentCandidate {
    pub digest: ContentTreeDigest,
    pub tree: DiscoveredContentTree,
}

impl ContentCandidateStore {
    pub fn open(
        state_root: &Path,
        limits: ContentTreeLimits,
    ) -> Result<Self, ContentCandidateStoreError> {
        let root = state_root.join(STORE_DIRECTORY);
        prepare_private_directory(&root).map_err(ContentCandidateStoreError::Directory)?;
        sync_directory(state_root).map_err(ContentCandidateStoreError::Directory)?;
        Ok(Self { root, limits })
    }

    pub fn retain(
        &self,
        tree: &DiscoveredContentTree,
    ) -> Result<ContentTreeDigest, ContentCandidateStoreError> {
        validate_tree(tree, self.limits)?;
        let digest = tree.digest();
        let staging_path = self.staging_path();
        let staging = StagingPath::new(staging_path.clone());
        let mut file = create_staging_file(&staging_path)?;
        {
            let mut writer = BufWriter::new(&mut file);
            encode_candidate(&mut writer, &digest, tree)?;
            writer.flush().map_err(ContentCandidateStoreError::Io)?;
        }
        file.sync_all().map_err(ContentCandidateStoreError::Io)?;
        drop(file);

        let candidate_path = self.candidate_path(&digest);
        match publish_no_replace(&staging_path, &candidate_path)? {
            PublishOutcome::Published => {
                sync_directory(&self.root).map_err(ContentCandidateStoreError::Io)?;
            }
            PublishOutcome::AlreadyExists => {
                self.load(&digest)?;
                if !files_equal(&staging_path, &candidate_path)? {
                    return Err(ContentCandidateStoreError::Collision);
                }
            }
        }
        staging.cleanup()?;
        Ok(digest)
    }

    pub fn load(
        &self,
        digest: &ContentTreeDigest,
    ) -> Result<DiscoveredContentTree, ContentCandidateStoreError> {
        let path = self.candidate_path(digest);
        let file = open_candidate_file(&path)?;
        let metadata = file.metadata().map_err(ContentCandidateStoreError::Io)?;
        if u128::from(metadata.len()) > maximum_archive_bytes(self.limits) {
            return Err(ContentCandidateStoreError::LimitExceeded(
                "candidate archive exceeds its configured maximum size",
            ));
        }
        decode_candidate(BufReader::new(file), metadata.len(), digest, self.limits)
    }

    pub fn load_all(&self) -> Result<Vec<RetainedContentCandidate>, ContentCandidateStoreError> {
        let mut candidates = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(ContentCandidateStoreError::Io)? {
            let entry = entry.map_err(ContentCandidateStoreError::Io)?;
            let file_type = entry.file_type().map_err(ContentCandidateStoreError::Io)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ContentCandidateStoreError::UnsafeEntry)?;

            if is_staging_name(&name) {
                if !file_type.is_file() || file_type.is_symlink() {
                    return Err(ContentCandidateStoreError::UnsafeEntry);
                }
                drop(open_candidate_file(&entry.path())?);
                continue;
            }
            let digest =
                parse_candidate_name(&name).ok_or(ContentCandidateStoreError::UnexpectedEntry)?;
            if !file_type.is_file() || file_type.is_symlink() {
                return Err(ContentCandidateStoreError::UnsafeEntry);
            }
            candidates.push((name, digest));
        }
        candidates.sort_unstable_by(|left, right| left.0.cmp(&right.0));

        candidates
            .into_iter()
            .map(|(_, digest)| {
                let tree = self.load(&digest)?;
                Ok(RetainedContentCandidate { digest, tree })
            })
            .collect()
    }

    fn candidate_path(&self, digest: &ContentTreeDigest) -> PathBuf {
        self.root.join(format!("{digest}{CANDIDATE_SUFFIX}"))
    }

    fn staging_path(&self) -> PathBuf {
        self.root.join(format!(
            "{STAGING_PREFIX}{}{STAGING_SUFFIX}",
            Uuid::new_v4().hyphenated()
        ))
    }
}

#[derive(Debug, Error)]
pub enum ContentCandidateStoreError {
    #[error("the content candidate directory is unavailable")]
    Directory(#[source] io::Error),
    #[error("content candidate storage failed")]
    Io(#[source] io::Error),
    #[error("the content candidate store contains an unsafe entry")]
    UnsafeEntry,
    #[error("the content candidate store contains an unexpected entry")]
    UnexpectedEntry,
    #[error("the content candidate archive is invalid: {0}")]
    InvalidArchive(&'static str),
    #[error("the content candidate exceeds configured limits: {0}")]
    LimitExceeded(&'static str),
    #[error("the content candidate digest does not match its archive or content")]
    DigestMismatch,
    #[error("the content candidate key is already occupied by different bytes")]
    Collision,
}

fn encode_candidate(
    writer: &mut impl Write,
    digest: &ContentTreeDigest,
    tree: &DiscoveredContentTree,
) -> Result<(), ContentCandidateStoreError> {
    write_all(writer, ARCHIVE_MAGIC)?;
    write_all(writer, &ARCHIVE_VERSION.to_be_bytes())?;
    write_all(writer, digest.as_bytes())?;
    write_all(writer, &tree.total_bytes.to_be_bytes())?;

    write_string(writer, tree.publication.path.as_str())?;
    write_bytes(writer, tree.publication.source.as_bytes())?;

    let mut posts: Vec<_> = tree.posts.iter().collect();
    posts.sort_unstable_by(compare_posts);
    write_u32(writer, posts.len())?;
    for post in posts {
        write_all(writer, &[collection_tag(post.collection)])?;
        write_string(writer, post.path.as_str())?;
        write_bytes(writer, post.source.as_bytes())?;
    }

    let mut assets: Vec<_> = tree.assets.iter().collect();
    assets.sort_unstable_by(compare_assets);
    write_u32(writer, assets.len())?;
    for asset in assets {
        write_string(writer, asset.path.as_str())?;
        write_bytes(writer, &asset.bytes)?;
    }
    Ok(())
}

fn decode_candidate(
    reader: BufReader<File>,
    archive_bytes: u64,
    expected_digest: &ContentTreeDigest,
    limits: ContentTreeLimits,
) -> Result<DiscoveredContentTree, ContentCandidateStoreError> {
    let mut decoder = Decoder {
        reader,
        remaining: archive_bytes,
    };
    if decoder.fixed::<17>()? != *ARCHIVE_MAGIC {
        return Err(ContentCandidateStoreError::InvalidArchive(
            "unknown archive magic",
        ));
    }
    if u16::from_be_bytes(decoder.fixed()?) != ARCHIVE_VERSION {
        return Err(ContentCandidateStoreError::InvalidArchive(
            "unsupported archive version",
        ));
    }
    let stored_digest = ContentTreeDigest::from_bytes(decoder.fixed()?);
    if &stored_digest != expected_digest {
        return Err(ContentCandidateStoreError::DigestMismatch);
    }
    let stored_total = u64::from_be_bytes(decoder.fixed()?);

    let publication_path = decoder.string(limits.path_bytes.get())?;
    validate_publication_path(&publication_path, limits)?;
    let publication_source = decoder.utf8_bytes(limits.publication_file_bytes.get())?;
    let mut total_bytes = publication_source.len() as u64;
    ensure_total_within_limit(total_bytes, limits)?;
    let publication = DiscoveredPublication {
        path: LogicalContentPath::new(publication_path),
        source: publication_source.into_boxed_str(),
    };

    let post_count = decoder.count(limits.entries.get().saturating_sub(1))?;
    let mut posts = Vec::new();
    let mut previous_post_path: Option<String> = None;
    let mut paths = PathRegistry::default();
    paths.register(publication.path.as_str())?;
    for _ in 0..post_count {
        let collection = parse_collection(decoder.byte()?)?;
        let path = decoder.string(limits.path_bytes.get())?;
        validate_post_path(&path, collection, limits)?;
        ensure_canonical_path_order(previous_post_path.as_deref(), &path)?;
        paths.register(&path)?;
        previous_post_path = Some(path.clone());
        let source = decoder.utf8_bytes(limits.post_file_bytes.get())?;
        total_bytes = add_total(total_bytes, source.len(), limits)?;
        posts.push(DiscoveredPost {
            path: LogicalContentPath::new(path),
            collection,
            source: source.into_boxed_str(),
        });
    }

    let remaining_entries = limits
        .entries
        .get()
        .saturating_sub(1)
        .saturating_sub(post_count);
    let asset_count = decoder.count(remaining_entries)?;
    let mut assets = Vec::new();
    let mut previous_asset_path: Option<String> = None;
    for _ in 0..asset_count {
        let path = decoder.string(limits.path_bytes.get())?;
        validate_asset_path(&path, limits)?;
        ensure_canonical_path_order(previous_asset_path.as_deref(), &path)?;
        paths.register(&path)?;
        previous_asset_path = Some(path.clone());
        let bytes = decoder.bytes(limits.asset_file_bytes.get())?;
        total_bytes = add_total(total_bytes, bytes.len(), limits)?;
        assets.push(DiscoveredAsset {
            path: LogicalAssetPath::parse(&path).map_err(|_| {
                ContentCandidateStoreError::InvalidArchive("invalid logical asset path")
            })?,
            bytes: Arc::from(bytes),
        });
    }
    decoder.end()?;

    if total_bytes != stored_total {
        return Err(ContentCandidateStoreError::InvalidArchive(
            "stored tree byte count is inconsistent",
        ));
    }
    let tree = DiscoveredContentTree::new(publication, posts, assets, total_bytes);
    if tree.digest() != stored_digest {
        return Err(ContentCandidateStoreError::DigestMismatch);
    }
    Ok(tree)
}

fn validate_tree(
    tree: &DiscoveredContentTree,
    limits: ContentTreeLimits,
) -> Result<(), ContentCandidateStoreError> {
    let entry_count = 1usize
        .checked_add(tree.posts.len())
        .and_then(|count| count.checked_add(tree.assets.len()))
        .ok_or(ContentCandidateStoreError::LimitExceeded(
            "candidate entry count overflows",
        ))?;
    if entry_count > limits.entries.get()
        || tree.posts.len() > u32::MAX as usize
        || tree.assets.len() > u32::MAX as usize
    {
        return Err(ContentCandidateStoreError::LimitExceeded(
            "candidate has too many entries",
        ));
    }

    validate_publication_path(tree.publication.path.as_str(), limits)?;
    ensure_file_size(
        tree.publication.source.len(),
        limits.publication_file_bytes.get(),
    )?;
    let mut total = tree.publication.source.len() as u64;
    ensure_total_within_limit(total, limits)?;
    let mut paths = PathRegistry::default();
    paths.register(tree.publication.path.as_str())?;

    for post in &tree.posts {
        validate_post_path(post.path.as_str(), post.collection, limits)?;
        ensure_file_size(post.source.len(), limits.post_file_bytes.get())?;
        paths.register(post.path.as_str())?;
        total = add_total(total, post.source.len(), limits)?;
    }
    for asset in &tree.assets {
        validate_asset_path(asset.path.as_str(), limits)?;
        ensure_file_size(asset.bytes.len(), limits.asset_file_bytes.get())?;
        paths.register(asset.path.as_str())?;
        total = add_total(total, asset.bytes.len(), limits)?;
    }
    if total != tree.total_bytes {
        return Err(ContentCandidateStoreError::InvalidArchive(
            "tree byte count is inconsistent",
        ));
    }
    Ok(())
}

fn validate_publication_path(
    path: &str,
    limits: ContentTreeLimits,
) -> Result<(), ContentCandidateStoreError> {
    validate_portable_path(path, limits)?;
    if path != "publication.toml" {
        return Err(ContentCandidateStoreError::InvalidArchive(
            "publication must use the root publication.toml path",
        ));
    }
    Ok(())
}

fn validate_post_path(
    path: &str,
    collection: PostCollection,
    limits: ContentTreeLimits,
) -> Result<(), ContentCandidateStoreError> {
    validate_portable_path(path, limits)?;
    if !collection.contains_path(path) || !path.ends_with(".md") {
        return Err(ContentCandidateStoreError::InvalidArchive(
            "post path does not match its collection",
        ));
    }
    Ok(())
}

fn validate_asset_path(
    path: &str,
    limits: ContentTreeLimits,
) -> Result<(), ContentCandidateStoreError> {
    let portable = validate_portable_path(path, limits)?;
    LogicalAssetPath::parse(portable.as_str()).map_err(|_| {
        ContentCandidateStoreError::InvalidArchive("asset path is outside the asset namespace")
    })?;
    Ok(())
}

fn validate_portable_path(
    path: &str,
    limits: ContentTreeLimits,
) -> Result<PortableLogicalPath, ContentCandidateStoreError> {
    let portable =
        PortableLogicalPath::parse(path, limits.path_bytes.get()).map_err(|error| match error {
            super::LogicalTreePathError::TooLong => ContentCandidateStoreError::LimitExceeded(
                "candidate logical path exceeds its configured limit",
            ),
            _ => ContentCandidateStoreError::InvalidArchive("invalid portable logical path"),
        })?;
    if path.split('/').count() > limits.depth.get() {
        return Err(ContentCandidateStoreError::LimitExceeded(
            "candidate logical path exceeds its configured depth",
        ));
    }
    Ok(portable)
}

#[derive(Default)]
struct PathRegistry {
    exact: BTreeSet<String>,
    folded_prefixes: BTreeMap<String, String>,
}

impl PathRegistry {
    fn register(&mut self, path: &str) -> Result<(), ContentCandidateStoreError> {
        if !self.exact.insert(path.to_owned()) {
            return Err(ContentCandidateStoreError::InvalidArchive(
                "candidate contains duplicate or case-colliding paths",
            ));
        }
        for end in path
            .match_indices('/')
            .map(|(index, _)| index)
            .chain(std::iter::once(path.len()))
        {
            let prefix = &path[..end];
            let folded = prefix.to_ascii_lowercase();
            if self
                .folded_prefixes
                .get(&folded)
                .is_some_and(|existing| existing != prefix)
            {
                return Err(ContentCandidateStoreError::InvalidArchive(
                    "candidate contains duplicate or case-colliding paths",
                ));
            }
            self.folded_prefixes
                .entry(folded)
                .or_insert_with(|| prefix.to_owned());
        }
        Ok(())
    }
}

fn ensure_canonical_path_order(
    previous: Option<&str>,
    current: &str,
) -> Result<(), ContentCandidateStoreError> {
    if previous.is_some_and(|previous| previous >= current) {
        return Err(ContentCandidateStoreError::InvalidArchive(
            "candidate records are not in canonical order",
        ));
    }
    Ok(())
}

fn ensure_file_size(length: usize, limit: u64) -> Result<(), ContentCandidateStoreError> {
    let length = u64::try_from(length).map_err(|_| {
        ContentCandidateStoreError::LimitExceeded("candidate file length cannot be represented")
    })?;
    if length > limit {
        return Err(ContentCandidateStoreError::LimitExceeded(
            "candidate file exceeds its configured limit",
        ));
    }
    Ok(())
}

fn ensure_total_within_limit(
    total: u64,
    limits: ContentTreeLimits,
) -> Result<(), ContentCandidateStoreError> {
    if total > limits.total_tree_bytes.get() {
        return Err(ContentCandidateStoreError::LimitExceeded(
            "candidate tree exceeds its configured byte limit",
        ));
    }
    Ok(())
}

fn add_total(
    total: u64,
    length: usize,
    limits: ContentTreeLimits,
) -> Result<u64, ContentCandidateStoreError> {
    let length = u64::try_from(length).map_err(|_| {
        ContentCandidateStoreError::LimitExceeded("candidate tree byte count overflows")
    })?;
    let total = total
        .checked_add(length)
        .ok_or(ContentCandidateStoreError::LimitExceeded(
            "candidate tree byte count overflows",
        ))?;
    ensure_total_within_limit(total, limits)?;
    Ok(total)
}

fn maximum_archive_bytes(limits: ContentTreeLimits) -> u128 {
    let entries = limits.entries.get() as u128;
    let path_bytes = limits.path_bytes.get() as u128;
    u128::from(ARCHIVE_HEADER_BYTES)
        + u128::from(SEQUENCE_LENGTH_BYTES * 2)
        + u128::from(PUBLICATION_RECORD_OVERHEAD)
        + entries * (path_bytes + u128::from(POST_RECORD_OVERHEAD.max(ASSET_RECORD_OVERHEAD)))
        + u128::from(limits.total_tree_bytes.get())
}

fn compare_posts(left: &&DiscoveredPost, right: &&DiscoveredPost) -> Ordering {
    left.path
        .cmp(&right.path)
        .then_with(|| collection_tag(left.collection).cmp(&collection_tag(right.collection)))
        .then_with(|| left.source.cmp(&right.source))
}

fn compare_assets(left: &&DiscoveredAsset, right: &&DiscoveredAsset) -> Ordering {
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

fn parse_collection(tag: u8) -> Result<PostCollection, ContentCandidateStoreError> {
    match tag {
        POSTS_COLLECTION => Ok(PostCollection::Posts),
        DRAFTS_COLLECTION => Ok(PostCollection::Drafts),
        _ => Err(ContentCandidateStoreError::InvalidArchive(
            "unknown post collection tag",
        )),
    }
}

fn write_all(writer: &mut impl Write, bytes: &[u8]) -> Result<(), ContentCandidateStoreError> {
    writer
        .write_all(bytes)
        .map_err(ContentCandidateStoreError::Io)
}

fn write_u32(writer: &mut impl Write, value: usize) -> Result<(), ContentCandidateStoreError> {
    let value = u32::try_from(value).map_err(|_| {
        ContentCandidateStoreError::LimitExceeded("candidate sequence is too large")
    })?;
    write_all(writer, &value.to_be_bytes())
}

fn write_string(writer: &mut impl Write, value: &str) -> Result<(), ContentCandidateStoreError> {
    let length = u32::try_from(value.len()).map_err(|_| {
        ContentCandidateStoreError::LimitExceeded("candidate logical path is too large")
    })?;
    write_all(writer, &length.to_be_bytes())?;
    write_all(writer, value.as_bytes())
}

fn write_bytes(writer: &mut impl Write, bytes: &[u8]) -> Result<(), ContentCandidateStoreError> {
    let length = u64::try_from(bytes.len()).map_err(|_| {
        ContentCandidateStoreError::LimitExceeded("candidate file length cannot be represented")
    })?;
    write_all(writer, &length.to_be_bytes())?;
    write_all(writer, bytes)
}

struct Decoder {
    reader: BufReader<File>,
    remaining: u64,
}

impl Decoder {
    fn fixed<const LENGTH: usize>(&mut self) -> Result<[u8; LENGTH], ContentCandidateStoreError> {
        let mut bytes = [0; LENGTH];
        self.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, ContentCandidateStoreError> {
        Ok(self.fixed::<1>()?[0])
    }

    fn count(&mut self, limit: usize) -> Result<usize, ContentCandidateStoreError> {
        let count = u32::from_be_bytes(self.fixed()?) as usize;
        if count > limit {
            return Err(ContentCandidateStoreError::LimitExceeded(
                "candidate has too many entries",
            ));
        }
        Ok(count)
    }

    fn string(&mut self, limit: usize) -> Result<String, ContentCandidateStoreError> {
        let length = u32::from_be_bytes(self.fixed()?) as usize;
        if length > limit {
            return Err(ContentCandidateStoreError::LimitExceeded(
                "candidate logical path exceeds its configured limit",
            ));
        }
        String::from_utf8(self.read_vec(length)?).map_err(|_| {
            ContentCandidateStoreError::InvalidArchive("logical path is not valid UTF-8")
        })
    }

    fn utf8_bytes(&mut self, limit: u64) -> Result<String, ContentCandidateStoreError> {
        String::from_utf8(self.bytes(limit)?).map_err(|_| {
            ContentCandidateStoreError::InvalidArchive("authored source is not valid UTF-8")
        })
    }

    fn bytes(&mut self, limit: u64) -> Result<Vec<u8>, ContentCandidateStoreError> {
        let length = u64::from_be_bytes(self.fixed()?);
        if length > limit {
            return Err(ContentCandidateStoreError::LimitExceeded(
                "candidate file exceeds its configured limit",
            ));
        }
        let length = usize::try_from(length).map_err(|_| {
            ContentCandidateStoreError::LimitExceeded("candidate file cannot fit in memory")
        })?;
        self.read_vec(length)
    }

    fn read_vec(&mut self, length: usize) -> Result<Vec<u8>, ContentCandidateStoreError> {
        self.ensure_available(length)?;
        let mut bytes = vec![0; length];
        self.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    fn read_exact(&mut self, bytes: &mut [u8]) -> Result<(), ContentCandidateStoreError> {
        self.ensure_available(bytes.len())?;
        self.reader.read_exact(bytes).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                ContentCandidateStoreError::InvalidArchive("candidate archive is truncated")
            } else {
                ContentCandidateStoreError::Io(error)
            }
        })?;
        self.remaining -= bytes.len() as u64;
        Ok(())
    }

    fn ensure_available(&self, length: usize) -> Result<(), ContentCandidateStoreError> {
        if u64::try_from(length).map_or(true, |length| length > self.remaining) {
            return Err(ContentCandidateStoreError::InvalidArchive(
                "candidate archive is truncated",
            ));
        }
        Ok(())
    }

    fn end(mut self) -> Result<(), ContentCandidateStoreError> {
        if self.remaining != 0 {
            return Err(ContentCandidateStoreError::InvalidArchive(
                "candidate archive has trailing bytes",
            ));
        }
        let mut byte = [0];
        match self.reader.read(&mut byte) {
            Ok(0) => Ok(()),
            Ok(_) => Err(ContentCandidateStoreError::InvalidArchive(
                "candidate archive has trailing bytes",
            )),
            Err(error) => Err(ContentCandidateStoreError::Io(error)),
        }
    }
}

fn create_staging_file(path: &Path) -> Result<File, ContentCandidateStoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(ContentCandidateStoreError::Io)?;
    validate_private_file_metadata(&file.metadata().map_err(ContentCandidateStoreError::Io)?)?;
    Ok(file)
}

fn open_candidate_file(path: &Path) -> Result<File, ContentCandidateStoreError> {
    let before = fs::symlink_metadata(path).map_err(ContentCandidateStoreError::Io)?;
    validate_private_file_metadata(&before)?;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let file: File = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| ContentCandidateStoreError::Io(error.into()))?
    .into();

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let file = File::open(path).map_err(ContentCandidateStoreError::Io)?;

    let after = file.metadata().map_err(ContentCandidateStoreError::Io)?;
    validate_private_file_metadata(&after)?;
    validate_same_file(&before, &after)?;
    Ok(file)
}

#[cfg(unix)]
fn validate_private_file_metadata(
    metadata: &fs::Metadata,
) -> Result<(), ContentCandidateStoreError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(ContentCandidateStoreError::UnsafeEntry);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_file_metadata(
    metadata: &fs::Metadata,
) -> Result<(), ContentCandidateStoreError> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(ContentCandidateStoreError::UnsafeEntry);
    }
    Ok(())
}

#[cfg(unix)]
fn validate_same_file(
    before: &fs::Metadata,
    after: &fs::Metadata,
) -> Result<(), ContentCandidateStoreError> {
    use std::os::unix::fs::MetadataExt as _;

    if before.dev() != after.dev() || before.ino() != after.ino() {
        return Err(ContentCandidateStoreError::UnsafeEntry);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_same_file(
    _before: &fs::Metadata,
    _after: &fs::Metadata,
) -> Result<(), ContentCandidateStoreError> {
    Ok(())
}

enum PublishOutcome {
    Published,
    AlreadyExists,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish_no_replace(
    from: &Path,
    to: &Path,
) -> Result<PublishOutcome, ContentCandidateStoreError> {
    match rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    ) {
        Ok(()) => Ok(PublishOutcome::Published),
        Err(rustix::io::Errno::EXIST) => Ok(PublishOutcome::AlreadyExists),
        Err(error) => Err(ContentCandidateStoreError::Io(error.into())),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn publish_no_replace(
    from: &Path,
    to: &Path,
) -> Result<PublishOutcome, ContentCandidateStoreError> {
    match fs::hard_link(from, to) {
        Ok(()) => {
            fs::remove_file(from).map_err(ContentCandidateStoreError::Io)?;
            Ok(PublishOutcome::Published)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Ok(PublishOutcome::AlreadyExists)
        }
        Err(error) => Err(ContentCandidateStoreError::Io(error)),
    }
}

fn files_equal(left: &Path, right: &Path) -> Result<bool, ContentCandidateStoreError> {
    let mut left = open_candidate_file(left)?;
    let mut right = open_candidate_file(right)?;
    if left
        .metadata()
        .map_err(ContentCandidateStoreError::Io)?
        .len()
        != right
            .metadata()
            .map_err(ContentCandidateStoreError::Io)?
            .len()
    {
        return Ok(false);
    }

    let mut left_buffer = [0; 64 * 1024];
    let mut right_buffer = [0; 64 * 1024];
    loop {
        let left_read = left
            .read(&mut left_buffer)
            .map_err(ContentCandidateStoreError::Io)?;
        let right_read = right
            .read(&mut right_buffer)
            .map_err(ContentCandidateStoreError::Io)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sync_directory(path: &Path) -> io::Result<()> {
    let directory: File = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )?
    .into();
    directory.sync_all()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

struct StagingPath {
    path: PathBuf,
    cleaned: bool,
}

impl StagingPath {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            cleaned: false,
        }
    }

    fn cleanup(mut self) -> Result<(), ContentCandidateStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => sync_directory(self.path.parent().ok_or(
                ContentCandidateStoreError::InvalidArchive("candidate staging path has no parent"),
            )?)
            .map_err(ContentCandidateStoreError::Io)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(ContentCandidateStoreError::Io(error)),
        }
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for StagingPath {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        match fs::remove_file(&self.path) {
            Ok(()) => {
                let _ = self.path.parent().map(sync_directory);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
}

fn parse_candidate_name(name: &str) -> Option<ContentTreeDigest> {
    let digest = name
        .strip_prefix(DIGEST_PREFIX)?
        .strip_suffix(CANDIDATE_SUFFIX)?;
    if digest.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (destination, pair) in bytes.iter_mut().zip(digest.as_bytes().chunks_exact(2)) {
        *destination = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(ContentTreeDigest::from_bytes(bytes))
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn is_staging_name(name: &str) -> bool {
    let Some(uuid) = name
        .strip_prefix(STAGING_PREFIX)
        .and_then(|name| name.strip_suffix(STAGING_SUFFIX))
    else {
        return false;
    };
    Uuid::parse_str(uuid).is_ok_and(|parsed| parsed.hyphenated().to_string() == uuid)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use super::*;
    use crate::tree::{asset, post, publication};

    fn fixture(marker: &str) -> DiscoveredContentTree {
        let publication_source = format!("title = \"Café {marker}\"\n");
        let posts = vec![
            post(
                "drafts/later.md",
                PostCollection::Drafts,
                format!("---\ntitle: Later {marker}\n---\n\nExact draft.\n"),
            ),
            post(
                "posts/hello.md",
                PostCollection::Posts,
                format!("---\ntitle: Hello {marker}\n---\n\nExact post.\n"),
            ),
        ];
        let assets = vec![
            asset(
                LogicalAssetPath::parse("assets/data.bin").unwrap(),
                vec![0, 1, 0xff, marker.len() as u8],
            ),
            asset(
                LogicalAssetPath::parse("assets/picture.png").unwrap(),
                b"not really a png".to_vec(),
            ),
        ];
        let total_bytes = publication_source.len() as u64
            + posts
                .iter()
                .map(|post| post.source.len() as u64)
                .sum::<u64>()
            + assets
                .iter()
                .map(|asset| asset.bytes.len() as u64)
                .sum::<u64>();
        DiscoveredContentTree::new(
            publication("publication.toml", publication_source),
            posts,
            assets,
            total_bytes,
        )
    }

    fn store() -> (tempfile::TempDir, ContentCandidateStore) {
        let state = tempfile::tempdir().unwrap();
        let store =
            ContentCandidateStore::open(state.path(), ContentTreeLimits::default()).unwrap();
        (state, store)
    }

    #[test]
    fn retains_and_recovers_every_exact_authored_byte() {
        let (_state, store) = store();
        let tree = fixture("one");
        let digest = store.retain(&tree).unwrap();

        assert_eq!(store.load(&digest).unwrap(), tree);
        let recovered = store.load_all().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].digest, digest);
        assert_eq!(recovered[0].tree, tree);

        #[cfg(unix)]
        {
            let directory_mode = fs::metadata(&store.root).unwrap().permissions().mode();
            let file_mode = fs::metadata(store.candidate_path(&digest))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(directory_mode & 0o077, 0);
            assert_eq!(file_mode & 0o077, 0);
        }
    }

    #[test]
    fn retain_is_deterministic_and_idempotent_across_input_order() {
        let (_state, store) = store();
        let tree = fixture("same");
        let digest = store.retain(&tree).unwrap();
        let first_bytes = fs::read(store.candidate_path(&digest)).unwrap();

        let mut reordered = tree.clone();
        reordered.posts.reverse();
        reordered.assets.reverse();
        assert_eq!(store.retain(&reordered).unwrap(), digest);
        assert_eq!(
            fs::read(store.candidate_path(&digest)).unwrap(),
            first_bytes
        );
        assert_eq!(store.load_all().unwrap().len(), 1);
    }

    #[test]
    fn load_all_is_digest_sorted_and_survives_a_new_store_instance() {
        let (state, store) = store();
        let first = store.retain(&fixture("first")).unwrap();
        let second = store.retain(&fixture("second")).unwrap();
        drop(store);

        let reopened =
            ContentCandidateStore::open(state.path(), ContentTreeLimits::default()).unwrap();
        let recovered = reopened.load_all().unwrap();
        let actual: Vec<_> = recovered
            .iter()
            .map(|candidate| candidate.digest.to_string())
            .collect();
        let mut expected = vec![first.to_string(), second.to_string()];
        expected.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn interrupted_private_staging_file_does_not_hide_recoverable_candidates() {
        let (_state, store) = store();
        let digest = store.retain(&fixture("retained")).unwrap();
        let staging_path = store.staging_path();
        let mut staging = create_staging_file(&staging_path).unwrap();
        staging.write_all(b"partial archive").unwrap();
        staging.sync_all().unwrap();
        drop(staging);

        let recovered = store.load_all().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].digest, digest);
        assert!(staging_path.exists());
    }

    #[test]
    fn rejects_header_digest_and_logical_path_tampering() {
        let (_state, store) = store();
        let tree = fixture("tamper");
        let digest = store.retain(&tree).unwrap();
        let path = store.candidate_path(&digest);
        let original = fs::read(&path).unwrap();

        let wrong_key = ContentTreeDigest::from_bytes([0x5a; 32]);
        let wrong_path = store.candidate_path(&wrong_key);
        fs::copy(&path, &wrong_path).unwrap();
        assert!(matches!(
            store.load(&wrong_key),
            Err(ContentCandidateStoreError::DigestMismatch)
        ));
        fs::remove_file(wrong_path).unwrap();

        let mut changed_digest = original.clone();
        changed_digest[ARCHIVE_MAGIC.len() + 2] ^= 1;
        fs::write(&path, changed_digest).unwrap();
        assert!(matches!(
            store.load(&digest),
            Err(ContentCandidateStoreError::DigestMismatch)
        ));

        let mut changed_path = original;
        let post_path_offset = changed_path
            .windows(b"posts/hello.md".len())
            .position(|window| window == b"posts/hello.md")
            .unwrap();
        changed_path[post_path_offset + "posts/".len()] = b'j';
        fs::write(&path, changed_path).unwrap();
        assert!(matches!(
            store.load(&digest),
            Err(ContentCandidateStoreError::DigestMismatch)
        ));
    }

    #[test]
    fn corrupt_existing_key_is_never_replaced_during_idempotent_retain() {
        let (_state, store) = store();
        let tree = fixture("collision");
        let digest = store.retain(&tree).unwrap();
        let path = store.candidate_path(&digest);
        let mut corrupt = fs::read(&path).unwrap();
        corrupt[ARCHIVE_MAGIC.len() + 2] ^= 1;
        fs::write(&path, &corrupt).unwrap();

        assert!(store.retain(&tree).is_err());
        assert_eq!(fs::read(path).unwrap(), corrupt);
    }

    #[test]
    fn rejects_trailing_data_and_oversized_counts_before_allocation() {
        let (_state, store) = store();
        let digest = store.retain(&fixture("bounds")).unwrap();
        let path = store.candidate_path(&digest);
        let mut bytes = fs::read(&path).unwrap();
        bytes.push(0);
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            store.load(&digest),
            Err(ContentCandidateStoreError::InvalidArchive(
                "candidate archive has trailing bytes"
            ))
        ));

        let digest = fixture("count").digest();
        let path = store.candidate_path(&digest);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(ARCHIVE_MAGIC);
        bytes.extend_from_slice(&ARCHIVE_VERSION.to_be_bytes());
        bytes.extend_from_slice(digest.as_bytes());
        bytes.extend_from_slice(&0_u64.to_be_bytes());
        bytes.extend_from_slice(&("publication.toml".len() as u32).to_be_bytes());
        bytes.extend_from_slice(b"publication.toml");
        bytes.extend_from_slice(&0_u64.to_be_bytes());
        bytes.extend_from_slice(&u32::MAX.to_be_bytes());
        fs::write(&path, bytes).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            store.load(&digest),
            Err(ContentCandidateStoreError::LimitExceeded(
                "candidate has too many entries"
            ))
        ));
    }

    #[test]
    #[cfg(unix)]
    fn rejects_symlink_non_regular_and_open_permission_targets() {
        let (_state, store) = store();
        let tree = fixture("unsafe");
        let digest = tree.digest();
        let target = store.root.join("outside");
        fs::write(&target, b"outside").unwrap();
        let candidate = store.candidate_path(&digest);
        symlink(&target, &candidate).unwrap();
        assert!(matches!(
            store.load(&digest),
            Err(ContentCandidateStoreError::UnsafeEntry)
        ));
        assert!(matches!(
            store.load_all(),
            Err(ContentCandidateStoreError::UnsafeEntry)
        ));
        fs::remove_file(&candidate).unwrap();

        fs::create_dir(&candidate).unwrap();
        assert!(matches!(
            store.load(&digest),
            Err(ContentCandidateStoreError::UnsafeEntry)
        ));
        fs::remove_dir(&candidate).unwrap();

        fs::write(&candidate, b"not private").unwrap();
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            store.load(&digest),
            Err(ContentCandidateStoreError::UnsafeEntry)
        ));
    }
}
