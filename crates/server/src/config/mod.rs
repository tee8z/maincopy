//! Typed host configuration and redacted secret references.

mod diagnostic;
mod host;
pub(crate) mod secret;

pub use diagnostic::{ConfigurationDiagnostic, ConfigurationErrors, ConfigurationValidationCode};
pub(crate) use host::HostConfigurationOverrides;
pub use host::{
    DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
    DatabaseWriterQueueCapacity, HostConfiguration, HostConfigurationLoader, HostConfigurationView,
};
pub use secret::{SecretFileReference, SensitivePath};

pub const DEFAULT_HOST_CONFIGURATION_FILE: &str = "maincopy.toml";
