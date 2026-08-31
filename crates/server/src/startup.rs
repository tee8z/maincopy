use std::{
    collections::BTreeMap,
    error::Error as StdError,
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use clap::error::ErrorKind;
use markdown_compiler::{
    ContentCandidateStore, ContentCandidateStoreError, ContentTreeDigest, ContentTreeLimits,
    ContentValidationErrors, DiscoveredContentTree, PostId, PostRevisionDigest,
    ResolveContentAssetsError, SiteSnapshotDigest, ValidatedContent, discover_content_tree,
    resolve_content_assets,
};
use time::OffsetDateTime;
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::{
    admin::{
        AdminSecurityState, AdminServer, AdminSessionPolicy, origin::AdminBind,
        runtime_admin_router,
    },
    cli::{ServerInvocation, parse_process_invocation},
    config::{HostConfiguration, HostConfigurationLoader},
    content_sync::ContentSync,
    database::{self, DatabaseStore},
    domain::{
        auth::{Argon2idPolicy, store::ConfiguredLoginProviders},
        profile::TipRecipientProjection,
        publication::{
            PublicLedgerProjection, PublishedPostRevision, SourceCommit,
            activation::{
                PublicationCoordinator, PublicationCoordinatorActor, PublicationCoordinatorHandle,
            },
            scheduler::PublicationScheduler,
            store::{
                InstallStartupSnapshot, ObservedPostRevision, RecoverablePublicationActivation,
            },
        },
    },
    error::{
        ApplicationError, CriticalTaskName, ProcessError, ProcessExit, ShutdownSignal, StartupStage,
    },
    frontend_assets::{FrontendAssetManifest, embedded_manifest},
    observability::{initialize_logging, task_span},
    offline_identity,
    process_lock::{ProcessLock, ProcessLockError},
    render::{
        CatalogBuildError, CatalogRetentionError, ContentCatalog, SiteSnapshot,
        build_site_snapshot, compile_content_catalog, render_site_shell, snapshot_store,
    },
    source_provenance::{SourceCommitDiscovery, discover_source_commit},
    web::{PublicServer, PublicState, Readiness},
};

type ShutdownFuture = Pin<Box<dyn Future<Output = Result<(), ApplicationError>> + Send>>;
type CriticalTaskFuture = Pin<Box<dyn Future<Output = CriticalTaskResult> + Send>>;
type CriticalTaskResult = Result<(), CriticalTaskFailure>;
type CriticalTaskFailure = Box<dyn std::error::Error + Send + Sync>;
const PUBLICATION_COORDINATOR_QUEUE_CAPACITY: usize = 32;

/// Owns Maincopy's process-level resources and lifecycle.
///
/// Configuration validation, dependency construction, listener binding, and
/// task creation belong in [`Application::build`]. Runtime supervision and
/// ordered shutdown belong in [`Application::run_until_stop`].
pub(crate) struct Application {
    _startup: StartupConfiguration,
    _database: DatabaseStore,
    publication_coordinator: PublicationCoordinatorHandle,
    runtime: ApplicationRuntime,
    #[cfg(test)]
    public_addr: std::net::SocketAddr,
    #[cfg(test)]
    admin_addr: std::net::SocketAddr,
}

struct ApplicationRuntime {
    readiness: Readiness,
    cancellation: CancellationToken,
    database_shutdown: CancellationToken,
    shutdown: ShutdownFuture,
    critical_tasks: JoinSet<(CriticalTaskName, CriticalTaskCompletion)>,
    database_writer: Option<JoinHandle<(CriticalTaskName, CriticalTaskCompletion)>>,
}

struct StartupConfiguration {
    _process_lock: ProcessLock,
    _host: HostConfiguration,
    _content_tree: DiscoveredContentTree,
    _validated_content: ValidatedContent,
}

struct CompiledStartupContent {
    catalog: Arc<ContentCatalog>,
    observed_posts: Vec<ObservedPostRevision>,
    source_commit: Option<SourceCommit>,
    content_digest: ContentTreeDigest,
}

struct ServingState {
    readiness: Readiness,
    publication_coordinator: PublicationCoordinatorHandle,
    publication_actor: PublicationCoordinatorActor,
    public_server: PublicServer,
    admin_server: AdminServer,
}

impl StartupConfiguration {
    fn load_with_discovery<Discover>(
        config_path: PathBuf,
        discover: Discover,
    ) -> Result<Self, ProcessError>
    where
        Discover: FnOnce(
            &Path,
            ContentTreeLimits,
        ) -> Result<DiscoveredContentTree, ContentValidationErrors>,
    {
        let host = HostConfigurationLoader::from_process_working_directory()?.load(&config_path)?;
        let host_view = host.view();
        let process_lock = match ProcessLock::acquire(host_view.runtime_root) {
            Ok(process_lock) => process_lock,
            Err(ProcessLockError::AlreadyRunning) => return Err(ProcessError::AlreadyRunning),
            Err(error) => {
                return Err(startup_failure(
                    StartupStage::ProcessLock,
                    "acquire the process lock",
                    error,
                ));
            }
        };
        let content_tree = discover(host_view.content_root, host_view.content_limits)?;
        let validated_content = content_tree.validate()?;

        Ok(Self {
            _process_lock: process_lock,
            _host: host,
            _content_tree: content_tree,
            _validated_content: validated_content,
        })
    }
}

/// Parses the server arguments and runs the daemon to completion.
pub async fn run_until_stop() -> ProcessExit {
    initialize_logging();
    let invocation = match parse_process_invocation() {
        Ok(invocation) => invocation,
        Err(error) => return report_command_error(error),
    };
    let result: Result<(), ProcessError> = async {
        match invocation {
            ServerInvocation::Serve { config_path } => {
                let startup =
                    StartupConfiguration::load_with_discovery(config_path, discover_content_tree)?;
                let application = Application::build(startup).await?;
                application
                    .run_until_stop()
                    .await
                    .map_err(ProcessError::from)
            }
            ServerInvocation::BootstrapIdentity {
                config_path,
                credential,
            } => offline_identity::bootstrap_owner(config_path, credential).await,
        }
    }
    .await;

    match result {
        Ok(()) => ProcessExit::Success,
        Err(error) => {
            let exit = error.exit();
            tracing::error!(
                error = %error,
                category = error.category(),
                exit_code = exit.code(),
                "server process failed"
            );
            exit
        }
    }
}

fn report_command_error(error: clap::Error) -> ProcessExit {
    let exit = match error.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => ProcessExit::Success,
        _ => ProcessExit::Usage,
    };

    if let Err(print_error) = error.print() {
        tracing::error!(
            error = %print_error,
            "failed to print command output"
        );
        return ProcessExit::Internal;
    }

    exit
}

impl Application {
    async fn build(startup: StartupConfiguration) -> Result<Self, ProcessError> {
        let cancellation = CancellationToken::new();
        let host = startup._host.view();
        let content_root = host.content_root.to_path_buf();
        let state_root = host.state_root.to_path_buf();
        let content_limits = host.content_limits;
        let database_queue_capacity = host.database.writer_queue_capacity.get();
        let frontend = embedded_manifest();
        frontend.validate().map_err(|error| {
            startup_failure(
                StartupStage::FrontendAssets,
                "validate embedded frontend assets",
                error,
            )
        })?;
        let shutdown = install_termination_signal()?;
        let candidate_store =
            ContentCandidateStore::open(&state_root, content_limits).map_err(|error| {
                startup_failure(
                    StartupStage::Content,
                    "open the retained content candidate store",
                    error,
                )
            })?;
        candidate_store
            .retain(&startup._content_tree)
            .map_err(|error| {
                startup_failure(
                    StartupStage::Content,
                    "retain the startup content candidate",
                    error,
                )
            })?;
        let compiled = compile_startup_content(&startup, host.content_root)?;
        let active_content_digest = compiled.content_digest.clone();

        let database = match database::bootstrap(host.database).await {
            Ok(database) => database,
            Err(database::DatabaseStartupError::AlreadyOwned) => {
                return Err(ProcessError::AlreadyRunning);
            }
            Err(error) => {
                return Err(startup_failure(
                    StartupStage::Database,
                    "bootstrap the database",
                    error,
                ));
            }
        };
        let (database_store, database_writer) = database.into_store(database_queue_capacity);
        let database_shutdown = CancellationToken::new();
        let writer_shutdown = database_shutdown.clone();
        let database_task = CriticalTask::new(CriticalTaskName::DatabaseWriter, async move {
            database_writer
                .run(writer_shutdown)
                .await
                .map_err(|error| Box::new(error) as CriticalTaskFailure)
        });
        let database_task = spawn_critical_task(database_task);

        let providers = ConfiguredLoginProviders::new(true, true)
            .expect("password and Nostr form a valid login-provider set");
        let security = match AdminSecurityState::new(
            host.admin_origin.clone(),
            database_store.auth.clone(),
            providers,
            AdminSessionPolicy::default(),
            Argon2idPolicy::v1(),
        )
        .await
        {
            Ok(security) => security,
            Err(error) => {
                let error = startup_failure(
                    StartupStage::Identity,
                    "initialize the admin authentication boundary",
                    error,
                );
                return Err(close_writer_after_startup_failure(
                    database_store,
                    database_shutdown,
                    database_task,
                    error,
                )
                .await);
            }
        };

        let serving_state = match prepare_serving_state(
            &database_store,
            compiled,
            frontend,
            host.public_bind,
            host.admin_bind,
            security,
            cancellation.clone(),
            &candidate_store,
        )
        .await
        {
            Ok(setup) => setup,
            Err(error) => {
                return Err(close_writer_after_startup_failure(
                    database_store,
                    database_shutdown,
                    database_task,
                    error,
                )
                .await);
            }
        };
        let ServingState {
            readiness,
            publication_coordinator,
            publication_actor,
            public_server,
            admin_server,
        } = serving_state;
        #[cfg(test)]
        let public_addr = public_server.local_addr;
        #[cfg(test)]
        let admin_addr = admin_server.local_addr;
        let public_cancellation = cancellation.clone();
        let public_task = CriticalTask::new(CriticalTaskName::PublicServer, async move {
            public_server
                .serve(public_cancellation)
                .await
                .map_err(|error| Box::new(error) as CriticalTaskFailure)
        });
        let admin_cancellation = cancellation.clone();
        let admin_task = CriticalTask::new(CriticalTaskName::AdminServer, async move {
            admin_server
                .serve(admin_cancellation)
                .await
                .map_err(|error| Box::new(error) as CriticalTaskFailure)
        });
        let actor_cancellation = cancellation.clone();
        let publication_actor_task =
            CriticalTask::new(CriticalTaskName::PublicationCoordinator, async move {
                publication_actor
                    .run(actor_cancellation)
                    .await
                    .map_err(|error| Box::new(error) as CriticalTaskFailure)
            });
        let content_sync = ContentSync::new(
            content_root,
            content_limits,
            candidate_store,
            active_content_digest,
            publication_coordinator.clone(),
            cancellation.clone(),
        );
        let content_task = CriticalTask::new(CriticalTaskName::ContentSync, async move {
            content_sync
                .run()
                .await
                .map_err(|error| Box::new(error) as CriticalTaskFailure)
        });
        let scheduler_wakeup = publication_coordinator.scheduler_wakeup();
        let scheduler = PublicationScheduler::new(
            database_store.publications.clone(),
            publication_coordinator.clone(),
            scheduler_wakeup,
            cancellation.clone(),
        );
        let scheduler_task = CriticalTask::new(CriticalTaskName::Scheduler, async move {
            scheduler
                .run()
                .await
                .map_err(|error| Box::new(error) as CriticalTaskFailure)
        });

        Ok(Self {
            _startup: startup,
            _database: database_store,
            publication_coordinator,
            runtime: ApplicationRuntime::with_database_writer(
                readiness,
                cancellation,
                database_shutdown,
                shutdown,
                vec![
                    publication_actor_task,
                    public_task,
                    admin_task,
                    content_task,
                    scheduler_task,
                ],
                database_task,
            ),
            #[cfg(test)]
            public_addr,
            #[cfg(test)]
            admin_addr,
        })
    }

