use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::{NonZeroU64, NonZeroUsize},
    path::{Path, PathBuf},
    time::Duration,
};

use maincopy_shared::source::SshCredentialName;
use markdown_compiler::{
    ContentDepthLimit, ContentEntryLimit, ContentFileByteLimit, ContentPathByteLimit,
    ContentTreeByteLimit, ContentTreeLimits,
};
use serde::Deserialize;

use crate::admin::origin::{AdminBind, AdminOrigin};

use super::diagnostic::{
    ConfigurationDiagnostic, ConfigurationErrors, ConfigurationValidationCode, DiagnosticCollector,
    single_error, toml_location,
};
use super::secret::{SecretFileReference, SensitivePath};

const MAX_HOST_DOCUMENT_BYTES: u64 = 1024 * 1024;
const DEFAULT_CONTENT_ROOT: &str = "content";
const DEFAULT_STATE_ROOT: &str = "state";
const DEFAULT_RUNTIME_ROOT: &str = "run";
const DEFAULT_DATABASE_FILE_NAME: &str = "maincopy.db";
const DEFAULT_PUBLIC_PORT: u16 = 3000;
const DEFAULT_ADMIN_PORT: u16 = 3001;
const DEFAULT_ADMIN_ORIGIN: &str = "https://admin.localhost";
const DEFAULT_BUSY_TIMEOUT_MILLISECONDS: u64 = 5_000;
const DEFAULT_WRITER_QUEUE_CAPACITY: usize = 128;
const DEFAULT_READ_POOL_SIZE: usize = 4;
const MAX_DATABASE_BUSY_TIMEOUT_MILLISECONDS: u64 = 300_000;
const MAX_DATABASE_WRITER_QUEUE_CAPACITY: usize = 65_536;
const MAX_DATABASE_READ_POOL_SIZE: usize = 256;
const DEFAULT_GIT_FETCH_TIMEOUT_SECONDS: u64 = 120;
const DEFAULT_GIT_COMMAND_OUTPUT_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_GIT_MIRROR_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_GIT_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const DEFAULT_GIT_ADDRESS_SPACE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_GIT_CPU_SECONDS: u64 = 120;
const DEFAULT_GIT_OPEN_FILES: u64 = 256;
const MAX_GIT_FETCH_TIMEOUT_SECONDS: u64 = 3_600;
const MAX_GIT_COMMAND_OUTPUT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_GIT_MIRROR_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_GIT_FILE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_GIT_ADDRESS_SPACE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_GIT_CPU_SECONDS: u64 = 3_600;
const MAX_GIT_OPEN_FILES: u64 = 4_096;

macro_rules! bounded_usize_setting {
    ($name:ident, $minimum:expr, $maximum:expr) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $name(NonZeroUsize);

        impl $name {
            pub fn new(value: usize) -> Option<Self> {
                NonZeroUsize::new(value)
                    .filter(|value| ($minimum..=$maximum).contains(&value.get()))
                    .map(Self)
            }

            pub const fn get(self) -> usize {
                self.0.get()
            }
        }
    };
}

macro_rules! bounded_duration_setting {
    ($name:ident, $constructor:ident, $duration_constructor:ident, $maximum:expr) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $name(Duration);

        impl $name {
            pub fn $constructor(value: u64) -> Option<Self> {
                (value > 0 && value <= $maximum)
                    .then_some(Self(Duration::$duration_constructor(value)))
            }

            pub const fn get(self) -> Duration {
                self.0
            }
        }
    };
}

bounded_duration_setting!(
    DatabaseBusyTimeout,
    from_milliseconds,
    from_millis,
    MAX_DATABASE_BUSY_TIMEOUT_MILLISECONDS
);
bounded_usize_setting!(
    DatabaseWriterQueueCapacity,
    1,
    MAX_DATABASE_WRITER_QUEUE_CAPACITY
);
bounded_usize_setting!(DatabaseReadPoolSize, 1, MAX_DATABASE_READ_POOL_SIZE);

macro_rules! bounded_u64_setting {
    ($name:ident, $maximum:expr) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $name(NonZeroU64);

        impl $name {
            pub fn new(value: u64) -> Option<Self> {
                NonZeroU64::new(value)
                    .filter(|value| value.get() <= $maximum)
                    .map(Self)
            }

            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }
    };
}

bounded_duration_setting!(
    GitFetchTimeout,
    from_seconds,
    from_secs,
    MAX_GIT_FETCH_TIMEOUT_SECONDS
);
bounded_u64_setting!(GitCommandOutputByteLimit, MAX_GIT_COMMAND_OUTPUT_BYTES);
bounded_u64_setting!(GitMirrorByteLimit, MAX_GIT_MIRROR_BYTES);
bounded_u64_setting!(GitFileByteLimit, MAX_GIT_FILE_BYTES);
bounded_u64_setting!(GitAddressSpaceByteLimit, MAX_GIT_ADDRESS_SPACE_BYTES);
bounded_u64_setting!(GitCpuSecondLimit, MAX_GIT_CPU_SECONDS);
bounded_u64_setting!(GitOpenFileLimit, MAX_GIT_OPEN_FILES);

/// Selects the one host-owned content source mechanism.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum SourceMode {
    #[default]
    ExternalCheckout,
    ManagedGit,
}

/// Host paths for one SSH identity. Both paths stay redacted under `Debug`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshCredentialReference {
    pub private_key: SecretFileReference,
    pub known_hosts: SecretFileReference,
}

/// Process and storage ceilings applied to every managed Git phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitProcessLimits {
    pub wall_time: GitFetchTimeout,
    pub command_output_bytes: GitCommandOutputByteLimit,
    pub mirror_bytes: GitMirrorByteLimit,
    pub file_bytes: GitFileByteLimit,
    pub address_space_bytes: GitAddressSpaceByteLimit,
    pub cpu_seconds: GitCpuSecondLimit,
    pub open_files: GitOpenFileLimit,
}

