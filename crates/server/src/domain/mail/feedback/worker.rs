//! The feedback task owns polling and acknowledgements. Only the database
//! writer changes delivery knowledge, suppression, or admission health.

use std::{future::Future, sync::Arc, time::Duration};

use thiserror::Error;
use time::OffsetDateTime;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    AuthenticatedFeedback, FeedbackClient, FeedbackDelivery, FeedbackError, FeedbackKind,
    FeedbackPoll, FeedbackQueueStatus,
};
use crate::{
    database::store::DatabaseAdmissionError,
    domain::mail::{
        campaign::CampaignId,
        controls::MailControls,
        ses::MessageId,
        subscriber::{
            ApplyFeedback, BeginFeedbackRun, FeedbackHealth, FeedbackKind as StoredFeedbackKind,
            FeedbackObservation, FeedbackPollAdmission, FeedbackPollIntent,
            RecordFeedbackObservation, SubscriberCommandError, SubscriberDigest,
            store::{SubscriberMutationError, SubscriberStore},
        },
    },
};

const PREFLIGHT_INTERVAL: Duration = Duration::from_secs(60);
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(100);
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

pub(crate) struct FeedbackWorker {
    client: FeedbackClient,
    controls: Arc<MailControls>,
    subscribers: SubscriberStore,
    configuration_binding: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum FeedbackWorkerError {
    #[error("the subscriber writer stopped before feedback state could be confirmed")]
    DatabaseUnavailable,
    #[error("the subscriber writer rejected an internal feedback transition")]
    DatabaseInvariant,
}

#[derive(Debug)]
enum WriterFailure {
    Busy,
    Obsolete,
    Conflict,
    Fatal(FeedbackWorkerError),
}

#[cfg(all(test, unix))]
mod tests;

impl From<SubscriberMutationError> for WriterFailure {
    fn from(error: SubscriberMutationError) -> Self {
        match error {
            SubscriberMutationError::Admission(DatabaseAdmissionError::QueueFull) => Self::Busy,
            SubscriberMutationError::Admission(DatabaseAdmissionError::WriterClosed)
            | SubscriberMutationError::Command(SubscriberCommandError::OutcomeUnknown) => {
                Self::Fatal(FeedbackWorkerError::DatabaseUnavailable)
            }
            SubscriberMutationError::Command(SubscriberCommandError::ConfigurationChanged) => {
                Self::Obsolete
            }
            SubscriberMutationError::Command(
                SubscriberCommandError::AttemptConflict | SubscriberCommandError::InvalidValue,
            ) => Self::Conflict,
            SubscriberMutationError::Command(
                SubscriberCommandError::Forbidden
                | SubscriberCommandError::StaleVersion
                | SubscriberCommandError::ReconciliationNotRequired
                | SubscriberCommandError::IdempotencyConflict
                | SubscriberCommandError::Capacity
                | SubscriberCommandError::Paused
                | SubscriberCommandError::ControlIdentityChanged
                | SubscriberCommandError::ControlsUnavailable
                | SubscriberCommandError::CampaignUnavailable,
            ) => Self::Fatal(FeedbackWorkerError::DatabaseInvariant),
        }
    }
}

impl FeedbackWorker {
    /// Construction owns already prepared capabilities and performs no I/O.
    pub(in crate::domain::mail) fn new(
        client: FeedbackClient,
        controls: Arc<MailControls>,
        subscribers: SubscriberStore,
        configuration_binding: [u8; 32],
    ) -> Self {
        Self {
            client,
            controls,
            subscribers,
            configuration_binding,
        }
    }

    pub(crate) async fn run(self, shutdown: CancellationToken) -> Result<(), FeedbackWorkerError> {
        match self.observe(&shutdown).await {
            Ok(()) => Ok(()),
            Err(WriterFailure::Obsolete) => {
                // A replacement policy owns admission. This worker cannot
                // refresh or clear the replacement consumer's durable marker.
                shutdown.cancelled().await;
                Ok(())
            }
            Err(WriterFailure::Fatal(error)) => Err(error),
            Err(WriterFailure::Busy | WriterFailure::Conflict) => {
                Err(FeedbackWorkerError::DatabaseInvariant)
            }
        }
    }

