#[cfg(unix)]
mod platform {
    use std::{
        fs, io,
        os::unix::{
            fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
            net::UnixStream,
        },
        path::{Path, PathBuf},
    };

    use tokio::net::UnixListener;
    use tokio_util::sync::CancellationToken;

    use crate::process_lock::prepare_private_directory;

    pub(crate) struct AdminSocket {
        listener: UnixListener,
        cleanup: SocketCleanup,
    }

    impl AdminSocket {
        pub(crate) fn bind(path: &Path) -> Result<Self, AdminSocketError> {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .ok_or(AdminSocketError::InvalidPath)?;
            prepare_private_directory(parent).map_err(AdminSocketError::ParentDirectory)?;
            remove_abandoned_socket(path)?;

            let listener = UnixListener::bind(path).map_err(AdminSocketError::Bind)?;
            let cleanup = SocketCleanup::capture(path).map_err(AdminSocketError::Bind)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .map_err(AdminSocketError::Permissions)?;
            validate_bound_socket(path).map_err(AdminSocketError::Permissions)?;
            Ok(Self { listener, cleanup })
        }

        pub(crate) async fn serve(self, cancellation: CancellationToken) -> Result<(), io::Error> {
            let Self { listener, cleanup } = self;
            let result = axum::serve(listener, super::super::admin_router())
                .with_graceful_shutdown(cancellation.cancelled_owned())
                .await;
            drop(cleanup);
            result
        }
    }

    #[derive(Debug, thiserror::Error)]
    pub(crate) enum AdminSocketError {
        #[error("the admin socket path has no parent directory")]
        InvalidPath,

        #[error("the admin socket parent directory is unavailable")]
        ParentDirectory(#[source] io::Error),

        #[error("the admin socket path contains a non-socket entry")]
        PathOccupied,

        #[error("another service owns the admin socket")]
        LiveSocket,

        #[error("the existing admin socket state is ambiguous")]
        StaleSocketProbe(#[source] io::Error),

        #[error("the admin socket could not be bound")]
        Bind(#[source] io::Error),

        #[error("the admin socket permissions are unsafe")]
        Permissions(#[source] io::Error),
    }

    impl AdminSocketError {
        pub(crate) const fn is_conflict(&self) -> bool {
            matches!(self, Self::LiveSocket)
        }
    }

    fn remove_abandoned_socket(path: &Path) -> Result<(), AdminSocketError> {
        let before = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(AdminSocketError::StaleSocketProbe(error)),
        };
        if !before.file_type().is_socket() {
            return Err(AdminSocketError::PathOccupied);
        }

        match UnixStream::connect(path) {
            Ok(_) => return Err(AdminSocketError::LiveSocket),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) => {}
            Err(error) => return Err(AdminSocketError::StaleSocketProbe(error)),
        }

        let after = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(AdminSocketError::StaleSocketProbe(error)),
        };
        if !after.file_type().is_socket()
            || before.dev() != after.dev()
            || before.ino() != after.ino()
        {
            return Err(AdminSocketError::PathOccupied);
        }
        fs::remove_file(path).map_err(AdminSocketError::StaleSocketProbe)
    }

