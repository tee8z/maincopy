//! Personalized bodies are short-lived protected buffers. Rendering never
//! authorizes a recipient, consumes consent, or submits a provider request.

use markdown_compiler::PublicationBaseUrl;
use maud::Render as _;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    campaign::CampaignContent,
    config::SubscriptionPolicy,
    control::{EncodedControlToken, MAX_TOKEN_BYTES},
    identity::EmailAddress,
    ses::{EmailMessage, MAX_BODY_BYTES, OneClickUrl},
};

const REMOVE_LABEL: &str = "Unsubscribe and remove my address";
pub(super) const UNSUBSCRIBE_ROUTE: &str = "/email/unsubscribe/{token}";
const UNSUBSCRIBE_PATH: &str = "email/unsubscribe/";
const CONFIRM_PATH: &str = "email/confirm/";
const HTML_FOOTER_START: &str = "<hr><p><a href=\"";
const HTML_FOOTER_END: &str = "\">Unsubscribe and remove my address</a></p>";

/// Validate the entire current control-token envelope at startup, before any
/// enrollment can depend on an origin that a mail client cannot use.
pub(super) fn validate_origin(origin: &PublicationBaseUrl) -> Result<(), MessagePreparationError> {
    let candidate = format!(
        "{}{UNSUBSCRIBE_PATH}{}",
        origin.as_str(),
        "x".repeat(MAX_TOKEN_BYTES)
    );
    OneClickUrl::parse(&candidate)
        .map(|_| ())
        .map_err(|_| MessagePreparationError::ControlUrl)
}

/// The caller must issue a Manage token for the current consent generation.
/// Its stable site binding must survive provider changes and ordinary releases.
pub(super) fn unsubscribe_link(
    origin: &PublicationBaseUrl,
    token: &EncodedControlToken,
) -> Result<OneClickUrl, MessagePreparationError> {
    control_link(origin, UNSUBSCRIBE_PATH, token)
}

fn control_link(
    origin: &PublicationBaseUrl,
    path: &str,
    token: &EncodedControlToken,
) -> Result<OneClickUrl, MessagePreparationError> {
    let mut value = Zeroizing::new(String::with_capacity(
        origin.as_str().len() + path.len() + token.as_str().len(),
    ));
    value.push_str(origin.as_str());
    value.push_str(path);
    value.push_str(token.as_str());
    OneClickUrl::parse(&value).map_err(|_| MessagePreparationError::ControlUrl)
}

pub(super) struct Newsletter<'content> {
    content: &'content CampaignContent,
    text: Zeroizing<String>,
    html: Zeroizing<String>,
    removal: OneClickUrl,
}

impl<'content> Newsletter<'content> {
    pub(super) fn render(
        content: &'content CampaignContent,
        removal: OneClickUrl,
        policy: &SubscriptionPolicy,
    ) -> Result<Self, MessagePreparationError> {
        content
            .validate()
            .map_err(|_| MessagePreparationError::Content)?;
        let canonical = url::Url::parse(&content.canonical_url)
            .map_err(|_| MessagePreparationError::Content)?;
        let origin_prefix = format!("{}/", canonical.origin().ascii_serialization());
        if !removal.as_str().starts_with(&origin_prefix) {
            return Err(MessagePreparationError::ControlOrigin);
        }
        // Reserve the maximum HTML escaping expansion before any control bytes
        // enter this allocation. The URL is already bounded by OneClickUrl.
        let footer = SenderFooter::render(policy);
        let html_capacity = content.html.len()
            + footer.html.len()
            + HTML_FOOTER_START.len()
            + removal.as_str().len() * 6
            + HTML_FOOTER_END.len();
        let text_capacity = content.text.len()
            + footer.text.len()
            + 2
            + REMOVE_LABEL.len()
            + 2
            + removal.as_str().len()
            + 1;
        if html_capacity > MAX_BODY_BYTES || text_capacity > MAX_BODY_BYTES {
            return Err(MessagePreparationError::BodyTooLong);
        }
        let mut html = Zeroizing::new(String::with_capacity(html_capacity));
        html.push_str(&content.html);
        html.push_str(&footer.html);
        html.push_str(HTML_FOOTER_START);
        removal.as_str().render_to(&mut html);
        html.push_str(HTML_FOOTER_END);
        let mut text = Zeroizing::new(String::with_capacity(text_capacity));
        text.push_str(&content.text);
        text.push_str(&footer.text);
        text.push_str("\n\n");
        text.push_str(REMOVE_LABEL);
        text.push_str(": ");
        text.push_str(removal.as_str());
        text.push('\n');
        Ok(Self {
            content,
            text,
            html,
            removal,
        })
    }

