//! Typed host configuration and redacted secret references.

mod diagnostic;
mod host;
mod secret;

pub use diagnostic::{
    ConfigurationAuthority, ConfigurationDiagnostic, ConfigurationErrors,
    ConfigurationValidationCode,
};
pub use host::{
    AdminListenerConfiguration, DatabaseBusyTimeout, DatabaseConfiguration, DatabaseReadPoolSize,
    DatabaseWriterQueueCapacity, HostConfiguration, HostConfigurationLoader,
    HostConfigurationOverrides, HostPaths, LexeConfiguration, LexeInFlightLimit, LexeNetwork,
    LexePendingLimit, LexeReconciliationPageSize, LexeRecoveryInterval, LexeResponseTimeout,
    LightningConfiguration, PublicListenerConfiguration,
};
pub use secret::{
    EnvironmentSecretReference, SecretFileReference, SecretReference, SecretReferenceKind,
    SecretResolver, SensitivePath,
};

use diagnostic::single_error;

pub const DEFAULT_HOST_CONFIGURATION_FILE: &str = "maincopy.toml";

pub(crate) fn tip_provider_required() -> ConfigurationErrors {
    single_error(ConfigurationDiagnostic::new(
        ConfigurationAuthority::Host,
        "lightning",
        ConfigurationValidationCode::TipProviderRequired,
        "configured publication tips require a configured Lightning receive provider",
    ))
}
