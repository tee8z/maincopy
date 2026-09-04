use std::{
    io::{Read, Write as _},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::Duration,
};

use rustix::{
    io::retry_on_intr,
    process::{Pid, Signal, WaitId, WaitIdOptions, kill_process, waitid},
};

const SERVER_START_LIMIT: Duration = Duration::from_secs(45);
const SERVER_STOP_LIMIT: Duration = Duration::from_secs(20);
const FORCED_STOP_LIMIT: Duration = Duration::from_secs(5);
const MAX_CAPTURED_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_DAEMON_LOG_LINE_BYTES: usize = 8 * 1024;
const PUBLIC_READY_MESSAGE: &str = "public listener bound";
const ADMIN_READY_MESSAGE: &str = "authenticated admin backend listener bound";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DaemonAddresses {
    pub(super) public: SocketAddr,
    pub(super) admin: SocketAddr,
}

pub(super) struct CapturedChild {
    child: Child,
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    stopped: bool,
}

impl CapturedChild {
    pub(super) fn new(child: Child) -> Self {
        let mut process = Self {
            child,
            stdout: None,
            stderr: None,
            stopped: false,
        };
        let stdout = process
            .child
            .stdout
            .take()
            .expect("captured child stdout must be piped");
        process.stdout = Some(capture_output(stdout));
        let stderr = process
            .child
            .stderr
            .take()
            .expect("captured child stderr must be piped");
        process.stderr = Some(capture_output(stderr));
        process
    }

    pub(super) fn write_stdin(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.child
            .stdin
            .take()
            .expect("captured child stdin must be piped")
            .write_all(bytes)
    }

    pub(super) fn wait(mut self, limit: Duration) -> (ProcessCompletion, Vec<u8>, Vec<u8>) {
        let completion = wait_for_child(&mut self.child, limit);
        self.stopped = true;
        let (stdout, stderr) = self.join_output();
        (completion, stdout, stderr)
    }

    fn force_stop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = wait_for_child(&mut self.child, FORCED_STOP_LIMIT);
        self.stopped = true;
        let _ = self.join_output();
    }

    fn join_output(&mut self) -> (Vec<u8>, Vec<u8>) {
        let stdout = self.stdout.take().map(join_output).unwrap_or_default();
        let stderr = self.stderr.take().map(join_output).unwrap_or_default();
        (stdout, stderr)
    }
}

impl Drop for CapturedChild {
    fn drop(&mut self) {
        if !self.stopped {
            self.force_stop();
        }
    }
}

pub(super) struct ProcessCompletion {
    pub(super) status: Result<ExitStatus, Box<str>>,
    pub(super) timed_out: bool,
    pub(super) wait_error: Option<Box<str>>,
    pub(super) termination_error: Option<Box<str>>,
}

fn wait_for_child(child: &mut Child, limit: Duration) -> ProcessCompletion {
    match child.try_wait() {
        Ok(Some(status)) => {
            return ProcessCompletion {
                status: Ok(status),
                timed_out: false,
                wait_error: None,
                termination_error: None,
            };
        }
        Ok(None) => {}
        Err(error) => {
            return kill_and_reap(child, false, Some(error.to_string().into_boxed_str()));
        }
    }

    let pid = Pid::from_child(child);
    let (observed_tx, observed_rx) = mpsc::sync_channel(1);
    let observer = thread::spawn(move || {
        let observed = observe_child_exit(pid);
        let _ = observed_tx.send(observed);
    });
    match observed_rx.recv_timeout(limit) {
        Ok(Ok(())) => {
            let observer_error = observer
                .join()
                .err()
                .map(|_| "child exit observer panicked".into());
            ProcessCompletion {
                status: child
                    .wait()
                    .map_err(|error| error.to_string().into_boxed_str()),
                timed_out: false,
                wait_error: observer_error,
                termination_error: None,
            }
        }
        Ok(Err(error)) => {
            let observer_error = observer
                .join()
                .err()
                .map(|_| "child exit observer panicked".into());
            kill_and_reap(child, false, observer_error.or(Some(error)))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let completion = kill_and_reap(child, true, None);
            let _ = observer.join();
            completion
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let observer_error = observer
                .join()
                .err()
                .map(|_| "child exit observer panicked".into())
                .or_else(|| Some("child exit observer disconnected".into()));
            kill_and_reap(child, false, observer_error)
        }
    }
}

