use serde::Deserialize;
use url::Url;

use super::{ConfigurationDiagnostic, SenderAddress, validated_field};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubscriptionMode {
    Paused,
    Enabled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SubscriptionPolicy {
    mode: SubscriptionMode,
    operator_name: String,
    postal_address: String,
    purpose: String,
    privacy_url: Url,
    contact_address: SenderAddress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SubscriptionPolicyView<'policy> {
    pub mode: SubscriptionMode,
    pub operator_name: &'policy str,
    pub postal_address: &'policy str,
    pub purpose: &'policy str,
    pub privacy_url: &'policy Url,
    pub contact_address: &'policy SenderAddress,
}

impl SubscriptionPolicy {
    pub(crate) fn view(&self) -> SubscriptionPolicyView<'_> {
        SubscriptionPolicyView {
            mode: self.mode,
            operator_name: &self.operator_name,
            postal_address: &self.postal_address,
            purpose: &self.purpose,
            privacy_url: &self.privacy_url,
            contact_address: &self.contact_address,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SubscriptionCandidate {
    mode: SubscriptionMode,
    operator_name: String,
    postal_address: String,
    purpose: String,
    privacy_url: String,
    contact_address: String,
}

impl SubscriptionCandidate {
    pub(super) fn validate(
        self,
        diagnostics: &mut Vec<ConfigurationDiagnostic>,
    ) -> Option<SubscriptionPolicy> {
        let operator_name = validated_field(
            public_text(self.operator_name, 200),
            "mail.subscriptions.operator_name",
            "mail operator name must contain 1 to 200 bytes of public text without control characters",
            diagnostics,
        );
        let postal_address = validated_field(
            public_text(self.postal_address, 500),
            "mail.subscriptions.postal_address",
            "mail postal address must contain 1 to 500 bytes of public text without control characters",
            diagnostics,
        );
        let purpose = validated_field(
            public_text(self.purpose, 2000),
            "mail.subscriptions.purpose",
            "mail purpose must contain 1 to 2000 bytes of public text without control characters",
            diagnostics,
        );
        let privacy_url = validated_field(
            privacy_url(&self.privacy_url),
            "mail.subscriptions.privacy_url",
            "mail privacy notice must be a bounded HTTPS URL without credentials",
            diagnostics,
        );
        let contact_address = validated_field(
            SenderAddress::parse(&self.contact_address).ok(),
            "mail.subscriptions.contact_address",
            "mail contact address must be a monitored public ASCII mailbox",
            diagnostics,
        );
        match (
            operator_name,
            postal_address,
            purpose,
            privacy_url,
            contact_address,
        ) {
            (
                Some(operator_name),
                Some(postal_address),
                Some(purpose),
                Some(privacy_url),
                Some(contact_address),
            ) => Some(SubscriptionPolicy {
                mode: self.mode,
                operator_name,
                postal_address,
                purpose,
                privacy_url,
                contact_address,
            }),
            _ => None,
        }
    }
}

fn public_text(value: String, limit: usize) -> Option<String> {
    (!value.is_empty()
        && value.trim() == value
        && value.len() <= limit
        && !value.chars().any(char::is_control))
    .then_some(value)
}

fn privacy_url(value: &str) -> Option<Url> {
    if value.len() > 2048 || value.chars().any(char::is_control) || value.contains('\\') {
        return None;
    }
    let parsed = Url::parse(value).ok()?;
    (parsed.scheme() == "https"
        && parsed.host_str().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none())
    .then_some(parsed)
}
