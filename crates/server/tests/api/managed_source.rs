use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use axum::body::Bytes;
use reqwest::{
    Method, Response, StatusCode,
    header::{CONTENT_TYPE, COOKIE, ETAG, HOST, LOCATION, ORIGIN, SET_COOKIE},
};
use rustix::process::{Pid, test_kill_process};
use serde::{Serialize, de::DeserializeOwned};
use uuid::Uuid;
use zeroize::Zeroizing;

use maincopy_shared::{
    auth_api::{
        ADMIN_SESSIONS_PATH, CSRF_COOKIE_NAME, CSRF_HEADER_NAME, CreateAdminSessionRequest,
        SESSION_COOKIE_NAME, SecretString,
    },
    posts::{ListPostsResponse, POSTS_PATH, PostPublicationState},
    publication::{
        IDEMPOTENCY_KEY_HEADER, PREVIEW_DIGEST_HEADER, PUBLICATIONS_PATH, PreviewDigest,
        PublishNowRequest, PublishNowResponse,
    },
    source::{
        BeginSourceSyncResponse, ListSourceSyncsResponse, SOURCE_PATH, SOURCE_SYNCS_PATH,
        SourceStatusResponse, SourceSyncAdmission, SourceSyncOutcome, SourceSyncRequestOrigin,
        SourceSyncResource,
    },
};

use super::process_harness::{CapturedChild, Daemon};

const ADMIN_ORIGIN: &str = "https://admin.example.test";
const ADMIN_AUTHORITY: &str = "admin.example.test";
const OWNER_USERNAME: &str = "managed-source-owner";
const OWNER_PASSWORD: &str = "correct horse battery staple";
const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
const INITIAL_TITLE: &str = "Initial managed post";
const INITIAL_BODY: &str = "Initial managed body.";
const UPDATED_TITLE: &str = "Updated managed post";
const UPDATED_BODY: &str = "This push became a private preview without a restart.";
const COMMAND_LIMIT: Duration = Duration::from_secs(30);
const REQUEST_LIMIT: Duration = Duration::from_secs(10);
const POLL_LIMIT: Duration = Duration::from_secs(75);
const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024;
const MAX_SOURCE_SYNC_REQUEST_BYTES: usize = 4 * 1024;

#[tokio::test]
async fn real_managed_git_poll_updates_only_the_private_preview_without_a_restart() {
    let fixture = ManagedGitFixture::start().await;
    fixture.verify_source_admin_workflow().await;
    let initial = fixture.only_post().await;
    assert_eq!(initial.title.as_ref(), INITIAL_TITLE);
    assert_eq!(initial.publication_state, PostPublicationState::Unpublished);

    let initial_preview = fixture.admin_get(&preview_path()).await;
    assert_eq!(initial_preview.status(), StatusCode::OK);
    let initial_preview_digest = preview_digest(&initial_preview);
    let initial_preview = response_text(initial_preview).await;
    assert!(initial_preview.contains(INITIAL_BODY));
    assert!(!initial_preview.contains(UPDATED_BODY));

    let published = fixture
        .admin_json(
            Method::POST,
            PUBLICATIONS_PATH,
            &PublishNowRequest {
                post_id: Uuid::parse_str(POST_ID).unwrap(),
                preview_digest: initial_preview_digest,
                expected_revision: None,
                scheduled_for: None,
            },
        )
        .await;
    assert_eq!(published.status(), StatusCode::OK);
    let published: PublishNowResponse = response_json(published).await;
    assert_eq!(published.revision, initial.revision);

    let initial_public = fixture.public_get("/posts/managed").await;
    assert_eq!(initial_public.status(), StatusCode::OK);
    let initial_etag = initial_public.headers()[ETAG].clone();
    let initial_public = response_text(initial_public).await;
    assert!(initial_public.contains(INITIAL_BODY));
    assert!(!initial_public.contains(UPDATED_BODY));

    let pushed_commit = fixture.push_update();
    fixture.wait_for_applied_poll(&pushed_commit).await;

    let updated = fixture.only_post().await;
    assert_eq!(updated.title.as_ref(), UPDATED_TITLE);
    assert_eq!(
        updated.publication_state,
        PostPublicationState::UnpublishedChange
    );
    assert_ne!(updated.revision, published.revision);

    let updated_preview = fixture.admin_get(&preview_path()).await;
    assert_eq!(updated_preview.status(), StatusCode::OK);
    let updated_preview = response_text(updated_preview).await;
    assert!(updated_preview.contains(UPDATED_BODY));
    assert!(!updated_preview.contains(INITIAL_BODY));

    let still_pinned = fixture.public_get("/posts/managed").await;
    assert_eq!(still_pinned.status(), StatusCode::OK);
    assert_eq!(still_pinned.headers()[ETAG], initial_etag);
    let still_pinned = response_text(still_pinned).await;
    assert!(still_pinned.contains(INITIAL_BODY));
    assert!(!still_pinned.contains(UPDATED_BODY));

    assert!(
        fixture.ssh_marker.is_file(),
        "the packaged maincopy-ssh helper never invoked the constrained SSH transport"
    );
    fixture.stop();
}