    async fn observe(&self, shutdown: &CancellationToken) -> Result<(), WriterFailure> {
        let Some(mut run) = self.start_run(shutdown).await? else {
            return Ok(());
        };
        let mut observation = PollObservation::Observed(FeedbackObservation::Observed {
            provider_now: run.provider_now,
            drained: false,
        });
        let mut backoff = INITIAL_BACKOFF;
        loop {
            let (health, delay) = match observation {
                PollObservation::Observed(health) => {
                    backoff = INITIAL_BACKOFF;
                    (health, MIN_POLL_INTERVAL)
                }
                PollObservation::Retry(health) => (health, next_backoff(&mut backoff)),
            };
            let health = run.preserve_reconciliation(health);
            self.publish_health(&run, health).await?;
            if cancelled_during(shutdown, delay).await {
                break;
            }
            observation = match self.cycle(&mut run, shutdown).await {
                Ok(Some(observation)) => observation,
                Ok(None) => break,
                Err(WriterFailure::Conflict) => {
                    PollObservation::Retry(FeedbackObservation::ReconciliationRequired)
                }
                Err(error) => return Err(error),
            };
        }
        self.finish_run(&run).await
    }

    async fn start_run(
        &self,
        shutdown: &CancellationToken,
    ) -> Result<Option<ConsumerRun>, WriterFailure> {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            // Admission starts unavailable; an admitted writer command is
            // awaited even when cancellation arrives while it is in flight.
            match self
                .subscribers
                .record_feedback_health(self.configuration_binding, FeedbackHealth::Unavailable)
                .await
            {
                Ok(()) => {}
                Err(error) => match WriterFailure::from(error) {
                    WriterFailure::Busy => {
                        if cancelled_during(shutdown, next_backoff(&mut backoff)).await {
                            return Ok(None);
                        }
                        continue;
                    }
                    error => return Err(error),
                },
            }
            let status = tokio::select! {
                biased;
                _ = shutdown.cancelled() => return Ok(None),
                status = self.client.preflight() => status,
            };
            match status {
                Ok(status) => return self.admit_run(status).await.map(Some),
                Err(error) => {
                    self.preflight_failure(error).await?;
                }
            }
            if cancelled_during(shutdown, next_backoff(&mut backoff)).await {
                return Ok(None);
            }
        }
    }

    async fn admit_run(&self, status: FeedbackQueueStatus) -> Result<ConsumerRun, WriterFailure> {
        let source_binding = self.client.configuration.source_binding();
        let admitted = retry_writer_admission(|| async {
            self.subscribers
                .begin_feedback_run(BeginFeedbackRun {
                    configuration_binding: self.configuration_binding,
                    source_binding,
                    retention_seconds: status.source_retention_seconds,
                    provider_now: status.provider_now,
                })
                .await
                .map_err(WriterFailure::from)
        })
        .await?;
        Ok(ConsumerRun {
            run_id: admitted.run_id,
            source_binding,
            retention_seconds: status.source_retention_seconds,
            preflight_at: Instant::now(),
            drained: false,
            provider_now: status.provider_now,
            recover_after: admitted.recover_after,
            reconciliation_required: admitted.health == FeedbackHealth::ReconciliationRequired
                || status.dead_lettered > 0,
        })
    }

