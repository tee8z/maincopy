use std::{path::PathBuf, time::Duration};

#[cfg(windows)]
use maincopy_shared::is_valid_windows_admin_pipe_name;
use maincopy_shared::{CAPABILITIES_PATH, Capabilities};
use reqwest::{StatusCode, header::ACCEPT};
use thiserror::Error;

const ADMIN_ORIGIN: &str = "http://maincopy.local";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Concrete HTTP client for Maincopy's private local admin API.
#[derive(Clone, Debug)]
pub struct AdminClient {
    http: reqwest::Client,
    socket_path: PathBuf,
    max_response_bytes: usize,
}

impl AdminClient {
    /// Creates a client that sends every request through `socket_path`.
    pub fn new(socket_path: PathBuf) -> Result<Self, AdminClientError> {
        Self::with_limits(
            socket_path,
            DEFAULT_REQUEST_TIMEOUT,
            DEFAULT_MAX_RESPONSE_BYTES,
        )
    }

    fn with_limits(
        socket_path: PathBuf,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<Self, AdminClientError> {
        if socket_path.as_os_str().is_empty() {
            return Err(AdminClientError::InvalidSocketPath);
        }
        #[cfg(windows)]
        if !socket_path
            .to_str()
            .is_some_and(is_valid_windows_admin_pipe_name)
        {
            return Err(AdminClientError::InvalidSocketPath);
        }

        let builder = reqwest::Client::builder().no_proxy().timeout(timeout);
        #[cfg(unix)]
        let builder = builder.unix_socket(socket_path.clone());
        #[cfg(windows)]
        let builder = builder.windows_named_pipe(socket_path.clone());
        let http = builder.build().map_err(AdminClientError::Build)?;

        Ok(Self {
            http,
            socket_path,
            max_response_bytes,
        })
    }

    /// Fetches the versions advertised by the running server.
    pub async fn capabilities(&self) -> Result<Capabilities, AdminClientError> {
        let mut response = self
            .http
            .get(format!("{ADMIN_ORIGIN}{CAPABILITIES_PATH}"))
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(|source| AdminClientError::Request {
                socket_path: self.socket_path.clone(),
                source,
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(AdminClientError::HttpStatus { status });
        }

        let mut body = Vec::new();
        while let Some(chunk) =
            response
                .chunk()
                .await
                .map_err(|source| AdminClientError::Request {
                    socket_path: self.socket_path.clone(),
                    source,
                })?
        {
            let Some(length) = body.len().checked_add(chunk.len()) else {
                return Err(AdminClientError::ResponseTooLarge {
                    limit: self.max_response_bytes,
                });
            };
            if length > self.max_response_bytes {
                return Err(AdminClientError::ResponseTooLarge {
                    limit: self.max_response_bytes,
                });
            }
            body.extend_from_slice(&chunk);
        }

        serde_json::from_slice(&body).map_err(AdminClientError::InvalidResponse)
    }
}

/// Failures produced before a typed admin response is available.
#[derive(Debug, Error)]
pub enum AdminClientError {
    #[error("the local admin endpoint is invalid")]
    InvalidSocketPath,

    #[error("failed to construct the admin HTTP client: {0}")]
    Build(#[source] reqwest::Error),

    #[error("admin request through {socket_path:?} failed: {source}")]
    Request {
        socket_path: PathBuf,
        #[source]
        source: reqwest::Error,
    },

    #[error("the admin server returned HTTP {status}")]
    HttpStatus { status: StatusCode },

    #[error("the admin response exceeded the {limit}-byte limit")]
    ResponseTooLarge { limit: usize },

    #[error("the admin server returned an invalid response: {0}")]
    InvalidResponse(#[source] serde_json::Error),
}

#[cfg(all(test, unix))]
mod tests {
    use std::future::pending;

    use axum::{Json, Router, routing::get};
    use maincopy_shared::{AdminApiVersion, CapabilityContractVersion, FeatureVersions};
    use tempfile::TempDir;
    use tokio::{net::UnixListener, sync::oneshot, task::JoinHandle};

    use super::*;

    struct TestServer {
        _directory: TempDir,
        socket_path: PathBuf,
        shutdown: oneshot::Sender<()>,
        task: JoinHandle<()>,
    }

    impl TestServer {
        async fn start(router: Router) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let socket_path = directory.path().join("admin.sock");
            let listener = UnixListener::bind(&socket_path).unwrap();
            let (shutdown, stopped) = oneshot::channel();
            let task = tokio::spawn(async move {
                axum::serve(listener, router)
                    .with_graceful_shutdown(async move {
                        let _ = stopped.await;
                    })
                    .await
                    .unwrap();
            });

            Self {
                _directory: directory,
                socket_path,
                shutdown,
                task,
            }
        }

        async fn stop(self) {
            let _ = self.shutdown.send(());
            self.task.await.unwrap();
        }
    }

    fn capabilities() -> Capabilities {
        Capabilities {
            api_version: AdminApiVersion::V1,
            features: FeatureVersions {
                capabilities: CapabilityContractVersion::V1,
            },
        }
    }

    #[tokio::test]
    async fn client_reads_typed_capabilities_over_a_real_unix_socket() {
        let expected = capabilities();
        let server = TestServer::start(Router::new().route(
            CAPABILITIES_PATH,
            get(move || async move { Json(expected) }),
        ))
        .await;
        let client = AdminClient::new(server.socket_path.clone()).unwrap();

        assert_eq!(client.capabilities().await.unwrap(), expected);

        server.stop().await;
    }

    #[tokio::test]
    async fn cloned_clients_can_make_concurrent_requests() {
        let server = TestServer::start(
            Router::new().route(CAPABILITIES_PATH, get(|| async { Json(capabilities()) })),
        )
        .await;
        let client = AdminClient::new(server.socket_path.clone()).unwrap();
        let other_client = client.clone();

        let (left, right) = tokio::join!(client.capabilities(), other_client.capabilities());
        assert_eq!(left.unwrap(), capabilities());
        assert_eq!(right.unwrap(), capabilities());

        server.stop().await;
    }

    #[tokio::test]
    async fn response_limit_is_enforced_while_streaming() {
        let server = TestServer::start(
            Router::new().route(CAPABILITIES_PATH, get(|| async { "x".repeat(33) })),
        )
        .await;
        let client =
            AdminClient::with_limits(server.socket_path.clone(), DEFAULT_REQUEST_TIMEOUT, 32)
                .unwrap();

        assert!(matches!(
            client.capabilities().await,
            Err(AdminClientError::ResponseTooLarge { limit: 32 })
        ));

        server.stop().await;
    }

    #[tokio::test]
    async fn malformed_success_response_is_rejected() {
        let server =
            TestServer::start(Router::new().route(CAPABILITIES_PATH, get(|| async { "not json" })))
                .await;
        let client = AdminClient::new(server.socket_path.clone()).unwrap();

        assert!(matches!(
            client.capabilities().await,
            Err(AdminClientError::InvalidResponse(_))
        ));

        server.stop().await;
    }

    #[tokio::test]
    async fn request_timeout_is_a_transport_error() {
        let server = TestServer::start(Router::new().route(
            CAPABILITIES_PATH,
            get(|| async { pending::<String>().await }),
        ))
        .await;
        let client = AdminClient::with_limits(
            server.socket_path.clone(),
            Duration::from_millis(10),
            DEFAULT_MAX_RESPONSE_BYTES,
        )
        .unwrap();

        let Err(AdminClientError::Request { source, .. }) = client.capabilities().await else {
            panic!("timed-out request must be a transport error");
        };
        assert!(source.is_timeout());

        server.stop().await;
    }

    #[tokio::test]
    async fn missing_socket_is_an_actionable_transport_error() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("missing.sock");
        let client = AdminClient::new(socket_path.clone()).unwrap();

        let Err(AdminClientError::Request {
            socket_path: reported,
            ..
        }) = client.capabilities().await
        else {
            panic!("a missing socket must fail at the transport boundary");
        };
        assert_eq!(reported, socket_path);
    }
}
