use super::health::DatabaseHealth;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::domain::auth::store::{
    AcceptAgentProof, AgentCredentialMutationResult, AuthCommandError, AuthStore,
    BootstrapIdentity, BootstrapIdentityResult, BrowserSessionMutationResult, CreateBrowserSession,
    CreateLoginChallenge, CreateUser, PutHumanCredential, RecordAdminAuditFailure,
    RegisterAgentCredential, RemoveHumanCredential, ReplaceAgentScopes, ReplaceUserRoles,
    RevokeAgentCredential, RevokeBrowserSession, SetUserStatus, StoredLoginChallenge,
    UserMutationResult,
};
use crate::domain::profile::store::{
    ProfileStore, SetTipRecipient, SetTipRecipientResult, UpdateProfile, UpdateProfileResult,
};
use crate::domain::publication::store::{
    BeginPublishNow, BeginPublishNowResult, BeginScheduledActivation,
    BeginScheduledActivationResult, BlockScheduled, ChangeRelease, FinishPublication,
    FinishPublicationResult, IndexContentCatalog, IndexContentCatalogResult,
    InstallStartupSnapshot, InstallStartupSnapshotResult, PublicationStore, ReleaseChangeReceipt,
    ReleaseCommandError, SchedulePublication, SchedulePublicationResult,
};
use crate::domain::source::store::{
    AdvanceSourceSync, ApplyManagedSourceCatalog, BeginSourceReconfiguration, BeginSourceSync,
    BeginSourceSyncResult, FinishSourceSync, PutSourceConfiguration, SourceStore,
    StoredSourceConfiguration, StoredSourceSync,
};

/// The server-facing database capability.
///
/// Read methods use the private query-only pool. Mutation methods admit typed
/// commands to the sole writer task. This type never exposes a SQLx connection.
#[derive(Clone)]
pub(crate) struct DatabaseStore {
    pub(crate) health: DatabaseHealth,
    pub(crate) auth: AuthStore,
    pub(crate) profiles: ProfileStore,
    pub(crate) publications: PublicationStore,
    pub(crate) source: SourceStore,
}

impl DatabaseStore {
    pub(super) const fn new(
        auth: AuthStore,
        profiles: ProfileStore,
        publications: PublicationStore,
        source: SourceStore,
        health: DatabaseHealth,
    ) -> Self {
        Self {
            health,
            auth,
            profiles,
            publications,
            source,
        }
    }
}

#[derive(Clone)]
pub(crate) struct MutationSender(mpsc::Sender<Mutation>);

impl MutationSender {
    pub(crate) const fn new(sender: mpsc::Sender<Mutation>) -> Self {
        Self(sender)
    }

    /// Rejected admission means no write was accepted. Once admitted, losing
    /// the reply leaves the outcome unknown and does not cancel the write.
    pub(crate) async fn send<Output, CommandError, MutationError>(
        &self,
        mutation: impl FnOnce(oneshot::Sender<Result<Output, CommandError>>) -> Mutation,
        outcome_unknown: CommandError,
    ) -> Result<Output, MutationError>
    where
        MutationError: From<DatabaseAdmissionError> + From<CommandError>,
    {
        let (respond_to, response) = oneshot::channel();
        self.0
            .try_send(mutation(respond_to))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => DatabaseAdmissionError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => DatabaseAdmissionError::WriterClosed,
            })?;
        response
            .await
            .map_err(|_| MutationError::from(outcome_unknown))?
            .map_err(MutationError::from)
    }
}

