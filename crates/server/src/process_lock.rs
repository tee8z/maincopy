use std::{
    fs::{DirBuilder, File, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
};

const PROCESS_LOCK_NAME: &str = "maincopy.lock";

/// Exclusive ownership of one Maincopy runtime directory.
///
/// The file remains on disk. The operating system releases the lock when this
/// value drops, including after an abnormal process exit.
pub(crate) struct ProcessLock {
    file: File,
}

impl ProcessLock {
    pub(crate) fn acquire(runtime_root: &Path) -> Result<Self, ProcessLockError> {
        prepare_private_directory(runtime_root).map_err(ProcessLockError::RuntimeDirectory)?;
        let path = runtime_root.join(PROCESS_LOCK_NAME);
        let file = open_private_file(&path).map_err(ProcessLockError::LockFile)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { file }),
            Err(TryLockError::WouldBlock) => Err(ProcessLockError::AlreadyRunning),
            Err(TryLockError::Error(source)) => Err(ProcessLockError::LockFile(source)),
        }
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        // `fork` briefly duplicates every descriptor before `exec` applies
        // close-on-exec. Unlock explicitly so such a child cannot extend
        // process ownership beyond this value's lifetime.
        if let Err(error) = self.file.unlock() {
            tracing::error!(%error, "process lock release failed");
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProcessLockError {
    #[error("another Maincopy server owns the process lock")]
    AlreadyRunning,

    #[error("the private runtime directory is unavailable")]
    RuntimeDirectory(#[source] io::Error),

    #[error("the process lock file is unavailable")]
    LockFile(#[source] io::Error),
}

pub(crate) fn prepare_private_directory(path: &Path) -> io::Result<()> {
    reject_symlink_components(path)?;
    create_private_directory(path)?;
    validate_private_directory(path)
}

/// Rejects symbolic links in every existing component of one private path.
pub(crate) fn reject_symlink_components(path: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "private paths cannot contain symbolic links",
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

    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).create(path)
}

#[cfg(unix)]
fn validate_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private runtime directory ownership or permissions are unsafe",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_directory(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private runtime path is not a directory",
        ))
    }
}

#[cfg(unix)]
pub(crate) fn open_private_file(path: &Path) -> io::Result<File> {
    open_private_file_with_creation(path, true)
}

#[cfg(unix)]
/// Opens an existing private regular file without creating a missing target.
pub(crate) fn open_existing_private_file(path: &Path) -> io::Result<File> {
    open_private_file_with_creation(path, false)
}

#[cfg(unix)]
fn open_private_file_with_creation(path: &Path, create: bool) -> io::Result<File> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private file path is not a regular file",
        ));
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private file ownership or permissions are unsafe",
        ));
    }
    Ok(file)
}

#[cfg(not(unix))]
pub(crate) fn open_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

#[cfg(not(unix))]
/// Opens an existing private regular file without creating a missing target.
pub(crate) fn open_existing_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_process_lock_is_rejected_until_the_owner_drops() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("run");
        let first = ProcessLock::acquire(&runtime).unwrap();

        assert!(matches!(
            ProcessLock::acquire(&runtime),
            Err(ProcessLockError::AlreadyRunning)
        ));

        drop(first);
        ProcessLock::acquire(&runtime).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn private_runtime_directory_rejects_symlinks_and_open_permissions() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let linked = root.path().join("linked");
        symlink(&real, &linked).unwrap();
        assert!(ProcessLock::acquire(&linked).is_err());

        let open = root.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ProcessLock::acquire(&open).is_err());
    }
}
