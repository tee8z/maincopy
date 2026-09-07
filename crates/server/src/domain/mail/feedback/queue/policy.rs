use serde::Deserialize;
use url::Url;

use super::super::{FeedbackConfiguration, FeedbackError, resource_name};
use crate::domain::mail::ses::ProtectedText;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct QueueAttributes {
    queue_arn: ProtectedText,
    policy: Option<ProtectedText>,
    message_retention_period: ProtectedText,
    maximum_message_size: ProtectedText,
    sqs_managed_sse_enabled: ProtectedText,
    redrive_policy: Option<ProtectedText>,
    redrive_allow_policy: Option<ProtectedText>,
    approximate_number_of_messages: ProtectedText,
    approximate_number_of_messages_not_visible: ProtectedText,
    approximate_number_of_messages_delayed: ProtectedText,
}

pub(super) struct DeadLetter {
    pub url: Url,
    arn: String,
    pub source_retention: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RedrivePolicy {
    dead_letter_target_arn: ProtectedText,
    max_receive_count: ReceiveCount,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ReceiveCount {
    Number(u32),
    Text(ProtectedText),
}

impl ReceiveCount {
    fn value(&self) -> Result<u32, FeedbackError> {
        match self {
            Self::Number(value) => Ok(*value),
            Self::Text(value) => value
                .as_str()
                .parse()
                .map_err(|_| FeedbackError::DeadLetterPolicy),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RedriveAllow {
    redrive_permission: ProtectedText,
    source_queue_arns: Vec<ProtectedText>,
}

impl QueueAttributes {
    pub(super) fn validate_source(
        &self,
        configuration: &FeedbackConfiguration,
    ) -> Result<DeadLetter, FeedbackError> {
        let retention = self.protection(&configuration.queue_arn, 86_400)?;
        let policy = self.policy.as_ref().ok_or(FeedbackError::QueuePolicy)?;
        validate_policy(policy.as_str(), configuration)?;
        let redrive: RedrivePolicy = serde_json::from_str(
            self.redrive_policy
                .as_ref()
                .ok_or(FeedbackError::DeadLetterPolicy)?
                .as_str(),
        )
        .map_err(|_| FeedbackError::DeadLetterPolicy)?;
        if !(3..=10).contains(&redrive.max_receive_count.value()?) {
            return Err(FeedbackError::DeadLetterPolicy);
        }
        let prefix = format!(
            "arn:aws:sqs:{}:{}:",
            configuration.region, configuration.account_id
        );
        let name = redrive
            .dead_letter_target_arn
            .as_str()
            .strip_prefix(&prefix)
            .ok_or(FeedbackError::DeadLetterPolicy)?;
        if !resource_name(name, 80)
            || redrive.dead_letter_target_arn.as_str() == configuration.queue_arn
        {
            return Err(FeedbackError::DeadLetterPolicy);
        }
        let mut url = configuration.queue_url.clone();
        url.set_path(&format!("/{}/{name}", configuration.account_id));
        Ok(DeadLetter {
            url,
            arn: redrive.dead_letter_target_arn.as_str().into(),
            source_retention: retention,
        })
    }

    pub(super) fn validate_dead_letter(
        &self,
        configuration: &FeedbackConfiguration,
        dead_letter: &DeadLetter,
    ) -> Result<(), FeedbackError> {
        let retention = self
            .protection(&dead_letter.arn, 345_600)
            .map_err(|_| FeedbackError::DeadLetterPolicy)?;
        if retention <= dead_letter.source_retention || self.redrive_policy.is_some() {
            return Err(FeedbackError::DeadLetterPolicy);
        }
        // No additional resource-policy senders or chained DLQ are supported.
        // Account IAM remains a separate deployment trust boundary.
        if self
            .policy
            .as_ref()
            .is_some_and(|value| !value.as_str().is_empty())
        {
            return Err(FeedbackError::DeadLetterPolicy);
        }
        let allow: RedriveAllow = serde_json::from_str(
            self.redrive_allow_policy
                .as_ref()
                .ok_or(FeedbackError::DeadLetterPolicy)?
                .as_str(),
        )
        .map_err(|_| FeedbackError::DeadLetterPolicy)?;
        if allow.redrive_permission.as_str() != "byQueue"
            || allow.source_queue_arns.len() != 1
            || allow.source_queue_arns[0].as_str() != configuration.queue_arn
        {
            return Err(FeedbackError::DeadLetterPolicy);
        }
        Ok(())
    }

    fn protection(&self, expected_arn: &str, retention_limit: u32) -> Result<u32, FeedbackError> {
        let retention: u32 = self
            .message_retention_period
            .as_str()
            .parse()
            .map_err(|_| FeedbackError::QueueBounds)?;
        let size: u32 = self
            .maximum_message_size
            .as_str()
            .parse()
            .map_err(|_| FeedbackError::QueueBounds)?;
        if self.queue_arn.as_str() != expected_arn
            || self.sqs_managed_sse_enabled.as_str() != "true"
            || !(3600..=retention_limit).contains(&retention)
            || !(1024..=65_536).contains(&size)
        {
            return Err(FeedbackError::QueueBounds);
        }
        Ok(retention)
    }

    pub(super) fn counts(&self) -> Result<QueueCounts, FeedbackError> {
        let count = |value: &ProtectedText| {
            value
                .as_str()
                .parse()
                .map_err(|_| FeedbackError::InvalidResponse)
        };
        Ok(QueueCounts {
            queued: count(&self.approximate_number_of_messages)?,
            in_flight: count(&self.approximate_number_of_messages_not_visible)?,
            delayed: count(&self.approximate_number_of_messages_delayed)?,
        })
    }
}

pub(super) struct QueueCounts {
    pub queued: u64,
    pub in_flight: u64,
    pub delayed: u64,
}

impl QueueCounts {
    pub(super) fn total(&self) -> Result<u64, FeedbackError> {
        self.queued
            .checked_add(self.in_flight)
            .and_then(|total| total.checked_add(self.delayed))
            .ok_or(FeedbackError::InvalidResponse)
    }
}

// This is an intentionally narrow resource-policy format, not an IAM policy
// interpreter. Reject extra statements, principals, wildcards, or conditions
// rather than claiming to evaluate arbitrary effective permissions.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "PascalCase")]
struct Policy {
    version: ProtectedText,
    #[serde(rename = "Id")]
    _id: Option<ProtectedText>,
    statement: Vec<Statement>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "PascalCase")]
struct Statement {
    #[serde(rename = "Sid")]
    _sid: Option<ProtectedText>,
    effect: ProtectedText,
    principal: Principal,
    action: ProtectedText,
    resource: ProtectedText,
    condition: Condition,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Principal {
    Service(ServicePrincipal),
    Wildcard(ProtectedText),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "PascalCase")]
struct ServicePrincipal {
    service: ProtectedText,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "PascalCase")]
struct Condition {
    arn_equals: Option<SourceArn>,
    string_equals: Option<SourceAccount>,
    arn_not_equals: Option<SourceArn>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceArn {
    #[serde(rename = "aws:SourceArn")]
    source: ProtectedText,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceAccount {
    #[serde(rename = "aws:SourceAccount")]
    account: ProtectedText,
}

fn validate_policy(
    value: &str,
    configuration: &FeedbackConfiguration,
) -> Result<(), FeedbackError> {
    if value.len() > 16 * 1024 {
        return Err(FeedbackError::QueuePolicy);
    }
    let policy: Policy = serde_json::from_str(value).map_err(|_| FeedbackError::QueuePolicy)?;
    if policy.version.as_str() != "2012-10-17" || policy.statement.len() != 2 {
        return Err(FeedbackError::QueuePolicy);
    }
    let allow = policy
        .statement
        .iter()
        .find(|statement| statement.effect.as_str() == "Allow")
        .ok_or(FeedbackError::QueuePolicy)?;
    let deny = policy
        .statement
        .iter()
        .find(|statement| statement.effect.as_str() == "Deny")
        .ok_or(FeedbackError::QueuePolicy)?;
    for statement in [allow, deny] {
        if statement.action.as_str() != "sqs:SendMessage"
            || statement.resource.as_str() != configuration.queue_arn
        {
            return Err(FeedbackError::QueuePolicy);
        }
    }
    allow.validate_allow(configuration)?;
    deny.validate_deny(configuration)
}

impl Statement {
    fn validate_allow(&self, configuration: &FeedbackConfiguration) -> Result<(), FeedbackError> {
        if !matches!(&self.principal, Principal::Service(principal) if principal.service.as_str() == "sns.amazonaws.com")
            || self.condition.arn_not_equals.is_some()
            || !self
                .condition
                .arn_equals
                .as_ref()
                .is_some_and(|arn| arn.source.as_str() == configuration.topic_arn)
            || !self
                .condition
                .string_equals
                .as_ref()
                .is_some_and(|account| account.account.as_str() == configuration.account_id)
        {
            return Err(FeedbackError::QueuePolicy);
        }
        Ok(())
    }

    fn validate_deny(&self, configuration: &FeedbackConfiguration) -> Result<(), FeedbackError> {
        // AWS negated ARN conditions also match when SourceArn is absent.
        // This explicit Deny overrides same-account identity grants and bars
        // generic DLQ replay into the authenticated source queue.
        if !matches!(&self.principal, Principal::Wildcard(principal) if principal.as_str() == "*")
            || self.condition.arn_equals.is_some()
            || self.condition.string_equals.is_some()
            || !self
                .condition
                .arn_not_equals
                .as_ref()
                .is_some_and(|arn| arn.source.as_str() == configuration.topic_arn)
        {
            return Err(FeedbackError::QueuePolicy);
        }
        Ok(())
    }
}
