//! Supervised client for the isolated Mermaid renderer helper.

use std::{
    env,
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Mutex, OnceLock},
    time::Instant,
};

#[cfg(target_os = "linux")]
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    process::{Pid, PidfdFlags, Signal, kill_process_group, pidfd_open, pidfd_send_signal},
};
#[cfg(target_os = "linux")]
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use thiserror::Error;

use crate::protocol::{
    DETERMINISTIC_ENVIRONMENT, HelperExit, MAX_RAW_SVG_BYTES, MAX_SOURCE_BYTES, MAX_WALL_TIME,
    PROTOCOL_VERSION,
};

const HELPER_ENVIRONMENT_VARIABLE: &str = "MAINCOPY_MERMAID_HELPER";
const FONTCONFIG: &str = "<fontconfig><dir>fonts</dir></fontconfig>\n";

/// A local Mermaid rendering capability backed by one supervised helper process at a time.
#[derive(Debug)]
pub struct MermaidRenderer {
    executable: PathBuf,
    slot: Mutex<()>,
    protocol_verified: OnceLock<()>,
}

impl MermaidRenderer {
    /// Locates the packaged helper next to `maincopyd`, unless an absolute test/operator path is set.
    pub fn discover() -> Result<Self, MermaidRenderError> {
        if let Some(configured) = env::var_os(HELPER_ENVIRONMENT_VARIABLE) {
            return Self::from_executable(configured);
        }
        let current = env::current_exe().map_err(MermaidRenderError::DiscoverExecutable)?;
        let mut directory = current
            .parent()
            .ok_or(MermaidRenderError::ExecutableHasNoParent)?;
        if directory.file_name() == Some(OsStr::new("deps")) {
            directory = directory
                .parent()
                .ok_or(MermaidRenderError::ExecutableHasNoParent)?;
        }
        let executable = directory.join(format!("maincopy-mermaid{}", env::consts::EXE_SUFFIX));
        Self::from_executable(executable)
    }

    /// Selects an explicit absolute helper path.
    pub fn from_executable(path: impl Into<PathBuf>) -> Result<Self, MermaidRenderError> {
        let executable = path.into();
        if !executable.is_absolute() {
            return Err(MermaidRenderError::ExecutablePathNotAbsolute);
        }
        Ok(Self {
            executable,
            slot: Mutex::new(()),
            protocol_verified: OnceLock::new(),
        })
    }

    /// Produces untrusted SVG bytes. The caller must pass them through its SVG policy validator.
    pub fn render(&self, source: &str) -> Result<RawMermaidSvg, MermaidRenderError> {
        if source.len() > MAX_SOURCE_BYTES {
            return Err(MermaidRenderError::SourceTooLarge);
        }
        let _slot = self
            .slot
            .lock()
            .map_err(|_| MermaidRenderError::ConcurrencyStatePoisoned)?;
        self.verify_protocol()?;

        let workspace = tempfile::Builder::new()
            .prefix("maincopy-mermaid-")
            .tempdir()
            .map_err(MermaidRenderError::CreateWorkspace)?;
        let input = workspace.path().join("source.mmd");
        let output = workspace.path().join("diagram.svg");
        let fontconfig = workspace.path().join("fonts.conf");
        let fonts = workspace.path().join("fonts");
        let cache = workspace.path().join("cache");
        fs::create_dir(&fonts).map_err(MermaidRenderError::PrepareEnvironment)?;
        fs::create_dir(&cache).map_err(MermaidRenderError::PrepareEnvironment)?;
        fs::write(&input, source.as_bytes()).map_err(MermaidRenderError::WriteInput)?;
        fs::write(&fontconfig, FONTCONFIG.as_bytes())
            .map_err(MermaidRenderError::PrepareEnvironment)?;
        validate_deterministic_path(&fontconfig)?;
        validate_deterministic_path(&cache)?;

        let mut command = self.base_command();
        command
            .arg(&input)
            .arg(&output)
            .current_dir(workspace.path())
            .env("MAINCOPY_MERMAID_ENVIRONMENT", DETERMINISTIC_ENVIRONMENT)
            .env("FONTCONFIG_FILE", &fontconfig)
            .env("XDG_CACHE_HOME", &cache)
            .env("LANG", "C.UTF-8")
            .env("TZ", "UTC");
        let status = run_to_completion(&mut command)?;
        classify_render_status(status)?;
        read_raw_svg(&output)
    }