    pub(super) fn as_message<'mail>(
        &'mail self,
        recipient: &'mail EmailAddress,
        mail_epoch: Uuid,
        campaign_id: Uuid,
        attempt_id: Uuid,
    ) -> EmailMessage<'mail> {
        EmailMessage {
            recipient,
            mail_epoch,
            subject: &self.content.subject,
            text: &self.text,
            html: &self.html,
            campaign_id: Some(campaign_id),
            attempt_id,
            one_click: Some(&self.removal),
        }
    }
}

struct SenderFooter {
    text: String,
    html: String,
}

impl SenderFooter {
    fn render(policy: &SubscriptionPolicy) -> Self {
        let policy = policy.view();
        Self {
            text: format!(
                "\n\n{}\n{}\nContact: {}\nPrivacy: {}\n",
                policy.operator_name,
                policy.postal_address,
                policy.contact_address.as_str(),
                policy.privacy_url
            ),
            html: maud::html! {
                p { (policy.operator_name) }
                p { (policy.postal_address) }
                p { "Contact: " (policy.contact_address.as_str()) }
                p { a href=(policy.privacy_url.as_str()) { "Privacy notice" } }
            }
            .into_string(),
        }
    }
}

/// Confirmation requests are not newsletter consent. Both links remain in
/// protected buffers and only their explicit POST actions change enrollment.
pub(super) struct Confirmation {
    text: Zeroizing<String>,
    html: Zeroizing<String>,
}

impl Confirmation {
    pub(super) fn render(
        origin: &PublicationBaseUrl,
        token: &EncodedControlToken,
        removal: OneClickUrl,
        policy: &SubscriptionPolicy,
    ) -> Result<Self, MessagePreparationError> {
        let confirmation = control_link(origin, CONFIRM_PATH, token)?;
        if !removal.as_str().starts_with(origin.as_str()) {
            return Err(MessagePreparationError::ControlOrigin);
        }
        let footer = SenderFooter::render(policy);
        let purpose = policy.view().purpose;
        let introduction = format!(
            "Confirm your email subscription\n\n{purpose}\n\nIf you requested this subscription, open the link and confirm. The confirmation expires within 24 hours.\n\nConfirm: "
        );
        let public_html = maud::html! {
            h1 { "Confirm your email subscription" }
            p { (purpose) }
            p { "If you requested this subscription, open the link and confirm. The confirmation expires within 24 hours." }
        }.into_string();
        let text_capacity = introduction.len()
            + confirmation.as_str().len()
            + footer.text.len()
            + removal.as_str().len()
            + 200;
        let html_capacity = public_html.len()
            + footer.html.len()
            + (confirmation.as_str().len() + removal.as_str().len()) * 6
            + 300;
        if text_capacity > MAX_BODY_BYTES || html_capacity > MAX_BODY_BYTES {
            return Err(MessagePreparationError::BodyTooLong);
        }
        let mut text = Zeroizing::new(String::with_capacity(text_capacity));
        text.push_str(&introduction);
        text.push_str(confirmation.as_str());
        text.push_str("\n\nIf you did not request this, you can remove the pending address without subscribing.\n");
        text.push_str(REMOVE_LABEL);
        text.push_str(": ");
        text.push_str(removal.as_str());
        text.push_str(&footer.text);
        let mut html = Zeroizing::new(String::with_capacity(html_capacity));
        html.push_str(&public_html);
        html.push_str("<p><a href=\"");
        confirmation.as_str().render_to(&mut html);
        html.push_str("\">Review and confirm subscription</a></p><p>If you did not request this, you can remove the pending address without subscribing.</p>");
        html.push_str(&footer.html);
        html.push_str(HTML_FOOTER_START);
        removal.as_str().render_to(&mut html);
        html.push_str(HTML_FOOTER_END);
        Ok(Self { text, html })
    }