impl Default for GitProcessLimits {
    fn default() -> Self {
        Self {
            wall_time: GitFetchTimeout::from_seconds(DEFAULT_GIT_FETCH_TIMEOUT_SECONDS)
                .expect("the built-in Git timeout is valid"),
            command_output_bytes: GitCommandOutputByteLimit::new(DEFAULT_GIT_COMMAND_OUTPUT_BYTES)
                .expect("the built-in Git output limit is valid"),
            mirror_bytes: GitMirrorByteLimit::new(DEFAULT_GIT_MIRROR_BYTES)
                .expect("the built-in Git mirror limit is valid"),
            file_bytes: GitFileByteLimit::new(DEFAULT_GIT_FILE_BYTES)
                .expect("the built-in Git file limit is valid"),
            address_space_bytes: GitAddressSpaceByteLimit::new(DEFAULT_GIT_ADDRESS_SPACE_BYTES)
                .expect("the built-in Git address-space limit is valid"),
            cpu_seconds: GitCpuSecondLimit::new(DEFAULT_GIT_CPU_SECONDS)
                .expect("the built-in Git CPU limit is valid"),
            open_files: GitOpenFileLimit::new(DEFAULT_GIT_OPEN_FILES)
                .expect("the built-in Git open-file limit is valid"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManagedGitHostConfiguration {
    mirror_root: SensitivePath,
    credentials: BTreeMap<SshCredentialName, SshCredentialReference>,
    limits: GitProcessLimits,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SourceConfiguration {
    ExternalCheckout,
    ManagedGit(ManagedGitHostConfiguration),
}

/// Borrowed host-owned settings for the active source mechanism.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceConfigurationView<'configuration> {
    ExternalCheckout,
    ManagedGit {
        mirror_root: &'configuration SensitivePath,
        credentials: &'configuration BTreeMap<SshCredentialName, SshCredentialReference>,
        limits: GitProcessLimits,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DatabaseConfiguration {
    path: PathBuf,
    busy_timeout: DatabaseBusyTimeout,
    writer_queue_capacity: DatabaseWriterQueueCapacity,
    read_pool_size: DatabaseReadPoolSize,
}

/// Read-only database settings borrowed from validated host configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseConfigurationView<'configuration> {
    pub path: &'configuration Path,
    pub busy_timeout: DatabaseBusyTimeout,
    pub writer_queue_capacity: DatabaseWriterQueueCapacity,
    pub read_pool_size: DatabaseReadPoolSize,
}

/// Effective host-owned runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostConfiguration {
    content_root: PathBuf,
    state_root: PathBuf,
    runtime_root: PathBuf,
    content_limits: ContentTreeLimits,
    public_bind: SocketAddr,
    admin_bind: AdminBind,
    admin_origin: AdminOrigin,
    database: DatabaseConfiguration,
    source: SourceConfiguration,
}

/// Read-only settings borrowed from validated host configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostConfigurationView<'configuration> {
    pub content_root: &'configuration Path,
    pub state_root: &'configuration Path,
    pub runtime_root: &'configuration Path,
    pub content_limits: ContentTreeLimits,
    pub public_bind: SocketAddr,
    pub admin_bind: AdminBind,
    pub admin_origin: &'configuration AdminOrigin,
    pub database: DatabaseConfigurationView<'configuration>,
    pub source: SourceConfigurationView<'configuration>,
}

impl HostConfiguration {
    pub fn view(&self) -> HostConfigurationView<'_> {
        HostConfigurationView {
            content_root: &self.content_root,
            state_root: &self.state_root,
            runtime_root: &self.runtime_root,
            content_limits: self.content_limits,
            public_bind: self.public_bind,
            admin_bind: self.admin_bind,
            admin_origin: &self.admin_origin,
            database: DatabaseConfigurationView {
                path: &self.database.path,
                busy_timeout: self.database.busy_timeout,
                writer_queue_capacity: self.database.writer_queue_capacity,
                read_pool_size: self.database.read_pool_size,
            },
            source: match &self.source {
                SourceConfiguration::ExternalCheckout => SourceConfigurationView::ExternalCheckout,
                SourceConfiguration::ManagedGit(managed) => SourceConfigurationView::ManagedGit {
                    mirror_root: &managed.mirror_root,
                    credentials: &managed.credentials,
                    limits: managed.limits,
                },
            },
        }
    }
}

/// Loads one host file using an injected process working directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostConfigurationLoader {
    working_directory: PathBuf,
}

impl HostConfigurationLoader {
    pub fn from_process_working_directory() -> Result<Self, ConfigurationErrors> {
        let working_directory = std::env::current_dir().map_err(|_| {
            single_error(host_diagnostic(
                "$working_directory",
                ConfigurationValidationCode::WorkingDirectoryUnavailable,
                "process working directory is unavailable",
            ))
        })?;
        Self::new(working_directory)
    }

    fn new(working_directory: PathBuf) -> Result<Self, ConfigurationErrors> {
        if !working_directory.is_absolute() {
            return Err(single_error(host_diagnostic(
                "$working_directory",
                ConfigurationValidationCode::WorkingDirectoryUnavailable,
                "process working directory must be an absolute path",
            )));
        }
        Ok(Self { working_directory })
    }

