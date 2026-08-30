use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use clap::{Parser, error::ErrorKind};
use time::OffsetDateTime;
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::{
    admin::AdminSocket,
    cli::ServerArguments,
    config::{HostConfiguration, HostConfigurationLoader, tip_provider_required},
    content::{
        ContentTreeLimits, ContentValidationErrors, DiscoveredContentTree, SourceCommit,
        SourceCommitDiscovery, ValidatedContent, discover_content_tree, discover_source_commit,
        resolve_content_assets,
    },
    database::{self, DatabaseStore},
    domain::publication::store::{InstallStartupSnapshot, ObservedPostRevision},
    error::{
        ApplicationError, CriticalTaskName, ProcessError, ProcessExit, ShutdownSignal, StartupStage,
    },
    frontend_assets::FrontendAssetManifest,
    observability::{initialize_logging, task_span},
    process_lock::{ProcessLock, ProcessLockError},
    render::{
        ContentCatalog, SiteSnapshotReader, build_site_snapshot, compile_content_catalog,
        render_site_shell,
    },
    web::{PublicServer, PublicState, Readiness},
};

type ShutdownFuture = Pin<Box<dyn Future<Output = Result<(), ApplicationError>> + Send>>;
type CriticalTaskFuture = Pin<Box<dyn Future<Output = CriticalTaskResult> + Send>>;
type CriticalTaskResult = Result<(), CriticalTaskFailure>;
type CriticalTaskFailure = Box<dyn std::error::Error + Send + Sync>;

/// Owns Maincopy's process-level resources and lifecycle.
///
/// Configuration validation, dependency construction, listener binding, and
/// task creation belong in [`Application::build`]. Runtime supervision and
/// ordered shutdown belong in [`Application::run_until_stop`].
pub(crate) struct Application {
    _startup: StartupConfiguration,
    _database: DatabaseStore,
    runtime: ApplicationRuntime,
    #[cfg(test)]
    public_addr: std::net::SocketAddr,
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
}

struct ServingState {
    readiness: Readiness,
    public_server: PublicServer,
    admin_socket: AdminSocket,
}

impl StartupConfiguration {
    fn load_with_discovery<Discover>(
        arguments: ServerArguments,
        discover: Discover,
    ) -> Result<Self, ProcessError>
    where
        Discover: FnOnce(
            &Path,
            ContentTreeLimits,
        ) -> Result<DiscoveredContentTree, ContentValidationErrors>,
    {
        let (path, overrides) = arguments.into_configuration();
        let host = HostConfigurationLoader::from_process_working_directory()?
            .load_with_overrides(&path, overrides)?;
        let host_view = host.view();
        let process_lock = match ProcessLock::acquire(host_view.runtime_root) {
            Ok(process_lock) => process_lock,
            Err(ProcessLockError::AlreadyRunning) => return Err(ProcessError::AlreadyRunning),
            Err(error) => {
                tracing::error!(error = %error, "process lock acquisition failed");
                return Err(ApplicationError::Startup {
                    stage: crate::error::StartupStage::ProcessLock,
                }
                .into());
            }
        };
        let content_tree = discover(host_view.content_root, host_view.content_limits)?;
        let validated_content = content_tree.validate()?;
        if validated_content.publication.tips.is_configured() && host_view.lightning.is_none() {
            return Err(tip_provider_required().into());
        }

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
    let arguments = match ServerArguments::try_parse_from(std::env::args_os()) {
        Ok(arguments) => arguments,
        Err(error) => return report_command_error(error),
    };
    let result: Result<(), ProcessError> = async {
        let startup = StartupConfiguration::load_with_discovery(arguments, discover_content_tree)?;
        let application = Application::build(startup).await?;
        application
            .run_until_stop()
            .await
            .map_err(ProcessError::from)
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
        let database_queue_capacity = host.database.writer_queue_capacity.get();
        let frontend = crate::frontend_assets::embedded_manifest();
        frontend.validate().map_err(|error| {
            startup_failure(
                StartupStage::FrontendAssets,
                "validate embedded frontend assets",
                error,
            )
        })?;
        let shutdown = install_termination_signal()?;
        let compiled = compile_startup_content(&startup, host.content_root)?;

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

        let serving_state = match prepare_serving_state(
            &database_store,
            compiled,
            frontend,
            host.public_bind,
            host.admin_socket,
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
            public_server,
            admin_socket,
        } = serving_state;
        #[cfg(test)]
        let public_addr = public_server.local_addr;
        let public_cancellation = cancellation.clone();
        let public_task = CriticalTask::new(CriticalTaskName::PublicServer, async move {
            public_server
                .serve(public_cancellation)
                .await
                .map_err(|error| Box::new(error) as CriticalTaskFailure)
        });
        let admin_cancellation = cancellation.clone();
        let admin_task = CriticalTask::new(CriticalTaskName::AdminServer, async move {
            admin_socket
                .serve(admin_cancellation)
                .await
                .map_err(|error| Box::new(error) as CriticalTaskFailure)
        });

        Ok(Self {
            _startup: startup,
            _database: database_store,
            runtime: ApplicationRuntime::with_database_writer(
                readiness,
                cancellation,
                database_shutdown,
                shutdown,
                vec![public_task, admin_task],
                database_task,
            ),
            #[cfg(test)]
            public_addr,
        })
    }

    async fn run_until_stop(self) -> Result<(), ApplicationError> {
        let Self {
            _startup: startup,
            _database: database,
            runtime,
            #[cfg(test)]
                public_addr: _,
        } = self;
        let runtime_result = runtime.run_until_stop().await;
        drop(database);
        drop(startup);
        runtime_result
    }
}

fn compile_startup_content(
    startup: &StartupConfiguration,
    content_root: &Path,
) -> Result<CompiledStartupContent, ProcessError> {
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
    })
}

