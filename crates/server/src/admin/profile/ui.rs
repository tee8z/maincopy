use std::str::FromStr;

use axum::{
    Form, Router,
    extract::{DefaultBodyLimit, State, rejection::FormRejection},
    http::StatusCode,
    middleware,
    response::Response,
    routing::get,
};
use maincopy_shared::{
    auth::{AdminScope, UserId},
    auth_api::SecretString,
    profile::{LightningAddress, ProfileDisplayName, ProfileVersion},
};
use maud::{Markup, html};
use serde::{Deserialize, Deserializer, de::Error as _};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{PROFILE_REQUEST_BODY_LIMIT, load_problem, transition_problem};
use crate::{
    admin::{
        AdminRuntimeState, AdminSecurityState, BrowserFormSession, browser_scoped_router,
        principal::AdminPrincipal,
        request_id::RequestId,
        ui::{PageKind, adapt_security_response, mutation_error_response, page_response, redirect},
    },
    domain::{
        auth::store::AdminMutationKey,
        profile::{
            ProfilePrecondition, ProfileStore, SetTipRecipient, StoredUserProfile, UpdateProfile,
        },
        publication::activation::PublicationCoordinatorHandle,
    },
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileForm {
    _csrf: SecretString,
    operation_id: Uuid,
    expected_version: Option<ProfileVersion>,
    #[serde(deserialize_with = "optional_text")]
    display_name: Option<ProfileDisplayName>,
    #[serde(deserialize_with = "optional_text")]
    lightning_address: Option<LightningAddress>,
    tips_enabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TipRecipientForm {
    _csrf: SecretString,
    operation_id: Uuid,
    expected_version: ProfileVersion,
    #[serde(deserialize_with = "optional_text")]
    user_id: Option<Uuid>,
}

fn optional_text<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let value = Box::<str>::deserialize(deserializer)?;
    if value.is_empty() {
        Ok(None)
    } else {
        value.parse().map(Some).map_err(D::Error::custom)
    }
}

pub(in crate::admin) fn browser_router(security: &AdminSecurityState) -> Router<AdminRuntimeState> {
    browser_scoped_router(
        Router::new().route("/admin/profile", get(show_profile).post(save_profile)),
        security,
        AdminScope::ProfileManage,
    )
    .merge(browser_scoped_router(
        Router::new().route(
            "/admin/tips",
            get(show_tip_recipient).post(save_tip_recipient),
        ),
        security,
        AdminScope::LightningManage,
    ))
    .layer(DefaultBodyLimit::max(PROFILE_REQUEST_BODY_LIMIT))
    .layer(middleware::from_fn(adapt_security_response))
}

async fn show_profile(
    request_id: RequestId,
    browser: BrowserFormSession,
    State(store): State<ProfileStore>,
) -> Response {
    let profile = match store.profile(browser.session.user_id).await {
        Ok(profile) => profile,
        Err(error) => {
            return mutation_error_response(
                load_problem(error, request_id).status,
                "/admin/profile",
                request_id,
            );
        }
    };
    page_response(
        StatusCode::OK,
        "Your profile",
        PageKind::Authenticated,
        html! {
            section class="panel" {
                h1 { "Your profile" }
                p { "Set your public display name and Lightning Address. These settings do not approve or release articles." }
                p class="muted" { "User: " code { (browser.session.user_id) } }
                @if profile.is_none() {
                    p { "You have not configured a profile yet." }
                }
                (profile_form(&browser, profile.as_ref()))
                p { a href="/admin/tips" { "Manage the site's active tip recipient" } }
            }
        },
    )
}

fn profile_form(browser: &BrowserFormSession, profile: Option<&StoredUserProfile>) -> Markup {
    let display_name = profile.and_then(|profile| profile.display_name.as_ref());
    let address = profile.and_then(|profile| profile.lightning_address.as_ref());
    let tips_enabled = profile.is_some_and(|profile| profile.tips_enabled);
    html! {
        form method="post" action="/admin/profile" {
            input type="hidden" name="_csrf" value=(browser.csrf_token.expose_secret());
            input type="hidden" name="operation_id" value=(Uuid::new_v4());
            @if let Some(profile) = profile {
                input type="hidden" name="expected_version" value=(profile.version.into_u64());
                p class="muted" { "Profile version: " (profile.version.into_u64()) }
            }
            label for="display_name" { "Public display name" }
            input id="display_name" name="display_name" type="text" maxlength="160"
                value=(display_name.map_or("", ProfileDisplayName::as_str));
            label for="lightning_address" { "Lightning Address" }
            input id="lightning_address" name="lightning_address" type="text" maxlength="320"
                placeholder="name@example.com" value=(address.map_or("", LightningAddress::as_str));
            p class="muted" { "Leave either field empty to clear it. Use a lowercase Lightning Address." }
            label for="tips_enabled" { "Accept tips when selected as the site recipient" }
            select id="tips_enabled" name="tips_enabled" required {
                option value="false" selected[!tips_enabled] { "No" }
                option value="true" selected[tips_enabled] { "Yes" }
            }
            button type="submit" { "Save profile" }
        }
    }
}

async fn save_profile(
    request_id: RequestId,
    principal: AdminPrincipal,
    State(coordinator): State<PublicationCoordinatorHandle>,
    form: Result<Form<ProfileForm>, FormRejection>,
) -> Response {
    let status = match form {
        Err(error) => error.status(),
        Ok(Form(form)) => {
            let result = coordinator
                .update_profile(UpdateProfile {
                    user_id: principal.user_id,
                    precondition: ProfilePrecondition::from(form.expected_version),
                    display_name: form.display_name,
                    lightning_address: form.lightning_address,
                    tips_enabled: form.tips_enabled,
                    occurred_at: OffsetDateTime::now_utc(),
                    audit: principal
                        .mutation_audit(request_id, AdminMutationKey(form.operation_id)),
                })
                .await;
            match result {
                Ok(_) => return redirect("/admin/profile"),
                Err(error) => transition_problem(error, request_id).status,
            }
        }
    };
    mutation_error_response(status, "/admin/profile", request_id)
}

async fn show_tip_recipient(
    request_id: RequestId,
    browser: BrowserFormSession,
    State(store): State<ProfileStore>,
) -> Response {
    let setting = match store.active_tip_recipient().await {
        Ok(setting) => setting,
        Err(error) => {
            return mutation_error_response(
                load_problem(error, request_id).status,
                "/admin/tips",
                request_id,
            );
        }
    };
    let recipient = match store.effective_tip_recipient().await {
        Ok(recipient) => recipient,
        Err(error) => {
            return mutation_error_response(
                load_problem(error, request_id).status,
                "/admin/tips",
                request_id,
            );
        }
    };
    page_response(
        StatusCode::OK,
        "Tip recipient",
        PageKind::Authenticated,
        html! {
            section class="panel" {
                h1 { "Active tip recipient" }
                @if let Some(recipient) = recipient {
                    p { "Tips are available for articles that enable them. Recipient: "
                        strong { (recipient.as_view().address) }
                    }
                } @else if setting.recipient_user_id.is_some() {
                    p { "The selected recipient is ineligible. The account must be enabled, with tips enabled and a valid Lightning Address in its profile. Articles remain readable." }
                } @else {
                    p { "No tip recipient is selected. Articles remain readable without a tip link." }
                }
                p class="muted" { "Setting version: " (setting.version.into_u64()) }
                p { "Your user ID: " code { (browser.session.user_id) } }
                form method="post" action="/admin/tips" {
                    input type="hidden" name="_csrf" value=(browser.csrf_token.expose_secret());
                    input type="hidden" name="operation_id" value=(Uuid::new_v4());
                    input type="hidden" name="expected_version" value=(setting.version.into_u64());
                    label for="user_id" { "Recipient user ID" }
                    input id="user_id" name="user_id" type="text" maxlength="36"
                        value=(setting.recipient_user_id.map_or_else(String::new, |id| id.to_string()));
                    p class="muted" { "Leave this field empty to remove the active recipient." }
                    button type="submit" { "Save recipient" }
                }
                p { a href="/admin/profile" { "Edit your profile and Lightning Address" } }
            }
        },
    )
}

async fn save_tip_recipient(
    request_id: RequestId,
    principal: AdminPrincipal,
    State(coordinator): State<PublicationCoordinatorHandle>,
    form: Result<Form<TipRecipientForm>, FormRejection>,
) -> Response {
    let status = match form {
        Err(error) => error.status(),
        Ok(Form(form)) => {
            match coordinator
                .set_tip_recipient(SetTipRecipient {
                    expected_version: form.expected_version,
                    recipient_user_id: form.user_id.map(UserId::from_uuid),
                    occurred_at: OffsetDateTime::now_utc(),
                    audit: principal
                        .mutation_audit(request_id, AdminMutationKey(form.operation_id)),
                })
                .await
            {
                Ok(_) => return redirect("/admin/tips"),
                Err(error) => transition_problem(error, request_id).status,
            }
        }
    };
    mutation_error_response(status, "/admin/tips", request_id)
}