    async fn run_until_stop(self) -> Result<(), ApplicationError> {
        let Self {
            _startup: startup,
            _database: database,
            publication_coordinator,
            runtime,
            #[cfg(test)]
                public_addr: _,
            #[cfg(test)]
                admin_addr: _,
        } = self;
        let runtime_result = runtime.run_until_stop().await;
        drop(publication_coordinator);
        drop(database);
        drop(startup);
        runtime_result
    }
}

fn compile_startup_content(
    startup: &StartupConfiguration,
    content_root: &Path,
) -> Result<CompiledStartupContent, ProcessError> {
    let content_digest = startup._content_tree.digest();
    let resolved_assets =
        resolve_content_assets(&startup._content_tree, &startup._validated_content).map_err(
            |error| startup_failure(StartupStage::Content, "resolve content assets", error),
        )?;
    let catalog = Arc::new(
        compile_content_catalog(&startup._validated_content, &resolved_assets).map_err(
            |error| startup_failure(StartupStage::Content, "compile the content catalog", error),
        )?,
    );
    let observed_posts = catalog
        .rendered_posts()
        .map(|post| ObservedPostRevision {
            stable_post_id: post.document.metadata.id.clone(),
            revision_digest: post.revision.clone(),
            publication_status: post.document.metadata.draft,
            slug: post.document.metadata.slug.clone(),
        })
        .collect();
    let source_commit = match discover_source_commit(content_root) {
        SourceCommitDiscovery::Discovered(commit) => Some(commit),
        SourceCommitDiscovery::Unavailable(reason) => {
            tracing::warn!(?reason, "content source commit is unavailable");
            None
        }
    };
    Ok(CompiledStartupContent {
        catalog,
        observed_posts,
        source_commit,
        content_digest,
    })
}

fn compile_retained_catalogs(
    store: &ContentCandidateStore,
) -> Result<BTreeMap<ContentTreeDigest, Arc<ContentCatalog>>, RetainedCatalogError> {
    store
        .load_all()
        .map_err(RetainedCatalogError::Load)?
        .into_iter()
        .map(|candidate| {
            let digest = candidate.digest;
            let content =
                candidate
                    .tree
                    .validate()
                    .map_err(|source| RetainedCatalogError::Validate {
                        digest: digest.clone(),
                        source,
                    })?;
            let assets = resolve_content_assets(&candidate.tree, &content).map_err(|source| {
                RetainedCatalogError::ResolveAssets {
                    digest: digest.clone(),
                    source,
                }
            })?;
            match compile_content_catalog(&content, &assets) {
                Ok(catalog) => Ok((digest, Arc::new(catalog))),
                Err(source) => Err(RetainedCatalogError::Compile { digest, source }),
            }
        })
        .collect()
}

fn hydrate_catalog(
    mut base: ContentCatalog,
    retained: &BTreeMap<ContentTreeDigest, Arc<ContentCatalog>>,
    pins: impl IntoIterator<Item = (PostId, PostRevisionDigest)>,
) -> Result<Arc<ContentCatalog>, RetainedCatalogError> {
    for (post_id, revision) in pins {
        if base.get(&post_id, &revision).is_some() {
            continue;
        }
        let source = retained
            .values()
            .find(|catalog| catalog.get(&post_id, &revision).is_some())
            .ok_or_else(|| RetainedCatalogError::RevisionUnavailable {
                post_id: post_id.clone(),
                revision: revision.clone(),
            })?;
        base.retain_revisions_from(source, std::iter::once((post_id.clone(), revision.clone())))
            .map_err(RetainedCatalogError::Retain)?;
    }
    Ok(Arc::new(base))
}

fn find_retained_public_snapshot(
    retained: &BTreeMap<ContentTreeDigest, Arc<ContentCatalog>>,
    ledger: &PublicLedgerProjection,
    expected: &SiteSnapshotDigest,
    frontend: &'static FrontendAssetManifest,
    tip_recipient: Option<&TipRecipientProjection>,
) -> Result<SiteSnapshot, RetainedCatalogError> {
    let pins = ledger
        .published_posts()
        .map(|published| (published.post_id.clone(), published.revision.clone()))
        .collect::<Vec<_>>();
    for base in retained.values() {
        let catalog = hydrate_catalog(base.as_ref().clone(), retained, pins.clone())?;
        let Ok(shell) = render_site_shell(catalog, frontend, ledger) else {
            continue;
        };
        let shell = shell.bind_tip_recipient(tip_recipient.cloned());
        let Ok(snapshot) = build_site_snapshot(shell, ledger) else {
            continue;
        };
        if &snapshot.digest == expected {
            return Ok(snapshot);
        }
    }
    Err(RetainedCatalogError::PublicSnapshotUnavailable {
        expected: expected.clone(),
    })
}

fn find_retained_activation_catalog(
    retained: &BTreeMap<ContentTreeDigest, Arc<ContentCatalog>>,
    ledger: &PublicLedgerProjection,
    activation: &RecoverablePublicationActivation,
    frontend: &'static FrontendAssetManifest,
    tip_recipient: Option<&TipRecipientProjection>,
) -> Result<Arc<ContentCatalog>, RetainedCatalogError> {
    let view = activation.publication.view();
    let activated_at = view
        .activation_started_at
        .ok_or(RetainedCatalogError::ActivationTimestampMissing)?;
    let candidate_ledger = ledger.with_approved(PublishedPostRevision::new(
        view.stable_post_id.clone(),
        view.pinned_post_digest.clone(),
        activated_at,
    ));
    let pins = candidate_ledger
        .published_posts()
        .map(|published| (published.post_id.clone(), published.revision.clone()))
        .collect::<Vec<_>>();
    for base in retained.values() {
        let catalog = hydrate_catalog(base.as_ref().clone(), retained, pins.clone())?;
        let Ok(shell) = render_site_shell(Arc::clone(&catalog), frontend, &candidate_ledger) else {
            continue;
        };
        let shell = shell.bind_tip_recipient(tip_recipient.cloned());
        let Ok(snapshot) = build_site_snapshot(shell, &candidate_ledger) else {
            continue;
        };
        if snapshot.digest == activation.candidate_site_digest {
            return Ok(catalog);
        }
    }
    Err(RetainedCatalogError::ActivationSnapshotUnavailable {
        expected: activation.candidate_site_digest.clone(),
    })
}

