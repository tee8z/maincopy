use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write as _},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::Duration,
};

use axum::body::Bytes;
use base64::{Engine as _, engine::general_purpose};
use k256::schnorr::SigningKey;
use reqwest::{
    Method, Response, StatusCode,
    header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, HOST, ORIGIN, SET_COOKIE},
};
use rustix::{
    io::retry_on_intr,
    process::{Pid, Signal, WaitId, WaitIdOptions, kill_process, waitid},
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroizing;

use maincopy_shared::{
    auth::AdminScope,
    auth_api::{
        ADMIN_AGENT_CREDENTIALS_PATH, ADMIN_AUDIT_EVENTS_PATH, ADMIN_SESSIONS_PATH,
        ADMIN_USERS_PATH, CSRF_COOKIE_NAME, CSRF_HEADER_NAME, CreateAdminSessionRequest,
        SESSION_COOKIE_NAME, SecretString,
    },
    publication::IDEMPOTENCY_KEY_HEADER,
};

const ADMIN_ORIGIN: &str = "https://admin.example.test";
const ADMIN_AUTHORITY: &str = "admin.example.test";
const OWNER_USERNAME: &str = "first-owner";
const OWNER_PASSWORD: &str = "correct horse battery staple";
const NIP98_EVENT_KIND: u64 = 27_235;
const BOOTSTRAP_LIMIT: Duration = Duration::from_secs(20);
const SERVER_START_LIMIT: Duration = Duration::from_secs(20);
const SERVER_STOP_LIMIT: Duration = Duration::from_secs(20);
const FORCED_STOP_LIMIT: Duration = Duration::from_secs(5);
const REQUEST_LIMIT: Duration = Duration::from_secs(10);
const MAX_CAPTURED_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_DAEMON_LOG_LINE_BYTES: usize = 8 * 1024;
const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024;
const READY_MESSAGE: &str = "authenticated admin backend listener bound";
const TEST_PUBLICATION: &str = "[site]\n\
title = \"Admin identity integration test\"\n\
base_url = \"https://publication.example.test\"\n\
description = \"A self-contained publication fixture.\"\n\
[author]\n\
name = \"Integration Tester\"\n";

#[test]
fn admin_readiness_accepts_only_the_configured_ephemeral_loopback_address() {
    assert_eq!(
        admin_address_from_ready_line("INFO public listener bound bind=127.0.0.1:1234"),
        Ok(None)
    );
    assert_eq!(
        admin_address_from_ready_line(
            "INFO authenticated admin backend listener bound bind=127.0.0.1:43123"
        ),
        Ok(Some("127.0.0.1:43123".parse().unwrap()))
    );

    for invalid in [
        "INFO authenticated admin backend listener bound",
        "INFO authenticated admin backend listener bound bind=invalid",
        "INFO authenticated admin backend listener bound bind=127.0.0.1:0",
        "INFO authenticated admin backend listener bound bind=127.0.0.2:43123",
        "INFO authenticated admin backend listener bound bind=[::1]:43123",
    ] {
        assert!(
            admin_address_from_ready_line(invalid).is_err(),
            "unexpectedly accepted {invalid}"
        );
    }
}

#[test]
fn response_body_budget_accepts_the_exact_limit_and_rejects_the_next_byte() {
    let mut body = Vec::new();
    assert!(append_response_chunk(&mut body, &vec![0; MAX_RESPONSE_BODY_BYTES - 1]).is_ok());
    assert!(append_response_chunk(&mut body, &[0]).is_ok());
    assert_eq!(body.len(), MAX_RESPONSE_BODY_BYTES);
    assert!(append_response_chunk(&mut body, &[0]).is_err());
    assert_eq!(body.len(), MAX_RESPONSE_BODY_BYTES);
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

#[tokio::test]
async fn identity_mutations_persist_a_complete_user_and_agent_lifecycle() {
    let harness = AdminProcessHarness::start().await;
    let users = harness.get(ADMIN_USERS_PATH).await;
    assert_eq!(users.status(), StatusCode::OK);
    let users = response_json(users).await;
    assert_eq!(users["users"][0]["user_id"], harness.owner_user_id);
    let first_human_key = test_public_key(6);

    let created = harness
        .send_json(
            Method::POST,
            ADMIN_USERS_PATH,
            json!({
                "status": "disabled",
                "roles": ["publisher"],
                "credentials": [{
                    "provider": "nostr",
                    "public_key": first_human_key,
                }],
            }),
        )
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = response_json(created).await;
    let user_id = created["user_id"].as_str().unwrap().to_owned();
    assert_eq!(created["version"], 1);

    let roles = harness
        .send_json(
            Method::PUT,
            &format!("{ADMIN_USERS_PATH}/{user_id}/roles"),
            json!({
                "expected_version": 1,
                "roles": ["administrator"],
            }),
        )
        .await;
    assert_eq!(roles.status(), StatusCode::OK);
    assert_eq!(response_json(roles).await["version"], 2);

    let replacement_human_key = test_public_key(7);
    let credential = harness
        .send_json(
            Method::PUT,
            &format!("{ADMIN_USERS_PATH}/{user_id}/credentials/nostr"),
            json!({
                "mode": "replace",
                "expected_version": 1,
                "credential": {
                    "provider": "nostr",
                    "public_key": replacement_human_key,
                },
            }),
        )
        .await;
    assert_eq!(credential.status(), StatusCode::OK);
    assert_eq!(response_json(credential).await["version"], 3);

    let removed = harness
        .send_json(
            Method::DELETE,
            &format!("{ADMIN_USERS_PATH}/{user_id}/credentials/nostr"),
            json!({ "expected_version": 2 }),
        )
        .await;
    assert_eq!(removed.status(), StatusCode::OK);
    assert_eq!(response_json(removed).await["version"], 4);

    let registered = harness
        .send_json(
            Method::POST,
            ADMIN_AGENT_CREDENTIALS_PATH,
            json!({
                "owner_user_id": harness.owner_user_id,
                "public_key": test_public_key(8),
                "label": "release helper",
                "scopes": ["content_read", "preview_read"],
                "expires_at": null,
            }),
        )
        .await;
    let registered_status = registered.status();
    let registered = response_json(registered).await;
    assert_eq!(
        registered_status,
        StatusCode::CREATED,
        "unexpected registration response: {registered}"
    );
    let agent_id = registered["agent_credential_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(registered["version"], 1);

    let scopes = harness
        .send_json(
            Method::PUT,
            &format!("{ADMIN_AGENT_CREDENTIALS_PATH}/{agent_id}/scopes"),
            json!({
                "expected_version": 1,
                "scopes": ["content_read"],
            }),
        )
        .await;
    assert_eq!(scopes.status(), StatusCode::OK);
    assert_eq!(response_json(scopes).await["version"], 2);

    let revoked = harness
        .send_json(
            Method::DELETE,
            &format!("{ADMIN_AGENT_CREDENTIALS_PATH}/{agent_id}"),
            json!({ "expected_version": 2 }),
        )
        .await;
    assert_eq!(revoked.status(), StatusCode::OK);
    assert_eq!(response_json(revoked).await["version"], 3);

    let user = harness.get(&format!("{ADMIN_USERS_PATH}/{user_id}")).await;
    assert_eq!(user.status(), StatusCode::OK);
    let user = response_json(user).await;
    assert_eq!(user["status"], "disabled");
    assert_eq!(user["roles"], json!(["administrator"]));
    assert_eq!(user["credentials"], json!([]));
    assert_eq!(user["version"], 4);

    let agent = harness
        .get(&format!("{ADMIN_AGENT_CREDENTIALS_PATH}/{agent_id}"))
        .await;
    assert_eq!(agent.status(), StatusCode::OK);
    let agent = response_json(agent).await;
    assert_eq!(agent["scopes"], json!(["content_read"]));
    assert_eq!(agent["effective_scopes"], json!(["content_read"]));
    assert!(agent["revoked_at"].is_string());
    assert_eq!(agent["version"], 3);

    let audit = harness.get(ADMIN_AUDIT_EVENTS_PATH).await;
    assert_eq!(audit.status(), StatusCode::OK);
    let audit = response_json(audit).await;
    let successful_actions = audit["audit_events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["outcome"] == "succeeded")
        .filter_map(|event| event["action"].as_str())
        .collect::<BTreeSet<_>>();
    for action in [
        "identity.user.create",
        "identity.user.roles.replace",
        "identity.user.credential.put",
        "identity.user.credential.remove",
        "identity.agent.register",
        "identity.agent.scopes.replace",
        "identity.agent.revoke",
    ] {
        assert!(
            successful_actions.contains(action),
            "missing successful audit event for {action}"
        );
    }

    harness.stop();
}

#[tokio::test]
async fn identity_mutations_reject_ambiguous_paths_and_unsafe_inputs() {
    let harness = AdminProcessHarness::start().await;

    for body in [
        json!({
            "status": "disabled",
            "roles": [],
            "credentials": [],
        }),
        json!({
            "status": "disabled",
            "roles": ["publisher"],
            "credentials": [
                { "provider": "nostr", "public_key": test_public_key(9) },
                { "provider": "nostr", "public_key": test_public_key(10) },
            ],
        }),
        json!({
            "status": "disabled",
            "roles": ["publisher"],
            "credentials": [
                { "provider": "nostr", "public_key": test_public_key(11) },
                { "provider": "nostr", "public_key": test_public_key(12) },
                { "provider": "nostr", "public_key": test_public_key(13) },
            ],
        }),
    ] {
        let response = harness
            .send_json(Method::POST, ADMIN_USERS_PATH, body)
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_problem(response, "invalid_identity_request").await;
    }

    let password = harness
        .send_json(
            Method::POST,
            ADMIN_USERS_PATH,
            json!({
                "status": "enabled",
                "roles": ["publisher"],
                "credentials": [{
                    "provider": "password",
                    "username": "writer",
                    "password": "correct horse battery staple",
                }],
            }),
        )
        .await;
    assert_eq!(password.status(), StatusCode::FORBIDDEN);
    assert_problem(password, "fresh_authentication_required").await;

    let noncanonical_user_id = Uuid::new_v4().hyphenated().to_string().to_uppercase();
    let invalid_user = harness
        .send_json(
            Method::PUT,
            &format!("{ADMIN_USERS_PATH}/{noncanonical_user_id}/credentials/nostr"),
            json!({
                "mode": "create",
                "credential": {
                    "provider": "nostr",
                    "public_key": test_public_key(14),
                },
            }),
        )
        .await;
    assert_eq!(invalid_user.status(), StatusCode::BAD_REQUEST);
    assert_problem(invalid_user, "invalid_identity_identifier").await;

    let invalid_provider = harness
        .send_json(
            Method::DELETE,
            &format!("{ADMIN_USERS_PATH}/{}/credentials/email", Uuid::new_v4()),
            json!({ "expected_version": 1 }),
        )
        .await;
    assert_eq!(invalid_provider.status(), StatusCode::BAD_REQUEST);
    assert_problem(invalid_provider, "invalid_identity_identifier").await;

    let invalid_key = harness
        .send_json(
            Method::POST,
            ADMIN_AGENT_CREDENTIALS_PATH,
            json!({
                "owner_user_id": harness.owner_user_id,
                "public_key": "not-a-public-key",
                "label": "invalid",
                "scopes": ["content_read"],
                "expires_at": null,
            }),
        )
        .await;
    assert_eq!(invalid_key.status(), StatusCode::BAD_REQUEST);
    assert_problem(invalid_key, "invalid_identity_request").await;

    let users = harness.get(ADMIN_USERS_PATH).await;
    assert_eq!(users.status(), StatusCode::OK);
    assert_eq!(
        response_json(users).await["users"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let agents = harness.get(ADMIN_AGENT_CREDENTIALS_PATH).await;
    assert_eq!(agents.status(), StatusCode::OK);
    assert_eq!(
        response_json(agents).await["agent_credentials"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    harness.stop();
}

struct AdminProcessHarness {
    daemon: Daemon,
    _root: tempfile::TempDir,
    client: reqwest::Client,
    admin_url: String,
    owner_user_id: String,
    signing_key: SigningKey,
}

impl AdminProcessHarness {
    async fn start() -> Self {
        let root = tempfile::tempdir().expect("admin process root must be created");
        write_host_file(root.path());
        bootstrap_password_owner(root.path());

        let (daemon, admin_address) = Daemon::start(root.path());
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_LIMIT)
            .build()
            .expect("admin integration client must build");
        let admin_url = format!("http://{admin_address}");
        let (owner_user_id, session) = password_login(&client, &admin_url).await;
        let signing_key = SigningKey::from_bytes(&[3_u8; 32])
            .expect("the fixed agent integration key must be valid");
        register_test_agent(&client, &admin_url, &owner_user_id, &session, &signing_key).await;

        Self {
            daemon,
            _root: root,
            client,
            admin_url,
            owner_user_id,
            signing_key,
        }
    }

    async fn get(&self, path: &str) -> Response {
        self.send(Method::GET, path, Vec::new(), false).await
    }

    async fn send_json(&self, method: Method, path: &str, body: Value) -> Response {
        self.send(
            method,
            path,
            serde_json::to_vec(&body).expect("admin request fixture must serialize"),
            true,
        )
        .await
    }

    async fn send(&self, method: Method, path: &str, body: Vec<u8>, json_body: bool) -> Response {
        let idempotency_key = Uuid::new_v4().hyphenated().to_string();
        let authorization =
            agent_authorization(&self.signing_key, &method, path, &body, &idempotency_key);
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.admin_url))
            .header(HOST, ADMIN_AUTHORITY)
            .header(ORIGIN, ADMIN_ORIGIN)
            .header(AUTHORIZATION, authorization)
            .header(IDEMPOTENCY_KEY_HEADER, idempotency_key)
            .body(body);
        if json_body {
            request = request.header(CONTENT_TYPE, "application/json");
        }
        request
            .send()
            .await
            .expect("admin integration request must complete")
    }

    fn stop(self) {
        self.daemon.stop();
    }
}

struct HumanSession {
    cookie: Zeroizing<String>,
    csrf: Zeroizing<String>,
}

async fn password_login(client: &reqwest::Client, admin_url: &str) -> (String, HumanSession) {
    let login = CreateAdminSessionRequest::Password {
        username: OWNER_USERNAME.into(),
        password: SecretString::new(OWNER_PASSWORD),
    };
    let body = Bytes::from_owner(Zeroizing::new(
        serde_json::to_vec(&login).expect("password login fixture must serialize"),
    ));
    drop(login);
    let request = client
        .post(format!("{admin_url}{ADMIN_SESSIONS_PATH}"))
        .header(HOST, ADMIN_AUTHORITY)
        .header(ORIGIN, ADMIN_ORIGIN)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .build()
        .expect("password login request must build");
    let response = client
        .execute(request)
        .await
        .expect("password login request must complete");
    let status = response.status();
    let session = session_cookies(response.headers());
    let body = response_json(response).await;
    assert_eq!(status, StatusCode::CREATED, "password login failed: {body}");
    let owner_user_id = body["user_id"]
        .as_str()
        .expect("password login must identify the owner")
        .to_owned();
    (owner_user_id, session)
}

async fn register_test_agent(
    client: &reqwest::Client,
    admin_url: &str,
    owner_user_id: &str,
    session: &HumanSession,
    signing_key: &SigningKey,
) {
    let body = serde_json::to_vec(&json!({
        "owner_user_id": owner_user_id,
        "public_key": public_key(signing_key),
        "label": "protected process contract test agent",
        "scopes": AdminScope::ALL,
        "expires_at": null,
    }))
    .expect("agent registration fixture must serialize");
    let idempotency_key = Uuid::new_v4().hyphenated().to_string();
    let cookie = Zeroizing::new(format!(
        "{SESSION_COOKIE_NAME}={}; {CSRF_COOKIE_NAME}={}",
        session.cookie.as_str(),
        session.csrf.as_str()
    ));
    let response = client
        .post(format!("{admin_url}{ADMIN_AGENT_CREDENTIALS_PATH}"))
        .header(HOST, ADMIN_AUTHORITY)
        .header(ORIGIN, ADMIN_ORIGIN)
        .header(CONTENT_TYPE, "application/json")
        .header(COOKIE, cookie.as_str())
        .header(CSRF_HEADER_NAME, session.csrf.as_str())
        .header(IDEMPOTENCY_KEY_HEADER, idempotency_key)
        .body(body)
        .send()
        .await
        .expect("agent registration request must complete");
    let status = response.status();
    let body = response_json(response).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "agent registration failed: {body}"
    );
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
    HumanSession {
        cookie: Zeroizing::new(session.expect("password login must set the session cookie")),
        csrf: Zeroizing::new(csrf.expect("password login must set the CSRF cookie")),
    }
}

fn write_host_file(root: &Path) {
    let content_root = root.join("content");
    fs::create_dir(&content_root).expect("admin integration content directory must be created");
    fs::write(content_root.join("publication.toml"), TEST_PUBLICATION)
        .expect("admin integration publication fixture must be written");
    fs::write(
        root.join("maincopy.toml"),
        format!(
            "[paths]\n\
             content_root = \"content\"\n\
             state_root = \"state\"\n\
             runtime_root = \"run\"\n\
             [public]\n\
             bind = \"127.0.0.1:0\"\n\
             [admin]\n\
             bind = \"127.0.0.1:0\"\n\
             origin = \"{ADMIN_ORIGIN}\"\n"
        ),
    )
    .expect("admin integration host file must be written");
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
    let (completion, stdout, stderr) = process.wait(BOOTSTRAP_LIMIT);
    let diagnostic = captured_process_diagnostic(&stdout, &stderr);
    assert!(
        completion.wait_error.is_none(),
        "identity bootstrap wait failed: {}: {diagnostic}",
        completion
            .wait_error
            .as_deref()
            .unwrap_or("unknown wait failure")
    );
    assert!(
        !completion.timed_out,
        "identity bootstrap exceeded {BOOTSTRAP_LIMIT:?}: {diagnostic}"
    );
    assert!(
        completion.termination_error.is_none(),
        "identity bootstrap could not be killed after its timeout: {}: {diagnostic}",
        completion
            .termination_error
            .as_deref()
            .unwrap_or("unknown termination failure")
    );
    assert!(
        completion
            .status
            .as_ref()
            .unwrap_or_else(|error| panic!("identity bootstrap could not be reaped: {error}"))
            .success(),
        "identity bootstrap failed: {diagnostic}"
    );
}

struct CapturedChild {
    child: Child,
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    stopped: bool,
}

impl CapturedChild {
    fn new(child: Child) -> Self {
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

    fn write_stdin(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.child
            .stdin
            .take()
            .expect("captured child stdin must be piped")
            .write_all(bytes)
    }

    fn wait(mut self, limit: Duration) -> (ProcessCompletion, Vec<u8>, Vec<u8>) {
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

struct ProcessCompletion {
    status: Result<ExitStatus, Box<str>>,
    timed_out: bool,
    wait_error: Option<Box<str>>,
    termination_error: Option<Box<str>>,
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

fn captured_process_diagnostic(stdout: &[u8], stderr: &[u8]) -> String {
    format!(
        "stdout: {}; stderr: {}",
        redacted_output(stdout),
        redacted_output(stderr)
    )
}

fn redacted_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace(OWNER_PASSWORD, "<redacted>")
}

fn admin_address_from_ready_line(line: &str) -> Result<Option<SocketAddr>, &'static str> {
    if !line.contains(READY_MESSAGE) {
        return Ok(None);
    }
    let encoded = line
        .split_whitespace()
        .find_map(|field| field.strip_prefix("bind="))
        .ok_or("admin readiness log omitted its bound address")?;
    let address = encoded
        .parse::<SocketAddr>()
        .map_err(|_| "admin readiness log contained an invalid bound address")?;
    if address.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) || address.port() == 0 {
        return Err("admin readiness log contained an unsafe bound address");
    }
    Ok(Some(address))
}

fn drain_daemon_stderr<Reader>(
    mut reader: Reader,
    ready_tx: mpsc::SyncSender<Result<SocketAddr, Box<str>>>,
) -> Vec<u8>
where
    Reader: Read,
{
    let mut ready_tx = Some(ready_tx);
    let mut captured = Vec::new();
    let mut line = Vec::with_capacity(MAX_DAEMON_LOG_LINE_BYTES);
    let mut line_exceeded_limit = false;
    let mut buffer = [0_u8; 4 * 1024];

    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                if !line.is_empty() && !line_exceeded_limit {
                    observe_daemon_ready_line(&line, &mut ready_tx);
                }
                if let Some(sender) = ready_tx.take() {
                    let _ =
                        sender.send(Err("daemon exited before binding its admin listener".into()));
                }
                break;
            }
            Ok(count) => {
                append_captured_bytes(&mut captured, &buffer[..count]);
                for &byte in &buffer[..count] {
                    if byte == b'\n' {
                        if !line_exceeded_limit {
                            observe_daemon_ready_line(&line, &mut ready_tx);
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
    ready_tx: &mut Option<mpsc::SyncSender<Result<SocketAddr, Box<str>>>>,
) {
    let Some(sender) = ready_tx.as_ref() else {
        return;
    };
    let line = String::from_utf8_lossy(line);
    let result = match admin_address_from_ready_line(line.trim_end_matches('\r')) {
        Ok(None) => return,
        Ok(Some(address)) => Ok(address),
        Err(error) => Err(error.into()),
    };
    let _ = sender.send(result);
    *ready_tx = None;
}

struct Daemon {
    child: Child,
    stderr: Option<JoinHandle<Vec<u8>>>,
    stopped: bool,
}

impl Daemon {
    fn start(root: &Path) -> (Self, SocketAddr) {
        let child = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
            .args(["--config", "maincopy.toml"])
            .current_dir(root)
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("admin integration daemon must start");
        let mut daemon = Self {
            child,
            stderr: None,
            stopped: false,
        };
        let stderr = daemon
            .child
            .stderr
            .take()
            .expect("admin integration daemon stderr must be captured");
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<SocketAddr, Box<str>>>(1);
        let stderr = thread::spawn(move || drain_daemon_stderr(stderr, ready_tx));
        daemon.stderr = Some(stderr);
        match ready_rx.recv_timeout(SERVER_START_LIMIT) {
            Ok(Ok(address)) => (daemon, address),
            Ok(Err(message)) => {
                let logs = daemon.force_stop();
                panic!("admin integration daemon did not become ready: {message}: {logs}");
            }
            Err(error) => {
                let logs = daemon.force_stop();
                panic!("admin integration daemon readiness timed out ({error}): {logs}");
            }
        }
    }

    fn stop(mut self) {
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
            "admin integration daemon could not begin graceful shutdown: {}: {logs}",
            graceful_shutdown_error
                .as_deref()
                .unwrap_or("unknown shutdown failure")
        );
        assert!(
            completion.wait_error.is_none(),
            "admin integration daemon wait failed: {}: {logs}",
            completion
                .wait_error
                .as_deref()
                .unwrap_or("unknown wait failure")
        );
        assert!(
            !completion.timed_out,
            "admin integration daemon exceeded its shutdown limit: {logs}"
        );
        assert!(
            completion.termination_error.is_none(),
            "admin integration daemon could not be killed after its shutdown timeout: {}: {logs}",
            completion
                .termination_error
                .as_deref()
                .unwrap_or("unknown termination failure")
        );
        assert!(
            completion
                .status
                .as_ref()
                .unwrap_or_else(|error| {
                    panic!("admin integration daemon could not be reaped: {error}")
                })
                .success(),
            "admin integration daemon failed: {logs}"
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
        redacted_output(&captured)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.force_stop();
        }
    }
}

async fn assert_problem(response: Response, expected_code: &str) {
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .expect("problem request ID must be visible ASCII")
        .to_owned();
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], expected_code);
    assert_eq!(body["error"]["request_id"], request_id);
}

async fn response_json(mut response: Response) -> Value {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BODY_BYTES as u64)
    {
        panic!(
            "admin integration response body declared more than {MAX_RESPONSE_BODY_BYTES} bytes"
        );
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .expect("admin integration response body must be readable")
    {
        append_response_chunk(&mut body, &chunk).unwrap_or_else(|_| {
            panic!("admin integration response body exceeded {MAX_RESPONSE_BODY_BYTES} bytes")
        });
    }
    serde_json::from_slice(&body).expect("admin integration response must be valid JSON")
}

struct ResponseBodyTooLarge;

fn append_response_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), ResponseBodyTooLarge> {
    let received = body
        .len()
        .checked_add(chunk.len())
        .ok_or(ResponseBodyTooLarge)?;
    if received > MAX_RESPONSE_BODY_BYTES {
        return Err(ResponseBodyTooLarge);
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn test_public_key(seed: u8) -> String {
    let signing_key =
        SigningKey::from_bytes(&[seed; 32]).expect("the fixed test signing key must be valid");
    public_key(&signing_key)
}

fn public_key(signing_key: &SigningKey) -> String {
    lower_hex(&signing_key.verifying_key().to_bytes())
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Serialize)]
struct SignedEvent {
    id: String,
    pubkey: String,
    created_at: i64,
    kind: u64,
    tags: Vec<Vec<String>>,
    content: String,
    sig: String,
}

fn agent_authorization(
    signing_key: &SigningKey,
    method: &Method,
    path: &str,
    body: &[u8],
    idempotency_key: &str,
) -> String {
    let created_at = OffsetDateTime::now_utc().unix_timestamp();
    let tags = vec![
        vec!["u".into(), format!("{ADMIN_ORIGIN}{path}")],
        vec!["method".into(), method.as_str().into()],
        vec!["payload".into(), lower_hex(&Sha256::digest(body))],
        vec!["idempotency".into(), idempotency_key.into()],
    ];
    let pubkey = public_key(signing_key);
    let content = String::new();
    let encoded = serde_json::to_vec(&(0, &pubkey, created_at, NIP98_EVENT_KIND, &tags, &content))
        .expect("the test NIP-01 event must serialize");
    let event_id: [u8; 32] = Sha256::digest(encoded).into();
    let signature = signing_key
        .sign_raw(&event_id, &[7_u8; 32])
        .expect("the fixed test key must sign")
        .to_bytes();
    let event = SignedEvent {
        id: lower_hex(&event_id),
        pubkey,
        created_at,
        kind: NIP98_EVENT_KIND,
        tags,
        content,
        sig: lower_hex(&signature),
    };
    format!(
        "Nostr {}",
        general_purpose::STANDARD
            .encode(serde_json::to_vec(&event).expect("the signed test event must serialize"))
    )
}
