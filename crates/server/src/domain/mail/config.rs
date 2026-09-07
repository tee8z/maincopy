//! Host-owned mail settings. Valid configuration is not dispatch authorization:
//! startup must also admit protected credentials and a current subscriber authority.

use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;

use super::{
    feedback::FeedbackConfiguration,
    identity::{EmailAddress, InvalidEmailAddress},
    ses::{ResourceName, SesCredentials, SesRegion},
};

#[path = "config/subscriptions.rs"]
mod subscriptions;
use crate::config::{
    ConfigurationDiagnostic, ConfigurationErrors, ConfigurationValidationCode, SecretFileReference,
};
use subscriptions::SubscriptionCandidate;
pub(crate) use subscriptions::{SubscriptionMode, SubscriptionPolicy};

const DEFAULT_CAMPAIGN_RECIPIENTS: u64 = 2_000;
const DEFAULT_DAILY_MESSAGES: u64 = 5_000;
const DEFAULT_DAILY_CONFIRMATION_MESSAGES: u64 = 100;
const DEFAULT_SEND_INTERVAL_MILLISECONDS: u64 = 1_000;
const MAX_CAMPAIGN_RECIPIENTS: u64 = 100_000;
const MAX_DAILY_MESSAGES: u64 = 1_000_000;

/// The publication's visible sender is public configuration, not a subscriber
/// address. Subscriber EmailAddress intentionally has no Clone or Debug surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SenderAddress(String);

impl SenderAddress {
    fn parse(value: &str) -> Result<Self, InvalidEmailAddress> {
        EmailAddress::parse(value).map(|address| Self(address.as_str().to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum MailConfiguration {
    #[default]
    Disabled,
    Ses(Box<SesMailConfiguration>),
}

/// Canonical validated settings. Secret references never contain loaded bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SesMailConfiguration {
    sender: SenderAddress,
    region: String,
    configuration_set: String,
    credential_file: SecretFileReference,
    control_signing_key_file: SecretFileReference,
    max_campaign_recipients: u64,
    max_daily_messages: u64,
    max_daily_confirmation_messages: u64,
    send_interval: Duration,
    subscriptions: Option<SubscriptionPolicy>,
    feedback: Option<FeedbackConfiguration>,
}

/// Borrowed operator and startup view; numbers have already passed host bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SesMailConfigurationView<'configuration> {
    pub sender: &'configuration SenderAddress,
    pub region: &'configuration str,
    pub configuration_set: &'configuration str,
    pub credential_file: &'configuration SecretFileReference,
    pub control_signing_key_file: &'configuration SecretFileReference,
    pub max_campaign_recipients: u64,
    pub max_daily_messages: u64,
    pub max_daily_confirmation_messages: u64,
    pub send_interval: Duration,
    pub subscriptions: Option<&'configuration SubscriptionPolicy>,
    pub feedback: Option<&'configuration FeedbackConfiguration>,
}

impl SesMailConfiguration {
    pub(crate) fn view(&self) -> SesMailConfigurationView<'_> {
        SesMailConfigurationView {
            sender: &self.sender,
            region: &self.region,
            configuration_set: &self.configuration_set,
            credential_file: &self.credential_file,
            control_signing_key_file: &self.control_signing_key_file,
            max_campaign_recipients: self.max_campaign_recipients,
            max_daily_messages: self.max_daily_messages,
            max_daily_confirmation_messages: self.max_daily_confirmation_messages,
            send_interval: self.send_interval,
            subscriptions: self.subscriptions.as_ref(),
            feedback: self.feedback.as_ref(),
        }
    }

    /// Bind campaign approval to the provider settings and actual loaded identity.
    /// Moving an unchanged credential file does not change this binding. Changing
    /// its loaded identity does. Never use this value for management links.
    pub(super) fn provider_binding(&self, credentials: &SesCredentials) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_derive_key("maincopy mail provider configuration v1");
        for value in [self.sender.as_str(), &self.region, &self.configuration_set] {
            hasher.update(&(value.len() as u64).to_le_bytes());
            hasher.update(value.as_bytes());
        }
        for value in [
            self.max_campaign_recipients,
            self.max_daily_messages,
            self.max_daily_confirmation_messages,
            self.send_interval.as_millis() as u64,
        ] {
            hasher.update(&value.to_le_bytes());
        }
        credentials.bind_configuration(&mut hasher);
        hasher.update(&[u8::from(self.subscriptions.is_some())]);
        if let Some(policy) = &self.subscriptions {
            let view = policy.view();
            for value in [
                view.operator_name,
                view.postal_address,
                view.purpose,
                view.privacy_url.as_str(),
                view.contact_address.as_str(),
            ] {
                hasher.update(&(value.len() as u64).to_le_bytes());
                hasher.update(value.as_bytes());
            }
        }
        hasher.update(&[u8::from(self.feedback.is_some())]);
        if let Some(feedback) = &self.feedback {
            let view = feedback.view();
            for value in [view.queue_url, view.topic_arn] {
                hasher.update(&(value.len() as u64).to_le_bytes());
                hasher.update(value.as_bytes());
            }
        }
        *hasher.finalize().as_bytes()
    }
}

