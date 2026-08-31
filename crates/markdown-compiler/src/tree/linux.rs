use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

use rustix::{
    fd::{AsFd, OwnedFd},
    fs::{Dir, FileType, Mode, OFlags, ResolveFlags, Stat, fstat, open, openat2},
    io::Errno,
};

use super::{
    ContentFileKind, ContentTreeLimits, DiscoveredAsset, DiscoveredContentTree, DiscoveredPost,
    DiscoveredPublication, LogicalAssetPath, PortableLogicalPath, asset, post, publication,
    tree_error,
};
use crate::{
    ContentValidationCode, ContentValidationError, ContentValidationErrors, DiagnosticCollector,
    FieldPath, ValidationLocation,
};
use crate::{LogicalContentPath, PostCollection};

const RESOLVE_POLICY: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS)
    .union(ResolveFlags::NO_XDEV);

pub(super) fn discover(
    root: &Path,
    limits: ContentTreeLimits,
    before_read: impl FnOnce(),
    mut before_file_read: impl FnMut(&str),
    mut after_file_read: impl FnMut(&str),
) -> Result<DiscoveredContentTree, ContentValidationErrors> {
    let root_fd = match open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(root_fd) => root_fd,
        Err(_) => {
            return Err(single_root_error(
                ContentValidationCode::ContentRootUnavailable,
            ));
        }
    };
    let root_fingerprint = match fstat(&root_fd) {
        Ok(stat) => Fingerprint::from_stat(&stat),
        Err(_) => {
            return Err(single_root_error(
                ContentValidationCode::ContentRootUnavailable,
            ));
        }
    };

    let mut discovery = Discovery::new(root_fd, root_fingerprint, limits);
    discovery.discover_root();
    before_read();
    discovery.read_files(&mut before_file_read, &mut after_file_read);
    discovery.verify_tree_stability();
    discovery.finish()
}

fn single_root_error(code: ContentValidationCode) -> ContentValidationErrors {
    let mut diagnostics = DiagnosticCollector::default();
    diagnostics.push(tree_error(
        "<content-root>",
        code,
        "configured content root is not an accessible directory",
    ));
    diagnostics.finish()
}

struct Discovery {
    root_fd: OwnedFd,
    root_fingerprint: Fingerprint,
    limits: ContentTreeLimits,
    diagnostics: DiagnosticCollector,
    files: Vec<DiscoveredFile>,
    loaded_files: Vec<DiscoveredFile>,
    directories: Vec<DiscoveredDirectory>,
    exact_paths: BTreeMap<String, LogicalContentPath>,
    folded_paths: BTreeMap<String, LogicalContentPath>,
    entry_count: usize,
    entry_limit_reported: bool,
    publication: Option<DiscoveredPublication>,
    posts: Vec<DiscoveredPost>,
    assets: Vec<DiscoveredAsset>,
    total_bytes: u64,
}

impl Discovery {
    fn new(root_fd: OwnedFd, root_fingerprint: Fingerprint, limits: ContentTreeLimits) -> Self {
        Self {
            root_fd,
            root_fingerprint,
            limits,
            diagnostics: DiagnosticCollector::default(),
            files: Vec::new(),
            loaded_files: Vec::new(),
            directories: Vec::new(),
            exact_paths: BTreeMap::new(),
            folded_paths: BTreeMap::new(),
            entry_count: 0,
            entry_limit_reported: false,
            publication: None,
            posts: Vec::new(),
            assets: Vec::new(),
            total_bytes: 0,
        }
    }

