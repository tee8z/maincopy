//! Retention continues when delivery is disabled, so removing credentials cannot
//! indefinitely retain old attempt bindings or expired suppression digests.

use std::time::Duration;

use thiserror::Error;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use super::subscriber::store::{SubscriberMutationError, SubscriberStore};
use crate::database::store::DatabaseAdmissionError;

pub(crate) struct MailRetention {
    subscribers: SubscriberStore,
}

#[derive(Debug, Error)]
#[error("subscriber retention could not confirm its database transition")]
pub(crate) struct MailRetentionError(#[source] SubscriberMutationError);

impl MailRetention {
    pub(crate) fn new(subscribers: SubscriberStore) -> Self {
        Self { subscribers }
    }

    pub(crate) async fn run(self, shutdown: CancellationToken) -> Result<(), MailRetentionError> {
        let mut ticks = tokio::time::interval(Duration::from_secs(1));
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut next_cleanup = Instant::now();
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return Ok(()),
                _ = ticks.tick() => {}
            }
            if Instant::now() < next_cleanup {
                continue;
            }
            match self.subscribers.cleanup().await {
                Ok(cleanup) => {
                    let remaining = cleanup.expired_enrollments
                        + cleanup.removed_attempts
                        + cleanup.removed_enrollments
                        + cleanup.removed_suppressions;
                    next_cleanup =
                        Instant::now() + Duration::from_secs(if remaining == 0 { 60 } else { 1 });
                }
                Err(SubscriberMutationError::Admission(DatabaseAdmissionError::QueueFull)) => {}
                Err(error) => return Err(MailRetentionError(error)),
            }
        }
    }
}