#[derive(Debug, thiserror::Error)]
enum RetainedCatalogError {
    #[error("retained content candidates could not be loaded")]
    Load(#[source] ContentCandidateStoreError),
    #[error("retained content candidate {digest} is invalid")]
    Validate {
        digest: ContentTreeDigest,
        #[source]
        source: ContentValidationErrors,
    },
    #[error("assets for retained content candidate {digest} could not be resolved")]
    ResolveAssets {
        digest: ContentTreeDigest,
        #[source]
        source: ResolveContentAssetsError,
    },
    #[error("retained content candidate {digest} could not be compiled")]
    Compile {
        digest: ContentTreeDigest,
        #[source]
        source: CatalogBuildError,
    },
    #[error("retained revision {revision} for post {post_id} is unavailable")]
    RevisionUnavailable {
        post_id: PostId,
        revision: PostRevisionDigest,
    },
    #[error("a retained revision could not be installed in the recovery catalog")]
    Retain(#[source] CatalogRetentionError),
    #[error("no retained content candidate rebuilds durable site {expected}")]
    PublicSnapshotUnavailable { expected: SiteSnapshotDigest },
    #[error("the activating publication has no activation timestamp")]
    ActivationTimestampMissing,
    #[error("no retained content candidate rebuilds activating site {expected}")]
    ActivationSnapshotUnavailable { expected: SiteSnapshotDigest },
}

#[expect(
    clippy::too_many_arguments,
    reason = "startup composition passes each independently owned runtime resource across this single boundary"
)]
async fn prepare_serving_state(
    database: &DatabaseStore,
    compiled: CompiledStartupContent,
    frontend: &'static FrontendAssetManifest,
    public_bind: std::net::SocketAddr,
    admin_bind: AdminBind,
    security: AdminSecurityState,
    cancellation: CancellationToken,
    candidate_store: &ContentCandidateStore,
) -> Result<ServingState, ProcessError> {
    let tip_recipient = database
        .profiles
        .effective_tip_recipient()
        .await
        .map_err(|error| {
            startup_failure(
                StartupStage::Database,
                "load the active tip recipient profile",
                error,
            )
        })?;
    let mut startup_state = database
        .publications
        .startup_snapshot_state()
        .await
        .map_err(|error| {
            startup_failure(
                StartupStage::Database,
                "load the startup publication ledger",
                error,
            )
        })?;
    let ledger = startup_state.ledger.clone();
    let retained_catalogs = compile_retained_catalogs(candidate_store).map_err(|error| {
        startup_failure(
            StartupStage::Content,
            "compile retained content candidates",
            error,
        )
    })?;
    let preview_pins = ledger
        .published_posts()
        .map(|published| (published.post_id.clone(), published.revision.clone()))
        .chain(startup_state.scheduled.iter().map(|scheduled| {
            let view = scheduled.publication.view();
            (view.stable_post_id.clone(), view.pinned_post_digest.clone())
        }))
        .chain(startup_state.activating.iter().map(|activation| {
            let view = activation.publication.view();
            (view.stable_post_id.clone(), view.pinned_post_digest.clone())
        }))
        .collect::<Vec<_>>();
    let preview_catalog = hydrate_catalog(
        compiled.catalog.as_ref().clone(),
        &retained_catalogs,
        preview_pins,
    )
    .map_err(|error| {
        startup_failure(
            StartupStage::Content,
            "hydrate the private preview catalog",
            error,
        )
    })?;
    let recovery_catalog = startup_state
        .activating
        .first()
        .map(|activation| {
            find_retained_activation_catalog(
                &retained_catalogs,
                &ledger,
                activation,
                frontend,
                tip_recipient.as_ref(),
            )
        })
        .transpose()
        .map_err(|error| {
            startup_failure(
                StartupStage::Content,
                "rebuild the activating publication candidate",
                error,
            )
        })?;
    let snapshot = match startup_state.site.as_ref() {
        Some(expected) => find_retained_public_snapshot(
            &retained_catalogs,
            &ledger,
            &expected.digest,
            frontend,
            tip_recipient.as_ref(),
        )
        .map_err(|error| {
            startup_failure(
                StartupStage::Content,
                "rebuild the approved public site snapshot",
                error,
            )
        })?,
        None => {
            let shell = render_site_shell(Arc::clone(&preview_catalog), frontend, &ledger)
                .map_err(|error| {
                    startup_failure(StartupStage::Content, "render the site shell", error)
                })?
                .bind_tip_recipient(tip_recipient.clone());
            build_site_snapshot(shell, &ledger).map_err(|error| {
                startup_failure(StartupStage::Content, "build the site snapshot", error)
            })?
        }
    };
    if !startup_state.activating.is_empty()
        && startup_state.site.as_ref().map(|site| &site.digest) != Some(&snapshot.digest)
    {
        return Err(startup_failure(
            StartupStage::Content,
            "rebuild the durable pre-activation site snapshot",
            StartupInvariantError::BaseSnapshotMismatch,
        ));
    }
    if let Some(expected) = startup_state.site.as_ref()
        && expected.digest != snapshot.digest
    {
        return Err(startup_failure(
            StartupStage::Content,
            "rebuild the approved public site snapshot",
            StartupInvariantError::PublicSnapshotMismatch,
        ));
    }
    let installed_site = database
        .publications
        .install_startup_snapshot(InstallStartupSnapshot {
            expected: startup_state.site.clone(),
            candidate_digest: snapshot.digest.clone(),
            activated_at: OffsetDateTime::now_utc(),
            source_commit: compiled.source_commit.clone(),
            posts: compiled.observed_posts,
        })
        .await
        .map_err(|error| {
            startup_failure(
                StartupStage::Database,
                "install the startup site snapshot",
                error,
            )
        })?;

    let readiness = Readiness::default();
    let (snapshots, activator) = snapshot_store(snapshot);
    let mut publication_coordinator = PublicationCoordinator {
        catalog: recovery_catalog.unwrap_or_else(|| Arc::clone(&preview_catalog)),
        content_digest: compiled.content_digest,
        candidates: Arc::new(retained_catalogs),
        ledger,
        site: installed_site,
        activator,
        store: database.publications.clone(),
        profiles: database.profiles.clone(),
        tip_recipient,
        frontend,
        source_commit: compiled.source_commit,
        scheduled: startup_state
            .scheduled
            .drain(..)
            .map(|scheduled| (scheduled.publication_id, scheduled))
            .collect(),
        scheduler_wakeup: Arc::new(tokio::sync::Notify::new()),
        readiness: readiness.clone(),
        cancellation: cancellation.clone(),
    };
    if let Some(activation) = startup_state.activating.pop() {
        publication_coordinator
            .recover(activation)
            .await
            .map_err(|error| {
                startup_failure(
                    StartupStage::Database,
                    "recover the activating publication",
                    error,
                )
            })?;
        publication_coordinator.catalog = preview_catalog;
    }
    let (publication_coordinator, publication_actor) =
        publication_coordinator.into_actor(PUBLICATION_COORDINATOR_QUEUE_CAPACITY);
    let protected_admin_router = runtime_admin_router(
        publication_coordinator.clone(),
        security,
        database.profiles.clone(),
    );
    let public_server = PublicServer::bind(
        public_bind,
        PublicState {
            snapshots,
            readiness: readiness.clone(),
        },
    )
    .await
    .map_err(|error| startup_failure(StartupStage::Listeners, "bind the public listener", error))?;
    tracing::info!(bind = %public_server.local_addr, "public listener bound");
    let admin_server = AdminServer::bind(admin_bind, protected_admin_router)
        .await
        .map_err(|error| {
            startup_failure(StartupStage::Listeners, "bind the admin listener", error)
        })?;
    tracing::info!(
        bind = %admin_server.local_addr,
        "authenticated admin backend listener bound"
    );
    Ok(ServingState {
        readiness,
        publication_coordinator,
        publication_actor,
        public_server,
        admin_server,
    })
}

fn startup_failure(
    stage: StartupStage,
    operation: &'static str,
    error: impl StdError + Send + Sync + 'static,
) -> ProcessError {
    tracing::error!(%stage, operation, error = %error, "startup operation failed");
    ApplicationError::Startup {
        stage,
        operation,
        source: Box::new(error),
    }
    .into()
}

#[derive(Debug, thiserror::Error)]
enum StartupInvariantError {
    #[error("the rebuilt base snapshot does not match the durable site head")]
    BaseSnapshotMismatch,
    #[error("the retained public representation does not match the durable site head")]
    PublicSnapshotMismatch,
}

async fn close_writer_after_startup_failure(
    database: DatabaseStore,
    shutdown: CancellationToken,
    writer: JoinHandle<(CriticalTaskName, CriticalTaskCompletion)>,
    process_error: ProcessError,
) -> ProcessError {
    drop(database);
    shutdown.cancel();
    if let Some(error) = drained_task_failure(writer.await) {
        tracing::error!(error = %error, "database writer failed during startup cleanup");
    }
    process_error
}

impl ApplicationRuntime {
    fn with_parts(
        readiness: Readiness,
        cancellation: CancellationToken,
        shutdown: ShutdownFuture,
        critical_tasks: Vec<CriticalTask>,
    ) -> Self {
        let mut supervisor = JoinSet::new();
        for task in critical_tasks {
            let span = task_span(task.name);
            supervisor.spawn(task.instrument(span));
        }

        Self {
            readiness,
            cancellation,
            database_shutdown: CancellationToken::new(),
            shutdown,
            critical_tasks: supervisor,
            database_writer: None,
        }
    }

    fn with_database_writer(
        readiness: Readiness,
        cancellation: CancellationToken,
        database_shutdown: CancellationToken,
        shutdown: ShutdownFuture,
        critical_tasks: Vec<CriticalTask>,
        database_writer: JoinHandle<(CriticalTaskName, CriticalTaskCompletion)>,
    ) -> Self {
        let mut runtime = Self::with_parts(readiness, cancellation, shutdown, critical_tasks);
        runtime.database_shutdown = database_shutdown;
        runtime.database_writer = Some(database_writer);
        runtime
    }