    fn discover_root(&mut self) {
        let remaining = self.limits.entries.get().saturating_sub(self.entry_count);
        let entries = match read_bounded_sorted_entries(&self.root_fd, remaining, |name| {
            std::str::from_utf8(name)
                .ok()
                .and_then(ReservedRootEntry::from_name)
                .is_some()
        }) {
            Ok(entries) => entries,
            Err(ReadEntriesError::LimitExceeded) => {
                self.push_entry_limit("<content-root>");
                return;
            }
            Err(ReadEntriesError::Unreadable) => {
                self.diagnostics.push(tree_error(
                    "<content-root>",
                    ContentValidationCode::ContentEntryUnreadable,
                    "content root entries could not be read",
                ));
                return;
            }
        };
        self.entry_count += entries.counted;

        let mut publication_found = false;
        for entry in entries.entries {
            let Some(name) = root_entry_name(&entry) else {
                continue;
            };
            let reserved = ReservedRootEntry::from_name(&name);
            let Some(reserved) = reserved else {
                continue;
            };
            if reserved.exact_name() != name {
                self.diagnostics.push(tree_error(
                    name,
                    ContentValidationCode::LogicalPathCaseCollision,
                    "root entry differs from a reserved content name only by ASCII case",
                ));
                continue;
            }

            match reserved {
                ReservedRootEntry::Publication => {
                    publication_found = true;
                    self.discover_publication(entry);
                }
                ReservedRootEntry::Posts => {
                    self.discover_namespace(entry, ContentFileKind::Post(PostCollection::Posts));
                }
                ReservedRootEntry::Drafts => {
                    self.discover_namespace(entry, ContentFileKind::Post(PostCollection::Drafts));
                }
                ReservedRootEntry::Assets => {
                    self.discover_namespace(entry, ContentFileKind::Asset);
                }
            }
        }

        if !publication_found {
            self.diagnostics.push(tree_error(
                "publication.toml",
                ContentValidationCode::PublicationFileMissing,
                "publication.toml is required at the content root",
            ));
        }
    }

    fn discover_publication(&mut self, entry: RawEntry) {
        let logical = PortableLogicalPath("publication.toml".to_owned());
        if !self.root_path_within_limit(&logical) {
            return;
        }
        if !self.register_path(&logical, ContentFileKind::Publication) {
            return;
        }
        let Some((file_type, fingerprint)) = self.inspect(&logical, entry.file_type) else {
            return;
        };
        if file_type != FileType::RegularFile {
            self.diagnostics.push(tree_error(
                logical.as_str(),
                ContentValidationCode::ContentNamespaceInvalid,
                "publication.toml must be a regular file",
            ));
            return;
        }
        self.files.push(DiscoveredFile {
            raw_path: logical.as_str().to_owned(),
            logical,
            kind: ContentFileKind::Publication,
            fingerprint,
        });
    }

    fn discover_namespace(&mut self, entry: RawEntry, kind: ContentFileKind) {
        let name = match kind {
            ContentFileKind::Post(PostCollection::Posts) => "posts",
            ContentFileKind::Post(PostCollection::Drafts) => "drafts",
            ContentFileKind::Asset => "assets",
            ContentFileKind::Publication => return,
        };
        let logical = PortableLogicalPath(name.to_owned());
        if !self.root_path_within_limit(&logical) {
            return;
        }
        if !self.register_path(&logical, kind) {
            return;
        }
        let Some((file_type, fingerprint)) = self.inspect(&logical, entry.file_type) else {
            return;
        };
        if file_type != FileType::Directory {
            self.diagnostics.push(tree_error(
                logical.as_str(),
                ContentValidationCode::ContentNamespaceInvalid,
                "managed content namespace must be a directory",
            ));
            return;
        }
        self.directories.push(DiscoveredDirectory {
            raw_path: logical.as_str().to_owned(),
            logical_path: logical.as_str().to_owned(),
            fingerprint,
        });
        self.walk_directory(logical.as_str(), logical.as_str(), kind, 1, fingerprint);
    }

