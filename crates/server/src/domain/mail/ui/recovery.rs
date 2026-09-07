//! An unrecoverable feedback gap can retire the entire consent epoch. This is
//! an explicit fresh-Owner operation, with a durable retry receipt.

use axum::{
    Extension, Form, extract::rejection::FormRejection, http::StatusCode, response::Response,
};
use maincopy_shared::auth_api::SecretString;
use maud::html;
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{MailUiState, UiError, canonical_uuid, decode_form, hex, respond};
use crate::{
    admin::{
        BrowserFormSession,
        principal::AdminPrincipal,
        request_id::RequestId,
        ui::{self as admin_ui, PageKind},
    },
    domain::{
        auth::store::AdminMutationKey,
        mail::subscriber::{
            FeedbackHealth, ResetSubscriberConsent, SubscriberCommandError,
            store::SubscriberMutationError,
        },
    },
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResetForm {
    #[serde(rename = "_csrf")]
    _csrf: SecretString,
    idempotency_key: Box<str>,
    expected_version: u64,
    configuration_binding: Box<str>,
    confirmation: ResetConfirmation,
}

#[derive(Deserialize)]
enum ResetConfirmation {
    #[serde(rename = "REMOVE SUBSCRIBERS")]
    RemoveSubscribers,
}

pub(super) async fn review(
    request_id: RequestId,
    Extension(state): Extension<MailUiState>,
    browser: BrowserFormSession,
) -> Response {
    let result = async {
        let binding = state.access.review()?;
        let status = state
            .subscribers
            .status()
            .await
            .map_err(|_| UiError::Unavailable)?;
        let fresh = browser.session.fresh_until > OffsetDateTime::now_utc();
        Ok(admin_ui::page_response(
            StatusCode::OK,
            "Recover email delivery",
            PageKind::Authenticated,
            html! {
                h1 { "Recover email delivery" }
                p { a href="/admin/mail" { "Return to mail campaigns" } }
                p { "Temporary polling interruptions recover automatically after the old request can no longer hide feedback. Wait for that recovery before taking a destructive action." }
                @if status.feedback_health == FeedbackHealth::ReconciliationRequired {
                    section class="panel" {
                        h2 { "Start again without the old subscriber list" }
                        p { "Use this only when missing or rejected feedback cannot be recovered. This removes all current subscriptions and pending confirmations, including requests that arrive after this page opens." }
                        p { "Previously submitted messages cannot be recalled. Campaign totals, suppression records, and sending budgets remain. Old feedback cannot restore a subscription." }
                        p { "After the operator repairs the feedback source and restarts mail, readers must voluntarily subscribe and confirm again. No email will ask the old list to subscribe again." }
                        dl {
                            dt { "Confirmed subscriptions" } dd { (status.active_enrollments) }
                            dt { "Pending confirmations" } dd { (status.pending_enrollments) }
                            dt { "Stored email addresses" } dd { (status.addressed_enrollments) }
                        }
                        @if !fresh { p class="notice" { "Sign out and sign in again before resetting subscriptions. Your session is no longer fresh." } }
                        form method="post" action="/admin/mail/recovery" {
                            input type="hidden" name="_csrf" value=(browser.csrf_token.expose_secret());
                            input type="hidden" name="idempotency_key" value=(Uuid::new_v4());
                            input type="hidden" name="expected_version" value=(status.control_version);
                            input type="hidden" name="configuration_binding" value=(hex(&binding.configuration_binding));
                            label for="reset-confirmation" { "Type REMOVE SUBSCRIBERS to confirm:" }
                            input id="reset-confirmation" name="confirmation" autocomplete="off" required;
                            button type="submit" disabled[!fresh] { "Remove all subscriptions and pause mail" }
                        }
                    }
                } @else {
                    p { "There is no unrecoverable feedback gap to reset. Check the current mail status and the operator runbook." }
                }
            },
        ))
    }
    .await;
    respond(request_id, result)
}

pub(super) async fn reset(
    request_id: RequestId,
    principal: AdminPrincipal,
    Extension(state): Extension<MailUiState>,
    form: Result<Form<ResetForm>, FormRejection>,
) -> Response {
    let result = async {
        let form = decode_form(form)?;
        let ResetConfirmation::RemoveSubscribers = form.confirmation;
        let operation = AdminMutationKey(canonical_uuid(&form.idempotency_key)?);
        let parsed = blake3::Hash::from_hex(form.configuration_binding.as_ref())
            .map_err(|_| UiError::InvalidInput)?;
        let binding = *parsed.as_bytes();
        if form.configuration_binding.as_ref() != hex(&binding) {
            return Err(UiError::InvalidInput);
        }
        // The writer owns current gap/version/freshness checks. Do not replace
        // an exact successful receipt retry with a new live-state precheck.
        let reset = state
            .subscribers
            .reset_consent(ResetSubscriberConsent {
                expected_version: form.expected_version,
                configuration_binding: binding,
                audit: principal.mutation_audit(request_id, operation),
                now: OffsetDateTime::now_utc(),
            })
            .await
            .map_err(UiError::SubscriberMutation)?;
        Ok(admin_ui::page_response(
            StatusCode::OK,
            "Subscriptions removed",
            PageKind::Authenticated,
            html! {
                h1 { "Subscriptions removed; mail is paused" }
                p { (reset.discarded_enrollments) " enrollment records and " (reset.discarded_attempts) " recipient attempts were removed." }
                p { (reset.quarantined_campaigns) " unfinished campaigns were paused for review. Previously submitted messages cannot be recalled." }
                p { "Repair the feedback source before restarting mail. Readers must subscribe and confirm again; the old list will not receive a confirmation request." }
                p { a href="/admin/mail" { "Return to mail campaigns" } }
            },
        ))
    }
    .await;
    respond(request_id, result)
}

pub(super) fn mutation_error(error: SubscriberMutationError) -> (StatusCode, &'static str) {
    match error {
        SubscriberMutationError::Admission(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The reset was not admitted. Retry the original form with its original operation ID.",
        ),
        SubscriberMutationError::Command(command) => match command {
            SubscriberCommandError::Forbidden => (
                StatusCode::FORBIDDEN,
                "A fresh session from a currently enabled Owner is required.",
            ),
            SubscriberCommandError::OutcomeUnknown => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The reset outcome is unknown. Retry the original form in the same session with its original operation ID.",
            ),
            SubscriberCommandError::ConfigurationChanged => {
                UiError::ConfigurationChanged.description()
            }
            SubscriberCommandError::StaleVersion
            | SubscriberCommandError::ReconciliationNotRequired => (
                StatusCode::CONFLICT,
                "The recovery state changed. Inspect it before starting another operation.",
            ),
            SubscriberCommandError::IdempotencyConflict => (
                StatusCode::CONFLICT,
                "This operation ID belongs to a different form or session. Inspect mail status before starting another operation.",
            ),
            SubscriberCommandError::Capacity => (
                StatusCode::CONFLICT,
                "The recovery history limit has been reached. Contact the server operator.",
            ),
            SubscriberCommandError::InvalidValue => UiError::InvalidInput.description(),
            SubscriberCommandError::Paused
            | SubscriberCommandError::ControlIdentityChanged
            | SubscriberCommandError::ControlsUnavailable
            | SubscriberCommandError::AttemptConflict
            | SubscriberCommandError::CampaignUnavailable => UiError::Unavailable.description(),
        },
    }
}
