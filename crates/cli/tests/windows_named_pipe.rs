#![cfg(windows)]

use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use maincopy_cli::AdminClient;
use maincopy_shared::{
    AdminApiVersion, CAPABILITIES_PATH, Capabilities, CapabilityContractVersion, FeatureVersions,
    is_valid_windows_admin_pipe_name,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::windows::named_pipe::ServerOptions,
};

static NEXT_PIPE_ID: AtomicU64 = AtomicU64::new(0);
const MAX_REQUEST_BYTES: usize = 16 * 1024;

fn unique_pipe_name() -> String {
    format!(
        r"\\.\pipe\maincopy-cli-test-{}-{}",
        std::process::id(),
        NEXT_PIPE_ID.fetch_add(1, Ordering::Relaxed)
    )
}

#[tokio::test]
async fn concrete_client_reads_capabilities_over_a_native_named_pipe() {
    let pipe_name = unique_pipe_name();
    assert!(is_valid_windows_admin_pipe_name(&pipe_name));

    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_name)
        .unwrap();
    let expected = Capabilities {
        api_version: AdminApiVersion::V1,
        features: FeatureVersions {
            capabilities: CapabilityContractVersion::V1,
        },
    };
    let body = serde_json::to_vec(&expected).unwrap();
    let mut response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);

    let server_task = tokio::spawn(async move {
        server.connect().await.unwrap();

        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        loop {
            let read = server.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0, "client closed before sending HTTP headers");
            request.extend_from_slice(&chunk[..read]);
            assert!(
                request.len() <= MAX_REQUEST_BYTES,
                "request exceeded the test server limit"
            );
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        assert!(request.starts_with(format!("GET {CAPABILITIES_PATH} HTTP/1.1\r\n").as_bytes()));

        server.write_all(&response).await.unwrap();
        server.shutdown().await.unwrap();
    });

    let client = AdminClient::new(PathBuf::from(pipe_name)).unwrap();
    assert_eq!(client.capabilities().await.unwrap(), expected);
    server_task.await.unwrap();
}