    fn walk_directory(
        &mut self,
        raw_directory: &str,
        logical_directory: &str,
        kind: ContentFileKind,
        depth: usize,
        expected: Fingerprint,
    ) {
        let directory_fd = match self.secure_open(
            raw_directory,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        ) {
            Ok(fd) => fd,
            Err(error) => {
                self.push_open_error(logical_directory, error, false);
                return;
            }
        };
        let Ok(stat) = fstat(&directory_fd) else {
            self.push_unreadable(logical_directory);
            return;
        };
        if !expected.same_identity(&stat) {
            self.push_changed(logical_directory);
            return;
        }
        let remaining = self.limits.entries.get().saturating_sub(self.entry_count);
        let entries = match read_bounded_sorted_entries(&directory_fd, remaining, |_| true) {
            Ok(entries) => entries,
            Err(ReadEntriesError::LimitExceeded) => {
                self.push_entry_limit(logical_directory);
                return;
            }
            Err(ReadEntriesError::Unreadable) => {
                self.push_unreadable(logical_directory);
                return;
            }
        };
        self.entry_count += entries.counted;

        for entry in entries.entries {
            let name = match std::str::from_utf8(&entry.name) {
                Ok(name) => name,
                Err(_) => {
                    let encoded = encode_filename_bytes(&entry.name);
                    self.diagnostics.push(tree_error(
                        format!("{logical_directory}/{encoded}"),
                        ContentValidationCode::UnsupportedFilenameEncoding,
                        "managed content names must use UTF-8",
                    ));
                    continue;
                }
            };
            let raw_path = format!("{raw_directory}/{name}");
            let logical_text = format!("{logical_directory}/{name}");
            let logical =
                match PortableLogicalPath::parse(&logical_text, self.limits.path_bytes.get()) {
                    Ok(logical) => logical,
                    Err(super::LogicalTreePathError::TooLong) => {
                        self.diagnostics.push(tree_error(
                            logical_text,
                            ContentValidationCode::ContentPathTooLong,
                            "logical content path exceeds the configured byte limit",
                        ));
                        continue;
                    }
                    Err(_) => {
                        self.diagnostics.push(tree_error(
                            logical_text,
                            ContentValidationCode::InvalidLogicalContentPath,
                            "managed content path does not use portable filename components",
                        ));
                        continue;
                    }
                };

            let entry_depth = depth.saturating_add(1);
            if entry_depth > self.limits.depth.get() {
                self.diagnostics.push(tree_error(
                    logical.as_str(),
                    ContentValidationCode::ContentDepthLimitExceeded,
                    "managed content path exceeds the configured depth limit",
                ));
                continue;
            }
            if !self.register_path(&logical, kind) {
                continue;
            }

            let Some((file_type, fingerprint)) = self.inspect(&logical, entry.file_type) else {
                continue;
            };
            match file_type {
                FileType::Directory => {
                    self.directories.push(DiscoveredDirectory {
                        raw_path: raw_path.clone(),
                        logical_path: logical.as_str().to_owned(),
                        fingerprint,
                    });
                    self.walk_directory(
                        &raw_path,
                        logical.as_str(),
                        kind,
                        entry_depth,
                        fingerprint,
                    );
                }
                FileType::RegularFile => {
                    if self.validate_file_name(&logical, kind) {
                        self.files.push(DiscoveredFile {
                            raw_path,
                            logical,
                            kind,
                            fingerprint,
                        });
                    }
                }
                _ => self.diagnostics.push(tree_error(
                    logical.as_str(),
                    ContentValidationCode::UnsupportedContentEntryKind,
                    "managed content accepts only directories and regular files",
                )),
            }
        }
    }

    fn inspect(
        &mut self,
        logical: &PortableLogicalPath,
        entry_type: FileType,
    ) -> Option<(FileType, Fingerprint)> {
        if entry_type == FileType::Symlink {
            self.diagnostics.push(tree_error(
                logical.as_str(),
                ContentValidationCode::ContentSymlinkUnsupported,
                "descendant symlinks are not supported in managed content",
            ));
            return None;
        }
        let fd = match self.secure_open(logical.as_str(), OFlags::PATH | OFlags::CLOEXEC) {
            Ok(fd) => fd,
            Err(error) => {
                self.push_open_error(logical.as_str(), error, entry_type == FileType::Symlink);
                return None;
            }
        };
        let stat = match fstat(&fd) {
            Ok(stat) => stat,
            Err(_) => {
                self.push_unreadable(logical.as_str());
                return None;
            }
        };
        Some((
            FileType::from_raw_mode(stat.st_mode),
            Fingerprint::from_stat(&stat),
        ))
    }

