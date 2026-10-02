//! Public newsletter settings share the consent writer. Host configuration
//! retains credentials, sender identity, and provider endpoints.

pub(crate) mod store;

use std::sync::Arc;

use super::{
    config::{NewsletterSettings, SesMailConfiguration, SubscriptionMode},
    ses::SesCredentials,
    subscriber::{
        SubscriberMode, SubscriberPolicy,
        store::{SubscriberLoadError, SubscriberStore},
    },
};
use crate::domain::auth::store::MutationAuditContext;

#[derive(Clone)]
pub(crate) struct MailSettingsSource {
    configuration: SesMailConfiguration,
    credentials: Arc<SesCredentials>,
}

#[derive(Clone)]
pub(crate) struct EffectiveMailSettings {
    pub configuration: SesMailConfiguration,
    pub configuration_binding: [u8; 32],
    pub version: u64,
    pub source: MailSettingsSource,
}

pub(crate) struct StoredMailSettings {
    pub version: u64,
    pub settings: NewsletterSettings,
}

pub(crate) struct UpdateMailSettings {
    pub expected_version: u64,
    pub activation: SettingsActivation,
    pub settings: NewsletterSettings,
    pub audit: MutationAuditContext,
}

pub(crate) enum SettingsActivation {
    Offline,
    Live {
        expected_control_version: u64,
        expected_binding: [u8; 32],
        configuration_binding: [u8; 32],
    },
}

impl MailSettingsSource {
    pub(super) fn new(
        configuration: SesMailConfiguration,
        credentials: Arc<SesCredentials>,
    ) -> Self {
        Self {
            configuration,
            credentials,
        }
    }

    pub(super) fn resolve(&self, stored: Option<&StoredMailSettings>) -> EffectiveMailSettings {
        let configuration = match stored {
            Some(stored) => self
                .configuration
                .with_newsletter_settings(&stored.settings),
            None => self.configuration.clone(),
        };
        EffectiveMailSettings {
            configuration_binding: configuration.provider_binding(&self.credentials),
            configuration,
            version: stored.map_or(0, |stored| stored.version),
            source: self.clone(),
        }
    }

    pub(super) fn newsletter_binding(&self, settings: &NewsletterSettings) -> [u8; 32] {
        self.configuration
            .with_newsletter_settings(settings)
            .provider_binding(&self.credentials)
    }

    pub(super) async fn load(
        &self,
        subscribers: &SubscriberStore,
    ) -> Result<EffectiveMailSettings, SubscriberLoadError> {
        let stored = subscribers.mail_settings().await?;
        Ok(self.resolve(stored.as_ref()))
    }
}

impl EffectiveMailSettings {
    pub(super) fn from_configuration(
        configuration: SesMailConfiguration,
        credentials: Arc<SesCredentials>,
    ) -> Self {
        MailSettingsSource::new(configuration, credentials).resolve(None)
    }

    pub(super) fn subscriber_policy(&self) -> SubscriberPolicy {
        let view = self.configuration.view();
        let mode = match view.subscriptions.map(|policy| policy.view().mode) {
            Some(SubscriptionMode::Enabled) => SubscriberMode::Enabled,
            Some(SubscriptionMode::Paused) | None => SubscriberMode::Paused,
        };
        SubscriberPolicy {
            configuration_binding: self.configuration_binding,
            mode,
        }
    }
}