fn observe_child_exit(pid: Pid) -> Result<(), Box<str>> {
    match retry_on_intr(|| {
        waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        )
    }) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err("child exit observer returned without an exit".into()),
        Err(error) => Err(error.to_string().into_boxed_str()),
    }
}

fn kill_and_reap(
    child: &mut Child,
    timed_out: bool,
    wait_error: Option<Box<str>>,
) -> ProcessCompletion {
    let termination_error = child
        .kill()
        .err()
        .map(|error| error.to_string().into_boxed_str());
    let status = child
        .wait()
        .map_err(|error| error.to_string().into_boxed_str());
    ProcessCompletion {
        status,
        timed_out,
        wait_error,
        termination_error,
    }
}

fn capture_output<Reader>(mut reader: Reader) -> JoinHandle<Vec<u8>>
where
    Reader: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut captured = Vec::new();
        let mut buffer = [0_u8; 4 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => append_captured_bytes(&mut captured, &buffer[..count]),
                Err(error) => {
                    append_captured_bytes(
                        &mut captured,
                        format!("\nfailed to read child output: {error}\n").as_bytes(),
                    );
                    break;
                }
            }
        }
        captured
    })
}

fn append_captured_bytes(captured: &mut Vec<u8>, bytes: &[u8]) {
    let remaining = MAX_CAPTURED_OUTPUT_BYTES.saturating_sub(captured.len());
    captured.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
}

fn join_output(reader: JoinHandle<Vec<u8>>) -> Vec<u8> {
    reader
        .join()
        .unwrap_or_else(|_| b"child output reader panicked".to_vec())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Listener {
    Public,
    Admin,
}

fn listener_address_from_ready_line(
    line: &str,
) -> Result<Option<(Listener, SocketAddr)>, &'static str> {
    let listener = if line.contains(ADMIN_READY_MESSAGE) {
        Listener::Admin
    } else if line.contains(PUBLIC_READY_MESSAGE) {
        Listener::Public
    } else {
        return Ok(None);
    };
    let encoded = line
        .split_whitespace()
        .find_map(|field| field.strip_prefix("bind="))
        .ok_or("listener readiness log omitted its bound address")?;
    let address = encoded
        .parse::<SocketAddr>()
        .map_err(|_| "listener readiness log contained an invalid bound address")?;
    if address.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) || address.port() == 0 {
        return Err("listener readiness log contained an unsafe bound address");
    }
    Ok(Some((listener, address)))
}

#[derive(Default)]
struct PendingAddresses {
    public: Option<SocketAddr>,
    admin: Option<SocketAddr>,
}

impl PendingAddresses {
    fn observe(&mut self, listener: Listener, address: SocketAddr) -> Result<(), &'static str> {
        let slot = match listener {
            Listener::Public => &mut self.public,
            Listener::Admin => &mut self.admin,
        };
        match *slot {
            Some(previous) if previous != address => {
                Err("listener readiness log changed its bound address")
            }
            Some(_) => Ok(()),
            None => {
                *slot = Some(address);
                Ok(())
            }
        }
    }

    fn complete(&self) -> Option<DaemonAddresses> {
        Some(DaemonAddresses {
            public: self.public?,
            admin: self.admin?,
        })
    }
}

