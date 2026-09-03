use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};

use reqwest::{Method, StatusCode, Url, header::HeaderMap};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject as _},
    },
};

use super::{
    AdditionalRootCertificates, HttpRequest, HttpResponse, ReqwestExecutor, TransportError,
};

const LOCALHOST_CERTIFICATE: &[u8] = include_bytes!("../../tests/fixtures/tls/localhost-cert.pem");
const LOCALHOST_PRIVATE_KEY: &[u8] = include_bytes!("../../tests/fixtures/tls/localhost-key.pem");
const MAX_REQUEST_HEADER_BYTES: usize = 8 * 1024;
const MAX_CHILD_OUTPUT_BYTES: usize = 64 * 1024;
const NETWORK_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const PROXY_CHILD_MARKER: &str = "MAINCOPY_CLI_PROXY_TEST_CHILD";
const PROXY_CHILD_URL: &str = "MAINCOPY_CLI_PROXY_TEST_URL";

struct TlsServer {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<io::Result<TlsServerReport>>,
}

#[derive(Debug, Default)]
struct TlsServerReport {
    accepted_connections: usize,
    failed_connections: usize,
    request_targets: Vec<String>,
}

impl TlsServer {
    async fn start(response: Vec<u8>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("the TLS test listener binds to a selected loopback port");
        let address = listener
            .local_addr()
            .expect("the TLS test listener has a local address");
        let acceptor = test_tls_acceptor();
        let (shutdown, mut shutdown_requested) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut report = TlsServerReport::default();
            'serve: loop {
                let (stream, _) = tokio::select! {
                    biased;
                    _ = &mut shutdown_requested => break 'serve,
                    accepted = listener.accept() => accepted?,
                };
                report.accepted_connections += 1;
                let handled = tokio::select! {
                    biased;
                    _ = &mut shutdown_requested => break 'serve,
                    handled = timeout(
                        NETWORK_TIMEOUT,
                        serve_tls_connection(&acceptor, stream, &response),
                    ) => handled,
                };
                match handled {
                    Ok(Ok(target)) => report.request_targets.push(target),
                    Ok(Err(_)) | Err(_) => report.failed_connections += 1,
                }
            }
            Ok(report)
        });
        Self {
            address,
            shutdown: Some(shutdown),
            task,
        }
    }

    fn url(&self, host: &str, path: &str) -> Url {
        Url::parse(&format!("https://{host}:{}{path}", self.address.port()))
            .expect("the test server URL is valid")
    }

    async fn finish(mut self) -> TlsServerReport {
        self.shutdown
            .take()
            .expect("the TLS test server shuts down once")
            .send(())
            .ok();
        match timeout(SHUTDOWN_TIMEOUT, &mut self.task).await {
            Ok(joined) => joined
                .expect("the TLS test server task does not panic")
                .expect("the TLS test listener remains available"),
            Err(_) => {
                self.task.abort();
                self.task
                    .await
                    .expect_err("an aborted TLS test task does not complete normally");
                panic!("the TLS test server did not stop within its deadline");
            }
        }
    }
}

struct ProxyTrap {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<io::Result<bool>>,
}

impl ProxyTrap {
    async fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("the proxy trap binds to a selected loopback port");
        let address = listener
            .local_addr()
            .expect("the proxy trap has a local address");
        let (shutdown, mut shutdown_requested) = oneshot::channel();
        let task = tokio::spawn(async move {
            tokio::select! {
                biased;
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted?;
                    stream.shutdown().await?;
                    Ok(true)
                }
                _ = &mut shutdown_requested => Ok(false),
            }
        });
        Self {
            address,
            shutdown: Some(shutdown),
            task,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    async fn finish(mut self) -> bool {
        self.shutdown
            .take()
            .expect("the proxy trap shuts down once")
            .send(())
            .ok();
        match timeout(SHUTDOWN_TIMEOUT, &mut self.task).await {
            Ok(joined) => joined
                .expect("the proxy trap task does not panic")
                .expect("the proxy trap listener remains available"),
            Err(_) => {
                self.task.abort();
                self.task
                    .await
                    .expect_err("an aborted proxy trap task does not complete normally");
                panic!("the proxy trap did not stop within its deadline");
            }
        }
    }
}

struct ChildOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

