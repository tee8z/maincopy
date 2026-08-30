use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    pin::Pin,
    task::{Context, Poll},
};

use clap::{Parser, error::ErrorKind};
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::{
    cli::{AdminCommand, ProcessArguments, ProcessCommand, ServeArguments},
    config::{HostConfiguration, HostConfigurationLoader, tip_provider_required},
    content::{
        ContentTreeLimits, ContentValidationErrors, DiscoveredContentTree, ValidatedContent,
        discover_content_tree,
    },
    error::{ApplicationError, CriticalTaskName, ProcessError, ProcessExit, ShutdownSignal},
    observability::{initialize_logging, task_span},
    web::Readiness,
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
    runtime: ApplicationRuntime,
}

struct ApplicationRuntime {
    readiness: Readiness,
    cancellation: CancellationToken,
    shutdown: ShutdownFuture,
    critical_tasks: JoinSet<(CriticalTaskName, CriticalTaskCompletion)>,
}

struct StartupConfiguration {
    _host: HostConfiguration,
    _content_tree: DiscoveredContentTree,
    _validated_content: ValidatedContent,
}

impl StartupConfiguration {
    fn load(arguments: ServeArguments) -> Result<Self, ProcessError> {
        Self::load_with_discovery(arguments, discover_content_tree)
    }

    fn load_with_discovery<Discover>(
        arguments: ServeArguments,
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
        let content_tree = discover(host_view.content_root, host_view.content_limits)?;
        let validated_content = content_tree.validate()?;
        if validated_content.publication().tips().is_configured() && host_view.lightning.is_none() {
            return Err(tip_provider_required().into());
        }

        Ok(Self {
            _host: host,
            _content_tree: content_tree,
            _validated_content: validated_content,
        })
    }
}

/// Parses one process command and runs it to completion.
pub async fn run_until_stop() -> ProcessExit {
    initialize_logging();
    let mut driver = ProductionDriver;
    run_with(std::env::args_os(), &mut driver).await
}