fn drain_daemon_stderr<Reader>(
    mut reader: Reader,
    ready_tx: mpsc::SyncSender<Result<DaemonAddresses, Box<str>>>,
) -> Vec<u8>
where
    Reader: Read,
{
    let mut ready_tx = Some(ready_tx);
    let mut pending = Some(PendingAddresses::default());
    let mut captured = Vec::new();
    let mut line = Vec::with_capacity(MAX_DAEMON_LOG_LINE_BYTES);
    let mut line_exceeded_limit = false;
    let mut buffer = [0_u8; 4 * 1024];

    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                if !line.is_empty() && !line_exceeded_limit {
                    observe_daemon_ready_line(&line, &mut pending, &mut ready_tx);
                }
                if let Some(sender) = ready_tx.take() {
                    let _ = sender.send(Err(
                        "daemon exited before binding its public and admin listeners".into(),
                    ));
                }
                break;
            }
            Ok(count) => {
                append_captured_bytes(&mut captured, &buffer[..count]);
                for &byte in &buffer[..count] {
                    if byte == b'\n' {
                        if !line_exceeded_limit {
                            observe_daemon_ready_line(&line, &mut pending, &mut ready_tx);
                        }
                        line.clear();
                        line_exceeded_limit = false;
                    } else if !line_exceeded_limit {
                        if line.len() < MAX_DAEMON_LOG_LINE_BYTES {
                            line.push(byte);
                        } else {
                            line.clear();
                            line_exceeded_limit = true;
                            if let Some(sender) = ready_tx.take() {
                                let _ = sender.send(Err(
                                    format!(
                                        "daemon stderr line exceeded {MAX_DAEMON_LOG_LINE_BYTES} bytes before readiness"
                                    )
                                    .into_boxed_str(),
                                ));
                            }
                        }
                    }
                }
            }
            Err(error) => {
                append_captured_bytes(
                    &mut captured,
                    format!("\nfailed to read daemon stderr: {error}\n").as_bytes(),
                );
                if let Some(sender) = ready_tx.take() {
                    let _ = sender.send(Err(format!(
                        "failed to read daemon stderr before readiness: {error}"
                    )
                    .into_boxed_str()));
                }
                break;
            }
        }
    }

    captured
}

fn observe_daemon_ready_line(
    line: &[u8],
    pending: &mut Option<PendingAddresses>,
    ready_tx: &mut Option<mpsc::SyncSender<Result<DaemonAddresses, Box<str>>>>,
) {
    let (Some(sender), Some(addresses)) = (ready_tx.as_ref(), pending.as_mut()) else {
        return;
    };
    let line = String::from_utf8_lossy(line);
    let result = match listener_address_from_ready_line(line.trim_end_matches('\r')) {
        Ok(None) => return,
        Ok(Some((listener, address))) => addresses.observe(listener, address),
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        let _ = sender.send(Err(error.into()));
        *ready_tx = None;
        *pending = None;
        return;
    }
    if let Some(addresses) = addresses.complete() {
        let _ = sender.send(Ok(addresses));
        *ready_tx = None;
        *pending = None;
    }
}

pub(super) struct Daemon {
    child: Child,
    stderr: Option<JoinHandle<Vec<u8>>>,
    stopped: bool,
}

impl Daemon {
    pub(super) fn start(mut command: Command) -> (Self, DaemonAddresses) {
        let child = command
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("integration daemon must start");
        let mut daemon = Self {
            child,
            stderr: None,
            stopped: false,
        };
        let stderr = daemon
            .child
            .stderr
            .take()
            .expect("integration daemon stderr must be captured");
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<DaemonAddresses, Box<str>>>(1);
        daemon.stderr = Some(thread::spawn(move || drain_daemon_stderr(stderr, ready_tx)));
        match ready_rx.recv_timeout(SERVER_START_LIMIT) {
            Ok(Ok(addresses)) => (daemon, addresses),
            Ok(Err(message)) => {
                let logs = daemon.force_stop();
                panic!("integration daemon did not become ready: {message}: {logs}");
            }
            Err(error) => {
                let logs = daemon.force_stop();
                panic!("integration daemon readiness timed out ({error}): {logs}");
            }
        }
    }

