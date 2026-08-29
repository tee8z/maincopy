use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    task::{Context, Poll},
};

use clap::{Parser, error::ErrorKind};
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::{
    cli::{AdminCommand, ProcessArguments, ProcessCommand},
    error::{
        ApplicationError, CriticalTaskName, PanicMessage, ProcessComponent, ProcessError,
        ProcessExit, ShutdownSignal, UnavailableReason,
    },
    web::Readiness,
};

type ShutdownFuture = Pin<Box<dyn Future<Output = Result<(), ApplicationError>> + Send>>;
type CriticalTaskFuture = Pin<Box<dyn Future<Output = CriticalTaskResult> + Send>>;
type CriticalTaskResult = Result<(), CriticalTaskFailure>;

/// Owns Maincopy's process-level resources and lifecycle.
///
/// Configuration validation, dependency construction, listener binding, and
/// task creation belong in [`Application::build`]. Runtime supervision and
/// ordered shutdown belong in [`Application::run_until_stop`].
pub(crate) struct Application {
    readiness: Readiness,
    cancellation: CancellationToken,
    shutdown: ShutdownFuture,
    critical_tasks: JoinSet<(CriticalTaskName, CriticalTaskCompletion)>,
}

/// Parses one process command and runs it to completion.
pub async fn run_until_stop() -> ProcessExit {
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
            eprintln!("maincopy: {error}");
            error.exit()
        }
    }
}

fn report_command_error(error: clap::Error) -> ProcessExit {
    let exit = match error.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => ProcessExit::Success,
        _ => ProcessExit::Usage,
    };

    if let Err(print_error) = error.print() {
        eprintln!("maincopy: failed to print command output: {print_error}");
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
        ProcessCommand::Serve => {
            let application = driver.build_application().await?;
            driver.run_application(application).await
        }
        ProcessCommand::Admin(arguments) => driver.run_admin_command(arguments.command).await,
    }
}

trait ProcessDriver {
    type Application;

    async fn build_application(&mut self) -> Result<Self::Application, ProcessError>;

    async fn run_application(&mut self, application: Self::Application)
    -> Result<(), ProcessError>;

    async fn run_admin_command(&mut self, command: AdminCommand) -> Result<(), ProcessError>;
}

struct ProductionDriver;

impl ProcessDriver for ProductionDriver {
    type Application = Application;

    async fn build_application(&mut self) -> Result<Self::Application, ProcessError> {
        Application::build().map_err(ProcessError::from)
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
            AdminCommand::Capabilities => Err(ProcessError::Unavailable {
                component: ProcessComponent::AdminApi,
                reason: UnavailableReason::NotImplemented,
            }),
        }
    }
}

impl Application {
    fn build() -> Result<Self, ApplicationError> {
        Self::build_with_signal_installer(Readiness::default(), install_termination_signal)
    }

    fn build_with_signal_installer<Install>(
        readiness: Readiness,
        install_signal: Install,
    ) -> Result<Self, ApplicationError>
    where
        Install: FnOnce() -> Result<ShutdownFuture, ApplicationError>,
    {
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
            supervisor.spawn(SupervisedTask::new(task));
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
        Some(Ok((task, CriticalTaskCompletion::Returned(Ok(()))))) => {
            ApplicationError::CriticalTaskExited { task }
        }
        Some(Ok((task, CriticalTaskCompletion::Returned(Err(failure))))) => {
            ApplicationError::CriticalTaskFailed {
                task,
                source: failure.0,
            }
        }
        Some(Ok((task, CriticalTaskCompletion::Panicked(message)))) => {
            ApplicationError::CriticalTaskPanicked { task, message }
        }
        Some(Err(source)) => ApplicationError::TaskSupervisor { source },
        None => ApplicationError::TaskSupervisorEmpty,
    }
}

fn drained_task_failure(
    completion: Result<(CriticalTaskName, CriticalTaskCompletion), JoinError>,
) -> Option<ApplicationError> {
    match completion {
        Ok((_, CriticalTaskCompletion::Returned(Ok(())))) => None,
        Ok((task, CriticalTaskCompletion::Returned(Err(failure)))) => {
            Some(ApplicationError::CriticalTaskFailed {
                task,
                source: failure.0,
            })
        }
        Ok((task, CriticalTaskCompletion::Panicked(message))) => {
            Some(ApplicationError::CriticalTaskPanicked { task, message })
        }
        Err(source) => Some(ApplicationError::TaskSupervisor { source }),
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

struct CriticalTaskFailure(Box<dyn std::error::Error + Send + Sync>);

enum CriticalTaskCompletion {
    Returned(CriticalTaskResult),
    Panicked(PanicMessage),
}

struct SupervisedTask {
    name: CriticalTaskName,
    future: CriticalTaskFuture,
}

impl SupervisedTask {
    fn new(task: CriticalTask) -> Self {
        Self {
            name: task.name,
            future: task.future,
        }
    }
}

impl Future for SupervisedTask {
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
                CriticalTaskCompletion::Panicked(PanicMessage::from_payload(payload.as_ref())),
            )),
        }
    }
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
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use clap::Parser;
    use tokio::sync::oneshot;

    use super::*;
    use crate::error::StartupStage;

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

        async fn build_application(&mut self) -> Result<Self::Application, ProcessError> {
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
    fn signal_registration_failure_prevents_application_construction_and_readiness() {
        let readiness = Readiness::default();
        let mut installation_attempted = false;

        let result = Application::build_with_signal_installer(readiness.clone(), || {
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
        let application = Application::with_parts(
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
                Err(CriticalTaskFailure(Box::new(std::io::Error::other(
                    "task failed",
                ))))
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
        let application = Application::with_parts(
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
