use std::fmt;

use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, SeqAccess, Visitor},
};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{AuthenticatedFeedback, FeedbackConfiguration, FeedbackKind, FeedbackRejection};
use crate::domain::mail::{
    identity::EmailAddress,
    ses::{MessageId, ProtectedText},
};

/// Exactly one original recipient and tag value: reject ambiguity without
/// retaining an arbitrarily long array of recipient-linkable strings.
struct One<Value>(Value);

impl<'de, Value: Deserialize<'de>> Deserialize<'de> for One<Value> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OneVisitor<Value>(std::marker::PhantomData<Value>);
        impl<'de, Value: Deserialize<'de>> Visitor<'de> for OneVisitor<Value> {
            type Value = One<Value>;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("exactly one value")
            }
            fn visit_seq<Sequence: SeqAccess<'de>>(
                self,
                mut sequence: Sequence,
            ) -> Result<Self::Value, Sequence::Error> {
                let value = sequence
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::custom("missing value"))?;
                if sequence.next_element::<IgnoredAny>()?.is_some() {
                    return Err(serde::de::Error::custom("multiple values"));
                }
                Ok(One(value))
            }
        }
        deserializer.deserialize_seq(OneVisitor(std::marker::PhantomData))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Envelope {
    #[serde(rename = "Type")]
    kind: ProtectedText,
    topic_arn: ProtectedText,
    message_id: ProtectedText,
    timestamp: ProtectedText,
    message: ProtectedText,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    event_type: ProtectedText,
    mail: Mail,
    bounce: Option<Bounce>,
    complaint: Option<Complaint>,
    delivery: Option<Delivery>,
    send: Option<EmptyEvent>,
    reject: Option<Rejected>,
    failure: Option<RenderingFailure>,
}

#[derive(Deserialize)]
struct EmptyEvent {}

