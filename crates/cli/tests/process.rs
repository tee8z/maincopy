#![cfg(unix)]

use std::{
    ffi::OsString,
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Json, Router,
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header::LINK},
    routing::{get, post},
};
use maincopy_shared::{
    AdminApiVersion, CAPABILITIES_PATH, Capabilities, CapabilityContractVersion, FeatureVersions,
    posts::{ListPostsResponse, POSTS_PATH},
    publication::{
        CONTENT_DIGEST_HEADER, IDEMPOTENCY_KEY_HEADER, POST_REVISION_HEADER, PREVIEW_DIGEST_HEADER,
        PUBLICATIONS_PATH, PublishNowRequest, PublishNowResponse,
    },
};
use tempfile::TempDir;
use tokio::{net::UnixListener, sync::oneshot, task::JoinHandle};

const PREVIEW_DIGEST: &str =
    "preview-b3-v1-4444444444444444444444444444444444444444444444444444444444444444";

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
async fn standalone_binary_lists_all_post_pages_as_one_json_document() {
    const NEXT_CURSOR: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let first: ListPostsResponse = serde_json::from_value(serde_json::json!({
        "content_digest":
            "content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333",
        "site_digest":
            "site-b3-v1-2222222222222222222222222222222222222222222222222222222222222222",
        "site_version": 2,
        "posts": [{
            "post_id": "11111111-1111-4111-8111-111111111111",
            "source_path": "posts/ready.md",
            "title": "Ready to publish",
            "slug": "ready-to-publish",
            "revision":
                "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111",
            "publication_state": "unpublished_change",
            "published_at": "2026-08-29T12:00:00Z"
        }],
        "next_cursor": NEXT_CURSOR
    }))
    .unwrap();
    let second: ListPostsResponse = serde_json::from_value(serde_json::json!({
        "content_digest":
            "content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333",
        "site_digest":
            "site-b3-v1-2222222222222222222222222222222222222222222222222222222222222222",
        "site_version": 2,
        "posts": [{
            "post_id": "22222222-2222-4222-8222-222222222222",
            "source_path": "posts/already-live.md",
            "title": "Already live",
            "slug": "already-live",
            "revision":
                "post-b3-v1-2222222222222222222222222222222222222222222222222222222222222222",
            "publication_state": "published",
            "published_at": "2026-08-30T12:00:00Z"
        }],
        "next_cursor": null
    }))
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&calls);
    let server = TestServer::start_with_router(Router::new().route(
        POSTS_PATH,
        get(move |uri: Uri| {
            let first = first.clone();
            let second = second.clone();
            let calls = Arc::clone(&handler_calls);
            async move {
                match calls.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        assert_eq!(uri.query(), Some("limit=100"));
                        Json(first)
                    }
                    1 => {
                        assert_eq!(
                            uri.query(),
                            Some("limit=100&cursor=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
                        );
                        Json(second)
                    }
                    call => panic!("unexpected posts request {call}"),
                }
            }
        }),
    ))
    .await;

    let output = run_cli(vec![
        OsString::from("posts"),
        OsString::from("--socket"),
        server.socket_path.as_os_str().to_owned(),
        OsString::from("--json"),
    ])
    .await;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let listed = serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    assert_eq!(listed["site_version"], 2);
    assert_eq!(listed["posts"].as_array().unwrap().len(), 2);
    assert!(listed["next_cursor"].is_null());
    assert_eq!(
        listed["posts"][0]["post_id"],
        "11111111-1111-4111-8111-111111111111"
    );
    assert_eq!(listed["posts"][0]["source_path"], "posts/ready.md");
    assert_eq!(
        listed["posts"][0]["publication_state"],
        "unpublished_change"
    );
    assert_eq!(
        listed["posts"][1]["revision"],
        "post-b3-v1-2222222222222222222222222222222222222222222222222222222222222222"
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_binary_downloads_an_exact_preview_without_html_on_stdout() {
    const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const REVISION: &str =
        "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111";
    const CONTENT_DIGEST: &str =
        "content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333";
    const HTML: &str = "<!doctype html><html><title>Ready</title></html>";
    let server = TestServer::start_with_router(Router::new().route(
        "/api/admin/v1/posts/{post_id}/preview",
        get(|uri: Uri| async move {
            assert_eq!(
                uri.path_and_query().unwrap().as_str(),
                concat!(
                    "/api/admin/v1/posts/11111111-1111-4111-8111-111111111111/preview?",
                    "revision=post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111&",
                    "content_digest=content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333"
                )
            );
            let mut headers = HeaderMap::new();
            headers.insert(
                PREVIEW_DIGEST_HEADER,
                HeaderValue::from_static(PREVIEW_DIGEST),
            );
            headers.insert(POST_REVISION_HEADER, HeaderValue::from_static(REVISION));
            headers.insert(
                CONTENT_DIGEST_HEADER,
                HeaderValue::from_static(CONTENT_DIGEST),
            );
            headers.insert(
                LINK,
                HeaderValue::from_static(
                    "<https://example.test/posts/ready>; rel=\"canonical\"",
                ),
            );
            (headers, HTML)
        }),
    ))
    .await;
    let output_directory = tempfile::tempdir().unwrap();
    let output_path = output_directory.path().join("ready.html");

    let output = run_cli(vec![
        OsString::from("preview"),
        OsString::from(POST_ID),
        OsString::from("--output"),
        output_path.as_os_str().to_owned(),
        OsString::from("--revision"),
        OsString::from(REVISION),
        OsString::from("--content-digest"),
        OsString::from(CONTENT_DIGEST),
        OsString::from("--socket"),
        server.socket_path.as_os_str().to_owned(),
        OsString::from("--json"),
    ])
    .await;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read_to_string(&output_path).unwrap(), HTML);
    let metadata = serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    assert_eq!(metadata["post_id"], POST_ID);
    assert_eq!(metadata["preview_digest"], PREVIEW_DIGEST);
    assert_eq!(metadata["revision"], REVISION);
    assert_eq!(metadata["content_digest"], CONTENT_DIGEST);
    assert_eq!(
        metadata["canonical_url"],
        "https://example.test/posts/ready"
    );
    assert_eq!(metadata["output"], output_path.display().to_string());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("<!doctype"));

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preview_refuses_to_overwrite_an_existing_output_file() {
    const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
    let server = TestServer::start_with_router(Router::new().route(
        "/api/admin/v1/posts/{post_id}/preview",
        get(|| async {
            let mut headers = HeaderMap::new();
            headers.insert(
                PREVIEW_DIGEST_HEADER,
                HeaderValue::from_static(PREVIEW_DIGEST),
            );
            headers.insert(
                POST_REVISION_HEADER,
                HeaderValue::from_static(
                    "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111",
                ),
            );
            headers.insert(
                CONTENT_DIGEST_HEADER,
                HeaderValue::from_static(
                    "content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333",
                ),
            );
            headers.insert(
                LINK,
                HeaderValue::from_static(
                    "<https://example.test/posts/ready>; rel=\"canonical\"",
                ),
            );
            (headers, "replacement")
        }),
    ))
    .await;
    let output_directory = tempfile::tempdir().unwrap();
    let output_path = output_directory.path().join("ready.html");
    std::fs::write(&output_path, "keep me").unwrap();

    let output = run_cli(vec![
        OsString::from("preview"),
        OsString::from(POST_ID),
        OsString::from("--output"),
        output_path.as_os_str().to_owned(),
        OsString::from("--socket"),
        server.socket_path.as_os_str().to_owned(),
        OsString::from("--json"),
    ])
    .await;

    assert_eq!(output.status.code(), Some(75));
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read_to_string(&output_path).unwrap(), "keep me");
    let error = serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    assert_eq!(error["error"]["category"], "conflict");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("refusing to overwrite")
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_preview_metadata_creates_no_output_file() {
    const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
    let server = TestServer::start_with_router(Router::new().route(
        "/api/admin/v1/posts/{post_id}/preview",
        get(|| async { "metadata is intentionally missing" }),
    ))
    .await;
    let output_directory = tempfile::tempdir().unwrap();
    let output_path = output_directory.path().join("missing.html");

    let output = run_cli(vec![
        OsString::from("preview"),
        OsString::from(POST_ID),
        OsString::from("--output"),
        output_path.as_os_str().to_owned(),
        OsString::from("--socket"),
        server.socket_path.as_os_str().to_owned(),
        OsString::from("--json"),
    ])
    .await;

    assert_eq!(output.status.code(), Some(70));
    assert!(output.stderr.is_empty());
    assert!(!output_path.exists());
    let error = serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    assert_eq!(error["error"]["category"], "internal");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid preview metadata")
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
        "preview_digest": PREVIEW_DIGEST,
        "revision": REVISION,
        "state": "published",
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
                    assert_eq!(request.preview_digest.as_str(), PREVIEW_DIGEST);
                    assert_eq!(request.expected_revision.as_deref(), Some(REVISION));
                    assert_eq!(request.scheduled_for, None);
                    Json(response)
                }
            },
        ),
    ))
    .await;

    let output = run_cli(vec![
        OsString::from("publish-now"),
        OsString::from(POST_ID),
        OsString::from("--preview-digest"),
        OsString::from(PREVIEW_DIGEST),
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
    assert_eq!(published["preview_digest"], PREVIEW_DIGEST);
    assert_eq!(published["revision"], REVISION);
    assert_eq!(published["state"], "published");
    assert_eq!(published["published_at"], "2026-08-30T12:00:00Z");
    assert_eq!(published["site_version"], 2);

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_binary_schedules_an_exact_revision_and_utc_time() {
    const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const REVISION: &str =
        "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111";
    const IDEMPOTENCY_KEY: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const SCHEDULED_FOR: &str = "2026-09-01T12:30:00Z";
    let response: PublishNowResponse = serde_json::from_value(serde_json::json!({
        "publication_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "post_id": POST_ID,
        "preview_digest": PREVIEW_DIGEST,
        "revision": REVISION,
        "state": "scheduled",
        "scheduled_for": SCHEDULED_FOR,
        "published_at": null,
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
                    assert_eq!(
                        headers
                            .get(IDEMPOTENCY_KEY_HEADER)
                            .and_then(|value| value.to_str().ok()),
                        Some(IDEMPOTENCY_KEY)
                    );
                    assert_eq!(request.post_id, uuid::Uuid::parse_str(POST_ID).unwrap());
                    assert_eq!(request.preview_digest.as_str(), PREVIEW_DIGEST);
                    assert_eq!(request.expected_revision.as_deref(), Some(REVISION));
                    assert_eq!(
                        serde_json::to_value(&request).unwrap()["scheduled_for"],
                        SCHEDULED_FOR
                    );
                    Json(response)
                }
            },
        ),
    ))
    .await;

    let output = run_cli(vec![
        OsString::from("schedule"),
        OsString::from(POST_ID),
        OsString::from("--preview-digest"),
        OsString::from(PREVIEW_DIGEST),
        OsString::from("--at"),
        OsString::from(SCHEDULED_FOR),
        OsString::from("--revision"),
        OsString::from(REVISION),
        OsString::from("--idempotency-key"),
        OsString::from(IDEMPOTENCY_KEY),
        OsString::from("--socket"),
        server.socket_path.as_os_str().to_owned(),
        OsString::from("--json"),
    ])
    .await;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let scheduled = serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    assert_eq!(scheduled["idempotency_key"], IDEMPOTENCY_KEY);
    assert_eq!(
        scheduled["publication_id"],
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    );
    assert_eq!(scheduled["post_id"], POST_ID);
    assert_eq!(scheduled["preview_digest"], PREVIEW_DIGEST);
    assert_eq!(scheduled["revision"], REVISION);
    assert_eq!(scheduled["state"], "scheduled");
    assert_eq!(scheduled["scheduled_for"], SCHEDULED_FOR);
    assert!(scheduled["published_at"].is_null());
    assert_eq!(scheduled["site_version"], 2);

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

#[test]
fn malformed_preview_selector_is_a_local_validation_error() {
    let directory = tempfile::tempdir().unwrap();
    let socket_path = directory.path().join("missing.sock");
    let output_path = directory.path().join("must-not-exist.html");
    let output = Command::new(env!("CARGO_BIN_EXE_maincopy"))
        .args([
            "preview",
            "11111111-1111-4111-8111-111111111111",
            "--output",
        ])
        .arg(&output_path)
        .args([
            "--revision",
            "post-b3-v1-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "--socket",
        ])
        .arg(&socket_path)
        .arg("--json")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(65));
    assert!(output.stderr.is_empty());
    assert!(!output_path.exists());
    let error = serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    assert_eq!(error["error"]["category"], "validation");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("64 lowercase hexadecimal")
    );
}