    async fn cycle(
        &self,
        run: &mut ConsumerRun,
        shutdown: &CancellationToken,
    ) -> Result<Option<PollObservation>, WriterFailure> {
        if Instant::now() >= run.preflight_at {
            let status = tokio::select! {
                biased;
                _ = shutdown.cancelled() => return Ok(None),
                status = self.client.preflight() => status,
            };
            let status = match status {
                Ok(status) => status,
                Err(error) => {
                    run.drained = false;
                    return Ok(Some(PollObservation::Retry(
                        self.preflight_failure(error).await?,
                    )));
                }
            };
            if !run.observe_queue(status) {
                return Ok(Some(PollObservation::Retry(
                    FeedbackObservation::Unavailable,
                )));
            }
            // A successful preflight advances the trusted provider clock for
            // recovery, but never claims the queue has been drained by itself.
            let observation = if run.drained {
                FeedbackObservation::Checked {
                    provider_now: run.provider_now,
                }
            } else {
                FeedbackObservation::Observed {
                    provider_now: run.provider_now,
                    drained: false,
                }
            };
            let observation = run.preserve_reconciliation(observation);
            self.publish_health(run, observation).await?;
        }
        if shutdown.is_cancelled() {
            return Ok(None);
        }
        if run
            .recover_after
            .is_some_and(|deadline| run.provider_now < deadline)
        {
            return Ok(Some(PollObservation::Retry(
                FeedbackObservation::Unavailable,
            )));
        }
        let intent = match self.admit_poll(run).await? {
            FeedbackPollAdmission::Ready(intent) => intent,
            FeedbackPollAdmission::Recovering => {
                return Ok(Some(PollObservation::Retry(
                    FeedbackObservation::Unavailable,
                )));
            }
        };
        // The exact signing timestamp is durable before network I/O. A lost
        // reply retains a protocol-bounded visibility lease across restart.
        let observation = match self.client.receive(intent.signed_at).await {
            Ok(poll) => self.apply_poll(run, intent, poll).await?,
            Err(
                FeedbackError::Transport
                | FeedbackError::InvalidResponse
                | FeedbackError::ServiceFailure,
            ) => {
                self.settle_poll(run, intent.poll_id, PollCompletion::Uncertain)
                    .await?;
                run.recover_after = Some(intent.recover_after);
                PollObservation::Retry(FeedbackObservation::Unavailable)
            }
            Err(_) => {
                self.settle_poll(run, intent.poll_id, PollCompletion::Confirmed)
                    .await?;
                PollObservation::Retry(FeedbackObservation::Unavailable)
            }
        };
        Ok(Some(observation))
    }

    async fn admit_poll(&self, run: &ConsumerRun) -> Result<FeedbackPollAdmission, WriterFailure> {
        retry_writer_admission(|| async {
            self.subscribers
                .begin_feedback_poll(self.configuration_binding, run.run_id)
                .await
                .map_err(WriterFailure::from)
        })
        .await
    }

    async fn preflight_failure(
        &self,
        error: FeedbackError,
    ) -> Result<FeedbackObservation, WriterFailure> {
        match error {
            FeedbackError::QueuePolicy
            | FeedbackError::QueueBounds
            | FeedbackError::DeadLetterPolicy => {}
            FeedbackError::ClientConfiguration
            | FeedbackError::Preparation
            | FeedbackError::Transport
            | FeedbackError::InvalidResponse
            | FeedbackError::AccessDenied
            | FeedbackError::Throttled
            | FeedbackError::QueueUnavailable
            | FeedbackError::ServiceFailure
            | FeedbackError::AcknowledgementUnknown => return Ok(FeedbackObservation::Unavailable),
        }
        // A successful read revealed that the feedback trust or retention
        // contract was broken. Repairing the queue cannot prove that existing
        // consent received every authentic event during that exposure. The
        // writer checks retained state and latches the gap in one transaction;
        // initial setup without subscriber history remains merely unavailable.
        let health = retry_writer_admission(|| async {
            self.subscribers
                .record_feedback_integrity_failure(self.configuration_binding)
                .await
                .map_err(WriterFailure::from)
        })
        .await?;
        match health {
            FeedbackHealth::ReconciliationRequired => {
                Ok(FeedbackObservation::ReconciliationRequired)
            }
            FeedbackHealth::Unavailable => Ok(FeedbackObservation::Unavailable),
            FeedbackHealth::Healthy => {
                Err(WriterFailure::Fatal(FeedbackWorkerError::DatabaseInvariant))
            }
        }
    }

