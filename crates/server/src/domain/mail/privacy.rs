//! A public notice uses the saved newsletter details without loading provider secrets.

use axum::{
    Router, extract::State, http::StatusCode, middleware, response::Response, routing::get,
};
use markdown_compiler::PublicationBaseUrl;
use maud::html;
use url::Url;

use super::{
    config::{MailConfiguration, SubscriptionPolicy},
    public::{page, private_response},
    subscriber::store::SubscriberStore,
};

pub(super) const NOTICE_PATH: &str = "/email/privacy";

#[derive(Clone)]
struct PrivacyState {
    subscribers: SubscriberStore,
    fallback: Option<SubscriptionPolicy>,
}

pub(super) fn notice_url(origin: &PublicationBaseUrl) -> Url {
    let mut url = origin.as_url().clone();
    url.set_path(NOTICE_PATH);
    url.set_query(None);
    url.set_fragment(None);
    url
}

pub(crate) fn router(subscribers: SubscriberStore, configuration: &MailConfiguration) -> Router {
    let fallback = match configuration {
        MailConfiguration::Disabled => None,
        MailConfiguration::Ses(configuration) => configuration.view().subscriptions.cloned(),
    };
    Router::new()
        .route(NOTICE_PATH, get(notice))
        .with_state(PrivacyState {
            subscribers,
            fallback,
        })
        .layer(middleware::from_fn(private_response))
}

async fn notice(State(state): State<PrivacyState>) -> Response {
    let policy = match state.subscribers.mail_settings().await {
        Ok(Some(stored)) => Some(stored.settings.view().subscriptions.clone()),
        Ok(None) => state.fallback,
        Err(_) => {
            return page(
                StatusCode::SERVICE_UNAVAILABLE,
                "Newsletter privacy",
                html! {
                    p { "Newsletter details are temporarily unavailable. Please try again later." }
                },
            );
        }
    };
    page(
        StatusCode::OK,
        "Newsletter privacy",
        html! {
            @if let Some(policy) = policy.as_ref().map(SubscriptionPolicy::view) {
                p { "This newsletter is operated by " (policy.operator_name) "." }
                p { (policy.purpose) }
                p { "For questions about your subscription or information, contact "
                    a href=(format!("mailto:{}", policy.contact_address.as_str())) { (policy.contact_address.as_str()) } "."
                }
                @if let Some(address) = policy.postal_address { p { "Postal address: " (address) } }
            } @else {
                p { "Newsletter signup is not configured yet." }
            }
            h2 { "Information used for delivery" }
            p { "Maincopy stores your email address, confirmation status, and delivery records to send confirmation emails and the article updates you request. Confirm your address before receiving article updates." }
            p { "Amazon Web Services processes your address and message content through Simple Email Service (SES). Delivery, bounce, and complaint events are processed through Amazon SNS and SQS." }
            p { "Maincopy does not add tracking pixels or click-tracking redirects to these emails." }
            h2 { "Unsubscribe and retention" }
            p { "Use the unsubscribe link in an email to stop future delivery. No account or login is required. A message already being submitted may still arrive." }
            p { "Unsubscribing removes your address from the live subscription records. Unconfirmed signups expire after 24 hours and are removed by cleanup. Delivery correlation records are retained for up to 14 days; bounce and complaint suppression digests for 30 days. Service downtime can delay cleanup." }
            p { "Historical backups and provider records follow their own retention schedules; unsubscribing does not immediately erase those copies. Aggregate delivery totals can remain without your email address." }
            p { a href="/" { "Return to the site" } }
        },
    )
}