#[derive(Deserialize)]
struct Rejected {
    reason: ProtectedText,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenderingFailure {
    error_message: ProtectedText,
    template_name: ProtectedText,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Mail {
    sending_account_id: ProtectedText,
    source_arn: ProtectedText,
    timestamp: ProtectedText,
    message_id: ProtectedText,
    destination: One<ProtectedText>,
    tags: Tags,
}

#[derive(Deserialize)]
struct Tags {
    #[serde(rename = "ses:configuration-set")]
    configuration_set: One<ProtectedText>,
    #[serde(rename = "maincopy-campaign")]
    campaign: Option<One<ProtectedText>>,
    #[serde(rename = "maincopy-attempt")]
    attempt: One<ProtectedText>,
    #[serde(rename = "maincopy-epoch")]
    epoch: One<ProtectedText>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventRecipient {
    email_address: ProtectedText,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bounce {
    bounce_type: ProtectedText,
    timestamp: ProtectedText,
    bounced_recipients: Vec<EventRecipient>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Complaint {
    timestamp: ProtectedText,
    complained_recipients: Vec<EventRecipient>,
    complaint_feedback_type: Option<ProtectedText>,
}

#[derive(Deserialize)]
struct Delivery {
    timestamp: ProtectedText,
    recipients: One<ProtectedText>,
}

pub(super) fn parse(
    bytes: &[u8],
    configuration: &FeedbackConfiguration,
    now: OffsetDateTime,
) -> Result<AuthenticatedFeedback, FeedbackRejection> {
    let envelope: Envelope =
        serde_json::from_slice(bytes).map_err(|_| FeedbackRejection::Envelope)?;
    if envelope.kind.as_str() != "Notification"
        || envelope.topic_arn.as_str() != configuration.topic_arn
    {
        return Err(FeedbackRejection::Topic);
    }
    identity(envelope.message_id.as_str()).map_err(|_| FeedbackRejection::Envelope)?;
    let published_at = timestamp(envelope.timestamp.as_str())?;
    let event: Event =
        serde_json::from_str(envelope.message.as_str()).map_err(|_| FeedbackRejection::Event)?;
    event.into_feedback(configuration, published_at, now)
}

impl Event {
    fn into_feedback(
        self,
        configuration: &FeedbackConfiguration,
        published_at: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<AuthenticatedFeedback, FeedbackRejection> {
        self.mail.validate_provider(configuration)?;
        let recipient = EmailAddress::parse(self.mail.destination.0.as_str())
            .map_err(|_| FeedbackRejection::Correlation)?;
        let campaign_id = self
            .mail
            .tags
            .campaign
            .as_ref()
            .map(|tag| identity(tag.0.as_str()))
            .transpose()?;
        let attempt_id = identity(self.mail.tags.attempt.0.as_str())?;
        let mail_epoch = identity(self.mail.tags.epoch.0.as_str())
            .map_err(|_| FeedbackRejection::Correlation)?;
        let provider_message_id = MessageId::parse(self.mail.message_id.as_str())
            .map_err(|_| FeedbackRejection::Correlation)?;
        let sent_at = timestamp(self.mail.timestamp.as_str())?;
        let (kind, occurred_at) = self.outcome(&recipient, sent_at, published_at)?;
        validate_timestamps(sent_at, occurred_at, published_at, now)?;
        Ok(AuthenticatedFeedback {
            recipient,
            campaign_id,
            attempt_id,
            mail_epoch,
            provider_message_id,
            sent_at,
            occurred_at,
            kind,
        })
    }

    fn outcome(
        &self,
        recipient: &EmailAddress,
        sent_at: OffsetDateTime,
        published_at: OffsetDateTime,
    ) -> Result<(FeedbackKind, OffsetDateTime), FeedbackRejection> {
        match self.event_type.as_str() {
            "Send" if self.send.is_some() => Ok((FeedbackKind::Accepted, sent_at)),
            "Delivery" => {
                let delivery = self.delivery.as_ref().ok_or(FeedbackRejection::Event)?;
                let delivered = EmailAddress::parse(delivery.recipients.0.as_str())
                    .map_err(|_| FeedbackRejection::Correlation)?;
                if delivered.as_str() != recipient.as_str() {
                    return Err(FeedbackRejection::Correlation);
                }
                Ok((
                    FeedbackKind::Delivered,
                    timestamp(delivery.timestamp.as_str())?,
                ))
            }
            "Bounce" => self
                .bounce
                .as_ref()
                .ok_or(FeedbackRejection::Event)?
                .outcome(),
            "Complaint" => self
                .complaint
                .as_ref()
                .ok_or(FeedbackRejection::Event)?
                .outcome(),
            // These outcomes prove provider acceptance followed by terminal
            // failure, never permission to submit the attempt again.
            "Reject"
                if self
                    .reject
                    .as_ref()
                    .is_some_and(|rejection| rejection.reason.as_str() == "Bad content") =>
            {
                Ok((FeedbackKind::DeliveryFailed, published_at))
            }
            "Rendering Failure" | "RenderingFailure"
                if self.failure.as_ref().is_some_and(|failure| {
                    !failure.error_message.as_str().is_empty()
                        && !failure.template_name.as_str().is_empty()
                }) =>
            {
                Ok((FeedbackKind::DeliveryFailed, published_at))
            }
            _ => Err(FeedbackRejection::Event),
        }
    }
}

impl Mail {
    fn validate_provider(
        &self,
        configuration: &FeedbackConfiguration,
    ) -> Result<(), FeedbackRejection> {
        let source_prefix = format!(
            "arn:aws:ses:{}:{}:identity/",
            configuration.region, configuration.account_id
        );
        if self.sending_account_id.as_str() != configuration.account_id
            || self.tags.configuration_set.0.as_str() != configuration.configuration_set
            || !self
                .source_arn
                .as_str()
                .strip_prefix(&source_prefix)
                .is_some_and(|name| !name.is_empty() && name.len() <= 320)
        {
            return Err(FeedbackRejection::Provider);
        }
        Ok(())
    }
}

impl Bounce {
    fn outcome(&self) -> Result<(FeedbackKind, OffsetDateTime), FeedbackRejection> {
        validate_event_recipients(&self.bounced_recipients)?;
        let kind = match self.bounce_type.as_str() {
            "Permanent" => FeedbackKind::HardBounce,
            "Transient" | "Undetermined" => FeedbackKind::DeliveryFailed,
            _ => return Err(FeedbackRejection::Event),
        };
        Ok((kind, timestamp(self.timestamp.as_str())?))
    }
}

impl Complaint {
    fn outcome(&self) -> Result<(FeedbackKind, OffsetDateTime), FeedbackRejection> {
        validate_event_recipients(&self.complained_recipients)?;
        let kind = match self
            .complaint_feedback_type
            .as_ref()
            .map(ProtectedText::as_str)
        {
            // An authenticated correction still proves acceptance. It cannot
            // clear a prior complaint or restore eligibility.
            Some("not-spam") => FeedbackKind::Accepted,
            None | Some("abuse" | "auth-failure" | "fraud" | "other" | "virus") => {
                FeedbackKind::Complaint
            }
            Some(_) => return Err(FeedbackRejection::Event),
        };
        Ok((kind, timestamp(self.timestamp.as_str())?))
    }
}

fn validate_event_recipients(recipients: &[EventRecipient]) -> Result<(), FeedbackRejection> {
    if recipients.is_empty() || recipients.len() > 16 {
        return Err(FeedbackRejection::Correlation);
    }
    // DSN Final-Recipient can be a forwarded mailbox. It is bounded and parsed,
    // but it must never replace mail.destination when selecting local consent.
    for recipient in recipients {
        EmailAddress::parse(recipient.email_address.as_str())
            .map_err(|_| FeedbackRejection::Correlation)?;
    }
    Ok(())
}

fn identity(value: &str) -> Result<Uuid, FeedbackRejection> {
    let id = Uuid::parse_str(value).map_err(|_| FeedbackRejection::Correlation)?;
    let mut canonical = Zeroizing::new([0_u8; 36]);
    if id.is_nil() || value != id.hyphenated().encode_lower(&mut *canonical) {
        return Err(FeedbackRejection::Correlation);
    }
    Ok(id)
}

fn timestamp(value: &str) -> Result<OffsetDateTime, FeedbackRejection> {
    if value.len() > 40 {
        return Err(FeedbackRejection::Timestamp);
    }
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| FeedbackRejection::Timestamp)
}

fn validate_timestamps(
    sent_at: OffsetDateTime,
    occurred_at: OffsetDateTime,
    published_at: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<(), FeedbackRejection> {
    let skew = Duration::minutes(5);
    for value in [sent_at, occurred_at, published_at] {
        if value < OffsetDateTime::UNIX_EPOCH || value > now.saturating_add(skew) {
            return Err(FeedbackRejection::Timestamp);
        }
    }
    if occurred_at.saturating_add(skew) < sent_at || published_at.saturating_add(skew) < occurred_at
    {
        return Err(FeedbackRejection::Timestamp);
    }
    // Do not discard delayed complaints with an arbitrary age cutoff. The
    // writer rejects unknown/expired attempts and never recreates erased state.
    Ok(())
}