/// Only the host parser consumes this wire candidate. Its alternatives and raw
/// fields remain private until all values have been validated together.
#[derive(Default, Deserialize)]
#[serde(transparent)]
pub(crate) struct MailConfigurationCandidate(MailCandidate);

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum MailCandidate {
    // A unit variant discards extra map entries in Serde's internally tagged
    // visitor. The empty struct variant enforces deny_unknown_fields.
    Disabled {},
    Ses(Box<SesCandidate>),
}

impl Default for MailCandidate {
    fn default() -> Self {
        Self::Disabled {}
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SesCandidate {
    sender: String,
    region: String,
    configuration_set: String,
    credential_file: PathBuf,
    control_signing_key_file: PathBuf,
    max_campaign_recipients: Option<u64>,
    max_daily_messages: Option<u64>,
    max_daily_confirmation_messages: Option<u64>,
    send_interval_milliseconds: Option<u64>,
    subscriptions: Option<SubscriptionCandidate>,
    feedback: Option<FeedbackCandidate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedbackCandidate {
    queue_url: String,
    topic_arn: String,
}

impl MailConfigurationCandidate {
    pub(crate) fn validate(
        self,
        file_base: &Path,
    ) -> Result<MailConfiguration, ConfigurationErrors> {
        match self.0 {
            MailCandidate::Disabled {} => Ok(MailConfiguration::Disabled),
            MailCandidate::Ses(candidate) => candidate
                .validate(file_base)
                .map(Box::new)
                .map(MailConfiguration::Ses),
        }
    }
}

impl SesCandidate {
    fn validate(self, file_base: &Path) -> Result<SesMailConfiguration, ConfigurationErrors> {
        let mut diagnostics = Vec::new();
        let limits = self.limits(&mut diagnostics);
        let subscriptions = self
            .subscriptions
            .and_then(|candidate| candidate.validate(&mut diagnostics));
        let feedback = self.feedback.and_then(|candidate| validated_field(
            FeedbackConfiguration::new(&candidate.queue_url, &candidate.topic_arn, &self.region, &self.configuration_set).ok(),
            "mail.feedback",
            "mail feedback must identify an SQS queue and SNS topic in the configured SES region and one AWS account",
            &mut diagnostics,
        ));
        if subscriptions
            .as_ref()
            .is_some_and(|policy| policy.view().mode == SubscriptionMode::Enabled)
            && feedback.is_none()
        {
            diagnostics.push(ConfigurationDiagnostic::new(
                "mail.feedback",
                ConfigurationValidationCode::HostTomlInvalid,
                "enabled subscriptions require an authenticated SES feedback queue",
            ));
        }
        let sender = validated_field(
            SenderAddress::parse(&self.sender).ok(),
            "mail.sender",
            "mail.sender must be a bounded ASCII mailbox without display-name syntax",
            &mut diagnostics,
        );
        let region = validated_field(
            SesRegion::parse(&self.region).ok().map(|_| self.region),
            "mail.region",
            "mail.region must name a supported commercial AWS region",
            &mut diagnostics,
        );
        let configuration_set = validated_field(
            ResourceName::parse(&self.configuration_set)
                .ok()
                .map(|_| self.configuration_set),
            "mail.configuration_set",
            "mail.configuration_set must be a bounded SES resource name",
            &mut diagnostics,
        );
        let credential_file = protected_reference(
            self.credential_file,
            file_base,
            "mail.credential_file",
            &mut diagnostics,
        );
        let control_signing_key_file = protected_reference(
            self.control_signing_key_file,
            file_base,
            "mail.control_signing_key_file",
            &mut diagnostics,
        );
        if !diagnostics.is_empty() {
            return Err(ConfigurationErrors::from_diagnostics(diagnostics));
        }
        match (
            sender,
            region,
            configuration_set,
            credential_file,
            control_signing_key_file,
            limits,
        ) {
            (
                Some(sender),
                Some(region),
                Some(configuration_set),
                Some(credential_file),
                Some(control_signing_key_file),
                Some(limits),
            ) => Ok(SesMailConfiguration {
                sender,
                region,
                configuration_set,
                credential_file,
                control_signing_key_file,
                max_campaign_recipients: limits.campaign,
                max_daily_messages: limits.daily,
                max_daily_confirmation_messages: limits.confirmation,
                send_interval: Duration::from_millis(limits.interval_milliseconds),
                subscriptions,
                feedback,
            }),
            _ => Err(ConfigurationErrors::from_diagnostics(vec![
                ConfigurationDiagnostic::new(
                    "mail",
                    ConfigurationValidationCode::HostTomlInvalid,
                    "effective mail settings could not be constructed",
                ),
            ])),
        }
    }

    fn limits(&self, diagnostics: &mut Vec<ConfigurationDiagnostic>) -> Option<MailLimits> {
        let campaign = bounded_limit(
            self.max_campaign_recipients
                .unwrap_or(DEFAULT_CAMPAIGN_RECIPIENTS),
            1,
            MAX_CAMPAIGN_RECIPIENTS,
            "mail.max_campaign_recipients",
            diagnostics,
        );
        let daily = bounded_limit(
            self.max_daily_messages.unwrap_or(DEFAULT_DAILY_MESSAGES),
            1,
            MAX_DAILY_MESSAGES,
            "mail.max_daily_messages",
            diagnostics,
        );
        let confirmation = bounded_limit(
            self.max_daily_confirmation_messages
                .unwrap_or(DEFAULT_DAILY_CONFIRMATION_MESSAGES),
            1,
            MAX_DAILY_MESSAGES,
            "mail.max_daily_confirmation_messages",
            diagnostics,
        );
        let interval = bounded_limit(
            self.send_interval_milliseconds
                .unwrap_or(DEFAULT_SEND_INTERVAL_MILLISECONDS),
            100,
            60_000,
            "mail.send_interval_milliseconds",
            diagnostics,
        );
        if let (Some(daily), Some(confirmation)) = (daily, confirmation)
            && confirmation > daily
        {
            diagnostics.push(ConfigurationDiagnostic::new(
                "mail.max_daily_confirmation_messages",
                ConfigurationValidationCode::LimitOutOfRange,
                "confirmation messages must fit within the total daily message budget",
            ));
            return None;
        }
        match (campaign, daily, confirmation, interval) {
            (Some(campaign), Some(daily), Some(confirmation), Some(interval_milliseconds)) => {
                Some(MailLimits {
                    campaign,
                    daily,
                    confirmation,
                    interval_milliseconds,
                })
            }
            _ => None,
        }
    }
}

struct MailLimits {
    campaign: u64,
    daily: u64,
    confirmation: u64,
    interval_milliseconds: u64,
}

fn validated_field<Value>(
    value: Option<Value>,
    field: &'static str,
    message: &'static str,
    diagnostics: &mut Vec<ConfigurationDiagnostic>,
) -> Option<Value> {
    if value.is_none() {
        diagnostics.push(ConfigurationDiagnostic::new(
            field,
            ConfigurationValidationCode::HostTomlInvalid,
            message,
        ));
    }
    value
}

fn bounded_limit(
    value: u64,
    minimum: u64,
    maximum: u64,
    field: &'static str,
    diagnostics: &mut Vec<ConfigurationDiagnostic>,
) -> Option<u64> {
    if (minimum..=maximum).contains(&value) {
        Some(value)
    } else {
        diagnostics.push(ConfigurationDiagnostic::new(
            field,
            ConfigurationValidationCode::LimitOutOfRange,
            "mail budget or send interval is outside its supported positive range",
        ));
        None
    }
}

fn protected_reference(
    path: PathBuf,
    base: &Path,
    field: &'static str,
    diagnostics: &mut Vec<ConfigurationDiagnostic>,
) -> Option<SecretFileReference> {
    let nonempty = !path.as_os_str().is_empty();
    let path = base.join(path);
    let canonical = std::fs::canonicalize(&path).ok();
    if !nonempty
        || !path.is_absolute()
        || path.as_os_str().as_encoded_bytes().contains(&0)
        || in_nix_store(&path)
        || canonical.as_deref().is_some_and(in_nix_store)
    {
        diagnostics.push(ConfigurationDiagnostic::new(
            field,
            ConfigurationValidationCode::SecretReferenceInvalid,
            "mail secret references must resolve outside the Nix store to nonempty absolute paths",
        ));
        return None;
    }
    SecretFileReference::new(path)
}

fn in_nix_store(path: &Path) -> bool {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str())
            }
        }
    }
    normalized.starts_with("/nix/store")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SES: &str = r#"mode = "ses"
