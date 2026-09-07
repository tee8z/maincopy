use super::{
    InputError, ServiceRejection, SesClient, SesError,
    transport::{ProtectedBytes, ProtectedText},
};
use crate::domain::mail::identity::EmailAddress;
use serde::{Deserialize, Serialize};
use std::fmt;
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

pub(in crate::domain::mail) const MAX_BODY_BYTES: usize = 96 * 1024;
// SES limits header name plus value to 996 ASCII characters. The name has
// sixteen characters and the angle brackets consume two more.
const MAX_UNSUBSCRIBE_URL_BYTES: usize = 978;

/// Borrowed content can include recipient control tokens. This type deliberately
/// has neither Debug nor Serialize; only the private SES wire view serializes it.
pub(in crate::domain::mail) struct EmailMessage<'a> {
    pub(in crate::domain::mail) recipient: &'a EmailAddress,
    pub(in crate::domain::mail) subject: &'a str,
    pub(in crate::domain::mail) text: &'a str,
    pub(in crate::domain::mail) html: &'a str,
    pub(in crate::domain::mail) campaign_id: Option<Uuid>,
    pub(in crate::domain::mail) attempt_id: Uuid,
    pub(in crate::domain::mail) mail_epoch: Uuid,
    pub(in crate::domain::mail) one_click: Option<&'a OneClickUrl>,
}

pub(in crate::domain::mail) struct OneClickUrl(ProtectedText);

impl OneClickUrl {
    pub(in crate::domain::mail) fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub(in crate::domain::mail) fn parse(value: &str) -> Result<Self, InputError> {
        if value.len() > MAX_UNSUBSCRIBE_URL_BYTES
            || !value
                .bytes()
                .all(|byte| (33..=126).contains(&byte) && !b"<>".contains(&byte))
        {
            return Err(InputError::OneClickUrl);
        }
        let parsed = Url::parse(value).map_err(|_| InputError::OneClickUrl)?;
        let valid = parsed.scheme() == "https"
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.fragment().is_none();
        // Url owns the full bearer URL. Move its serialization into protected
        // storage before either returning or rejecting it; do not leave a
        // second ordinary String allocation behind after validation.
        let canonical = Zeroizing::new(String::from(parsed));
        if !valid || canonical.as_str() != value {
            return Err(InputError::OneClickUrl);
        }
        Ok(Self(ProtectedText::from_owned(canonical)))
    }
}

/// A provider receipt links an individual delivery. It stays protected even
/// after validation and is exposed only for explicit authority correlation.
#[derive(Eq, PartialEq)]
pub(crate) struct MessageId(Zeroizing<String>);

impl fmt::Debug for MessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MessageId([redacted])")
    }
}

impl MessageId {
    pub(in crate::domain::mail) fn parse(value: &str) -> Result<Self, SesError> {
        if value.is_empty()
            || value.len() > 256
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        {
            return Err(SesError::InvalidResponse);
        }
        Ok(Self(Zeroizing::new(value.to_owned())))
    }

    pub(in crate::domain::mail) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::domain::mail) enum SendOutcome {
    Accepted(MessageId),
    Rejected(ServiceRejection),
    Retryable(ServiceRejection),
    Unknown,
}

#[derive(Serialize)]
struct SendRequest<'a> {
    #[serde(rename = "FromEmailAddress")]
    from: &'a str,
    #[serde(rename = "ConfigurationSetName")]
    configuration_set: &'a str,
    #[serde(rename = "Destination")]
    destination: Destination<'a>,
    #[serde(rename = "Content")]
    content: Content<'a>,
    #[serde(rename = "EmailTags")]
    tags: Vec<Tag<'a>>,
}