#[test]
fn managed_git_wall_time_covers_output_held_open_by_descendants() {
    let root = tempfile::tempdir().expect("managed source process root must be created");
    write_credentials(root.path());
    write_host_file_with_fetch_timeout(root.path(), Some(1));
    bootstrap_password_owner(root.path());
    configure_source(root.path(), &root.path().join("remote.git"));

    let fake_git = root.path().join("git-with-pipe-holding-descendant");
    let descendant_pid = root.path().join("pipe-holding-descendant.pid");
    fs::write(
        &fake_git,
        format!(
            "#!/bin/sh\n\
             /bin/sh -c 'kill -STOP $$' &\n\
             printf '%s\\n' \"$!\" > {}\n\
             exit 0\n",
            shell_literal(&descendant_pid),
        ),
    )
    .expect("pipe-holding Git fixture must be written");
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o700))
        .expect("pipe-holding Git fixture must be executable");
    let fake_git =
        fs::canonicalize(fake_git).expect("pipe-holding Git fixture path must be canonical");

    let started = Instant::now();
    let child = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args(["--config", "maincopy.toml"])
        .current_dir(root.path())
        .env("MAINCOPY_GIT_EXECUTABLE", fake_git)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("managed source daemon must start");
    let (completion, _stdout, stderr) = CapturedChild::new(child).wait(Duration::from_secs(5));
    let elapsed = started.elapsed();
    let diagnostic = String::from_utf8_lossy(&stderr);

    assert!(
        !completion.timed_out,
        "daemon outlived the Git wall-time limit: {diagnostic}"
    );
    assert!(
        elapsed >= Duration::from_millis(750),
        "daemon did not exercise the configured Git wall-time limit: {elapsed:?}: {diagnostic}"
    );
    assert!(
        diagnostic.contains("TimedOut"),
        "daemon did not classify the Git wall-time failure: {diagnostic}"
    );
    assert!(
        completion.wait_error.is_none(),
        "daemon wait failed: {}: {diagnostic}",
        completion
            .wait_error
            .as_deref()
            .unwrap_or("unknown wait failure")
    );
    assert!(
        completion.termination_error.is_none(),
        "daemon termination failed: {}: {diagnostic}",
        completion
            .termination_error
            .as_deref()
            .unwrap_or("unknown termination failure")
    );
    assert!(
        !completion
            .status
            .as_ref()
            .unwrap_or_else(|error| panic!("daemon could not be reaped: {error}"))
            .success(),
        "startup unexpectedly survived an incomplete Git command"
    );
    let descendant_pid = fs::read_to_string(descendant_pid)
        .expect("the fake Git leader must record its descendant")
        .trim()
        .parse::<i32>()
        .ok()
        .and_then(Pid::from_raw)
        .expect("the fake Git descendant must have a valid PID");
    let reaping_limit = Instant::now() + Duration::from_secs(1);
    while test_kill_process(descendant_pid).is_ok() && Instant::now() < reaping_limit {
        std::thread::yield_now();
    }
    assert!(
        test_kill_process(descendant_pid).is_err(),
        "the timed-out Git descendant survived its process group"
    );
}

struct ManagedGitFixture {
    daemon: Daemon,
    root: tempfile::TempDir,
    work: PathBuf,
    ssh_marker: PathBuf,
    client: reqwest::Client,
    admin_url: String,
    public_url: String,
    session: HumanSession,
}

