use maincopy_shared::profile_api::{ACTIVE_TIP_RECIPIENT_PATH, CURRENT_USER_PROFILE_PATH};

use super::*;

async fn browser_request(
    harness: &AdminProcessHarness,
    session: &HumanSession,
    method: Method,
    path: &str,
    fields: &[(&str, &str)],
) -> Response {
    let cookie = Zeroizing::new(format!(
        "{SESSION_COOKIE_NAME}={}; {CSRF_COOKIE_NAME}={}",
        session.cookie.as_str(),
        session.csrf.as_str(),
    ));
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    form.append_pair("_csrf", session.csrf.as_str());
    form.extend_pairs(fields.iter().copied());
    harness
        .client
        .request(method, format!("{}{path}", harness.admin_url))
        .header(HOST, ADMIN_AUTHORITY)
        .header(ORIGIN, ADMIN_ORIGIN)
        .header(COOKIE, cookie.as_str())
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form.finish())
        .send()
        .await
        .unwrap()
}

async fn browser_page(harness: &AdminProcessHarness, session: &HumanSession, path: &str) -> String {
    let response = browser_request(harness, session, Method::GET, path, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    response.text().await.unwrap()
}

fn operation_id(page: &str) -> &str {
    page.split("name=\"operation_id\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
}

#[tokio::test]
async fn browser_profiles_and_tip_selection_preserve_versions_replay_and_restart() {
    let harness = AdminProcessHarness::start().await;
    let (_, session) = password_login(&harness.client, &harness.admin_url).await;
    let page = browser_page(&harness, &session, "/admin/profile").await;
    assert!(page.contains("not configured a profile"));
    let create = [
        ("operation_id", operation_id(&page)),
        ("display_name", "Alice <Writer>"),
        ("lightning_address", "alice@example.test"),
        ("tips_enabled", "true"),
    ];
    let response =
        browser_request(&harness, &session, Method::POST, "/admin/profile", &create).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()["location"], "/admin/profile");
    let profile = response_json(harness.get(CURRENT_USER_PROFILE_PATH).await).await;
    assert_eq!(profile["version"], 1);
    assert_eq!(profile["display_name"], "Alice <Writer>");
    let page = browser_page(&harness, &session, "/admin/tips").await;
    assert!(page.contains("No tip recipient is selected"));
    let response = browser_request(
        &harness,
        &session,
        Method::POST,
        "/admin/tips",
        &[
            ("operation_id", operation_id(&page)),
            ("expected_version", "1"),
            ("user_id", &harness.owner_user_id),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let page = browser_page(&harness, &session, "/admin/tips").await;
    assert!(page.contains("alice@example.test"));
    assert!(page.contains("Tips are available"));

    // Restart the real daemon with the same SQLite ledger and content artifacts.
    harness.daemon.stop();
    let (daemon, address) = start_admin_daemon(harness._root.path());
    let harness = AdminProcessHarness {
        daemon,
        admin_url: format!("http://{address}"),
        ..harness
    };
    let (_, session) = password_login(&harness.client, &harness.admin_url).await;
    let page = browser_page(&harness, &session, "/admin/tips").await;
    assert!(page.contains("alice@example.test"));
    assert!(page.contains("Tips are available"));

    let page = browser_page(&harness, &session, "/admin/profile").await;
    assert!(page.contains("Alice &lt;Writer&gt;"));
    let change_id = operation_id(&page);
    let stale = browser_request(
        &harness,
        &session,
        Method::POST,
        "/admin/profile",
        &[
            ("operation_id", change_id),
            ("expected_version", "2"),
            ("display_name", "Wrong"),
            ("lightning_address", ""),
            ("tips_enabled", "false"),
        ],
    )
    .await;
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
    let stale_page = stale.text().await.unwrap();
    assert!(stale_page.contains("page is out of date"));
    assert!(stale_page.contains("Request ID:"));
    assert_eq!(
        response_json(harness.get(CURRENT_USER_PROFILE_PATH).await).await,
        profile
    );

    let update = [
        ("operation_id", change_id),
        ("expected_version", "1"),
        ("display_name", ""),
        ("lightning_address", ""),
        ("tips_enabled", "false"),
    ];
    assert_eq!(
        browser_request(&harness, &session, Method::POST, "/admin/profile", &update)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        browser_request(&harness, &session, Method::POST, "/admin/profile", &update)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let current = response_json(harness.get(CURRENT_USER_PROFILE_PATH).await).await;
    assert_eq!(current["version"], 2);
    assert!(current["display_name"].is_null());
    assert!(current["lightning_address"].is_null());
    assert_eq!(current["tips_enabled"], false);
    let page = browser_page(&harness, &session, "/admin/tips").await;
    assert!(page.contains("selected recipient is ineligible"));
    let response = browser_request(
        &harness,
        &session,
        Method::POST,
        "/admin/tips",
        &[
            ("operation_id", operation_id(&page)),
            ("expected_version", "2"),
            ("user_id", ""),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let setting = response_json(harness.get(ACTIVE_TIP_RECIPIENT_PATH).await).await;
    assert_eq!(setting["version"], 3);
    assert!(setting["user_id"].is_null());
    assert!(
        browser_page(&harness, &session, "/admin/tips")
            .await
            .contains("No tip recipient is selected")
    );
    let releases = response_json(harness.get("/api/admin/v1/releases").await).await;
    assert_eq!(releases["releases"], json!([]));
    harness.stop();
}

#[tokio::test]
async fn browser_profile_forms_reject_invalid_input_csrf_and_agent_access() {
    let harness = AdminProcessHarness::start().await;
    let (_, session) = password_login(&harness.client, &harness.admin_url).await;
    for path in ["/admin/profile", "/admin/tips"] {
        assert_eq!(harness.get(path).await.status(), StatusCode::FORBIDDEN);
        let response = harness
            .client
            .get(format!("{}{path}", harness.admin_url))
            .header(HOST, ADMIN_AUTHORITY)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()["location"], "/admin/login");
    }
    let operation = Uuid::new_v4().to_string();
    for invalid in ["UPPER@example.test", "not-an-address", "alice@127.0.0.1"] {
        let response = browser_request(
            &harness,
            &session,
            Method::POST,
            "/admin/profile",
            &[
                ("operation_id", &operation),
                ("display_name", "Alice"),
                ("lightning_address", invalid),
                ("tips_enabled", "true"),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            response
                .text()
                .await
                .unwrap()
                .contains("form was not valid")
        );
    }
    let response = browser_request(
        &harness,
        &session,
        Method::POST,
        "/admin/profile",
        &[
            ("operation_id", &operation),
            ("display_name", &"x".repeat(8192)),
            ("lightning_address", ""),
            ("tips_enabled", "false"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let invalid_session = HumanSession {
        cookie: Zeroizing::new(session.cookie.to_string()),
        csrf: Zeroizing::new("mcc1_".to_owned() + &"0".repeat(64)),
    };
    assert_eq!(
        browser_request(
            &harness,
            &invalid_session,
            Method::POST,
            "/admin/profile",
            &[]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        harness.get(CURRENT_USER_PROFILE_PATH).await.status(),
        StatusCode::NOT_FOUND
    );
    harness.stop();
}

#[tokio::test]
async fn publisher_browser_sessions_cannot_read_or_change_profiles_or_tips() {
    let harness = AdminProcessHarness::start().await;
    let (_, owner_session) = password_login(&harness.client, &harness.admin_url).await;
    let cookie = Zeroizing::new(format!(
        "{SESSION_COOKIE_NAME}={}; {CSRF_COOKIE_NAME}={}",
        owner_session.cookie.as_str(),
        owner_session.csrf.as_str()
    ));
    let response = harness.client.post(format!("{}{ADMIN_USERS_PATH}", harness.admin_url))
        .header(HOST, ADMIN_AUTHORITY).header(ORIGIN, ADMIN_ORIGIN)
        .header(COOKIE, cookie.as_str()).header(CSRF_HEADER_NAME, owner_session.csrf.as_str())
        .header(CONTENT_TYPE, "application/json").header(IDEMPOTENCY_KEY_HEADER, Uuid::new_v4().to_string())
        .body(serde_json::to_vec(&json!({"status":"enabled", "roles":["publisher"], "credentials":[{"provider":"password", "username":"publisher", "password":OWNER_PASSWORD}]})).unwrap())
        .send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let login = CreateAdminSessionRequest::Password {
        username: "publisher".into(),
        password: SecretString::new(OWNER_PASSWORD),
    };
    let response = harness
        .client
        .post(format!("{}{ADMIN_SESSIONS_PATH}", harness.admin_url))
        .header(HOST, ADMIN_AUTHORITY)
        .header(ORIGIN, ADMIN_ORIGIN)
        .header(CONTENT_TYPE, "application/json")
        .body(Bytes::from_owner(Zeroizing::new(
            serde_json::to_vec(&login).unwrap(),
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let publisher = session_cookies(response.headers());
    for path in ["/admin/profile", "/admin/tips"] {
        for method in [Method::GET, Method::POST] {
            let response = browser_request(&harness, &publisher, method, path, &[]).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(response.text().await.unwrap().contains("Request denied"));
        }
    }
    assert_eq!(
        harness.get(CURRENT_USER_PROFILE_PATH).await.status(),
        StatusCode::NOT_FOUND
    );
    let recipient = response_json(harness.get(ACTIVE_TIP_RECIPIENT_PATH).await).await;
    assert_eq!(recipient["version"], 1);
    assert!(recipient["user_id"].is_null());
    harness.stop();
}