    async fn apply_poll(
        &self,
        run: &mut ConsumerRun,
        intent: FeedbackPollIntent,
        poll: FeedbackPoll,
    ) -> Result<PollObservation, WriterFailure> {
        let outcome = match poll.delivery {
            Some(delivery) => {
                run.drained = false;
                self.apply_delivery(delivery).await
            }
            None => Ok(DeliveryOutcome::Acknowledged),
        };
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(WriterFailure::Conflict) => DeliveryOutcome::Poison,
            Err(error) => return Err(error),
        };
        if outcome == DeliveryOutcome::Poison {
            run.reconciliation_required = true;
            self.publish_health(run, FeedbackObservation::ReconciliationRequired)
                .await?;
            self.settle_poll(run, intent.poll_id, PollCompletion::Uncertain)
                .await?;
            return Ok(PollObservation::Retry(
                FeedbackObservation::ReconciliationRequired,
            ));
        }
        // Even an ambiguous acknowledgement follows a confirmed feedback
        // commit. Its possible duplicate cannot conceal missing knowledge.
        self.settle_poll(run, intent.poll_id, PollCompletion::Confirmed)
            .await?;
        if poll.provider_now < run.provider_now
            || outcome == DeliveryOutcome::AcknowledgementUnknown
        {
            return Ok(PollObservation::Retry(FeedbackObservation::Unavailable));
        }
        run.provider_now = poll.provider_now;
        Ok(PollObservation::Observed(FeedbackObservation::Observed {
            provider_now: poll.provider_now,
            drained: run.drained,
        }))
    }

    async fn settle_poll(
        &self,
        run: &ConsumerRun,
        poll_id: Uuid,
        completion: PollCompletion,
    ) -> Result<(), WriterFailure> {
        retry_writer_admission(|| async {
            let result = match completion {
                PollCompletion::Confirmed => {
                    self.subscribers
                        .complete_feedback_poll(self.configuration_binding, run.run_id, poll_id)
                        .await
                }
                PollCompletion::Uncertain => {
                    self.subscribers
                        .defer_feedback_poll(self.configuration_binding, run.run_id, poll_id)
                        .await
                }
            };
            result.map_err(WriterFailure::from)
        })
        .await
    }

    async fn apply_delivery(
        &self,
        delivery: FeedbackDelivery,
    ) -> Result<DeliveryOutcome, WriterFailure> {
        let event = match delivery.event {
            Ok(event) => event,
            Err(_) => return Ok(DeliveryOutcome::Poison),
        };
        self.commit_event(&event).await?;
        // Shutdown drains through acknowledgement; no admitted queue or writer
        // operation is dropped by the cooperative cancellation path.
        if self.client.acknowledge(delivery.receipt).await.is_err() {
            return Ok(DeliveryOutcome::AcknowledgementUnknown);
        }
        Ok(DeliveryOutcome::Acknowledged)
    }

    async fn commit_event(&self, event: &AuthenticatedFeedback) -> Result<(), WriterFailure> {
        retry_writer_admission(|| async {
            let command = self.command(event)?;
            self.subscribers
                .apply_feedback(command)
                .await
                .map(|_| ())
                .map_err(WriterFailure::from)
        })
        .await
    }

    fn command(&self, event: &AuthenticatedFeedback) -> Result<ApplyFeedback, WriterFailure> {
        let kind = match event.kind {
            FeedbackKind::Accepted => StoredFeedbackKind::Accepted,
            FeedbackKind::Delivered => StoredFeedbackKind::Delivered,
            FeedbackKind::HardBounce => StoredFeedbackKind::HardBounce,
            FeedbackKind::Complaint => StoredFeedbackKind::Complaint,
            FeedbackKind::DeliveryFailed => StoredFeedbackKind::DeliveryFailed,
        };
        // Keep the protected event until queue admission succeeds. A full
        // writer queue must not discard an otherwise valid invisible delivery.
        let provider_message_id = MessageId::parse(event.provider_message_id.as_str())
            .map_err(|_| WriterFailure::Fatal(FeedbackWorkerError::DatabaseInvariant))?;
        Ok(ApplyFeedback {
            attempt_id: event.attempt_id,
            mail_epoch: event.mail_epoch,
            source_binding: self.client.configuration.source_binding(),
            campaign_id: event.campaign_id.map(CampaignId),
            mailbox_digest: SubscriberDigest::from_bytes(
                self.controls.mailbox_digest(&event.recipient),
            ),
            configuration_binding: self.configuration_binding,
            provider_message_id,
            kind,
            sent_at: event.sent_at,
            occurred_at: event.occurred_at,
        })
    }

    async fn publish_health(
        &self,
        run: &ConsumerRun,
        mut observation: FeedbackObservation,
    ) -> Result<FeedbackObservation, WriterFailure> {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            let result = self
                .subscribers
                .record_feedback_observation(RecordFeedbackObservation {
                    configuration_binding: self.configuration_binding,
                    run_id: run.run_id,
                    source_binding: run.source_binding,
                    retention_seconds: run.retention_seconds,
                    observation,
                })
                .await;
            match result {
                Ok(()) => return Ok(observation),
                Err(error) => match WriterFailure::from(error) {
                    WriterFailure::Busy => {
                        // Delayed positive evidence is no longer a fresh
                        // heartbeat. A pending gap must still reach the writer.
                        if matches!(
                            observation,
                            FeedbackObservation::Observed { .. }
                                | FeedbackObservation::Checked { .. }
                        ) {
                            observation = FeedbackObservation::Unavailable;
                        }
                        sleep(next_backoff(&mut backoff)).await;
                    }
                    failure => return Err(failure),
                },
            }
        }
    }

    async fn finish_run(&self, run: &ConsumerRun) -> Result<(), WriterFailure> {
        retry_writer_admission(|| async {
            self.subscribers
                .finish_feedback_run(self.configuration_binding, run.run_id)
                .await
                .map_err(WriterFailure::from)
        })
        .await
    }
}