    async fn run_until_stop(mut self) -> Result<(), ApplicationError> {
        self.readiness.mark_ready();

        let failure = if self.critical_tasks.is_empty() && self.database_writer.is_none() {
            self.shutdown.await.err()
        } else {
            tokio::select! {
                biased;
                shutdown = &mut self.shutdown => shutdown.err(),
                completion = self.critical_tasks.join_next(), if !self.critical_tasks.is_empty() => {
                    Some(unexpected_task_failure(completion))
                }
                completion = wait_for_database_writer(&mut self.database_writer) => {
                    self.database_writer.take();
                    Some(unexpected_task_failure(Some(completion)))
                }
            }
        };

        self.readiness.mark_not_ready();
        self.cancellation.cancel();

        let drain_failure = drain_critical_tasks(&mut self.critical_tasks).await;
        self.database_shutdown.cancel();
        let database_failure = drain_database_writer(&mut self.database_writer).await;
        match first_shutdown_failure([failure, drain_failure, database_failure]) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn spawn_critical_task(
    task: CriticalTask,
) -> JoinHandle<(CriticalTaskName, CriticalTaskCompletion)> {
    let span = task_span(task.name);
    tokio::spawn(task.instrument(span))
}

fn first_shutdown_failure(failures: [Option<ApplicationError>; 3]) -> Option<ApplicationError> {
    let mut failures = failures.into_iter().flatten();
    let first = failures.next();
    for failure in failures {
        tracing::error!(error = %failure, "additional failure during shutdown");
    }
    first
}

async fn wait_for_database_writer(
    writer: &mut Option<JoinHandle<(CriticalTaskName, CriticalTaskCompletion)>>,
) -> Result<(CriticalTaskName, CriticalTaskCompletion), JoinError> {
    match writer.as_mut() {
        Some(writer) => writer.await,
        None => std::future::pending().await,
    }
}

async fn drain_database_writer(
    writer: &mut Option<JoinHandle<(CriticalTaskName, CriticalTaskCompletion)>>,
) -> Option<ApplicationError> {
    let writer = writer.take()?;
    drained_task_failure(writer.await)
}

async fn drain_critical_tasks(
    critical_tasks: &mut JoinSet<(CriticalTaskName, CriticalTaskCompletion)>,
) -> Option<ApplicationError> {
    let mut first_failure = None;

    while let Some(completion) = critical_tasks.join_next().await {
        if let Some(failure) = drained_task_failure(completion) {
            if first_failure.is_none() {
                first_failure = Some(failure);
            } else {
                tracing::error!(error = %failure, "additional critical task failure during shutdown");
            }
        }
    }

    first_failure
}

fn unexpected_task_failure(
    completion: Option<Result<(CriticalTaskName, CriticalTaskCompletion), JoinError>>,
) -> ApplicationError {
    match completion {
        Some(Ok((task, completion))) => task_completion_failure(task, completion)
            .unwrap_or(ApplicationError::CriticalTaskExited { task }),
        Some(Err(source)) => ApplicationError::TaskSupervisor { source },
        None => ApplicationError::TaskSupervisorEmpty,
    }
}

fn drained_task_failure(
    completion: Result<(CriticalTaskName, CriticalTaskCompletion), JoinError>,
) -> Option<ApplicationError> {
    match completion {
        Ok((task, completion)) => task_completion_failure(task, completion),
        Err(source) => Some(ApplicationError::TaskSupervisor { source }),
    }
}

fn task_completion_failure(
    task: CriticalTaskName,
    completion: CriticalTaskCompletion,
) -> Option<ApplicationError> {
    match completion {
        CriticalTaskCompletion::Returned(Ok(())) => None,
        CriticalTaskCompletion::Returned(Err(source)) => {
            Some(ApplicationError::CriticalTaskFailed { task, source })
        }
        CriticalTaskCompletion::Panicked(message) => {
            Some(ApplicationError::CriticalTaskPanicked { task, message })
        }
    }
}

struct CriticalTask {
    name: CriticalTaskName,
    future: CriticalTaskFuture,
}

impl CriticalTask {
    fn new<Future>(name: CriticalTaskName, future: Future) -> Self
    where
        Future: std::future::Future<Output = CriticalTaskResult> + Send + 'static,
    {
        Self {
            name,
            future: Box::pin(future),
        }
    }
}

enum CriticalTaskCompletion {
    Returned(CriticalTaskResult),
    Panicked(Box<str>),
}

impl Future for CriticalTask {
    type Output = (CriticalTaskName, CriticalTaskCompletion);

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let task = self.get_mut();
        match catch_unwind(AssertUnwindSafe(|| task.future.as_mut().poll(context))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(result)) => {
                Poll::Ready((task.name, CriticalTaskCompletion::Returned(result)))
            }
            Err(payload) => Poll::Ready((
                task.name,
                CriticalTaskCompletion::Panicked(panic_message(payload.as_ref())),
            )),
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> Box<str> {
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone().into_boxed_str();
    }
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return Box::from(*message);
    }
    Box::from("non-string panic payload")
}

#[cfg(unix)]
fn install_termination_signal() -> Result<ShutdownFuture, ApplicationError> {
    use tokio::signal::unix::{SignalKind, signal};

    let interrupt =
        signal(SignalKind::interrupt()).map_err(|source| ApplicationError::SignalRegistration {
            signal: ShutdownSignal::Interrupt,
            source,
        })?;
    let terminate =
        signal(SignalKind::terminate()).map_err(|source| ApplicationError::SignalRegistration {
            signal: ShutdownSignal::Terminate,
            source,
        })?;

    Ok(Box::pin(wait_for_unix_termination(interrupt, terminate)))
}

#[cfg(unix)]
async fn wait_for_unix_termination(
    mut interrupt: tokio::signal::unix::Signal,
    mut terminate: tokio::signal::unix::Signal,
) -> Result<(), ApplicationError> {
    let closed_signal = tokio::select! {
        received = interrupt.recv() => {
            if received.is_some() {
                return Ok(());
            }
            ShutdownSignal::Interrupt
        }
        received = terminate.recv() => {
            if received.is_some() {
                return Ok(());
            }
            ShutdownSignal::Terminate
        }
    };

    Err(ApplicationError::SignalStreamClosed {
        signal: closed_signal,
    })
}

#[cfg(windows)]
fn install_termination_signal() -> Result<ShutdownFuture, ApplicationError> {
    let interrupt = tokio::signal::windows::ctrl_c().map_err(|source| {
        ApplicationError::SignalRegistration {
            signal: ShutdownSignal::Interrupt,
            source,
        }
    })?;

    Ok(Box::pin(wait_for_windows_termination(interrupt)))
}

#[cfg(windows)]
async fn wait_for_windows_termination(
    mut interrupt: tokio::signal::windows::CtrlC,
) -> Result<(), ApplicationError> {
    if interrupt.recv().await.is_some() {
        Ok(())
    } else {
        Err(ApplicationError::SignalStreamClosed {
            signal: ShutdownSignal::Interrupt,
        })
    }
}

#[cfg(not(any(unix, windows)))]
fn install_termination_signal() -> Result<ShutdownFuture, ApplicationError> {
    Err(ApplicationError::SignalPlatformUnsupported)
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        fs,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use axum::http::{Method, StatusCode};
    use k256::schnorr::SigningKey;
    use markdown_compiler::DefaultPostTipPolicy;
    use serde::{Serialize, de::DeserializeOwned};
    use tokio::sync::oneshot;

    use maincopy_shared::auth::{
        AdminAuditEventId, AdminScope, AgentCredentialId, InstanceId, UserId,
    };

    use crate::{
        admin::test_support::{ADMIN_AUTHORITY, ADMIN_ORIGIN, agent_authorization},
        config::ConfigurationValidationCode,
        domain::auth::{
            NostrPublicKey,
            store::{
                AdminMutationKey, AuditPrincipalReference, BootstrapIdentity, MutationAuditContext,
                NewHumanCredential, RegisterAgentCredential,
            },
        },
        render::render_bound_post_preview,
    };

    use super::*;

    const VALID_PUBLICATION: &str = "[site]\n\
title = \"Pinned startup source\"\n\
base_url = \"https://startup.example.test\"\n\
description = \"Startup configuration test.\"\n\
[author]\n\
name = \"Startup Tester\"\n";
    const DURABLE_POST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const DURABLE_PUBLICATION_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const TEST_OWNER_NOSTR_PUBLIC_KEY: &str =
        "63fe6318dc58583cfe16810f86dd09e18bfd76aabc24a0081ce2856f330504ed";
    const TEST_AGENT_SECRET: [u8; 32] = [3_u8; 32];
    const DURABLE_POST: &str = "+++\n\
id = \"11111111-1111-4111-8111-111111111111\"\n\
title = \"Durable publication\"\n\
slug = \"durable-publication\"\n\
authored_at = 2026-08-29T12:00:00Z\n\
description = \"A publication restored from SQLite.\"\n\
+++\n\
# Durable publication\n\n\
Durable article body.\n";

    fn startup_host_source(extra: &str, public_bind: &str) -> String {
        startup_host_source_with_admin(extra, public_bind, "127.0.0.1:0")
    }

    fn startup_host_source_with_admin(extra: &str, public_bind: &str, admin_bind: &str) -> String {
        format!(
            "[paths]\n\
             content_root = \"content\"\n\
             state_root = \"state\"\n\
             runtime_root = \"run\"\n\
             [public]\n\
             bind = \"{public_bind}\"\n\
             {extra}\n\
             [admin]\n\
             bind = \"{admin_bind}\"\n\
             origin = \"https://admin.example.test\"\n"
        )
    }

    fn startup_fixture(
        host_source: &str,
        publication_source: &str,
    ) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let content_root = root.path().join("content");
        fs::create_dir(&content_root).unwrap();
        let publication_path = content_root.join("publication.toml");
        fs::write(&publication_path, publication_source).unwrap();
        let config_path = root.path().join("maincopy.toml");
        fs::write(
            &config_path,
            startup_host_source(host_source, "127.0.0.1:0"),
        )
        .unwrap();
        (root, config_path, publication_path)
    }

    fn write_durable_post(content_root: &Path) {
        fs::create_dir(content_root.join("posts")).unwrap();
        fs::write(
            content_root.join("posts/durable-publication.md"),
            DURABLE_POST,
        )
        .unwrap();
    }

    struct AdminTestClient {
        address: std::net::SocketAddr,
        client: reqwest::Client,
        signing_key: SigningKey,
    }

    fn admin_client(application: &Application) -> AdminTestClient {
        AdminTestClient {
            address: application.admin_addr,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap(),
            signing_key: SigningKey::from_bytes(&TEST_AGENT_SECRET).unwrap(),
        }
    }

    async fn admin_get(admin: &AdminTestClient, path: &str) -> reqwest::Response {
        let idempotency_key = uuid::Uuid::new_v4().to_string();
        let authorization = agent_authorization(
            &admin.signing_key,
            &Method::GET,
            path,
            &[],
            Some(&idempotency_key),
        );
        admin
            .client
            .get(format!("http://{}{path}", admin.address))
            .header(reqwest::header::HOST, ADMIN_AUTHORITY)
            .header("origin", ADMIN_ORIGIN)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .header(
                maincopy_shared::publication::IDEMPOTENCY_KEY_HEADER,
                idempotency_key,
            )
            .send()
            .await
            .unwrap()
    }

    async fn admin_post_json(
        admin: &AdminTestClient,
        path: &str,
        idempotency_key: &str,
        value: &impl Serialize,
    ) -> reqwest::Response {
        admin_post_body(
            admin,
            path,
            idempotency_key,
            None,
            serde_json::to_vec(value).unwrap(),
        )
        .await
    }

