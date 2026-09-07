//! One sender owns one live campaign lease. Each request consumes a fresh,
//! committed recipient attempt; cancellation never drops an in-flight result.

use std::{sync::Arc, time::Duration};

use markdown_compiler::PublicationBaseUrl;
use thiserror::Error;
use time::OffsetDateTime;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    campaign::{Campaign, CampaignFence, CampaignId, CampaignState},
    config::{SesMailConfiguration, SubscriptionMode, SubscriptionPolicy},
    control::{ControlClaims, ControlTokenError},
    controls::MailControls,
    message::{Confirmation, MessagePreparationError, Newsletter, unsubscribe_link},
    ses::{EmailMessage, SendOutcome, SesClient, SesError},
    store::{
        CampaignCommandError, CampaignLoadError, CampaignMutationError, CampaignStore,
        ClaimCampaign, FinishCampaign, FinishIntent, RenewCampaignClaim,
    },
    subscriber::{
        AdmitCampaignRecipient, ClaimConfirmation, ConfirmationHandle, DeliveryAdmission,
        DeliveryAttempt, FinishAttempt, SubmissionOutcome, SubscriberCommandError,
        SubscriberDigest,
        store::{SubscriberLoadError, SubscriberMutationError, SubscriberStore},
    },
};
use crate::database::store::DatabaseAdmissionError;

const LEASE_SECONDS: u32 = 300;
const RENEW_BEFORE_SECONDS: i64 = 120;

pub(crate) struct MailDispatcher {
    campaigns: CampaignStore,
    subscribers: SubscriberStore,
    client: SesClient,
    controls: Arc<MailControls>,
    origin: PublicationBaseUrl,
    configuration_binding: [u8; 32],
    policy: SubscriptionPolicy,
    interval: Duration,
    claim: Option<(CampaignId, CampaignFence)>,
}

pub(super) struct DispatchResources {
    pub campaigns: CampaignStore,
    pub subscribers: SubscriberStore,
    pub client: SesClient,
    pub controls: Arc<MailControls>,
    pub origin: PublicationBaseUrl,
    pub configuration_binding: [u8; 32],
    pub configuration: SesMailConfiguration,
    pub policy: SubscriptionPolicy,
}

impl MailDispatcher {
    pub(super) fn new(resources: DispatchResources) -> Self {
        Self {
            campaigns: resources.campaigns,
            subscribers: resources.subscribers,
            client: resources.client,
            controls: resources.controls,
            origin: resources.origin,
            configuration_binding: resources.configuration_binding,
            interval: resources.configuration.view().send_interval,
            policy: resources.policy,
            claim: None,
        }
    }

