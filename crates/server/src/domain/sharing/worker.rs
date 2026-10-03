//! One supervised worker posts queued teasers, one request sequence at a time.
//! Settings are read from the database on every pass, so an Owner's change in
//! the admin portal applies without a restart.

use std::time::Duration;

use reqwest::Client;
use thiserror::Error;
use time::OffsetDateTime;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use super::{
    http::client,
    settings::{Channel, ChannelMode, SubstackSettings},
    store::{
        ClaimDelivery, DeliveryFailure, DeliveryOutcome, DueDelivery, FinishDelivery,
        SharingCommandError, SharingLoadError, SharingMutationError, SharingStore,
    },
    substack::SubstackClient,
};
use crate::database::store::DatabaseAdmissionError;

const POLL_INTERVAL: Duration = Duration::from_secs(30);

pub(crate) struct SharingWorker {
    store: SharingStore,
    http: Client,
}

#[derive(Debug, Error)]
pub(crate) enum SharingWorkerError {
    #[error("the sharing HTTP client could not be configured")]
    ClientConfiguration,
    #[error("a teaser delivery transition could not be confirmed")]
    Mutation(#[from] SharingMutationError),
}

/// Enabled settings whose credentials the provider has not refused.
enum Sender {
    Substack(SubstackSettings),
}

impl SharingWorker {
    pub(crate) fn new(store: SharingStore) -> Result<Self, SharingWorkerError> {
        Ok(Self {
            store,
            http: client(true).map_err(|_| SharingWorkerError::ClientConfiguration)?,
        })
    }

    pub(crate) async fn run(
        self,
        cancellation: CancellationToken,
    ) -> Result<(), SharingWorkerError> {
        let interrupted = self.store.fail_interrupted().await?;
        if interrupted != 0 {
            tracing::warn!(
                interrupted,
                "teaser deliveries interrupted by the previous shutdown were marked failed"
            );
        }
        let mut ticks = tokio::time::interval(POLL_INTERVAL);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Ok(()),
                _ = ticks.tick() => {}
            }
            // A started delivery always records its outcome before shutdown wins.
            for channel in Channel::ALL {
                self.deliver_next(channel).await?;
            }
        }
    }

    /// Unreadable settings or teasers skip this pass; they never stop the site.
    async fn deliver_next(&self, channel: Channel) -> Result<(), SharingWorkerError> {
        let loaded = async {
            let Some((sender, settings_version)) = self.sender(channel).await? else {
                return Ok(None);
            };
            let due = self.store.due(channel, OffsetDateTime::now_utc()).await?;
            Ok::<_, SharingLoadError>(due.map(|due| (sender, settings_version, due)))
        }
        .await;
        let (sender, settings_version, due) = match loaded {
            Ok(Some(loaded)) => loaded,
            Ok(None) => return Ok(()),
            Err(error) => {
                tracing::error!(channel = channel.as_str(), %error, "sharing state could not be read");
                return Ok(());
            }
        };
        let post_id = due.teaser.view().post_id.clone();
        match self
            .store
            .claim(ClaimDelivery {
                post_id: post_id.clone(),
                channel,
                settings_version,
            })
            .await
        {
            Ok(()) => {}
            // Nothing was claimed: the queue is busy or an Owner just changed something.
            Err(
                SharingMutationError::Admission(DatabaseAdmissionError::QueueFull)
                | SharingMutationError::Command(
                    SharingCommandError::StaleVersion | SharingCommandError::StateConflict,
                ),
            ) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let outcome = self.send(&sender, &due).await;
        tracing::info!(
            channel = channel.as_str(),
            post_id = %post_id,
            outcome = outcome_label(&outcome),
            "teaser delivery finished"
        );
        self.store
            .finish(FinishDelivery {
                post_id,
                channel,
                settings_version,
                outcome,
            })
            .await?;
        Ok(())
    }

    async fn sender(&self, channel: Channel) -> Result<Option<(Sender, u64)>, SharingLoadError> {
        Ok(match channel {
            Channel::Substack => self
                .store
                .substack()
                .await?
                .filter(|stored| {
                    stored.settings.mode == ChannelMode::Enabled && !stored.credentials_rejected
                })
                .map(|stored| (Sender::Substack(stored.settings), stored.version)),
        })
    }

    async fn send(&self, sender: &Sender, due: &DueDelivery) -> DeliveryOutcome {
        // Validated settings always form a request; anything else is a defect.
        let unprepared = DeliveryOutcome::Failed(DeliveryFailure::UnexpectedResponse);
        match sender {
            Sender::Substack(settings) => match SubstackClient::new(self.http.clone(), settings) {
                Ok(client) => client.deliver(&due.teaser, due.draft).await,
                Err(_) => unprepared,
            },
        }
    }
}

const fn outcome_label(outcome: &DeliveryOutcome) -> &'static str {
    match outcome {
        DeliveryOutcome::Posted { .. } => "posted",
        DeliveryOutcome::Retry { .. } => "retry",
        DeliveryOutcome::Failed(_) => "failed",
        DeliveryOutcome::CredentialsRejected { .. } => "credentials_rejected",
    }
}
