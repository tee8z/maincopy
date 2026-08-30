//! Descriptor-confined frontend build input and output.

use std::io;

use crate::frontend_build_support::{FrontendBuildError, FrontendBuildOperation};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod supported {
    use std::{
        ffi::{CStr, CString, OsStr},
        fs::{File, Metadata},
        io::{Read as _, Write as _},
        os::unix::{ffi::OsStrExt as _, fs::MetadataExt as _},
        path::{Component, Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    };

    use rustix::{
        fs::{
            AtFlags, Dir, FileType, Mode, OFlags, Stat, mkdirat, openat, renameat, statat, unlinkat,
        },
        io::Errno,
    };

    use super::{FrontendBuildError, FrontendBuildOperation, io};

    const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::CLOEXEC)
        .union(OFlags::NOFOLLOW);
    const INPUT_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::CLOEXEC)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::NONBLOCK);
    const TEMP_FLAGS: OFlags = OFlags::WRONLY
        .union(OFlags::CREATE)
        .union(OFlags::EXCL)
        .union(OFlags::CLOEXEC)
        .union(OFlags::NOFOLLOW);
    const DIRECTORY_MODE: Mode = Mode::RUSR
        .union(Mode::WUSR)
        .union(Mode::XUSR)
        .union(Mode::RGRP)
        .union(Mode::XGRP)
        .union(Mode::ROTH)
        .union(Mode::XOTH);
    const FILE_MODE: Mode = Mode::RUSR.union(Mode::WUSR);
    const TEMP_ATTEMPTS: u64 = 128;
    const READ_CHUNK_BYTES: usize = 64 * 1024;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Fingerprint {
        device: u64,
        inode: u64,
        mode: u64,
        links: u64,
        bytes: i128,
        modified_seconds: i128,
        modified_nanoseconds: i128,
        changed_seconds: i128,
        changed_nanoseconds: i128,
    }

    impl Fingerprint {
        fn from_stat(stat: &Stat) -> Self {
            Self {
                device: stat_device(stat),
                inode: stat.st_ino,
                mode: u64::from(stat.st_mode),
                links: stat_links(stat),
                bytes: i128::from(stat.st_size),
                modified_seconds: i128::from(stat.st_mtime),
                modified_nanoseconds: i128::from(stat.st_mtime_nsec),
                changed_seconds: i128::from(stat.st_ctime),
                changed_nanoseconds: i128::from(stat.st_ctime_nsec),
            }
        }

        fn from_metadata(metadata: &Metadata) -> Self {
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                mode: u64::from(metadata.mode()),
                links: metadata.nlink(),
                bytes: i128::from(metadata.size()),
                modified_seconds: i128::from(metadata.mtime()),
                modified_nanoseconds: i128::from(metadata.mtime_nsec()),
                changed_seconds: i128::from(metadata.ctime()),
                changed_nanoseconds: i128::from(metadata.ctime_nsec()),
            }
        }

        fn byte_length(self, path: &Path) -> Result<u64, FrontendBuildError> {
            u64::try_from(self.bytes).map_err(|_| FrontendBuildError::InputChanged {
                path: path.to_owned(),
            })
        }

        const fn same_object(self, other: Self) -> bool {
            self.device == other.device && self.inode == other.inode && self.mode == other.mode
        }
    }

    #[cfg(target_os = "linux")]
    fn stat_device(stat: &Stat) -> u64 {
        stat.st_dev
    }

    #[cfg(target_os = "macos")]
    fn stat_device(stat: &Stat) -> u64 {
        stat.st_dev as u64
    }

    #[cfg(target_os = "linux")]
    fn stat_links(stat: &Stat) -> u64 {
        stat.st_nlink
    }

    #[cfg(target_os = "macos")]
    fn stat_links(stat: &Stat) -> u64 {
        u64::from(stat.st_nlink)
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum ConfinedEntryKind {
        File,
        Directory,
        Symlink,
        Special,
    }

    pub(crate) struct ConfinedEntry {
        name: CString,
        fingerprint: Fingerprint,
        kind: ConfinedEntryKind,
    }

    #[derive(Clone)]
    struct ConfinedPathComponent {
        name: CString,
        fingerprint: Fingerprint,
        display_path: PathBuf,
    }

    #[derive(Clone)]
    pub(crate) struct ConfinedDirectoryIdentity {
        root: ConfinedDirectory,
        components: Vec<ConfinedPathComponent>,
    }

    impl ConfinedDirectoryIdentity {
        pub(crate) fn child(
            &self,
            entry: &ConfinedEntry,
            display_path: &Path,
        ) -> Result<Self, FrontendBuildError> {
            if entry.kind != ConfinedEntryKind::Directory {
                return Err(FrontendBuildError::SpecialFile {
                    path: display_path.to_owned(),
                });
            }
            let mut components = self.components.clone();
            components.push(ConfinedPathComponent {
                name: entry.name.clone(),
                fingerprint: entry.fingerprint,
                display_path: display_path.to_owned(),
            });
            Ok(Self {
                root: self.root.clone(),
                components,
            })
        }

        pub(crate) fn file(
            &self,
            entry: &ConfinedEntry,
            display_path: &Path,
        ) -> Result<ConfinedFileIdentity, FrontendBuildError> {
            if entry.kind != ConfinedEntryKind::File {
                return Err(FrontendBuildError::SpecialFile {
                    path: display_path.to_owned(),
                });
            }
            Ok(ConfinedFileIdentity {
                directory: self.clone(),
                leaf: entry.name.clone(),
                fingerprint: entry.fingerprint,
                display_path: display_path.to_owned(),
            })
        }

        pub(crate) fn verify_unchanged(&self) -> Result<(), FrontendBuildError> {
            let _ = self.open_verified()?;
            Ok(())
        }

        fn open_verified(&self) -> Result<File, FrontendBuildError> {
            self.root.verify_unchanged()?;
            let mut descriptor =
                self.root
                    .descriptor
                    .try_clone()
                    .map_err(|source| FrontendBuildError::Io {
                        operation: FrontendBuildOperation::Open,
                        path: self.root.display_path.clone(),
                        source,
                    })?;
            for component in &self.components {
                let captured = statat(&descriptor, &component.name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(|error| changed_or_io(&component.display_path, error))?;
                if FileType::from_raw_mode(captured.st_mode) != FileType::Directory
                    || Fingerprint::from_stat(&captured) != component.fingerprint
                {
                    return Err(FrontendBuildError::InputChanged {
                        path: component.display_path.clone(),
                    });
                }
                let opened = openat(&descriptor, &component.name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(|error| changed_or_io(&component.display_path, error))?;
                let opened = File::from(opened);
                if Fingerprint::from_metadata(&opened.metadata().map_err(|source| {
                    FrontendBuildError::Io {
                        operation: FrontendBuildOperation::Inspect,
                        path: component.display_path.clone(),
                        source,
                    }
                })?) != component.fingerprint
                {
                    return Err(FrontendBuildError::InputChanged {
                        path: component.display_path.clone(),
                    });
                }
                let post_open = statat(&descriptor, &component.name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(|error| changed_or_io(&component.display_path, error))?;
                if FileType::from_raw_mode(post_open.st_mode) != FileType::Directory
                    || Fingerprint::from_stat(&post_open) != component.fingerprint
                {
                    return Err(FrontendBuildError::InputChanged {
                        path: component.display_path.clone(),
                    });
                }
                descriptor = opened;
            }
            self.root.verify_unchanged()?;
            Ok(descriptor)
        }
    }

    #[derive(Clone)]
    pub(crate) struct ConfinedFileIdentity {
        directory: ConfinedDirectoryIdentity,
        leaf: CString,
        fingerprint: Fingerprint,
        display_path: PathBuf,
    }

    impl ConfinedFileIdentity {
        pub(crate) fn verify_unchanged(&self) -> Result<(), FrontendBuildError> {
            let descriptor = self.directory.open_verified()?;
            let captured = statat(&descriptor, &self.leaf, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| changed_or_io(&self.display_path, error))?;
            if FileType::from_raw_mode(captured.st_mode) != FileType::RegularFile
                || Fingerprint::from_stat(&captured) != self.fingerprint
            {
                return Err(FrontendBuildError::InputChanged {
                    path: self.display_path.clone(),
                });
            }
            let opened = openat(&descriptor, &self.leaf, INPUT_FLAGS, Mode::empty())
                .map_err(|error| changed_or_io(&self.display_path, error))?;
            let opened = File::from(opened);
            if Fingerprint::from_metadata(&opened.metadata().map_err(|source| {
                FrontendBuildError::Io {
                    operation: FrontendBuildOperation::Inspect,
                    path: self.display_path.clone(),
                    source,
                }
            })?) != self.fingerprint
            {
                return Err(FrontendBuildError::InputChanged {
                    path: self.display_path.clone(),
                });
            }
            let post_open = statat(&descriptor, &self.leaf, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| changed_or_io(&self.display_path, error))?;
            if FileType::from_raw_mode(post_open.st_mode) != FileType::RegularFile
                || Fingerprint::from_stat(&post_open) != self.fingerprint
            {
                return Err(FrontendBuildError::InputChanged {
                    path: self.display_path.clone(),
                });
            }
            self.directory.verify_unchanged()?;
            Ok(())
        }
    }

    impl ConfinedEntry {
        pub(crate) fn name(&self) -> &OsStr {
            OsStr::from_bytes(self.name.to_bytes())
        }

        pub(crate) const fn kind(&self) -> ConfinedEntryKind {
            self.kind
        }
    }

    #[derive(Clone)]
    pub(crate) struct ConfinedDirectory {
        descriptor: Arc<File>,
        fingerprint: Fingerprint,
        display_path: PathBuf,
    }

    impl ConfinedDirectory {
        pub(crate) fn identity(&self) -> ConfinedDirectoryIdentity {
            ConfinedDirectoryIdentity {
                root: self.clone(),
                components: Vec::new(),
            }
        }

        pub(crate) fn open_absolute(path: &Path) -> Result<Self, FrontendBuildError> {
            let mut components = path.components();
            if components.next() != Some(Component::RootDir) {
                return Err(FrontendBuildError::PathEscape {
                    path: path.to_owned(),
                });
            }

            let root = openat(rustix::fs::CWD, "/", DIRECTORY_FLAGS, Mode::empty())
                .map_err(|error| io_error(FrontendBuildOperation::Open, Path::new("/"), error))?;
            let root_file = File::from(root);
            let mut directory = Self {
                fingerprint: Fingerprint::from_metadata(&root_file.metadata().map_err(
                    |source| FrontendBuildError::Io {
                        operation: FrontendBuildOperation::Inspect,
                        path: PathBuf::from("/"),
                        source,
                    },
                )?),
                descriptor: Arc::new(root_file),
                display_path: PathBuf::from("/"),
            };

            for component in components {
                let Component::Normal(name) = component else {
                    return Err(FrontendBuildError::PathEscape {
                        path: path.to_owned(),
                    });
                };
                let next_path = directory.display_path.join(name);
                directory = directory.open_named_directory_inner(name, &next_path, false)?;
            }
            Ok(directory)
        }

        pub(crate) fn open_required_directory(
            &self,
            name: &OsStr,
            display_path: &Path,
        ) -> Result<Self, FrontendBuildError> {
            match self.open_named_directory(name, display_path) {
                Err(FrontendBuildError::Io { source, .. })
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    Err(FrontendBuildError::MissingInputRoot {
                        path: display_path.to_owned(),
                    })
                }
                result => result,
            }
        }

        pub(crate) fn ensure_output_directory(
            &self,
            name: &OsStr,
            display_path: &Path,
        ) -> Result<Self, FrontendBuildError> {
            match self.open_named_directory(name, display_path) {
                Ok(directory) => Ok(directory),
                Err(FrontendBuildError::Io { source, .. })
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    match mkdirat(&*self.descriptor, name, DIRECTORY_MODE) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(error) => {
                            return Err(io_error(
                                FrontendBuildOperation::CreateOutputDirectory,
                                display_path,
                                error,
                            ));
                        }
                    }
                    self.refreshed()?.open_named_directory(name, display_path)
                }
                Err(error) => Err(error),
            }
        }

        fn open_named_directory(
            &self,
            name: &OsStr,
            display_path: &Path,
        ) -> Result<Self, FrontendBuildError> {
            self.open_named_directory_inner(name, display_path, true)
        }

        fn open_named_directory_inner(
            &self,
            name: &OsStr,
            display_path: &Path,
            verify_parent: bool,
        ) -> Result<Self, FrontendBuildError> {
            if verify_parent {
                self.verify_unchanged()?;
            }
            let captured = statat(&*self.descriptor, name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| io_error(FrontendBuildOperation::Inspect, display_path, error))?;
            let kind = FileType::from_raw_mode(captured.st_mode);
            match kind {
                FileType::Symlink => {
                    return Err(FrontendBuildError::Symlink {
                        path: display_path.to_owned(),
                    });
                }
                FileType::Directory => {}
                _ => {
                    return Err(FrontendBuildError::InputRootNotDirectory {
                        path: display_path.to_owned(),
                    });
                }
            }
            let captured = Fingerprint::from_stat(&captured);
            let descriptor = openat(&*self.descriptor, name, DIRECTORY_FLAGS, Mode::empty())
                .map_err(|error| io_error(FrontendBuildOperation::Open, display_path, error))?;
            let file = File::from(descriptor);
            let opened = Fingerprint::from_metadata(&file.metadata().map_err(|source| {
                FrontendBuildError::Io {
                    operation: FrontendBuildOperation::Inspect,
                    path: display_path.to_owned(),
                    source,
                }
            })?);
            if (verify_parent && captured != opened)
                || (!verify_parent && !captured.same_object(opened))
            {
                return Err(FrontendBuildError::InputChanged {
                    path: display_path.to_owned(),
                });
            }
            if verify_parent {
                self.verify_unchanged()?;
            }
            Ok(Self {
                descriptor: Arc::new(file),
                fingerprint: opened,
                display_path: display_path.to_owned(),
            })
        }

        fn refreshed(&self) -> Result<Self, FrontendBuildError> {
            let fingerprint =
                Fingerprint::from_metadata(&self.descriptor.metadata().map_err(|source| {
                    FrontendBuildError::Io {
                        operation: FrontendBuildOperation::Inspect,
                        path: self.display_path.clone(),
                        source,
                    }
                })?);
            Ok(Self {
                descriptor: Arc::clone(&self.descriptor),
                fingerprint,
                display_path: self.display_path.clone(),
            })
        }

        pub(crate) fn entries(
            &self,
            already_seen: usize,
            limit: usize,
        ) -> Result<Vec<ConfinedEntry>, FrontendBuildError> {
            self.verify_unchanged()?;
            let mut directory = Dir::read_from(&*self.descriptor).map_err(|error| {
                io_error(FrontendBuildOperation::Enumerate, &self.display_path, error)
            })?;
            let mut entries = Vec::new();
            for result in &mut directory {
                let entry = result.map_err(|error| {
                    io_error(FrontendBuildOperation::Enumerate, &self.display_path, error)
                })?;
                if entry.file_name().to_bytes() == b"." || entry.file_name().to_bytes() == b".." {
                    continue;
                }
                let total = already_seen.saturating_add(entries.len()).saturating_add(1);
                if total > limit {
                    return Err(FrontendBuildError::InputEntryLimit {
                        entries: total,
                        limit,
                    });
                }
                let stat = statat(
                    &*self.descriptor,
                    entry.file_name(),
                    AtFlags::SYMLINK_NOFOLLOW,
                )
                .map_err(|error| {
                    io_error(
                        FrontendBuildOperation::Inspect,
                        &self
                            .display_path
                            .join(OsStr::from_bytes(entry.file_name().to_bytes())),
                        error,
                    )
                })?;
                let kind = match FileType::from_raw_mode(stat.st_mode) {
                    FileType::RegularFile => ConfinedEntryKind::File,
                    FileType::Directory => ConfinedEntryKind::Directory,
                    FileType::Symlink => ConfinedEntryKind::Symlink,
                    _ => ConfinedEntryKind::Special,
                };
                entries.push(ConfinedEntry {
                    name: entry.file_name().to_owned(),
                    fingerprint: Fingerprint::from_stat(&stat),
                    kind,
                });
            }
            entries.sort_by(|left, right| left.name.to_bytes().cmp(right.name.to_bytes()));
            self.verify_unchanged()?;
            Ok(entries)
        }

        pub(crate) fn open_entry_directory(
            &self,
            entry: &ConfinedEntry,
            display_path: &Path,
        ) -> Result<Self, FrontendBuildError> {
            if entry.kind != ConfinedEntryKind::Directory {
                return Err(FrontendBuildError::SpecialFile {
                    path: display_path.to_owned(),
                });
            }
            let directory = self.open_named_directory(entry.name(), display_path)?;
            if directory.fingerprint != entry.fingerprint {
                return Err(FrontendBuildError::InputChanged {
                    path: display_path.to_owned(),
                });
            }
            Ok(directory)
        }

        pub(crate) fn open_entry_file(
            &self,
            entry: &ConfinedEntry,
            display_path: &Path,
        ) -> Result<ConfinedInput, FrontendBuildError> {
            if entry.kind != ConfinedEntryKind::File {
                return Err(FrontendBuildError::SpecialFile {
                    path: display_path.to_owned(),
                });
            }
            let descriptor = openat(&*self.descriptor, entry.name(), INPUT_FLAGS, Mode::empty())
                .map_err(|error| io_error(FrontendBuildOperation::Open, display_path, error))?;
            let file = File::from(descriptor);
            let opened = Fingerprint::from_metadata(&file.metadata().map_err(|source| {
                FrontendBuildError::Io {
                    operation: FrontendBuildOperation::Inspect,
                    path: display_path.to_owned(),
                    source,
                }
            })?);
            if opened != entry.fingerprint {
                return Err(FrontendBuildError::InputChanged {
                    path: display_path.to_owned(),
                });
            }
            Ok(ConfinedInput {
                descriptor: file,
                parent: Arc::clone(&self.descriptor),
                parent_fingerprint: self.fingerprint,
                parent_display_path: self.display_path.clone(),
                leaf: entry.name.clone(),
                fingerprint: opened,
                display_path: display_path.to_owned(),
            })
        }

        pub(crate) fn verify_unchanged(&self) -> Result<(), FrontendBuildError> {
            let current =
                Fingerprint::from_metadata(&self.descriptor.metadata().map_err(|source| {
                    FrontendBuildError::Io {
                        operation: FrontendBuildOperation::Inspect,
                        path: self.display_path.clone(),
                        source,
                    }
                })?);
            if current != self.fingerprint {
                return Err(FrontendBuildError::InputChanged {
                    path: self.display_path.clone(),
                });
            }
            Ok(())
        }

        pub(crate) fn write_atomic_with_hook<Hook>(
            &self,
            leaf: &str,
            bytes: &[u8],
            before_rename: Hook,
        ) -> Result<(), FrontendBuildError>
        where
            Hook: FnOnce() -> Result<(), FrontendBuildError>,
        {
            let destination = self.display_path.join(leaf);
            let existing = self.validate_output_leaf(leaf, &destination)?;
            let (temporary_name, mut temporary, created) = self.create_temporary(leaf)?;
            let temporary_path = self
                .display_path
                .join(OsStr::from_bytes(temporary_name.to_bytes()));
            let result = (|| {
                temporary
                    .write_all(bytes)
                    .map_err(|source| FrontendBuildError::Io {
                        operation: FrontendBuildOperation::Write,
                        path: temporary_path.clone(),
                        source,
                    })?;
                temporary
                    .sync_all()
                    .map_err(|source| FrontendBuildError::Io {
                        operation: FrontendBuildOperation::Sync,
                        path: temporary_path.clone(),
                        source,
                    })?;
                let completed =
                    Fingerprint::from_metadata(&temporary.metadata().map_err(|source| {
                        FrontendBuildError::Io {
                            operation: FrontendBuildOperation::Inspect,
                            path: temporary_path.clone(),
                            source,
                        }
                    })?);
                if completed.device != created.device
                    || completed.inode != created.inode
                    || completed.mode != created.mode
                    || completed.links != 1
                    || completed.bytes != bytes.len() as i128
                {
                    return Err(FrontendBuildError::UnsafeOutputPath {
                        path: temporary_path.clone(),
                    });
                }
                before_rename()?;
                self.verify_temporary_name(&temporary_name, completed)?;
                self.revalidate_output_leaf(leaf, &destination, existing)?;
                renameat(&*self.descriptor, &temporary_name, &*self.descriptor, leaf).map_err(
                    |error| io_error(FrontendBuildOperation::Rename, &destination, error),
                )?;
                Ok(())
            })();
            if result.is_err() {
                self.cleanup_temporary(&temporary_name, created);
            }
            result
        }

        fn validate_output_leaf(
            &self,
            leaf: &str,
            display_path: &Path,
        ) -> Result<Option<Fingerprint>, FrontendBuildError> {
            match statat(&*self.descriptor, leaf, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => {
                    let fingerprint = Fingerprint::from_stat(&stat);
                    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
                        return Err(FrontendBuildError::UnsafeOutputPath {
                            path: display_path.to_owned(),
                        });
                    }
                    if fingerprint.links != 1 {
                        return Err(FrontendBuildError::OutputHardlink {
                            path: display_path.to_owned(),
                            links: fingerprint.links,
                        });
                    }
                    Ok(Some(fingerprint))
                }
                Err(Errno::NOENT) => Ok(None),
                Err(error) => Err(io_error(
                    FrontendBuildOperation::Inspect,
                    display_path,
                    error,
                )),
            }
        }

        fn revalidate_output_leaf(
            &self,
            leaf: &str,
            display_path: &Path,
            expected: Option<Fingerprint>,
        ) -> Result<(), FrontendBuildError> {
            let current = self.validate_output_leaf(leaf, display_path)?;
            if current != expected {
                return Err(FrontendBuildError::OutputChanged {
                    path: display_path.to_owned(),
                });
            }
            Ok(())
        }

        fn create_temporary(
            &self,
            leaf: &str,
        ) -> Result<(CString, File, Fingerprint), FrontendBuildError> {
            for _ in 0..TEMP_ATTEMPTS {
                let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let name = CString::new(format!(
                    ".maincopy-{leaf}-{}-{sequence}.tmp",
                    std::process::id()
                ))
                .map_err(|_| FrontendBuildError::UnsafeOutputPath {
                    path: self.display_path.join(leaf),
                })?;
                match openat(&*self.descriptor, &name, TEMP_FLAGS, FILE_MODE) {
                    Ok(descriptor) => {
                        let file = File::from(descriptor);
                        let fingerprint =
                            Fingerprint::from_metadata(&file.metadata().map_err(|source| {
                                FrontendBuildError::Io {
                                    operation: FrontendBuildOperation::Inspect,
                                    path: self
                                        .display_path
                                        .join(OsStr::from_bytes(name.to_bytes())),
                                    source,
                                }
                            })?);
                        if fingerprint.links != 1 {
                            self.cleanup_temporary(&name, fingerprint);
                            return Err(FrontendBuildError::UnsafeOutputPath {
                                path: self.display_path.join(OsStr::from_bytes(name.to_bytes())),
                            });
                        }
                        return Ok((name, file, fingerprint));
                    }
                    Err(Errno::EXIST) => continue,
                    Err(error) => {
                        return Err(io_error(
                            FrontendBuildOperation::Write,
                            &self.display_path.join(OsStr::from_bytes(name.to_bytes())),
                            error,
                        ));
                    }
                }
            }
            Err(FrontendBuildError::TemporaryOutputExhausted {
                path: self.display_path.join(leaf),
                attempts: TEMP_ATTEMPTS,
            })
        }

        fn verify_temporary_name(
            &self,
            name: &CStr,
            expected: Fingerprint,
        ) -> Result<(), FrontendBuildError> {
            let display_path = self.display_path.join(OsStr::from_bytes(name.to_bytes()));
            let stat = statat(&*self.descriptor, name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| io_error(FrontendBuildOperation::Inspect, &display_path, error))?;
            let current = Fingerprint::from_stat(&stat);
            if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || current != expected
            {
                return Err(FrontendBuildError::UnsafeOutputPath { path: display_path });
            }
            Ok(())
        }

        fn cleanup_temporary(&self, name: &CStr, expected: Fingerprint) {
            let Ok(stat) = statat(&*self.descriptor, name, AtFlags::SYMLINK_NOFOLLOW) else {
                return;
            };
            let current = Fingerprint::from_stat(&stat);
            if current.device == expected.device && current.inode == expected.inode {
                let _ = unlinkat(&*self.descriptor, name, AtFlags::empty());
            }
        }
    }

    pub(crate) struct ConfinedInput {
        descriptor: File,
        parent: Arc<File>,
        parent_fingerprint: Fingerprint,
        parent_display_path: PathBuf,
        leaf: CString,
        fingerprint: Fingerprint,
        display_path: PathBuf,
    }

    impl ConfinedInput {
        pub(crate) fn byte_length(&self) -> Result<u64, FrontendBuildError> {
            self.fingerprint.byte_length(&self.display_path)
        }

        pub(crate) fn read_verified(
            &mut self,
            limit: usize,
        ) -> Result<Vec<u8>, FrontendBuildError> {
            self.read_verified_with_hook(limit, || {})
        }

        pub(crate) fn read_verified_with_hook<Hook>(
            &mut self,
            limit: usize,
            after_first_read: Hook,
        ) -> Result<Vec<u8>, FrontendBuildError>
        where
            Hook: FnOnce(),
        {
            self.verify_unchanged()?;
            let capacity = usize::try_from(self.fingerprint.byte_length(&self.display_path)?)
                .unwrap_or(limit)
                .min(limit);
            let mut bytes = Vec::with_capacity(capacity);
            let mut buffer = [0_u8; READ_CHUNK_BYTES];
            let mut hook = Some(after_first_read);
            loop {
                let count = match self.descriptor.read(&mut buffer) {
                    Ok(count) => count,
                    Err(source) if source.kind() == io::ErrorKind::Interrupted => continue,
                    Err(source) => {
                        return Err(FrontendBuildError::Io {
                            operation: FrontendBuildOperation::Read,
                            path: self.display_path.clone(),
                            source,
                        });
                    }
                };
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(hook) = hook.take() {
                    hook();
                }
                if bytes.len() > limit {
                    return Ok(bytes);
                }
            }
            self.verify_unchanged()?;
            Ok(bytes)
        }

        pub(crate) fn verify_unchanged(&self) -> Result<(), FrontendBuildError> {
            self.verify_parent_fingerprint()?;
            self.verify_named_fingerprint()?;
            self.verify_descriptor_fingerprint()
        }

        fn verify_parent_fingerprint(&self) -> Result<(), FrontendBuildError> {
            let current =
                Fingerprint::from_metadata(&self.parent.metadata().map_err(|source| {
                    FrontendBuildError::Io {
                        operation: FrontendBuildOperation::Inspect,
                        path: self.parent_display_path.clone(),
                        source,
                    }
                })?);
            if current != self.parent_fingerprint {
                return Err(FrontendBuildError::InputChanged {
                    path: self.parent_display_path.clone(),
                });
            }
            Ok(())
        }

        fn verify_descriptor_fingerprint(&self) -> Result<(), FrontendBuildError> {
            let current =
                Fingerprint::from_metadata(&self.descriptor.metadata().map_err(|source| {
                    FrontendBuildError::Io {
                        operation: FrontendBuildOperation::Inspect,
                        path: self.display_path.clone(),
                        source,
                    }
                })?);
            if current != self.fingerprint {
                return Err(FrontendBuildError::InputChanged {
                    path: self.display_path.clone(),
                });
            }
            Ok(())
        }

        fn verify_named_fingerprint(&self) -> Result<(), FrontendBuildError> {
            let stat =
                statat(&*self.parent, &self.leaf, AtFlags::SYMLINK_NOFOLLOW).map_err(|error| {
                    io_error(FrontendBuildOperation::Inspect, &self.display_path, error)
                })?;
            if Fingerprint::from_stat(&stat) != self.fingerprint {
                return Err(FrontendBuildError::InputChanged {
                    path: self.display_path.clone(),
                });
            }
            Ok(())
        }
    }

    fn io_error(
        operation: FrontendBuildOperation,
        path: &Path,
        error: Errno,
    ) -> FrontendBuildError {
        FrontendBuildError::Io {
            operation,
            path: path.to_owned(),
            source: io::Error::from_raw_os_error(error.raw_os_error()),
        }
    }

    fn changed_or_io(path: &Path, error: Errno) -> FrontendBuildError {
        if matches!(error, Errno::NOENT | Errno::LOOP | Errno::NOTDIR) {
            FrontendBuildError::InputChanged {
                path: path.to_owned(),
            }
        } else {
            io_error(FrontendBuildOperation::Open, path, error)
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use supported::*;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[allow(dead_code)]
mod unsupported {
    use std::{ffi::OsStr, path::Path};

    use super::FrontendBuildError;

    #[derive(Clone)]
    pub(crate) struct ConfinedDirectory;
    #[derive(Clone)]
    pub(crate) struct ConfinedDirectoryIdentity;
    #[derive(Clone)]
    pub(crate) struct ConfinedFileIdentity;
    pub(crate) struct ConfinedEntry;
    pub(crate) struct ConfinedInput;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum ConfinedEntryKind {
        File,
        Directory,
        Symlink,
        Special,
    }

    impl ConfinedDirectory {
        pub(crate) fn identity(&self) -> ConfinedDirectoryIdentity {
            ConfinedDirectoryIdentity
        }

        pub(crate) fn open_absolute(_path: &Path) -> Result<Self, FrontendBuildError> {
            Err(FrontendBuildError::UnsupportedHost)
        }

        pub(crate) fn open_required_directory(
            &self,
            _name: &OsStr,
            _display_path: &Path,
        ) -> Result<Self, FrontendBuildError> {
            Err(FrontendBuildError::UnsupportedHost)
        }

        pub(crate) fn ensure_output_directory(
            &self,
            _name: &OsStr,
            _display_path: &Path,
        ) -> Result<Self, FrontendBuildError> {
            Err(FrontendBuildError::UnsupportedHost)
        }

        pub(crate) fn entries(
            &self,
            _already_seen: usize,
            _limit: usize,
        ) -> Result<Vec<ConfinedEntry>, FrontendBuildError> {
            Err(FrontendBuildError::UnsupportedHost)
        }

        pub(crate) fn open_entry_directory(
            &self,
            _entry: &ConfinedEntry,
            _display_path: &Path,
        ) -> Result<Self, FrontendBuildError> {
            Err(FrontendBuildError::UnsupportedHost)
        }

        pub(crate) fn open_entry_file(
            &self,
            _entry: &ConfinedEntry,
            _display_path: &Path,
        ) -> Result<ConfinedInput, FrontendBuildError> {
            Err(FrontendBuildError::UnsupportedHost)
        }

        pub(crate) fn verify_unchanged(&self) -> Result<(), FrontendBuildError> {
            Err(FrontendBuildError::UnsupportedHost)
        }

        pub(crate) fn write_atomic_with_hook<Hook>(
            &self,
            _leaf: &str,
            _bytes: &[u8],
            _before_rename: Hook,
        ) -> Result<(), FrontendBuildError>
        where
            Hook: FnOnce() -> Result<(), FrontendBuildError>,
        {
            Err(FrontendBuildError::UnsupportedHost)
        }
    }

    impl ConfinedDirectoryIdentity {
        pub(crate) fn child(
            &self,
            _entry: &ConfinedEntry,
            _display_path: &Path,
        ) -> Result<Self, FrontendBuildError> {
            Err(FrontendBuildError::UnsupportedHost)
        }

        pub(crate) fn file(
            &self,
            _entry: &ConfinedEntry,
            _display_path: &Path,
        ) -> Result<ConfinedFileIdentity, FrontendBuildError> {
            Err(FrontendBuildError::UnsupportedHost)
        }

        pub(crate) fn verify_unchanged(&self) -> Result<(), FrontendBuildError> {
            Err(FrontendBuildError::UnsupportedHost)
        }
    }

    impl ConfinedFileIdentity {
        pub(crate) fn verify_unchanged(&self) -> Result<(), FrontendBuildError> {
            Err(FrontendBuildError::UnsupportedHost)
        }
    }

    impl ConfinedEntry {
        pub(crate) fn name(&self) -> &OsStr {
            OsStr::new("")
        }

        pub(crate) const fn kind(&self) -> ConfinedEntryKind {
            ConfinedEntryKind::Special
        }
    }

    impl ConfinedInput {
        pub(crate) fn byte_length(&self) -> Result<u64, FrontendBuildError> {
            Err(FrontendBuildError::UnsupportedHost)
        }

        pub(crate) fn read_verified(
            &mut self,
            _limit: usize,
        ) -> Result<Vec<u8>, FrontendBuildError> {
            Err(FrontendBuildError::UnsupportedHost)
        }

        pub(crate) fn verify_unchanged(&self) -> Result<(), FrontendBuildError> {
            Err(FrontendBuildError::UnsupportedHost)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) use unsupported::*;
