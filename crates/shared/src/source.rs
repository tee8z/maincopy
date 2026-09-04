//! Validated managed-source values and versioned source-sync wire contracts.

use std::{fmt, net::Ipv4Addr, num::NonZeroU16, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, de};
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

pub const SOURCE_PATH: &str = "/api/admin/v1/source";
pub const SOURCE_SYNCS_PATH: &str = "/api/admin/v1/source-syncs";

const MIN_SOURCE_POLL_INTERVAL_SECONDS: u64 = 30;
const MAX_SOURCE_POLL_INTERVAL_SECONDS: u64 = 24 * 60 * 60;
pub const GIT_SHA1_SOURCE_COMMIT_PREFIX: &str = "git-sha1:";
pub const GIT_SHA256_SOURCE_COMMIT_PREFIX: &str = "git-sha256:";
const SOURCE_CONTENT_DIGEST_PREFIX: &str = "content-b3-v1-";

const MAX_SSH_USER_BYTES: usize = 64;
const MAX_SSH_HOST_BYTES: usize = 253;
const MAX_REPOSITORY_PATH_BYTES: usize = 1_024;
const MAX_BRANCH_BYTES: usize = 255;
const MAX_CONTENT_SUBDIRECTORY_BYTES: usize = 1_024;
const MAX_CREDENTIAL_NAME_BYTES: usize = 64;
const MAX_SOURCE_VERSION: u64 = i64::MAX as u64;

macro_rules! uuid_identifier {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
        )]
        #[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
        #[cfg_attr(feature = "schema", schema(value_type = Uuid, format = Uuid))]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

uuid_identifier!(SourceSyncId);

