use thiserror::Error;
use tokio::sync::oneshot;

use crate::domain::publication::store::{
    BeginPublishNow, BeginPublishNowResult, CreateTargetJob, CreateTargetJobResult,
    FinishPublication, FinishPublicationResult, InstallStartupSnapshot,
    InstallStartupSnapshotResult, PublicationStore,
};

/// The server-facing database capability.
///
/// Read methods use the private query-only pool. Mutation methods admit typed
/// commands to the sole writer task. This type never exposes a SQLx connection.
#[derive(Clone)]
pub(crate) struct DatabaseStore {
    pub(crate) publications: PublicationStore,
}

impl DatabaseStore {
    pub(super) const fn new(publications: PublicationStore) -> Self {
        Self { publications }
    }
}

pub(crate) enum Mutation {
    InstallStartupSnapshot {
        command: InstallStartupSnapshot,
        respond_to: oneshot::Sender<InstallStartupSnapshotResult>,
    },
    CreateTargetJob {
        command: CreateTargetJob,
        respond_to: oneshot::Sender<CreateTargetJobResult>,
    },
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the publication coordinator is composed now; the admin command lands next"
        )
    )]
    BeginPublishNow {
        command: BeginPublishNow,
        respond_to: oneshot::Sender<BeginPublishNowResult>,
    },
    FinishPublication {
        command: FinishPublication,
        respond_to: oneshot::Sender<FinishPublicationResult>,
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