sender = "Newsletter@EXAMPLE.COM"
region = "us-east-1"
configuration_set = "newsletter"
credential_file = "../secrets/aws credentials.json"
control_signing_key_file = "../secrets/control.key"
"#;
    const CREDENTIAL: &[u8] =
        br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"1234567890123456"}"#;

    fn configured(source: &str) -> SesMailConfiguration {
        let candidate: MailConfigurationCandidate = toml::from_str(source).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let MailConfiguration::Ses(configuration) = candidate.validate(directory.path()).unwrap()
        else {
            panic!("the fixture selects SES");
        };
        *configuration
    }

    #[test]
    fn disabled_mode_rejects_ignored_provider_fields_and_unknown_modes() {
        assert!(matches!(
            MailConfigurationCandidate::default()
                .validate(Path::new("/unused"))
                .unwrap(),
            MailConfiguration::Disabled
        ));
        for extra in [
            "sender = 'newsletter@example.com'",
            "credential_file = '/secret/credential'",
            "control_signing_key_file = '/secret/control'",
            "max_daily_messages = 100",
        ] {
            assert!(
                toml::from_str::<MailConfigurationCandidate>(&format!(
                    "mode = 'disabled'\n{extra}\n"
                ))
                .is_err()
            );
        }
        for invalid in ["mode = 'smtp'", "mode = 'ses'", "enabled = true"] {
            assert!(toml::from_str::<MailConfigurationCandidate>(invalid).is_err());
        }
        assert!(
            toml::from_str::<MailConfigurationCandidate>(&format!(
                "{SES}endpoint = 'http://localhost'\n"
            ))
            .is_err()
        );
    }

    #[test]
    fn defaults_and_sender_normalization_are_reviewable_without_loading_secrets() {
        let configuration = configured(SES);
        let view = configuration.view();
        assert_eq!(view.sender.as_str(), "Newsletter@example.com");
        assert_eq!(view.region, "us-east-1");
        assert_eq!(view.configuration_set, "newsletter");
        assert_eq!(view.max_campaign_recipients, 2_000);
        assert_eq!(view.max_daily_messages, 5_000);
        assert_eq!(view.max_daily_confirmation_messages, 100);
        assert_eq!(view.send_interval, Duration::from_secs(1));
        assert!(view.credential_file.path().is_absolute());
        assert!(
            view.credential_file
                .path()
                .ends_with("../secrets/aws credentials.json")
        );
        let rendered = format!("{configuration:?} {view:?}");
        assert!(rendered.contains("Newsletter@example.com"));
        assert!(!rendered.contains("aws credentials.json"));
        assert!(!rendered.contains("control.key"));
    }

    #[test]
    fn malformed_settings_report_all_safe_field_errors_in_sorted_order() {
        let source = SES
            .replace("Newsletter@EXAMPLE.COM", "hidden@bad domain")
            .replace("us-east-1", "https://hidden.example")
            .replace(
                "configuration_set = \"newsletter\"",
                "configuration_set = 'hidden/value'",
            )
            .replace("../secrets/aws credentials.json", "")
            .replace("../secrets/control.key", "/nix/store/hidden-key");
        let source = format!(
            "{source}max_campaign_recipients = 0\nmax_daily_messages = 1\nmax_daily_confirmation_messages = 2\nsend_interval_milliseconds = 99\n"
        );
        let candidate: MailConfigurationCandidate = toml::from_str(&source).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let errors = candidate.validate(directory.path()).unwrap_err();
        let fields: Vec<_> = errors
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.field.as_ref())
            .collect();
        assert_eq!(
            fields,
            [
                "mail.configuration_set",
                "mail.control_signing_key_file",
                "mail.credential_file",
                "mail.max_campaign_recipients",
                "mail.max_daily_confirmation_messages",
                "mail.region",
                "mail.send_interval_milliseconds",
                "mail.sender"
            ]
        );
        let rendered = format!("{errors:?} {errors}");
        assert!(!rendered.contains("hidden"));
    }

    #[test]
    fn inclusive_message_and_rate_limits_reject_zero_and_excessive_values() {
        for (field, minimum, maximum, additional) in [
            ("max_campaign_recipients", 1, 100_000, ""),
            (
                "max_daily_messages",
                1,
                1_000_000,
                "max_daily_confirmation_messages = 1\n",
            ),
            (
                "max_daily_confirmation_messages",
                1,
                1_000_000,
                "max_daily_messages = 1000000\n",
            ),
            ("send_interval_milliseconds", 100, 60_000, ""),
        ] {
            for (value, valid) in [
                (minimum - 1, false),
                (minimum, true),
                (maximum, true),
                (maximum + 1, false),
            ] {
                let source = format!("{SES}{field} = {value}\n{additional}");
                let candidate: MailConfigurationCandidate = toml::from_str(&source).unwrap();
                let directory = tempfile::tempdir().unwrap();
                assert_eq!(
                    candidate.validate(directory.path()).is_ok(),
                    valid,
                    "{field} boundary {value}"
                );
            }
        }
    }

    #[test]
    fn confirmation_budget_is_part_of_the_total_daily_budget() {
        for (daily, valid) in [(99, false), (100, true)] {
            let candidate: MailConfigurationCandidate =
                toml::from_str(&format!("{SES}max_daily_messages = {daily}\n")).unwrap();
            let directory = tempfile::tempdir().unwrap();
            assert_eq!(candidate.validate(directory.path()).is_ok(), valid);
        }
    }

    #[test]
    fn secret_references_reject_empty_paths_and_lexical_nix_store_escapes() {
        for path in [
            "",
            "/nix/store/private",
            "/tmp/../nix/store/private",
            "/nix/./store/private",
        ] {
            let source = SES.replace("../secrets/aws credentials.json", path);
            let candidate: MailConfigurationCandidate = toml::from_str(&source).unwrap();
            let directory = tempfile::tempdir().unwrap();
            let errors = candidate.validate(directory.path()).unwrap_err();
            assert_eq!(
                errors.diagnostics()[0].field.as_ref(),
                "mail.credential_file"
            );
            assert_eq!(
                errors.diagnostics()[0].code,
                ConfigurationValidationCode::SecretReferenceInvalid
            );
        }
    }

    #[test]
    fn equivalent_config_and_relocated_credentials_preserve_the_approval_binding() {
        let credentials = SesCredentials::parse(CREDENTIAL).unwrap();
        let original = configured(SES).provider_binding(&credentials);
        let equivalent = SES
            .replace("Newsletter@EXAMPLE.COM", "Newsletter@example.com")
            .replace("../secrets/aws credentials.json", "relocated.json")
            .replace("../secrets/control.key", "relocated-control.key");
        assert_eq!(
            original,
            configured(&equivalent).provider_binding(&credentials)
        );
        let reformatted = SesCredentials::parse(
            br#"{ "secret_access_key": "1234567890123456", "access_key_id": "AKIDEXAMPLE" }"#,
        )
        .unwrap();
        assert_eq!(original, configured(SES).provider_binding(&reformatted));
    }

    #[test]
    fn changed_provider_identity_or_admission_limits_invalidate_old_approvals() {
        let credentials = SesCredentials::parse(CREDENTIAL).unwrap();
        let configuration = configured(SES);
        let original = configuration.provider_binding(&credentials);
        for (before, after) in [
            ("Newsletter@EXAMPLE.COM", "newsletter@example.com"),
            ("us-east-1", "us-west-2"),
            (
                "configuration_set = \"newsletter\"",
                "configuration_set = 'other'",
            ),
        ] {
            assert_ne!(
                original,
                configured(&SES.replace(before, after)).provider_binding(&credentials)
            );
        }
        for change in [
            "max_campaign_recipients = 1999",
            "max_daily_messages = 4999",
            "max_daily_confirmation_messages = 99",
            "send_interval_milliseconds = 1001",
        ] {
            assert_ne!(
                original,
                configured(&format!("{SES}{change}\n")).provider_binding(&credentials)
            );
        }
        for changed in [
            br#"{"access_key_id":"AKIDOTHER","secret_access_key":"1234567890123456"}"#.as_slice(),
            br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"6543210987654321"}"#.as_slice(),
            br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"1234567890123456","session_token":"protected-session"}"#.as_slice(),
        ] {
            assert_ne!(original, configuration.provider_binding(&SesCredentials::parse(changed).unwrap()));
        }
    }

    const SUBSCRIPTIONS: &str = r#"