fn test_tls_acceptor() -> TlsAcceptor {
    let certificates = CertificateDer::pem_slice_iter(LOCALHOST_CERTIFICATE)
        .collect::<Result<Vec<_>, _>>()
        .expect("the localhost certificate fixture is valid PEM");
    let private_key = PrivateKeyDer::from_pem_slice(LOCALHOST_PRIVATE_KEY)
        .expect("the localhost private-key fixture is valid PEM");
    let configuration = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .expect("the localhost certificate and private-key fixtures match");
    TlsAcceptor::from(Arc::new(configuration))
}

async fn serve_tls_connection(
    acceptor: &TlsAcceptor,
    stream: TcpStream,
    response: &[u8],
) -> io::Result<String> {
    let mut stream = acceptor.accept(stream).await?;
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        if request.len() == MAX_REQUEST_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "test request headers exceeded their bound",
            ));
        }
        let mut bytes = [0; 1024];
        let remaining = MAX_REQUEST_HEADER_BYTES - request.len();
        let read_limit = remaining.min(bytes.len());
        let received = stream.read(&mut bytes[..read_limit]).await?;
        if received == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "test request ended before its headers",
            ));
        }
        request.extend_from_slice(&bytes[..received]);
    }

    let request_line = request
        .split(|byte| *byte == b'\n')
        .next()
        .and_then(|line| line.strip_suffix(b"\r"))
        .and_then(|line| std::str::from_utf8(line).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid test request line"))?;
    let mut fields = request_line.split_ascii_whitespace();
    let method = fields.next();
    let target = fields.next();
    let version = fields.next();
    if method != Some("GET")
        || target.is_none()
        || version != Some("HTTP/1.1")
        || fields.next().is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected test request line",
        ));
    }

    stream.write_all(response).await?;
    stream.shutdown().await?;
    Ok(target.expect("the request target was validated").to_owned())
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tls")
        .join(name)
}

fn executor_with_root(name: &str) -> ReqwestExecutor {
    let roots = AdditionalRootCertificates::from_file(&fixture_path(name))
        .expect("the test root certificate fixture is valid");
    ReqwestExecutor::new(roots).expect("the HTTPS test client can be configured")
}

async fn execute_get(executor: &ReqwestExecutor, url: Url) -> Result<HttpResponse, TransportError> {
    timeout(
        NETWORK_TIMEOUT,
        executor.execute(
            HttpRequest {
                method: Method::GET,
                url,
                headers: HeaderMap::new(),
                body: Vec::new().into(),
            },
            64,
        ),
    )
    .await
    .expect("the HTTPS request completes within its test deadline")
}

fn ok_response() -> Vec<u8> {
    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
        .to_vec()
}

async fn read_child_output(stream: impl tokio::io::AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    stream
        .take((MAX_CHILD_OUTPUT_BYTES + 1) as u64)
        .read_to_end(&mut output)
        .await?;
    Ok(output)
}

async fn join_output_task(task: JoinHandle<io::Result<Vec<u8>>>) -> Vec<u8> {
    task.await
        .expect("the child-output reader task does not panic")
        .expect("the child-output pipe remains readable")
}

async fn run_proxy_child(url: &Url, proxy_url: &str) -> ChildOutput {
    let mut command = Command::new(
        std::env::current_exe().expect("the current CLI unit-test executable has a path"),
    );
    command
        .arg("proxy_child_request_uses_direct_tls")
        .arg("--ignored")
        .arg("--nocapture")
        .env(PROXY_CHILD_MARKER, "1")
        .env(PROXY_CHILD_URL, url.as_str())
        .env("HTTP_PROXY", proxy_url)
        .env("http_proxy", proxy_url)
        .env("HTTPS_PROXY", proxy_url)
        .env("https_proxy", proxy_url)
        .env("ALL_PROXY", proxy_url)
        .env("all_proxy", proxy_url)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .expect("the isolated proxy-test child process starts");
    let stdout = child
        .stdout
        .take()
        .expect("the proxy-test child stdout is piped");
    let stderr = child
        .stderr
        .take()
        .expect("the proxy-test child stderr is piped");
    let stdout_task = tokio::spawn(read_child_output(stdout));
    let stderr_task = tokio::spawn(read_child_output(stderr));

    let (status, timed_out) = match timeout(PROCESS_TIMEOUT, child.wait()).await {
        Ok(status) => (
            status.expect("the isolated proxy-test child can be awaited"),
            false,
        ),
        Err(_) => {
            child
                .kill()
                .await
                .expect("the timed-out proxy-test child can be killed");
            (
                child
                    .wait()
                    .await
                    .expect("the killed proxy-test child can be reaped"),
                true,
            )
        }
    };
    ChildOutput {
        status,
        stdout: join_output_task(stdout_task).await,
        stderr: join_output_task(stderr_task).await,
        timed_out,
    }
}

