use std::{sync::Arc, time::Duration};

use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::database::store::{DatabaseAdmissionError, DatabaseCommandError, DatabaseMutationError};

use super::{
    activation::{
        PublicationActivationError, PublicationCoordinatorHandle, PublicationCoordinatorUnavailable,
    },
    store::{PublicationStore, StartupSnapshotLoadError},
};

const RETRY_DELAY: Duration = Duration::from_millis(100);

/// Activates durable scheduled approvals when their UTC activation time arrives.
pub(crate) struct PublicationScheduler {
    store: PublicationStore,
    coordinator: PublicationCoordinatorHandle,
    wakeup: Arc<Notify>,
    cancellation: CancellationToken,
}

impl PublicationScheduler {
    pub(crate) fn new(
        store: PublicationStore,
        coordinator: PublicationCoordinatorHandle,
        wakeup: Arc<Notify>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            store,
            coordinator,
            wakeup,
            cancellation,
        }
    }

    /// Runs until cancellation or an activation outcome that cannot be retried safely.
    pub(crate) async fn run(self) -> Result<(), PublicationSchedulerError> {
        loop {
            let next = tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => return Ok(()),
                result = self.store.next_scheduled_publication() => {
                    result.map_err(PublicationSchedulerError::Load)?
                }
            };

            let Some(scheduled) = next else {
                if wait_for_requery(None, &self.wakeup, &self.cancellation).await
                    == WaitOutcome::Cancelled
                {
                    return Ok(());
                }
                continue;
            };

            let scheduled_at = scheduled.publication.view().scheduled_at;
            let delay = delay_until(scheduled_at, OffsetDateTime::now_utc());
            if !delay.is_zero() {
                if wait_for_requery(Some(delay), &self.wakeup, &self.cancellation).await
                    == WaitOutcome::Cancelled
                {
                    return Ok(());
                }
                continue;
            }

            let publication_id = scheduled.publication_id;
            // Once admitted, scheduled activation must run to a known durable
            // outcome even when shutdown is requested concurrently.
            let result = self
                .coordinator
                .activate_scheduled(publication_id, OffsetDateTime::now_utc())
                .await;

            if self.cancellation.is_cancelled()
                && matches!(
                    &result,
                    Err(PublicationActivationError::Coordinator(
                        PublicationCoordinatorUnavailable::Closed
                    ))
                )
            {
                return Ok(());
            }

            match result {
                Ok(_) => {}
                Err(error) if retryable(&error) => {
                    if wait_for_requery(Some(RETRY_DELAY), &self.wakeup, &self.cancellation).await
                        == WaitOutcome::Cancelled
                    {
                        return Ok(());
                    }
                }
                Err(source) => {
                    return Err(PublicationSchedulerError::Activation {
                        publication_id,
                        source,
                    });
                }
            }
        }
    }
}

fn delay_until(scheduled_at: OffsetDateTime, now: OffsetDateTime) -> Duration {
    if scheduled_at <= now {
        return Duration::ZERO;
    }
    (scheduled_at - now).try_into().unwrap_or(Duration::MAX)
}

fn retryable(error: &PublicationActivationError) -> bool {
    matches!(
        error,
        PublicationActivationError::Database(
            DatabaseMutationError::Admission(DatabaseAdmissionError::QueueFull)
                | DatabaseMutationError::Command(
                    DatabaseCommandError::IdempotencyConflict | DatabaseCommandError::Rejected
                )
        )
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitOutcome {
    Requery,
    Cancelled,
}

async fn wait_for_requery(
    delay: Option<Duration>,
    wakeup: &Notify,
    cancellation: &CancellationToken,
) -> WaitOutcome {
    match delay {
        Some(delay) => {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => WaitOutcome::Cancelled,
                _ = wakeup.notified() => WaitOutcome::Requery,
                _ = tokio::time::sleep(delay) => WaitOutcome::Requery,
            }
        }
        None => {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => WaitOutcome::Cancelled,
                _ = wakeup.notified() => WaitOutcome::Requery,
            }
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum PublicationSchedulerError {
    #[error("could not load the scheduled publication queue")]
    Load(#[source] StartupSnapshotLoadError),
    #[error("could not activate scheduled publication {publication_id}")]
    Activation {
        publication_id: Uuid,
        #[source]
        source: PublicationActivationError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryability_is_limited_to_backpressure_and_ordinary_conflicts() {
        let queue_full =
            PublicationActivationError::Database(DatabaseAdmissionError::QueueFull.into());
        let rejected = PublicationActivationError::Database(DatabaseCommandError::Rejected.into());
        let idempotency_conflict =
            PublicationActivationError::Database(DatabaseCommandError::IdempotencyConflict.into());
        let writer_closed =
            PublicationActivationError::Database(DatabaseAdmissionError::WriterClosed.into());
        let uncertain =
            PublicationActivationError::Database(DatabaseCommandError::OutcomeUnknown.into());
        let invalid =
            PublicationActivationError::Database(DatabaseCommandError::InvalidValue.into());

        assert!(retryable(&queue_full));
        assert!(retryable(&rejected));
        assert!(retryable(&idempotency_conflict));
        assert!(!retryable(&writer_closed));
        assert!(!retryable(&uncertain));
        assert!(!retryable(&invalid));
        assert!(!retryable(
            &PublicationActivationError::DurableStateMismatch
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn timed_wait_requeries_at_deadline() {
        let wakeup = Arc::new(Notify::new());
        let cancellation = CancellationToken::new();
        let task = tokio::spawn({
            let wakeup = Arc::clone(&wakeup);
            let cancellation = cancellation.clone();
            async move { wait_for_requery(Some(Duration::from_secs(60)), &wakeup, &cancellation).await }
        });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!task.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(task.await.unwrap(), WaitOutcome::Requery);
    }

    #[tokio::test(start_paused = true)]
    async fn wakeup_requeries_before_a_later_deadline() {
        let wakeup = Arc::new(Notify::new());
        let cancellation = CancellationToken::new();
        let task = tokio::spawn({
            let wakeup = Arc::clone(&wakeup);
            let cancellation = cancellation.clone();
            async move { wait_for_requery(Some(Duration::from_secs(60)), &wakeup, &cancellation).await }
        });

        tokio::task::yield_now().await;
        wakeup.notify_one();
        assert_eq!(task.await.unwrap(), WaitOutcome::Requery);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_stops_an_idle_wait() {
        let wakeup = Arc::new(Notify::new());
        let cancellation = CancellationToken::new();
        let task = tokio::spawn({
            let wakeup = Arc::clone(&wakeup);
            let cancellation = cancellation.clone();
            async move { wait_for_requery(None, &wakeup, &cancellation).await }
        });

        tokio::task::yield_now().await;
        cancellation.cancel();
        assert_eq!(task.await.unwrap(), WaitOutcome::Cancelled);
    }
}