    fn verify_protocol(&self) -> Result<(), MermaidRenderError> {
        if self.protocol_verified.get().is_some() {
            return Ok(());
        }
        let workspace = tempfile::Builder::new()
            .prefix("maincopy-mermaid-protocol-")
            .tempdir()
            .map_err(MermaidRenderError::CreateWorkspace)?;
        let output = workspace.path().join("protocol.txt");
        let mut command = self.base_command();
        command
            .arg("--protocol-version")
            .arg(&output)
            .current_dir(workspace.path());
        let status = run_to_completion(&mut command)?;
        if !status.success() {
            return Err(protocol_failure(HelperTermination::from_status(status)));
        }
        let mut file = File::open(&output).map_err(MermaidRenderError::OpenProtocolOutput)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(129)
            .read_to_end(&mut bytes)
            .map_err(MermaidRenderError::ReadProtocolOutput)?;
        if bytes.len() > 128 || bytes.as_slice() != format!("{PROTOCOL_VERSION}\n").as_bytes() {
            return Err(MermaidRenderError::ProtocolMismatch);
        }
        let _ = self.protocol_verified.set(());
        Ok(())
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(target_os = "linux")]
        command.process_group(0);
        command
    }
}

fn validate_deterministic_path(path: &Path) -> Result<(), MermaidRenderError> {
    path.to_str()
        .map(|_| ())
        .ok_or(MermaidRenderError::DeterministicPathNotUtf8)
}

/// Renderer output that has not crossed Maincopy's SVG trust boundary.
#[derive(Debug)]
pub struct RawMermaidSvg(String);