[subscriptions]
mode = "paused"
operator_name = "Example Publication"
postal_address = "PO Box 123, Example City"
purpose = "New articles from Example."
privacy_url = "https://example.com/privacy"
contact_address = "contact@example.com"
"#;
    const FEEDBACK: &str = r#"
[feedback]
queue_url = "https://sqs.us-east-1.amazonaws.com/123456789012/newsletter"
topic_arn = "arn:aws:sns:us-east-1:123456789012:newsletter"
"#;

    #[test]
    fn enabled_capture_requires_feedback_but_paused_controls_remain_configurable() {
        let paused = configured(&format!("{SES}{SUBSCRIPTIONS}"));
        assert_eq!(
            paused.view().subscriptions.unwrap().view().mode,
            SubscriptionMode::Paused
        );
        assert!(paused.view().feedback.is_none());
        let enabled = format!("{SES}{}", SUBSCRIPTIONS.replace("paused", "enabled"));
        let candidate: MailConfigurationCandidate = toml::from_str(&enabled).unwrap();
        let errors = candidate.validate(Path::new("/unused")).unwrap_err();
        assert_eq!(errors.diagnostics()[0].field.as_ref(), "mail.feedback");
        assert!(
            configured(&format!("{enabled}{FEEDBACK}"))
                .view()
                .feedback
                .is_some()
        );
    }

    #[test]
    fn public_policy_changes_require_review_while_pausing_preserves_approved_content_binding() {
        let credentials = SesCredentials::parse(CREDENTIAL).unwrap();
        let source = format!("{SES}{SUBSCRIPTIONS}{FEEDBACK}");
        let original = configured(&source).provider_binding(&credentials);
        assert_eq!(
            original,
            configured(&source.replace("paused", "enabled")).provider_binding(&credentials)
        );
        for (before, after) in [
            ("Example Publication", "Another Publication"),
            ("PO Box 123", "PO Box 124"),
            ("New articles from Example.", "Monthly summaries."),
            (
                "https://example.com/privacy",
                "https://example.com/privacy-updated",
            ),
            ("contact@example.com", "help@example.com"),
            ("123456789012/newsletter", "123456789012/feedback"),
            ("123456789012:newsletter", "123456789012:feedback"),
        ] {
            assert_ne!(
                original,
                configured(&source.replace(before, after)).provider_binding(&credentials)
            );
        }
    }

    #[test]
    fn invalid_public_disclosures_are_rejected_without_echoing_untrusted_values() {
        for (before, after, field) in [
            ("Example Publication", "", "operator_name"),
            ("PO Box 123, Example City", " ", "postal_address"),
            ("New articles from Example.", "\\tprivate-marker", "purpose"),
            (
                "https://example.com/privacy",
                "https://private-marker@example.com/privacy",
                "privacy_url",
            ),
            (
                "https://example.com/privacy",
                "http://example.com/privacy",
                "privacy_url",
            ),
            (
                "contact@example.com",
                "Name <private-marker@example.com>",
                "contact_address",
            ),
        ] {
            let candidate: MailConfigurationCandidate =
                toml::from_str(&format!("{SES}{}", SUBSCRIPTIONS.replace(before, after))).unwrap();
            let errors = candidate.validate(Path::new("/unused")).unwrap_err();
            assert!(errors.diagnostics().iter().any(
                |diagnostic| diagnostic.field.as_ref() == format!("mail.subscriptions.{field}")
            ));
            assert!(!format!("{errors:?}").contains("private-marker"));
        }
    }
}
