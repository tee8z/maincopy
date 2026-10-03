//! Owner browser management of sharing channels and their teasers. Saved
//! credentials are accepted here and never rendered back.

use axum::{
    Extension, Form, Router,
    extract::{
        DefaultBodyLimit, Path, Request,
        rejection::{FormRejection, PathRejection},
    },
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use maincopy_shared::{
    auth::{UserRole, UserStatus},
    auth_api::SecretString,
};
use markdown_compiler::PostId;
use maud::{Markup, html};
use serde::Deserialize;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use super::{
    settings::{
        Channel, ChannelMode, SettingsError, StoredChannel, SubstackSubdomain, UpdateSubstack,
        substack_session,
    },
    store::{
        Delivery, DeliveryFailure, DeliveryState, ShareTeaser, SharedTeaser, SharingCommandError,
        SharingMutationError, SharingStore,
    },
};
use crate::{
    admin::{
        AdminRuntimeState, AdminSecurityState, BrowserFormSession, RequiredBrowserSession,
        browser_session_router,
        principal::AdminPrincipal,
        request_id::RequestId,
        ui::{self as admin_ui, PageKind},
    },
    domain::auth::store::AdminMutationKey,
};

const RECENT_TEASERS: u32 = 20;
const MAX_FORM_BYTES: usize = 8192;

#[derive(Clone)]
pub(crate) struct SharingUiState {
    pub store: SharingStore,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubstackForm {
    #[serde(rename = "_csrf")]
    _csrf: SecretString,
    idempotency_key: Box<str>,
    expected_version: u64,
    mode: ChannelMode,
    subdomain: Box<str>,
    session: SecretString,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareForm {
    #[serde(rename = "_csrf")]
    _csrf: SecretString,
    idempotency_key: Box<str>,
}

/// The article ID and channel name of one delivery.
type SharePath = Path<(Box<str>, Box<str>)>;

/// What the page may say about a channel. Credentials stay in the store.
#[derive(Clone, Copy)]
struct ChannelStatus {
    version: u64,
    mode: ChannelMode,
    credentials_rejected: bool,
}

impl ChannelStatus {
    fn of<Settings>(stored: &StoredChannel<Settings>, mode: ChannelMode) -> Self {
        Self {
            version: stored.version,
            mode,
            credentials_rejected: stored.credentials_rejected,
        }
    }
}

pub(crate) fn router(
    security: &AdminSecurityState,
    state: SharingUiState,
) -> Router<AdminRuntimeState> {
    browser_session_router(
        Router::new()
            .route("/admin/sharing", get(overview))
            .route("/admin/sharing/substack", post(save_substack))
            .route("/admin/sharing/teasers/{post_id}/{channel}", post(share))
            .route_layer(middleware::from_fn(require_owner))
            .layer(DefaultBodyLimit::max(MAX_FORM_BYTES))
            .layer(Extension(state)),
        security,
    )
    .layer(middleware::from_fn(admin_ui::adapt_security_response))
}

async fn require_owner(browser: RequiredBrowserSession, request: Request, next: Next) -> Response {
    match browser.security.store.user(browser.session.user_id).await {
        Ok(Some(user))
            if user.status == UserStatus::Enabled && user.roles.contains(&UserRole::Owner) =>
        {
            next.run(request).await
        }
        Ok(_) => admin_ui::error_response(
            StatusCode::FORBIDDEN,
            "Owner access required",
            "Only a currently enabled Owner can manage article sharing.",
            browser.request_id,
        ),
        Err(_) => UiError::Unavailable.response(browser.request_id),
    }
}

async fn overview(
    request_id: RequestId,
    Extension(state): Extension<SharingUiState>,
    browser: BrowserFormSession,
) -> Response {
    respond(request_id, overview_page(&state, &browser).await)
}

async fn overview_page(
    state: &SharingUiState,
    browser: &BrowserFormSession,
) -> Result<Response, UiError> {
    let substack = state
        .store
        .substack()
        .await
        .map_err(|_| UiError::Unavailable)?;
    let teasers = state
        .store
        .recent(RECENT_TEASERS)
        .await
        .map_err(|_| UiError::Unavailable)?;
    let subdomain = substack
        .as_ref()
        .map_or("", |stored| stored.settings.subdomain.as_str());
    let substack = substack
        .as_ref()
        .map(|stored| ChannelStatus::of(stored, stored.settings.mode));
    let csrf = browser.csrf_token.expose_secret();
    let fresh = browser.session.fresh_until > OffsetDateTime::now_utc();
    Ok(admin_ui::page_response(
        StatusCode::OK,
        "Sharing",
        PageKind::Authenticated,
        html! {
            h1 { "Sharing" }
            p { "When an article is published for the first time, Maincopy writes one short teaser: a post text and the article's link. Substack receives it automatically while enabled. For X or anywhere else, copy the post text below and add the link under it or in a reply; together they always fit a single X post. Edits and republishing never share an article again." }
            @if !fresh { p class="notice" { "Sign out and sign in again before saving settings or sharing a teaser. Your session is no longer fresh." } }
            section class="panel" {
                h2 { "Substack" }
                (channel_summary(Channel::Substack, substack))
                p class="muted" { "Substack has no publishing API, so Maincopy signs in as you with your browser's session cookie. The cookie gives full access to your Substack account, lasts some months, and Substack can change how this works at any time. Teasers are published to the web only; Substack emails nobody." }
                form method="post" action="/admin/sharing/substack" {
                    (form_preamble(csrf, substack))
                    (mode_field("substack-mode", substack))
                    p { label for="substack-subdomain" { "Publication address" }
                        input id="substack-subdomain" name="subdomain" value=(subdomain) maxlength="200" placeholder="example.substack.com" required;
                    }
                    p { label for="substack-session" { "Session cookie (substack.sid)" }
                        input id="substack-session" name="session" type="password" autocomplete="off" maxlength="1100" placeholder=(secret_placeholder(substack));
                    }
                    button type="submit" disabled[!fresh] { "Save Substack" }
                }
            }
            section class="panel" {
                h2 { "Teasers" }
                @if teasers.is_empty() {
                    p { "No article has been published since sharing was added. The next first publication appears here." }
                }
                @for teaser in &teasers {
                    (teaser_panel(teaser, csrf, fresh, [(Channel::Substack, substack)]))
                }
            }
        },
    ))
}

fn form_preamble(csrf: &str, status: Option<ChannelStatus>) -> Markup {
    html! {
        input type="hidden" name="_csrf" value=(csrf);
        input type="hidden" name="idempotency_key" value=(Uuid::new_v4());
        input type="hidden" name="expected_version" value=(status.map_or(0, |status| status.version));
    }
}

fn mode_field(id: &str, status: Option<ChannelStatus>) -> Markup {
    let enabled = status.is_some_and(|status| status.mode == ChannelMode::Enabled);
    html! {
        p { label for=(id) { "Automatic sharing" }
            select id=(id) name="mode" {
                option value="paused" selected[!enabled] { "Paused" }
                option value="enabled" selected[enabled] { "Enabled" }
            }
        }
    }
}

fn secret_placeholder(status: Option<ChannelStatus>) -> &'static str {
    match status {
        Some(_) => "Saved. Leave blank to keep it.",
        None => "",
    }
}

fn channel_summary(channel: Channel, status: Option<ChannelStatus>) -> Markup {
    html! {
        @match status {
            None => { p { "Not set up." } }
            Some(status) if status.credentials_rejected => {
                p class="notice" { (channel.label()) " refused the saved credentials. Queued teasers wait until you save new ones." }
            }
            Some(status) if status.mode == ChannelMode::Enabled => {
                p { "Enabled. Newly published articles are shared automatically." }
            }
            Some(_) => { p { "Paused. Nothing is sent, and queued teasers wait." } }
        }
    }
}

fn teaser_panel(
    shared: &SharedTeaser,
    csrf: &str,
    fresh: bool,
    channels: [(Channel, Option<ChannelStatus>); 1],
) -> Markup {
    let view = shared.teaser.view();
    html! {
        article {
            h3 { (view.title) }
            p class="muted" { "First published " (timestamp(shared.created_at)) }
            p { label for=(format!("teaser-text-{}", view.post_id)) { "Post text" }
                textarea id=(format!("teaser-text-{}", view.post_id)) readonly rows="5" { (shared.teaser.lead()) }
            }
            p { label for=(format!("teaser-link-{}", view.post_id)) { "Article link" }
                input id=(format!("teaser-link-{}", view.post_id)) readonly value=(view.url);
            }
            dl {
                @for (channel, status) in channels {
                    @let delivery = shared.deliveries.iter().find(|delivery| delivery.channel == channel);
                    dt { (channel.label()) }
                    dd {
                        (delivery_summary(channel, delivery))
                        @if let Some(action) = share_action(delivery, status) {
                            form method="post" action=(format!("/admin/sharing/teasers/{}/{}", view.post_id, channel.as_str())) {
                                input type="hidden" name="_csrf" value=(csrf);
                                input type="hidden" name="idempotency_key" value=(Uuid::new_v4());
                                button type="submit" disabled[!fresh] { (action) " " (channel.label()) }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A teaser can be shared to a channel it missed, or again after a failure.
fn share_action(
    delivery: Option<&Delivery>,
    status: Option<ChannelStatus>,
) -> Option<&'static str> {
    status?;
    match delivery.map(|delivery| &delivery.state) {
        None => Some("Share on"),
        Some(DeliveryState::Failed(_)) => Some("Try again on"),
        Some(
            DeliveryState::Queued { .. } | DeliveryState::Sending | DeliveryState::Posted { .. },
        ) => None,
    }
}

fn delivery_summary(channel: Channel, delivery: Option<&Delivery>) -> Markup {
    let label = channel.label();
    html! {
        @match delivery.map(|delivery| &delivery.state) {
            None => { "Not shared. " }
            Some(DeliveryState::Queued { retry_after }) if *retry_after > OffsetDateTime::now_utc() => {
                "Waiting to retry after " (timestamp(*retry_after)) ". "
            }
            Some(DeliveryState::Queued { .. }) => { "Queued. " }
            Some(DeliveryState::Sending) => { "Sending now. " }
            Some(DeliveryState::Posted { url }) => {
                "Posted: " a href=(url) rel="noreferrer" { (url) } " "
            }
            Some(DeliveryState::Failed(DeliveryFailure::Interrupted)) => {
                "The request was cut off before " (label) " answered, so the post may already exist. Check " (label) " before trying again. "
            }
            Some(DeliveryState::Failed(DeliveryFailure::Refused)) => {
                (label) " refused the post. "
            }
            Some(DeliveryState::Failed(DeliveryFailure::UnexpectedResponse)) => {
                (label) " answered in a way Maincopy does not understand. "
            }
            Some(DeliveryState::Failed(DeliveryFailure::RetriesExhausted)) => {
                (label) " stayed unavailable for about an hour. "
            }
        }
    }
}

fn timestamp(value: OffsetDateTime) -> String {
    value
        .format(&Rfc3339)
        .unwrap_or_else(|_| "Time unavailable".to_owned())
}

async fn save_substack(
    request_id: RequestId,
    principal: AdminPrincipal,
    Extension(state): Extension<SharingUiState>,
    form: Result<Form<SubstackForm>, FormRejection>,
) -> Response {
    let result = async {
        let form = decode_form(form)?;
        let session = match form.session.expose_secret().trim() {
            "" => None,
            session => Some(substack_session(session).map_err(UiError::Settings)?),
        };
        if session.is_none() && form.expected_version == 0 {
            return Err(UiError::Settings(SettingsError::CredentialsRequired));
        }
        state
            .store
            .update_substack(UpdateSubstack {
                expected_version: form.expected_version,
                mode: form.mode,
                subdomain: SubstackSubdomain::parse(&form.subdomain).map_err(UiError::Settings)?,
                session,
                audit: principal.mutation_audit(request_id, operation_key(&form.idempotency_key)?),
            })
            .await
            .map_err(UiError::Mutation)?;
        Ok(saved_page("Substack settings saved"))
    }
    .await;
    respond(request_id, result)
}

async fn share(
    request_id: RequestId,
    principal: AdminPrincipal,
    Extension(state): Extension<SharingUiState>,
    path: Result<SharePath, PathRejection>,
    form: Result<Form<ShareForm>, FormRejection>,
) -> Response {
    let result = async {
        let Path((post_id, channel)) = path.map_err(|_| UiError::InvalidInput)?;
        let form = decode_form(form)?;
        state
            .store
            .share(ShareTeaser {
                post_id: PostId::parse(&post_id).map_err(|_| UiError::InvalidInput)?,
                channel: Channel::parse(&channel).ok_or(UiError::InvalidInput)?,
                audit: principal.mutation_audit(request_id, operation_key(&form.idempotency_key)?),
            })
            .await
            .map_err(UiError::Mutation)?;
        Ok(saved_page("Teaser queued"))
    }
    .await;
    respond(request_id, result)
}

fn saved_page(title: &str) -> Response {
    admin_ui::page_response(
        StatusCode::OK,
        title,
        PageKind::Authenticated,
        html! {
            h1 { (title) }
            p { "Queued teasers are sent within a minute while their channel is enabled." }
            p { a href="/admin/sharing" { "Return to sharing" } }
        },
    )
}

fn decode_form<T>(form: Result<Form<T>, FormRejection>) -> Result<T, UiError> {
    form.map(|Form(value)| value)
        .map_err(|error| match error.status() {
            StatusCode::PAYLOAD_TOO_LARGE => UiError::TooLarge,
            _ => UiError::InvalidInput,
        })
}

fn operation_key(value: &str) -> Result<AdminMutationKey, UiError> {
    let id = Uuid::parse_str(value).map_err(|_| UiError::InvalidInput)?;
    if id.to_string() != value {
        return Err(UiError::InvalidInput);
    }
    Ok(AdminMutationKey(id))
}

#[derive(Debug, Error)]
enum UiError {
    #[error("the sharing form or route is invalid")]
    InvalidInput,
    #[error("the sharing form exceeds its byte bound")]
    TooLarge,
    #[error("sharing state is unavailable")]
    Unavailable,
    #[error("the sharing settings are invalid")]
    Settings(#[source] SettingsError),
    #[error("apply the sharing mutation")]
    Mutation(#[source] SharingMutationError),
}

impl UiError {
    fn response(self, request_id: RequestId) -> Response {
        if let Self::Settings(error) = &self {
            return admin_ui::page_response(
                StatusCode::BAD_REQUEST,
                "Check sharing settings",
                PageKind::Authenticated,
                html! {
                    h1 { "Check sharing settings" }
                    p { (error) }
                    p { "No settings were changed." }
                    p { a href="/admin/sharing" { "Return to sharing" } }
                },
            );
        }
        let (status, message) = self.description();
        admin_ui::error_response(status, "Sharing request not completed", message, request_id)
    }

    fn description(self) -> (StatusCode, &'static str) {
        match self {
            Self::InvalidInput | Self::Settings(_) => (
                StatusCode::BAD_REQUEST,
                "The sharing form or address is invalid. Return to sharing and review the current values.",
            ),
            Self::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "The sharing form is too large. Return to sharing and try again.",
            ),
            Self::Mutation(SharingMutationError::Command(SharingCommandError::Forbidden)) => (
                StatusCode::FORBIDDEN,
                "Sign in again as an enabled Owner before changing sharing.",
            ),
            Self::Mutation(SharingMutationError::Command(
                SharingCommandError::StaleVersion | SharingCommandError::IdempotencyConflict,
            )) => (
                StatusCode::CONFLICT,
                "The sharing settings changed. Reload sharing before saving again.",
            ),
            Self::Mutation(SharingMutationError::Command(SharingCommandError::InvalidValue)) => (
                StatusCode::BAD_REQUEST,
                "Enter the channel's credentials before saving it for the first time.",
            ),
            Self::Mutation(SharingMutationError::Command(SharingCommandError::NotFound)) => (
                StatusCode::NOT_FOUND,
                "That teaser does not exist. Return to sharing and review the current teasers.",
            ),
            Self::Mutation(SharingMutationError::Command(SharingCommandError::StateConflict)) => (
                StatusCode::CONFLICT,
                "That teaser is already queued, sending, or posted on this channel.",
            ),
            Self::Mutation(SharingMutationError::Command(SharingCommandError::Capacity)) => (
                StatusCode::CONFLICT,
                "The sharing history limit has been reached. Contact the server operator.",
            ),
            Self::Unavailable
            | Self::Mutation(
                SharingMutationError::Admission(_)
                | SharingMutationError::Command(SharingCommandError::OutcomeUnknown),
            ) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The request could not be confirmed. Reload sharing to see the current state before trying again.",
            ),
        }
    }
}

fn respond(request_id: RequestId, result: Result<Response, UiError>) -> Response {
    result.unwrap_or_else(|error| error.response(request_id))
}

#[cfg(test)]
#[path = "ui/tests.rs"]
mod tests;