async fn prepare_serving_state(
    database: &DatabaseStore,
    compiled: CompiledStartupContent,
    frontend: &'static FrontendAssetManifest,
    public_bind: std::net::SocketAddr,
    admin_socket: &Path,
) -> Result<ServingState, ProcessError> {
    let startup_state = database
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
    let shell = render_site_shell(compiled.catalog, frontend, &startup_state.ledger)
        .map_err(|error| startup_failure(StartupStage::Content, "render the site shell", error))?;
    let snapshot = build_site_snapshot(shell, &startup_state.ledger).map_err(|error| {
        startup_failure(StartupStage::Content, "build the site snapshot", error)
    })?;
    database
        .publications
        .install_startup_snapshot(InstallStartupSnapshot {
            expected: startup_state.site,
            candidate_digest: snapshot.digest.clone(),
            activated_at: OffsetDateTime::now_utc(),
            source_commit: compiled.source_commit,
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
    let public_server = PublicServer::bind(
        public_bind,
        PublicState {
            snapshots: SiteSnapshotReader::from_snapshot(snapshot),
            readiness: readiness.clone(),
        },
    )
    .await
    .map_err(|error| startup_failure(StartupStage::Listeners, "bind the public listener", error))?;
    tracing::info!(bind = %public_server.local_addr, "public listener bound");
    let admin_socket = AdminSocket::bind(admin_socket).map_err(|error| {
        if error.is_conflict() {
            ProcessError::AlreadyRunning
        } else {
            startup_failure(StartupStage::Listeners, "bind the admin listener", error)
        }
    })?;
    Ok(ServingState {
        readiness,
        public_server,
        admin_socket,
    })
}

fn startup_failure(
    stage: StartupStage,
    operation: &'static str,
    error: impl std::fmt::Display,
) -> ProcessError {
    tracing::error!(%stage, operation, error = %error, "startup operation failed");
    ApplicationError::Startup { stage }.into()
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
        ffi::OsString,
        fs,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use clap::Parser;
    use tokio::sync::oneshot;

    use super::*;

    const VALID_PUBLICATION: &str = "[site]\n\
title = \"Pinned startup source\"\n\
base_url = \"https://startup.example.test\"\n\
description = \"Startup configuration test.\"\n\
[author]\n\
name = \"Startup Tester\"\n";

    fn server_arguments_for(config: &Path, content_root: &Path) -> ServerArguments {
        server_arguments_for_with(config, content_root, &[])
    }

    fn server_arguments_for_with(
        config: &Path,
        content_root: &Path,
        overrides: &[(&str, &str)],
    ) -> ServerArguments {
        let fixture_root = config.parent().expect("test configuration has a parent");
        let mut command = vec![
            OsString::from("maincopyd"),
            OsString::from("--config"),
            config.as_os_str().to_owned(),
            OsString::from("--content-root"),
            content_root.as_os_str().to_owned(),
            OsString::from("--state-root"),
            fixture_root.join("state").into_os_string(),
            OsString::from("--runtime-root"),
            fixture_root.join("run").into_os_string(),
            OsString::from("--public-bind"),
            OsString::from("127.0.0.1:0"),
        ];
        for (flag, value) in overrides {
            command.push(OsString::from(flag));
            command.push(OsString::from(value));
        }
        ServerArguments::try_parse_from(command).unwrap()
    }

    fn startup_fixture(
        host_source: &str,
        publication_source: &str,
    ) -> (tempfile::TempDir, ServerArguments, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let content_root = root.path().join("content");
        fs::create_dir(&content_root).unwrap();
        let publication_path = content_root.join("publication.toml");
        fs::write(&publication_path, publication_source).unwrap();
        let config_path = root.path().join("maincopy.toml");
        fs::write(&config_path, host_source).unwrap();
        let arguments = server_arguments_for(&config_path, &content_root);
        (root, arguments, publication_path)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn content_discovery_runs_once_receives_effective_limits_and_validation_reuses_owned_bytes() {
        let host_source = "[content]\n\
publication_file_bytes = 1024\n\
post_file_bytes = 2048\n\
asset_file_bytes = 3072\n\
total_tree_bytes = 4096\n\
entries = 100\n\
depth = 8\n\
path_bytes = 512\n";
        let (root, _, publication_path) = startup_fixture(host_source, VALID_PUBLICATION);
        let arguments = server_arguments_for_with(
            &root.path().join("maincopy.toml"),
            &root.path().join("content"),
            &[
                ("--content-publication-file-bytes", "512"),
                ("--content-post-file-bytes", "1536"),
                ("--content-asset-file-bytes", "2560"),
                ("--content-total-tree-bytes", "3584"),
                ("--content-entries", "90"),
                ("--content-depth", "7"),
                ("--content-path-bytes", "400"),
            ],
        );
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
        let arguments = server_arguments_for(
            &root.path().join("missing-maincopy.toml"),
            &root.path().join("content"),
        );
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
    fn configured_disabled_tip_range_still_requires_a_provider() {
        let publication = format!(
            "{VALID_PUBLICATION}[tips]\n\
             enabled = false\n\
             minimum_sats = 1\n\
             maximum_sats = 2\n"
        );
        let (_root, arguments, _) = startup_fixture("", &publication);

        let result = StartupConfiguration::load_with_discovery(arguments, discover_content_tree);
        let Err(error) = result else {
            panic!("a configured tip range must require a provider");
        };

        assert_eq!(error.exit(), ProcessExit::Configuration);
        let ProcessError::Configuration(errors) = error else {
            panic!("missing tip capability must be a host configuration error");
        };
        assert_eq!(errors.diagnostics().len(), 1);
        assert_eq!(
            errors.diagnostics()[0].code,
            crate::config::ConfigurationValidationCode::TipProviderRequired
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn unconfigured_tips_do_not_activate_or_open_lexe_credentials() {
        let host = "[lightning]\n\
provider = \"lexe\"\n\
network = \"signet\"\n\
credentials = { source = \"file\", path = \"must-not-open.json\" }\n";
        let (root, arguments, _) = startup_fixture(host, VALID_PUBLICATION);
        let credential_path = root.path().join("must-not-open.json");

        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();

        assert!(startup._host.view().lightning.is_some());
        assert!(!credential_path.exists());
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn application_build_serves_the_initial_site_and_binds_the_admin_socket() {
        let (root, arguments, _) = startup_fixture("", VALID_PUBLICATION);
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let socket_path = root.path().join("run/admin.sock");

        let application = Application::build(startup).await.unwrap();

        assert!(socket_path.exists());
        assert_eq!(application.runtime.critical_tasks.len(), 2);
        assert!(application.runtime.database_writer.is_some());
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap()
            .get(format!("http://{}/", application.public_addr))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let html = response.text().await.unwrap();
        assert!(html.contains("Pinned startup source"));
        assert!(html.contains("No posts have been published yet."));

        let Application {
            _startup: startup,
            _database: database,
            mut runtime,
            public_addr: _,
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
        drop(database);
        drop(startup);
        tokio::task::yield_now().await;
        assert!(!socket_path.exists());
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn database_failure_leaves_the_admin_socket_absent() {
        use std::os::unix::fs::PermissionsExt as _;

        use sqlx::{ConnectOptions as _, Connection as _};

        let (root, arguments, _) = startup_fixture("", VALID_PUBLICATION);
        let startup =
            StartupConfiguration::load_with_discovery(arguments, discover_content_tree).unwrap();
        let database_path = startup._host.view().database.path.to_owned();
        let socket_path = root.path().join("run/admin.sock");
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
                stage: crate::error::StartupStage::Database
            })
        ));
        assert!(!socket_path.exists());
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
        let (result, readiness, cancellation, companion_drained) =
            run_with_unexpected_task(async {
                panic!("task panicked");
                #[allow(unreachable_code)]
                Ok(())
            })
            .await;

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
