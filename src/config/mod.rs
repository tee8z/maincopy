//! Typed host configuration and redacted secret references.

mod diagnostic;
mod host;
pub(crate) mod secret;

pub use diagnostic::{
    ConfigurationAuthority, ConfigurationDiagnostic, ConfigurationErrors,
    ConfigurationValidationCode,
};
pub(crate) use host::HostConfigurationOverrides;
pub use host::{
    DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
    DatabaseWriterQueueCapacity, HostConfiguration, HostConfigurationLoader, HostConfigurationView,
    LexeConfigurationView, LexeInFlightLimit, LexeNetwork, LexePendingLimit,
    LexeReconciliationPageSize, LexeRecoveryInterval, LexeResponseTimeout,
};
pub use secret::{SecretFileReference, SensitivePath};

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