    pub(super) fn stop(mut self) {
        let graceful_shutdown_error = match self.child.try_wait() {
            Ok(Some(_)) => None,
            Ok(None) => kill_process(Pid::from_child(&self.child), Signal::TERM)
                .err()
                .map(|error| error.to_string().into_boxed_str()),
            Err(error) => {
                let termination = kill_process(Pid::from_child(&self.child), Signal::TERM)
                    .err()
                    .map(|error| error.to_string());
                Some(
                    match termination {
                        Some(termination) => {
                            format!("status check failed: {error}; SIGTERM failed: {termination}")
                        }
                        None => format!("status check failed before SIGTERM: {error}"),
                    }
                    .into_boxed_str(),
                )
            }
        };
        let completion = wait_for_child(&mut self.child, SERVER_STOP_LIMIT);
        self.stopped = true;
        let logs = self.join_stderr();
        assert!(
            graceful_shutdown_error.is_none(),
            "integration daemon could not begin graceful shutdown: {}: {logs}",
            graceful_shutdown_error
                .as_deref()
                .unwrap_or("unknown shutdown failure")
        );
        assert!(
            completion.wait_error.is_none(),
            "integration daemon wait failed: {}: {logs}",
            completion
                .wait_error
                .as_deref()
                .unwrap_or("unknown wait failure")
        );
        assert!(
            !completion.timed_out,
            "integration daemon exceeded its shutdown limit: {logs}"
        );
        assert!(
            completion.termination_error.is_none(),
            "integration daemon could not be killed after its shutdown timeout: {}: {logs}",
            completion
                .termination_error
                .as_deref()
                .unwrap_or("unknown termination failure")
        );
        assert!(
            completion
                .status
                .as_ref()
                .unwrap_or_else(|error| panic!("integration daemon could not be reaped: {error}"))
                .success(),
            "integration daemon failed: {logs}"
        );
    }

    fn force_stop(&mut self) -> String {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = wait_for_child(&mut self.child, FORCED_STOP_LIMIT);
        self.stopped = true;
        self.join_stderr()
    }

    fn join_stderr(&mut self) -> String {
        let captured = self.stderr.take().map(join_output).unwrap_or_default();
        String::from_utf8_lossy(&captured).into_owned()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.force_stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_accepts_only_configured_ephemeral_loopback_addresses() {
        assert_eq!(
            listener_address_from_ready_line("INFO public listener bound bind=127.0.0.1:1234"),
            Ok(Some((Listener::Public, "127.0.0.1:1234".parse().unwrap())))
        );
        assert_eq!(
            listener_address_from_ready_line(
                "INFO authenticated admin backend listener bound bind=127.0.0.1:43123"
            ),
            Ok(Some((Listener::Admin, "127.0.0.1:43123".parse().unwrap())))
        );

        for invalid in [
            "INFO authenticated admin backend listener bound",
            "INFO authenticated admin backend listener bound bind=invalid",
            "INFO authenticated admin backend listener bound bind=127.0.0.1:0",
            "INFO authenticated admin backend listener bound bind=127.0.0.2:43123",
            "INFO authenticated admin backend listener bound bind=[::1]:43123",
        ] {
            assert!(
                listener_address_from_ready_line(invalid).is_err(),
                "unexpectedly accepted {invalid}"
            );
        }
    }

    #[test]
    fn daemon_stderr_framing_rejects_a_newline_free_line_before_readiness() {
        let input = vec![b'x'; MAX_DAEMON_LOG_LINE_BYTES + 1];
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let captured = drain_daemon_stderr(std::io::Cursor::new(input), ready_tx);
        let error = ready_rx
            .recv()
            .expect("stderr framing must report its readiness result")
            .expect_err("an overlong readiness log line must be rejected");

        assert!(error.contains("exceeded"), "unexpected error: {error}");
        assert!(captured.len() <= MAX_CAPTURED_OUTPUT_BYTES);
    }
}