    pub fn load(&self, config_path: &Path) -> Result<HostConfiguration, ConfigurationErrors> {
        let config_path = resolve_path(&self.working_directory, config_path).ok_or_else(|| {
            single_error(host_diagnostic(
                "$config_file",
                ConfigurationValidationCode::PathInvalid,
                "host configuration path must not be empty",
            ))
        })?;
        let file_base = config_path
            .parent()
            .unwrap_or(self.working_directory.as_path());
        let source = read_host_source(&config_path)?;
        let candidate = toml::from_str::<HostCandidate>(&source).map_err(|error| {
            let mut diagnostic = host_diagnostic(
                "$document",
                ConfigurationValidationCode::HostTomlInvalid,
                "host TOML does not match the canonical schema",
            );
            if let Some((line, column)) = toml_location(&source, &error) {
                diagnostic = diagnostic.at(line, column);
            }
            single_error(diagnostic)
        })?;
        finalize_host(candidate, &self.working_directory, file_base)
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HostCandidate {
    paths: PathCandidate,
    content: ContentCandidate,
    public: PublicCandidate,
    admin: AdminCandidate,
    database: DatabaseCandidate,
    source: SourceCandidate,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ContentCandidate {
    publication_file_bytes: Option<u64>,
    post_file_bytes: Option<u64>,
    asset_file_bytes: Option<u64>,
    total_tree_bytes: Option<u64>,
    entries: Option<u64>,
    depth: Option<u64>,
    path_bytes: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PathCandidate {
    content_root: Option<PathBuf>,
    state_root: Option<PathBuf>,
    runtime_root: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PublicCandidate {
    bind: Option<SocketAddr>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AdminCandidate {
    bind: Option<SocketAddr>,
    origin: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DatabaseCandidate {
    path: Option<PathBuf>,
    busy_timeout_ms: Option<u64>,
    writer_queue_capacity: Option<u64>,
    read_pool_size: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SourceCandidate {
    mode: SourceMode,
    mirror_root: Option<PathBuf>,
    fetch_timeout_seconds: Option<u64>,
    command_output_bytes: Option<u64>,
    mirror_bytes: Option<u64>,
    file_bytes: Option<u64>,
    address_space_bytes: Option<u64>,
    cpu_seconds: Option<u64>,
    open_files: Option<u64>,
    ssh_credentials: BTreeMap<String, SshCredentialCandidate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SshCredentialCandidate {
    private_key_file: PathBuf,
    known_hosts_file: PathBuf,
}

fn read_host_source(path: &Path) -> Result<String, ConfigurationErrors> {
    let mut file = File::open(path).map_err(|_| {
        single_error(host_diagnostic(
            "$document",
            ConfigurationValidationCode::HostFileUnreadable,
            "maincopy.toml could not be opened",
        ))
    })?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_HOST_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            single_error(host_diagnostic(
                "$document",
                ConfigurationValidationCode::HostFileUnreadable,
                "maincopy.toml could not be read",
            ))
        })?;
    if bytes.len() as u64 > MAX_HOST_DOCUMENT_BYTES {
        return Err(single_error(host_diagnostic(
            "$document",
            ConfigurationValidationCode::HostDocumentTooLarge,
            "maincopy.toml exceeds the 1 MiB source limit",
        )));
    }
    String::from_utf8(bytes).map_err(|_| {
        single_error(host_diagnostic(
            "$document",
            ConfigurationValidationCode::HostTextInvalidUtf8,
            "maincopy.toml must contain UTF-8 text",
        ))
    })
}

fn finalize_host(
    candidate: HostCandidate,
    working_directory: &Path,
    file_base: &Path,
) -> Result<HostConfiguration, ConfigurationErrors> {
    let mut diagnostics = DiagnosticCollector::default();
    let content_limits = validate_content_limits(candidate.content, &mut diagnostics);

    let content_root = select_path(
        candidate.paths.content_root,
        working_directory,
        file_base,
        Path::new(DEFAULT_CONTENT_ROOT),
        "paths.content_root",
        &mut diagnostics,
    );
    let state_root = select_path(
        candidate.paths.state_root,
        working_directory,
        file_base,
        Path::new(DEFAULT_STATE_ROOT),
        "paths.state_root",
        &mut diagnostics,
    );
    let runtime_root = select_path(
        candidate.paths.runtime_root,
        working_directory,
        file_base,
        Path::new(DEFAULT_RUNTIME_ROOT),
        "paths.runtime_root",
        &mut diagnostics,
    );

    let database_path = match candidate.database.path {
        Some(path) => validate_resolved_path(
            resolve_path(file_base, &path),
            "database.path",
            &mut diagnostics,
        ),
        None => state_root
            .as_ref()
            .map(|root| root.join(DEFAULT_DATABASE_FILE_NAME)),
    };
    let public_bind = candidate.public.bind.unwrap_or_else(default_public_bind);
    let admin_bind = validate_admin_bind(
        candidate.admin.bind.unwrap_or_else(default_admin_bind),
        &mut diagnostics,
    );
    let admin_origin = validate_admin_origin(
        candidate
            .admin
            .origin
            .unwrap_or_else(|| DEFAULT_ADMIN_ORIGIN.to_owned()),
        &mut diagnostics,
    );

    let busy_timeout = validate_duration::<DatabaseBusyTimeout>(
        candidate
            .database
            .busy_timeout_ms
            .unwrap_or(DEFAULT_BUSY_TIMEOUT_MILLISECONDS),
        "database.busy_timeout_ms",
        DatabaseBusyTimeout::from_milliseconds,
        &mut diagnostics,
    );
    let writer_queue_capacity = validate_usize::<DatabaseWriterQueueCapacity>(
        candidate
            .database
            .writer_queue_capacity
            .unwrap_or(DEFAULT_WRITER_QUEUE_CAPACITY as u64),
        "database.writer_queue_capacity",
        ConfigurationValidationCode::LimitOutOfRange,
        DatabaseWriterQueueCapacity::new,
        &mut diagnostics,
    );
    let read_pool_size = validate_usize::<DatabaseReadPoolSize>(
        candidate
            .database
            .read_pool_size
            .unwrap_or(DEFAULT_READ_POOL_SIZE as u64),
        "database.read_pool_size",
        ConfigurationValidationCode::LimitOutOfRange,
        DatabaseReadPoolSize::new,
        &mut diagnostics,
    );
    let source = validate_source_configuration(
        candidate.source,
        file_base,
        state_root.as_deref(),
        database_path.as_deref(),
        content_root.as_deref(),
        runtime_root.as_deref(),
        &mut diagnostics,
    );
    diagnostics.into_result()?;
    match (
        content_root,
        state_root,
        runtime_root,
        database_path,
        busy_timeout,
        writer_queue_capacity,
        read_pool_size,
        content_limits,
        admin_bind,
        admin_origin,
        source,
    ) {
        (
            Some(content_root),
            Some(state_root),
            Some(runtime_root),
            Some(database_path),
            Some(busy_timeout),
            Some(writer_queue_capacity),
            Some(read_pool_size),
            Some(content),
            Some(admin_bind),
            Some(admin_origin),
            Some(source),
        ) => Ok(HostConfiguration {
            content_root,
            state_root,
            runtime_root,
            content_limits: content,
            public_bind,
            admin_bind,
            admin_origin,
            database: DatabaseConfiguration {
                path: database_path,
                busy_timeout,
                writer_queue_capacity,
                read_pool_size,
            },
            source,
        }),
        _ => Err(single_error(host_diagnostic(
            "$document",
            ConfigurationValidationCode::HostTomlInvalid,
            "effective host settings could not be constructed",
        ))),
    }
}

fn validate_source_configuration(
    candidate: SourceCandidate,
    file_base: &Path,
    state_root: Option<&Path>,
    database_path: Option<&Path>,
    content_root: Option<&Path>,
    runtime_root: Option<&Path>,
    diagnostics: &mut DiagnosticCollector,
) -> Option<SourceConfiguration> {
    let has_managed_settings = candidate.mirror_root.is_some()
        || candidate.fetch_timeout_seconds.is_some()
        || candidate.command_output_bytes.is_some()
        || candidate.mirror_bytes.is_some()
        || candidate.file_bytes.is_some()
        || candidate.address_space_bytes.is_some()
        || candidate.cpu_seconds.is_some()
        || candidate.open_files.is_some()
        || !candidate.ssh_credentials.is_empty();
    let limits = validate_git_process_limits(&candidate, diagnostics);
    let credentials = validate_ssh_credentials(candidate.ssh_credentials, file_base, diagnostics);
    match candidate.mode {
        SourceMode::ExternalCheckout => {
            if has_managed_settings {
                diagnostics.push(host_diagnostic(
                    "source",
                    ConfigurationValidationCode::SourceModeConflict,
                    "managed Git settings are available only in managed_git mode",
                ));
            }
            limits.map(|_| SourceConfiguration::ExternalCheckout)
        }
        SourceMode::ManagedGit => {
            let mirror_root = candidate
                .mirror_root
                .and_then(|path| resolve_path(file_base, &path))
                .and_then(|path| {
                    validate_managed_mirror_root(
                        path,
                        state_root,
                        database_path,
                        content_root,
                        runtime_root,
                        diagnostics,
                    )
                });
            if credentials.is_empty() {
                diagnostics.push(host_diagnostic(
                    "source.ssh_credentials",
                    ConfigurationValidationCode::SecretReferenceInvalid,
                    "managed_git mode requires at least one named SSH credential",
                ));
            }
            match (mirror_root, limits) {
                (Some(mirror_root), Some(limits)) if !credentials.is_empty() => Some(
                    SourceConfiguration::ManagedGit(ManagedGitHostConfiguration {
                        mirror_root: SensitivePath::new(mirror_root)
                            .expect("a validated mirror path is nonempty"),
                        credentials,
                        limits,
                    }),
                ),
                _ => None,
            }
        }
    }
}

fn validate_managed_mirror_root(
    mirror_root: PathBuf,
    state_root: Option<&Path>,
    database_path: Option<&Path>,
    content_root: Option<&Path>,
    runtime_root: Option<&Path>,
    diagnostics: &mut DiagnosticCollector,
) -> Option<PathBuf> {
    let within_state = state_root
        .and_then(|state_root| mirror_root.strip_prefix(state_root).ok())
        .is_some_and(|relative| {
            let mut components = relative.components();
            matches!(components.next(), Some(std::path::Component::Normal(_)))
                && components.next().is_none()
        });
    let overlaps_protected_path = [database_path, content_root, runtime_root]
        .into_iter()
        .flatten()
        .any(|path| path.starts_with(&mirror_root) || mirror_root.starts_with(path));
    if mirror_root.as_os_str().is_empty() || !within_state || overlaps_protected_path {
        diagnostics.push(host_diagnostic(
            "source.mirror_root",
            ConfigurationValidationCode::PathInvalid,
            "managed Git mirror root must be one dedicated direct child of paths.state_root",
        ));
        None
    } else {
        Some(mirror_root)
    }
}

fn validate_ssh_credentials(
    candidates: BTreeMap<String, SshCredentialCandidate>,
    file_base: &Path,
    diagnostics: &mut DiagnosticCollector,
) -> BTreeMap<SshCredentialName, SshCredentialReference> {
    let mut credentials = BTreeMap::new();
    for (raw_name, candidate) in candidates {
        let Ok(name) = SshCredentialName::parse(&raw_name) else {
            diagnostics.push(host_diagnostic(
                "source.ssh_credentials",
                ConfigurationValidationCode::SecretReferenceInvalid,
                "SSH credential names must use bounded lowercase ASCII identifiers",
            ));
            continue;
        };
        let private_key = resolve_secret_reference(
            file_base,
            candidate.private_key_file,
            "source.ssh_credentials.private_key_file",
            diagnostics,
        );
        let known_hosts = resolve_secret_reference(
            file_base,
            candidate.known_hosts_file,
            "source.ssh_credentials.known_hosts_file",
            diagnostics,
        );
        if let (Some(private_key), Some(known_hosts)) = (private_key, known_hosts) {
            credentials.insert(
                name,
                SshCredentialReference {
                    private_key,
                    known_hosts,
                },
            );
        }
    }
    credentials
}

fn resolve_secret_reference(
    file_base: &Path,
    path: PathBuf,
    field: &'static str,
    diagnostics: &mut DiagnosticCollector,
) -> Option<SecretFileReference> {
    let Some(path) = resolve_path(file_base, &path) else {
        diagnostics.push(host_diagnostic(
            field,
            ConfigurationValidationCode::SecretReferenceInvalid,
            "secret file reference must resolve to a nonempty path",
        ));
        return None;
    };
    if !ssh_argument_path_is_literal(&path) {
        diagnostics.push(host_diagnostic(
            field,
            ConfigurationValidationCode::SecretReferenceInvalid,
            "SSH credential paths may contain only ASCII letters, digits, '/', '.', '_', and '-' after resolution",
        ));
        return None;
    }
    if path_is_in_nix_store(&path)
        || std::fs::canonicalize(&path).is_ok_and(|canonical| path_is_in_nix_store(&canonical))
    {
        diagnostics.push(host_diagnostic(
            field,
            ConfigurationValidationCode::SecretReferenceInvalid,
            "SSH credential files must live outside the immutable Nix store",
        ));
        return None;
    }
    SecretFileReference::new(path).or_else(|| {
        diagnostics.push(host_diagnostic(
            field,
            ConfigurationValidationCode::SecretReferenceInvalid,
            "secret file reference must resolve to a nonempty path",
        ));
        None
    })
}

fn ssh_argument_path_is_literal(path: &Path) -> bool {
    path.to_str().is_some_and(|value| {
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
    })
}

fn path_is_in_nix_store(path: &Path) -> bool {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Prefix(_)
            | std::path::Component::RootDir
            | std::path::Component::Normal(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized.starts_with(Path::new("/nix/store"))
}

fn validate_git_process_limits(
    candidate: &SourceCandidate,
    diagnostics: &mut DiagnosticCollector,
) -> Option<GitProcessLimits> {
    let defaults = GitProcessLimits::default();
    let wall_time = validate_duration(
        candidate
            .fetch_timeout_seconds
            .unwrap_or(defaults.wall_time.get().as_secs()),
        "source.fetch_timeout_seconds",
        GitFetchTimeout::from_seconds,
        diagnostics,
    );
    let command_output_bytes = validate_u64(
        candidate
            .command_output_bytes
            .unwrap_or(defaults.command_output_bytes.get()),
        "source.command_output_bytes",
        GitCommandOutputByteLimit::new,
        diagnostics,
    );
    let mirror_bytes = validate_u64(
        candidate
            .mirror_bytes
            .unwrap_or(defaults.mirror_bytes.get()),
        "source.mirror_bytes",
        GitMirrorByteLimit::new,
        diagnostics,
    );
    let file_bytes = validate_u64(
        candidate.file_bytes.unwrap_or(defaults.file_bytes.get()),
        "source.file_bytes",
        GitFileByteLimit::new,
        diagnostics,
    );
    let address_space_bytes = validate_u64(
        candidate
            .address_space_bytes
            .unwrap_or(defaults.address_space_bytes.get()),
        "source.address_space_bytes",
        GitAddressSpaceByteLimit::new,
        diagnostics,
    );
    let cpu_seconds = validate_u64(
        candidate.cpu_seconds.unwrap_or(defaults.cpu_seconds.get()),
        "source.cpu_seconds",
        GitCpuSecondLimit::new,
        diagnostics,
    );
    let open_files = validate_u64(
        candidate.open_files.unwrap_or(defaults.open_files.get()),
        "source.open_files",
        GitOpenFileLimit::new,
        diagnostics,
    );
    match (
        wall_time,
        command_output_bytes,
        mirror_bytes,
        file_bytes,
        address_space_bytes,
        cpu_seconds,
        open_files,
    ) {
        (
            Some(wall_time),
            Some(command_output_bytes),
            Some(mirror_bytes),
            Some(file_bytes),
            Some(address_space_bytes),
            Some(cpu_seconds),
            Some(open_files),
        ) => Some(GitProcessLimits {
            wall_time,
            command_output_bytes,
            mirror_bytes,
            file_bytes,
            address_space_bytes,
            cpu_seconds,
            open_files,
        }),
        _ => None,
    }
}

fn validate_u64<Value>(
    raw: u64,
    field: &'static str,
    constructor: impl FnOnce(u64) -> Option<Value>,
    diagnostics: &mut DiagnosticCollector,
) -> Option<Value> {
    let parsed = constructor(raw);
    if parsed.is_none() {
        diagnostics.push(host_diagnostic(
            field,
            ConfigurationValidationCode::LimitOutOfRange,
            "configured limit is outside its accepted positive range",
        ));
    }
    parsed
}

fn validate_content_limits(
    candidate: ContentCandidate,
    diagnostics: &mut DiagnosticCollector,
) -> Option<ContentTreeLimits> {
    let defaults = ContentTreeLimits::default();
    let publication_file_bytes = validate_content_file_limit(
        candidate
            .publication_file_bytes
            .unwrap_or(defaults.publication_file_bytes.get()),
        defaults.publication_file_bytes.get(),
        "content.publication_file_bytes",
        diagnostics,
    );
    let post_file_bytes = validate_content_file_limit(
        candidate
            .post_file_bytes
            .unwrap_or(defaults.post_file_bytes.get()),
        defaults.post_file_bytes.get(),
        "content.post_file_bytes",
        diagnostics,
    );
    let asset_file_bytes = validate_content_file_limit(
        candidate
            .asset_file_bytes
            .unwrap_or(defaults.asset_file_bytes.get()),
        defaults.asset_file_bytes.get(),
        "content.asset_file_bytes",
        diagnostics,
    );
    let total_tree_bytes = validate_content_tree_limit(
        candidate
            .total_tree_bytes
            .unwrap_or(defaults.total_tree_bytes.get()),
        defaults.total_tree_bytes.get(),
        "content.total_tree_bytes",
        diagnostics,
    );
    let entries = validate_usize(
        candidate.entries.unwrap_or(defaults.entries.get() as u64),
        "content.entries",
        ConfigurationValidationCode::LimitOutOfRange,
        |value| {
            if value <= defaults.entries.get() {
                ContentEntryLimit::new(value)
            } else {
                None
            }
        },
        diagnostics,
    );
    let depth = validate_usize(
        candidate.depth.unwrap_or(defaults.depth.get() as u64),
        "content.depth",
        ConfigurationValidationCode::LimitOutOfRange,
        |value| {
            if value <= defaults.depth.get() {
                ContentDepthLimit::new(value)
            } else {
                None
            }
        },
        diagnostics,
    );
    let path_bytes = validate_usize(
        candidate
            .path_bytes
            .unwrap_or(defaults.path_bytes.get() as u64),
        "content.path_bytes",
        ConfigurationValidationCode::LimitOutOfRange,
        |value| {
            if value <= defaults.path_bytes.get() {
                ContentPathByteLimit::new(value)
            } else {
                None
            }
        },
        diagnostics,
    );

    let (
        Some(publication_file_bytes),
        Some(post_file_bytes),
        Some(asset_file_bytes),
        Some(total_tree_bytes),
        Some(entries),
        Some(depth),
        Some(path_bytes),
    ) = (
        publication_file_bytes,
        post_file_bytes,
        asset_file_bytes,
        total_tree_bytes,
        entries,
        depth,
        path_bytes,
    )
    else {
        return None;
    };

    match ContentTreeLimits::new(
        publication_file_bytes,
        post_file_bytes,
        asset_file_bytes,
        total_tree_bytes,
        entries,
        depth,
        path_bytes,
    ) {
        Ok(limits) => Some(limits),
        Err(_) => {
            diagnostics.push(host_diagnostic(
                "content.total_tree_bytes",
                ConfigurationValidationCode::ContentLimitRelationshipInvalid,
                "each content file limit must not exceed the total tree limit",
            ));
            None
        }
    }
}

fn validate_content_file_limit(
    raw: u64,
    maximum: u64,
    field: &'static str,
    diagnostics: &mut DiagnosticCollector,
) -> Option<ContentFileByteLimit> {
    let parsed = if raw <= maximum {
        ContentFileByteLimit::new(raw)
    } else {
        None
    };
    if parsed.is_none() {
        diagnostics.push(host_diagnostic(
            field,
            ConfigurationValidationCode::LimitOutOfRange,
            "configured content file limit is outside its accepted positive range",
        ));
    }
    parsed
}

fn validate_content_tree_limit(
    raw: u64,
    maximum: u64,
    field: &'static str,
    diagnostics: &mut DiagnosticCollector,
) -> Option<ContentTreeByteLimit> {
    let parsed = if raw <= maximum {
        ContentTreeByteLimit::new(raw)
    } else {
        None
    };
    if parsed.is_none() {
        diagnostics.push(host_diagnostic(
            field,
            ConfigurationValidationCode::LimitOutOfRange,
            "configured content tree limit is outside its accepted positive range",
        ));
    }
    parsed
}

fn select_path(
    file: Option<PathBuf>,
    working_directory: &Path,
    file_base: &Path,
    default: &Path,
    field: &'static str,
    diagnostics: &mut DiagnosticCollector,
) -> Option<PathBuf> {
    let resolved = match file {
        Some(path) => resolve_path(file_base, &path),
        None => resolve_path(working_directory, default),
    };
    validate_resolved_path(resolved, field, diagnostics)
}

fn validate_resolved_path(
    path: Option<PathBuf>,
    field: &'static str,
    diagnostics: &mut DiagnosticCollector,
) -> Option<PathBuf> {
    match path {
        Some(path) if !path.as_os_str().is_empty() => Some(path),
        _ => {
            diagnostics.push(host_diagnostic(
                field,
                ConfigurationValidationCode::PathInvalid,
                "configured path must not be empty",
            ));
            None
        }
    }
}

fn validate_usize<Value>(
    raw: u64,
    field: &'static str,
    code: ConfigurationValidationCode,
    constructor: impl FnOnce(usize) -> Option<Value>,
    diagnostics: &mut DiagnosticCollector,
) -> Option<Value> {
    let parsed = usize::try_from(raw).ok().and_then(constructor);
    if parsed.is_none() {
        diagnostics.push(host_diagnostic(
            field,
            code,
            "configured limit is outside its accepted positive range",
        ));
    }
    parsed
}

fn validate_duration<Value>(
    raw: u64,
    field: &'static str,
    constructor: impl FnOnce(u64) -> Option<Value>,
    diagnostics: &mut DiagnosticCollector,
) -> Option<Value> {
    let parsed = constructor(raw);
    if parsed.is_none() {
        diagnostics.push(host_diagnostic(
            field,
            ConfigurationValidationCode::DurationInvalid,
            "configured duration is outside its accepted positive range",
        ));
    }
    parsed
}

fn host_diagnostic(
    field: impl Into<Box<str>>,
    code: ConfigurationValidationCode,
    message: &'static str,
) -> ConfigurationDiagnostic {
    ConfigurationDiagnostic::new(field, code, message)
}

fn default_public_bind() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PUBLIC_PORT)
}

fn default_admin_bind() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_ADMIN_PORT)
}

fn validate_admin_bind(
    value: SocketAddr,
    diagnostics: &mut DiagnosticCollector,
) -> Option<AdminBind> {
    match AdminBind::new(value) {
        Ok(value) => Some(value),
        Err(_) => {
            diagnostics.push(host_diagnostic(
                "admin.bind",
                ConfigurationValidationCode::AdminBindInvalid,
                "admin.bind must use a loopback address",
            ));
            None
        }
    }
}

fn validate_admin_origin(
    value: String,
    diagnostics: &mut DiagnosticCollector,
) -> Option<AdminOrigin> {
    match AdminOrigin::parse(&value) {
        Ok(value) => Some(value),
        Err(_) => {
            diagnostics.push(host_diagnostic(
                "admin.origin",
                ConfigurationValidationCode::AdminOriginInvalid,
                "admin.origin must be one canonical HTTPS origin",
            ));
            None
        }
    }
}

fn resolve_path(base: &Path, path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        return None;
    }
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        Some(base.join(path))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn loader(root: &Path) -> HostConfigurationLoader {
        HostConfigurationLoader::new(root.to_path_buf()).unwrap()
    }

    fn write_config(root: &Path, relative: &str, source: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, source).unwrap();
        path
    }

    #[test]
    fn empty_file_locks_every_built_in_host_default() {
        let root = tempdir().unwrap();
        write_config(root.path(), "maincopy.toml", "");
        let config = loader(root.path())
            .load(Path::new("maincopy.toml"))
            .unwrap();

        assert_eq!(config.view().content_root, root.path().join("content"));
        assert_eq!(config.view().state_root, root.path().join("state"));
        assert_eq!(config.view().runtime_root, root.path().join("run"));
        assert_eq!(
            config.view().public_bind,
            "127.0.0.1:3000".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            config.view().admin_bind.into_socket_addr(),
            "127.0.0.1:3001".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            config.view().admin_origin.as_str(),
            "https://admin.localhost"
        );
        assert_eq!(
            config.view().database.path,
            root.path().join("state/maincopy.db")
        );
        assert_eq!(
            config.view().database.busy_timeout.get(),
            Duration::from_secs(5)
        );
        assert_eq!(config.view().database.writer_queue_capacity.get(), 128);
        assert_eq!(config.view().database.read_pool_size.get(), 4);
        assert_eq!(config.view().content_limits, ContentTreeLimits::default());
    }

    #[test]
    fn checked_in_example_configuration_points_at_checked_in_content() {
        let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let config = loader(&crate_root)
            .load(Path::new("examples/maincopy.toml"))
            .unwrap();

        assert_eq!(
            config.view().content_root,
            crate_root.join("examples/content")
        );
        assert!(
            config
                .view()
                .content_root
                .join("publication.toml")
                .is_file()
        );
        assert_eq!(
            config.view().state_root,
            crate_root.join("examples/../../../target/maincopy-example/state")
        );
        assert_eq!(
            config.view().runtime_root,
            crate_root.join("examples/../../../target/maincopy-example/run")
        );
        assert_eq!(
            config.view().admin_origin.as_str(),
            "https://admin.localhost:8443"
        );
    }

    #[test]
    fn complete_content_limit_schema_is_typed() {
        let root = tempdir().unwrap();
        write_config(
            root.path(),
            "maincopy.toml",
            "[content]\n\
             publication_file_bytes = 10\n\
             post_file_bytes = 20\n\
             asset_file_bytes = 30\n\
             total_tree_bytes = 40\n\
             entries = 50\n\
             depth = 7\n\
             path_bytes = 70\n",
        );

        let config = loader(root.path())
            .load(Path::new("maincopy.toml"))
            .unwrap();
        let limits = config.view().content_limits;

        assert_eq!(limits.publication_file_bytes.get(), 10);
        assert_eq!(limits.post_file_bytes.get(), 20);
        assert_eq!(limits.asset_file_bytes.get(), 30);
        assert_eq!(limits.total_tree_bytes.get(), 40);
        assert_eq!(limits.entries.get(), 50);
        assert_eq!(limits.depth.get(), 7);
        assert_eq!(limits.path_bytes.get(), 70);
    }

    #[test]
    fn content_limit_file_values_map_to_typed_limits() {
        let root = tempdir().unwrap();
        write_config(
            root.path(),
            "maincopy.toml",
            "[content]\n\
             publication_file_bytes = 11\n\
             post_file_bytes = 12\n\
             asset_file_bytes = 13\n\
             total_tree_bytes = 14\n\
             entries = 15\n\
             depth = 8\n\
             path_bytes = 16\n",
        );

        let config = loader(root.path())
            .load(Path::new("maincopy.toml"))
            .unwrap();
        let limits = config.view().content_limits;

        assert_eq!(limits.publication_file_bytes.get(), 11);
        assert_eq!(limits.post_file_bytes.get(), 12);
        assert_eq!(limits.asset_file_bytes.get(), 13);
        assert_eq!(limits.total_tree_bytes.get(), 14);
        assert_eq!(limits.entries.get(), 15);
        assert_eq!(limits.depth.get(), 8);
        assert_eq!(limits.path_bytes.get(), 16);
    }

    #[test]
    fn content_limit_hard_caps_are_inclusive() {
        let root = tempdir().unwrap();
        let caps = ContentTreeLimits::default();
        write_config(
            root.path(),
            "maincopy.toml",
            &format!(
                "[content]\n\
                 publication_file_bytes = {}\n\
                 post_file_bytes = {}\n\
                 asset_file_bytes = {}\n\
                 total_tree_bytes = {}\n\
                 entries = {}\n\
                 depth = {}\n\
                 path_bytes = {}\n",
                caps.publication_file_bytes.get(),
                caps.post_file_bytes.get(),
                caps.asset_file_bytes.get(),
                caps.total_tree_bytes.get(),
                caps.entries.get(),
                caps.depth.get(),
                caps.path_bytes.get(),
            ),
        );

        let config = loader(root.path())
            .load(Path::new("maincopy.toml"))
            .unwrap();
        let limits = config.view().content_limits;

        assert_eq!(limits, caps);
    }

    #[test]
    fn every_content_limit_rejects_zero_and_one_above_its_hard_cap() {
        let root = tempdir().unwrap();
        let caps = ContentTreeLimits::default();
        let cases = [
            (
                "publication_file_bytes",
                caps.publication_file_bytes.get() + 1,
            ),
            ("post_file_bytes", caps.post_file_bytes.get() + 1),
            ("asset_file_bytes", caps.asset_file_bytes.get() + 1),
            ("total_tree_bytes", caps.total_tree_bytes.get() + 1),
            ("entries", caps.entries.get() as u64 + 1),
            ("depth", caps.depth.get() as u64 + 1),
            ("path_bytes", caps.path_bytes.get() as u64 + 1),
        ];

        for (index, (field, above_cap)) in cases.into_iter().enumerate() {
            for (boundary, value) in [("zero", 0), ("above-cap", above_cap)] {
                let name = format!("content-{boundary}-{index}.toml");
                write_config(
                    root.path(),
                    &name,
                    &format!("[content]\n{field} = {value}\n"),
                );
                let errors = loader(root.path()).load(Path::new(&name)).unwrap_err();

                assert_eq!(errors.diagnostics().len(), 1);
                let expected_field = format!("content.{field}");
                assert_eq!(
                    errors.diagnostics()[0].field.as_ref(),
                    expected_field.as_str()
                );
                assert_eq!(
                    errors.diagnostics()[0].code,
                    ConfigurationValidationCode::LimitOutOfRange
                );
            }
        }
    }

    #[test]
    fn content_file_limits_must_not_exceed_the_total_tree_limit() {
        let root = tempdir().unwrap();
        write_config(
            root.path(),
            "maincopy.toml",
            "[content]\n\
             publication_file_bytes = 2\n\
             post_file_bytes = 1\n\
             asset_file_bytes = 1\n\
             total_tree_bytes = 1\n\
             entries = 1\n\
             depth = 1\n\
             path_bytes = 1\n",
        );

        let errors = loader(root.path())
            .load(Path::new("maincopy.toml"))
            .unwrap_err();

        assert_eq!(errors.diagnostics().len(), 1);
        assert_eq!(
            errors.diagnostics()[0].field.as_ref(),
            "content.total_tree_bytes"
        );
        assert_eq!(
            errors.diagnostics()[0].code,
            ConfigurationValidationCode::ContentLimitRelationshipInvalid
        );
    }

    #[test]
    fn relative_file_paths_resolve_from_the_configuration_parent() {
        let root = tempdir().unwrap();
        write_config(
            root.path(),
            "host/maincopy.toml",
            "[paths]\n\
             content_root = \"file-content\"\n\
             state_root = \"file-state\"\n\
             runtime_root = \"file-run\"\n\
             [public]\n\
             bind = \"127.0.0.1:3001\"\n\
             [admin]\n\
             bind = \"127.0.0.1:3002\"\n\
             origin = \"https://file-admin.example.test\"\n\
             [database]\n\
             path = \"file.db\"\n\
             busy_timeout_ms = 6000\n\
             writer_queue_capacity = 129\n\
             read_pool_size = 5\n",
        );
        let config = loader(root.path())
            .load(Path::new("host/maincopy.toml"))
            .unwrap();

        assert_eq!(
            config.view().content_root,
            root.path().join("host/file-content")
        );
        assert_eq!(
            config.view().state_root,
            root.path().join("host/file-state")
        );
        assert_eq!(
            config.view().runtime_root,
            root.path().join("host/file-run")
        );
        assert_eq!(config.view().public_bind.port(), 3_001);
        assert_eq!(
            config.view().admin_bind.into_socket_addr(),
            "127.0.0.1:3002".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            config.view().admin_origin.as_str(),
            "https://file-admin.example.test"
        );
        assert_eq!(
            config.view().database.path,
            root.path().join("host/file.db")
        );
        assert_eq!(
            config.view().database.busy_timeout.get(),
            Duration::from_secs(6)
        );
        assert_eq!(config.view().database.writer_queue_capacity.get(), 129);
        assert_eq!(config.view().database.read_pool_size.get(), 5);
    }

    #[test]
    fn derived_database_default_follows_the_effective_state_root() {
        let root = tempdir().unwrap();
        write_config(
            root.path(),
            "host/maincopy.toml",
            "[paths]\nstate_root = \"file-state\"\n",
        );
        let config = loader(root.path())
            .load(Path::new("host/maincopy.toml"))
            .unwrap();

        assert_eq!(config.view().content_root, root.path().join("content"));
        assert_eq!(
            config.view().database.path,
            root.path().join("host/file-state/maincopy.db")
        );
    }

    #[test]
    fn source_defaults_to_the_external_checkout_boundary() {
        let root = tempdir().unwrap();
        write_config(root.path(), "maincopy.toml", "");

        let config = loader(root.path())
            .load(Path::new("maincopy.toml"))
            .unwrap();

        assert_eq!(
            config.view().source,
            SourceConfigurationView::ExternalCheckout
        );
    }

    #[test]
    fn managed_git_configuration_keeps_secret_paths_redacted() {
        let root = tempdir().unwrap();
        write_config(
            root.path(),
            "maincopy.toml",
            "[paths]\n\
             state_root = \"state\"\n\
             [source]\n\
             mode = \"managed_git\"\n\
             mirror_root = \"state/git-mirror\"\n\
             fetch_timeout_seconds = 60\n\
             command_output_bytes = 1048576\n\
             mirror_bytes = 1073741824\n\
             file_bytes = 536870912\n\
             address_space_bytes = 1073741824\n\
             cpu_seconds = 60\n\
             open_files = 128\n\
             [source.ssh_credentials.deploy]\n\
             private_key_file = \"secrets/deploy-key\"\n\
             known_hosts_file = \"secrets/known-hosts\"\n",
        );

        let config = loader(root.path())
            .load(Path::new("maincopy.toml"))
            .unwrap();
        let SourceConfigurationView::ManagedGit {
            mirror_root,
            credentials,
            limits,
        } = config.view().source
        else {
            panic!("expected managed Git source mode");
        };
        assert_eq!(mirror_root.path(), root.path().join("state/git-mirror"));
        assert_eq!(credentials.len(), 1);
        assert_eq!(limits.wall_time.get(), Duration::from_secs(60));
        assert_eq!(limits.command_output_bytes.get(), 1_048_576);
        let rendered = format!("{credentials:?} {mirror_root:?}");
        assert!(!rendered.contains("deploy-key"));
        assert!(!rendered.contains("known-hosts"));
        assert!(!rendered.contains(root.path().to_string_lossy().as_ref()));
    }

    #[test]
    fn managed_git_requires_a_dedicated_state_child_and_named_credential() {
        let root = tempdir().unwrap();
        for (index, source) in [
            "[source]\nmode = \"managed_git\"\nmirror_root = \"elsewhere\"\n",
            "[source]\nmode = \"managed_git\"\nmirror_root = \"state/git\"\n",
            "[source]\nmode = \"external_checkout\"\nmirror_root = \"state/git\"\n",
            "[source]\nmode = \"external_checkout\"\n[source.ssh_credentials.deploy]\nprivate_key_file = \"key\"\nknown_hosts_file = \"hosts\"\n",
        ]
        .into_iter()
        .enumerate()
        {
            let name = format!("invalid-source-{index}.toml");
            write_config(root.path(), &name, source);
            let errors = loader(root.path()).load(Path::new(&name)).unwrap_err();
            assert!(
                errors.diagnostics().iter().any(|diagnostic| matches!(
                    diagnostic.code,
                    ConfigurationValidationCode::PathInvalid
                        | ConfigurationValidationCode::SecretReferenceInvalid
                        | ConfigurationValidationCode::SourceModeConflict
                )),
                "unexpected diagnostics: {errors:?}"
            );
        }
    }

    #[test]
    fn managed_git_credentials_cannot_reside_in_the_nix_store() {
        let root = tempdir().unwrap();
        write_config(
            root.path(),
            "maincopy.toml",
            "[source]\n\
             mode = \"managed_git\"\n\
             mirror_root = \"state/git\"\n\
             [source.ssh_credentials.deploy]\n\
             private_key_file = \"/nix/store/example-private-key\"\n\
             known_hosts_file = \"/var/../nix/store/example-known-hosts\"\n",
        );

        let errors = loader(root.path())
            .load(Path::new("maincopy.toml"))
            .unwrap_err();

        for field in [
            "source.ssh_credentials.private_key_file",
            "source.ssh_credentials.known_hosts_file",
        ] {
            assert!(errors.diagnostics().iter().any(|diagnostic| {
                diagnostic.field.as_ref() == field
                    && diagnostic.code == ConfigurationValidationCode::SecretReferenceInvalid
            }));
        }
        let rendered = format!("{errors:?} {errors}");
        assert!(!rendered.contains("example-private-key"));
        assert!(!rendered.contains("example-known-hosts"));
    }

    #[test]
    fn managed_git_credential_paths_cannot_reenter_ssh_configuration() {
        let root = tempdir().unwrap();
        for (index, field, private_key, known_hosts) in [
            (
                0,
                "source.ssh_credentials.private_key_file",
                "secrets/deploy%h",
                "secrets/known-hosts",
            ),
            (
                1,
                "source.ssh_credentials.known_hosts_file",
                "secrets/deploy-key",
                "secrets/known hosts",
            ),
            (
                2,
                "source.ssh_credentials.private_key_file",
                "secrets/${HOME}/deploy-key",
                "secrets/known-hosts",
            ),
        ] {
            let name = format!("unsafe-ssh-path-{index}.toml");
            write_config(
                root.path(),
                &name,
                &format!(
                    "[source]\n\
                     mode = \"managed_git\"\n\
                     mirror_root = \"state/git\"\n\
                     [source.ssh_credentials.deploy]\n\
                     private_key_file = \"{private_key}\"\n\
                     known_hosts_file = \"{known_hosts}\"\n"
                ),
            );

            let errors = loader(root.path()).load(Path::new(&name)).unwrap_err();
            assert!(errors.diagnostics().iter().any(|diagnostic| {
                diagnostic.field.as_ref() == field
                    && diagnostic.code == ConfigurationValidationCode::SecretReferenceInvalid
            }));
            let rendered = format!("{errors:?} {errors}");
            assert!(!rendered.contains(private_key));
            assert!(!rendered.contains(known_hosts));
        }
    }

    #[test]
    fn every_managed_git_limit_has_a_closed_positive_cap() {
        assert!(GitFetchTimeout::from_seconds(MAX_GIT_FETCH_TIMEOUT_SECONDS).is_some());
        assert!(GitFetchTimeout::from_seconds(MAX_GIT_FETCH_TIMEOUT_SECONDS + 1).is_none());
        assert!(GitCommandOutputByteLimit::new(MAX_GIT_COMMAND_OUTPUT_BYTES).is_some());
        assert!(GitCommandOutputByteLimit::new(MAX_GIT_COMMAND_OUTPUT_BYTES + 1).is_none());
        assert!(GitMirrorByteLimit::new(MAX_GIT_MIRROR_BYTES).is_some());
        assert!(GitMirrorByteLimit::new(MAX_GIT_MIRROR_BYTES + 1).is_none());
        assert!(GitFileByteLimit::new(MAX_GIT_FILE_BYTES).is_some());
        assert!(GitFileByteLimit::new(MAX_GIT_FILE_BYTES + 1).is_none());
        assert!(GitAddressSpaceByteLimit::new(MAX_GIT_ADDRESS_SPACE_BYTES).is_some());
        assert!(GitAddressSpaceByteLimit::new(MAX_GIT_ADDRESS_SPACE_BYTES + 1).is_none());
        assert!(GitCpuSecondLimit::new(MAX_GIT_CPU_SECONDS).is_some());
        assert!(GitCpuSecondLimit::new(MAX_GIT_CPU_SECONDS + 1).is_none());
        assert!(GitOpenFileLimit::new(MAX_GIT_OPEN_FILES).is_some());
        assert!(GitOpenFileLimit::new(MAX_GIT_OPEN_FILES + 1).is_none());
    }

    #[test]
    fn removed_payment_provider_configuration_is_rejected_as_unknown() {
        let root = tempdir().unwrap();
        write_config(
            root.path(),
            "host/maincopy.toml",
            "[lightning]\n\
             provider = \"lexe\"\n\
             network = \"mainnet\"\n\
             credentials = { source = \"file\", path = \"secret/credential.json\" }\n\
             cache_path = \"private/cache\"\n",
        );
        let errors = loader(root.path())
            .load(Path::new("host/maincopy.toml"))
            .unwrap_err();

        assert_eq!(
            errors.diagnostics()[0].code,
            ConfigurationValidationCode::HostTomlInvalid
        );
        let diagnostic_view = format!("{errors:?} {errors}");
        for protected in ["credential.json", "private/cache"] {
            assert!(!diagnostic_view.contains(protected));
        }
    }

    #[test]
    fn removed_admin_socket_configuration_is_rejected_as_unknown() {
        let root = tempdir().unwrap();
        write_config(
            root.path(),
            "host/maincopy.toml",
            "[admin]\nsocket = \"admin.sock\"\n",
        );

        let errors = loader(root.path())
            .load(Path::new("host/maincopy.toml"))
            .unwrap_err();

        assert_eq!(
            errors.diagnostics()[0].code,
            ConfigurationValidationCode::HostTomlInvalid
        );
    }

    #[test]
    fn admin_listener_and_origin_reject_unsafe_values() {
        let root = tempdir().unwrap();
        for (index, (field, value, code)) in [
            (
                "bind",
                "0.0.0.0:3001",
                ConfigurationValidationCode::AdminBindInvalid,
            ),
            (
                "origin",
                "http://admin.example.test",
                ConfigurationValidationCode::AdminOriginInvalid,
            ),
            (
                "origin",
                "https://admin.example.test/path",
                ConfigurationValidationCode::AdminOriginInvalid,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let name = format!("unsafe-admin-{index}.toml");
            write_config(
                root.path(),
                &name,
                &format!("[admin]\n{field} = \"{value}\"\n"),
            );

            let errors = loader(root.path()).load(Path::new(&name)).unwrap_err();
            assert_eq!(errors.diagnostics().len(), 1);
            assert_eq!(errors.diagnostics()[0].code, code);
        }
    }

    #[test]
    fn every_host_nesting_rejects_unknown_fields() {
        let root = tempdir().unwrap();
        let cases = [
            "unknown = true\n",
            "[paths]\nunknown = true\n",
            "[content]\nunknown = true\n",
            "[public]\nunknown = true\n",
            "[admin]\nunknown = true\n",
            "[database]\nunknown = true\n",
            "[source]\nunknown = true\n",
            "[source.ssh_credentials.deploy]\nprivate_key_file = \"key\"\nknown_hosts_file = \"hosts\"\nunknown = true\n",
            "[lightning]\nprovider = \"lexe\"\nnetwork = \"mainnet\"\ncredentials = { source = \"file\", path = \"secret\" }\nunknown = true\n",
            "[lightning]\nprovider = \"lexe\"\nnetwork = \"mainnet\"\ncredentials = { source = \"file\", path = \"secret\", unknown = true }\n",
        ];

        for (index, source) in cases.into_iter().enumerate() {
            let name = format!("case-{index}.toml");
            write_config(root.path(), &name, source);
            let errors = loader(root.path()).load(Path::new(&name)).unwrap_err();
            assert_eq!(
                errors.diagnostics()[0].code,
                ConfigurationValidationCode::HostTomlInvalid,
                "source unexpectedly accepted: {source}"
            );
        }
    }

    #[test]
    fn wrong_host_field_types_are_rejected_without_echoing_source_values() {
        let root = tempdir().unwrap();
        let protected = "DO_NOT_DISCLOSE_THIS_CONFIGURATION_VALUE";
        let cases = [
            "paths = \"not-a-table\"\n".to_owned(),
            "content = \"not-a-table\"\n".to_owned(),
            "[public]\nbind = 3000\n".to_owned(),
            format!("[database]\nread_pool_size = \"{protected}\"\n"),
            "[lightning]\nprovider = 7\n".to_owned(),
        ];

        for (index, source) in cases.into_iter().enumerate() {
            let name = format!("wrong-type-{index}.toml");
            write_config(root.path(), &name, &source);
            let errors = loader(root.path()).load(Path::new(&name)).unwrap_err();

            assert_eq!(
                errors.diagnostics()[0].code,
                ConfigurationValidationCode::HostTomlInvalid,
                "source unexpectedly accepted: {source}"
            );
            let rendered = format!("{errors:?} {errors}");
            assert!(!rendered.contains(protected));
            assert!(!rendered.contains(root.path().to_string_lossy().as_ref()));
        }
    }

    #[test]
    fn invalid_limits_aggregate_stable_codes() {
        let root = tempdir().unwrap();
        write_config(
            root.path(),
            "maincopy.toml",
            "[database]\n\
             busy_timeout_ms = 0\n\
             writer_queue_capacity = 0\n\
             read_pool_size = 0\n",
        );
        let errors = loader(root.path())
            .load(Path::new("maincopy.toml"))
            .unwrap_err();
        let codes = errors
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.code)
            .collect::<Vec<_>>();

        assert!(codes.contains(&ConfigurationValidationCode::LimitOutOfRange));
        assert!(codes.contains(&ConfigurationValidationCode::DurationInvalid));
    }

    #[test]
    fn numeric_operational_caps_reject_runtime_overflow_and_resource_values() {
        assert!(
            DatabaseBusyTimeout::from_milliseconds(MAX_DATABASE_BUSY_TIMEOUT_MILLISECONDS)
                .is_some()
        );
        assert!(
            DatabaseBusyTimeout::from_milliseconds(MAX_DATABASE_BUSY_TIMEOUT_MILLISECONDS + 1)
                .is_none()
        );
        assert!(DatabaseWriterQueueCapacity::new(MAX_DATABASE_WRITER_QUEUE_CAPACITY).is_some());
        assert!(DatabaseWriterQueueCapacity::new(MAX_DATABASE_WRITER_QUEUE_CAPACITY + 1).is_none());
        assert!(DatabaseReadPoolSize::new(MAX_DATABASE_READ_POOL_SIZE).is_some());
        assert!(DatabaseReadPoolSize::new(MAX_DATABASE_READ_POOL_SIZE + 1).is_none());
    }

    #[test]
    fn host_loader_requires_an_absolute_injected_working_directory() {
        let errors = HostConfigurationLoader::new(PathBuf::from("relative")).unwrap_err();

        assert_eq!(
            errors.diagnostics()[0].code,
            ConfigurationValidationCode::WorkingDirectoryUnavailable
        );
    }

    #[test]
    fn host_source_limits_and_utf8_errors_are_typed() {
        let root = tempdir().unwrap();
        fs::write(
            root.path().join("too-large.toml"),
            vec![b' '; MAX_HOST_DOCUMENT_BYTES as usize + 1],
        )
        .unwrap();
        fs::write(root.path().join("not-utf8.toml"), [0xff, 0x00]).unwrap();

        let too_large = loader(root.path())
            .load(Path::new("too-large.toml"))
            .unwrap_err();
        let not_utf8 = loader(root.path())
            .load(Path::new("not-utf8.toml"))
            .unwrap_err();

        assert_eq!(
            too_large.diagnostics()[0].code,
            ConfigurationValidationCode::HostDocumentTooLarge
        );
        assert_eq!(
            not_utf8.diagnostics()[0].code,
            ConfigurationValidationCode::HostTextInvalidUtf8
        );
    }
}