#[derive(Serialize)]
struct Destination<'a> {
    #[serde(rename = "ToAddresses")]
    to: [&'a str; 1],
}
#[derive(Serialize)]
struct Content<'a> {
    #[serde(rename = "Simple")]
    simple: Simple<'a>,
}
#[derive(Serialize)]
struct Simple<'a> {
    #[serde(rename = "Subject")]
    subject: Text<'a>,
    #[serde(rename = "Body")]
    body: Body<'a>,
    #[serde(rename = "Headers", skip_serializing_if = "Vec::is_empty")]
    headers: Vec<Header<'a>>,
}
#[derive(Serialize)]
struct Body<'a> {
    #[serde(rename = "Text")]
    text: Text<'a>,
    #[serde(rename = "Html")]
    html: Text<'a>,
}
#[derive(Serialize)]
struct Text<'a> {
    #[serde(rename = "Data")]
    data: &'a str,
    #[serde(rename = "Charset")]
    charset: &'static str,
}
#[derive(Serialize)]
struct Header<'a> {
    #[serde(rename = "Name")]
    name: &'static str,
    #[serde(rename = "Value")]
    value: &'a str,
}
#[derive(Serialize)]
struct Tag<'a> {
    #[serde(rename = "Name")]
    name: &'static str,
    #[serde(rename = "Value")]
    value: &'a str,
}

impl SesClient {
    /// The caller must durably admit this attempt and validate current consent.
    /// This adapter executes one request. Dropping/cancelling the future after
    /// transmission must be treated by that caller as an unknown outcome.
    pub(in crate::domain::mail) async fn send(
        &self,
        message: &EmailMessage<'_>,
    ) -> Result<SendOutcome, SesError> {
        let body = self.prepare_message(message)?;
        let reply = self.request(body).await;
        let body = match reply.and_then(|reply| reply.require_ok()) {
            Ok(body) => body,
            Err(SesError::Unknown) => return Ok(SendOutcome::Unknown),
            Err(SesError::Rejected(reason)) => return Ok(classify_rejection(reason)),
            Err(error) => return Err(error),
        };
        #[derive(Deserialize)]
        struct SendResponse {
            #[serde(rename = "MessageId")]
            message_id: ProtectedText,
        }
        // Success headers alone do not establish a usable receipt. The service
        // may already have accepted the request even when the body is malformed.
        Ok(serde_json::from_slice::<SendResponse>(body.as_ref())
            .ok()
            .and_then(|reply| MessageId::parse(reply.message_id.as_str()).ok())
            .map_or(SendOutcome::Unknown, SendOutcome::Accepted))
    }

    fn prepare_message(&self, message: &EmailMessage<'_>) -> Result<ProtectedBytes, SesError> {
        validate_message(message)?;
        let campaign_id = message.campaign_id.map(|id| id.to_string());
        let mut attempt_buffer = Zeroizing::new([0_u8; 36]);
        let attempt_id = message
            .attempt_id
            .hyphenated()
            .encode_lower(&mut *attempt_buffer);
        let mut epoch_buffer = Zeroizing::new([0_u8; 36]);
        let mail_epoch = message
            .mail_epoch
            .hyphenated()
            .encode_lower(&mut *epoch_buffer);
        let mut tags = Vec::with_capacity(3);
        if let Some(campaign_id) = campaign_id.as_ref() {
            tags.push(Tag {
                name: "maincopy-campaign",
                value: campaign_id,
            });
        }
        tags.push(Tag {
            name: "maincopy-attempt",
            value: attempt_id,
        });
        tags.push(Tag {
            name: "maincopy-epoch",
            value: mail_epoch,
        });
        let mut unsubscribe = Zeroizing::new(String::new());
        let headers = match message.one_click {
            Some(url) => {
                unsubscribe.reserve(url.0.as_str().len() + 2);
                unsubscribe.push('<');
                unsubscribe.push_str(url.0.as_str());
                unsubscribe.push('>');
                vec![
                    Header {
                        name: "List-Unsubscribe",
                        value: &unsubscribe,
                    },
                    Header {
                        name: "List-Unsubscribe-Post",
                        value: "List-Unsubscribe=One-Click",
                    },
                ]
            }
            None => Vec::new(),
        };
        let wire = SendRequest {
            from: self.configuration.sender.as_str(),
            configuration_set: self.configuration.configuration_set.as_str(),
            destination: Destination {
                to: [message.recipient.as_str()],
            },
            content: Content {
                simple: Simple {
                    subject: Text {
                        data: message.subject,
                        charset: "UTF-8",
                    },
                    body: Body {
                        text: Text {
                            data: message.text,
                            charset: "UTF-8",
                        },
                        html: Text {
                            data: message.html,
                            charset: "UTF-8",
                        },
                    },
                    headers,
                },
            },
            tags,
        };
        ProtectedBytes::json(&wire)
    }
}

