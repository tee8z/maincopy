use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use bytes::Bytes;
use reqwest::{Method, Request, StatusCode, header::HeaderValue};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc2822};
use url::Url;

use super::{
    FeedbackClient, FeedbackConfiguration, FeedbackDelivery, FeedbackError, FeedbackPoll,
    FeedbackQueueStatus, event,
};
use crate::domain::mail::ses::{
    ProtectedBytes, ProtectedText, SesCredentials, SigningService, read_response, sign_request,
};

mod policy;

const MAX_ENVELOPE_BYTES: usize = 64 * 1024;
const MAX_RECEIPT_BYTES: usize = 4096;

/// A transient queue capability. It is never stored, logged, or automatically
/// acknowledged. A duplicate delivery must pass the writer's durable dedup.
pub(in crate::domain::mail) struct FeedbackReceipt(ProtectedText);

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct QueueMessage {
    body: ProtectedText,
    receipt_handle: ProtectedText,
    attributes: MessageAttributes,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MessageAttributes {
    approximate_receive_count: ProtectedText,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReceiveReply {
    #[serde(default)]
    messages: Vec<QueueMessage>,
}

impl FeedbackClient {
    pub(in crate::domain::mail) fn new(
        configuration: FeedbackConfiguration,
        credentials: Arc<SesCredentials>,
    ) -> Result<Self, FeedbackError> {
        let endpoint = Url::parse(&format!(
            "https://sqs.{}.amazonaws.com/",
            configuration.region
        ))
        .map_err(|_| FeedbackError::ClientConfiguration)?;
        let http = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(35))
            .referer(false)
            .build()
            .map_err(|_| FeedbackError::ClientConfiguration)?;
        Ok(Self {
            http,
            endpoint,
            credentials,
            configuration,
        })
    }

    /// Verify the limited queue-policy contract, not all effective account IAM
    /// or the SNS/SES subscription. Deployment must prove those independently.
    pub(in crate::domain::mail) async fn preflight(
        &self,
    ) -> Result<FeedbackQueueStatus, FeedbackError> {
        let source = self
            .attributes(self.configuration.queue_url.as_str())
            .await?;
        let dead_letter = source.attributes.validate_source(&self.configuration)?;
        let dead = self.attributes(dead_letter.url.as_str()).await?;
        dead.attributes
            .validate_dead_letter(&self.configuration, &dead_letter)?;
        let counts = source.attributes.counts()?;
        Ok(FeedbackQueueStatus {
            provider_now: source.provider_now.min(dead.provider_now),
            queued: counts.queued,
            in_flight: counts.in_flight,
            delayed: counts.delayed,
            dead_lettered: dead.attributes.counts()?.total()?,
            source_retention_seconds: dead_letter.source_retention,
        })
    }

    /// Poll exactly one envelope. Invalid trusted-queue data retains its receipt
    /// as poison; runtime leaves it unacknowledged and reports degraded feedback.
    pub(in crate::domain::mail) async fn receive(
        &self,
        signed_at: OffsetDateTime,
    ) -> Result<FeedbackPoll, FeedbackError> {
        #[derive(Serialize)]
        #[serde(rename_all = "PascalCase")]
        struct Receive<'queue> {
            queue_url: &'queue str,
            max_number_of_messages: u8,
            wait_time_seconds: u8,
            visibility_timeout: u16,
            message_system_attribute_names: [&'static str; 1],
        }
        let body = self
            .request(
                "AmazonSQS.ReceiveMessage",
                &Receive {
                    queue_url: self.configuration.queue_url.as_str(),
                    max_number_of_messages: 1,
                    wait_time_seconds: 20,
                    visibility_timeout: 120,
                    message_system_attribute_names: ["ApproximateReceiveCount"],
                },
                signing_time(signed_at)?,
            )
            .await?;
        let provider_now = body.provider_now.ok_or(FeedbackError::InvalidResponse)?;
        let reply: ReceiveReply = serde_json::from_slice(body.body.as_ref())
            .map_err(|_| FeedbackError::InvalidResponse)?;
        if reply.messages.len() > 1 {
            return Err(FeedbackError::InvalidResponse);
        }
        let delivery = reply
            .messages
            .into_iter()
            .next()
            .map(|message| message.into_delivery(&self.configuration, provider_now))
            .transpose()?;
        Ok(FeedbackPoll {
            provider_now,
            delivery,
        })
    }

    /// Call only after the corresponding writer command committed. Even a 200
    /// can be followed by duplicate standard-queue delivery; do not erase dedup.
    pub(in crate::domain::mail) async fn acknowledge(
        &self,
        receipt: FeedbackReceipt,
    ) -> Result<(), FeedbackError> {
        #[derive(Serialize)]
        #[serde(rename_all = "PascalCase")]
        struct Delete<'request> {
            queue_url: &'request str,
            receipt_handle: &'request str,
        }
        self.request(
            "AmazonSQS.DeleteMessage",
            &Delete {
                queue_url: self.configuration.queue_url.as_str(),
                receipt_handle: receipt.0.as_str(),
            },
            SystemTime::now(),
        )
        .await
        .map(|_| ())
        .map_err(|_| FeedbackError::AcknowledgementUnknown)
    }

    async fn attributes(&self, queue_url: &str) -> Result<AttributeObservation, FeedbackError> {
        #[derive(Serialize)]
        #[serde(rename_all = "PascalCase")]
        struct Get<'queue> {
            queue_url: &'queue str,
            attribute_names: [&'static str; 1],
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Reply {
            attributes: policy::QueueAttributes,
        }
        let body = self
            .request(
                "AmazonSQS.GetQueueAttributes",
                &Get {
                    queue_url,
                    attribute_names: ["All"],
                },
                SystemTime::now(),
            )
            .await?;
        let provider_now = body.provider_now.ok_or(FeedbackError::InvalidResponse)?;
        let reply: Reply = serde_json::from_slice(body.body.as_ref())
            .map_err(|_| FeedbackError::InvalidResponse)?;
        Ok(AttributeObservation {
            attributes: reply.attributes,
            provider_now,
        })
    }

    async fn request(
        &self,
        target: &'static str,
        value: &impl Serialize,
        signed_at: SystemTime,
    ) -> Result<QueueReply, FeedbackError> {
        let body = ProtectedBytes::json(value).map_err(|_| FeedbackError::Preparation)?;
        let mut request = Request::new(Method::POST, self.endpoint.clone());
        request
            .headers_mut()
            .insert("x-amz-target", HeaderValue::from_static(target));
        sign_request(
            &mut request,
            body.as_ref(),
            &self.credentials,
            &self.configuration.region,
            SigningService::Sqs,
            signed_at,
        )
        .map_err(|_| FeedbackError::Preparation)?;
        *request.body_mut() = Some(Bytes::from_owner(body).into());
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|_| FeedbackError::Transport)?;
        let status = response.status();
        let provider_now = response
            .headers()
            .get(reqwest::header::DATE)
            .filter(|value| value.as_bytes().len() <= 64)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| OffsetDateTime::parse(value, &Rfc2822).ok())
            .filter(|value| value.unix_timestamp() >= 0);
        let body = read_response(response)
            .await
            .map_err(|_| FeedbackError::InvalidResponse)?;
        if status == StatusCode::OK {
            return Ok(QueueReply { body, provider_now });
        }
        Err(service_failure(status, body.as_ref()))
    }
}

