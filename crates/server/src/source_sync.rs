//! Durable coordination shared by startup, polling, and operator source syncs.

use std::{sync::Arc, time::Duration};

use maincopy_shared::source::{
    BeginSourceSyncResponse, ListSourceSyncsResponse, SourceStatusResponse, SourceSyncAdmission,
    SourceSyncFailureCode, SourceSyncId, SourceSyncOutcome, SourceSyncResource, SourceSyncStage,
};
use markdown_compiler::{
    ContentCandidateStore, ContentCandidateStoreError, ContentTreeDigest, DiscoveredContentTree,
};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::{
    sync::{Mutex, Notify},
    task::JoinError,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    content_sync::{
        ContentCandidatePreparationError, PreparedContentCandidate, prepare_immutable_candidate,
    },
    database::store::DatabaseMutationError,
    domain::{
        auth::store::MutationAuditContext,
        publication::{
            SourceCommit,
            activation::{
                ContentReloadError, PublicationCoordinatorHandle, observed_post_revisions,
            },
        },
        source::store::{
            AdvanceSourceSync, ApplyManagedSourceCatalog, BeginSourceSync, BeginSourceSyncResult,
            FinishSourceSync, InstalledSource, SourceLoadError, SourceStore, SourceSyncCompletion,
            SourceSyncProgress, SourceSyncRequest, StoredSourceConfiguration, StoredSourceSync,
        },
    },
    git_sync::{GitSync, GitSyncError, GitSyncOutcome},
    render::ContentCompiler,
};

#[derive(Clone)]
pub(crate) struct SourceSyncHandle {
    store: SourceStore,
    runtime: SourceRuntime,
}

/// Selects source behavior at the application composition boundary.
pub(crate) enum SourceRuntimeMode {
    ExternalCheckout,
    ManagedGit {
        configuration: StoredSourceConfiguration,
        cancellation: CancellationToken,
    },
}

#[derive(Clone)]
enum SourceRuntime {
    ExternalCheckout,
    ManagedGit(Arc<ManagedSourceControl>),
}

struct ManagedSourceControl {
    configuration: StoredSourceConfiguration,
    cancellation: CancellationToken,
    admission: Mutex<()>,
    wakeup: Notify,
}

impl SourceSyncHandle {
    pub(crate) fn new(store: SourceStore, mode: SourceRuntimeMode) -> Self {
        let runtime = match mode {
            SourceRuntimeMode::ExternalCheckout => SourceRuntime::ExternalCheckout,
            SourceRuntimeMode::ManagedGit {
                configuration,
                cancellation,
            } => SourceRuntime::ManagedGit(Arc::new(ManagedSourceControl {
                configuration,
                cancellation,
                admission: Mutex::new(()),
                wakeup: Notify::new(),
            })),
        };
        Self { store, runtime }
    }

    pub(crate) async fn status(&self) -> Result<SourceStatusResponse, SourceControlError> {
        match &self.runtime {
            SourceRuntime::ExternalCheckout => {
                return Ok(SourceStatusResponse::ExternalCheckout);
            }
            SourceRuntime::ManagedGit(_) => {}
        }
        let status = self.store.status().await?;
        let configuration = status
            .configuration
            .ok_or(SourceControlError::ConfigurationUnavailable)?;
        Ok(SourceStatusResponse::ManagedGit {
            configuration: Box::new(configuration.configuration),
            installed_commit: status
                .installation
                .as_ref()
                .map(|installed| installed.source_commit.to_string().into_boxed_str()),
            content_digest: status
                .installation
                .as_ref()
                .map(|installed| installed.content_digest.to_string().into_boxed_str()),
            active_sync: status.active_sync.map(source_sync_resource).map(Box::new),
            latest_sync: status.latest_sync.map(source_sync_resource).map(Box::new),
            next_poll_at: configuration.next_poll_at,
        })
    }