    pub(crate) async fn run(
        mut self,
        cancellation: CancellationToken,
    ) -> Result<(), DispatchError> {
        let mut ticks = tokio::time::interval(self.interval);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Ok(()),
                _ = ticks.tick() => {}
            }
            if let Err(error) = self.tick(&cancellation).await
                && !error.deferred()
            {
                return Err(error);
            }
        }
    }

    async fn tick(&mut self, cancellation: &CancellationToken) -> Result<(), DispatchError> {
        let campaign = self.current_campaign().await?;
        if self.policy.view().mode == SubscriptionMode::Paused || cancellation.is_cancelled() {
            return Ok(());
        }
        if self.confirmation(cancellation).await? {
            return Ok(());
        }
        if let Some(campaign) = campaign {
            self.campaign_recipient(campaign, cancellation).await?;
        }
        Ok(())
    }

    /// Refresh the durable state before considering recipients. Owner cancellation
    /// is processed even when feedback or configured capture is paused.
    async fn current_campaign(&mut self) -> Result<Option<Campaign>, DispatchError> {
        let Some((id, fence)) = self.claim else {
            if self.policy.view().mode == SubscriptionMode::Paused {
                return Ok(None);
            }
            let Some(active) = self.campaigns.active_campaign().await? else {
                return Ok(None);
            };
            match active.state {
                CampaignState::Queued { .. } => {}
                CampaignState::Claimed { lease, .. } | CampaignState::Cancelling { lease, .. }
                    if lease.expires_at <= OffsetDateTime::now_utc() => {}
                CampaignState::Draft
                | CampaignState::Claimed { .. }
                | CampaignState::Cancelling { .. }
                | CampaignState::Completed { .. }
                | CampaignState::Cancelled { .. }
                | CampaignState::Unknown { .. }
                | CampaignState::Quarantined { .. } => return Ok(None),
            }
            let campaign = self
                .campaigns
                .claim(ClaimCampaign {
                    configuration_binding: self.configuration_binding,
                    lease_seconds: LEASE_SECONDS,
                    now: OffsetDateTime::now_utc(),
                })
                .await?;
            if let Some(Campaign {
                campaign_id,
                state: CampaignState::Claimed { lease, .. },
                ..
            }) = &campaign
            {
                self.claim = Some((*campaign_id, lease.fence));
            }
            return Ok(campaign);
        };
        let Some(campaign) = self.campaigns.campaign(id).await? else {
            self.claim = None;
            return Ok(None);
        };
        match &campaign.state {
            CampaignState::Cancelling { lease, .. } if lease.fence == fence => {
                self.finish(id, fence, FinishIntent::Cancel).await?;
                Ok(None)
            }
            CampaignState::Claimed { lease, .. } if lease.fence == fence => {
                let now = OffsetDateTime::now_utc();
                if lease.expires_at - now <= time::Duration::seconds(RENEW_BEFORE_SECONDS) {
                    let renewed = self
                        .campaigns
                        .renew_claim(RenewCampaignClaim {
                            campaign_id: id,
                            fence,
                            configuration_binding: self.configuration_binding,
                            expires_at: now + time::Duration::seconds(i64::from(LEASE_SECONDS)),
                        })
                        .await;
                    match renewed {
                        Ok(campaign) => Ok(Some(campaign)),
                        Err(error) => {
                            self.retain_cancelling_claim(id, fence).await?;
                            Err(error.into())
                        }
                    }
                } else {
                    Ok(Some(campaign))
                }
            }
            CampaignState::Draft
            | CampaignState::Queued { .. }
            | CampaignState::Claimed { .. }
            | CampaignState::Cancelling { .. }
            | CampaignState::Completed { .. }
            | CampaignState::Cancelled { .. }
            | CampaignState::Unknown { .. }
            | CampaignState::Quarantined { .. } => {
                self.claim = None;
                Ok(None)
            }
        }
    }

    /// A cancellation can win either renewal or admission. Preserve custody
    /// only when the same durable fence still needs its cancellation drained.
    async fn retain_cancelling_claim(
        &mut self,
        id: CampaignId,
        fence: CampaignFence,
    ) -> Result<(), DispatchError> {
        let current = self.campaigns.campaign(id).await?;
        if !current.is_some_and(|campaign| {
            matches!(campaign.state,
                CampaignState::Cancelling { lease, .. } if lease.fence == fence)
        }) {
            self.claim = None;
        }
        Ok(())
    }

    async fn confirmation(&self, cancellation: &CancellationToken) -> Result<bool, DispatchError> {
        let Some(handle) = self
            .subscribers
            .queued_confirmations(self.configuration_binding, 1)
            .await?
            .pop()
        else {
            return Ok(false);
        };
        if cancellation.is_cancelled() {
            return Ok(true);
        }
        let nonce = Uuid::new_v4();
        let expires_at = handle.pending_expires_at;
        let now = OffsetDateTime::now_utc();
        if expires_at <= now {
            return Ok(false);
        }
        let body = self.prepare_confirmation(&handle, nonce, now)?;
        let admission = self
            .subscribers
            .claim_confirmation(ClaimConfirmation {
                attempt_id: handle.attempt_id,
                nonce_digest: SubscriberDigest::from_bytes(MailControls::confirmation_digest(
                    &nonce,
                )),
                expires_at,
                configuration_binding: self.configuration_binding,
            })
            .await?;
        match admission {
            DeliveryAdmission::Ready(permit) => {
                let permit = permit.into_attempt();
                if permit.enrollment != handle.enrollment || permit.generation != handle.generation
                {
                    return Err(DispatchError::PermitMismatch);
                }
                let result = self
                    .submit(
                        &permit,
                        body.as_message(&permit.address, permit.mail_epoch, permit.attempt_id),
                        cancellation,
                    )
                    .await;
                self.finish_attempt(permit, result).await?;
                Ok(true)
            }
            DeliveryAdmission::AlreadyRecorded(outcome) => {
                tracing::debug!(?outcome, "confirmation admission already recorded");
                Ok(false)
            }
            DeliveryAdmission::Unavailable | DeliveryAdmission::Deferred => Ok(false),
        }
    }

    fn prepare_confirmation(
        &self,
        handle: &ConfirmationHandle,
        nonce: Uuid,
        now: OffsetDateTime,
    ) -> Result<Confirmation, DispatchError> {
        let expires_at = handle.pending_expires_at;
        let confirm = self.controls.issue(
            ControlClaims::Confirm {
                enrollment: handle.enrollment,
                generation: handle.generation,
                confirmation_nonce: nonce,
                expires_at,
            },
            now,
        )?;
        let manage = self.controls.issue(
            ControlClaims::Manage {
                enrollment: handle.enrollment,
                generation: handle.generation,
            },
            now,
        )?;
        Ok(Confirmation::render(
            &self.origin,
            &confirm,
            unsubscribe_link(&self.origin, &manage)?,
            &self.policy,
        )?)
    }

    async fn campaign_recipient(
        &mut self,
        campaign: Campaign,
        cancellation: &CancellationToken,
    ) -> Result<(), DispatchError> {
        let Some((id, fence)) = self.claim else {
            return Ok(());
        };
        let Some(handle) = self.subscribers.recipients(&campaign, None, 1).await?.pop() else {
            return self.finish(id, fence, FinishIntent::Complete).await;
        };
        let manage = self.controls.issue(
            ControlClaims::Manage {
                enrollment: handle.enrollment,
                generation: handle.generation,
            },
            OffsetDateTime::now_utc(),
        )?;
        let body = Newsletter::render(
            &campaign.content,
            unsubscribe_link(&self.origin, &manage)?,
            &self.policy,
        )?;
        if cancellation.is_cancelled() {
            return Ok(());
        }
        let admission = self
            .subscribers
            .admit_campaign_recipient(AdmitCampaignRecipient {
                campaign_id: id,
                campaign_fence: fence,
                enrollment: handle.enrollment,
                generation: handle.generation,
                attempt_id: Uuid::new_v4(),
                configuration_binding: self.configuration_binding,
            })
            .await;
        if matches!(
            admission,
            Err(SubscriberMutationError::Command(
                SubscriberCommandError::CampaignUnavailable
            ))
        ) {
            // A cancellation can win after the read above. Keep that same
            // fence so the next tick drains cancellation without waiting for
            // lease expiry. Revoked Owner authority must not keep renewing.
            self.retain_cancelling_claim(id, fence).await?;
        }
        match admission? {
            DeliveryAdmission::Ready(permit) => {
                let permit = permit.into_attempt();
                if permit.enrollment != handle.enrollment || permit.generation != handle.generation
                {
                    return Err(DispatchError::PermitMismatch);
                }
                let result = self
                    .submit(
                        &permit,
                        body.as_message(
                            &permit.address,
                            permit.mail_epoch,
                            id.0,
                            permit.attempt_id,
                        ),
                        cancellation,
                    )
                    .await;
                self.finish_attempt(permit, result).await?;
            }
            DeliveryAdmission::AlreadyRecorded(outcome) => {
                tracing::debug!(?outcome, "campaign admission already recorded");
            }
            DeliveryAdmission::Unavailable | DeliveryAdmission::Deferred => {}
        }
        Ok(())
    }

    async fn submit(
        &self,
        permit: &DeliveryAttempt,
        message: EmailMessage<'_>,
        cancellation: &CancellationToken,
    ) -> SubmissionOutcome {
        // A suspended process must not start an old admitted request after its
        // short claim window. Once transmission starts, its result still drains.
        let age = OffsetDateTime::now_utc() - permit.admitted_at;
        if cancellation.is_cancelled()
            || age < time::Duration::ZERO
            || age >= time::Duration::seconds(i64::from(LEASE_SECONDS))
            || message.campaign_id != permit.campaign_id.map(|id| id.0)
        {
            return SubmissionOutcome::Rejected;
        }
        // Once the request starts, drain its bounded response and record the
        // outcome even during shutdown. Transport uncertainty is never retried.
        match self.client.send(&message).await {
            Ok(SendOutcome::Accepted(id)) => SubmissionOutcome::Accepted(id),
            Ok(SendOutcome::Rejected(_) | SendOutcome::Retryable(_)) => SubmissionOutcome::Rejected,
            Ok(SendOutcome::Unknown) | Err(SesError::Unknown | SesError::InvalidResponse) => {
                SubmissionOutcome::Unknown
            }
            Err(
                SesError::Input(_)
                | SesError::ClientConfiguration
                | SesError::Preparation
                | SesError::Rejected(_),
            ) => SubmissionOutcome::Rejected,
        }
    }

    async fn finish_attempt(
        &self,
        permit: DeliveryAttempt,
        outcome: SubmissionOutcome,
    ) -> Result<(), DispatchError> {
        self.subscribers
            .finish_attempt(FinishAttempt {
                mail_epoch: permit.mail_epoch,
                attempt_id: permit.attempt_id,
                attempt_fence: permit.attempt_fence,
                outcome,
            })
            .await?;
        Ok(())
    }

    async fn finish(
        &mut self,
        id: CampaignId,
        fence: CampaignFence,
        intent: FinishIntent,
    ) -> Result<(), DispatchError> {
        self.campaigns
            .finish(FinishCampaign {
                campaign_id: id,
                fence,
                intent,
                now: OffsetDateTime::now_utc(),
            })
            .await?;
        self.claim = None;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub(crate) enum DispatchError {
    #[error("the committed recipient attempt differs from its prepared controls")]
    PermitMismatch,
    #[error("mail campaign state could not be read")]
    CampaignLoad(#[from] CampaignLoadError),
    #[error("mail subscriber state could not be read")]
    SubscriberLoad(#[from] SubscriberLoadError),
    #[error("mail campaign transition failed")]
    CampaignMutation(#[from] CampaignMutationError),
    #[error("mail subscriber transition failed")]
    SubscriberMutation(#[from] SubscriberMutationError),
    #[error("mail control could not be issued")]
    Control(#[from] ControlTokenError),
    #[error("mail message could not be prepared")]
    Message(#[from] MessagePreparationError),
}

impl DispatchError {
    fn deferred(&self) -> bool {
        match self {
            Self::CampaignMutation(
                CampaignMutationError::Admission(DatabaseAdmissionError::QueueFull)
                | CampaignMutationError::Command(
                    CampaignCommandError::ConfigurationChanged
                    | CampaignCommandError::SendingUnavailable
                    | CampaignCommandError::StaleClaim
                    | CampaignCommandError::InvalidTransition,
                ),
            )
            | Self::SubscriberMutation(SubscriberMutationError::Command(
                SubscriberCommandError::Paused
                | SubscriberCommandError::ConfigurationChanged
                | SubscriberCommandError::CampaignUnavailable,
            )) => true,
            // Never continue after a result failed to commit, including a full
            // writer queue. Startup will quarantine the unresolved attempt.
            Self::PermitMismatch
            | Self::CampaignLoad(_)
            | Self::SubscriberLoad(_)
            | Self::CampaignMutation(_)
            | Self::SubscriberMutation(_)
            | Self::Control(_)
            | Self::Message(_) => false,
        }
    }
}