impl RawMermaidSvg {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn run_to_completion(command: &mut Command) -> Result<ExitStatus, MermaidRenderError> {
    let mut child = command.spawn().map_err(MermaidRenderError::SpawnHelper)?;
    wait_for_child(&mut child)
}

#[cfg(target_os = "linux")]
fn wait_for_child(child: &mut Child) -> Result<ExitStatus, MermaidRenderError> {
    wait_for_child_with_timeout(child, MAX_WALL_TIME)
}

#[cfg(target_os = "linux")]
fn wait_for_child_with_timeout(
    child: &mut Child,
    wall_time: std::time::Duration,
) -> Result<ExitStatus, MermaidRenderError> {
    let pid = match child_pid(child) {
        Ok(pid) => pid,
        Err(error) => return terminate_and_reap(child, None, None, error),
    };
    let pidfd = match pidfd_open(pid, PidfdFlags::empty()) {
        Ok(pidfd) => pidfd,
        Err(source) => {
            let error = MermaidRenderError::OpenProcessHandle(std::io::Error::from_raw_os_error(
                source.raw_os_error(),
            ));
            return match completed_or_error(child, error) {
                Ok(status) => Ok(status),
                Err(error) => terminate_and_reap(child, Some(pid), None, error),
            };
        }
    };
    match poll_child_until(child, &pidfd, wall_time) {
        Ok(status) => Ok(status),
        Err(error) => terminate_and_reap(child, Some(pid), Some(&pidfd), error),
    }
}

#[cfg(target_os = "linux")]
fn child_pid(child: &Child) -> Result<Pid, MermaidRenderError> {
    let raw_pid = i32::try_from(child.id()).map_err(|_| MermaidRenderError::InvalidChildId)?;
    Pid::from_raw(raw_pid).ok_or(MermaidRenderError::InvalidChildId)
}

#[cfg(target_os = "linux")]
fn poll_child_until(
    child: &mut Child,
    pidfd: &std::os::fd::OwnedFd,
    wall_time: std::time::Duration,
) -> Result<ExitStatus, MermaidRenderError> {
    let deadline = Instant::now()
        .checked_add(wall_time)
        .ok_or(MermaidRenderError::InvalidTimeout)?;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout =
            Timespec::try_from(remaining).map_err(|_| MermaidRenderError::InvalidTimeout)?;
        let mut descriptor = [PollFd::new(pidfd, PollFlags::IN)];
        match poll(&mut descriptor, Some(&timeout)) {
            Ok(0) => return completed_or_error(child, MermaidRenderError::TimedOut),
            Ok(_) => return child.wait().map_err(MermaidRenderError::WaitForHelper),
            Err(source) if source == rustix::io::Errno::INTR => continue,
            Err(source) => {
                return Err(MermaidRenderError::PollProcess(
                    std::io::Error::from_raw_os_error(source.raw_os_error()),
                ));
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn completed_or_error(
    child: &mut Child,
    error: MermaidRenderError,
) -> Result<ExitStatus, MermaidRenderError> {
    match child.try_wait() {
        Ok(Some(status)) => Ok(status),
        Ok(None) => Err(error),
        Err(source) => Err(MermaidRenderError::WaitForHelper(source)),
    }
}

#[cfg(not(target_os = "linux"))]
fn wait_for_child(child: &mut Child) -> Result<ExitStatus, MermaidRenderError> {
    terminate_and_reap(child, None, None, MermaidRenderError::UnsupportedPlatform)
}

#[cfg(target_os = "linux")]
fn terminate_and_reap(
    child: &mut Child,
    pid: Option<Pid>,
    pidfd: Option<&std::os::fd::OwnedFd>,
    result: MermaidRenderError,
) -> Result<ExitStatus, MermaidRenderError> {
    let group_result = pid.map(|pid| kill_process_group(pid, Signal::KILL));
    let terminate_result = if let Some(pidfd) = pidfd {
        pidfd_send_signal(pidfd, Signal::KILL)
            .map_err(|source| std::io::Error::from_raw_os_error(source.raw_os_error()))
    } else {
        child.kill()
    };
    if let Err(source) = terminate_result {
        match child.try_wait() {
            Ok(Some(_)) => return Err(result),
            Ok(None) | Err(_) => return Err(MermaidRenderError::TerminateHelper(source)),
        }
    }
    child.wait().map_err(MermaidRenderError::ReapHelper)?;
    if let Some(Err(source)) = group_result
        && source != rustix::io::Errno::SRCH
    {
        return Err(MermaidRenderError::TerminateProcessGroup(
            std::io::Error::from_raw_os_error(source.raw_os_error()),
        ));
    }
    Err(result)
}

#[cfg(not(target_os = "linux"))]
fn terminate_and_reap(
    child: &mut Child,
    _pid: Option<()>,
    _pidfd: Option<&std::fs::File>,
    result: MermaidRenderError,
) -> Result<ExitStatus, MermaidRenderError> {
    child.kill().map_err(MermaidRenderError::TerminateHelper)?;
    child.wait().map_err(MermaidRenderError::ReapHelper)?;
    Err(result)
}

fn classify_render_status(status: ExitStatus) -> Result<(), MermaidRenderError> {
    classify_render_termination(HelperTermination::from_status(status))
}

fn classify_render_termination(termination: HelperTermination) -> Result<(), MermaidRenderError> {
    match termination {
        HelperTermination::Exited(code) => match HelperExit::try_from(code) {
            Ok(HelperExit::Success) => Ok(()),
            Ok(HelperExit::InvalidDiagram) => Err(MermaidRenderError::InvalidDiagram),
            Ok(HelperExit::InputRejected) => Err(MermaidRenderError::SourceTooLarge),
            Ok(HelperExit::ResourceLimit) => Err(MermaidRenderError::ResourceLimit),
            Ok(HelperExit::Usage) => Err(MermaidRenderError::HelperUsage),
            Ok(HelperExit::CannotCreate) => Err(MermaidRenderError::HelperCannotCreateOutput),
            Ok(HelperExit::Io) => Err(MermaidRenderError::HelperIo),
            Ok(HelperExit::Internal) => Err(MermaidRenderError::HelperInternal),
            Err(()) => Err(MermaidRenderError::HelperUnknownExit { code }),
        },
        HelperTermination::Signaled { signal } if resource_signal(signal) => {
            Err(MermaidRenderError::HelperResourceSignal { signal })
        }
        HelperTermination::Signaled { signal } => Err(MermaidRenderError::HelperSignal { signal }),
        HelperTermination::Unknown => Err(MermaidRenderError::HelperTerminationUnknown),
    }
}

fn protocol_failure(termination: HelperTermination) -> MermaidRenderError {
    match termination {
        HelperTermination::Exited(code) => MermaidRenderError::ProtocolHelperExit { code },
        HelperTermination::Signaled { signal } => {
            MermaidRenderError::ProtocolHelperSignal { signal }
        }
        HelperTermination::Unknown => MermaidRenderError::ProtocolHelperTerminationUnknown,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HelperTermination {
    Exited(i32),
    Signaled { signal: i32 },
    Unknown,
}

impl HelperTermination {
    fn from_status(status: ExitStatus) -> Self {
        if let Some(code) = status.code() {
            return Self::Exited(code);
        }
        #[cfg(target_os = "linux")]
        if let Some(signal) = status.signal() {
            return Self::Signaled { signal };
        }
        Self::Unknown
    }
}

#[cfg(target_os = "linux")]
fn resource_signal(signal: i32) -> bool {
    // Rust's Linux stack-overflow handler aborts after the configured stack is
    // exhausted, so ABRT is part of the helper's resource-failure contract.
    [
        Signal::XCPU,
        Signal::XFSZ,
        Signal::KILL,
        Signal::SEGV,
        Signal::ABORT,
    ]
    .into_iter()
    .any(|candidate| candidate.as_raw() == signal)
}

#[cfg(not(target_os = "linux"))]
const fn resource_signal(_signal: i32) -> bool {
    false
}

fn read_raw_svg(path: &Path) -> Result<RawMermaidSvg, MermaidRenderError> {
    let mut file = File::open(path).map_err(MermaidRenderError::OpenOutput)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_RAW_SVG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(MermaidRenderError::ReadOutput)?;
    if bytes.len() > MAX_RAW_SVG_BYTES {
        return Err(MermaidRenderError::OutputTooLarge);
    }
    String::from_utf8(bytes)
        .map(RawMermaidSvg)
        .map_err(|_| MermaidRenderError::OutputIsNotUtf8)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MermaidRenderErrorCode {
    Unavailable,
    InvalidDiagram,
    ResourceLimit,
    TimedOut,
    InvalidOutput,
    Internal,
}

#[derive(Debug, Error)]
pub enum MermaidRenderError {
    #[error("could not discover the current executable")]
    DiscoverExecutable(#[source] std::io::Error),
    #[error("the current executable has no parent directory")]
    ExecutableHasNoParent,
    #[error("the Mermaid helper path is not absolute")]
    ExecutablePathNotAbsolute,
    #[error("the Mermaid helper concurrency state is unavailable")]
    ConcurrencyStatePoisoned,
    #[error("Mermaid source exceeds the renderer input limit")]
    SourceTooLarge,
    #[error("could not create the Mermaid renderer workspace")]
    CreateWorkspace(#[source] std::io::Error),
    #[error("could not prepare the deterministic Mermaid environment")]
    PrepareEnvironment(#[source] std::io::Error),
    #[error("the deterministic Mermaid environment path is not UTF-8")]
    DeterministicPathNotUtf8,
    #[error("could not write Mermaid source")]
    WriteInput(#[source] std::io::Error),
    #[error("could not start the Mermaid helper")]
    SpawnHelper(#[source] std::io::Error),
    #[error("the Mermaid helper child identifier is invalid")]
    InvalidChildId,
    #[error("could not open a stable Mermaid helper process handle")]
    OpenProcessHandle(#[source] std::io::Error),
    #[error("could not wait for the Mermaid helper")]
    WaitForHelper(#[source] std::io::Error),
    #[error("could not poll the Mermaid helper")]
    PollProcess(#[source] std::io::Error),
    #[error("could not terminate the Mermaid helper")]
    TerminateHelper(#[source] std::io::Error),
    #[error("could not terminate the Mermaid helper process group")]
    TerminateProcessGroup(#[source] std::io::Error),
    #[error("could not reap the Mermaid helper")]
    ReapHelper(#[source] std::io::Error),
    #[error("the Mermaid helper timeout could not be represented")]
    InvalidTimeout,
    #[error("this platform cannot enforce the Mermaid helper deadline")]
    UnsupportedPlatform,
    #[error("the Mermaid helper exceeded its wall-clock deadline")]
    TimedOut,
    #[error("the Mermaid helper protocol process exited with code {code}")]
    ProtocolHelperExit { code: i32 },
    #[error("the Mermaid helper protocol process was stopped by signal {signal}")]
    ProtocolHelperSignal { signal: i32 },
    #[error("the Mermaid helper protocol process termination status is unavailable")]
    ProtocolHelperTerminationUnknown,
    #[error("could not open the Mermaid helper protocol output")]
    OpenProtocolOutput(#[source] std::io::Error),
    #[error("could not read the Mermaid helper protocol output")]
    ReadProtocolOutput(#[source] std::io::Error),
    #[error("the Mermaid helper protocol version does not match")]
    ProtocolMismatch,
    #[error("the Mermaid diagram is invalid or unsupported")]
    InvalidDiagram,
    #[error("the Mermaid helper exceeded a resource limit")]
    ResourceLimit,
    #[error("the Mermaid helper rejected Maincopy's fixed invocation")]
    HelperUsage,
    #[error("the Mermaid helper could not create its private output")]
    HelperCannotCreateOutput,
    #[error("the Mermaid helper reported an I/O failure")]
    HelperIo,
    #[error("the Mermaid helper reported an internal failure")]
    HelperInternal,
    #[error("the Mermaid helper exited with unknown code {code}")]
    HelperUnknownExit { code: i32 },
    #[error("the Mermaid helper was stopped by resource-related signal {signal}")]
    HelperResourceSignal { signal: i32 },
    #[error("the Mermaid helper was stopped by signal {signal}")]
    HelperSignal { signal: i32 },
    #[error("the Mermaid helper termination status is unavailable")]
    HelperTerminationUnknown,
    #[error("the Mermaid helper did not create its output")]
    OpenOutput(#[source] std::io::Error),
    #[error("could not read Mermaid helper output")]
    ReadOutput(#[source] std::io::Error),
    #[error("Mermaid helper output exceeds the byte limit")]
    OutputTooLarge,
    #[error("Mermaid helper output is not UTF-8")]
    OutputIsNotUtf8,
}

impl MermaidRenderError {
    pub const fn code(&self) -> MermaidRenderErrorCode {
        match self {
            Self::DiscoverExecutable(_)
            | Self::ExecutableHasNoParent
            | Self::ExecutablePathNotAbsolute
            | Self::SpawnHelper(_)
            | Self::UnsupportedPlatform
            | Self::ProtocolHelperExit { .. }
            | Self::ProtocolHelperSignal { .. }
            | Self::ProtocolHelperTerminationUnknown
            | Self::OpenProtocolOutput(_)
            | Self::ReadProtocolOutput(_)
            | Self::ProtocolMismatch => MermaidRenderErrorCode::Unavailable,
            Self::SourceTooLarge | Self::ResourceLimit | Self::HelperResourceSignal { .. } => {
                MermaidRenderErrorCode::ResourceLimit
            }
            Self::TimedOut => MermaidRenderErrorCode::TimedOut,
            Self::InvalidDiagram => MermaidRenderErrorCode::InvalidDiagram,
            Self::OpenOutput(_)
            | Self::ReadOutput(_)
            | Self::OutputTooLarge
            | Self::OutputIsNotUtf8 => MermaidRenderErrorCode::InvalidOutput,
            Self::ConcurrencyStatePoisoned
            | Self::CreateWorkspace(_)
            | Self::PrepareEnvironment(_)
            | Self::DeterministicPathNotUtf8
            | Self::WriteInput(_)
            | Self::InvalidChildId
            | Self::OpenProcessHandle(_)
            | Self::WaitForHelper(_)
            | Self::PollProcess(_)
            | Self::TerminateHelper(_)
            | Self::TerminateProcessGroup(_)
            | Self::ReapHelper(_)
            | Self::InvalidTimeout
            | Self::HelperUsage
            | Self::HelperCannotCreateOutput
            | Self::HelperIo
            | Self::HelperInternal
            | Self::HelperUnknownExit { .. }
            | Self::HelperSignal { .. }
            | Self::HelperTerminationUnknown => MermaidRenderErrorCode::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_paths_must_be_absolute() {
        let error = MermaidRenderer::from_executable("relative/helper").unwrap_err();
        assert!(matches!(
            error,
            MermaidRenderError::ExecutablePathNotAbsolute
        ));
    }

    #[test]
    fn helper_exit_codes_map_without_parsing_process_text() {
        for (exit, expected) in [
            (
                HelperExit::InvalidDiagram,
                MermaidRenderErrorCode::InvalidDiagram,
            ),
            (
                HelperExit::InputRejected,
                MermaidRenderErrorCode::ResourceLimit,
            ),
            (
                HelperExit::ResourceLimit,
                MermaidRenderErrorCode::ResourceLimit,
            ),
            (HelperExit::Internal, MermaidRenderErrorCode::Internal),
            (HelperExit::Usage, MermaidRenderErrorCode::Internal),
            (HelperExit::CannotCreate, MermaidRenderErrorCode::Internal),
            (HelperExit::Io, MermaidRenderErrorCode::Internal),
        ] {
            let error =
                classify_render_termination(HelperTermination::Exited(exit as i32)).unwrap_err();
            assert_eq!(error.code(), expected);
        }
        assert!(
            classify_render_termination(HelperTermination::Exited(HelperExit::Success as i32))
                .is_ok()
        );
        let unknown = classify_render_termination(HelperTermination::Exited(127)).unwrap_err();
        assert!(matches!(
            unknown,
            MermaidRenderError::HelperUnknownExit { code: 127 }
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resource_signals_have_a_stable_public_class() {
        for signal in [
            Signal::XCPU,
            Signal::XFSZ,
            Signal::KILL,
            Signal::SEGV,
            Signal::ABORT,
        ] {
            let error = classify_render_termination(HelperTermination::Signaled {
                signal: signal.as_raw(),
            })
            .unwrap_err();
            assert_eq!(error.code(), MermaidRenderErrorCode::ResourceLimit);
        }
        let error = classify_render_termination(HelperTermination::Signaled {
            signal: Signal::TERM.as_raw(),
        })
        .unwrap_err();
        assert_eq!(error.code(), MermaidRenderErrorCode::Internal);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_cannot_disable_the_deterministic_font_environment() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let path = PathBuf::from(OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]));
        assert!(matches!(
            validate_deterministic_path(&path),
            Err(MermaidRenderError::DeterministicPathNotUtf8)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn child_completion_before_the_deadline_is_observed() {
        const FIXTURE_ENV: &str = "MAINCOPY_MERMAID_COMPLETION_FIXTURE";
        if env::var_os(FIXTURE_ENV).is_some() {
            return;
        }
        let executable = env::current_exe().expect("test executable must be discoverable");
        let test_name = "client::tests::child_completion_before_the_deadline_is_observed";
        let mut child = Command::new(executable)
            .arg("--exact")
            .arg(test_name)
            .env(FIXTURE_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("fixture child must start");

        let status = wait_for_child_with_timeout(&mut child, std::time::Duration::from_secs(5))
            .expect("fixture child must finish before the deadline");

        assert!(status.success());
        assert!(child.try_wait().unwrap().is_some(), "child must be reaped");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wall_deadline_kills_and_reaps_the_exact_child() {
        const FIXTURE_ENV: &str = "MAINCOPY_MERMAID_WAIT_FIXTURE";
        if env::var_os(FIXTURE_ENV).is_some() {
            std::thread::park();
            return;
        }
        let executable = env::current_exe().expect("test executable must be discoverable");
        let test_name = "client::tests::wall_deadline_kills_and_reaps_the_exact_child";
        let mut child = Command::new(executable)
            .arg("--exact")
            .arg(test_name)
            .env(FIXTURE_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("fixture child must start");

        let error = wait_for_child_with_timeout(&mut child, std::time::Duration::from_millis(25))
            .unwrap_err();

        assert!(matches!(error, MermaidRenderError::TimedOut));
        assert!(child.try_wait().unwrap().is_some(), "child must be reaped");
    }
}