    pub(crate) async fn begin_manual(
        &self,
        audit: MutationAuditContext,
    ) -> Result<BeginSourceSyncResponse, SourceControlError> {
        let managed = match &self.runtime {
            SourceRuntime::ExternalCheckout => return Err(SourceControlError::Unsupported),
            SourceRuntime::ManagedGit(managed) => managed,
        };
        let _admission = managed.admission.lock().await;
        if managed.cancellation.is_cancelled() {
            return Err(SourceControlError::ShuttingDown);
        }
        let BeginSourceSyncResult { admission, sync } = self
            .store
            .begin_sync(BeginSourceSync {
                proposed_source_sync_id: SourceSyncId::from_uuid(Uuid::new_v4()),
                expected_configuration_version: managed.configuration.configuration.version,
                requested_at: OffsetDateTime::now_utc(),
                request: SourceSyncRequest::Manual { audit },
            })
            .await?;
        if sync.outcome.is_none() {
            managed.wakeup.notify_one();
        }
        Ok(BeginSourceSyncResponse {
            admission,
            sync: source_sync_resource(sync),
        })
    }

    pub(crate) async fn sync(
        &self,
        source_sync_id: SourceSyncId,
    ) -> Result<Option<SourceSyncResource>, SourceControlError> {
        Ok(self
            .store
            .sync(source_sync_id)
            .await?
            .map(source_sync_resource))
    }

    pub(crate) async fn list(
        &self,
        after: Option<SourceSyncId>,
        limit: usize,
    ) -> Result<ListSourceSyncsResponse, SourceControlError> {
        let page = self.store.list_syncs(after, limit).await?;
        Ok(ListSourceSyncsResponse {
            syncs: page.syncs.into_iter().map(source_sync_resource).collect(),
            next_cursor: page.next_cursor,
        })
    }
}

fn source_sync_resource(sync: StoredSourceSync) -> SourceSyncResource {
    SourceSyncResource {
        source_sync_id: sync.source_sync_id,
        configuration_version: sync.configuration_version,
        request_origin: sync.request_origin,
        stage: sync.stage,
        outcome: sync.outcome,
        source_commit: sync
            .source_commit
            .map(|commit| commit.to_string().into_boxed_str()),
        content_digest: sync
            .content_digest
            .map(|digest| digest.to_string().into_boxed_str()),
        failure_code: sync.failure_code,
        version: sync.version,
        requested_at: sync.requested_at,
        updated_at: sync.updated_at,
        finished_at: sync.finished_at,
    }
}