    fn secure_open(&self, path: &str, flags: OFlags) -> Result<OwnedFd, Errno> {
        openat2(&self.root_fd, path, flags, Mode::empty(), RESOLVE_POLICY)
    }

    fn root_path_within_limit(&mut self, logical: &PortableLogicalPath) -> bool {
        if logical.as_str().len() <= self.limits.path_bytes.get() {
            true
        } else {
            self.diagnostics.push(tree_error(
                logical.as_str(),
                ContentValidationCode::ContentPathTooLong,
                "logical content path exceeds the configured byte limit",
            ));
            false
        }
    }

    fn register_path(&mut self, logical: &PortableLogicalPath, kind: ContentFileKind) -> bool {
        let path = LogicalContentPath::new(logical.as_str());
        if let Some(first) = self.exact_paths.get(logical.as_str()) {
            let code = if kind == ContentFileKind::Asset {
                ContentValidationCode::DuplicateLogicalAssetPath
            } else {
                ContentValidationCode::InvalidLogicalContentPath
            };
            self.diagnostics.push(
                tree_error(logical.as_str(), code, "logical content path is duplicated")
                    .with_related(ValidationLocation::new(
                        first.clone(),
                        FieldPath::new("$path"),
                    )),
            );
            return false;
        }
        let folded = logical.case_collision_key();
        if let Some(first) = self.folded_paths.get(&folded) {
            self.diagnostics.push(
                tree_error(
                    logical.as_str(),
                    ContentValidationCode::LogicalPathCaseCollision,
                    "logical content path collides after ASCII case normalization",
                )
                .with_related(ValidationLocation::new(
                    first.clone(),
                    FieldPath::new("$path"),
                )),
            );
            return false;
        }
        self.exact_paths
            .insert(logical.as_str().to_owned(), path.clone());
        self.folded_paths.insert(folded, path);
        true
    }

    fn validate_file_name(&mut self, logical: &PortableLogicalPath, kind: ContentFileKind) -> bool {
        let name = logical.as_str().rsplit('/').next().unwrap_or_default();
        match kind {
            ContentFileKind::Publication => true,
            ContentFileKind::Post(_) if name.ends_with(".md") => true,
            ContentFileKind::Post(_) => {
                self.diagnostics.push(tree_error(
                    logical.as_str(),
                    ContentValidationCode::UnexpectedPostEntry,
                    "posts and drafts accept only files with the exact .md extension",
                ));
                false
            }
            ContentFileKind::Asset => {
                let lower = name.to_ascii_lowercase();
                if lower.ends_with(".svg") || lower.ends_with(".svgz") {
                    self.diagnostics.push(tree_error(
                        logical.as_str(),
                        ContentValidationCode::AuthoredSvgUnsupported,
                        "authored SVG and compressed SVG assets are not supported in v1",
                    ));
                    false
                } else {
                    true
                }
            }
        }
    }