impl ManagedGitFixture {
    async fn start() -> Self {
        let root = tempfile::tempdir().expect("managed source process root must be created");
        let work = root.path().join("work");
        let remote = root.path().join("remote.git");
        let ssh_marker = root.path().join("constrained-ssh-invoked");
        initialize_remote(root.path(), &work, &remote);
        let fake_ssh = write_transport_fixture(root.path(), &remote, &ssh_marker);
        write_credentials(root.path());
        write_host_file(root.path());
        bootstrap_password_owner(root.path());
        configure_source(root.path(), &remote);

        let daemon_binary = fs::canonicalize(env!("CARGO_BIN_EXE_maincopyd"))
            .expect("the packaged daemon test binary must exist");
        let helper_binary = fs::canonicalize(env!("CARGO_BIN_EXE_maincopy-ssh"))
            .expect("the packaged SSH helper test binary must exist");
        assert_eq!(
            daemon_binary.parent(),
            helper_binary.parent(),
            "the daemon must discover the packaged sibling SSH helper"
        );
        let mut command = Command::new(daemon_binary);
        command
            .args(["--config", "maincopy.toml"])
            .current_dir(root.path())
            .env("MAINCOPY_SSH_EXECUTABLE", fake_ssh);
        let (daemon, addresses) = Daemon::start(command);
        assert!(
            ssh_marker.is_file(),
            "startup synchronization did not traverse the packaged SSH helper"
        );

        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_LIMIT)
            .build()
            .expect("managed source integration client must build");
        let admin_url = format!("http://{}", addresses.admin);
        let public_url = format!("http://{}", addresses.public);
        let session = password_login(&client, &admin_url).await;