#[tokio::test]
async fn explicit_additional_root_accepts_its_matching_localhost_server() {
    let server = TlsServer::start(ok_response()).await;
    let response = execute_get(
        &executor_with_root("root-ca.pem"),
        server.url("localhost", "/trusted"),
    )
    .await
    .expect("the explicitly trusted TLS server is accepted");
    let report = server.finish().await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, b"ok");
    assert_eq!(report.request_targets, ["/trusted"]);
    assert_eq!(report.failed_connections, 0);
}

#[tokio::test]
async fn default_roots_reject_the_private_test_authority() {
    let server = TlsServer::start(ok_response()).await;
    let result = execute_get(
        &ReqwestExecutor::new(AdditionalRootCertificates::default())
            .expect("the default HTTPS client can be configured"),
        server.url("localhost", "/default-roots"),
    )
    .await;
    let report = server.finish().await;

    assert!(matches!(result, Err(TransportError::Request(_))));
    assert!(report.accepted_connections > 0);
    assert!(report.request_targets.is_empty());
}

#[tokio::test]
async fn unrelated_additional_root_rejects_the_private_test_authority() {
    let server = TlsServer::start(ok_response()).await;
    let result = execute_get(
        &executor_with_root("wrong-root-ca.pem"),
        server.url("localhost", "/wrong-root"),
    )
    .await;
    let report = server.finish().await;

    assert!(matches!(result, Err(TransportError::Request(_))));
    assert!(report.accepted_connections > 0);
    assert!(report.request_targets.is_empty());
}

#[tokio::test]
async fn trusted_root_does_not_disable_hostname_verification() {
    let server = TlsServer::start(ok_response()).await;
    let result = execute_get(
        &executor_with_root("root-ca.pem"),
        server.url("127.0.0.1", "/wrong-hostname"),
    )
    .await;
    let report = server.finish().await;

    assert!(matches!(result, Err(TransportError::Request(_))));
    assert_eq!(report.accepted_connections, 1);
    assert!(report.request_targets.is_empty());
}

#[tokio::test]
async fn redirect_policy_never_contacts_the_https_redirect_target() {
    let target = TlsServer::start(ok_response()).await;
    let location = target.url("localhost", "/credential-sink");
    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    let source = TlsServer::start(response).await;

    let result = execute_get(
        &executor_with_root("root-ca.pem"),
        source.url("localhost", "/redirect"),
    )
    .await
    .expect("the redirect response is returned without being followed");
    let source_report = source.finish().await;
    let target_report = target.finish().await;

    assert_eq!(result.status, StatusCode::FOUND);
    assert_eq!(source_report.request_targets, ["/redirect"]);
    assert_eq!(target_report.accepted_connections, 0);
    assert!(target_report.request_targets.is_empty());
}

#[tokio::test]
async fn environment_proxy_discovery_is_disabled_for_https_requests() {
    let server = TlsServer::start(ok_response()).await;
    let proxy = ProxyTrap::start().await;
    let child = run_proxy_child(&server.url("localhost", "/without-proxy"), &proxy.url()).await;
    let server_report = server.finish().await;
    let proxy_contacted = proxy.finish().await;

    assert!(!child.timed_out, "the proxy-test child process timed out");
    assert!(
        child.status.success(),
        "proxy-test child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr),
    );
    assert!(!proxy_contacted);
    assert_eq!(server_report.request_targets, ["/without-proxy"]);
}

#[tokio::test]
#[ignore = "run only in a child process with isolated proxy environment variables"]
async fn proxy_child_request_uses_direct_tls() {
    assert_eq!(
        std::env::var(PROXY_CHILD_MARKER).as_deref(),
        Ok("1"),
        "the proxy child must be launched by its parent trust-boundary test"
    );
    let url = Url::parse(
        &std::env::var(PROXY_CHILD_URL).expect("the proxy child receives its target URL"),
    )
    .expect("the proxy child target URL is valid");
    let response = execute_get(&executor_with_root("root-ca.pem"), url)
        .await
        .expect("the proxy child reaches the TLS server directly");
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, b"ok");
}
