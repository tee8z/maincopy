//! Typed host configuration and redacted secret references.

mod diagnostic;
mod host;
pub(crate) mod secret;

pub use diagnostic::{ConfigurationDiagnostic, ConfigurationErrors, ConfigurationValidationCode};
#[cfg(test)]
pub(crate) use host::{DatabaseBusyTimeout, DatabaseReadPoolSize, DatabaseWriterQueueCapacity};
pub(crate) use host::{
    DatabaseConfigurationView, GitProcessLimits, HostConfiguration, HostConfigurationLoader,
    SourceConfigurationView, SshCredentialReference,
};
pub use secret::{SecretFileReference, SensitivePath};