    async fn admin_post_body(
        admin: &AdminTestClient,
        path: &str,
        idempotency_key: &str,
        request_id: Option<&str>,
        body: impl AsRef<[u8]>,
    ) -> reqwest::Response {
        let body = body.as_ref().to_vec();
        let authorization = agent_authorization(
            &admin.signing_key,
            &Method::POST,
            path,
            &body,
            Some(idempotency_key),
        );
        let mut request = admin
            .client
            .post(format!("http://{}{path}", admin.address))
            .header(reqwest::header::HOST, ADMIN_AUTHORITY)
            .header("origin", ADMIN_ORIGIN)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .header(
                maincopy_shared::publication::IDEMPOTENCY_KEY_HEADER,
                idempotency_key,
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        if let Some(request_id) = request_id {
            request = request.header("x-request-id", request_id);
        }
        request.body(body).send().await.unwrap()
    }

    async fn admin_json<ResponseType: DeserializeOwned>(
        response: reqwest::Response,
    ) -> ResponseType {
        let bytes = response.bytes().await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn admin_text(response: reqwest::Response) -> String {
        response.text().await.unwrap()
    }

    async fn admin_preview_digest(
        admin: &AdminTestClient,
        post_id: uuid::Uuid,
    ) -> maincopy_shared::publication::PreviewDigest {
        use maincopy_shared::{
            posts::POSTS_PATH,
            publication::{PREVIEW_DIGEST_HEADER, PreviewDigest},
        };

        let response = admin_get(admin, &format!("{POSTS_PATH}/{post_id}/preview")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let encoded = response
            .headers()
            .get(PREVIEW_DIGEST_HEADER)
            .unwrap()
            .to_str()
            .unwrap();
        PreviewDigest::parse(encoded).unwrap()
    }

    async fn stop_built_application(application: Application) {
        let Application {
            _startup: startup,
            _database: database,
            publication_coordinator,
            mut runtime,
            public_addr: _,
            admin_addr: _,
        } = application;
        runtime.cancellation.cancel();
        assert!(
            drain_critical_tasks(&mut runtime.critical_tasks)
                .await
                .is_none()
        );
        runtime.database_shutdown.cancel();
        assert!(
            drain_database_writer(&mut runtime.database_writer)
                .await
                .is_none()
        );
        drop(publication_coordinator);
        drop(database);
        drop(startup);
        tokio::task::yield_now().await;
    }

    async fn bootstrap_test_identity(startup: &StartupConfiguration) {
        let host = startup._host.view();
        let database = database::bootstrap(host.database).await.unwrap();
        let (store, writer) = database.into_store(host.database.writer_queue_capacity.get());
        let shutdown = CancellationToken::new();
        let writer_shutdown = shutdown.clone();
        let writer = tokio::spawn(async move {
            writer.run(writer_shutdown).await.unwrap();
        });
        if store
            .auth
            .identity_state()
            .await
            .unwrap()
            .bootstrap_required
        {
            let owner_user_id = UserId::from_uuid(uuid::Uuid::new_v4());
            store
                .auth
                .bootstrap_identity(BootstrapIdentity {
                    instance_id: InstanceId::from_uuid(uuid::Uuid::new_v4()),
                    owner_user_id,
                    credential: NewHumanCredential::Nostr {
                        public_key: NostrPublicKey::parse(TEST_OWNER_NOSTR_PUBLIC_KEY).unwrap(),
                    },
                    configured_providers: ConfiguredLoginProviders::new(true, true).unwrap(),
                    occurred_at: OffsetDateTime::now_utc(),
                    audit_event_id: AdminAuditEventId::from_uuid(uuid::Uuid::new_v4()),
                })
                .await
                .unwrap();
            let signing_key = SigningKey::from_bytes(&TEST_AGENT_SECRET).unwrap();
            store
                .auth
                .register_agent_credential(RegisterAgentCredential {
                    credential_id: AgentCredentialId::from_uuid(uuid::Uuid::new_v4()),
                    owner_user_id,
                    issuer_user_id: owner_user_id,
                    public_key: NostrPublicKey::from_bytes(
                        signing_key.verifying_key().to_bytes().into(),
                    )
                    .unwrap(),
                    label: "startup integration agent".into(),
                    scopes: AdminScope::PUBLISHER.into_iter().collect(),
                    created_at: OffsetDateTime::now_utc(),
                    expires_at: None,
                    audit: MutationAuditContext {
                        audit_event_id: AdminAuditEventId::from_uuid(uuid::Uuid::new_v4()),
                        principal: AuditPrincipalReference::Offline {
                            user_id: Some(owner_user_id),
                        },
                        request_id: None,
                        idempotency_key: AdminMutationKey(uuid::Uuid::new_v4()),
                    },
                })
                .await
                .unwrap();
        }
        drop(store);
        shutdown.cancel();
        writer.await.unwrap();
    }

    async fn build_test_application(
        startup: StartupConfiguration,
    ) -> Result<Application, ProcessError> {
        bootstrap_test_identity(&startup).await;
        Application::build(startup).await
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn content_discovery_runs_once_receives_effective_limits_and_validation_reuses_owned_bytes() {
        let host_source = "[content]\n\
publication_file_bytes = 512\n\
post_file_bytes = 1536\n\
asset_file_bytes = 2560\n\
total_tree_bytes = 3584\n\
entries = 90\n\
depth = 7\n\
path_bytes = 400\n";
        let (_root, arguments, publication_path) = startup_fixture(host_source, VALID_PUBLICATION);
        let discovery_calls = Cell::new(0);
        let observed_limits = Cell::new(None);

        let startup =
            StartupConfiguration::load_with_discovery(arguments, |content_root, limits| {
                discovery_calls.set(discovery_calls.get() + 1);
                observed_limits.set(Some(limits));
                let tree = discover_content_tree(content_root, limits)?;
                fs::write(&publication_path, "not the discovered publication").unwrap();
                Ok(tree)
            })
            .unwrap();

        assert_eq!(discovery_calls.get(), 1);
        let limits = observed_limits.get().unwrap();
        assert_eq!(limits.publication_file_bytes.get(), 512);
        assert_eq!(limits.post_file_bytes.get(), 1536);
        assert_eq!(limits.asset_file_bytes.get(), 2560);
        assert_eq!(limits.total_tree_bytes.get(), 3584);
        assert_eq!(limits.entries.get(), 90);
        assert_eq!(limits.depth.get(), 7);
        assert_eq!(limits.path_bytes.get(), 400);
        assert!(
            startup
                ._content_tree
                .publication
                .source
                .contains("Pinned startup source")
        );
        assert_eq!(
            startup._validated_content.publication.site.title.as_str(),
            "Pinned startup source"
        );
    }

    #[test]
    fn host_configuration_failure_prevents_content_discovery() {
        let root = tempfile::tempdir().unwrap();
        let arguments = root.path().join("missing-maincopy.toml");
        let discovery_calls = Cell::new(0);

        let result = StartupConfiguration::load_with_discovery(arguments, |_, _| {
            discovery_calls.set(discovery_calls.get() + 1);
            unreachable!("content discovery must follow host configuration")
        });

        assert!(matches!(result, Err(ProcessError::Configuration(_))));
        assert_eq!(discovery_calls.get(), 0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn content_validation_failure_has_stable_exit() {
        let (_root, arguments, _) = startup_fixture("", "unknown = true\n");
        let discovery_calls = Cell::new(0);

        let result = StartupConfiguration::load_with_discovery(arguments, |root, limits| {
            discovery_calls.set(discovery_calls.get() + 1);
            discover_content_tree(root, limits)
        });

        let Err(error) = result else {
            panic!("invalid content must fail startup");
        };
        assert!(matches!(error, ProcessError::Validation(_)));
        assert_eq!(error.exit(), ProcessExit::Validation);
        assert_eq!(discovery_calls.get(), 1);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn authored_tips_do_not_require_a_payment_provider() {
        let publication = format!(
            "{VALID_PUBLICATION}[tips]\n\
             enabled = true\n"
        );
        let (_root, arguments, _) = startup_fixture("", &publication);

        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();

        assert_eq!(
            startup._validated_content.publication.tips,
            DefaultPostTipPolicy::Enabled
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn removed_provider_configuration_is_rejected_without_opening_credentials() {
        let host = "[lightning]\n\
provider = \"lexe\"\n\
network = \"signet\"\n\
credentials = { source = \"file\", path = \"must-not-open.json\" }\n";
        let (root, arguments, _) = startup_fixture(host, VALID_PUBLICATION);
        let credential_path = root.path().join("must-not-open.json");

        let Err(error) =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree)
        else {
            panic!("removed provider configuration must fail host validation");
        };

        let ProcessError::Configuration(errors) = error else {
            panic!("removed provider configuration must fail host validation");
        };
        assert_eq!(
            errors.diagnostics()[0].code,
            ConfigurationValidationCode::HostTomlInvalid
        );
        assert!(!credential_path.exists());
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn unbootstrapped_identity_prevents_both_network_listeners() {
        let (root, _, _) = startup_fixture("", VALID_PUBLICATION);
        let config_path = root.path().join("maincopy.toml");
        let public_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let public_addr = public_probe.local_addr().unwrap();
        drop(public_probe);
        fs::write(
            &config_path,
            startup_host_source("", &public_addr.to_string()),
        )
        .unwrap();
        let arguments = config_path;
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let admin_addr = startup._host.view().admin_bind.into_socket_addr();

        let error = match Application::build(startup).await {
            Ok(_) => panic!("an unbootstrapped identity must prevent listener binding"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ProcessError::Application(ApplicationError::Startup {
                stage: StartupStage::Identity,
                ..
            })
        ));
        let rebound_public = tokio::net::TcpListener::bind(public_addr).await.unwrap();
        let rebound_admin = tokio::net::TcpListener::bind(admin_addr).await.unwrap();
        drop((rebound_public, rebound_admin));
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn application_build_serves_public_site_and_protected_admin_backend() {
        let (_root, arguments, _) = startup_fixture("", VALID_PUBLICATION);
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();

        let application = build_test_application(startup).await.unwrap();

        assert_eq!(application.runtime.critical_tasks.len(), 5);
        assert!(application.runtime.database_writer.is_some());
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        let response = client
            .get(format!("http://{}/", application.public_addr))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let html = response.text().await.unwrap();
        assert!(html.contains("Pinned startup source"));
        assert!(html.contains("No posts have been published yet."));

        let protected = client
            .get(format!(
                "http://{}/api/admin/v1/capabilities",
                application.admin_addr
            ))
            .header(reqwest::header::HOST, "admin.example.test")
            .send()
            .await
            .unwrap();
        assert_eq!(protected.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(
            protected.headers()[reqwest::header::CACHE_CONTROL],
            "private, no-store"
        );

        stop_built_application(application).await;
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn admin_publication_route_activates_the_public_site_and_replays_success() {
        use maincopy_shared::publication::{
            PUBLICATIONS_PATH, PublishNowRequest, PublishNowResponse,
        };

        let (root, arguments, _) = startup_fixture("", VALID_PUBLICATION);
        let content_root = root.path().join("content");
        write_durable_post(&content_root);
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let application = build_test_application(startup).await.unwrap();
        let admin = admin_client(&application);
        let post_id = uuid::Uuid::parse_str(DURABLE_POST_ID).unwrap();
        let request = PublishNowRequest {
            post_id,
            preview_digest: admin_preview_digest(&admin, post_id).await,
            expected_revision: None,
            scheduled_for: None,
        };
        let creation_key = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

        let response = admin_post_json(&admin, PUBLICATIONS_PATH, creation_key, &request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let published: PublishNowResponse = admin_json(response).await;
        assert_eq!(published.post_id, request.post_id);
        assert!(published.revision.starts_with("post-b3-v1-"));
        assert!(published.site_digest.starts_with("site-b3-v1-"));
        assert_eq!(published.site_version, 2);

        let replay = admin_post_body(
            &admin,
            PUBLICATIONS_PATH,
            creation_key,
            None,
            serde_json::to_string_pretty(&request).unwrap(),
        )
        .await;
        assert_eq!(replay.status(), StatusCode::OK);
        assert_eq!(admin_json::<PublishNowResponse>(replay).await, published);

        let request_id = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let malformed_key = uuid::Uuid::new_v4().to_string();
        let malformed = admin_post_body(
            &admin,
            PUBLICATIONS_PATH,
            &malformed_key,
            Some(request_id),
            "{",
        )
        .await;
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        let error: serde_json::Value = admin_json(malformed).await;
        assert_eq!(error["error"]["code"], "invalid_request_body");
        assert_eq!(error["error"]["request_id"], request_id);

        let oversized_key = uuid::Uuid::new_v4().to_string();
        let oversized = admin_post_body(
            &admin,
            PUBLICATIONS_PATH,
            &oversized_key,
            Some(request_id),
            " ".repeat(4 * 1024 + 1),
        )
        .await;
        assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(oversized.headers()["x-request-id"], request_id);
        let error: serde_json::Value = admin_json(oversized).await;
        assert_eq!(error["error"]["code"], "request_body_too_large");
        assert_eq!(error["error"]["request_id"], request_id);
        assert_eq!(
            admin_get(&admin, "/api/admin/v1/capabilities")
                .await
                .status(),
            StatusCode::OK
        );

        let public = reqwest::get(format!(
            "http://{}/posts/durable-publication",
            application.public_addr
        ))
        .await
        .unwrap();
        assert_eq!(public.status(), reqwest::StatusCode::OK);
        assert!(
            public
                .text()
                .await
                .unwrap()
                .contains("Durable article body.")
        );

        stop_built_application(application).await;
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn edited_markdown_updates_private_preview_until_explicit_publication_approval() {
        use maincopy_shared::{
            posts::{ListPostsResponse, POSTS_PATH, PostPublicationState},
            publication::{PUBLICATIONS_PATH, PublishNowRequest, PublishNowResponse},
        };

        let (root, arguments, _) = startup_fixture("", VALID_PUBLICATION);
        let content_root = root.path().join("content");
        write_durable_post(&content_root);
        let post_path = content_root.join("posts/durable-publication.md");
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let application = build_test_application(startup).await.unwrap();
        let admin = admin_client(&application);
        let response = admin_post_json(
            &admin,
            PUBLICATIONS_PATH,
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            &PublishNowRequest {
                post_id: uuid::Uuid::parse_str(DURABLE_POST_ID).unwrap(),
                preview_digest: admin_preview_digest(
                    &admin,
                    uuid::Uuid::parse_str(DURABLE_POST_ID).unwrap(),
                )
                .await,
                expected_revision: None,
                scheduled_for: None,
            },
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let published: PublishNowResponse = admin_json(response).await;

        let public_url = format!(
            "http://{}/posts/durable-publication",
            application.public_addr
        );
        let public = reqwest::Client::builder().no_proxy().build().unwrap();
        let initial = public.get(&public_url).send().await.unwrap();
        let initial_etag = initial.headers()[reqwest::header::ETAG].clone();
        assert!(
            initial
                .text()
                .await
                .unwrap()
                .contains("Durable article body.")
        );

        let edited = DURABLE_POST.replace(
            "Durable article body.",
            "This edit appeared without restarting Maincopy.",
        );
        fs::write(&post_path, &edited).unwrap();

        let edited_summary = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let posts: ListPostsResponse =
                    admin_json(admin_get(&admin, POSTS_PATH).await).await;
                if let Some(summary) = posts.posts.into_iter().find(|summary| {
                    summary.post_id.to_string() == DURABLE_POST_ID
                        && summary.publication_state == PostPublicationState::UnpublishedChange
                        && summary.revision != published.revision
                }) {
                    break summary;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("a stable Markdown edit must enter the private preview catalog");
        let preview_url = format!("{POSTS_PATH}/{}/preview", edited_summary.post_id);
        let preview = admin_get(&admin, &preview_url).await;
        assert_eq!(preview.status(), StatusCode::OK);
        let preview_body = admin_text(preview).await;
        assert!(preview_body.contains("This edit appeared without restarting Maincopy."));
        assert!(!preview_body.contains("Durable article body."));

        let still_pinned = public.get(&public_url).send().await.unwrap();
        assert_eq!(still_pinned.headers()[reqwest::header::ETAG], initial_etag);
        let still_pinned_body = still_pinned.text().await.unwrap();
        assert!(still_pinned_body.contains("Durable article body."));
        assert!(!still_pinned_body.contains("This edit appeared without restarting Maincopy."));

        fs::write(&post_path, "+++\ninvalid = true\n").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        let posts: ListPostsResponse = admin_json(admin_get(&admin, POSTS_PATH).await).await;
        let after_invalid = posts
            .posts
            .into_iter()
            .find(|summary| summary.post_id == edited_summary.post_id)
            .unwrap();
        assert_eq!(after_invalid.revision, edited_summary.revision);
        assert_eq!(
            after_invalid.publication_state,
            PostPublicationState::UnpublishedChange
        );
        let preview = admin_get(&admin, &preview_url).await;
        assert_eq!(preview.status(), StatusCode::OK);
        assert!(
            admin_text(preview)
                .await
                .contains("This edit appeared without restarting Maincopy.")
        );
        let after_invalid_public = public.get(&public_url).send().await.unwrap();
        assert_eq!(
            after_invalid_public.headers()[reqwest::header::ETAG],
            initial_etag
        );
        let after_invalid_public_body = after_invalid_public.text().await.unwrap();
        assert!(after_invalid_public_body.contains("Durable article body."));
        assert!(
            !after_invalid_public_body.contains("This edit appeared without restarting Maincopy.")
        );

        fs::write(&post_path, &edited).unwrap();
        let response = admin_post_json(
            &admin,
            PUBLICATIONS_PATH,
            "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
            &PublishNowRequest {
                post_id: edited_summary.post_id,
                preview_digest: admin_preview_digest(&admin, edited_summary.post_id).await,
                expected_revision: Some(edited_summary.revision.clone()),
                scheduled_for: None,
            },
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let approved: PublishNowResponse = admin_json(response).await;
        assert_eq!(approved.revision, edited_summary.revision);

        let updated_public = public.get(&public_url).send().await.unwrap();
        assert_ne!(
            updated_public.headers()[reqwest::header::ETAG],
            initial_etag
        );
        let updated_body = updated_public.text().await.unwrap();
        assert!(updated_body.contains("This edit appeared without restarting Maincopy."));
        assert!(!updated_body.contains("Durable article body."));

        stop_built_application(application).await;
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn a_new_markdown_file_becomes_publishable_without_restarting() {
        use maincopy_shared::{
            posts::{ListPostsResponse, POSTS_PATH},
            publication::{PUBLICATIONS_PATH, PublishNowRequest},
        };

        let (root, arguments, _) = startup_fixture("", VALID_PUBLICATION);
        let content_root = root.path().join("content");
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let application = build_test_application(startup).await.unwrap();
        let admin = admin_client(&application);

        write_durable_post(&content_root);
        let summary = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let posts: ListPostsResponse =
                    admin_json(admin_get(&admin, POSTS_PATH).await).await;
                if let Some(post) = posts
                    .posts
                    .into_iter()
                    .find(|post| post.post_id.to_string() == DURABLE_POST_ID)
                {
                    break post;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("a stable new Markdown file must enter the live catalog");

        let response = admin_post_json(
            &admin,
            PUBLICATIONS_PATH,
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            &PublishNowRequest {
                post_id: summary.post_id,
                preview_digest: admin_preview_digest(&admin, summary.post_id).await,
                expected_revision: Some(summary.revision),
                scheduled_for: None,
            },
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let body = reqwest::get(format!(
            "http://{}/posts/durable-publication",
            application.public_addr
        ))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
        assert!(body.contains("Durable article body."));

        stop_built_application(application).await;
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn restart_keeps_public_revision_pinned_until_the_stopped_edit_is_approved() {
        use maincopy_shared::{
            posts::{ListPostsResponse, POSTS_PATH, PostPublicationState},
            publication::{PUBLICATIONS_PATH, PublishNowRequest, PublishNowResponse},
        };

        let (root, arguments, _) = startup_fixture("", VALID_PUBLICATION);
        let content_root = root.path().join("content");
        write_durable_post(&content_root);
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let application = build_test_application(startup).await.unwrap();
        let admin = admin_client(&application);
        let response = admin_post_json(
            &admin,
            PUBLICATIONS_PATH,
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            &PublishNowRequest {
                post_id: uuid::Uuid::parse_str(DURABLE_POST_ID).unwrap(),
                preview_digest: admin_preview_digest(
                    &admin,
                    uuid::Uuid::parse_str(DURABLE_POST_ID).unwrap(),
                )
                .await,
                expected_revision: None,
                scheduled_for: None,
            },
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let published: PublishNowResponse = admin_json(response).await;
        let public = reqwest::Client::builder().no_proxy().build().unwrap();
        let initial_public_url = format!(
            "http://{}/posts/durable-publication",
            application.public_addr
        );
        let initial = public.get(&initial_public_url).send().await.unwrap();
        let initial_etag = initial.headers()[reqwest::header::ETAG].clone();
        assert!(
            initial
                .text()
                .await
                .unwrap()
                .contains("Durable article body.")
        );
        stop_built_application(application).await;

        fs::write(
            content_root.join("posts/durable-publication.md"),
            DURABLE_POST.replace(
                "Durable article body.",
                "This edit was made while Maincopy was stopped.",
            ),
        )
        .unwrap();
        let arguments = root.path().join("maincopy.toml");
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let restarted = build_test_application(startup).await.unwrap();
        let public_url = format!("http://{}/posts/durable-publication", restarted.public_addr);
        let pinned = public.get(&public_url).send().await.unwrap();
        assert_eq!(pinned.headers()[reqwest::header::ETAG], initial_etag);
        let pinned_body = pinned.text().await.unwrap();
        assert!(pinned_body.contains("Durable article body."));
        assert!(!pinned_body.contains("This edit was made while Maincopy was stopped."));

        let admin = admin_client(&restarted);
        let posts: ListPostsResponse = admin_json(admin_get(&admin, POSTS_PATH).await).await;
        let edited_summary = posts
            .posts
            .into_iter()
            .find(|summary| summary.post_id.to_string() == DURABLE_POST_ID)
            .unwrap();
        assert_eq!(
            edited_summary.publication_state,
            PostPublicationState::UnpublishedChange
        );
        assert_ne!(edited_summary.revision, published.revision);
        let preview = admin_get(
            &admin,
            &format!("{POSTS_PATH}/{}/preview", edited_summary.post_id),
        )
        .await;
        assert_eq!(preview.status(), StatusCode::OK);
        let preview_body = admin_text(preview).await;
        assert!(preview_body.contains("This edit was made while Maincopy was stopped."));
        assert!(!preview_body.contains("Durable article body."));

        let response = admin_post_json(
            &admin,
            PUBLICATIONS_PATH,
            "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
            &PublishNowRequest {
                post_id: edited_summary.post_id,
                preview_digest: admin_preview_digest(&admin, edited_summary.post_id).await,
                expected_revision: Some(edited_summary.revision.clone()),
                scheduled_for: None,
            },
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let approved: PublishNowResponse = admin_json(response).await;
        assert_eq!(approved.revision, edited_summary.revision);

        let updated = public.get(&public_url).send().await.unwrap();
        assert_ne!(updated.headers()[reqwest::header::ETAG], initial_etag);
        let updated_body = updated.text().await.unwrap();
        assert!(updated_body.contains("This edit was made while Maincopy was stopped."));
        assert!(!updated_body.contains("Durable article body."));

        stop_built_application(restarted).await;
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn restart_serves_the_durable_published_revision() {
        use maincopy_shared::{
            posts::{ListPostsResponse, POSTS_PATH, PostPublicationState},
            publication::{PUBLICATIONS_PATH, PublishNowRequest, PublishNowResponse},
        };

        let (root, arguments, _) = startup_fixture("", VALID_PUBLICATION);
        let content_root = root.path().join("content");
        write_durable_post(&content_root);
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let application = build_test_application(startup).await.unwrap();
        let admin = admin_client(&application);
        let response = admin_post_json(
            &admin,
            PUBLICATIONS_PATH,
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            &PublishNowRequest {
                post_id: uuid::Uuid::parse_str(DURABLE_POST_ID).unwrap(),
                preview_digest: admin_preview_digest(
                    &admin,
                    uuid::Uuid::parse_str(DURABLE_POST_ID).unwrap(),
                )
                .await,
                expected_revision: None,
                scheduled_for: None,
            },
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let published: PublishNowResponse = admin_json(response).await;
        let published_at = published.published_at.unwrap();
        let public = reqwest::Client::builder().no_proxy().build().unwrap();
        let initial_url = format!(
            "http://{}/posts/durable-publication",
            application.public_addr
        );
        let initial = public.get(&initial_url).send().await.unwrap();
        let initial_etag = initial.headers()[reqwest::header::ETAG].clone();
        let initial_html = initial.text().await.unwrap();
        assert!(initial_html.contains("Durable article body."));
        assert!(initial_html.contains(&published_at.to_string()));

        stop_built_application(application).await;

        let arguments = root.path().join("maincopy.toml");
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let application = build_test_application(startup).await.unwrap();
        let restarted_admin = admin_client(&application);
        let posts: ListPostsResponse =
            admin_json(admin_get(&restarted_admin, POSTS_PATH).await).await;
        let durable = posts
            .posts
            .into_iter()
            .find(|summary| summary.post_id == published.post_id)
            .unwrap();
        assert_eq!(durable.publication_state, PostPublicationState::Published);
        assert_eq!(durable.revision, published.revision);

        let response = public
            .get(format!(
                "http://{}/posts/durable-publication",
                application.public_addr
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.headers()[reqwest::header::ETAG], initial_etag);
        let html = response.text().await.unwrap();
        assert!(html.contains("Durable article body."));
        assert!(html.contains(&published_at.to_string()));

        stop_built_application(application).await;
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn startup_recovers_one_durable_publication_activation_before_binding() {
        use sqlx::{ConnectOptions as _, Connection as _};

        let (root, arguments, _) = startup_fixture("", VALID_PUBLICATION);
        let content_root = root.path().join("content");
        write_durable_post(&content_root);
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let database_path = startup._host.view().database.path.to_owned();
        stop_built_application(build_test_application(startup).await.unwrap()).await;

        let arguments = root.path().join("maincopy.toml");
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let content_digest = startup._content_tree.digest();
        let assets =
            resolve_content_assets(&startup._content_tree, &startup._validated_content).unwrap();
        let catalog =
            Arc::new(compile_content_catalog(&startup._validated_content, &assets).unwrap());
        let post_id = PostId::parse(DURABLE_POST_ID).unwrap();
        let rendered = catalog.current_post(&post_id).unwrap();
        let revision = rendered.revision.clone();
        let accepted_preview_digest = render_bound_post_preview(
            &catalog,
            embedded_manifest(),
            &post_id,
            None,
            "/api/admin/v1/preview-assets/recovery-fixture",
            None,
        )
        .unwrap()
        .unwrap()
        .digest;
        let activation_at = OffsetDateTime::from_unix_timestamp(1_777_734_400).unwrap();
        let candidate_ledger = PublicLedgerProjection::empty()
            .with_published(PublishedPostRevision::new(
                post_id,
                revision.clone(),
                activation_at,
            ))
            .unwrap();
        let shell = render_site_shell(Arc::clone(&catalog), embedded_manifest(), &candidate_ledger)
            .unwrap();
        let candidate = build_site_snapshot(shell, &candidate_ledger).unwrap();
        let candidate_digest = candidate.digest.clone();
        let activation_at_ns = i64::try_from(activation_at.unix_timestamp_nanos()).unwrap();
        let publication_id = uuid::Uuid::parse_str(DURABLE_PUBLICATION_ID)
            .unwrap()
            .into_bytes();
        let creation_key = uuid::Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
            .unwrap()
            .into_bytes();
        let stable_post_id = uuid::Uuid::parse_str(DURABLE_POST_ID).unwrap().into_bytes();
        let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&database_path)
            .foreign_keys(true)
            .connect()
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO canonical_publications (\
                publication_id, creation_key, command_kind, stable_post_id, pinned_post_digest, state, version, \
                scheduled_at_ns, activation_at_ns, activation_site_digest, content_tree_digest, \
                accepted_preview_digest\
             ) VALUES (?, ?, 'immediate', ?, ?, 'activating', 2, ?, ?, ?, ?, ?)",
        )
        .bind(publication_id.as_slice())
        .bind(creation_key.as_slice())
        .bind(stable_post_id.as_slice())
        .bind(revision.as_bytes().as_slice())
        .bind(activation_at_ns)
        .bind(activation_at_ns)
        .bind(candidate_digest.as_bytes().as_slice())
        .bind(content_digest.as_bytes().as_slice())
        .bind(accepted_preview_digest.as_bytes().as_slice())
        .execute(&mut connection)
        .await
        .unwrap();
        connection.close().await.unwrap();

        let application = build_test_application(startup).await.unwrap();
        {
            let projection = application.publication_coordinator.read();
            assert_eq!(projection.site.digest, candidate_digest);
            assert_eq!(projection.ledger.len(), 1);
        }
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap()
            .get(format!(
                "http://{}/posts/durable-publication",
                application.public_addr
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let html = response.text().await.unwrap();
        assert!(html.contains("Durable article body."));
        assert!(html.contains(&activation_at.to_string()));
        stop_built_application(application).await;

        let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&database_path)
            .connect()
            .await
            .unwrap();
        let (state, current_digest, published_at_ns): (String, Vec<u8>, i64) = sqlx::query_as(
            "SELECT state, current_published_digest, published_at_ns \
             FROM canonical_publications WHERE publication_id = ?",
        )
        .bind(publication_id.as_slice())
        .fetch_one(&mut connection)
        .await
        .unwrap();
        assert_eq!(state, "published");
        assert_eq!(current_digest, revision.as_bytes());
        assert_eq!(published_at_ns, activation_at_ns);
        connection.close().await.unwrap();
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn listener_failure_releases_the_public_port_and_database_ownership() {
        let (root, _, _) = startup_fixture("", VALID_PUBLICATION);
        let config_path = root.path().join("maincopy.toml");
        let reserved = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let public_addr = reserved.local_addr().unwrap();
        let public_bind = public_addr.to_string();
        fs::write(&config_path, startup_host_source("", &public_bind)).unwrap();
        let arguments = config_path.clone();
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let error = match build_test_application(startup).await {
            Ok(_) => panic!("an occupied public address must fail listener binding"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            ProcessError::Application(ApplicationError::Startup {
                stage: StartupStage::Listeners,
                ..
            })
        ));
        drop(reserved);
        let arguments = config_path;
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let application = build_test_application(startup).await.unwrap();
        assert_eq!(application.public_addr, public_addr);

        stop_built_application(application).await;
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn admin_listener_failure_releases_public_listener_and_database_ownership() {
        let (root, _, _) = startup_fixture("", VALID_PUBLICATION);
        let config_path = root.path().join("maincopy.toml");
        let reserved = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let admin_addr = reserved.local_addr().unwrap();
        fs::write(
            &config_path,
            startup_host_source_with_admin("", "127.0.0.1:0", &admin_addr.to_string()),
        )
        .unwrap();
        let arguments = config_path.clone();
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let error = match build_test_application(startup).await {
            Ok(_) => panic!("an occupied admin address must fail listener binding"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            ProcessError::Application(ApplicationError::Startup {
                stage: StartupStage::Listeners,
                ..
            })
        ));
        drop(reserved);
        let arguments = config_path;
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let application = build_test_application(startup).await.unwrap();
        assert_eq!(application.admin_addr, admin_addr);

        stop_built_application(application).await;
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn database_failure_prevents_application_build() {
        use std::os::unix::fs::PermissionsExt as _;

        use sqlx::{ConnectOptions as _, Connection as _};

        let (_root, arguments, _) = startup_fixture("", VALID_PUBLICATION);
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let database_path = startup._host.view().database.path.to_owned();
        let database_parent = database_path.parent().unwrap();
        fs::create_dir_all(database_parent).unwrap();
        fs::set_permissions(database_parent, fs::Permissions::from_mode(0o700)).unwrap();

        let mut foreign = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&database_path)
            .create_if_missing(true)
            .connect()
            .await
            .unwrap();
        sqlx::query("PRAGMA application_id = 7")
            .execute(&mut foreign)
            .await
            .unwrap();
        foreign.close().await.unwrap();
        fs::set_permissions(&database_path, fs::Permissions::from_mode(0o600)).unwrap();

        let error = match Application::build(startup).await {
            Ok(_) => panic!("a foreign database must fail before listener binding"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            ProcessError::Application(ApplicationError::Startup {
                stage: StartupStage::Database,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn shutdown_signal_marks_unready_cancels_and_drains_every_task() {
        let readiness = Readiness::default();
        let cancellation = CancellationToken::new();
        let drained = Arc::new(AtomicUsize::new(0));
        let observed_order = Arc::new(AtomicUsize::new(0));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let tasks = (0..2)
            .map(|_| {
                let readiness = readiness.clone();
                let cancellation = cancellation.clone();
                let drained = Arc::clone(&drained);
                let observed_order = Arc::clone(&observed_order);
                CriticalTask::new(CriticalTaskName::Worker, async move {
                    cancellation.cancelled().await;
                    if !readiness.is_ready() {
                        observed_order.fetch_add(1, Ordering::SeqCst);
                    }
                    drained.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            })
            .collect();
        let application = ApplicationRuntime::with_parts(
            readiness.clone(),
            cancellation.clone(),
            Box::pin(async move {
                let _ = shutdown_rx.await;
                Ok(())
            }),
            tasks,
        );

        let running = tokio::spawn(application.run_until_stop());
        while !readiness.is_ready() {
            tokio::task::yield_now().await;
        }
        assert!(readiness.is_ready());

        assert!(shutdown_tx.send(()).is_ok());
        assert!(running.await.unwrap().is_ok());
        assert!(!readiness.is_ready());
        assert!(cancellation.is_cancelled());
        assert_eq!(drained.load(Ordering::SeqCst), 2);
        assert_eq!(observed_order.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn shutdown_waits_for_the_database_writer_to_close() {
        let readiness = Readiness::default();
        let cancellation = CancellationToken::new();
        let database_shutdown = CancellationToken::new();
        let writer_shutdown = database_shutdown.clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (writer_started_tx, writer_started_rx) = oneshot::channel();
        let (writer_release_tx, writer_release_rx) = oneshot::channel();
        let writer = CriticalTask::new(CriticalTaskName::DatabaseWriter, async move {
            writer_shutdown.cancelled().await;
            let _ = writer_started_tx.send(());
            let _ = writer_release_rx.await;
            Ok(())
        });
        let application = ApplicationRuntime::with_database_writer(
            readiness.clone(),
            cancellation,
            database_shutdown,
            Box::pin(async move {
                let _ = shutdown_rx.await;
                Ok(())
            }),
            Vec::new(),
            spawn_critical_task(writer),
        );

        let running = tokio::spawn(application.run_until_stop());
        while !readiness.is_ready() {
            tokio::task::yield_now().await;
        }

        assert!(shutdown_tx.send(()).is_ok());
        assert!(writer_started_rx.await.is_ok());
        tokio::task::yield_now().await;
        assert!(!running.is_finished());

        assert!(writer_release_tx.send(()).is_ok());
        assert!(running.await.unwrap().is_ok());
        assert!(!readiness.is_ready());
    }

    #[tokio::test]
    async fn shutdown_drains_producers_before_stopping_the_database_writer() {
        let readiness = Readiness::default();
        let cancellation = CancellationToken::new();
        let producer_shutdown = cancellation.clone();
        let database_shutdown = CancellationToken::new();
        let writer_shutdown = database_shutdown.clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (producer_cancelled_tx, producer_cancelled_rx) = oneshot::channel();
        let (producer_release_tx, producer_release_rx) = oneshot::channel();
        let (writer_cancelled_tx, mut writer_cancelled_rx) = oneshot::channel();
        let producer = CriticalTask::new(CriticalTaskName::Worker, async move {
            producer_shutdown.cancelled().await;
            let _ = producer_cancelled_tx.send(());
            let _ = producer_release_rx.await;
            Ok(())
        });
        let writer = CriticalTask::new(CriticalTaskName::DatabaseWriter, async move {
            writer_shutdown.cancelled().await;
            let _ = writer_cancelled_tx.send(());
            Ok(())
        });
        let application = ApplicationRuntime::with_database_writer(
            readiness.clone(),
            cancellation,
            database_shutdown,
            Box::pin(async move {
                let _ = shutdown_rx.await;
                Ok(())
            }),
            vec![producer],
            spawn_critical_task(writer),
        );

        let running = tokio::spawn(application.run_until_stop());
        while !readiness.is_ready() {
            tokio::task::yield_now().await;
        }
        assert!(shutdown_tx.send(()).is_ok());
        assert!(producer_cancelled_rx.await.is_ok());
        assert!(matches!(
            writer_cancelled_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        assert!(producer_release_tx.send(()).is_ok());
        assert!(writer_cancelled_rx.await.is_ok());
        assert!(running.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn unexpected_success_cancels_and_drains_before_returning_failure() {
        let (result, readiness, cancellation, companion_drained) =
            run_with_unexpected_task(async { Ok(()) }).await;

        assert!(matches!(
            result,
            Err(ApplicationError::CriticalTaskExited {
                task: CriticalTaskName::Scheduler
            })
        ));
        assert_shutdown_state(readiness, cancellation, companion_drained);
    }

    #[tokio::test]
    async fn task_error_cancels_and_drains_before_returning_failure() {
        let (result, readiness, cancellation, companion_drained) =
            run_with_unexpected_task(async {
                Err(Box::new(std::io::Error::other("task failed")) as CriticalTaskFailure)
            })
            .await;

        assert!(matches!(
            result,
            Err(ApplicationError::CriticalTaskFailed {
                task: CriticalTaskName::Scheduler,
                ..
            })
        ));
        assert_shutdown_state(readiness, cancellation, companion_drained);
    }

    #[tokio::test]
    async fn task_panic_cancels_and_drains_before_returning_failure() {
        async fn panicking_task() -> Result<(), CriticalTaskFailure> {
            panic!("task panicked")
        }

        let (result, readiness, cancellation, companion_drained) =
            run_with_unexpected_task(panicking_task()).await;

        assert!(matches!(
            result,
            Err(ApplicationError::CriticalTaskPanicked {
                task: CriticalTaskName::Scheduler,
                ..
            })
        ));
        assert_shutdown_state(readiness, cancellation, companion_drained);
    }

    #[tokio::test]
    async fn database_writer_failure_marks_unready_and_drains_producers() {
        let readiness = Readiness::default();
        let cancellation = CancellationToken::new();
        let database_shutdown = CancellationToken::new();
        let companion_drained = Arc::new(AtomicBool::new(false));
        let companion = {
            let cancellation = cancellation.clone();
            let companion_drained = Arc::clone(&companion_drained);
            CriticalTask::new(CriticalTaskName::Worker, async move {
                cancellation.cancelled().await;
                companion_drained.store(true, Ordering::SeqCst);
                Ok(())
            })
        };
        let writer = CriticalTask::new(CriticalTaskName::DatabaseWriter, async {
            Err(Box::new(std::io::Error::other("writer stopped")) as CriticalTaskFailure)
        });
        let application = ApplicationRuntime::with_database_writer(
            readiness.clone(),
            cancellation.clone(),
            database_shutdown.clone(),
            Box::pin(std::future::pending()),
            vec![companion],
            spawn_critical_task(writer),
        );

        let result = application.run_until_stop().await;

        assert!(matches!(
            result,
            Err(ApplicationError::CriticalTaskFailed {
                task: CriticalTaskName::DatabaseWriter,
                ..
            })
        ));
        assert!(!readiness.is_ready());
        assert!(cancellation.is_cancelled());
        assert!(database_shutdown.is_cancelled());
        assert!(companion_drained.load(Ordering::SeqCst));
    }

    async fn run_with_unexpected_task<Future>(
        trigger: Future,
    ) -> (
        Result<(), ApplicationError>,
        Readiness,
        CancellationToken,
        Arc<AtomicBool>,
    )
    where
        Future: std::future::Future<Output = CriticalTaskResult> + Send + 'static,
    {
        let readiness = Readiness::default();
        let cancellation = CancellationToken::new();
        let companion_drained = Arc::new(AtomicBool::new(false));
        let companion = {
            let cancellation = cancellation.clone();
            let companion_drained = Arc::clone(&companion_drained);
            async move {
                cancellation.cancelled().await;
                companion_drained.store(true, Ordering::SeqCst);
                Ok(())
            }
        };
        let application = ApplicationRuntime::with_parts(
            readiness.clone(),
            cancellation.clone(),
            Box::pin(std::future::pending()),
            vec![
                CriticalTask::new(CriticalTaskName::Scheduler, trigger),
                CriticalTask::new(CriticalTaskName::Worker, companion),
            ],
        );

        let result = application.run_until_stop().await;
        (result, readiness, cancellation, companion_drained)
    }

    fn assert_shutdown_state(
        readiness: Readiness,
        cancellation: CancellationToken,
        companion_drained: Arc<AtomicBool>,
    ) {
        assert!(!readiness.is_ready());
        assert!(cancellation.is_cancelled());
        assert!(companion_drained.load(Ordering::SeqCst));
    }
}