struct QueueReply {
    body: ProtectedBytes,
    provider_now: Option<OffsetDateTime>,
}

struct AttributeObservation {
    attributes: policy::QueueAttributes,
    provider_now: OffsetDateTime,
}

fn signing_time(signed_at: OffsetDateTime) -> Result<SystemTime, FeedbackError> {
    let seconds =
        u64::try_from(signed_at.unix_timestamp()).map_err(|_| FeedbackError::Preparation)?;
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .ok_or(FeedbackError::Preparation)
}

impl QueueMessage {
    fn into_delivery(
        self,
        configuration: &FeedbackConfiguration,
        now: OffsetDateTime,
    ) -> Result<FeedbackDelivery, FeedbackError> {
        let receipt = self.receipt_handle.as_str();
        if receipt.is_empty()
            || receipt.len() > MAX_RECEIPT_BYTES
            || !receipt.bytes().all(|byte| (33..=126).contains(&byte))
        {
            return Err(FeedbackError::InvalidResponse);
        }
        let receive_count: u32 = self
            .attributes
            .approximate_receive_count
            .as_str()
            .parse()
            .map_err(|_| FeedbackError::InvalidResponse)?;
        if receive_count == 0 {
            return Err(FeedbackError::InvalidResponse);
        }
        let event = if self.body.as_str().len() > MAX_ENVELOPE_BYTES {
            Err(super::FeedbackRejection::Envelope)
        } else {
            event::parse(self.body.as_str().as_bytes(), configuration, now)
        };
        Ok(FeedbackDelivery {
            receipt: FeedbackReceipt(self.receipt_handle),
            event,
        })
    }
}

fn service_failure(status: StatusCode, body: &[u8]) -> FeedbackError {
    if status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT {
        return FeedbackError::ServiceFailure;
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        #[serde(rename = "__type")]
        kind: ProtectedText,
    }
    let problem = serde_json::from_slice::<ErrorBody>(body).ok();
    let kind = problem
        .as_ref()
        .map(|value| value.kind.as_str().rsplit('#').next().unwrap());
    match kind {
        Some(
            "AccessDenied"
            | "AccessDeniedException"
            | "InvalidSecurity"
            | "InvalidClientTokenId"
            | "SignatureDoesNotMatch",
        ) => FeedbackError::AccessDenied,
        Some("RequestThrottled" | "ThrottlingException") => FeedbackError::Throttled,
        _ => match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => FeedbackError::AccessDenied,
            StatusCode::TOO_MANY_REQUESTS => FeedbackError::Throttled,
            _ => FeedbackError::QueueUnavailable,
        },
    }
}