#[derive(Debug, Error)]
pub(crate) enum SourceControlError {
    #[error("managed source synchronization is disabled")]
    Unsupported,
    #[error("managed source synchronization is shutting down")]
    ShuttingDown,
    #[error("managed source configuration is unavailable")]
    ConfigurationUnavailable,
    #[error("managed source state could not be loaded")]
    Load(#[from] SourceLoadError),
    #[error("managed source state could not be changed")]
    Mutation(#[from] DatabaseMutationError),
}

pub(crate) fn accepted_status(admission: SourceSyncAdmission) -> axum::http::StatusCode {
    match admission {
        SourceSyncAdmission::Replayed => axum::http::StatusCode::OK,
        SourceSyncAdmission::Created | SourceSyncAdmission::Coalesced => {
            axum::http::StatusCode::ACCEPTED
        }
    }
}

/// Managed-source resources shared by startup and the live polling actor.
pub(crate) struct ManagedSourceEngine {
    git: GitSync,
    candidate_store: ContentCandidateStore,
    compiler: ContentCompiler,
    handle: SourceSyncHandle,
    managed: Arc<ManagedSourceControl>,
    cancellation: CancellationToken,
}

impl ManagedSourceEngine {
    pub(crate) fn new(
        store: SourceStore,
        configuration: StoredSourceConfiguration,
        git: GitSync,
        candidate_store: ContentCandidateStore,
        compiler: ContentCompiler,
        cancellation: CancellationToken,
    ) -> (Self, SourceSyncHandle) {
        let handle = SourceSyncHandle::new(
            store,
            SourceRuntimeMode::ManagedGit {
                configuration,
                cancellation: cancellation.clone(),
            },
        );
        let managed = match &handle.runtime {
            SourceRuntime::ManagedGit(managed) => Arc::clone(managed),
            SourceRuntime::ExternalCheckout => {
                unreachable!("the managed engine installs managed source control")
            }
        };
        let engine = Self {
            git,
            candidate_store,
            compiler,
            handle: handle.clone(),
            managed,
            cancellation,
        };
        (engine, handle)
    }

    /// Reconciles an interrupted operation and obtains the exact startup catalog.
    pub(crate) async fn prepare_startup(
        &self,
    ) -> Result<PreparedContentCandidate, ManagedSourceSyncError> {
        if let Some(interrupted) = self.handle.store.active_sync().await? {
            self.handle
                .store
                .fail_interrupted_sync(
                    interrupted.source_sync_id,
                    interrupted.version,
                    OffsetDateTime::now_utc(),
                )
                .await?;
        }
        let managed = &self.managed;
        let started = self
            .handle
            .store
            .begin_sync(BeginSourceSync {
                proposed_source_sync_id: SourceSyncId::from_uuid(Uuid::new_v4()),
                expected_configuration_version: managed.configuration.configuration.version,
                requested_at: OffsetDateTime::now_utc(),
                request: SourceSyncRequest::Startup,
            })
            .await?;
        let (mut sync, prepared) = self.prepare_operation(started.sync, true).await?;
        match prepared {
            PreparedOperation::NoChange {
                source_commit,
                startup_candidate,
            } => {
                let startup_candidate = startup_candidate.ok_or(
                    ManagedSourceSyncError::Invariant("startup no-change candidate was not loaded"),
                )?;
                self.handle
                    .store
                    .finish_sync(FinishSourceSync {
                        source_sync_id: sync.source_sync_id,
                        expected_version: sync.version,
                        completion: SourceSyncCompletion::NoChange { source_commit },
                        completed_at: OffsetDateTime::now_utc(),
                    })
                    .await?;
                Ok(startup_candidate)
            }
            PreparedOperation::Candidate(candidate) => {
                sync = self
                    .advance(
                        &sync,
                        SourceSyncProgress::Reloading {
                            source_commit: candidate.source_commit.clone().ok_or(
                                ManagedSourceSyncError::Invariant(
                                    "managed candidate has no source commit",
                                ),
                            )?,
                            content_digest: candidate.content_digest.clone(),
                        },
                    )
                    .await?;
                let source_commit =
                    candidate
                        .source_commit
                        .clone()
                        .ok_or(ManagedSourceSyncError::Invariant(
                            "managed candidate has no source commit",
                        ))?;
                self.handle
                    .store
                    .apply_catalog(ApplyManagedSourceCatalog {
                        source_sync_id: sync.source_sync_id,
                        expected_sync_version: sync.version,
                        source_commit,
                        content_digest: candidate.content_digest.clone(),
                        observed_posts: observed_post_revisions(&candidate.catalog),
                        completed_at: OffsetDateTime::now_utc(),
                    })
                    .await?;
                Ok(candidate)
            }
        }
    }

    pub(crate) fn into_live(self, publications: PublicationCoordinatorHandle) -> ManagedSourceSync {
        ManagedSourceSync {
            engine: self,
            publications,
        }
    }

    async fn begin_poll(&self) -> Result<(), ManagedSourceSyncError> {
        let managed = &self.managed;
        self.handle
            .store
            .begin_sync(BeginSourceSync {
                proposed_source_sync_id: SourceSyncId::from_uuid(Uuid::new_v4()),
                expected_configuration_version: managed.configuration.configuration.version,
                requested_at: OffsetDateTime::now_utc(),
                request: SourceSyncRequest::Poll,
            })
            .await?;
        Ok(())
    }

    async fn prepare_operation(
        &self,
        sync: StoredSourceSync,
        load_startup_candidate: bool,
    ) -> Result<(StoredSourceSync, PreparedOperation), ManagedSourceSyncError> {
        if sync.outcome.is_some() {
            return Err(ManagedSourceSyncError::Invariant(
                "a terminal source operation cannot execute",
            ));
        }
        let configuration = &self.managed.configuration.configuration;
        if sync.configuration_version != configuration.version {
            return self
                .fail_operation(
                    sync,
                    SourceSyncFailureCode::ConfigurationChanged,
                    "source configuration changed before synchronization",
                )
                .await;
        }
        let installation = self.handle.store.installation().await?;
        let installed_commit = installation.as_ref().and_then(|installed| {
            (installed.configuration_version == configuration.version)
                .then_some(&installed.source_commit)
        });
        let (mut sync, outcome) = self.fetch_operation(sync, installed_commit).await?;

        match outcome {
            GitSyncOutcome::NoChange { source_commit } => {
                self.prepare_no_change(sync, source_commit, installation, load_startup_candidate)
                    .await
            }
            GitSyncOutcome::Candidate(candidate) => {
                let prepared = self
                    .compile_tree(&mut sync, candidate.source_commit, candidate.tree)
                    .await?;
                Ok((sync, PreparedOperation::Candidate(prepared)))
            }
        }
    }

    async fn fetch_operation(
        &self,
        sync: StoredSourceSync,
        installed_commit: Option<&SourceCommit>,
    ) -> Result<(StoredSourceSync, GitSyncOutcome), ManagedSourceSyncError> {
        let configuration = &self.managed.configuration.configuration;
        let sync = self.advance(&sync, SourceSyncProgress::Fetching).await?;
        let outcome = match self
            .git
            .synchronize(
                &configuration.remote,
                &configuration.branch,
                &configuration.content_subdirectory,
                &configuration.credential_name,
                installed_commit,
                &self.cancellation,
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(GitSyncError::Cancelled) => {
                self.cancel_operation(sync).await?;
                return Err(ManagedSourceSyncError::Cancelled);
            }
            Err(error) => {
                let code = error.failure_code();
                self.fail_terminal(&sync, code).await?;
                return Err(ManagedSourceSyncError::OperationFailed {
                    code,
                    context: "managed Git synchronization failed",
                });
            }
        };
        let sync = self
            .advance(&sync, SourceSyncProgress::ResolvingCommit)
            .await?;
        Ok((sync, outcome))
    }

    async fn prepare_no_change(
        &self,
        mut sync: StoredSourceSync,
        source_commit: SourceCommit,
        installation: Option<InstalledSource>,
        load_startup_candidate: bool,
    ) -> Result<(StoredSourceSync, PreparedOperation), ManagedSourceSyncError> {
        if !load_startup_candidate {
            return Ok((
                sync,
                PreparedOperation::NoChange {
                    source_commit,
                    startup_candidate: None,
                },
            ));
        }
        let installed = installation.ok_or(ManagedSourceSyncError::Invariant(
            "no-change source has no installed candidate",
        ))?;
        let (effective_commit, tree) = self
            .load_or_recover_candidate(&sync, &source_commit, installed.content_digest)
            .await?;
        if effective_commit != source_commit {
            let prepared = self.compile_tree(&mut sync, effective_commit, tree).await?;
            return Ok((sync, PreparedOperation::Candidate(prepared)));
        }
        let prepared = self.prepare_tree(&sync, effective_commit, tree).await?;
        Ok((
            sync,
            PreparedOperation::NoChange {
                source_commit,
                startup_candidate: Some(prepared),
            },
        ))
    }

    async fn load_or_recover_candidate(
        &self,
        sync: &StoredSourceSync,
        source_commit: &SourceCommit,
        content_digest: ContentTreeDigest,
    ) -> Result<(SourceCommit, DiscoveredContentTree), ManagedSourceSyncError> {
        if let Ok(tree) =
            load_retained_candidate(self.candidate_store.clone(), content_digest).await
        {
            return Ok((source_commit.clone(), tree));
        }
        let configuration = &self.managed.configuration.configuration;
        match self
            .git
            .synchronize(
                &configuration.remote,
                &configuration.branch,
                &configuration.content_subdirectory,
                &configuration.credential_name,
                None,
                &self.cancellation,
            )
            .await
        {
            Ok(GitSyncOutcome::Candidate(recovered)) => {
                Ok((recovered.source_commit, recovered.tree))
            }
            Ok(GitSyncOutcome::NoChange { .. }) => Err(ManagedSourceSyncError::Invariant(
                "forced candidate recovery returned no change",
            )),
            Err(GitSyncError::Cancelled) => {
                self.cancel_operation(sync.clone()).await?;
                Err(ManagedSourceSyncError::Cancelled)
            }
            Err(error) => {
                let code = error.failure_code();
                self.fail_terminal(sync, code).await?;
                Err(ManagedSourceSyncError::OperationFailed {
                    code,
                    context: "retained candidate recovery failed",
                })
            }
        }
    }

    async fn compile_tree(
        &self,
        sync: &mut StoredSourceSync,
        source_commit: SourceCommit,
        tree: DiscoveredContentTree,
    ) -> Result<PreparedContentCandidate, ManagedSourceSyncError> {
        *sync = self
            .advance(
                sync,
                SourceSyncProgress::PreparingCandidate {
                    source_commit: source_commit.clone(),
                },
            )
            .await?;
        *sync = self
            .advance(
                sync,
                SourceSyncProgress::Compiling {
                    source_commit: source_commit.clone(),
                },
            )
            .await?;
        self.prepare_tree(sync, source_commit, tree).await
    }

    async fn prepare_tree(
        &self,
        sync: &StoredSourceSync,
        source_commit: SourceCommit,
        tree: DiscoveredContentTree,
    ) -> Result<PreparedContentCandidate, ManagedSourceSyncError> {
        match prepare_immutable_candidate(
            tree,
            Some(source_commit),
            self.candidate_store.clone(),
            self.compiler.clone(),
        )
        .await
        {
            Ok(prepared) => Ok(prepared),
            Err(error) => {
                let code = candidate_failure_code(&error);
                tracing::warn!(
                    source_sync_id = %sync.source_sync_id,
                    failure_code = code.as_str(),
                    error_class = candidate_error_class(&error),
                    "managed source candidate preparation failed"
                );
                self.fail_terminal(sync, code).await?;
                Err(ManagedSourceSyncError::OperationFailed {
                    code,
                    context: "immutable source candidate preparation failed",
                })
            }
        }
    }

    async fn advance(
        &self,
        sync: &StoredSourceSync,
        progress: SourceSyncProgress,
    ) -> Result<StoredSourceSync, ManagedSourceSyncError> {
        Ok(self
            .handle
            .store
            .advance_sync(AdvanceSourceSync {
                source_sync_id: sync.source_sync_id,
                expected_version: sync.version,
                progress,
                updated_at: OffsetDateTime::now_utc(),
            })
            .await?)
    }

    async fn fail_terminal(
        &self,
        sync: &StoredSourceSync,
        code: SourceSyncFailureCode,
    ) -> Result<(), ManagedSourceSyncError> {
        self.handle
            .store
            .finish_sync(FinishSourceSync {
                source_sync_id: sync.source_sync_id,
                expected_version: sync.version,
                completion: SourceSyncCompletion::Failed { code },
                completed_at: OffsetDateTime::now_utc(),
            })
            .await?;
        Ok(())
    }

    async fn fail_operation(
        &self,
        sync: StoredSourceSync,
        code: SourceSyncFailureCode,
        context: &'static str,
    ) -> Result<(StoredSourceSync, PreparedOperation), ManagedSourceSyncError> {
        self.fail_terminal(&sync, code).await?;
        Err(ManagedSourceSyncError::OperationFailed { code, context })
    }

    async fn cancel_operation(&self, sync: StoredSourceSync) -> Result<(), ManagedSourceSyncError> {
        self.handle
            .store
            .finish_sync(FinishSourceSync {
                source_sync_id: sync.source_sync_id,
                expected_version: sync.version,
                completion: SourceSyncCompletion::Cancelled,
                completed_at: OffsetDateTime::now_utc(),
            })
            .await?;
        Ok(())
    }
}

enum PreparedOperation {
    NoChange {
        source_commit: SourceCommit,
        startup_candidate: Option<PreparedContentCandidate>,
    },
    Candidate(PreparedContentCandidate),
}

/// Periodic and operator-triggered live synchronization task.
pub(crate) struct ManagedSourceSync {
    engine: ManagedSourceEngine,
    publications: PublicationCoordinatorHandle,
}

impl ManagedSourceSync {
    pub(crate) async fn run(self) -> Result<(), ManagedSourceSyncError> {
        let fallback_poll_interval = Duration::from_secs(
            self.engine
                .managed
                .configuration
                .configuration
                .poll_interval_seconds
                .seconds(),
        );

        loop {
            let poll_delay = match self.engine.handle.store.configuration().await? {
                Some(configuration) => configuration
                    .next_poll_at
                    .map(|deadline| delay_until(deadline, OffsetDateTime::now_utc()))
                    .unwrap_or(fallback_poll_interval),
                None => return Err(ManagedSourceSyncError::ConfigurationUnavailable),
            };
            let poll = tokio::time::sleep(poll_delay);
            tokio::pin!(poll);
            tokio::select! {
                biased;
                () = self.engine.cancellation.cancelled() => {
                    let _admission = self.engine.managed.admission.lock().await;
                    if let Some(sync) = self.engine.handle.store.active_sync().await? {
                        self.engine.cancel_operation(sync).await?;
                    }
                    return Ok(());
                }
                () = &mut poll => {
                    self.engine.begin_poll().await?;
                }
                () = self.engine.managed.wakeup.notified() => {}
            }
            self.process_active().await?;
        }
    }

    async fn process_active(&self) -> Result<(), ManagedSourceSyncError> {
        let Some(sync) = self.engine.handle.store.active_sync().await? else {
            return Ok(());
        };
        if sync.stage != SourceSyncStage::Queued {
            self.engine
                .handle
                .store
                .fail_interrupted_sync(sync.source_sync_id, sync.version, OffsetDateTime::now_utc())
                .await?;
            return Ok(());
        }

        let (sync, prepared) = match self.engine.prepare_operation(sync, false).await {
            Ok(prepared) => prepared,
            Err(ManagedSourceSyncError::OperationFailed { code, context }) => {
                tracing::warn!(failure_code = code.as_str(), context, "source sync failed");
                return Ok(());
            }
            Err(ManagedSourceSyncError::Cancelled) if self.engine.cancellation.is_cancelled() => {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        match prepared {
            PreparedOperation::NoChange { source_commit, .. } => {
                self.finish_no_change(sync, source_commit).await?;
            }
            PreparedOperation::Candidate(candidate) => {
                self.apply_candidate(sync, candidate).await?;
            }
        }
        Ok(())
    }

    async fn finish_no_change(
        &self,
        sync: StoredSourceSync,
        source_commit: SourceCommit,
    ) -> Result<(), ManagedSourceSyncError> {
        self.engine
            .handle
            .store
            .finish_sync(FinishSourceSync {
                source_sync_id: sync.source_sync_id,
                expected_version: sync.version,
                completion: SourceSyncCompletion::NoChange { source_commit },
                completed_at: OffsetDateTime::now_utc(),
            })
            .await?;
        Ok(())
    }

    async fn apply_candidate(
        &self,
        sync: StoredSourceSync,
        candidate: PreparedContentCandidate,
    ) -> Result<(), ManagedSourceSyncError> {
        let source_commit =
            candidate
                .source_commit
                .clone()
                .ok_or(ManagedSourceSyncError::Invariant(
                    "managed candidate has no source commit",
                ))?;
        let sync = self
            .engine
            .advance(
                &sync,
                SourceSyncProgress::Reloading {
                    source_commit: source_commit.clone(),
                    content_digest: candidate.content_digest.clone(),
                },
            )
            .await?;
        let result = self
            .publications
            .apply_managed_content_catalog(
                candidate.catalog,
                candidate.content_digest,
                source_commit,
                self.engine.handle.store.clone(),
                sync.source_sync_id,
                sync.version,
            )
            .await;
        let Err(error) = result else {
            return Ok(());
        };
        if self
            .engine
            .handle
            .store
            .sync(sync.source_sync_id)
            .await?
            .is_some_and(|current| current.outcome == Some(SourceSyncOutcome::Applied))
        {
            return Ok(());
        }
        self.engine
            .fail_terminal(&sync, SourceSyncFailureCode::ReloadFailed)
            .await?;
        tracing::warn!(
            failure_code = SourceSyncFailureCode::ReloadFailed.as_str(),
            error_class = reload_error_class(&error),
            "source candidate reload failed"
        );
        Ok(())
    }
}

fn delay_until(deadline: OffsetDateTime, now: OffsetDateTime) -> Duration {
    if deadline <= now {
        return Duration::ZERO;
    }
    (deadline - now).try_into().unwrap_or(Duration::MAX)
}

async fn load_retained_candidate(
    store: ContentCandidateStore,
    digest: ContentTreeDigest,
) -> Result<DiscoveredContentTree, ManagedSourceSyncError> {
    tokio::task::spawn_blocking(move || store.load(&digest))
        .await
        .map_err(ManagedSourceSyncError::Worker)?
        .map_err(ManagedSourceSyncError::RetainedCandidate)
}

fn candidate_failure_code(error: &ContentCandidatePreparationError) -> SourceSyncFailureCode {
    match error {
        ContentCandidatePreparationError::Validate(_) => SourceSyncFailureCode::ValidationFailed,
        ContentCandidatePreparationError::Compile(_) => SourceSyncFailureCode::CompileFailed,
        ContentCandidatePreparationError::Worker(_)
        | ContentCandidatePreparationError::ResolveAssets(_)
        | ContentCandidatePreparationError::Retention(_)
        | ContentCandidatePreparationError::RetainedDigestMismatch { .. } => {
            SourceSyncFailureCode::CandidateFailed
        }
    }
}

const fn candidate_error_class(error: &ContentCandidatePreparationError) -> &'static str {
    match error {
        ContentCandidatePreparationError::Validate(_) => "validation",
        ContentCandidatePreparationError::ResolveAssets(_) => "asset_resolution",
        ContentCandidatePreparationError::Compile(_) => "compile",
        ContentCandidatePreparationError::Worker(_) => "worker",
        ContentCandidatePreparationError::Retention(
            ContentCandidateStoreError::CapacityExceeded(_),
        ) => "candidate_store_capacity",
        ContentCandidatePreparationError::Retention(_) => "candidate_store",
        ContentCandidatePreparationError::RetainedDigestMismatch { .. } => "digest_mismatch",
    }
}

const fn reload_error_class(error: &ContentReloadError) -> &'static str {
    match error {
        ContentReloadError::Coordinator(_) => "coordinator",
        ContentReloadError::Retention(_) => "retention",
        ContentReloadError::Database(_) => "database",
    }
}

#[derive(Debug, Error)]
pub(crate) enum ManagedSourceSyncError {
    #[error("managed source state could not be loaded")]
    Load(#[from] SourceLoadError),
    #[error("managed source state could not be changed")]
    Mutation(#[from] DatabaseMutationError),
    #[error("managed source configuration is unavailable")]
    ConfigurationUnavailable,
    #[error("managed Git synchronization failed")]
    Git(#[from] GitSyncError),
    #[error("managed content candidate preparation failed")]
    Candidate(#[from] ContentCandidatePreparationError),
    #[error("managed content reload failed")]
    Reload(#[from] ContentReloadError),
    #[error("retained managed content could not be loaded")]
    RetainedCandidate(#[source] ContentCandidateStoreError),
    #[error("a retained-candidate worker failed")]
    Worker(#[source] JoinError),
    #[error("managed source synchronization was cancelled")]
    Cancelled,
    #[error("managed source operation failed ({code:?}): {context}")]
    OperationFailed {
        code: SourceSyncFailureCode,
        context: &'static str,
    },
    #[error("managed source invariant failed: {0}")]
    Invariant(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_poll_deadline_maps_past_to_immediate_and_future_to_exact_delay() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();

        assert_eq!(
            delay_until(now - time::Duration::SECOND, now),
            Duration::ZERO
        );
        assert_eq!(
            delay_until(now + time::Duration::seconds(45), now),
            Duration::from_secs(45)
        );
    }

    #[test]
    fn candidate_capacity_failure_has_a_safe_operator_log_class() {
        let error = ContentCandidatePreparationError::Retention(
            ContentCandidateStoreError::CapacityExceeded("test capacity"),
        );

        assert_eq!(candidate_error_class(&error), "candidate_store_capacity");
        assert_eq!(
            candidate_failure_code(&error),
            SourceSyncFailureCode::CandidateFailed
        );
    }
}