macro_rules! source_string {
    ($name:ident, $maximum:expr, $validator:expr, $message:literal) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
        #[cfg_attr(feature = "schema", schema(value_type = String))]
        pub struct $name(Box<str>);

        impl $name {
            pub fn parse(value: &str) -> Result<Self, SourceValueError> {
                if value.is_empty() || value.len() > $maximum || !($validator)(value) {
                    return Err(SourceValueError($message));
                }
                Ok(Self(value.into()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = SourceValueError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl Serialize for $name {
            fn serialize<SerializerType>(
                &self,
                serializer: SerializerType,
            ) -> Result<SerializerType::Ok, SerializerType::Error>
            where
                SerializerType: serde::Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<DeserializerType>(
                deserializer: DeserializerType,
            ) -> Result<Self, DeserializerType::Error>
            where
                DeserializerType: Deserializer<'de>,
            {
                let value = Box::<str>::deserialize(deserializer)?;
                Self::parse(&value).map_err(de::Error::custom)
            }
        }
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceValueError(&'static str);

impl fmt::Display for SourceValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for SourceValueError {}

source_string!(
    SshRemoteUser,
    MAX_SSH_USER_BYTES,
    |value: &str| !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') }),
    "SSH user must contain only ASCII letters, digits, hyphens, underscores, and periods"
);

source_string!(
    SshRepositoryPath,
    MAX_REPOSITORY_PATH_BYTES,
    valid_repository_path,
    "SSH repository path must be a canonical path without credentials, traversal, or controls"
);

source_string!(
    GitBranchName,
    MAX_BRANCH_BYTES,
    valid_branch,
    "Git branch must be one exact canonical branch name"
);

source_string!(
    RepositoryContentSubdirectory,
    MAX_CONTENT_SUBDIRECTORY_BYTES,
    valid_content_subdirectory,
    "repository content subdirectory must be a portable relative path or a single period"
);

source_string!(
    SshCredentialName,
    MAX_CREDENTIAL_NAME_BYTES,
    |value: &str| {
        value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'-' | b'_'))
        })
    },
    "SSH credential name must start with a lowercase letter or digit and use lowercase ASCII"
);

/// A canonical IPv4 address or unambiguous lowercase DNS name used only as a
/// structured SSH endpoint.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "schema", schema(value_type = String))]
pub struct SshRemoteHost(Box<str>);

impl SshRemoteHost {
    pub fn parse(value: &str) -> Result<Self, SourceValueError> {
        if value.is_empty() || value.len() > MAX_SSH_HOST_BYTES || !valid_host(value) {
            return Err(SourceValueError(
                "SSH host must be one canonical IPv4 address or unambiguous lowercase DNS name",
            ));
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SshRemoteHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SshRemoteHost {
    type Err = SourceValueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for SshRemoteHost {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SshRemoteHost {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        let value = Box::<str>::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

/// A nonzero TCP port for the structured SSH endpoint.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "schema", schema(value_type = u16))]
#[serde(transparent)]
pub struct SshRemotePort(NonZeroU16);

impl SshRemotePort {
    pub const fn new(value: u16) -> Option<Self> {
        match NonZeroU16::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

impl<'de> Deserialize<'de> for SshRemotePort {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        let value = u16::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| de::Error::custom("SSH port must be nonzero"))
    }
}

/// A positive source-configuration version representable by SQLite.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "schema", schema(value_type = u64))]
#[serde(transparent)]
pub struct SourceConfigurationVersion(u64);

impl SourceConfigurationVersion {
    pub const fn new(value: u64) -> Option<Self> {
        if value > 0 && value <= MAX_SOURCE_VERSION {
            Some(Self(value))
        } else {
            None
        }
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for SourceConfigurationVersion {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        Self::new(value)
            .ok_or_else(|| de::Error::custom("source configuration version must be positive"))
    }
}

/// A bounded whole-second managed-source poll interval.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "schema", schema(value_type = u64))]
#[serde(transparent)]
pub struct SourcePollInterval(u64);

impl SourcePollInterval {
    pub const fn from_seconds(value: u64) -> Option<Self> {
        if value >= MIN_SOURCE_POLL_INTERVAL_SECONDS && value <= MAX_SOURCE_POLL_INTERVAL_SECONDS {
            Some(Self(value))
        } else {
            None
        }
    }

    pub const fn seconds(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for SourcePollInterval {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        Self::from_seconds(value).ok_or_else(|| {
            de::Error::custom(format_args!(
                "source poll interval must be between {MIN_SOURCE_POLL_INTERVAL_SECONDS} and {MAX_SOURCE_POLL_INTERVAL_SECONDS} seconds"
            ))
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct SshRemote {
    pub user: SshRemoteUser,
    pub host: SshRemoteHost,
    pub port: SshRemotePort,
    pub repository_path: SshRepositoryPath,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ManagedSourceConfiguration {
    pub remote: SshRemote,
    pub branch: GitBranchName,
    pub content_subdirectory: RepositoryContentSubdirectory,
    pub credential_name: SshCredentialName,
    pub poll_interval_seconds: SourcePollInterval,
    pub version: SourceConfigurationVersion,
    #[serde(
        serialize_with = "time::serde::rfc3339::serialize",
        deserialize_with = "deserialize_utc_timestamp"
    )]
    #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
    pub updated_at: OffsetDateTime,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SourceSyncRequestOrigin {
    Startup,
    Poll,
    Manual,
}

impl SourceSyncRequestOrigin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Poll => "poll",
            Self::Manual => "manual",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "startup" => Some(Self::Startup),
            "poll" => Some(Self::Poll),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SourceSyncStage {
    Queued,
    Fetching,
    ResolvingCommit,
    PreparingCandidate,
    Compiling,
    Reloading,
}

impl SourceSyncStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Fetching => "fetching",
            Self::ResolvingCommit => "resolving_commit",
            Self::PreparingCandidate => "preparing_candidate",
            Self::Compiling => "compiling",
            Self::Reloading => "reloading",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "fetching" => Some(Self::Fetching),
            "resolving_commit" => Some(Self::ResolvingCommit),
            "preparing_candidate" => Some(Self::PreparingCandidate),
            "compiling" => Some(Self::Compiling),
            "reloading" => Some(Self::Reloading),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SourceSyncOutcome {
    Applied,
    NoChange,
    Failed,
    Cancelled,
}

impl SourceSyncOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::NoChange => "no_change",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "applied" => Some(Self::Applied),
            "no_change" => Some(Self::NoChange),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SourceSyncFailureCode {
    ConfigurationChanged,
    CredentialUnavailable,
    UnknownHost,
    AuthenticationFailed,
    RemoteUnavailable,
    BranchUnavailable,
    FetchFailed,
    CommitInvalid,
    CandidateFailed,
    ValidationFailed,
    CompileFailed,
    ReloadFailed,
    TimedOut,
    Interrupted,
    Internal,
}

impl SourceSyncFailureCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConfigurationChanged => "configuration_changed",
            Self::CredentialUnavailable => "credential_unavailable",
            Self::UnknownHost => "unknown_host",
            Self::AuthenticationFailed => "authentication_failed",
            Self::RemoteUnavailable => "remote_unavailable",
            Self::BranchUnavailable => "branch_unavailable",
            Self::FetchFailed => "fetch_failed",
            Self::CommitInvalid => "commit_invalid",
            Self::CandidateFailed => "candidate_failed",
            Self::ValidationFailed => "validation_failed",
            Self::CompileFailed => "compile_failed",
            Self::ReloadFailed => "reload_failed",
            Self::TimedOut => "timed_out",
            Self::Interrupted => "interrupted",
            Self::Internal => "internal",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "configuration_changed" => Some(Self::ConfigurationChanged),
            "credential_unavailable" => Some(Self::CredentialUnavailable),
            "unknown_host" => Some(Self::UnknownHost),
            "authentication_failed" => Some(Self::AuthenticationFailed),
            "remote_unavailable" => Some(Self::RemoteUnavailable),
            "branch_unavailable" => Some(Self::BranchUnavailable),
            "fetch_failed" => Some(Self::FetchFailed),
            "commit_invalid" => Some(Self::CommitInvalid),
            "candidate_failed" => Some(Self::CandidateFailed),
            "validation_failed" => Some(Self::ValidationFailed),
            "compile_failed" => Some(Self::CompileFailed),
            "reload_failed" => Some(Self::ReloadFailed),
            "timed_out" => Some(Self::TimedOut),
            "interrupted" => Some(Self::Interrupted),
            "internal" => Some(Self::Internal),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SourceSyncAdmission {
    Created,
    Coalesced,
    Replayed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct SourceSyncResource {
    pub source_sync_id: SourceSyncId,
    pub configuration_version: SourceConfigurationVersion,
    pub request_origin: SourceSyncRequestOrigin,
    pub stage: SourceSyncStage,
    pub outcome: Option<SourceSyncOutcome>,
    pub source_commit: Option<Box<str>>,
    pub content_digest: Option<Box<str>>,
    pub failure_code: Option<SourceSyncFailureCode>,
    pub version: u64,
    #[serde(serialize_with = "time::serde::rfc3339::serialize")]
    #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
    pub requested_at: OffsetDateTime,
    #[serde(serialize_with = "time::serde::rfc3339::serialize")]
    #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
    pub updated_at: OffsetDateTime,
    #[serde(serialize_with = "time::serde::rfc3339::option::serialize")]
    #[cfg_attr(feature = "schema", schema(value_type = Option<String>, format = DateTime))]
    pub finished_at: Option<OffsetDateTime>,
}

#[derive(Deserialize)]
struct SourceSyncWire {
    source_sync_id: SourceSyncId,
    configuration_version: SourceConfigurationVersion,
    request_origin: SourceSyncRequestOrigin,
    stage: SourceSyncStage,
    outcome: Option<SourceSyncOutcome>,
    #[serde(default, deserialize_with = "deserialize_optional_source_commit")]
    source_commit: Option<Box<str>>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_source_content_digest"
    )]
    content_digest: Option<Box<str>>,
    failure_code: Option<SourceSyncFailureCode>,
    version: u64,
    #[serde(deserialize_with = "deserialize_utc_timestamp")]
    requested_at: OffsetDateTime,
    #[serde(deserialize_with = "deserialize_utc_timestamp")]
    updated_at: OffsetDateTime,
    #[serde(default, deserialize_with = "deserialize_optional_utc_timestamp")]
    finished_at: Option<OffsetDateTime>,
}

impl<'de> Deserialize<'de> for SourceSyncResource {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        let wire = SourceSyncWire::deserialize(deserializer)?;
        let resource = Self {
            source_sync_id: wire.source_sync_id,
            configuration_version: wire.configuration_version,
            request_origin: wire.request_origin,
            stage: wire.stage,
            outcome: wire.outcome,
            source_commit: wire.source_commit,
            content_digest: wire.content_digest,
            failure_code: wire.failure_code,
            version: wire.version,
            requested_at: wire.requested_at,
            updated_at: wire.updated_at,
            finished_at: wire.finished_at,
        };
        validate_source_sync_resource(&resource).map_err(de::Error::custom)?;
        Ok(resource)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceWireDecodeError(&'static str);

impl fmt::Display for SourceWireDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for SourceWireDecodeError {}

fn validate_source_sync_resource(
    resource: &SourceSyncResource,
) -> Result<(), SourceWireDecodeError> {
    if resource.version == 0 || resource.version > MAX_SOURCE_VERSION {
        return Err(SourceWireDecodeError(
            "source synchronization version must be a positive SQLite integer",
        ));
    }
    if resource.updated_at < resource.requested_at
        || resource
            .finished_at
            .is_some_and(|finished_at| finished_at < resource.updated_at)
    {
        return Err(SourceWireDecodeError(
            "source synchronization timestamps must be monotonic",
        ));
    }
    validate_source_sync_shape(resource)
}

fn validate_source_sync_shape(resource: &SourceSyncResource) -> Result<(), SourceWireDecodeError> {
    if resource.outcome.is_some() != resource.finished_at.is_some()
        || (resource.outcome == Some(SourceSyncOutcome::Failed)) != resource.failure_code.is_some()
    {
        return Err(SourceWireDecodeError(
            "source synchronization terminal metadata is inconsistent",
        ));
    }
    match resource.outcome {
        Some(SourceSyncOutcome::Applied) => {
            if resource.stage != SourceSyncStage::Reloading
                || resource.source_commit.is_none()
                || resource.content_digest.is_none()
            {
                return Err(SourceWireDecodeError(
                    "applied source synchronization has impossible provenance",
                ));
            }
        }
        Some(SourceSyncOutcome::NoChange) => {
            if resource.stage != SourceSyncStage::ResolvingCommit
                || resource.source_commit.is_none()
                || resource.content_digest.is_none()
            {
                return Err(SourceWireDecodeError(
                    "unchanged source synchronization has impossible provenance",
                ));
            }
        }
        Some(SourceSyncOutcome::Failed | SourceSyncOutcome::Cancelled) => {}
        None => validate_active_source_sync_provenance(resource)?,
    }
    Ok(())
}

fn validate_active_source_sync_provenance(
    resource: &SourceSyncResource,
) -> Result<(), SourceWireDecodeError> {
    let has_commit = resource.source_commit.is_some();
    let has_digest = resource.content_digest.is_some();
    let valid = match resource.stage {
        SourceSyncStage::Queued | SourceSyncStage::Fetching | SourceSyncStage::ResolvingCommit => {
            !has_commit && !has_digest
        }
        SourceSyncStage::PreparingCandidate | SourceSyncStage::Compiling => {
            has_commit && !has_digest
        }
        SourceSyncStage::Reloading => has_commit && has_digest,
    };
    if valid {
        Ok(())
    } else {
        Err(SourceWireDecodeError(
            "active source synchronization has impossible provenance",
        ))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct BeginSourceSyncResponse {
    pub admission: SourceSyncAdmission,
    pub sync: SourceSyncResource,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ListSourceSyncsResponse {
    pub syncs: Vec<SourceSyncResource>,
    pub next_cursor: Option<SourceSyncId>,
}

/// Current source mode and its non-secret runtime state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SourceStatusResponse {
    ExternalCheckout,
    ManagedGit {
        configuration: Box<ManagedSourceConfiguration>,
        installed_commit: Option<Box<str>>,
        content_digest: Option<Box<str>>,
        active_sync: Option<Box<SourceSyncResource>>,
        latest_sync: Option<Box<SourceSyncResource>>,
        #[serde(serialize_with = "time::serde::rfc3339::option::serialize")]
        #[cfg_attr(feature = "schema", schema(value_type = Option<String>, format = DateTime))]
        next_poll_at: Option<OffsetDateTime>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum SourceStatusWire {
    ExternalCheckout,
    ManagedGit {
        configuration: Box<ManagedSourceConfiguration>,
        #[serde(default, deserialize_with = "deserialize_optional_source_commit")]
        installed_commit: Option<Box<str>>,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_source_content_digest"
        )]
        content_digest: Option<Box<str>>,
        active_sync: Option<Box<SourceSyncResource>>,
        latest_sync: Option<Box<SourceSyncResource>>,
        #[serde(default, deserialize_with = "deserialize_optional_utc_timestamp")]
        next_poll_at: Option<OffsetDateTime>,
    },
}

impl<'de> Deserialize<'de> for SourceStatusResponse {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        match SourceStatusWire::deserialize(deserializer)? {
            SourceStatusWire::ExternalCheckout => Ok(Self::ExternalCheckout),
            SourceStatusWire::ManagedGit {
                configuration,
                installed_commit,
                content_digest,
                active_sync,
                latest_sync,
                next_poll_at,
            } => {
                if installed_commit.is_some() != content_digest.is_some() {
                    return Err(de::Error::custom(SourceWireDecodeError(
                        "installed source commit and content digest must appear together",
                    )));
                }
                if active_sync
                    .as_deref()
                    .is_some_and(|sync| sync.outcome.is_some())
                {
                    return Err(de::Error::custom(SourceWireDecodeError(
                        "active source synchronization must be non-terminal",
                    )));
                }
                Ok(Self::ManagedGit {
                    configuration,
                    installed_commit,
                    content_digest,
                    active_sync,
                    latest_sync,
                    next_poll_at,
                })
            }
        }
    }
}

/// Validates the complete, algorithm-qualified Git identity used on the wire.
pub fn valid_source_commit(value: &str) -> bool {
    [
        (GIT_SHA1_SOURCE_COMMIT_PREFIX, 40),
        (GIT_SHA256_SOURCE_COMMIT_PREFIX, 64),
    ]
    .into_iter()
    .any(|(prefix, length)| {
        value.strip_prefix(prefix).is_some_and(|hex| {
            hex.len() == length
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    })
}

pub fn valid_source_content_digest(value: &str) -> bool {
    value
        .strip_prefix(SOURCE_CONTENT_DIGEST_PREFIX)
        .is_some_and(valid_64_byte_lowercase_hex)
}

fn valid_64_byte_lowercase_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn deserialize_optional_source_commit<'de, DeserializerType>(
    deserializer: DeserializerType,
) -> Result<Option<Box<str>>, DeserializerType::Error>
where
    DeserializerType: Deserializer<'de>,
{
    let value = Option::<Box<str>>::deserialize(deserializer)?;
    if value
        .as_deref()
        .is_some_and(|commit| !valid_source_commit(commit))
    {
        Err(de::Error::custom(
            "source commit must be one complete algorithm-qualified Git identity",
        ))
    } else {
        Ok(value)
    }
}

fn deserialize_optional_source_content_digest<'de, DeserializerType>(
    deserializer: DeserializerType,
) -> Result<Option<Box<str>>, DeserializerType::Error>
where
    DeserializerType: Deserializer<'de>,
{
    let value = Option::<Box<str>>::deserialize(deserializer)?;
    if value
        .as_deref()
        .is_some_and(|digest| !valid_source_content_digest(digest))
    {
        Err(de::Error::custom(
            "source content digest must use the complete content-b3-v1 encoding",
        ))
    } else {
        Ok(value)
    }
}

fn valid_host(value: &str) -> bool {
    if let Ok(address) = value.parse::<Ipv4Addr>() {
        return address.to_string() == value;
    }
    valid_dns_host(value) && !is_legacy_ipv4(value)
}

fn valid_dns_host(value: &str) -> bool {
    !value.ends_with('.')
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

// OpenSSH accepts legacy inet_aton-style numbers. Do not reinterpret one of
// those spellings as a DNS name after Rust's strict IPv4 parser rejects it.
fn is_legacy_ipv4(value: &str) -> bool {
    let mut components = [0_u64; 4];
    let mut count = 0;
    for component in value.split('.') {
        if count == components.len() {
            return false;
        }
        let Some(component) = parse_legacy_ipv4_component(component) else {
            return false;
        };
        components[count] = component;
        count += 1;
    }

    match count {
        1 => components[0] <= u32::MAX.into(),
        2 => components[0] <= u8::MAX.into() && components[1] <= 0x00ff_ffff,
        3 => {
            components[0] <= u8::MAX.into()
                && components[1] <= u8::MAX.into()
                && components[2] <= u16::MAX.into()
        }
        4 => components
            .iter()
            .all(|component| *component <= u8::MAX.into()),
        _ => false,
    }
}

fn parse_legacy_ipv4_component(value: &str) -> Option<u64> {
    let (digits, radix) = if let Some(hexadecimal) = value.strip_prefix("0x") {
        (hexadecimal, 16)
    } else if value.len() > 1 && value.starts_with('0') {
        (value, 8)
    } else {
        (value, 10)
    };
    if digits.is_empty() {
        None
    } else {
        u64::from_str_radix(digits, radix).ok()
    }
}

fn valid_repository_path(value: &str) -> bool {
    let path = value.strip_prefix('/').unwrap_or(value);
    !path.is_empty()
        && !path.starts_with('-')
        && !value.contains("//")
        && path.split('/').all(valid_path_segment)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
}

fn valid_content_subdirectory(value: &str) -> bool {
    value == "."
        || (!value.starts_with('-')
            && !value.starts_with('/')
            && !value.ends_with('/')
            && !value.contains("//")
            && value.split('/').all(valid_path_segment)
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.')
            }))
}

fn valid_path_segment(segment: &str) -> bool {
    !segment.is_empty() && !matches!(segment, "." | "..")
}

fn valid_branch(value: &str) -> bool {
    !value.starts_with('-')
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.ends_with('.')
        && !value.ends_with(".lock")
        && value != "@"
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains("//")
        && value.split('/').all(|segment| {
            valid_path_segment(segment) && !segment.starts_with('.') && !segment.ends_with('.')
        })
        && value.bytes().all(|byte| {
            !byte.is_ascii_control()
                && !matches!(byte, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
}

fn deserialize_utc_timestamp<'de, DeserializerType>(
    deserializer: DeserializerType,
) -> Result<OffsetDateTime, DeserializerType::Error>
where
    DeserializerType: Deserializer<'de>,
{
    let timestamp = time::serde::rfc3339::deserialize(deserializer)?;
    if timestamp.offset() == UtcOffset::UTC {
        Ok(timestamp)
    } else {
        Err(de::Error::custom(
            "source timestamp must use the UTC offset",
        ))
    }
}

fn deserialize_optional_utc_timestamp<'de, DeserializerType>(
    deserializer: DeserializerType,
) -> Result<Option<OffsetDateTime>, DeserializerType::Error>
where
    DeserializerType: Deserializer<'de>,
{
    let timestamp = time::serde::rfc3339::option::deserialize(deserializer)?;
    match timestamp {
        Some(timestamp) if timestamp.offset() != UtcOffset::UTC => Err(de::Error::custom(
            "source timestamp must use the UTC offset",
        )),
        timestamp => Ok(timestamp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote() -> SshRemote {
        SshRemote {
            user: SshRemoteUser::parse("git").unwrap(),
            host: SshRemoteHost::parse("git.example.test").unwrap(),
            port: SshRemotePort::new(22).unwrap(),
            repository_path: SshRepositoryPath::parse("publisher/site.git").unwrap(),
        }
    }

    #[test]
    fn structured_remote_rejects_embedded_credentials_and_ambiguous_fields() {
        for invalid in ["git@forge", "git:user", "git user", "-o", ""] {
            assert!(
                SshRemoteUser::parse(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        for invalid in ["User@host", "EXAMPLE.test", "example.test.", "-host", ""] {
            assert!(
                SshRemoteHost::parse(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        for invalid in [
            "../site.git",
            "org//site.git",
            "-upload-pack",
            "~/site.git",
            "~git/site.git",
            "",
        ] {
            assert!(
                SshRepositoryPath::parse(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        assert!(SshRemotePort::new(0).is_none());
        assert_eq!(serde_json::to_value(remote()).unwrap()["port"], 22);
    }

    #[test]
    fn ssh_host_accepts_canonical_ipv4_and_dns_but_rejects_legacy_ipv4_text() {
        for valid in [
            "127.0.0.1",
            "203.0.113.42",
            "git.example.test",
            "git-1.example.test",
        ] {
            assert!(SshRemoteHost::parse(valid).is_ok(), "rejected {valid:?}");
        }
        for invalid in ["127.1", "2130706433", "0177.0.0.1", "0x7f000001"] {
            assert!(
                SshRemoteHost::parse(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn branch_subdirectory_credential_and_poll_values_are_bounded() {
        for branch in ["main", "release/v1"] {
            assert!(GitBranchName::parse(branch).is_ok());
        }
        for branch in ["-main", "refs/heads/main.lock", "topic..next", "topic@{1}"] {
            assert!(GitBranchName::parse(branch).is_err(), "accepted {branch:?}");
        }
        for path in [".", "publication", "sites/main"] {
            assert!(RepositoryContentSubdirectory::parse(path).is_ok());
        }
        for path in ["/publication", "../publication", "sites//main"] {
            assert!(RepositoryContentSubdirectory::parse(path).is_err());
        }
        assert!(SshCredentialName::parse("deploy-key-1").is_ok());
        assert!(SshCredentialName::parse("Deploy Key").is_err());
        assert!(SourcePollInterval::from_seconds(MIN_SOURCE_POLL_INTERVAL_SECONDS).is_some());
        assert!(SourcePollInterval::from_seconds(MAX_SOURCE_POLL_INTERVAL_SECONDS).is_some());
        assert!(SourcePollInterval::from_seconds(MIN_SOURCE_POLL_INTERVAL_SECONDS - 1).is_none());
        assert!(SourcePollInterval::from_seconds(MAX_SOURCE_POLL_INTERVAL_SECONDS + 1).is_none());
    }

    #[test]
    fn source_commit_wire_identity_requires_an_algorithm_and_exact_lowercase_hex() {
        assert!(valid_source_commit(&format!(
            "{GIT_SHA1_SOURCE_COMMIT_PREFIX}{}",
            "ab".repeat(20)
        )));
        assert!(valid_source_commit(&format!(
            "{GIT_SHA256_SOURCE_COMMIT_PREFIX}{}",
            "cd".repeat(32)
        )));
        for invalid in [
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "git-sha1:aaaaaaaa",
            "git-sha1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "git-sha512:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(!valid_source_commit(invalid), "accepted {invalid}");
        }
    }
}