pub(crate) enum Mutation {
    BlockScheduled {
        command: BlockScheduled,
        respond_to: oneshot::Sender<Result<(), DatabaseCommandError>>,
    },
    ChangeRelease {
        command: ChangeRelease,
        respond_to: oneshot::Sender<Result<ReleaseChangeReceipt, ReleaseCommandError>>,
    },
    RecordAdminAuditFailure {
        command: RecordAdminAuditFailure,
        respond_to: oneshot::Sender<Result<(), AuthCommandError>>,
    },
    BootstrapIdentity {
        command: BootstrapIdentity,
        respond_to: oneshot::Sender<Result<BootstrapIdentityResult, AuthCommandError>>,
    },
    CreateUser {
        command: CreateUser,
        respond_to: oneshot::Sender<Result<UserMutationResult, AuthCommandError>>,
    },
    SetUserStatus {
        command: SetUserStatus,
        respond_to: oneshot::Sender<Result<UserMutationResult, AuthCommandError>>,
    },
    ReplaceUserRoles {
        command: ReplaceUserRoles,
        respond_to: oneshot::Sender<Result<UserMutationResult, AuthCommandError>>,
    },
    PutHumanCredential {
        command: PutHumanCredential,
        respond_to: oneshot::Sender<Result<UserMutationResult, AuthCommandError>>,
    },
    RemoveHumanCredential {
        command: RemoveHumanCredential,
        respond_to: oneshot::Sender<Result<UserMutationResult, AuthCommandError>>,
    },
    CreateLoginChallenge {
        command: CreateLoginChallenge,
        respond_to: oneshot::Sender<Result<StoredLoginChallenge, AuthCommandError>>,
    },
    CreateBrowserSession {
        command: CreateBrowserSession,
        respond_to: oneshot::Sender<Result<BrowserSessionMutationResult, AuthCommandError>>,
    },
    RevokeBrowserSession {
        command: RevokeBrowserSession,
        respond_to: oneshot::Sender<Result<BrowserSessionMutationResult, AuthCommandError>>,
    },
    RegisterAgentCredential {
        command: RegisterAgentCredential,
        respond_to: oneshot::Sender<Result<AgentCredentialMutationResult, AuthCommandError>>,
    },
    ReplaceAgentScopes {
        command: ReplaceAgentScopes,
        respond_to: oneshot::Sender<Result<AgentCredentialMutationResult, AuthCommandError>>,
    },
    RevokeAgentCredential {
        command: RevokeAgentCredential,
        respond_to: oneshot::Sender<Result<AgentCredentialMutationResult, AuthCommandError>>,
    },
    AcceptAgentProof {
        command: AcceptAgentProof,
        respond_to: oneshot::Sender<Result<AgentCredentialMutationResult, AuthCommandError>>,
    },
    UpdateProfile {
        command: UpdateProfile,
        respond_to: oneshot::Sender<UpdateProfileResult>,
    },
    SetTipRecipient {
        command: SetTipRecipient,
        respond_to: oneshot::Sender<SetTipRecipientResult>,
    },
    InstallStartupSnapshot {
        command: InstallStartupSnapshot,
        respond_to: oneshot::Sender<InstallStartupSnapshotResult>,
    },
    IndexContentCatalog {
        command: IndexContentCatalog,
        respond_to: oneshot::Sender<IndexContentCatalogResult>,
    },
    BeginPublishNow {
        command: BeginPublishNow,
        respond_to: oneshot::Sender<BeginPublishNowResult>,
    },
    SchedulePublication {
        command: SchedulePublication,
        respond_to: oneshot::Sender<SchedulePublicationResult>,
    },
    BeginScheduledActivation {
        command: BeginScheduledActivation,
        respond_to: oneshot::Sender<BeginScheduledActivationResult>,
    },
    FinishPublication {
        command: FinishPublication,
        respond_to: oneshot::Sender<FinishPublicationResult>,
    },
    PutSourceConfiguration {
        command: PutSourceConfiguration,
        respond_to: oneshot::Sender<Result<StoredSourceConfiguration, DatabaseCommandError>>,
    },
    BeginSourceReconfiguration {
        command: BeginSourceReconfiguration,
        respond_to: oneshot::Sender<Result<BeginSourceSyncResult, DatabaseCommandError>>,
    },
    BeginSourceSync {
        command: BeginSourceSync,
        respond_to: oneshot::Sender<Result<BeginSourceSyncResult, DatabaseCommandError>>,
    },
    AdvanceSourceSync {
        command: AdvanceSourceSync,
        respond_to: oneshot::Sender<Result<StoredSourceSync, DatabaseCommandError>>,
    },
    ApplyManagedSourceCatalog {
        command: ApplyManagedSourceCatalog,
        respond_to: oneshot::Sender<Result<StoredSourceSync, DatabaseCommandError>>,
    },
    FinishSourceSync {
        command: FinishSourceSync,
        respond_to: oneshot::Sender<Result<StoredSourceSync, DatabaseCommandError>>,
    },
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum DatabaseAdmissionError {
    #[error("the database writer queue is full")]
    QueueFull,
    #[error("the database writer is closed")]
    WriterClosed,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum DatabaseCommandError {
    #[error("the idempotency key is already bound to a different command")]
    IdempotencyConflict,
    #[error("the database command conflicts with durable state")]
    Rejected,
    #[error("the database command contains a value outside the persistence range")]
    InvalidValue,
    #[error("the database command outcome is unknown")]
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum DatabaseMutationError {
    #[error(transparent)]
    Admission(#[from] DatabaseAdmissionError),
    #[error(transparent)]
    Command(#[from] DatabaseCommandError),
}
