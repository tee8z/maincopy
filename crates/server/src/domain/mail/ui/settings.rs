//! Owner-managed newsletter details and admission limits.

use axum::{
    Extension, Form, extract::rejection::FormRejection, http::StatusCode, response::Response,
};
use maincopy_shared::auth_api::SecretString;
use maud::{Markup, html};
use serde::Deserialize;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{MailUiAccess, MailUiState, UiError, canonical_uuid, decode_form, hex};
use crate::{
    admin::{
        BrowserFormSession,
        principal::AdminPrincipal,
        request_id::RequestId,
        ui::{self as admin_ui, PageKind},
    },
    config::ConfigurationErrors,
    domain::{
        auth::store::AdminMutationKey,
        mail::{
            config::{
                NewsletterSettings, NewsletterSettingsCandidate, SubscriptionCandidate,
                SubscriptionMode, SubscriptionPolicy,
            },
            privacy::notice_url,
            settings::{SettingsActivation, StoredMailSettings, UpdateMailSettings},
            subscriber::{SubscriberCommandError, store::SubscriberMutationError},
        },
    },
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SettingsForm {
    #[serde(rename = "_csrf")]
    _csrf: SecretString,
    idempotency_key: String,
    expected_version: u64,
    expected_control_version: u64,
    configuration_binding: String,
    mode: SubscriptionMode,
    operator_name: String,
    postal_address: String,
    purpose: String,
    privacy_url: String,
    contact_address: String,
}

struct EditorValues {
    version: u64,
    policy: Option<SubscriptionPolicy>,
}

impl EditorValues {
    fn load(access: &MailUiAccess, stored: Option<&StoredMailSettings>) -> Self {
        if let Some(stored) = stored {
            let view = stored.settings.view();
            return Self {
                version: stored.version,
                policy: Some(view.subscriptions.clone()),
            };
        }
        match access {
            MailUiAccess::ReviewOnly(binding) | MailUiAccess::DispatchReady(binding) => {
                let view = binding.configuration.view();
                Self {
                    version: 0,
                    policy: view.subscriptions.cloned(),
                }
            }
            MailUiAccess::Unavailable => Self {
                version: 0,
                policy: None,
            },
        }
    }
}

pub(super) async fn edit(
    request_id: RequestId,
    Extension(state): Extension<MailUiState>,
    browser: BrowserFormSession,
) -> Response {
    respond(request_id, edit_page(&state, &browser).await)
}

async fn edit_page(
    state: &MailUiState,
    browser: &BrowserFormSession,
) -> Result<Response, SettingsError> {
    let stored = state
        .subscribers
        .mail_settings()
        .await
        .map_err(|_| SettingsError::Unavailable)?;
    let values = EditorValues::load(&state.access, stored.as_ref());
    let status = state
        .subscribers
        .status()
        .await
        .map_err(|_| SettingsError::Unavailable)?;
    let binding = match &state.access {
        MailUiAccess::DispatchReady(binding) => Some(binding),
        MailUiAccess::Unavailable | MailUiAccess::ReviewOnly(_) => None,
    };
    let fresh = browser.session.fresh_until > OffsetDateTime::now_utc();
    let enable_available =
        binding.is_some_and(|binding| binding.configuration.view().feedback.is_some());
    Ok(admin_ui::page_response(
        StatusCode::OK,
        "Newsletter settings",
        PageKind::Authenticated,
        html! {
            h1 { "Newsletter settings" }
            p { a href="/admin/mail" { "Return to newsletter" } }
            p { "Manage signup and the public details shown to readers. While enabled, new articles are emailed automatically. Resuming delivery also resumes waiting updates." }
            @if binding.is_none() {
                p class="notice" { "You can save your newsletter details now. Signup stays paused until the email service is configured." }
            }
            @if !fresh { p class="notice" { "Sign out and sign in again before saving settings. Your session is no longer fresh." } }
            form method="post" action="/admin/mail/settings" {
                input type="hidden" name="_csrf" value=(browser.csrf_token.expose_secret());
                input type="hidden" name="idempotency_key" value=(Uuid::new_v4());
                input type="hidden" name="expected_version" value=(values.version);
                input type="hidden" name="expected_control_version" value=(binding.map_or(0, |_| status.control_version));
                input type="hidden" name="configuration_binding" value=(binding.map_or_else(String::new, |binding| hex(&binding.configuration_binding)));
                (details_fields(&values, enable_available, notice_url(&state.publications.read().catalog.publication.site.base_url).as_str()))
                p { "Changing public details updates future messages and retires unsent confirmation requests. Readers can sign up again. Unsubscribe links keep working." }
                button type="submit" disabled[!fresh] { "Save settings" }
            }
        },
    ))
}

fn details_fields(
    values: &EditorValues,
    enable_available: bool,
    default_privacy_url: &str,
) -> Markup {
    let policy = values.policy.as_ref().map(SubscriptionPolicy::view);
    let enabled =
        enable_available && policy.is_some_and(|policy| policy.mode == SubscriptionMode::Enabled);
    html! {
        fieldset {
            legend { "Newsletter details" }
            p { label for="newsletter-mode" { "Signup and sending" }
                select id="newsletter-mode" name="mode" {
                    option value="paused" selected[!enabled] { "Paused" }
                    option value="enabled" selected[enabled] disabled[!enable_available] { "Enabled" }
                }
            }
            p class="muted" { "Pausing stops new signup and sending. Existing confirmation and unsubscribe links remain available." }
            p { label for="operator-name" { "Public operator name" }
                input id="operator-name" name="operator_name" value=(policy.map_or("", |policy| policy.operator_name)) maxlength="200" required;
            }
            p { label for="contact-address" { "Contact email" }
                input id="contact-address" name="contact_address" type="email" value=(policy.map_or("", |policy| policy.contact_address.as_str())) maxlength="254" required;
            }
            p { label for="postal-address" { "Postal address (optional)" }
                input id="postal-address" name="postal_address" value=(policy.and_then(|policy| policy.postal_address).unwrap_or_default()) maxlength="500";
            }
            p { label for="newsletter-purpose" { "What readers are subscribing to" }
                textarea id="newsletter-purpose" name="purpose" rows="3" maxlength="2000" required { (policy.map_or("", |policy| policy.purpose)) }
            }
            p { label for="privacy-url" { "Custom privacy notice URL (optional)" }
                input id="privacy-url" name="privacy_url" type="url" value=(policy.map_or("", |policy| if policy.privacy_url.as_str() == default_privacy_url { "" } else { policy.privacy_url.as_str() })) maxlength="2048" placeholder=(default_privacy_url);
            }
            p class="muted" { "Leave this blank to use the " a href=(default_privacy_url) { "built-in newsletter privacy notice" } ", which uses the public details above. Enter an HTTPS URL only if you want to use your own notice." }
        }
    }
}

pub(super) async fn save(
    request_id: RequestId,
    principal: AdminPrincipal,
    Extension(state): Extension<MailUiState>,
    form: Result<Form<SettingsForm>, FormRejection>,
) -> Response {
    respond(
        request_id,
        save_settings(request_id, principal, &state, form).await,
    )
}

async fn save_settings(
    request_id: RequestId,
    principal: AdminPrincipal,
    state: &MailUiState,
    form: Result<Form<SettingsForm>, FormRejection>,
) -> Result<Response, SettingsError> {
    let mut form = decode_form(form).map_err(SettingsError::Form)?;
    if form.privacy_url.is_empty() {
        form.privacy_url =
            notice_url(&state.publications.read().catalog.publication.site.base_url).to_string();
    }
    let operation =
        AdminMutationKey(canonical_uuid(&form.idempotency_key).map_err(SettingsError::Form)?);
    let expected_version = form.expected_version;
    let expected_control_version = form.expected_control_version;
    let expected_binding = form.configuration_binding.clone();
    let settings = validated_settings(form)?;
    let activation = activation(
        &state.access,
        expected_control_version,
        &expected_binding,
        &settings,
    )?;
    state
        .subscribers
        .update_mail_settings(UpdateMailSettings {
            expected_version,
            activation,
            settings,
            audit: principal.mutation_audit(request_id, operation),
        })
        .await
        .map_err(SettingsError::Mutation)?;
    Ok(admin_ui::page_response(
        StatusCode::OK,
        "Newsletter settings saved",
        PageKind::Authenticated,
        html! {
            h1 { "Newsletter settings saved" }
            p { "Your settings are saved. No newsletter was sent." }
            p { a href="/admin/mail/settings" { "Return to newsletter settings" } " · " a href="/admin/mail" { "View mail status and campaigns" } }
        },
    ))
}

fn validated_settings(form: SettingsForm) -> Result<NewsletterSettings, SettingsError> {
    let postal_address = match form.postal_address.is_empty() {
        true => None,
        false => Some(form.postal_address),
    };
    NewsletterSettingsCandidate {
        subscriptions: SubscriptionCandidate {
            mode: form.mode,
            operator_name: form.operator_name,
            postal_address,
            purpose: form.purpose,
            privacy_url: form.privacy_url,
            contact_address: form.contact_address,
        },
    }
    .validate()
    .map_err(SettingsError::Validation)
}

fn activation(
    access: &MailUiAccess,
    expected_control_version: u64,
    expected_binding: &str,
    settings: &NewsletterSettings,
) -> Result<SettingsActivation, SettingsError> {
    match access {
        MailUiAccess::DispatchReady(binding) => {
            if settings.view().subscriptions.view().mode == SubscriptionMode::Enabled
                && binding.configuration.view().feedback.is_none()
            {
                return Err(SettingsError::ProviderUnavailable);
            }
            let parsed = blake3::Hash::from_hex(expected_binding)
                .map_err(|_| SettingsError::InvalidPrecondition)?;
            if parsed.to_hex().as_str() != expected_binding {
                return Err(SettingsError::InvalidPrecondition);
            }
            Ok(SettingsActivation::Live {
                expected_control_version,
                expected_binding: *parsed.as_bytes(),
                configuration_binding: binding.source.newsletter_binding(settings),
            })
        }
        MailUiAccess::Unavailable | MailUiAccess::ReviewOnly(_) => {
            if !expected_binding.is_empty() || expected_control_version != 0 {
                return Err(SettingsError::InvalidPrecondition);
            }
            if settings.view().subscriptions.view().mode != SubscriptionMode::Paused {
                return Err(SettingsError::ProviderUnavailable);
            }
            Ok(SettingsActivation::Offline)
        }
    }
}

#[derive(Debug, Error)]
enum SettingsError {
    #[error("invalid settings form")]
    Form(UiError),
    #[error("invalid newsletter settings")]
    Validation(ConfigurationErrors),
    #[error("newsletter settings are unavailable")]
    Unavailable,
    #[error("the settings form has an invalid precondition")]
    InvalidPrecondition,
    #[error("the email service is not ready for enabled subscriptions")]
    ProviderUnavailable,
    #[error("newsletter settings could not be saved")]
    Mutation(SubscriberMutationError),
}

fn respond(request_id: RequestId, result: Result<Response, SettingsError>) -> Response {
    match result {
        Ok(response) => response,
        Err(SettingsError::Validation(errors)) => admin_ui::page_response(
            StatusCode::BAD_REQUEST,
            "Check newsletter settings",
            PageKind::Authenticated,
            html! {
                h1 { "Check newsletter settings" }
                ul { @for diagnostic in errors.diagnostics() { li { (diagnostic.message) } } }
                p { "No settings were changed. Return to the form and correct these values." }
                p { a href="/admin/mail/settings" { "Return to newsletter settings" } }
            },
        ),
        Err(error) => {
            let (status, message) = error_description(error);
            admin_ui::error_response(
                status,
                "Newsletter settings were not saved",
                message,
                request_id,
            )
        }
    }
}

fn error_description(error: SettingsError) -> (StatusCode, &'static str) {
    match error {
        SettingsError::Mutation(SubscriberMutationError::Command(
            SubscriberCommandError::Forbidden,
        )) => (
            StatusCode::FORBIDDEN,
            "Sign in again as an enabled Owner before changing newsletter settings.",
        ),
        SettingsError::Mutation(SubscriberMutationError::Command(
            SubscriberCommandError::StaleVersion
            | SubscriberCommandError::ConfigurationChanged
            | SubscriberCommandError::IdempotencyConflict,
        )) => (
            StatusCode::CONFLICT,
            "The settings or mail state changed. Reload newsletter settings before saving again.",
        ),
        SettingsError::ProviderUnavailable => (
            StatusCode::CONFLICT,
            "Keep signup paused until the email service and delivery feedback are configured.",
        ),
        SettingsError::InvalidPrecondition
        | SettingsError::Form(_)
        | SettingsError::Validation(_)
        | SettingsError::Mutation(SubscriberMutationError::Command(
            SubscriberCommandError::InvalidValue,
        )) => (
            StatusCode::BAD_REQUEST,
            "The newsletter form is invalid. Reload newsletter settings and try again.",
        ),
        SettingsError::Mutation(SubscriberMutationError::Command(
            SubscriberCommandError::Capacity,
        )) => (
            StatusCode::CONFLICT,
            "The newsletter settings history limit has been reached. Contact the server operator.",
        ),
        SettingsError::Mutation(SubscriberMutationError::Command(
            SubscriberCommandError::Paused
            | SubscriberCommandError::ControlsUnavailable
            | SubscriberCommandError::ControlIdentityChanged,
        )) => (
            StatusCode::CONFLICT,
            "The email service changed. Reload newsletter settings before saving again.",
        ),
        SettingsError::Unavailable
        | SettingsError::Mutation(SubscriberMutationError::Admission(_))
        | SettingsError::Mutation(SubscriberMutationError::Command(
            SubscriberCommandError::OutcomeUnknown
            | SubscriberCommandError::ReconciliationNotRequired
            | SubscriberCommandError::AttemptConflict
            | SubscriberCommandError::CampaignUnavailable,
        )) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The save could not be confirmed. Retry the original form with its original operation ID, then reload newsletter settings.",
        ),
    }
}
