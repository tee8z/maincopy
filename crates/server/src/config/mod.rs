//! Typed host configuration and redacted secret references.

mod diagnostic;
mod host;
pub(crate) mod secret;

pub use diagnostic::{ConfigurationDiagnostic, ConfigurationErrors, ConfigurationValidationCode};
pub(crate) use host::{
    BackupStatusConfigurationView, DatabaseConfigurationView, GitProcessLimits, HostConfiguration,
    HostConfigurationLoader, IdentityStartupBootstrap, SourceConfigurationView,
    SshCredentialReference,
};
#[cfg(test)]
pub(crate) use host::{DatabaseBusyTimeout, DatabaseReadPoolSize, DatabaseWriterQueueCapacity};
pub use secret::{SecretFileReference, SensitivePath};
