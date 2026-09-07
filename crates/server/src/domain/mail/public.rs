//! Public subscription controls. GET never changes consent. Tokens are accepted
//! only as purpose-bound bearer controls and are never echoed into page markup.
//! The sole writer orders consent, removal and sender admission; these handlers
//! never transmit mail or infer consent from provider state.

use std::sync::Arc;

use axum::{
    Form, Router,
    body::{Body, to_bytes},
    extract::{
        DefaultBodyLimit, FromRequest as _, Multipart, Path, Request, State,
        rejection::FormRejection,
    },
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{
            CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST, ORIGIN, REFERRER_POLICY,
            X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
        },
    },
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use maincopy_shared::auth_api::SecretString;
use markdown_compiler::PublicationBaseUrl;
use maud::{DOCTYPE, Markup, html};
use serde::Deserialize;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    config::{SesMailConfiguration, SubscriptionMode, SubscriptionPolicy},
    control::{ControlClaims, ControlPurpose},
    controls::MailControls,
    identity::EmailAddress,
    message::UNSUBSCRIBE_ROUTE,
    ses::SesCredentials,
    subscriber::{
        ConfirmEnrollment, ControlOutcome, EnrollmentRequestResult, FeedbackHealth,
        ManageEnrollment, RequestEnrollment, SubscriberCommandError, SubscriberDigest,
        SubscriberMode,
        store::{SubscriberMutationError, SubscriberStore},
    },
};

const SUBSCRIBE_ROUTE: &str = "/email/subscribe";
const CONFIRM_ROUTE: &str = "/email/confirm/{token}";
const MAX_FORM_BYTES: usize = 4096;
const PRIVATE_CSP: &str = "default-src 'none'; script-src 'none'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

/// Constructed only after startup admits the real controls and subscriber store.
/// The writer independently checks the current policy before accepting signup.
#[derive(Clone)]
pub(crate) struct PublicMailState {
    policy: SubscriptionPolicy,
    origin: PublicationBaseUrl,
    configuration_binding: [u8; 32],
    controls: Arc<MailControls>,
    subscribers: SubscriberStore,
}