async fn run_with<I, T, Driver>(arguments: I, driver: &mut Driver) -> ProcessExit
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
    Driver: ProcessDriver,
{
    let arguments = match ProcessArguments::try_parse_from(arguments) {
        Ok(arguments) => arguments,
        Err(error) => return report_command_error(error),
    };

    match dispatch_with(arguments, driver).await {
        Ok(()) => ProcessExit::Success,
        Err(error) => {
            let exit = error.exit();
            tracing::error!(
                error = %error,
                category = ?error.category(),
                exit_code = exit.code(),
                "process command failed"
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

async fn dispatch_with<Driver>(
    arguments: ProcessArguments,
    driver: &mut Driver,
) -> Result<(), ProcessError>
where
    Driver: ProcessDriver,
{
    match arguments.command {
        ProcessCommand::Serve(arguments) => {
            let application = driver.build_application(*arguments).await?;
            driver.run_application(application).await
        }
        ProcessCommand::Admin { command } => driver.run_admin_command(command).await,
    }
}

trait ProcessDriver {
    type Application;

    async fn build_application(
        &mut self,
        arguments: ServeArguments,
    ) -> Result<Self::Application, ProcessError>;

    async fn run_application(&mut self, application: Self::Application)
    -> Result<(), ProcessError>;

    async fn run_admin_command(&mut self, command: AdminCommand) -> Result<(), ProcessError>;
}

struct ProductionDriver;

fn build_application_with<Startup, BuiltApplication, LoadStartup, BuildApplication>(
    arguments: ServeArguments,
    load_startup: LoadStartup,
    build_application: BuildApplication,
) -> Result<BuiltApplication, ProcessError>
where
    LoadStartup: FnOnce(ServeArguments) -> Result<Startup, ProcessError>,
    BuildApplication: FnOnce(Startup) -> Result<BuiltApplication, ApplicationError>,
{
    let startup = load_startup(arguments)?;
    build_application(startup).map_err(ProcessError::from)
}

impl ProcessDriver for ProductionDriver {
    type Application = Application;

    async fn build_application(
        &mut self,
        arguments: ServeArguments,
    ) -> Result<Self::Application, ProcessError> {
        build_application_with(arguments, StartupConfiguration::load, Application::build)
    }

    async fn run_application(
        &mut self,
        application: Self::Application,
    ) -> Result<(), ProcessError> {
        application
            .run_until_stop()
            .await
            .map_err(ProcessError::from)
    }

    async fn run_admin_command(&mut self, command: AdminCommand) -> Result<(), ProcessError> {
        match command {
            AdminCommand::Capabilities => Err(ProcessError::AdminApiUnavailable),
        }
    }
}

impl Application {
    fn build(startup: StartupConfiguration) -> Result<Self, ApplicationError> {
        Ok(Self {
            _startup: startup,
            runtime: ApplicationRuntime::build_with_signal_installer(
                Readiness::default(),
                install_termination_signal,
            )?,
        })
    }

    async fn run_until_stop(self) -> Result<(), ApplicationError> {
        self.runtime.run_until_stop().await
    }
}

impl ApplicationRuntime {
    fn build_with_signal_installer<Install>(
        readiness: Readiness,
        install_signal: Install,
    ) -> Result<Self, ApplicationError>
    where
        Install: FnOnce() -> Result<ShutdownFuture, ApplicationError>,
    {
        crate::frontend_assets::embedded_manifest()
            .validate()
            .map_err(|_| ApplicationError::Startup {
                stage: crate::error::StartupStage::FrontendAssets,
            })?;
        let shutdown = install_signal()?;
        Ok(Self::with_parts(
            readiness,
            CancellationToken::new(),
            shutdown,
            Vec::new(),
        ))
    }

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
            shutdown,
            critical_tasks: supervisor,
        }
    }

    async fn run_until_stop(mut self) -> Result<(), ApplicationError> {
        self.readiness.mark_ready();

        let failure = if self.critical_tasks.is_empty() {
            self.shutdown.await.err()
        } else {
            tokio::select! {
                biased;
                shutdown = &mut self.shutdown => shutdown.err(),
                completion = self.critical_tasks.join_next() => {
                    Some(unexpected_task_failure(completion))
                }
            }
        };

        self.readiness.mark_not_ready();
        self.cancellation.cancel();

        let drain_failure = drain_critical_tasks(&mut self.critical_tasks).await;
        match failure.or(drain_failure) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

async fn drain_critical_tasks(
    critical_tasks: &mut JoinSet<(CriticalTaskName, CriticalTaskCompletion)>,
) -> Option<ApplicationError> {
    let mut first_failure = None;

    while let Some(completion) = critical_tasks.join_next().await {
        let failure = drained_task_failure(completion);
        if first_failure.is_none() {
            first_failure = failure;
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
    #[cfg(test)]
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
    use crate::error::StartupStage;

    const VALID_PUBLICATION: &str = "[site]\n\
title = \"Pinned startup source\"\n\
base_url = \"https://startup.example.test\"\n\
description = \"Startup configuration test.\"\n\
[author]\n\
name = \"Startup Tester\"\n";

    fn serve_arguments_for(config: &Path, content_root: &Path) -> ServeArguments {
        serve_arguments_for_with(config, content_root, &[])
    }

    fn serve_arguments_for_with(
        config: &Path,
        content_root: &Path,
        overrides: &[(&str, &str)],
    ) -> ServeArguments {
        let mut command = vec![
            OsString::from("maincopy"),
            OsString::from("serve"),
            OsString::from("--config"),
            config.as_os_str().to_owned(),
            OsString::from("--content-root"),
            content_root.as_os_str().to_owned(),
        ];
        for (flag, value) in overrides {
            command.push(OsString::from(flag));
            command.push(OsString::from(value));
        }
        let arguments = ProcessArguments::try_parse_from(command).unwrap();
        let ProcessCommand::Serve(arguments) = arguments.command else {
            panic!("serve command must parse");
        };
        *arguments
    }

    fn startup_fixture(
        host_source: &str,
        publication_source: &str,
    ) -> (tempfile::TempDir, ServeArguments, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let content_root = root.path().join("content");
        fs::create_dir(&content_root).unwrap();
        let publication_path = content_root.join("publication.toml");
        fs::write(&publication_path, publication_source).unwrap();
        let config_path = root.path().join("maincopy.toml");
        fs::write(&config_path, host_source).unwrap();
        let arguments = serve_arguments_for(&config_path, &content_root);
        (root, arguments, publication_path)
    }

    #[derive(Clone, Copy)]
    enum BuildOutcome {
        Succeed,
        Fail,
    }

    struct FakeDriver {
        build_outcome: BuildOutcome,
        build_calls: usize,
        run_calls: usize,
        admin_calls: usize,
        runtime_seen: bool,
    }

    impl FakeDriver {
        fn succeeding() -> Self {
            Self {
                build_outcome: BuildOutcome::Succeed,
                build_calls: 0,
                run_calls: 0,
                admin_calls: 0,
                runtime_seen: false,
            }
        }

        fn failing() -> Self {
            Self {
                build_outcome: BuildOutcome::Fail,
                ..Self::succeeding()
            }
        }
    }

    impl ProcessDriver for FakeDriver {
        type Application = ();

        async fn build_application(
            &mut self,
            _arguments: ServeArguments,
        ) -> Result<Self::Application, ProcessError> {
            self.build_calls += 1;
            self.runtime_seen = tokio::runtime::Handle::try_current().is_ok();

            match self.build_outcome {
                BuildOutcome::Succeed => Ok(()),
                BuildOutcome::Fail => Err(ApplicationError::Startup {
                    stage: StartupStage::Configuration,
                }
                .into()),
            }
        }

        async fn run_application(
            &mut self,
            _application: Self::Application,
        ) -> Result<(), ProcessError> {
            self.run_calls += 1;
            Ok(())
        }

        async fn run_admin_command(&mut self, _command: AdminCommand) -> Result<(), ProcessError> {
            self.admin_calls += 1;
            Ok(())
        }
    }

    #[tokio::test]
    async fn help_exits_successfully_without_building_the_application() {
        let mut driver = FakeDriver::succeeding();

        let exit = run_with(["maincopy", "--help"], &mut driver).await;

        assert_eq!(exit, ProcessExit::Success);
        assert_eq!(driver.build_calls, 0);
        assert_eq!(driver.run_calls, 0);
    }

    #[tokio::test]
    async fn unknown_command_is_usage_error_without_building_the_application() {
        let mut driver = FakeDriver::succeeding();

        let exit = run_with(["maincopy", "unknown"], &mut driver).await;

        assert_eq!(exit, ProcessExit::Usage);
        assert_eq!(driver.build_calls, 0);
        assert_eq!(driver.run_calls, 0);
    }

    #[tokio::test]
    async fn admin_command_does_not_build_the_application() {
        let arguments =
            ProcessArguments::try_parse_from(["maincopy", "admin", "capabilities"]).unwrap();
        let mut driver = FakeDriver::succeeding();

        dispatch_with(arguments, &mut driver).await.unwrap();

        assert_eq!(driver.admin_calls, 1);
        assert_eq!(driver.build_calls, 0);
        assert_eq!(driver.run_calls, 0);
    }

    #[tokio::test]
    async fn tokio_runtime_exists_before_application_build() {
        let arguments = ProcessArguments::try_parse_from(["maincopy", "serve"]).unwrap();
        let mut driver = FakeDriver::succeeding();

        dispatch_with(arguments, &mut driver).await.unwrap();

        assert!(driver.runtime_seen);
        assert_eq!(driver.build_calls, 1);
        assert_eq!(driver.run_calls, 1);
    }

    #[tokio::test]
    async fn failed_build_stops_before_application_run() {
        let arguments = ProcessArguments::try_parse_from(["maincopy", "serve"]).unwrap();
        let mut driver = FakeDriver::failing();

        let error = dispatch_with(arguments, &mut driver).await.unwrap_err();

        assert!(matches!(
            error,
            ProcessError::Application(ApplicationError::Startup {
                stage: StartupStage::Configuration
            })
        ));
        assert_eq!(driver.build_calls, 1);
        assert_eq!(driver.run_calls, 0);
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
        let arguments = serve_arguments_for_with(
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
        assert_eq!(limits.publication_file_bytes().get(), 512);
        assert_eq!(limits.post_file_bytes().get(), 1536);
        assert_eq!(limits.asset_file_bytes().get(), 2560);
        assert_eq!(limits.total_tree_bytes().get(), 3584);
        assert_eq!(limits.entries().get(), 90);
        assert_eq!(limits.depth().get(), 7);
        assert_eq!(limits.path_bytes().get(), 400);
        assert!(
            startup
                ._content_tree
                .publication()
                .source()
                .contains("Pinned startup source")
        );
        assert_eq!(
            startup
                ._validated_content
                .publication()
                .site()
                .title()
                .as_str(),
            "Pinned startup source"
        );
    }

    #[test]
    fn host_configuration_failure_prevents_content_and_application_stages() {
        let root = tempfile::tempdir().unwrap();
        let arguments = serve_arguments_for(
            &root.path().join("missing-maincopy.toml"),
            &root.path().join("content"),
        );
        let discovery_calls = Cell::new(0);
        let application_build_calls = Cell::new(0);

        let result = build_application_with(
            arguments,
            |arguments| {
                StartupConfiguration::load_with_discovery(arguments, |_, _| {
                    discovery_calls.set(discovery_calls.get() + 1);
                    unreachable!("content discovery must follow host configuration")
                })
            },
            |_| {
                application_build_calls.set(application_build_calls.get() + 1);
                Ok(())
            },
        );

        assert!(matches!(result, Err(ProcessError::Configuration(_))));
        assert_eq!(discovery_calls.get(), 0);
        assert_eq!(application_build_calls.get(), 0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn content_validation_failure_has_stable_exit_and_prevents_application_stages() {
        let (_root, arguments, _) = startup_fixture("", "unknown = true\n");
        let discovery_calls = Cell::new(0);
        let application_build_calls = Cell::new(0);

        let result = build_application_with(
            arguments,
            |arguments| {
                StartupConfiguration::load_with_discovery(arguments, |root, limits| {
                    discovery_calls.set(discovery_calls.get() + 1);
                    discover_content_tree(root, limits)
                })
            },
            |_| {
                application_build_calls.set(application_build_calls.get() + 1);
                Ok(())
            },
        );

        let error = result.unwrap_err();
        assert!(matches!(error, ProcessError::Validation(_)));
        assert_eq!(error.exit(), ProcessExit::Validation);
        assert_eq!(discovery_calls.get(), 1);
        assert_eq!(application_build_calls.get(), 0);
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
            errors.diagnostics()[0].code(),
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

    #[test]
    fn signal_registration_failure_prevents_application_construction_and_readiness() {
        let readiness = Readiness::default();
        let mut installation_attempted = false;

        let result = ApplicationRuntime::build_with_signal_installer(readiness.clone(), || {
            installation_attempted = true;
            Err(ApplicationError::SignalRegistration {
                signal: ShutdownSignal::Terminate,
                source: std::io::Error::other("signal registration failed"),
            })
        });

        assert!(installation_attempted);
        assert!(matches!(
            result,
            Err(ApplicationError::SignalRegistration {
                signal: ShutdownSignal::Terminate,
                ..
            })
        ));
        assert!(!readiness.is_ready());
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
        tokio::task::yield_now().await;
        assert!(readiness.is_ready());

        assert!(shutdown_tx.send(()).is_ok());
        assert!(running.await.unwrap().is_ok());
        assert!(!readiness.is_ready());
        assert!(cancellation.is_cancelled());
        assert_eq!(drained.load(Ordering::SeqCst), 2);
        assert_eq!(observed_order.load(Ordering::SeqCst), 2);
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