    fn read_files(
        &mut self,
        before_file_read: &mut impl FnMut(&str),
        after_file_read: &mut impl FnMut(&str),
    ) {
        self.files
            .sort_by(|left, right| left.logical.as_str().cmp(right.logical.as_str()));
        let files = std::mem::take(&mut self.files);
        for discovered in files {
            let Some(bytes) = self.read_file(&discovered, before_file_read) else {
                continue;
            };
            after_file_read(discovered.logical.as_str());
            self.loaded_files.push(discovered.clone());
            match discovered.kind {
                ContentFileKind::Publication => match String::from_utf8(bytes) {
                    Ok(source) => {
                        self.publication = Some(publication(discovered.logical.as_str(), source));
                    }
                    Err(_) => self.push_invalid_utf8(discovered.logical.as_str()),
                },
                ContentFileKind::Post(collection) => match String::from_utf8(bytes) {
                    Ok(source) => {
                        self.posts
                            .push(post(discovered.logical.as_str(), collection, source));
                    }
                    Err(_) => self.push_invalid_utf8(discovered.logical.as_str()),
                },
                ContentFileKind::Asset => {
                    match LogicalAssetPath::parse(discovered.logical.as_str()) {
                        Ok(path) => self.assets.push(asset(path, bytes)),
                        Err(_) => self.diagnostics.push(tree_error(
                            discovered.logical.as_str(),
                            ContentValidationCode::InvalidLogicalContentPath,
                            "asset path failed its validated logical-path contract",
                        )),
                    }
                }
            }
        }
        self.posts
            .sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));
        self.assets
            .sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));
    }

    fn read_file(
        &mut self,
        discovered: &DiscoveredFile,
        before_file_read: &mut impl FnMut(&str),
    ) -> Option<Vec<u8>> {
        let limit = self.limits.file_limit(discovered.kind).get();
        if discovered.fingerprint.size > limit {
            self.push_file_too_large(discovered.logical.as_str());
            return None;
        }
        let fd = match self.secure_open(
            &discovered.raw_path,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        ) {
            Ok(fd) => fd,
            Err(error) => {
                self.push_open_error(discovered.logical.as_str(), error, false);
                return None;
            }
        };
        let initial = match fstat(&fd) {
            Ok(stat) => stat,
            Err(_) => {
                self.push_unreadable(discovered.logical.as_str());
                return None;
            }
        };
        if FileType::from_raw_mode(initial.st_mode) != FileType::RegularFile
            || !discovered.fingerprint.stable_matches(&initial)
        {
            self.push_changed(discovered.logical.as_str());
            return None;
        }
        before_file_read(discovered.logical.as_str());

        let mut file = File::from(fd);
        let mut bytes = Vec::with_capacity((initial.st_size as u64).min(limit) as usize);
        let read_result = file.by_ref().take(limit + 1).read_to_end(&mut bytes);
        if read_result.is_err() {
            self.push_unreadable(discovered.logical.as_str());
            return None;
        }
        if bytes.len() as u64 > limit {
            self.push_file_too_large(discovered.logical.as_str());
            return None;
        }
        let final_stat = match fstat(&file) {
            Ok(stat) => stat,
            Err(_) => {
                self.push_unreadable(discovered.logical.as_str());
                return None;
            }
        };
        if !Fingerprint::from_stat(&initial).stable_matches(&final_stat)
            || final_stat.st_size < 0
            || final_stat.st_size as usize != bytes.len()
        {
            self.push_changed(discovered.logical.as_str());
            return None;
        }

        let Some(next_total) = self.total_bytes.checked_add(bytes.len() as u64) else {
            self.push_tree_too_large(discovered.logical.as_str());
            return None;
        };
        if next_total > self.limits.total_tree_bytes.get() {
            self.push_tree_too_large(discovered.logical.as_str());
            return None;
        }
        self.total_bytes = next_total;
        Some(bytes)
    }

    fn verify_tree_stability(&mut self) {
        let root_stat = match fstat(&self.root_fd) {
            Ok(stat) => stat,
            Err(_) => {
                self.push_changed("<content-root>");
                return;
            }
        };
        if !self.root_fingerprint.stable_matches(&root_stat) {
            self.push_changed("<content-root>");
        }

        let directories = self.directories.clone();
        for directory in directories {
            let fd = match self.secure_open(
                &directory.raw_path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            ) {
                Ok(fd) => fd,
                Err(_) => {
                    self.push_changed(&directory.logical_path);
                    continue;
                }
            };
            match fstat(&fd) {
                Ok(stat) if directory.fingerprint.stable_matches(&stat) => {}
                _ => self.push_changed(&directory.logical_path),
            }
        }

        let files = self.loaded_files.clone();
        for file in files {
            let fd = match self.secure_open(&file.raw_path, OFlags::PATH | OFlags::CLOEXEC) {
                Ok(fd) => fd,
                Err(_) => {
                    self.push_changed(file.logical.as_str());
                    continue;
                }
            };
            match fstat(&fd) {
                Ok(stat) if file.fingerprint.stable_matches(&stat) => {}
                _ => self.push_changed(file.logical.as_str()),
            }
        }
    }

    fn finish(mut self) -> Result<DiscoveredContentTree, ContentValidationErrors> {
        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics.finish());
        }
        let Some(publication) = self.publication else {
            self.diagnostics.push(tree_error(
                "publication.toml",
                ContentValidationCode::ContentEntryUnreadable,
                "publication.toml could not be loaded",
            ));
            return Err(self.diagnostics.finish());
        };
        Ok(DiscoveredContentTree::new(
            publication,
            self.posts,
            self.assets,
            self.total_bytes,
        ))
    }

    fn push_open_error(&mut self, path: &str, error: Errno, symlink_hint: bool) {
        let (code, message) = classify_open_error(error, symlink_hint);
        self.diagnostics.push(tree_error(path, code, message));
    }

    fn push_entry_limit(&mut self, path: &str) {
        if self.entry_limit_reported {
            return;
        }
        self.entry_limit_reported = true;
        self.entry_count = self.limits.entries.get();
        self.diagnostics.push(tree_error(
            path,
            ContentValidationCode::ContentEntryLimitExceeded,
            "content discovery exceeds the configured directory-entry scan limit",
        ));
    }

    fn push_unreadable(&mut self, path: &str) {
        self.diagnostics.push(tree_error(
            path,
            ContentValidationCode::ContentEntryUnreadable,
            "managed content entry could not be read",
        ));
    }

    fn push_changed(&mut self, path: &str) {
        self.diagnostics.push(tree_error(
            path,
            ContentValidationCode::ContentEntryChanged,
            "managed content tree changed during discovery",
        ));
    }

    fn push_file_too_large(&mut self, path: &str) {
        self.diagnostics.push(ContentValidationError::new(
            LogicalContentPath::new(path),
            "$document",
            ContentValidationCode::ContentFileTooLarge,
            "managed content file exceeds its configured byte limit",
        ));
    }

    fn push_tree_too_large(&mut self, path: &str) {
        self.diagnostics.push(ContentValidationError::new(
            LogicalContentPath::new(path),
            "$document",
            ContentValidationCode::ContentTreeTooLarge,
            "managed content exceeds the configured total byte limit",
        ));
    }

    fn push_invalid_utf8(&mut self, path: &str) {
        self.diagnostics.push(ContentValidationError::new(
            LogicalContentPath::new(path),
            "$document",
            ContentValidationCode::ContentTextInvalidUtf8,
            "publication and post source files must contain UTF-8 text",
        ));
    }
}

