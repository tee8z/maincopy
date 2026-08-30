#![cfg(unix)]

use std::{ffi::OsString, path::PathBuf, process::Command};

use axum::{Json, Router, routing::get};
use maincopy_shared::{
    AdminApiVersion, CAPABILITIES_PATH, Capabilities, CapabilityContractVersion, FeatureVersions,
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
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("admin.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let router = Router::new().route(
            CAPABILITIES_PATH,
            get(|| async {
                Json(Capabilities {
                    api_version: AdminApiVersion::V1,
                    features: FeatureVersions {
                        capabilities: CapabilityContractVersion::V1,
                    },
                })
            }),
        );
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