        Self {
            daemon,
            root,
            work,
            ssh_marker,
            client,
            admin_url,
            public_url,
            session,
        }
    }

    async fn only_post(&self) -> maincopy_shared::posts::PostSummary {
        let response = self.admin_get(POSTS_PATH).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut posts: ListPostsResponse = response_json(response).await;
        assert_eq!(posts.posts.len(), 1);
        posts.posts.pop().unwrap()
    }

    async fn verify_source_admin_workflow(&self) {
        let page = self.admin_get("/admin/source").await;
        assert_eq!(page.status(), StatusCode::OK);
        let page = response_text(page).await;
        assert!(page.contains("Managed Git"));
        assert!(page.contains("Start synchronization"));
        assert!(page.contains("This does not publish any post"));
        assert!(page.contains("Pushes can become private previews without restarting the service"));
        assert!(!page.contains("credentials/source-key"));

        for (body, expected_code) in [
            (b"{".as_slice(), "invalid_source_sync_request"),
            (
                b"{\"force\":true}".as_slice(),
                "invalid_source_sync_request",
            ),
            (b"{}".as_slice(), "missing_idempotency_key"),
        ] {
            let response = self
                .admin_raw(Method::POST, SOURCE_SYNCS_PATH, body, None)
                .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(problem_code(response).await, expected_code);
        }
        let oversized = vec![b' '; MAX_SOURCE_SYNC_REQUEST_BYTES + 1];
        let response = self
            .admin_raw(Method::POST, SOURCE_SYNCS_PATH, &oversized, None)
            .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            problem_code(response).await,
            "source_sync_request_too_large"
        );

        let idempotency_key = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("_csrf", self.session.csrf.as_str())
            .append_pair("idempotency_key", idempotency_key)
            .finish();
        let response = self.admin_form("/admin/source/sync", form).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response.headers()[LOCATION]
            .to_str()
            .expect("source UI redirect must be visible ASCII");
        let source_sync_id = location
            .strip_prefix("/admin/source?sync=")
            .expect("source UI redirect must identify its durable operation")
            .to_owned();
        self.wait_for_manual_no_change(&source_sync_id).await;

        let replayed = self
            .admin_raw(
                Method::POST,
                SOURCE_SYNCS_PATH,
                b"{}",
                Some(idempotency_key),
            )
            .await;
        assert_eq!(replayed.status(), StatusCode::OK);
        let replayed: BeginSourceSyncResponse = response_json(replayed).await;
        assert_eq!(replayed.admission, SourceSyncAdmission::Replayed);
        assert_eq!(replayed.sync.source_sync_id.to_string(), source_sync_id);

        let item = self
            .admin_get(&format!("{}/{source_sync_id}", SOURCE_SYNCS_PATH))
            .await;
        assert_eq!(item.status(), StatusCode::OK);
        let item: SourceSyncResource = response_json(item).await;
        assert_eq!(item.source_sync_id.to_string(), source_sync_id);

        let list = self.admin_get(SOURCE_SYNCS_PATH).await;
        assert_eq!(list.status(), StatusCode::OK);
        let list: ListSourceSyncsResponse = response_json(list).await;
        assert!(
            list.syncs
                .iter()
                .any(|sync| sync.source_sync_id.to_string() == source_sync_id)
        );

        let missing = self
            .admin_get("/api/admin/v1/source-syncs/11111111-1111-4111-8111-111111111111")
            .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(problem_code(missing).await, "source_sync_not_found");

        for path in [
            "/api/admin/v1/source-syncs?limit=0",
            "/api/admin/v1/source-syncs?unknown=value",
            "/api/admin/v1/source-syncs?cursor=11111111-1111-4111-8111-11111111111A",
            "/api/admin/v1/source-syncs/11111111-1111-4111-8111-11111111111A",
        ] {
            let invalid = self.admin_get(path).await;
            assert_eq!(invalid.status(), StatusCode::BAD_REQUEST, "{path}");
        }
    }

    async fn admin_get(&self, path: &str) -> Response {
        self.client
            .get(format!("{}{path}", self.admin_url))
            .header(HOST, ADMIN_AUTHORITY)
            .header(ORIGIN, ADMIN_ORIGIN)
            .header(COOKIE, self.session.cookie_header.as_str())
            .send()
            .await
            .expect("managed source admin request must complete")
    }

    async fn admin_json<Body>(&self, method: Method, path: &str, body: &Body) -> Response
    where
        Body: Serialize,
    {
        self.client
            .request(method, format!("{}{path}", self.admin_url))
            .header(HOST, ADMIN_AUTHORITY)
            .header(ORIGIN, ADMIN_ORIGIN)
            .header(COOKIE, self.session.cookie_header.as_str())
            .header(CSRF_HEADER_NAME, self.session.csrf.as_str())
            .header(IDEMPOTENCY_KEY_HEADER, Uuid::new_v4().to_string())
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(body).expect("admin mutation fixture must serialize"))
            .send()
            .await
            .expect("managed source admin mutation must complete")
    }

    async fn admin_raw(
        &self,
        method: Method,
        path: &str,
        body: &[u8],
        idempotency_key: Option<&str>,
    ) -> Response {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.admin_url))
            .header(HOST, ADMIN_AUTHORITY)
            .header(ORIGIN, ADMIN_ORIGIN)
            .header(COOKIE, self.session.cookie_header.as_str())
            .header(CSRF_HEADER_NAME, self.session.csrf.as_str())
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_vec());
        if let Some(idempotency_key) = idempotency_key {
            request = request.header(IDEMPOTENCY_KEY_HEADER, idempotency_key);
        }
        request
            .send()
            .await
            .expect("managed source admin request must complete")
    }

    async fn admin_form(&self, path: &str, body: String) -> Response {
        self.client
            .post(format!("{}{path}", self.admin_url))
            .header(HOST, ADMIN_AUTHORITY)
            .header(ORIGIN, ADMIN_ORIGIN)
            .header(COOKIE, self.session.cookie_header.as_str())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .expect("managed source admin form must complete")
    }

    async fn public_get(&self, path: &str) -> Response {
        self.client
            .get(format!("{}{path}", self.public_url))
            .send()
            .await
            .expect("managed source public request must complete")
    }

    fn push_update(&self) -> String {
        write_site(&self.work, UPDATED_TITLE, UPDATED_BODY);
        commit(&self.work, "update managed content");
        run_git(
            &self.work,
            [OsStr::new("push"), OsStr::new("origin"), OsStr::new("main")],
        );
        let commit = git_output(&self.work, [OsStr::new("rev-parse"), OsStr::new("HEAD")]);
        format!("git-sha1:{commit}")
    }

    async fn wait_for_applied_poll(&self, pushed_commit: &str) {
        tokio::time::timeout(POLL_LIMIT, async {
            loop {
                let response = self.admin_get(SOURCE_PATH).await;
                assert_eq!(response.status(), StatusCode::OK);
                let status: SourceStatusResponse = response_json(response).await;
                if let SourceStatusResponse::ManagedGit {
                    latest_sync: Some(sync),
                    ..
                } = status
                    && sync.request_origin == SourceSyncRequestOrigin::Poll
                    && sync.outcome == Some(SourceSyncOutcome::Applied)
                    && sync.source_commit.as_deref() == Some(pushed_commit)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("the pushed commit must be installed by a bounded background poll");
    }

    async fn wait_for_manual_no_change(&self, source_sync_id: &str) {
        tokio::time::timeout(REQUEST_LIMIT, async {
            loop {
                let response = self
                    .admin_get(&format!("{}/{source_sync_id}", SOURCE_SYNCS_PATH))
                    .await;
                assert_eq!(response.status(), StatusCode::OK);
                let sync: SourceSyncResource = response_json(response).await;
                if sync.outcome == Some(SourceSyncOutcome::NoChange) {
                    assert_eq!(sync.request_origin, SourceSyncRequestOrigin::Manual);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("the native admin sync must reach a durable no-change result");
    }

    fn stop(self) {
        let Self {
            daemon,
            root,
            work: _,
            ssh_marker: _,
            client: _,
            admin_url: _,
            public_url: _,
            session: _,
        } = self;
        daemon.stop();
        drop(root);
    }
}

struct HumanSession {
    cookie_header: Zeroizing<String>,
    csrf: Zeroizing<String>,
}

async fn password_login(client: &reqwest::Client, admin_url: &str) -> HumanSession {
    let login = CreateAdminSessionRequest::Password {
        username: OWNER_USERNAME.into(),
        password: SecretString::new(OWNER_PASSWORD),
    };
    let body = Bytes::from_owner(Zeroizing::new(
        serde_json::to_vec(&login).expect("password login fixture must serialize"),
    ));
    drop(login);
    let response = client
        .post(format!("{admin_url}{ADMIN_SESSIONS_PATH}"))
        .header(HOST, ADMIN_AUTHORITY)
        .header(ORIGIN, ADMIN_ORIGIN)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .expect("password login request must complete");
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "password login failed"
    );
    session_cookies(response.headers())
}

fn session_cookies(headers: &reqwest::header::HeaderMap) -> HumanSession {
    let mut session = None;
    let mut csrf = None;
    for value in headers.get_all(SET_COOKIE) {
        let pair = value
            .to_str()
            .expect("session cookie must be visible ASCII")
            .split(';')
            .next()
            .expect("session cookie must contain a name and value");
        let (name, value) = pair
            .split_once('=')
            .expect("session cookie must contain a separator");
        match name {
            SESSION_COOKIE_NAME => session = Some(value.to_owned()),
            CSRF_COOKIE_NAME => csrf = Some(value.to_owned()),
            _ => {}
        }
    }
    let session = Zeroizing::new(session.expect("password login must set the session cookie"));
    let csrf = Zeroizing::new(csrf.expect("password login must set the CSRF cookie"));
    HumanSession {
        cookie_header: Zeroizing::new(format!(
            "{SESSION_COOKIE_NAME}={}; {CSRF_COOKIE_NAME}={}",
            session.as_str(),
            csrf.as_str()
        )),
        csrf,
    }
}

fn initialize_remote(root: &Path, work: &Path, remote: &Path) {
    run_git(
        root,
        [
            OsStr::new("init"),
            OsStr::new("--initial-branch=main"),
            work.as_os_str(),
        ],
    );
    run_git(
        work,
        [
            OsStr::new("config"),
            OsStr::new("user.name"),
            OsStr::new("Maincopy integration test"),
        ],
    );
    run_git(
        work,
        [
            OsStr::new("config"),
            OsStr::new("user.email"),
            OsStr::new("integration@example.test"),
        ],
    );
    write_site(work, INITIAL_TITLE, INITIAL_BODY);
    commit(work, "initial managed content");
    run_git(
        root,
        [OsStr::new("init"), OsStr::new("--bare"), remote.as_os_str()],
    );
    run_git(
        work,
        [
            OsStr::new("remote"),
            OsStr::new("add"),
            OsStr::new("origin"),
            remote.as_os_str(),
        ],
    );
    run_git(
        work,
        [OsStr::new("push"), OsStr::new("origin"), OsStr::new("main")],
    );
}

fn write_site(work: &Path, title: &str, body: &str) {
    fs::create_dir_all(work.join("site/posts"))
        .expect("managed source content directory must be created");
    fs::write(
        work.join("site/publication.toml"),
        "[site]\n\
         title = \"Managed source integration test\"\n\
         base_url = \"https://publication.example.test\"\n\
         description = \"A real managed Git publication fixture.\"\n\
         [author]\n\
         name = \"Integration Tester\"\n\
         [assets]\n\
         allowed_https_origins = []\n",
    )
    .expect("managed source publication fixture must be written");
    fs::write(
        work.join("site/posts/managed.md"),
        format!(
            "+++\n\
             id = \"{POST_ID}\"\n\
             title = {title:?}\n\
             slug = \"managed\"\n\
             authored_at = 2026-09-04T12:00:00Z\n\
             description = \"Managed Git process integration fixture.\"\n\
             draft = false\n\
             +++\n\
             {body}\n"
        ),
    )
    .expect("managed source post fixture must be written");
}

fn commit(work: &Path, message: &str) {
    run_git(work, [OsStr::new("add"), OsStr::new(".")]);
    run_git(
        work,
        [OsStr::new("commit"), OsStr::new("-m"), OsStr::new(message)],
    );
}

fn write_transport_fixture(root: &Path, remote: &Path, marker: &Path) -> PathBuf {
    let exec_path = git_output(root, [OsStr::new("--exec-path")]);
    let upload_pack = Path::new(&exec_path).join("git-upload-pack");
    let fake_ssh = root.join("fixture-ssh");
    fs::write(
        &fake_ssh,
        format!(
            "#!/bin/sh\n\
             test \"$1\" = \"-F\" || exit 90\n\
             test \"$2\" = \"/dev/null\" || exit 91\n\
             case \" $* \" in\n\
               *\" StrictHostKeyChecking=yes \"*) ;;\n\
               *) exit 92 ;;\n\
             esac\n\
             : > {}\n\
             exec {} {}\n",
            shell_literal(marker),
            shell_literal(&upload_pack),
            shell_literal(remote),
        ),
    )
    .expect("constrained SSH transport fixture must be written");
    fs::set_permissions(&fake_ssh, fs::Permissions::from_mode(0o700))
        .expect("constrained SSH transport fixture must be executable");
    fs::canonicalize(fake_ssh).expect("constrained SSH transport path must be canonical")
}

fn shell_literal(path: &Path) -> String {
    let value = path
        .to_str()
        .expect("integration fixture paths must be UTF-8");
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn write_credentials(root: &Path) {
    let credentials = root.join("credentials");
    fs::create_dir(&credentials).expect("credential fixture directory must be created");
    let private_key = credentials.join("source-key");
    fs::write(&private_key, "integration fixture private key\n")
        .expect("private key fixture must be written");
    fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600))
        .expect("private key fixture must be owner-only");
    fs::write(
        credentials.join("known-hosts"),
        "fixture.test ssh-ed25519 fixture\n",
    )
    .expect("known-hosts fixture must be written");
}

fn write_host_file(root: &Path) {
    write_host_file_with_fetch_timeout(root, None);
}

fn write_host_file_with_fetch_timeout(root: &Path, fetch_timeout_seconds: Option<u64>) {
    let fetch_timeout = fetch_timeout_seconds
        .map(|seconds| format!("fetch_timeout_seconds = {seconds}\n"))
        .unwrap_or_default();
    fs::write(
        root.join("maincopy.toml"),
        format!(
            "[paths]\n\
             state_root = \"state\"\n\
             runtime_root = \"run\"\n\
             [public]\n\
             bind = \"127.0.0.1:0\"\n\
             [admin]\n\
             bind = \"127.0.0.1:0\"\n\
             origin = \"{ADMIN_ORIGIN}\"\n\
             [source]\n\
             mode = \"managed_git\"\n\
             mirror_root = \"state/source-mirror\"\n\
             {fetch_timeout}\
             [source.ssh_credentials.deploy]\n\
             private_key_file = \"credentials/source-key\"\n\
             known_hosts_file = \"credentials/known-hosts\"\n"
        ),
    )
    .expect("managed source host fixture must be written");
}

fn bootstrap_password_owner(root: &Path) {
    let child = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args([
            "--config",
            "maincopy.toml",
            "identity",
            "bootstrap",
            "password",
            "--username",
            OWNER_USERNAME,
        ])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("identity bootstrap process must start");
    let mut process = CapturedChild::new(child);
    let password = Zeroizing::new(format!("{OWNER_PASSWORD}\n"));
    process
        .write_stdin(password.as_bytes())
        .expect("identity bootstrap password must be written");
    assert_process_success("identity bootstrap", process);
}

fn configure_source(root: &Path, remote: &Path) {
    let child = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args([
            OsStr::new("--config"),
            OsStr::new("maincopy.toml"),
            OsStr::new("source"),
            OsStr::new("configure"),
            OsStr::new("--user"),
            OsStr::new("git"),
            OsStr::new("--host"),
            OsStr::new("fixture.test"),
            OsStr::new("--repository-path"),
            remote.as_os_str(),
            OsStr::new("--branch"),
            OsStr::new("main"),
            OsStr::new("--content-subdirectory"),
            OsStr::new("site"),
            OsStr::new("--credential-name"),
            OsStr::new("deploy"),
            OsStr::new("--poll-interval-seconds"),
            OsStr::new("30"),
        ])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("offline source configuration process must start");
    assert_process_success("offline source configuration", CapturedChild::new(child));
}

fn assert_process_success(operation: &str, process: CapturedChild) {
    let (completion, stdout, stderr) = process.wait(COMMAND_LIMIT);
    let diagnostic = format!(
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&stdout).replace(OWNER_PASSWORD, "<redacted>"),
        String::from_utf8_lossy(&stderr).replace(OWNER_PASSWORD, "<redacted>")
    );
    assert!(
        completion.wait_error.is_none(),
        "{operation} wait failed: {}: {diagnostic}",
        completion
            .wait_error
            .as_deref()
            .unwrap_or("unknown wait failure")
    );
    assert!(
        !completion.timed_out,
        "{operation} exceeded {COMMAND_LIMIT:?}: {diagnostic}"
    );
    assert!(
        completion.termination_error.is_none(),
        "{operation} could not be killed after timeout: {}: {diagnostic}",
        completion
            .termination_error
            .as_deref()
            .unwrap_or("unknown termination failure")
    );
    assert!(
        completion
            .status
            .as_ref()
            .unwrap_or_else(|error| panic!("{operation} could not be reaped: {error}"))
            .success(),
        "{operation} failed: {diagnostic}"
    );
}

fn run_git<Arguments, Argument>(directory: &Path, arguments: Arguments)
where
    Arguments: IntoIterator<Item = Argument>,
    Argument: AsRef<OsStr>,
{
    let status = Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("Git fixture command must start");
    assert!(status.success(), "Git fixture command failed");
}

fn git_output<Arguments, Argument>(directory: &Path, arguments: Arguments) -> String
where
    Arguments: IntoIterator<Item = Argument>,
    Argument: AsRef<OsStr>,
{
    let child = Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Git fixture query must start");
    let process = CapturedChild::new(child);
    let (completion, stdout, stderr) = process.wait(COMMAND_LIMIT);
    assert!(
        completion
            .status
            .as_ref()
            .is_ok_and(std::process::ExitStatus::success),
        "Git fixture query failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    String::from_utf8(stdout)
        .expect("Git fixture query must return UTF-8")
        .trim()
        .to_owned()
}

fn preview_path() -> String {
    format!("{POSTS_PATH}/{POST_ID}/preview")
}

fn preview_digest(response: &Response) -> PreviewDigest {
    PreviewDigest::parse(
        response.headers()[PREVIEW_DIGEST_HEADER]
            .to_str()
            .expect("preview digest header must be ASCII"),
    )
    .expect("preview digest header must be canonical")
}

async fn response_json<Value>(response: Response) -> Value
where
    Value: DeserializeOwned,
{
    serde_json::from_slice(&response_bytes(response).await)
        .expect("managed source response must contain valid JSON")
}

async fn problem_code(response: Response) -> String {
    let problem: serde_json::Value = response_json(response).await;
    problem["error"]["code"]
        .as_str()
        .expect("managed source problem must contain a string code")
        .to_owned()
}

async fn response_text(response: Response) -> String {
    String::from_utf8(response_bytes(response).await)
        .expect("managed source response must contain UTF-8")
}

async fn response_bytes(mut response: Response) -> Vec<u8> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BODY_BYTES as u64)
    {
        panic!("managed source response declared an oversized body");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .expect("managed source response body must be readable")
    {
        let received = body
            .len()
            .checked_add(chunk.len())
            .expect("managed source response body length must fit usize");
        assert!(
            received <= MAX_RESPONSE_BODY_BYTES,
            "managed source response exceeded its body limit"
        );
        body.extend_from_slice(&chunk);
    }
    body
}
