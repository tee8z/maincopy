use std::{fmt, process::Termination};

use markdown_compiler::ContentValidationErrors;
use thiserror::Error;

use crate::config::ConfigurationErrors;

macro_rules! impl_display {
    ($name:ty { $($pattern:pat => $value:literal),+ $(,)? }) => {
        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(match self {
                    $($pattern => $value),+
                })
            }
        }
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ProcessExit {
    Success = 0,
    Usage = 2,
    Validation = 65,
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
    #[error(transparent)]
    Configuration(#[from] ConfigurationErrors),

    #[error(transparent)]
    Validation(#[from] ContentValidationErrors),

    #[error("another process already owns a required Maincopy resource")]
    AlreadyRunning,

    #[error("identity bootstrap is already complete")]
    IdentityAlreadyBootstrapped,

    #[error("owner credential input is invalid")]
    IdentityCredentialInvalid,

    #[error(transparent)]
    Application(#[from] ApplicationError),
}

impl ProcessError {
    pub const fn exit(&self) -> ProcessExit {
        match self {
            Self::Configuration(_) => ProcessExit::Configuration,
            Self::Validation(_) | Self::IdentityCredentialInvalid => ProcessExit::Validation,
            Self::AlreadyRunning | Self::IdentityAlreadyBootstrapped => ProcessExit::Conflict,
            Self::Application(_) => ProcessExit::Internal,
        }
    }

    pub const fn category(&self) -> &'static str {
        match self {
            Self::Configuration(_) => "configuration",
            Self::Validation(_) | Self::IdentityCredentialInvalid => "validation",
            Self::AlreadyRunning | Self::IdentityAlreadyBootstrapped => "conflict",
            Self::Application(_) => "internal",
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
        message: Box<str>,
    },

    #[error("the critical task supervisor failed")]
    TaskSupervisor {
        #[source]
        source: tokio::task::JoinError,
    },

    #[error("the critical task supervisor became empty unexpectedly")]
    TaskSupervisorEmpty,

    #[error(
        "application construction failed during {stage} while attempting to {operation}: {source}"
    )]
    Startup {
        stage: StartupStage,
        operation: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownSignal {
    Interrupt,
    Terminate,
}

impl_display!(ShutdownSignal {
    Self::Interrupt => "interrupt",
    Self::Terminate => "terminate",
});

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CriticalTaskName {
    PublicServer,
    AdminServer,
    ContentSync,
    PublicationCoordinator,
    DatabaseWriter,
    Scheduler,
    Worker,
}

impl_display!(CriticalTaskName {
    Self::PublicServer => "public server",
    Self::AdminServer => "admin server",
    Self::ContentSync => "content sync",
    Self::PublicationCoordinator => "publication coordinator",
    Self::DatabaseWriter => "database writer",
    Self::Scheduler => "scheduler",
    Self::Worker => "worker",
});

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupStage {
    Configuration,
    ProcessLock,
    Database,
    Identity,
    Content,
    FrontendAssets,
    Listeners,
}

impl_display!(StartupStage {
    Self::Configuration => "configuration",
    Self::ProcessLock => "process lock",
    Self::Database => "database startup",
    Self::Identity => "identity bootstrap",
    Self::Content => "content compilation",
    Self::FrontendAssets => "frontend asset validation",
    Self::Listeners => "listener binding",
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigurationDiagnostic, ConfigurationValidationCode};

    fn configuration_errors() -> ConfigurationErrors {
        ConfigurationErrors::from_diagnostics(vec![ConfigurationDiagnostic::new(
            "$document",
            ConfigurationValidationCode::HostTomlInvalid,
            "host TOML does not match the schema",
        )])
    }

    fn validation_errors() -> ContentValidationErrors {
        let root = tempfile::tempdir().unwrap();
        markdown_compiler::discover_content_tree(
            &root.path().join("missing"),
            markdown_compiler::ContentTreeLimits::default(),
        )
        .unwrap_err()
    }

    #[test]
    fn server_errors_have_stable_exit_codes() {
        let cases = [
            (
                ProcessError::Configuration(configuration_errors()),
                ProcessExit::Configuration,
            ),
            (
                ProcessError::Validation(validation_errors()),
                ProcessExit::Validation,
            ),
            (ProcessError::AlreadyRunning, ProcessExit::Conflict),
            (
                ProcessError::IdentityAlreadyBootstrapped,
                ProcessExit::Conflict,
            ),
            (
                ProcessError::IdentityCredentialInvalid,
                ProcessExit::Validation,
            ),
            (
                ProcessError::Application(ApplicationError::Startup {
                    stage: StartupStage::Configuration,
                    operation: "load test configuration",
                    source: Box::new(std::io::Error::other("test startup failure")),
                }),
                ProcessExit::Internal,
            ),
        ];

        for (error, expected_exit) in cases {
            assert_eq!(error.exit(), expected_exit);
        }
    }

    #[test]
    fn server_exit_values_follow_the_documented_contract() {
        assert_eq!(ProcessExit::Success.code(), 0);
        assert_eq!(ProcessExit::Usage.code(), 2);
        assert_eq!(ProcessExit::Validation.code(), 65);
        assert_eq!(ProcessExit::Internal.code(), 70);
        assert_eq!(ProcessExit::Conflict.code(), 75);
        assert_eq!(ProcessExit::Configuration.code(), 78);
    }

    #[test]
    fn server_errors_have_stable_log_categories() {
        let cases = [
            (
                ProcessError::Configuration(configuration_errors()),
                "configuration",
            ),
            (ProcessError::Validation(validation_errors()), "validation"),
            (ProcessError::AlreadyRunning, "conflict"),
            (ProcessError::IdentityAlreadyBootstrapped, "conflict"),
            (ProcessError::IdentityCredentialInvalid, "validation"),
            (
                ProcessError::Application(ApplicationError::Startup {
                    stage: StartupStage::Configuration,
                    operation: "load test configuration",
                    source: Box::new(std::io::Error::other("test startup failure")),
                }),
                "internal",
            ),
        ];

        for (error, category) in cases {
            assert_eq!(error.category(), category);
        }
    }

    #[test]
    fn error_component_names_have_stable_text() {
        macro_rules! assert_display {
            ($($value:expr => $expected:literal),+ $(,)?) => {
                $(assert_eq!($value.to_string(), $expected);)+
            };
        }

        assert_display! {
            ProcessError::AlreadyRunning =>
                "another process already owns a required Maincopy resource",
            ProcessError::IdentityAlreadyBootstrapped =>
                "identity bootstrap is already complete",
            ProcessError::IdentityCredentialInvalid =>
                "owner credential input is invalid",
            ShutdownSignal::Interrupt => "interrupt",
            ShutdownSignal::Terminate => "terminate",
            CriticalTaskName::PublicServer => "public server",
            CriticalTaskName::AdminServer => "admin server",
            CriticalTaskName::ContentSync => "content sync",
            CriticalTaskName::PublicationCoordinator => "publication coordinator",
            CriticalTaskName::DatabaseWriter => "database writer",
            CriticalTaskName::Scheduler => "scheduler",
            CriticalTaskName::Worker => "worker",
            StartupStage::Configuration => "configuration",
            StartupStage::ProcessLock => "process lock",
            StartupStage::Database => "database startup",
            StartupStage::Identity => "identity bootstrap",
            StartupStage::Content => "content compilation",
            StartupStage::FrontendAssets => "frontend asset validation",
            StartupStage::Listeners => "listener binding",
        }
    }
}