#[derive(Clone)]
struct RawEntry {
    name: Vec<u8>,
    file_type: FileType,
}

struct BoundedEntries {
    entries: Vec<RawEntry>,
    counted: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadEntriesError {
    Unreadable,
    LimitExceeded,
}

fn read_bounded_sorted_entries(
    fd: impl AsFd,
    max_entries: usize,
    mut retain: impl FnMut(&[u8]) -> bool,
) -> Result<BoundedEntries, ReadEntriesError> {
    let mut entries = Vec::new();
    let mut counted = 0_usize;
    let directory = Dir::read_from(fd).map_err(|_| ReadEntriesError::Unreadable)?;
    for entry in directory {
        let entry = entry.map_err(|_| ReadEntriesError::Unreadable)?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if retain(name) {
            if counted >= max_entries {
                return Err(ReadEntriesError::LimitExceeded);
            }
            counted += 1;
            entries.push(RawEntry {
                name: name.to_vec(),
                file_type: entry.file_type(),
            });
        }
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(BoundedEntries { entries, counted })
}

pub(super) fn classify_open_error(
    error: Errno,
    symlink_hint: bool,
) -> (ContentValidationCode, &'static str) {
    if symlink_hint || error == Errno::LOOP {
        (
            ContentValidationCode::ContentSymlinkUnsupported,
            "descendant symlinks are not supported in managed content",
        )
    } else if matches!(error, Errno::NOSYS | Errno::INVAL | Errno::TOOBIG) {
        (
            ContentValidationCode::ContentPlatformUnsupported,
            "Linux kernel cannot enforce the required safe path resolver policy",
        )
    } else if error == Errno::XDEV {
        (
            ContentValidationCode::UnsupportedContentEntryKind,
            "managed content cannot cross a mount boundary",
        )
    } else {
        (
            ContentValidationCode::ContentEntryUnreadable,
            "managed content entry could not be opened safely",
        )
    }
}

fn root_entry_name(entry: &RawEntry) -> Option<String> {
    std::str::from_utf8(&entry.name).ok().map(str::to_owned)
}

#[derive(Clone, Copy)]
enum ReservedRootEntry {
    Publication,
    Posts,
    Drafts,
    Assets,
}

impl ReservedRootEntry {
    fn from_name(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("publication.toml") {
            Some(Self::Publication)
        } else if name.eq_ignore_ascii_case("posts") {
            Some(Self::Posts)
        } else if name.eq_ignore_ascii_case("drafts") {
            Some(Self::Drafts)
        } else if name.eq_ignore_ascii_case("assets") {
            Some(Self::Assets)
        } else {
            None
        }
    }

    const fn exact_name(self) -> &'static str {
        match self {
            Self::Publication => "publication.toml",
            Self::Posts => "posts",
            Self::Drafts => "drafts",
            Self::Assets => "assets",
        }
    }
}

#[derive(Clone, Copy)]
struct Fingerprint {
    device: u64,
    inode: u64,
    mode: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: u64,
    changed_seconds: i64,
    changed_nanoseconds: u64,
}

impl Fingerprint {
    fn from_stat(stat: &Stat) -> Self {
        Self {
            device: stat.st_dev,
            inode: stat.st_ino,
            mode: stat.st_mode,
            size: u64::try_from(stat.st_size).unwrap_or(u64::MAX),
            modified_seconds: stat.st_mtime,
            modified_nanoseconds: stat.st_mtime_nsec,
            changed_seconds: stat.st_ctime,
            changed_nanoseconds: stat.st_ctime_nsec,
        }
    }

    fn same_identity(self, stat: &Stat) -> bool {
        self.device == stat.st_dev
            && self.inode == stat.st_ino
            && FileType::from_raw_mode(self.mode) == FileType::from_raw_mode(stat.st_mode)
    }

    fn stable_matches(self, stat: &Stat) -> bool {
        self.same_identity(stat)
            && self.size == u64::try_from(stat.st_size).unwrap_or(u64::MAX)
            && self.modified_seconds == stat.st_mtime
            && self.modified_nanoseconds == stat.st_mtime_nsec
            && self.changed_seconds == stat.st_ctime
            && self.changed_nanoseconds == stat.st_ctime_nsec
    }
}

#[derive(Clone)]
struct DiscoveredFile {
    raw_path: String,
    logical: PortableLogicalPath,
    kind: ContentFileKind,
    fingerprint: Fingerprint,
}

#[derive(Clone)]
struct DiscoveredDirectory {
    raw_path: String,
    logical_path: String,
    fingerprint: Fingerprint,
}

fn encode_filename_bytes(bytes: &[u8]) -> String {
    let mut output = String::from("<non-utf8-");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output.push('>');
    output
}
