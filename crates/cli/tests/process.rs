#![cfg(unix)]

use std::{ffi::OsString, path::PathBuf, process::Command};

use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use maincopy_shared::{
    AdminApiVersion, CAPABILITIES_PATH, Capabilities, CapabilityContractVersion, FeatureVersions,
    publication::{
        IDEMPOTENCY_KEY_HEADER, PUBLICATIONS_PATH, PublishNowRequest, PublishNowResponse,
    },
};
use tempfile::TempDir;
use tokio::{net::UnixListener, sync::oneshot, task::JoinHandle};

struct TestServer {
    _directory: TempDir,
    socket_path: PathBuf,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_router(Router::new().route(
            CAPABILITIES_PATH,
            get(|| async {
                Json(Capabilities {
                    api_version: AdminApiVersion::V1,
                    features: FeatureVersions {
                        capabilities: CapabilityContractVersion::V1,
                    },
                })
            }),
        ))
        .await
    }

    async fn start_with_status(status: StatusCode) -> Self {
        Self::start_with_router(
            Router::new().route(CAPABILITIES_PATH, get(move || async move { status })),
        )
        .await
    }

    async fn start_with_router(router: Router) -> Self {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authentication_and_authorization_have_stable_machine_errors() {
    for (status, category) in [
        (StatusCode::UNAUTHORIZED, "authentication"),
        (StatusCode::FORBIDDEN, "authorization"),
    ] {
        let server = TestServer::start_with_status(status).await;
        let output = run_cli(vec![
            OsString::from("capabilities"),
            OsString::from("--socket"),
            server.socket_path.as_os_str().to_owned(),
            OsString::from("--json"),
        ])
        .await;

        assert_eq!(output.status.code(), Some(77));
        assert!(output.stderr.is_empty());
        let error = serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
        assert_eq!(error["error"]["category"], category);
        server.stop().await;
    }
}

async fn run_cli(arguments: Vec<OsString>) -> std::process::Output {
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_maincopy"))
            .args(arguments)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_binary_reads_capabilities_as_json_over_a_real_unix_socket() {
    let server = TestServer::start().await;

    let output = run_cli(vec![
        OsString::from("capabilities"),
        OsString::from("--socket"),
        server.socket_path.as_os_str().to_owned(),
        OsString::from("--json"),
    ])
    .await;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({
            "api_version": "v1",
            "features": { "capabilities": "v1" }
        })
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_binary_publishes_as_json_over_a_real_unix_socket() {
    const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const REVISION: &str =
        "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111";
    let response: PublishNowResponse = serde_json::from_value(serde_json::json!({
        "publication_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "post_id": POST_ID,
        "revision": REVISION,
        "published_at": "2026-08-30T12:00:00Z",
        "site_digest":
            "site-b3-v1-2222222222222222222222222222222222222222222222222222222222222222",
        "site_version": 2
    }))
    .unwrap();
    let server = TestServer::start_with_router(Router::new().route(
        PUBLICATIONS_PATH,
        post(
            move |headers: HeaderMap, Json(request): Json<PublishNowRequest>| {
                let response = response.clone();
                async move {
                    let idempotency_key = headers
                        .get(IDEMPOTENCY_KEY_HEADER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| uuid::Uuid::parse_str(value).ok());
                    assert!(idempotency_key.is_some());
                    assert_eq!(request.post_id, uuid::Uuid::parse_str(POST_ID).unwrap());
                    assert_eq!(request.expected_revision.as_deref(), Some(REVISION));
                    Json(response)
                }
            },
        ),
    ))
    .await;

    let output = run_cli(vec![
        OsString::from("publish-now"),
        OsString::from(POST_ID),
        OsString::from("--revision"),
        OsString::from(REVISION),
        OsString::from("--socket"),
        server.socket_path.as_os_str().to_owned(),
        OsString::from("--json"),
    ])
    .await;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let published = serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    assert!(
        uuid::Uuid::parse_str(published["idempotency_key"].as_str().unwrap()).is_ok(),
        "the CLI must report its generated retry identity"
    );
    assert_eq!(
        published["publication_id"],
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    );
    assert_eq!(published["post_id"], POST_ID);
    assert_eq!(published["revision"], REVISION);
    assert_eq!(published["published_at"], "2026-08-30T12:00:00Z");
    assert_eq!(published["site_version"], 2);

    server.stop().await;
}

#[test]
fn unavailable_socket_has_a_stable_machine_error_and_exit() {
    let directory = tempfile::tempdir().unwrap();
    let socket_path = directory.path().join("missing.sock");
    let output = Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .args(["capabilities", "--socket"])
        .arg(&socket_path)
        .arg("--json")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(69));
    assert!(output.stderr.is_empty());
    let error = serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    assert_eq!(error["error"]["category"], "availability");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing.sock")
    );
}