    pub(super) fn as_message<'mail>(
        &'mail self,
        recipient: &'mail EmailAddress,
        mail_epoch: Uuid,
        attempt_id: Uuid,
    ) -> EmailMessage<'mail> {
        EmailMessage {
            recipient,
            mail_epoch,
            subject: "Confirm your email subscription",
            text: &self.text,
            html: &self.html,
            campaign_id: None,
            attempt_id,
            one_click: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum MessagePreparationError {
    #[error("the saved public announcement failed validation")]
    Content,
    #[error("the subscriber control link exceeds the supported URL contract")]
    ControlUrl,
    #[error("the subscriber control link has a different origin from the reviewed article")]
    ControlOrigin,
    #[error("the personalized announcement exceeds the supported body limit")]
    BodyTooLong,
}

#[cfg(test)]
mod tests {
    use markdown_compiler::{PostId, PostRevisionDigest, SiteSnapshotDigest};
    use time::{Duration, OffsetDateTime};

    use super::*;
    use crate::domain::mail::{
        announcement::{MAX_ANNOUNCEMENT_BODY_BYTES, announcement_content_digest},
        config::{MailConfiguration, MailConfigurationCandidate},
        control::{ControlClaims, ControlPurpose, ControlSigningKey},
    };

    const REVIEWED_TEXT: &str =
        "A reviewed article\n\nSummary & details.\n\nRead: https://example.com/posts/article\n";
    const REVIEWED_HTML: &str = "<h1>A reviewed article</h1><p>Summary &amp; details.</p><a href=\"https://example.com/posts/article\">Read</a>";
    const EPOCH: Uuid = Uuid::from_u128(0xaaaaaaaa_aaaa_4aaa_8aaa_aaaaaaaaaaaa);
    const CAMPAIGN: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_111111111111);
    const ATTEMPT: Uuid = Uuid::from_u128(0x22222222_2222_4222_8222_222222222222);

    fn policy() -> SubscriptionPolicy {
        let candidate: MailConfigurationCandidate = toml::from_str(
            r#"
mode = "ses"
sender = "newsletter@example.com"
region = "us-east-1"
configuration_set = "newsletter"
credential_file = "/unused/credentials"
control_signing_key_file = "/unused/control"
[subscriptions]
mode = "paused"
operator_name = "Example & Company"
postal_address = "PO Box 123, Example City"
purpose = "New articles from Example."
privacy_url = "https://example.com/privacy"
contact_address = "contact@example.com"
"#,
        )
        .unwrap();
        let MailConfiguration::Ses(configuration) =
            candidate.validate(std::path::Path::new("/unused")).unwrap()
        else {
            panic!("SES fixture")
        };
        configuration.view().subscriptions.unwrap().clone()
    }

    // Saved common content is this boundary's input. Publication eligibility is
    // tested by Announcement; these fixtures carry the real saved-content digest.
    fn reviewed_content(text: &str, html: &str) -> CampaignContent {
        let post_id = PostId::parse("33333333-3333-4333-8333-333333333333").unwrap();
        let revision = PostRevisionDigest::from_bytes([3; 32]);
        let canonical_url = "https://example.com/posts/article";
        let subject = "A reviewed article";
        CampaignContent {
            content_digest: announcement_content_digest(
                &post_id,
                &revision,
                canonical_url,
                subject,
                text,
                html,
            ),
            post_id,
            revision,
            snapshot: SiteSnapshotDigest::from_bytes([4; 32]),
            site_version: 1,
            template_version: 1,
            canonical_url: canonical_url.into(),
            subject: subject.into(),
            text: text.into(),
            html: html.into(),
        }
    }

    #[test]
    fn delivery_preserves_reviewed_bodies_and_uses_one_escaped_removal_url_everywhere() {
        let content = reviewed_content(REVIEWED_TEXT, REVIEWED_HTML);
        let original = content.clone();
        let address = EmailAddress::parse("reader@example.net").unwrap();
        let removal = "https://example.com/email/unsubscribe/example?first=1&second=2";
        let newsletter =
            Newsletter::render(&content, OneClickUrl::parse(removal).unwrap(), &policy()).unwrap();
        let message = newsletter.as_message(&address, EPOCH, CAMPAIGN, ATTEMPT);

        assert_eq!(message.subject, original.subject);
        assert!(message.text.starts_with(REVIEWED_TEXT));
        assert!(message.html.starts_with(REVIEWED_HTML));
        assert!(message.text.contains("Example & Company"));
        assert!(message.html.contains("Example &amp; Company"));
        assert!(message.text.contains("contact@example.com"));
        assert!(message.html.contains("https://example.com/privacy"));
        assert!(message.text.ends_with("Unsubscribe and remove my address: https://example.com/email/unsubscribe/example?first=1&second=2\n"));
        assert!(message.html.ends_with("<hr><p><a href=\"https://example.com/email/unsubscribe/example?first=1&amp;second=2\">Unsubscribe and remove my address</a></p>"));
        assert_eq!(message.one_click.unwrap().as_str(), removal);
        assert_eq!(message.campaign_id, Some(CAMPAIGN));
        assert_eq!(message.attempt_id, ATTEMPT);
        assert!(!message.text.contains(address.as_str()));
        assert!(!message.html.contains(address.as_str()));
        drop(newsletter);
        assert_eq!(content, original);
    }

    #[test]
    fn foreign_hosts_and_ports_cannot_receive_a_reviewed_articles_removal_control() {
        let content = reviewed_content(REVIEWED_TEXT, REVIEWED_HTML);
        for foreign in [
            "https://other.example/email/unsubscribe/example",
            "https://example.com.attacker.invalid/email/unsubscribe/example",
            "https://example.com:444/email/unsubscribe/example",
        ] {
            assert_eq!(
                Newsletter::render(&content, OneClickUrl::parse(foreign).unwrap(), &policy()).err(),
                Some(MessagePreparationError::ControlOrigin)
            );
        }
    }

    #[test]
    fn changing_saved_common_content_without_review_rejects_personalization() {
        let alterations: [fn(&mut CampaignContent); 4] = [
            |content| content.subject.push_str(" altered"),
            |content| content.text.push_str("\nUnreviewed text"),
            |content| content.html.push_str("<p>Unreviewed HTML</p>"),
            |content| content.canonical_url = "https://example.com/posts/different".into(),
        ];
        for alter in alterations {
            let mut content = reviewed_content(REVIEWED_TEXT, REVIEWED_HTML);
            alter(&mut content);
            assert_eq!(
                Newsletter::render(
                    &content,
                    OneClickUrl::parse("https://example.com/email/unsubscribe/example").unwrap(),
                    &policy(),
                )
                .err(),
                Some(MessagePreparationError::Content)
            );
        }
    }

    #[test]
    fn maximum_reviewable_bodies_leave_room_for_the_complete_bounded_control_url() {
        let text = "t".repeat(32 * 1024);
        let html = "h".repeat(MAX_ANNOUNCEMENT_BODY_BYTES);
        let content = reviewed_content(&text, &html);
        let prefix = "https://example.com/email/unsubscribe/";
        // SES permits 978 URL bytes after the header name and angle brackets.
        let removal = format!("{prefix}{}", "x".repeat(978 - prefix.len()));
        let newsletter =
            Newsletter::render(&content, OneClickUrl::parse(&removal).unwrap(), &policy()).unwrap();
        let address = EmailAddress::parse("reader@example.net").unwrap();
        let message = newsletter.as_message(&address, EPOCH, CAMPAIGN, ATTEMPT);
        assert!(message.text.starts_with(&text));
        assert!(message.html.starts_with(&html));
        assert!(message.text.contains(&removal));
        assert!(message.html.contains(&removal));
        assert!(message.text.len() <= MAX_BODY_BYTES);
        assert!(message.html.len() <= MAX_BODY_BYTES);
        assert_eq!(message.one_click.unwrap().as_str(), removal);
    }

    #[test]
    fn generated_manage_link_preserves_the_authenticated_generation_for_old_mail() {
        let key = ControlSigningKey::from_bytes(Zeroizing::new([17; 32]));
        let issued = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let enrollment = Uuid::from_u128(0x44444444_4444_4444_8444_444444444444);
        let generation = Uuid::from_u128(0x55555555_5555_4555_8555_555555555555);
        let site_binding = [9; 32];
        let token = key
            .issue(
                &site_binding,
                ControlClaims::Manage {
                    enrollment,
                    generation,
                },
                issued,
            )
            .unwrap();
        let origin = PublicationBaseUrl::parse("https://EXAMPLE.COM/").unwrap();
        let removal = unsubscribe_link(&origin, &token).unwrap();
        let encoded = removal
            .as_str()
            .strip_prefix("https://example.com/email/unsubscribe/")
            .unwrap();
        assert_eq!(encoded, token.as_str());
        let verified = key
            .verify(
                &site_binding,
                ControlPurpose::Manage,
                encoded,
                issued + Duration::days(365),
            )
            .unwrap();
        assert!(
            verified
                == ControlClaims::Manage {
                    enrollment,
                    generation
                }
        );

        let content = reviewed_content(REVIEWED_TEXT, REVIEWED_HTML);
        let newsletter = Newsletter::render(&content, removal, &policy()).unwrap();
        let address = EmailAddress::parse("reader@example.net").unwrap();
        let first = newsletter.as_message(&address, EPOCH, CAMPAIGN, ATTEMPT);
        let later = newsletter.as_message(&address, EPOCH, Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(
            first.one_click.unwrap().as_str(),
            later.one_click.unwrap().as_str()
        );
    }

    #[test]
    fn confirmation_explains_consent_and_includes_removal_without_newsletter_headers() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let key = ControlSigningKey::from_bytes(Zeroizing::new([17; 32]));
        let enrollment = Uuid::new_v4();
        let generation = Uuid::new_v4();
        let confirm = key
            .issue(
                &[9; 32],
                ControlClaims::Confirm {
                    enrollment,
                    generation,
                    confirmation_nonce: Uuid::new_v4(),
                    expires_at: now + Duration::hours(24),
                },
                now,
            )
            .unwrap();
        let manage = key
            .issue(
                &[9; 32],
                ControlClaims::Manage {
                    enrollment,
                    generation,
                },
                now,
            )
            .unwrap();
        let origin = PublicationBaseUrl::parse("https://example.com/").unwrap();
        let confirmation = Confirmation::render(
            &origin,
            &confirm,
            unsubscribe_link(&origin, &manage).unwrap(),
            &policy(),
        )
        .unwrap();
        let address = EmailAddress::parse("reader@example.net").unwrap();
        let message = confirmation.as_message(&address, EPOCH, ATTEMPT);
        assert_eq!(message.campaign_id, None);
        assert!(message.one_click.is_none());
        for body in [message.text, message.html] {
            assert!(body.contains("New articles from Example."));
            assert!(body.contains(&format!(
                "https://example.com/email/confirm/{}",
                confirm.as_str()
            )));
            assert!(body.contains(&format!(
                "https://example.com/email/unsubscribe/{}",
                manage.as_str()
            )));
            assert!(body.contains("contact@example.com"));
            assert!(body.contains("24 hours"));
            assert!(!body.contains(address.as_str()));
            assert!(body.len() <= MAX_BODY_BYTES);
        }
    }
}
