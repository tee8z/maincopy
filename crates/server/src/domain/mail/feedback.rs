//! Authenticated feedback is polled from one protected SNS-to-SQS subscription.
//! No HTTP handler accepts provider payloads. Provisioning must restrict SNS
//! publication to the configured SES configuration set; queue preflight checks
//! the narrow resource-policy contract before runtime enables admission.

mod event;
mod queue;
mod worker;

use super::{
    identity::EmailAddress,
    ses::{MessageId, ResourceName, SesCredentials, SesRegion},
};
use reqwest::Client;
use std::sync::Arc;
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

pub(super) use queue::FeedbackReceipt;
pub(crate) use worker::FeedbackWorker;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FeedbackConfiguration {
    queue_url: Url,
    queue_arn: String,
    topic_arn: String,
    region: String,
    account_id: String,
    configuration_set: String,
}

pub(crate) struct FeedbackConfigurationView<'configuration> {
    pub queue_url: &'configuration str,
    pub topic_arn: &'configuration str,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum FeedbackConfigurationError {
    #[error("the feedback region or SES configuration set is invalid")]
    Provider,
    #[error("the feedback queue must be a canonical regional HTTPS standard SQS queue URL")]
    Queue,
    #[error("the feedback topic must be a standard SNS topic in the queue account and region")]
    Topic,
}

impl FeedbackConfiguration {
    pub(crate) fn new(
        queue_url: &str,
        topic_arn: &str,
        region: &str,
        configuration_set: &str,
    ) -> Result<Self, FeedbackConfigurationError> {
        SesRegion::parse(region).map_err(|_| FeedbackConfigurationError::Provider)?;
        ResourceName::parse(configuration_set).map_err(|_| FeedbackConfigurationError::Provider)?;
        let queue = queue_url_parts(queue_url, region)?;
        let account_id = queue.path_segments().unwrap().next().unwrap().to_owned();
        let queue_name = queue.path_segments().unwrap().nth(1).unwrap();
        let queue_arn = format!("arn:aws:sqs:{region}:{account_id}:{queue_name}");
        let topic_prefix = format!("arn:aws:sns:{region}:{account_id}:");
        let topic_name = topic_arn
            .strip_prefix(&topic_prefix)
            .ok_or(FeedbackConfigurationError::Topic)?;
        if !resource_name(topic_name, 256) {
            return Err(FeedbackConfigurationError::Topic);
        }
        Ok(Self {
            queue_url: queue,
            queue_arn,
            topic_arn: topic_arn.into(),
            region: region.into(),
            account_id,
            configuration_set: configuration_set.into(),
        })
    }

    fn source_binding(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"maincopy-feedback-source-v1\0");
        for value in [
            self.queue_url.as_str(),
            &self.topic_arn,
            &self.region,
            &self.account_id,
            &self.configuration_set,
        ] {
            hasher.update(&(value.len() as u64).to_le_bytes());
            hasher.update(value.as_bytes());
        }
        *hasher.finalize().as_bytes()
    }

    pub(crate) fn view(&self) -> FeedbackConfigurationView<'_> {
        FeedbackConfigurationView {
            queue_url: self.queue_url.as_str(),
            topic_arn: &self.topic_arn,
        }
    }
}

fn queue_url_parts(value: &str, region: &str) -> Result<Url, FeedbackConfigurationError> {
    if value.len() > 256 {
        return Err(FeedbackConfigurationError::Queue);
    }
    let url = Url::parse(value).map_err(|_| FeedbackConfigurationError::Queue)?;
    if url.scheme() != "https"
        || url.host_str() != Some(format!("sqs.{region}.amazonaws.com").as_str())
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.as_str() != value
    {
        return Err(FeedbackConfigurationError::Queue);
    }
    let mut segments = url
        .path_segments()
        .ok_or(FeedbackConfigurationError::Queue)?;
    let account = segments.next().ok_or(FeedbackConfigurationError::Queue)?;
    let name = segments.next().ok_or(FeedbackConfigurationError::Queue)?;
    if account.len() != 12
        || !account.bytes().all(|byte| byte.is_ascii_digit())
        || !resource_name(name, 80)
        || segments.next().is_some()
    {
        return Err(FeedbackConfigurationError::Queue);
    }
    Ok(url)
}

fn resource_name(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

pub(super) struct FeedbackClient {
    http: Client,
    endpoint: Url,
    credentials: Arc<SesCredentials>,
    configuration: FeedbackConfiguration,
}

/// Recipient-linkable fields intentionally have no Debug, Clone, or Serialize.
/// The writer must correlate this tuple with the original admitted attempt and
/// current enrollment generation before changing suppression or delivery state.
pub(super) struct AuthenticatedFeedback {
    pub recipient: EmailAddress,
    pub campaign_id: Option<Uuid>,
    pub attempt_id: Uuid,
    pub mail_epoch: Uuid,
    pub provider_message_id: MessageId,
    pub sent_at: OffsetDateTime,
    pub occurred_at: OffsetDateTime,
    pub kind: FeedbackKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FeedbackKind {
    Accepted,
    Delivered,
    HardBounce,
    Complaint,
    DeliveryFailed,
}

pub(super) struct FeedbackDelivery {
    pub receipt: FeedbackReceipt,
    pub event: Result<AuthenticatedFeedback, FeedbackRejection>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum FeedbackRejection {
    #[error("the feedback envelope is malformed or exceeds its limit")]
    Envelope,
    #[error("the feedback envelope is not a notification from the configured topic")]
    Topic,
    #[error("the feedback event is not from the configured SES account and configuration set")]
    Provider,
    #[error("the feedback event has invalid or ambiguous recipient correlation")]
    Correlation,
    #[error("the feedback event type or outcome is unsupported")]
    Event,
    #[error("the feedback event timestamps are invalid or outside the clock-skew bound")]
    Timestamp,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum FeedbackError {
    #[error("the protected feedback HTTP client could not be configured")]
    ClientConfiguration,
    #[error("the feedback request could not be prepared or signed")]
    Preparation,
    #[error("the feedback queue request did not complete")]
    Transport,
    #[error("the feedback service returned an invalid or oversized response")]
    InvalidResponse,
    #[error("feedback queue access was denied")]
    AccessDenied,
    #[error("feedback queue access was throttled")]
    Throttled,
    #[error("the feedback queue is unavailable or rejected the request")]
    QueueUnavailable,
    #[error("the feedback service failed before the operation outcome could be confirmed")]
    ServiceFailure,
    #[error("the feedback queue resource policy does not match the protected SNS subscription")]
    QueuePolicy,
    #[error("the feedback queue protection, size, or retention settings are unsupported")]
    QueueBounds,
    #[error(
        "the feedback dead-letter queue is absent or violates its protection and retention contract"
    )]
    DeadLetterPolicy,
    #[error("feedback acknowledgement did not complete; durable feedback must remain idempotent")]
    AcknowledgementUnknown,
}

pub(super) struct FeedbackPoll {
    pub provider_now: OffsetDateTime,
    pub delivery: Option<FeedbackDelivery>,
}

pub(super) struct FeedbackQueueStatus {
    pub provider_now: OffsetDateTime,
    pub queued: u64,
    pub in_flight: u64,
    pub delayed: u64,
    pub dead_lettered: u64,
    pub source_retention_seconds: u32,
}

impl FeedbackQueueStatus {
    fn is_drained(&self) -> bool {
        self.queued == 0 && self.in_flight == 0 && self.delayed == 0 && self.dead_lettered == 0
    }
}

#[cfg(test)]
mod tests;