// Admission may remain unavailable while valid backlog drains at full pace.
// Exponential retry delay belongs only to failed queue/feedback operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PollObservation {
    Observed(FeedbackObservation),
    Retry(FeedbackObservation),
}

struct ConsumerRun {
    run_id: Uuid,
    source_binding: [u8; 32],
    retention_seconds: u32,
    preflight_at: Instant,
    drained: bool,
    provider_now: OffsetDateTime,
    recover_after: Option<OffsetDateTime>,
    reconciliation_required: bool,
}

impl ConsumerRun {
    fn observe_queue(&mut self, status: FeedbackQueueStatus) -> bool {
        if status.provider_now < self.provider_now {
            self.drained = false;
            return false;
        }
        self.provider_now = status.provider_now;
        self.retention_seconds = status.source_retention_seconds;
        self.drained = status.is_drained();
        self.reconciliation_required |= status.dead_lettered > 0;
        self.preflight_at = Instant::now() + PREFLIGHT_INTERVAL;
        true
    }

    fn preserve_reconciliation(&mut self, observation: FeedbackObservation) -> FeedbackObservation {
        self.reconciliation_required |= observation == FeedbackObservation::ReconciliationRequired;
        if self.reconciliation_required {
            FeedbackObservation::ReconciliationRequired
        } else {
            observation
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeliveryOutcome {
    Acknowledged,
    AcknowledgementUnknown,
    Poison,
}

#[derive(Clone, Copy)]
enum PollCompletion {
    Confirmed,
    Uncertain,
}

/// A full queue has not accepted the command. Retry admission while retaining
/// its identity; once admitted, drain the reply even during shutdown. Every
/// other failure, including an unknown outcome, returns without retry.
async fn retry_writer_admission<T, F>(mut operation: impl FnMut() -> F) -> Result<T, WriterFailure>
where
    F: Future<Output = Result<T, WriterFailure>>,
{
    let mut backoff = INITIAL_BACKOFF;
    loop {
        match operation().await {
            Err(WriterFailure::Busy) => sleep(next_backoff(&mut backoff)).await,
            result => return result,
        }
    }
}

fn next_backoff(backoff: &mut Duration) -> Duration {
    let delay = *backoff;
    *backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
    delay
}

async fn cancelled_during(shutdown: &CancellationToken, delay: Duration) -> bool {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => true,
        _ = sleep(delay) => false,
    }
}