fn validate_message(message: &EmailMessage<'_>) -> Result<(), InputError> {
    if message.subject.is_empty()
        || message.subject.len() > 998
        || message.subject.chars().any(char::is_control)
        || message.text.is_empty()
        || message.html.is_empty()
        || message.text.len() > MAX_BODY_BYTES
        || message.html.len() > MAX_BODY_BYTES
        || message.campaign_id.is_some_and(|id| id.is_nil())
        || message.attempt_id.is_nil()
        || message.mail_epoch.is_nil()
    {
        return Err(InputError::Message);
    }
    Ok(())
}

fn classify_rejection(reason: ServiceRejection) -> SendOutcome {
    match reason {
        ServiceRejection::Throttled | ServiceRejection::LimitExceeded => {
            SendOutcome::Retryable(reason)
        }
        ServiceRejection::AccessDenied
        | ServiceRejection::InvalidRequest
        | ServiceRejection::NotFound
        | ServiceRejection::AlreadyExists
        | ServiceRejection::SendingDisabled
        | ServiceRejection::MessageRejected => SendOutcome::Rejected(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_click_url_obeys_the_ses_combined_header_bound_and_ascii_contract() {
        let prefix = "https://example.com/remove/";
        assert!(OneClickUrl::parse(&format!("{prefix}{}", "x".repeat(978 - prefix.len()))).is_ok());
        assert!(
            OneClickUrl::parse(&format!("{prefix}{}", "x".repeat(979 - prefix.len()))).is_err()
        );
        for invalid in [
            "https://example.com/é",
            "https://example.com/<token>",
            "https://user:secret@example.com/remove",
            "https://example.com/#token",
            "http://example.com/remove",
            "https://example.com/\r\nX-Injected: yes",
        ] {
            assert!(OneClickUrl::parse(invalid).is_err());
        }
    }

    #[test]
    fn malformed_provider_message_identifiers_cannot_escape_as_receipts() {
        assert!(MessageId::parse("010001-abcdef-123456").is_ok());
        assert!(MessageId::parse("person@example.com").is_err());
        assert!(MessageId::parse("").is_err());
        assert!(MessageId::parse(&"x".repeat(257)).is_err());
    }

    #[test]
    fn accepted_delivery_debug_never_exposes_the_recipient_linkable_receipt() {
        let receipt = MessageId::parse("010001-abcdef-123456").unwrap();
        assert_eq!(receipt.as_str(), "010001-abcdef-123456");
        assert_eq!(format!("{receipt:?}"), "MessageId([redacted])");
        assert_eq!(
            format!("{:?}", SendOutcome::Accepted(receipt)),
            "Accepted(MessageId([redacted]))"
        );
    }

    #[test]
    fn missing_content_or_operation_identity_cannot_reach_the_transport() {
        let recipient = EmailAddress::parse("reader@example.com").unwrap();
        let valid = || EmailMessage {
            recipient: &recipient,
            subject: "Subject",
            text: "Text",
            html: "<p>Text</p>",
            campaign_id: Some(Uuid::from_u128(1)),
            mail_epoch: Uuid::from_u128(3),
            attempt_id: Uuid::from_u128(2),
            one_click: None,
        };
        assert!(validate_message(&valid()).is_ok());
        let mut message = valid();
        message.subject = "";
        assert!(validate_message(&message).is_err());
        let subject = "x".repeat(999);
        let mut message = valid();
        message.subject = &subject;
        assert!(validate_message(&message).is_err());
        let mut message = valid();
        message.text = "";
        assert!(validate_message(&message).is_err());
        let mut message = valid();
        message.html = "";
        assert!(validate_message(&message).is_err());
        let oversized = "x".repeat(MAX_BODY_BYTES + 1);
        let mut message = valid();
        message.text = &oversized;
        assert!(validate_message(&message).is_err());
        let mut message = valid();
        message.html = &oversized;
        assert!(validate_message(&message).is_err());
        let mut message = valid();
        message.campaign_id = Some(Uuid::nil());
        assert!(validate_message(&message).is_err());
        let mut message = valid();
        message.attempt_id = Uuid::nil();
        assert!(validate_message(&message).is_err());
        let mut message = valid();
        message.mail_epoch = Uuid::nil();
        assert!(validate_message(&message).is_err());
    }
}