    fn validate_bound_socket(path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_socket()
            && metadata.uid() == rustix::process::geteuid().as_raw()
            && metadata.permissions().mode() & 0o177 == 0
        {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "admin socket ownership or permissions are unsafe",
            ))
        }
    }

    struct SocketCleanup {
        path: PathBuf,
        device: u64,
        inode: u64,
    }

    impl SocketCleanup {
        fn capture(path: &Path) -> io::Result<Self> {
            let metadata = fs::symlink_metadata(path)?;
            if !metadata.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "bound admin path is not a socket",
                ));
            }
            Ok(Self {
                path: path.to_owned(),
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
    }

    impl Drop for SocketCleanup {
        fn drop(&mut self) {
            let Ok(metadata) = fs::symlink_metadata(&self.path) else {
                return;
            };
            if metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
            {
                let _ = fs::remove_file(&self.path);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn socket_path(root: &tempfile::TempDir) -> PathBuf {
            root.path().join("r").join("admin.sock")
        }

        #[tokio::test]
        async fn bind_sets_private_permissions_and_drop_removes_its_socket() {
            let root = tempfile::tempdir().unwrap();
            let path = socket_path(&root);
            let socket = AdminSocket::bind(&path).unwrap();
            let mode = fs::symlink_metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);

            drop(socket);
            assert!(!path.exists());
        }

        #[tokio::test]
        async fn regular_files_and_live_sockets_are_never_removed() {
            let root = tempfile::tempdir().unwrap();
            let path = socket_path(&root);
            prepare_private_directory(path.parent().unwrap()).unwrap();
            fs::write(&path, b"do not replace").unwrap();
            assert!(matches!(
                AdminSocket::bind(&path),
                Err(AdminSocketError::PathOccupied)
            ));
            assert_eq!(fs::read(&path).unwrap(), b"do not replace");

            fs::remove_file(&path).unwrap();
            let live = std::os::unix::net::UnixListener::bind(&path).unwrap();
            assert!(matches!(
                AdminSocket::bind(&path),
                Err(AdminSocketError::LiveSocket)
            ));
            drop(live);
        }

        #[tokio::test]
        async fn abandoned_socket_is_replaced_only_after_connection_refusal() {
            let root = tempfile::tempdir().unwrap();
            let path = socket_path(&root);
            prepare_private_directory(path.parent().unwrap()).unwrap();
            let abandoned = std::os::unix::net::UnixListener::bind(&path).unwrap();
            drop(abandoned);

            let socket = AdminSocket::bind(&path).unwrap();
            assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket());
            drop(socket);
        }

        #[tokio::test]
        async fn cleanup_does_not_remove_a_replacement() {
            let root = tempfile::tempdir().unwrap();
            let path = socket_path(&root);
            let socket = AdminSocket::bind(&path).unwrap();
            fs::remove_file(&path).unwrap();
            fs::write(&path, b"replacement").unwrap();

            drop(socket);
            assert_eq!(fs::read(&path).unwrap(), b"replacement");
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::{
        ffi::{OsString, c_void},
        io, mem,
        path::Path,
        ptr,
        time::Duration,
    };

    use maincopy_shared::is_valid_windows_admin_pipe_name;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tokio_util::sync::CancellationToken;
    use windows_sys::Win32::{
        Foundation::{ERROR_ACCESS_DENIED, LocalFree},
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            SECURITY_ATTRIBUTES,
        },
    };

    const ACCEPT_RETRY_DELAY: Duration = Duration::from_secs(1);
    const OWNER_AND_SYSTEM_DACL: windows_sys::core::PCWSTR =
        windows_sys::core::w!("D:P(A;;GA;;;OW)(A;;GA;;;SY)");

    pub(crate) struct AdminSocket {
        listener: NamedPipeListener,
    }

    impl AdminSocket {
        pub(crate) fn bind(path: &Path) -> Result<Self, AdminSocketError> {
            let Some(name) = path.to_str() else {
                return Err(AdminSocketError::InvalidPath);
            };
            if !is_valid_windows_admin_pipe_name(name) {
                return Err(AdminSocketError::InvalidPath);
            }

            let name = path.as_os_str().to_owned();
            let next = match create_pipe(&name, true) {
                Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                    return Err(AdminSocketError::LiveSocket);
                }
                Err(error) => return Err(AdminSocketError::Bind(error)),
                Ok(next) => next,
            };
            Ok(Self {
                listener: NamedPipeListener { name, next },
            })
        }

        pub(crate) async fn serve(self, cancellation: CancellationToken) -> Result<(), io::Error> {
            axum::serve(self.listener, super::super::admin_router())
                .with_graceful_shutdown(cancellation.cancelled_owned())
                .await
        }
    }

    #[derive(Debug, thiserror::Error)]
    pub(crate) enum AdminSocketError {
        #[error("the admin named-pipe name is not a canonical local pipe name")]
        InvalidPath,

        #[error("another service owns the admin named pipe")]
        LiveSocket,

        #[error("the admin named pipe could not be bound")]
        Bind(#[source] io::Error),
    }

    impl AdminSocketError {
        pub(crate) const fn is_conflict(&self) -> bool {
            matches!(self, Self::LiveSocket)
        }
    }

    struct NamedPipeListener {
        name: OsString,
        next: NamedPipeServer,
    }

    impl axum::serve::Listener for NamedPipeListener {
        type Io = NamedPipeServer;
        type Addr = ();

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            loop {
                if let Err(error) = self.next.connect().await {
                    tracing::error!(
                        error = %error,
                        "admin named-pipe connection failed; retrying"
                    );
                    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    continue;
                }

                let replacement = loop {
                    match create_pipe(&self.name, false) {
                        Ok(pipe) => break pipe,
                        Err(error) => {
                            tracing::error!(
                                error = %error,
                                "admin named-pipe instance creation failed; retrying"
                            );
                            tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                        }
                    }
                };

                // Keep one unconnected instance available before Axum starts
                // serving the connection. Otherwise clients can observe a
                // transient missing-pipe error between accepts.
                return (mem::replace(&mut self.next, replacement), ());
            }
        }

        fn local_addr(&self) -> io::Result<Self::Addr> {
            Ok(())
        }
    }

    fn create_pipe(name: &OsString, first_instance: bool) -> io::Result<NamedPipeServer> {
        let descriptor = SecurityDescriptor::owner_and_system()?;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0.as_ptr(),
            bInheritHandle: 0,
        };
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first_instance)
            .reject_remote_clients(true);

        // SAFETY: `attributes` and its security descriptor remain valid for
        // the complete synchronous CreateNamedPipeW call. The returned handle
        // does not retain either pointer, and handle inheritance is disabled.
        unsafe {
            options.create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
            )
        }
    }

    struct SecurityDescriptor(ptr::NonNull<c_void>);

    impl SecurityDescriptor {
        fn owner_and_system() -> io::Result<Self> {
            let mut descriptor = ptr::null_mut();
            // SAFETY: `OWNER_AND_SYSTEM_DACL` is a static, NUL-terminated SDDL
            // string. Windows initializes `descriptor` on success and the
            // resulting allocation is owned by LocalFree.
            let converted = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    OWNER_AND_SYSTEM_DACL,
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            };
            if converted == 0 {
                return Err(io::Error::last_os_error());
            }
            ptr::NonNull::new(descriptor)
                .map(Self)
                .ok_or_else(|| io::Error::other("Windows returned a null security descriptor"))
        }
    }

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: this pointer was returned by
            // ConvertStringSecurityDescriptorToSecurityDescriptorW and is
            // released exactly once here.
            let _ = unsafe { LocalFree(self.0.as_ptr()) };
        }
    }

    #[cfg(test)]
    mod tests {
        use tokio::net::windows::named_pipe::ClientOptions;

        use super::*;

        fn unique_pipe_name() -> OsString {
            format!(
                r"\\.\pipe\maincopy-test-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            )
            .into()
        }

        #[tokio::test]
        async fn first_instance_conflicts_and_release_has_no_stale_state() {
            let name = unique_pipe_name();
            let path = Path::new(&name);
            let first = AdminSocket::bind(path).unwrap();
            assert!(matches!(
                AdminSocket::bind(path),
                Err(AdminSocketError::LiveSocket)
            ));

            drop(first);
            AdminSocket::bind(path).unwrap();
        }

        #[tokio::test]
        async fn same_owner_can_connect_to_the_private_pipe() {
            let name = unique_pipe_name();
            let mut listener = NamedPipeListener {
                next: create_pipe(&name, true).unwrap(),
                name: name.clone(),
            };
            let client = ClientOptions::new().open(&name).unwrap();

            let (server, ()) = axum::serve::Listener::accept(&mut listener).await;
            drop((client, server));
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::{io, path::Path};

    use tokio_util::sync::CancellationToken;

    pub(crate) struct AdminSocket;

    impl AdminSocket {
        pub(crate) fn bind(_path: &Path) -> Result<Self, AdminSocketError> {
            Err(AdminSocketError)
        }

        pub(crate) async fn serve(self, _cancellation: CancellationToken) -> Result<(), io::Error> {
            unreachable!("unsupported admin sockets cannot be constructed")
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("this platform does not support the private admin socket")]
    pub(crate) struct AdminSocketError;

    impl AdminSocketError {
        pub(crate) const fn is_conflict(&self) -> bool {
            false
        }
    }
}

pub(crate) use platform::AdminSocket;
