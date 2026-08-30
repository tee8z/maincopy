use std::{
    fs::File,
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::content::{
    ContentDepthLimit, ContentEntryLimit, ContentFileByteLimit, ContentPathByteLimit,
    ContentTreeByteLimit, ContentTreeLimits,
};

use super::{
    diagnostic::{
        ConfigurationAuthority, ConfigurationDiagnostic, ConfigurationErrors,
        ConfigurationValidationCode, DiagnosticCollector, single_error, toml_location,
    },
    secret::{SecretFileReference, SecretReferenceCandidate, SensitivePath},
};

const MAX_HOST_DOCUMENT_BYTES: u64 = 1024 * 1024;
const DEFAULT_CONTENT_ROOT: &str = "content";
const DEFAULT_STATE_ROOT: &str = "state";
const DEFAULT_RUNTIME_ROOT: &str = "run";
const DEFAULT_ADMIN_SOCKET_NAME: &str = "admin.sock";
const DEFAULT_DATABASE_FILE_NAME: &str = "maincopy.db";
const DEFAULT_PUBLIC_PORT: u16 = 3000;
const DEFAULT_BUSY_TIMEOUT_MILLISECONDS: u64 = 5_000;
const DEFAULT_WRITER_QUEUE_CAPACITY: usize = 128;
const DEFAULT_READ_POOL_SIZE: usize = 4;
const DEFAULT_LEXE_MAX_IN_FLIGHT: usize = 4;
const DEFAULT_LEXE_MAX_PENDING: usize = 64;
const DEFAULT_LEXE_RESPONSE_TIMEOUT_MILLISECONDS: u64 = 15_000;
const DEFAULT_LEXE_RECONCILIATION_PAGE_SIZE: usize = 100;
const DEFAULT_LEXE_RECOVERY_INTERVAL_SECONDS: u64 = 60;
const MAX_DATABASE_BUSY_TIMEOUT_MILLISECONDS: u64 = 300_000;
const MAX_DATABASE_WRITER_QUEUE_CAPACITY: usize = 65_536;
const MAX_DATABASE_READ_POOL_SIZE: usize = 256;
const MAX_LEXE_IN_FLIGHT: usize = 1_024;
const MAX_LEXE_PENDING: usize = 65_536;
const MAX_LEXE_RESPONSE_TIMEOUT_MILLISECONDS: u64 = 300_000;
const MAX_LEXE_RECONCILIATION_PAGE_SIZE: usize = 1_000;
const MAX_LEXE_RECOVERY_INTERVAL_SECONDS: u64 = 86_400;

/// Non-secret command-line overrides for `maincopy serve`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostConfigurationOverrides {
    content_root: Option<PathBuf>,
    state_root: Option<PathBuf>,
    runtime_root: Option<PathBuf>,
    database_path: Option<PathBuf>,
    public_bind: Option<SocketAddr>,
    admin_socket: Option<PathBuf>,
    database_busy_timeout_ms: Option<u64>,
    database_writer_queue_capacity: Option<u64>,
    database_read_pool_size: Option<u64>,
    content_publication_file_bytes: Option<u64>,
    content_post_file_bytes: Option<u64>,
    content_asset_file_bytes: Option<u64>,
    content_total_tree_bytes: Option<u64>,
    content_entries: Option<u64>,
    content_depth: Option<u64>,
    content_path_bytes: Option<u64>,
}

impl HostConfigurationOverrides {
    pub fn with_content_root(mut self, value: PathBuf) -> Self {
        self.content_root = Some(value);
        self
    }

    pub fn with_state_root(mut self, value: PathBuf) -> Self {
        self.state_root = Some(value);
        self
    }

    pub fn with_runtime_root(mut self, value: PathBuf) -> Self {
        self.runtime_root = Some(value);
        self
    }

    pub fn with_database_path(mut self, value: PathBuf) -> Self {
        self.database_path = Some(value);
        self
    }

    pub const fn with_public_bind(mut self, value: SocketAddr) -> Self {
        self.public_bind = Some(value);
        self
    }

    pub fn with_admin_socket(mut self, value: PathBuf) -> Self {
        self.admin_socket = Some(value);
        self
    }

    pub const fn with_database_busy_timeout_ms(mut self, value: u64) -> Self {
        self.database_busy_timeout_ms = Some(value);
        self
    }

    pub const fn with_database_writer_queue_capacity(mut self, value: u64) -> Self {
        self.database_writer_queue_capacity = Some(value);
        self
    }

    pub const fn with_database_read_pool_size(mut self, value: u64) -> Self {
        self.database_read_pool_size = Some(value);
        self
    }

    pub const fn with_content_publication_file_bytes(mut self, value: u64) -> Self {
        self.content_publication_file_bytes = Some(value);
        self
    }

    pub const fn with_content_post_file_bytes(mut self, value: u64) -> Self {
        self.content_post_file_bytes = Some(value);
        self
    }

    pub const fn with_content_asset_file_bytes(mut self, value: u64) -> Self {
        self.content_asset_file_bytes = Some(value);
        self
    }

    pub const fn with_content_total_tree_bytes(mut self, value: u64) -> Self {
        self.content_total_tree_bytes = Some(value);
        self
    }

    pub const fn with_content_entries(mut self, value: u64) -> Self {
        self.content_entries = Some(value);
        self
    }