impl PublicMailState {
    pub(super) fn new(
        configuration: SesMailConfiguration,
        credentials: &SesCredentials,
        policy: SubscriptionPolicy,
        origin: PublicationBaseUrl,
        controls: Arc<MailControls>,
        subscribers: SubscriberStore,
    ) -> Self {
        Self {
            policy,
            origin,
            configuration_binding: configuration.provider_binding(credentials),
            controls,
            subscribers,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubscribeForm {
    address: SecretString,
    consent: OptIn,
}

#[derive(Deserialize)]
enum OptIn {
    #[serde(rename = "yes")]
    Requested,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmForm {
    action: ConfirmAction,
}

#[derive(Deserialize)]
enum ConfirmAction {
    #[serde(rename = "confirm")]
    Confirm,
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum RemovalForm {
    Browser {
        action: RemovalAction,
    },
    OneClick {
        #[serde(rename = "List-Unsubscribe")]
        request: OneClick,
    },
}

#[derive(Deserialize)]
enum RemovalAction {
    #[serde(rename = "remove")]
    Remove,
}

#[derive(Deserialize)]
enum OneClick {
    #[serde(rename = "One-Click")]
    Requested,
}

pub(crate) fn router(state: PublicMailState) -> Router {
    Router::new()
        .route(
            SUBSCRIBE_ROUTE,
            get(subscribe_page).post(request_subscription),
        )
        .route(CONFIRM_ROUTE, get(confirm_page).post(confirm_subscription))
        .route(
            UNSUBSCRIBE_ROUTE,
            get(removal_page).post(remove_subscription),
        )
        .route_layer(middleware::from_fn(bounded_body))
        .route_layer(middleware::from_fn_with_state(state.clone(), validate_host))
        .layer(DefaultBodyLimit::max(MAX_FORM_BYTES))
        .with_state(state)
        .layer(middleware::from_fn(private_response))
}

async fn validate_host(
    State(state): State<PublicMailState>,
    request: Request,
    next: Next,
) -> Response {
    let authority = &state.origin.as_url()[url::Position::BeforeHost..url::Position::AfterPort];
    if !has_exact_header(request.headers(), HOST, authority)
        || request
            .uri()
            .authority()
            .is_some_and(|value| value.as_str() != authority)
        || request
            .uri()
            .scheme_str()
            .is_some_and(|value| value != "https")
    {
        return PublicError::WrongHost.into_response();
    }
    // No recipient identity or control state is accepted in query parameters.
    if request.uri().query().is_some() {
        return PublicError::InvalidForm.into_response();
    }
    next.run(request).await
}

async fn bounded_body(request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    match to_bytes(body, MAX_FORM_BYTES).await {
        Ok(bytes) => {
            next.run(Request::from_parts(parts, Body::from(bytes)))
                .await
        }
        Err(_) => PublicError::TooLarge.into_response(),
    }
}

async fn private_response(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(PRIVATE_CSP),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        "x-robots-tag",
        HeaderValue::from_static("noindex, nofollow, noarchive"),
    );
    response
}

fn has_exact_header(
    headers: &HeaderMap,
    name: axum::http::header::HeaderName,
    expected: &str,
) -> bool {
    let mut values = headers.get_all(name).iter();
    values
        .next()
        .is_some_and(|value| value.as_bytes() == expected.as_bytes())
        && values.next().is_none()
}

fn require_browser_origin(state: &PublicMailState, headers: &HeaderMap) -> Result<(), PublicError> {
    let expected = state.origin.as_url().origin().ascii_serialization();
    if has_exact_header(headers, ORIGIN, &expected) {
        Ok(())
    } else {
        Err(PublicError::WrongOrigin)
    }
}

async fn subscribe_page(State(state): State<PublicMailState>) -> Result<Response, PublicError> {
    let status = state
        .subscribers
        .status()
        .await
        .map_err(|_| PublicError::Unavailable)?;
    let enabled = state.policy.view().mode == SubscriptionMode::Enabled
        && status.feedback_health == FeedbackHealth::Healthy
        && status.policy.is_some_and(|policy| {
            policy.mode == SubscriberMode::Enabled
                && policy.configuration_binding == state.configuration_binding
        });
    Ok(page(
        StatusCode::OK,
        "Subscribe to article announcements",
        html! {
            (policy_notice(&state.policy))
            @if enabled {
                form method="post" {
                    p { label for="mail-address" { "Email address" } }
                    p { input id="mail-address" name="address" type="email" maxlength="254" autocomplete="email" required; }
                    p { label {
                        input name="consent" type="checkbox" value="yes" required;
                        " I want to receive these article announcements and the confirmation email."
                    } }
                    button type="submit" { "Request confirmation email" }
                }
            } @else {
                p { "New subscriptions are paused. Confirmation and removal links in existing messages still work." }
            }
        },
    ))
}

async fn request_subscription(
    State(state): State<PublicMailState>,
    headers: HeaderMap,
    form: Result<Form<SubscribeForm>, FormRejection>,
) -> Result<Response, PublicError> {
    require_browser_origin(&state, &headers)?;
    if matches!(state.policy.view().mode, SubscriptionMode::Paused) {
        return Err(PublicError::Paused);
    }
    let Form(SubscribeForm {
        address,
        consent: OptIn::Requested,
    }) = form.map_err(form_error)?;
    let address =
        EmailAddress::parse(address.expose_secret()).map_err(|_| PublicError::InvalidAddress)?;
    let mailbox_digest = SubscriberDigest::from_bytes(state.controls.mailbox_digest(&address));
    let outcome = state
        .subscribers
        .request_enrollment(RequestEnrollment {
            address,
            mailbox_digest,
            enrollment: Uuid::new_v4(),
            generation: Uuid::new_v4(),
            confirmation_attempt: Uuid::new_v4(),
            configuration_binding: state.configuration_binding,
        })
        .await
        .map_err(|error| match error {
            SubscriberMutationError::Command(SubscriberCommandError::Paused) => PublicError::Paused,
            SubscriberMutationError::Admission(_) | SubscriberMutationError::Command(_) => {
                PublicError::Unavailable
            }
        })?;
    match outcome {
        EnrollmentRequestResult::Queued | EnrollmentRequestResult::Unchanged => Ok(page(
            StatusCode::ACCEPTED,
            "Request received",
            html! {
                p { "If this address can subscribe, a confirmation email will be sent. Follow the link in that email to confirm your request." }
                (policy_notice(&state.policy))
            },
        )),
    }
}

async fn confirm_page(
    State(state): State<PublicMailState>,
    path: Result<Path<SecretString>, axum::extract::rejection::PathRejection>,
) -> Result<Response, PublicError> {
    verify_token(&state, path, ControlPurpose::Confirm)?;
    Ok(page(
        StatusCode::OK,
        "Confirm your subscription",
        html! {
            (policy_notice(&state.policy))
            p { "Opening this page does not subscribe you. Confirm below only if you requested these article announcements." }
            form method="post" {
                button type="submit" name="action" value="confirm" { "Confirm subscription" }
            }
        },
    ))
}

async fn confirm_subscription(
    State(state): State<PublicMailState>,
    headers: HeaderMap,
    path: Result<Path<SecretString>, axum::extract::rejection::PathRejection>,
    form: Result<Form<ConfirmForm>, FormRejection>,
) -> Result<Response, PublicError> {
    require_browser_origin(&state, &headers)?;
    let Form(ConfirmForm {
        action: ConfirmAction::Confirm,
    }) = form.map_err(form_error)?;
    let claims = verify_token(&state, path, ControlPurpose::Confirm)?;
    let ControlClaims::Confirm {
        enrollment,
        generation,
        confirmation_nonce,
        expires_at,
    } = claims
    else {
        return Err(PublicError::InvalidLink);
    };
    let nonce_digest =
        SubscriberDigest::from_bytes(MailControls::confirmation_digest(&confirmation_nonce));
    let outcome = state
        .subscribers
        .confirm(ConfirmEnrollment {
            enrollment,
            generation,
            nonce_digest,
            expires_at,
        })
        .await
        .map_err(|_| PublicError::Unavailable)?;
    match outcome {
        ControlOutcome::Changed | ControlOutcome::Unchanged => Ok(page(
            StatusCode::OK,
            "Confirmation processed",
            html! {
                p { "If this invitation is current, your subscription is now confirmed. An expired or replaced invitation cannot activate a subscription." }
                (policy_notice(&state.policy))
            },
        )),
    }
}

async fn removal_page(
    State(state): State<PublicMailState>,
    path: Result<Path<SecretString>, axum::extract::rejection::PathRejection>,
) -> Result<Response, PublicError> {
    verify_token(&state, path, ControlPurpose::Manage)?;
    Ok(page(
        StatusCode::OK,
        "Unsubscribe and remove your address",
        html! {
            p { "Opening this page does not change your subscription." }
            p { "Use the button to stop future mail for this signup and remove your address from the active mailing records. A newer signup is unaffected by an older removal link." }
            p { "Messages already being submitted may still arrive. Backup and provider copies follow the retention policy below." }
            form method="post" {
                button type="submit" name="action" value="remove" { "Unsubscribe and remove my address" }
            }
            (policy_notice(&state.policy))
        },
    ))
}

async fn remove_subscription(
    State(state): State<PublicMailState>,
    path: Result<Path<SecretString>, axum::extract::rejection::PathRejection>,
    request: Request,
) -> Result<Response, PublicError> {
    let browser_origin = require_browser_origin(&state, request.headers());
    let form = removal_form(request).await?;
    match form {
        RemovalForm::Browser {
            action: RemovalAction::Remove,
        } => browser_origin?,
        RemovalForm::OneClick {
            request: OneClick::Requested,
        } => {}
    }
    let claims = verify_token(&state, path, ControlPurpose::Manage)?;
    let ControlClaims::Manage {
        enrollment,
        generation,
    } = claims
    else {
        return Err(PublicError::InvalidLink);
    };
    let outcome = state
        .subscribers
        .remove(ManageEnrollment {
            enrollment,
            generation,
        })
        .await
        .map_err(|_| PublicError::Unavailable)?;
    match outcome {
        ControlOutcome::Changed | ControlOutcome::Unchanged => Ok(page(
            StatusCode::OK,
            "Removal processed",
            html! {
                p { "This link no longer authorizes future mail. A newer signup is unaffected. Messages already being submitted may still arrive." }
                p { "Backup and provider copies follow the retention policy." }
                (policy_notice(&state.policy))
            },
        )),
    }
}

async fn removal_form(request: Request) -> Result<RemovalForm, PublicError> {
    let mut content_types = request.headers().get_all(CONTENT_TYPE).iter();
    let content_type = content_types
        .next()
        .ok_or(PublicError::InvalidForm)?
        .to_str()
        .map_err(|_| PublicError::InvalidForm)?;
    if content_types.next().is_some() {
        return Err(PublicError::InvalidForm);
    }
    let media_type = content_type
        .split(';')
        .next()
        .ok_or(PublicError::InvalidForm)?
        .trim();
    if media_type.eq_ignore_ascii_case("multipart/form-data") {
        return multipart_one_click(request).await;
    }
    Form::<RemovalForm>::from_request(request, &())
        .await
        .map(|Form(form)| form)
        .map_err(form_error)
}

async fn multipart_one_click(request: Request) -> Result<RemovalForm, PublicError> {
    let mut multipart = Multipart::from_request(request, &())
        .await
        .map_err(|_| PublicError::InvalidForm)?;
    let field = multipart
        .next_field()
        .await
        .map_err(|_| PublicError::InvalidForm)?
        .ok_or(PublicError::InvalidForm)?;
    if field.name() != Some("List-Unsubscribe") || field.file_name().is_some() {
        return Err(PublicError::InvalidForm);
    }
    let value = Zeroizing::new(field.text().await.map_err(|_| PublicError::InvalidForm)?);
    if value.as_str() != "One-Click"
        || multipart
            .next_field()
            .await
            .map_err(|_| PublicError::InvalidForm)?
            .is_some()
    {
        return Err(PublicError::InvalidForm);
    }
    Ok(RemovalForm::OneClick {
        request: OneClick::Requested,
    })
}

fn verify_token(
    state: &PublicMailState,
    path: Result<Path<SecretString>, axum::extract::rejection::PathRejection>,
    purpose: ControlPurpose,
) -> Result<ControlClaims, PublicError> {
    let Path(token) = path.map_err(|_| PublicError::InvalidLink)?;
    state
        .controls
        .verify(purpose, token.expose_secret(), OffsetDateTime::now_utc())
        .map_err(|_| PublicError::InvalidLink)
}

fn policy_notice(policy: &SubscriptionPolicy) -> Markup {
    let view = policy.view();
    html! {
        p { "Operator: " (view.operator_name) }
        p { "Postal address: " (view.postal_address) }
        p { (view.purpose) }
        p { "Your address is stored by this site and processed by Amazon SES to deliver confirmation and announcement emails." }
        p { "Contact: " (view.contact_address.as_str()) }
        p { a href=(view.privacy_url.as_str()) rel="noreferrer" { "Privacy and retention policy" } }
    }
}

fn page(status: StatusCode, title: &str, content: Markup) -> Response {
    // Content includes only public policy and static text. Forms deliberately
    // omit action: the browser posts to the current token URL without copying
    // the bearer value into a response, hidden field or third-party resource.
    let document = html! {
        (DOCTYPE)
        html lang="en" {
            head { meta charset="utf-8"; meta name="viewport" content="width=device-width, initial-scale=1"; title { (title) } }
            body { main { h1 { (title) } (content) } }
        }
    };
    (status, Html(document.into_string())).into_response()
}

fn form_error(error: FormRejection) -> PublicError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        PublicError::TooLarge
    } else {
        PublicError::InvalidForm
    }
}

#[derive(Debug, Error)]
enum PublicError {
    #[error("the public request host does not match the configured HTTPS origin")]
    WrongHost,
    #[error("the browser request origin does not match the configured HTTPS origin")]
    WrongOrigin,
    #[error("the subscription form is invalid")]
    InvalidForm,
    #[error("the email address is unsupported")]
    InvalidAddress,
    #[error("the subscriber control link is invalid or expired")]
    InvalidLink,
    #[error("the subscription form exceeds the byte bound")]
    TooLarge,
    #[error("new subscription requests are paused")]
    Paused,
    #[error("the subscriber operation is unavailable or its outcome is not confirmed")]
    Unavailable,
}

impl IntoResponse for PublicError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::WrongHost => (
                StatusCode::MISDIRECTED_REQUEST,
                "Use the site's configured public HTTPS address.",
            ),
            Self::WrongOrigin => (
                StatusCode::FORBIDDEN,
                "Submit this form from the site's own HTTPS page.",
            ),
            Self::InvalidForm => (
                StatusCode::BAD_REQUEST,
                "The form could not be accepted. Use the fields and action shown on this page.",
            ),
            Self::InvalidAddress => (StatusCode::BAD_REQUEST, "Enter a valid email address."),
            Self::InvalidLink => (
                StatusCode::BAD_REQUEST,
                "This link is invalid, expired, or intended for a different action. Use the link in the original email.",
            ),
            Self::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "This form exceeds the request limit.",
            ),
            Self::Paused => (
                StatusCode::SERVICE_UNAVAILABLE,
                "New subscriptions are paused. Existing confirmation and removal links remain available.",
            ),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The request outcome could not be confirmed. Try the same form or original email link again before starting a new request.",
            ),
        };
        page(status, "Request not completed", html! { p { (message) } })
    }
}

#[cfg(test)]
#[path = "public/tests.rs"]
mod tests;
