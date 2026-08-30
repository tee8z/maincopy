use std::{fmt, process::Termination};

use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ProcessExit {
    Success = 0,
    Usage = 2,
    Unavailable = 69,
    Internal = 70,
    Conflict = 75,
    Configuration = 78,
}

impl ProcessExit {
    pub const fn code(self) -> u8 {
        self as u8
    }
}

impl Termination for ProcessExit {
    fn report(self) -> std::process::ExitCode {
        std::process::ExitCode::from(self.code())
    }
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("application configuration is invalid")]
    Configuration,

    #[error("{component} is unavailable: {reason}")]
    Unavailable {
        component: ProcessComponent,
        reason: UnavailableReason,
    },

    #[error("the requested operation conflicts with current {resource} state")]
    Conflict { resource: ConflictResource },

    #[error(transparent)]
    Application(#[from] ApplicationError),

    #[error("an internal process error occurred")]
    Internal,
}

impl ProcessError {
    pub const fn exit(&self) -> ProcessExit {
        match self {
            Self::Configuration => ProcessExit::Configuration,
            Self::Unavailable { .. } => ProcessExit::Unavailable,
            Self::Conflict { .. } => ProcessExit::Conflict,
            Self::Application(_) | Self::Internal => ProcessExit::Internal,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessComponent {
    AdminApi,
}

impl fmt::Display for ProcessComponent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AdminApi => formatter.write_str("the admin API"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    NotImplemented,
}

impl fmt::Display for UnavailableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotImplemented => {
                formatter.write_str("the client transport is not implemented yet")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictResource {
    Application,
}

impl fmt::Display for ConflictResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Application => formatter.write_str("application"),
        }
    }
}

#[derive(Debug, Error)]
pub enum ApplicationError {
    #[error("failed to register the {signal} shutdown signal: {source}")]
    SignalRegistration {
        signal: ShutdownSignal,
        #[source]
        source: std::io::Error,
    },

    #[error("the {signal} shutdown signal stream closed")]
    SignalStreamClosed { signal: ShutdownSignal },

    #[error("this platform does not provide a supported shutdown signal stream")]
    SignalPlatformUnsupported,

    #[error("critical task {task} exited unexpectedly")]
    CriticalTaskExited { task: CriticalTaskName },

    #[error("critical task {task} failed: {source}")]
    CriticalTaskFailed {
        task: CriticalTaskName,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("critical task {task} panicked: {message}")]
    CriticalTaskPanicked {
        task: CriticalTaskName,
        message: PanicMessage,
    },

    #[error("the critical task supervisor failed")]
    TaskSupervisor {
        #[source]
        source: tokio::task::JoinError,
    },

    #[error("the critical task supervisor became empty unexpectedly")]
    TaskSupervisorEmpty,

    #[error("application construction failed during {stage}")]
    Startup { stage: StartupStage },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownSignal {
    Interrupt,
    Terminate,
}

impl fmt::Display for ShutdownSignal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Interrupt => formatter.write_str("interrupt"),
            Self::Terminate => formatter.write_str("terminate"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CriticalTaskName {
    PublicServer,
    AdminServer,
    DatabaseWriter,
    Scheduler,
    Worker,
    Payments,
}

impl fmt::Display for CriticalTaskName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PublicServer => formatter.write_str("public server"),
            Self::AdminServer => formatter.write_str("admin server"),
            Self::DatabaseWriter => formatter.write_str("database writer"),
            Self::Scheduler => formatter.write_str("scheduler"),
            Self::Worker => formatter.write_str("worker"),
            Self::Payments => formatter.write_str("payments actor"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupStage {
    Configuration,
    ProcessLock,
    Database,
    Content,
    FrontendAssets,
    Payments,
    Listeners,
}

impl fmt::Display for StartupStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration => formatter.write_str("configuration"),
            Self::ProcessLock => formatter.write_str("process lock"),
            Self::Database => formatter.write_str("database startup"),
            Self::Content => formatter.write_str("content compilation"),
            Self::FrontendAssets => formatter.write_str("frontend asset validation"),
            Self::Payments => formatter.write_str("payment startup"),
            Self::Listeners => formatter.write_str("listener binding"),
        }
    }
}

#[derive(Debug)]
pub struct PanicMessage(String);

impl PanicMessage {
    pub(crate) fn from_payload(payload: &(dyn std::any::Any + Send)) -> Self {
        if let Some(message) = payload.downcast_ref::<String>() {
            return Self(message.clone());
        }

        if let Some(message) = payload.downcast_ref::<&'static str>() {
            return Self((*message).to_owned());
        }

        Self("non-string panic payload".to_owned())
    }
}

impl fmt::Display for PanicMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_error_categories_have_stable_exit_codes() {
        let cases = [
            (ProcessError::Configuration, ProcessExit::Configuration),
            (
                ProcessError::Unavailable {
                    component: ProcessComponent::AdminApi,
                    reason: UnavailableReason::NotImplemented,
                },
                ProcessExit::Unavailable,
            ),
            (
                ProcessError::Conflict {
                    resource: ConflictResource::Application,
                },
                ProcessExit::Conflict,
            ),
            (
                ProcessError::Application(ApplicationError::Startup {
                    stage: StartupStage::Configuration,
                }),
                ProcessExit::Internal,
            ),
            (ProcessError::Internal, ProcessExit::Internal),
        ];

        for (error, expected_exit) in cases {
            assert_eq!(error.exit(), expected_exit);
        }
    }

    #[test]
    fn process_exit_values_follow_the_documented_cli_contract() {
        assert_eq!(ProcessExit::Success.code(), 0);
        assert_eq!(ProcessExit::Usage.code(), 2);
        assert_eq!(ProcessExit::Unavailable.code(), 69);
        assert_eq!(ProcessExit::Internal.code(), 70);
        assert_eq!(ProcessExit::Conflict.code(), 75);
        assert_eq!(ProcessExit::Configuration.code(), 78);
    }
}