    pub const fn with_content_depth(mut self, value: u64) -> Self {
        self.content_depth = Some(value);
        self
    }

    pub const fn with_content_path_bytes(mut self, value: u64) -> Self {
        self.content_path_bytes = Some(value);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostPaths {
    content_root: PathBuf,
    state_root: PathBuf,
    runtime_root: PathBuf,
}

impl HostPaths {
    pub fn content_root(&self) -> &Path {
        &self.content_root
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub fn runtime_root(&self) -> &Path {
        &self.runtime_root
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicListenerConfiguration {
    bind: SocketAddr,
}

impl PublicListenerConfiguration {
    pub const fn bind(self) -> SocketAddr {
        self.bind
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminListenerConfiguration {
    socket: PathBuf,
}

impl AdminListenerConfiguration {
    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseBusyTimeout(Duration);

impl DatabaseBusyTimeout {
    pub fn from_milliseconds(value: u64) -> Option<Self> {
        (value > 0 && value <= MAX_DATABASE_BUSY_TIMEOUT_MILLISECONDS)
            .then_some(Self(Duration::from_millis(value)))
    }

    pub const fn get(self) -> Duration {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseWriterQueueCapacity(NonZeroUsize);

impl DatabaseWriterQueueCapacity {
    pub fn new(value: usize) -> Option<Self> {
        NonZeroUsize::new(value)
            .filter(|value| value.get() <= MAX_DATABASE_WRITER_QUEUE_CAPACITY)
            .map(Self)
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseReadPoolSize(NonZeroUsize);

impl DatabaseReadPoolSize {
    pub fn new(value: usize) -> Option<Self> {
        NonZeroUsize::new(value)
            .filter(|value| value.get() <= MAX_DATABASE_READ_POOL_SIZE)
            .map(Self)
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseConfiguration {
    path: PathBuf,
    busy_timeout: DatabaseBusyTimeout,
    writer_queue_capacity: DatabaseWriterQueueCapacity,
    read_pool_size: DatabaseReadPoolSize,
}

impl DatabaseConfiguration {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn busy_timeout(&self) -> DatabaseBusyTimeout {
        self.busy_timeout
    }

    pub const fn writer_queue_capacity(&self) -> DatabaseWriterQueueCapacity {
        self.writer_queue_capacity
    }

    pub const fn read_pool_size(&self) -> DatabaseReadPoolSize {
        self.read_pool_size
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LexeNetwork {
    Mainnet,
    Testnet3,
    Testnet4,
    Signet,
    Regtest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LexeInFlightLimit(NonZeroUsize);

impl LexeInFlightLimit {
    pub fn new(value: usize) -> Option<Self> {
        NonZeroUsize::new(value)
            .filter(|value| (2..=MAX_LEXE_IN_FLIGHT).contains(&value.get()))
            .map(Self)
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LexePendingLimit(NonZeroUsize);

impl LexePendingLimit {
    pub fn new(value: usize) -> Option<Self> {
        NonZeroUsize::new(value)
            .filter(|value| value.get() <= MAX_LEXE_PENDING)
            .map(Self)
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LexeResponseTimeout(Duration);

impl LexeResponseTimeout {
    pub fn from_milliseconds(value: u64) -> Option<Self> {
        (value > 0 && value <= MAX_LEXE_RESPONSE_TIMEOUT_MILLISECONDS)
            .then_some(Self(Duration::from_millis(value)))
    }

    pub const fn get(self) -> Duration {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LexeReconciliationPageSize(NonZeroUsize);

impl LexeReconciliationPageSize {
    pub fn new(value: usize) -> Option<Self> {
        NonZeroUsize::new(value)
            .filter(|value| value.get() <= MAX_LEXE_RECONCILIATION_PAGE_SIZE)
            .map(Self)
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LexeRecoveryInterval(Duration);

impl LexeRecoveryInterval {
    pub fn from_seconds(value: u64) -> Option<Self> {
        (value > 0 && value <= MAX_LEXE_RECOVERY_INTERVAL_SECONDS)
            .then_some(Self(Duration::from_secs(value)))
    }

    pub const fn get(self) -> Duration {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexeConfiguration {
    network: LexeNetwork,
    credentials: SecretFileReference,
    cache_path: Option<SensitivePath>,
    max_in_flight: LexeInFlightLimit,
    max_pending: LexePendingLimit,
    response_timeout: LexeResponseTimeout,
    reconciliation_page_size: LexeReconciliationPageSize,
    recovery_interval: LexeRecoveryInterval,
}

impl LexeConfiguration {
    pub const fn network(&self) -> LexeNetwork {
        self.network
    }

    pub const fn credentials(&self) -> &SecretFileReference {
        &self.credentials
    }

    pub const fn cache_path(&self) -> Option<&SensitivePath> {
        self.cache_path.as_ref()
    }

    pub const fn max_in_flight(&self) -> LexeInFlightLimit {
        self.max_in_flight
    }

    pub const fn max_pending(&self) -> LexePendingLimit {
        self.max_pending
    }

    pub const fn response_timeout(&self) -> LexeResponseTimeout {
        self.response_timeout
    }

    pub const fn reconciliation_page_size(&self) -> LexeReconciliationPageSize {
        self.reconciliation_page_size
    }

    pub const fn recovery_interval(&self) -> LexeRecoveryInterval {
        self.recovery_interval
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LightningConfiguration {
    Lexe(LexeConfiguration),
}

impl LightningConfiguration {
    pub const fn lexe(&self) -> &LexeConfiguration {
        match self {
            Self::Lexe(configuration) => configuration,
        }
    }
}

/// Effective host-owned runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostConfiguration {
    paths: HostPaths,
    content: ContentTreeLimits,
    public: PublicListenerConfiguration,
    admin: AdminListenerConfiguration,
    database: DatabaseConfiguration,
    lightning: Option<LightningConfiguration>,
}

impl HostConfiguration {
    pub const fn paths(&self) -> &HostPaths {
        &self.paths
    }

    pub const fn content(&self) -> ContentTreeLimits {
        self.content
    }

    pub const fn public(&self) -> PublicListenerConfiguration {
        self.public
    }

    pub const fn admin(&self) -> &AdminListenerConfiguration {
        &self.admin
    }

    pub const fn database(&self) -> &DatabaseConfiguration {
        &self.database
    }

    pub const fn lightning(&self) -> Option<&LightningConfiguration> {
        self.lightning.as_ref()
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

    pub fn new(working_directory: PathBuf) -> Result<Self, ConfigurationErrors> {
        if !working_directory.is_absolute() {
            return Err(single_error(host_diagnostic(
                "$working_directory",
                ConfigurationValidationCode::WorkingDirectoryUnavailable,
                "process working directory must be an absolute path",
            )));
        }
        Ok(Self { working_directory })
    }

    pub fn load(
        &self,
        config_path: &Path,
        overrides: HostConfigurationOverrides,
    ) -> Result<HostConfiguration, ConfigurationErrors> {
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
        finalize_host(candidate, overrides, &self.working_directory, file_base)
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
    lightning: Option<LightningCandidate>,
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
    socket: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DatabaseCandidate {
    path: Option<PathBuf>,
    busy_timeout_ms: Option<u64>,
    writer_queue_capacity: Option<u64>,
    read_pool_size: Option<u64>,
}

#[derive(Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
enum LightningCandidate {
    Lexe {
        network: LexeNetwork,
        credentials: SecretReferenceCandidate,
        cache_path: Option<PathBuf>,
        max_in_flight: Option<u64>,
        max_pending: Option<u64>,
        response_timeout_ms: Option<u64>,
        reconciliation_page_size: Option<u64>,
        recovery_interval_seconds: Option<u64>,
    },
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
    overrides: HostConfigurationOverrides,
    working_directory: &Path,
    file_base: &Path,
) -> Result<HostConfiguration, ConfigurationErrors> {
    let mut diagnostics = DiagnosticCollector::default();
    let content_limits = validate_content_limits(candidate.content, &overrides, &mut diagnostics);

    let content_root = select_path(
        overrides.content_root,
        candidate.paths.content_root,
        working_directory,
        file_base,
        Path::new(DEFAULT_CONTENT_ROOT),
        "paths.content_root",
        &mut diagnostics,
    );
    let state_root = select_path(
        overrides.state_root,
        candidate.paths.state_root,
        working_directory,
        file_base,
        Path::new(DEFAULT_STATE_ROOT),
        "paths.state_root",
        &mut diagnostics,
    );
    let runtime_root = select_path(
        overrides.runtime_root,
        candidate.paths.runtime_root,
        working_directory,
        file_base,
        Path::new(DEFAULT_RUNTIME_ROOT),
        "paths.runtime_root",
        &mut diagnostics,
    );

    let admin_socket = match overrides.admin_socket {
        Some(path) => validate_resolved_path(
            resolve_path(working_directory, &path),
            "admin.socket",
            &mut diagnostics,
        ),
        None => match candidate.admin.socket {
            Some(path) => validate_resolved_path(
                resolve_path(file_base, &path),
                "admin.socket",
                &mut diagnostics,
            ),
            None => runtime_root
                .as_ref()
                .map(|root| root.join(DEFAULT_ADMIN_SOCKET_NAME)),
        },
    };
    let database_path = match overrides.database_path {
        Some(path) => validate_resolved_path(
            resolve_path(working_directory, &path),
            "database.path",
            &mut diagnostics,
        ),
        None => match candidate.database.path {
            Some(path) => validate_resolved_path(
                resolve_path(file_base, &path),
                "database.path",
                &mut diagnostics,
            ),
            None => state_root
                .as_ref()
                .map(|root| root.join(DEFAULT_DATABASE_FILE_NAME)),
        },
    };
    let public_bind = overrides
        .public_bind
        .or(candidate.public.bind)
        .unwrap_or_else(default_public_bind);

    let busy_timeout = validate_duration_milliseconds::<DatabaseBusyTimeout>(
        overrides
            .database_busy_timeout_ms
            .or(candidate.database.busy_timeout_ms)
            .unwrap_or(DEFAULT_BUSY_TIMEOUT_MILLISECONDS),
        "database.busy_timeout_ms",
        DatabaseBusyTimeout::from_milliseconds,
        &mut diagnostics,
    );
    let writer_queue_capacity = validate_usize::<DatabaseWriterQueueCapacity>(
        overrides
            .database_writer_queue_capacity
            .or(candidate.database.writer_queue_capacity)
            .unwrap_or(DEFAULT_WRITER_QUEUE_CAPACITY as u64),
        "database.writer_queue_capacity",
        ConfigurationValidationCode::LimitOutOfRange,
        DatabaseWriterQueueCapacity::new,
        &mut diagnostics,
    );
    let read_pool_size = validate_usize::<DatabaseReadPoolSize>(
        overrides
            .database_read_pool_size
            .or(candidate.database.read_pool_size)
            .unwrap_or(DEFAULT_READ_POOL_SIZE as u64),
        "database.read_pool_size",
        ConfigurationValidationCode::LimitOutOfRange,
        DatabaseReadPoolSize::new,
        &mut diagnostics,
    );
    let lightning = candidate
        .lightning
        .and_then(|candidate| validate_lightning(candidate, file_base, &mut diagnostics));

    if !diagnostics.is_empty() {
        return Err(diagnostics.finish());
    }
    match (
        content_root,
        state_root,
        runtime_root,
        admin_socket,
        database_path,
        busy_timeout,
        writer_queue_capacity,
        read_pool_size,
        content_limits,
    ) {
        (
            Some(content_root),
            Some(state_root),
            Some(runtime_root),
            Some(admin_socket),
            Some(database_path),
            Some(busy_timeout),
            Some(writer_queue_capacity),
            Some(read_pool_size),
            Some(content),
        ) => Ok(HostConfiguration {
            paths: HostPaths {
                content_root,
                state_root,
                runtime_root,
            },
            content,
            public: PublicListenerConfiguration { bind: public_bind },
            admin: AdminListenerConfiguration {
                socket: admin_socket,
            },
            database: DatabaseConfiguration {
                path: database_path,
                busy_timeout,
                writer_queue_capacity,
                read_pool_size,
            },
            lightning,
        }),
        _ => Err(single_error(host_diagnostic(
            "$document",
            ConfigurationValidationCode::HostTomlInvalid,
            "effective host settings could not be constructed",
        ))),
    }
}

fn validate_content_limits(
    candidate: ContentCandidate,
    overrides: &HostConfigurationOverrides,
    diagnostics: &mut DiagnosticCollector,
) -> Option<ContentTreeLimits> {
    let defaults = ContentTreeLimits::default();
    let publication_file_bytes = validate_content_file_limit(
        overrides
            .content_publication_file_bytes
            .or(candidate.publication_file_bytes)
            .unwrap_or(defaults.publication_file_bytes().get()),
        defaults.publication_file_bytes().get(),
        "content.publication_file_bytes",
        diagnostics,
    );
    let post_file_bytes = validate_content_file_limit(
        overrides
            .content_post_file_bytes
            .or(candidate.post_file_bytes)
            .unwrap_or(defaults.post_file_bytes().get()),
        defaults.post_file_bytes().get(),
        "content.post_file_bytes",
        diagnostics,
    );
    let asset_file_bytes = validate_content_file_limit(
        overrides
            .content_asset_file_bytes
            .or(candidate.asset_file_bytes)
            .unwrap_or(defaults.asset_file_bytes().get()),
        defaults.asset_file_bytes().get(),
        "content.asset_file_bytes",
        diagnostics,
    );
    let total_tree_bytes = validate_content_tree_limit(
        overrides
            .content_total_tree_bytes
            .or(candidate.total_tree_bytes)
            .unwrap_or(defaults.total_tree_bytes().get()),
        defaults.total_tree_bytes().get(),
        "content.total_tree_bytes",
        diagnostics,
    );
    let entries = validate_usize(
        overrides
            .content_entries
            .or(candidate.entries)
            .unwrap_or(defaults.entries().get() as u64),
        "content.entries",
        ConfigurationValidationCode::LimitOutOfRange,
        |value| {
            if value <= defaults.entries().get() {
                ContentEntryLimit::new(value)
            } else {
                None
            }
        },
        diagnostics,
    );
    let depth = validate_usize(
        overrides
            .content_depth
            .or(candidate.depth)
            .unwrap_or(defaults.depth().get() as u64),
        "content.depth",
        ConfigurationValidationCode::LimitOutOfRange,
        |value| {
            if value <= defaults.depth().get() {
                ContentDepthLimit::new(value)
            } else {
                None
            }
        },
        diagnostics,
    );
    let path_bytes = validate_usize(
        overrides
            .content_path_bytes
            .or(candidate.path_bytes)
            .unwrap_or(defaults.path_bytes().get() as u64),
        "content.path_bytes",
        ConfigurationValidationCode::LimitOutOfRange,
        |value| {
            if value <= defaults.path_bytes().get() {
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

#[allow(clippy::too_many_arguments)]
fn select_path(
    command_line: Option<PathBuf>,
    file: Option<PathBuf>,
    working_directory: &Path,
    file_base: &Path,
    default: &Path,
    field: &'static str,
    diagnostics: &mut DiagnosticCollector,
) -> Option<PathBuf> {
    let resolved = match command_line {
        Some(path) => resolve_path(working_directory, &path),
        None => match file {
            Some(path) => resolve_path(file_base, &path),
            None => resolve_path(working_directory, default),
        },
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

fn validate_lightning(
    candidate: LightningCandidate,
    file_base: &Path,
    diagnostics: &mut DiagnosticCollector,
) -> Option<LightningConfiguration> {
    match candidate {
        LightningCandidate::Lexe {
            network,
            credentials,
            cache_path,
            max_in_flight,
            max_pending,
            response_timeout_ms,
            reconciliation_page_size,
            recovery_interval_seconds,
        } => {
            let credentials = match credentials.finalize(file_base) {
                Some(reference) => match reference.into_file() {
                    Ok(reference) => Some(reference),
                    Err(_) => {
                        diagnostics.push(host_diagnostic(
                            "lightning.credentials",
                            ConfigurationValidationCode::LexeCredentialsMustUseFile,
                            "Lexe credentials require a file secret reference",
                        ));
                        None
                    }
                },
                None => {
                    diagnostics.push(host_diagnostic(
                        "lightning.credentials",
                        ConfigurationValidationCode::SecretReferenceInvalid,
                        "secret reference syntax is invalid",
                    ));
                    None
                }
            };
            let cache_path = match cache_path {
                Some(path) => match resolve_path(file_base, &path).and_then(SensitivePath::new) {
                    Some(path) => Some(Some(path)),
                    None => {
                        diagnostics.push(host_diagnostic(
                            "lightning.cache_path",
                            ConfigurationValidationCode::PathInvalid,
                            "Lexe cache path must not be empty",
                        ));
                        None
                    }
                },
                None => Some(None),
            };
            let max_in_flight = validate_usize::<LexeInFlightLimit>(
                max_in_flight.unwrap_or(DEFAULT_LEXE_MAX_IN_FLIGHT as u64),
                "lightning.max_in_flight",
                ConfigurationValidationCode::LexeConcurrencyInvalid,
                LexeInFlightLimit::new,
                diagnostics,
            );
            let max_pending = validate_usize::<LexePendingLimit>(
                max_pending.unwrap_or(DEFAULT_LEXE_MAX_PENDING as u64),
                "lightning.max_pending",
                ConfigurationValidationCode::LimitOutOfRange,
                LexePendingLimit::new,
                diagnostics,
            );
            let response_timeout = validate_duration_milliseconds::<LexeResponseTimeout>(
                response_timeout_ms.unwrap_or(DEFAULT_LEXE_RESPONSE_TIMEOUT_MILLISECONDS),
                "lightning.response_timeout_ms",
                LexeResponseTimeout::from_milliseconds,
                diagnostics,
            );
            let reconciliation_page_size = validate_usize::<LexeReconciliationPageSize>(
                reconciliation_page_size.unwrap_or(DEFAULT_LEXE_RECONCILIATION_PAGE_SIZE as u64),
                "lightning.reconciliation_page_size",
                ConfigurationValidationCode::PageSizeInvalid,
                LexeReconciliationPageSize::new,
                diagnostics,
            );
            let recovery_interval = validate_duration_seconds::<LexeRecoveryInterval>(
                recovery_interval_seconds.unwrap_or(DEFAULT_LEXE_RECOVERY_INTERVAL_SECONDS),
                "lightning.recovery_interval_seconds",
                LexeRecoveryInterval::from_seconds,
                diagnostics,
            );

            match (
                credentials,
                cache_path,
                max_in_flight,
                max_pending,
                response_timeout,
                reconciliation_page_size,
                recovery_interval,
            ) {
                (
                    Some(credentials),
                    Some(cache_path),
                    Some(max_in_flight),
                    Some(max_pending),
                    Some(response_timeout),
                    Some(reconciliation_page_size),
                    Some(recovery_interval),
                ) => Some(LightningConfiguration::Lexe(LexeConfiguration {
                    network,
                    credentials,
                    cache_path,
                    max_in_flight,
                    max_pending,
                    response_timeout,
                    reconciliation_page_size,
                    recovery_interval,
                })),
                _ => None,
            }
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

fn validate_duration_milliseconds<Value>(
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

fn validate_duration_seconds<Value>(
    raw: u64,
    field: &'static str,
    constructor: impl FnOnce(u64) -> Option<Value>,
    diagnostics: &mut DiagnosticCollector,
) -> Option<Value> {
    validate_duration_milliseconds(raw, field, constructor, diagnostics)
}

fn host_diagnostic(
    field: impl Into<Box<str>>,
    code: ConfigurationValidationCode,
    message: &'static str,
) -> ConfigurationDiagnostic {
    ConfigurationDiagnostic::new(ConfigurationAuthority::Host, field, code, message)
}

fn default_public_bind() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PUBLIC_PORT)
}

pub(crate) fn resolve_path(base: &Path, path: &Path) -> Option<PathBuf> {
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
            .load(
                Path::new("maincopy.toml"),
                HostConfigurationOverrides::default(),
            )
            .unwrap();

        assert_eq!(config.paths().content_root(), root.path().join("content"));
        assert_eq!(config.paths().state_root(), root.path().join("state"));
        assert_eq!(config.paths().runtime_root(), root.path().join("run"));
        assert_eq!(
            config.public().bind(),
            "127.0.0.1:3000".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(config.admin().socket(), root.path().join("run/admin.sock"));
        assert_eq!(
            config.database().path(),
            root.path().join("state/maincopy.db")
        );
        assert_eq!(
            config.database().busy_timeout().get(),
            Duration::from_secs(5)
        );
        assert_eq!(config.database().writer_queue_capacity().get(), 128);
        assert_eq!(config.database().read_pool_size().get(), 4);
        assert_eq!(config.content(), ContentTreeLimits::default());
        assert!(config.lightning().is_none());
    }

    #[test]
    fn complete_content_limit_schema_and_command_line_precedence_are_typed() {
        let root = tempdir().unwrap();
        write_config(
            root.path(),
            "maincopy.toml",
            "[content]\n\
             publication_file_bytes = 1\n\
             post_file_bytes = 2\n\
             asset_file_bytes = 3\n\
             total_tree_bytes = 4\n\
             entries = 5\n\
             depth = 6\n\
             path_bytes = 7\n",
        );
        let overrides = HostConfigurationOverrides::default()
            .with_content_publication_file_bytes(10)
            .with_content_post_file_bytes(20)
            .with_content_asset_file_bytes(30)
            .with_content_total_tree_bytes(40)
            .with_content_entries(50)
            .with_content_depth(7)
            .with_content_path_bytes(70);

        let limits = loader(root.path())
            .load(Path::new("maincopy.toml"), overrides)
            .unwrap()
            .content();

        assert_eq!(limits.publication_file_bytes().get(), 10);
        assert_eq!(limits.post_file_bytes().get(), 20);
        assert_eq!(limits.asset_file_bytes().get(), 30);
        assert_eq!(limits.total_tree_bytes().get(), 40);
        assert_eq!(limits.entries().get(), 50);
        assert_eq!(limits.depth().get(), 7);
        assert_eq!(limits.path_bytes().get(), 70);
    }

    #[test]
    fn content_limit_file_values_map_without_command_line_overrides() {
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

        let limits = loader(root.path())
            .load(
                Path::new("maincopy.toml"),
                HostConfigurationOverrides::default(),
            )
            .unwrap()
            .content();

        assert_eq!(limits.publication_file_bytes().get(), 11);
        assert_eq!(limits.post_file_bytes().get(), 12);
        assert_eq!(limits.asset_file_bytes().get(), 13);
        assert_eq!(limits.total_tree_bytes().get(), 14);
        assert_eq!(limits.entries().get(), 15);
        assert_eq!(limits.depth().get(), 8);
        assert_eq!(limits.path_bytes().get(), 16);
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
                caps.publication_file_bytes().get(),
                caps.post_file_bytes().get(),
                caps.asset_file_bytes().get(),
                caps.total_tree_bytes().get(),
                caps.entries().get(),
                caps.depth().get(),
                caps.path_bytes().get(),
            ),
        );

        let limits = loader(root.path())
            .load(
                Path::new("maincopy.toml"),
                HostConfigurationOverrides::default(),
            )
            .unwrap()
            .content();

        assert_eq!(limits, caps);
    }

    #[test]
    fn every_content_limit_rejects_zero_and_one_above_its_hard_cap() {
        let root = tempdir().unwrap();
        let caps = ContentTreeLimits::default();
        let cases = [
            (
                "publication_file_bytes",
                caps.publication_file_bytes().get() + 1,
            ),
            ("post_file_bytes", caps.post_file_bytes().get() + 1),
            ("asset_file_bytes", caps.asset_file_bytes().get() + 1),
            ("total_tree_bytes", caps.total_tree_bytes().get() + 1),
            ("entries", caps.entries().get() as u64 + 1),
            ("depth", caps.depth().get() as u64 + 1),
            ("path_bytes", caps.path_bytes().get() as u64 + 1),
        ];

        for (index, (field, above_cap)) in cases.into_iter().enumerate() {
            for (boundary, value) in [("zero", 0), ("above-cap", above_cap)] {
                let name = format!("content-{boundary}-{index}.toml");
                write_config(
                    root.path(),
                    &name,
                    &format!("[content]\n{field} = {value}\n"),
                );
                let errors = loader(root.path())
                    .load(Path::new(&name), HostConfigurationOverrides::default())
                    .unwrap_err();

                assert_eq!(errors.diagnostics().len(), 1);
                let expected_field = format!("content.{field}");
                assert_eq!(errors.diagnostics()[0].field(), expected_field.as_str());
                assert_eq!(
                    errors.diagnostics()[0].code(),
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
            .load(
                Path::new("maincopy.toml"),
                HostConfigurationOverrides::default(),
            )
            .unwrap_err();

        assert_eq!(errors.diagnostics().len(), 1);
        assert_eq!(errors.diagnostics()[0].field(), "content.total_tree_bytes");
        assert_eq!(
            errors.diagnostics()[0].code(),
            ConfigurationValidationCode::ContentLimitRelationshipInvalid
        );
    }

    #[test]
    fn file_and_command_line_precedence_use_distinct_path_bases() {
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
             socket = \"file-admin.sock\"\n\
             [database]\n\
             path = \"file.db\"\n\
             busy_timeout_ms = 6000\n\
             writer_queue_capacity = 129\n\
             read_pool_size = 5\n",
        );
        let overrides = HostConfigurationOverrides::default()
            .with_content_root(PathBuf::from("cli-content"))
            .with_runtime_root(PathBuf::from("cli-run"))
            .with_public_bind("127.0.0.1:4000".parse().unwrap())
            .with_database_busy_timeout_ms(7_000)
            .with_database_writer_queue_capacity(130)
            .with_database_read_pool_size(6);
        let config = loader(root.path())
            .load(Path::new("host/maincopy.toml"), overrides)
            .unwrap();

        assert_eq!(
            config.paths().content_root(),
            root.path().join("cli-content")
        );
        assert_eq!(
            config.paths().state_root(),
            root.path().join("host/file-state")
        );
        assert_eq!(config.paths().runtime_root(), root.path().join("cli-run"));
        assert_eq!(config.public().bind().port(), 4_000);
        assert_eq!(
            config.admin().socket(),
            root.path().join("host/file-admin.sock")
        );
        assert_eq!(config.database().path(), root.path().join("host/file.db"));
        assert_eq!(
            config.database().busy_timeout().get(),
            Duration::from_secs(7)
        );
        assert_eq!(config.database().writer_queue_capacity().get(), 130);
        assert_eq!(config.database().read_pool_size().get(), 6);
    }

    #[test]
    fn derived_defaults_follow_effective_cli_roots() {
        let root = tempdir().unwrap();
        write_config(root.path(), "host/maincopy.toml", "");
        let overrides = HostConfigurationOverrides::default()
            .with_state_root(PathBuf::from("cli-state"))
            .with_runtime_root(PathBuf::from("cli-runtime"));
        let config = loader(root.path())
            .load(Path::new("host/maincopy.toml"), overrides)
            .unwrap();

        assert_eq!(config.paths().content_root(), root.path().join("content"));
        assert_eq!(
            config.database().path(),
            root.path().join("cli-state/maincopy.db")
        );
        assert_eq!(
            config.admin().socket(),
            root.path().join("cli-runtime/admin.sock")
        );
    }

    #[test]
    fn lexe_requires_network_file_credentials_and_locks_defaults() {
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
        let config = loader(root.path())
            .load(
                Path::new("host/maincopy.toml"),
                HostConfigurationOverrides::default(),
            )
            .unwrap();
        let lexe = config.lightning().unwrap().lexe();

        assert_eq!(lexe.network(), LexeNetwork::Mainnet);
        assert_eq!(
            lexe.credentials().path(),
            root.path().join("host/secret/credential.json")
        );
        assert_eq!(
            lexe.cache_path().unwrap().path(),
            root.path().join("host/private/cache")
        );
        assert_eq!(lexe.max_in_flight().get(), 4);
        assert_eq!(lexe.max_pending().get(), 64);
        assert_eq!(lexe.response_timeout().get(), Duration::from_secs(15));
        assert_eq!(lexe.reconciliation_page_size().get(), 100);
        assert_eq!(lexe.recovery_interval().get(), Duration::from_secs(60));

        let diagnostic_view = format!("{config:?}");
        for protected in ["credential.json", "private/cache"] {
            assert!(!diagnostic_view.contains(protected));
        }
        assert!(diagnostic_view.contains("SecretFileReference(<redacted>)"));
        assert!(diagnostic_view.contains("SensitivePath(<redacted>)"));
    }

    #[test]
    fn complete_documented_lexe_schema_uses_explicit_units() {
        let root = tempdir().unwrap();
        write_config(
            root.path(),
            "host/maincopy.toml",
            "[lightning]\n\
             provider = \"lexe\"\n\
             network = \"testnet4\"\n\
             credentials = { source = \"file\", path = \"secret/credential.json\" }\n\
             cache_path = \"private/cache\"\n\
             max_in_flight = 6\n\
             max_pending = 96\n\
             response_timeout_ms = 17000\n\
             reconciliation_page_size = 250\n\
             recovery_interval_seconds = 90\n",
        );
        let config = loader(root.path())
            .load(
                Path::new("host/maincopy.toml"),
                HostConfigurationOverrides::default(),
            )
            .unwrap();
        let lexe = config.lightning().unwrap().lexe();

        assert_eq!(lexe.network(), LexeNetwork::Testnet4);
        assert_eq!(lexe.max_in_flight().get(), 6);
        assert_eq!(lexe.max_pending().get(), 96);
        assert_eq!(lexe.response_timeout().get(), Duration::from_millis(17_000));
        assert_eq!(lexe.reconciliation_page_size().get(), 250);
        assert_eq!(lexe.recovery_interval().get(), Duration::from_secs(90));
    }

    #[test]
    fn lexe_network_and_credentials_are_required() {
        let root = tempdir().unwrap();
        for (index, source) in [
            "[lightning]\nprovider = \"lexe\"\ncredentials = { source = \"file\", path = \"credential\" }\n",
            "[lightning]\nprovider = \"lexe\"\nnetwork = \"mainnet\"\n",
        ]
        .into_iter()
        .enumerate()
        {
            let name = format!("missing-{index}.toml");
            write_config(root.path(), &name, source);
            let errors = loader(root.path())
                .load(Path::new(&name), HostConfigurationOverrides::default())
                .unwrap_err();
            assert_eq!(
                errors.diagnostics()[0].code(),
                ConfigurationValidationCode::HostTomlInvalid
            );
        }
    }

    #[test]
    fn lexe_network_wire_names_are_stable() {
        let cases = [
            (LexeNetwork::Mainnet, "mainnet"),
            (LexeNetwork::Testnet3, "testnet3"),
            (LexeNetwork::Testnet4, "testnet4"),
            (LexeNetwork::Signet, "signet"),
            (LexeNetwork::Regtest, "regtest"),
        ];
        for (network, expected) in cases {
            assert_eq!(serde_json::to_value(network).unwrap(), expected);
        }
    }

    #[test]
    fn environment_lexe_credentials_are_rejected_without_disclosure() {
        let root = tempdir().unwrap();
        write_config(
            root.path(),
            "maincopy.toml",
            "[lightning]\n\
             provider = \"lexe\"\n\
             network = \"mainnet\"\n\
             credentials = { source = \"environment\", variable = \"TOP_SECRET_VARIABLE\" }\n",
        );
        let errors = loader(root.path())
            .load(
                Path::new("maincopy.toml"),
                HostConfigurationOverrides::default(),
            )
            .unwrap_err();

        assert_eq!(
            errors.diagnostics()[0].code(),
            ConfigurationValidationCode::LexeCredentialsMustUseFile
        );
        assert_eq!(
            errors.to_string(),
            "configuration validation failed; \
             host:lightning.credentials:lexe_credentials_must_use_file: \
             Lexe credentials require a file secret reference"
        );
        let rendered = format!("{errors:?}");
        assert!(!rendered.contains("TOP_SECRET_VARIABLE"));
        assert!(!rendered.contains(root.path().to_string_lossy().as_ref()));
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
            "[lightning]\nprovider = \"lexe\"\nnetwork = \"mainnet\"\ncredentials = { source = \"file\", path = \"secret\" }\nunknown = true\n",
            "[lightning]\nprovider = \"lexe\"\nnetwork = \"mainnet\"\ncredentials = { source = \"file\", path = \"secret\", unknown = true }\n",
        ];

        for (index, source) in cases.into_iter().enumerate() {
            let name = format!("case-{index}.toml");
            write_config(root.path(), &name, source);
            let errors = loader(root.path())
                .load(Path::new(&name), HostConfigurationOverrides::default())
                .unwrap_err();
            assert_eq!(
                errors.diagnostics()[0].code(),
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
            let errors = loader(root.path())
                .load(Path::new(&name), HostConfigurationOverrides::default())
                .unwrap_err();

            assert_eq!(
                errors.diagnostics()[0].code(),
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
             read_pool_size = 0\n\
             [lightning]\n\
             provider = \"lexe\"\n\
             network = \"regtest\"\n\
             credentials = { source = \"file\", path = \"credential\" }\n\
             max_in_flight = 1\n\
             max_pending = 0\n\
             response_timeout_ms = 0\n\
             reconciliation_page_size = 1001\n\
             recovery_interval_seconds = 0\n",
        );
        let errors = loader(root.path())
            .load(
                Path::new("maincopy.toml"),
                HostConfigurationOverrides::default(),
            )
            .unwrap_err();
        let codes = errors
            .diagnostics()
            .iter()
            .map(ConfigurationDiagnostic::code)
            .collect::<Vec<_>>();

        assert!(codes.contains(&ConfigurationValidationCode::LexeConcurrencyInvalid));
        assert!(codes.contains(&ConfigurationValidationCode::LimitOutOfRange));
        assert!(codes.contains(&ConfigurationValidationCode::DurationInvalid));
        assert!(codes.contains(&ConfigurationValidationCode::PageSizeInvalid));
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
        assert!(LexeInFlightLimit::new(MAX_LEXE_IN_FLIGHT).is_some());
        assert!(LexeInFlightLimit::new(MAX_LEXE_IN_FLIGHT + 1).is_none());
        assert!(LexePendingLimit::new(MAX_LEXE_PENDING).is_some());
        assert!(LexePendingLimit::new(MAX_LEXE_PENDING + 1).is_none());
        assert!(
            LexeResponseTimeout::from_milliseconds(MAX_LEXE_RESPONSE_TIMEOUT_MILLISECONDS)
                .is_some()
        );
        assert!(
            LexeResponseTimeout::from_milliseconds(MAX_LEXE_RESPONSE_TIMEOUT_MILLISECONDS + 1)
                .is_none()
        );
        assert!(LexeReconciliationPageSize::new(MAX_LEXE_RECONCILIATION_PAGE_SIZE).is_some());
        assert!(LexeReconciliationPageSize::new(MAX_LEXE_RECONCILIATION_PAGE_SIZE + 1).is_none());
        assert!(LexeRecoveryInterval::from_seconds(MAX_LEXE_RECOVERY_INTERVAL_SECONDS).is_some());
        assert!(
            LexeRecoveryInterval::from_seconds(MAX_LEXE_RECOVERY_INTERVAL_SECONDS + 1).is_none()
        );
    }

    #[test]
    fn host_loader_requires_an_absolute_injected_working_directory() {
        let errors = HostConfigurationLoader::new(PathBuf::from("relative")).unwrap_err();

        assert_eq!(
            errors.diagnostics()[0].code(),
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
            .load(
                Path::new("too-large.toml"),
                HostConfigurationOverrides::default(),
            )
            .unwrap_err();
        let not_utf8 = loader(root.path())
            .load(
                Path::new("not-utf8.toml"),
                HostConfigurationOverrides::default(),
            )
            .unwrap_err();

        assert_eq!(
            too_large.diagnostics()[0].code(),
            ConfigurationValidationCode::HostDocumentTooLarge
        );
        assert_eq!(
            not_utf8.diagnostics()[0].code(),
            ConfigurationValidationCode::HostTextInvalidUtf8
        );
    }
}
