use super::{
    profile_workflows::{browser_page, browser_request, operation_id},
    *,
};

const NEW_PASSWORD: &str = "a different correct password";

async fn login(harness: &AdminProcessHarness, username: &str, password: &str) -> Response {
    let login = CreateAdminSessionRequest::Password {
        username: username.into(),
        password: SecretString::new(password),
    };
    let body = Bytes::from_owner(Zeroizing::new(serde_json::to_vec(&login).unwrap()));
    drop(login);
    harness
        .client
        .post(format!("{}{ADMIN_SESSIONS_PATH}", harness.admin_url))
        .header(HOST, ADMIN_AUTHORITY)
        .header(ORIGIN, ADMIN_ORIGIN)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn create_password_user(
    harness: &AdminProcessHarness,
    session: &HumanSession,
    username: &str,
    role: &str,
) -> String {
    let page = browser_page(harness, session, "/admin/users").await;
    let fields = [
        ("operation_id", operation_id(&page)),
        ("provider", "password"),
        ("role", role),
        ("username", username),
        ("password", NEW_PASSWORD),
        ("confirmation", NEW_PASSWORD),
    ];
    let created = browser_request(harness, session, Method::POST, "/admin/users", &fields).await;
    assert_eq!(created.status(), StatusCode::SEE_OTHER);
    let location = created.headers()["location"].to_str().unwrap().to_owned();
    let replay = browser_request(harness, session, Method::POST, "/admin/users", &fields).await;
    assert_eq!(replay.status(), StatusCode::SEE_OTHER);
    assert_eq!(replay.headers()["location"], location);
    location.strip_prefix("/admin/users/").unwrap().to_owned()
}

#[tokio::test]
async fn browser_accounts_preserve_role_versions_and_revoke_disabled_user_access() {
    let harness = AdminProcessHarness::start().await;
    let (_, owner) = password_login(&harness.client, &harness.admin_url).await;
    let user_id = create_password_user(&harness, &owner, "new-publisher", "publisher").await;
    let user_path = format!("/admin/users/{user_id}");
    let api_path = format!("{ADMIN_USERS_PATH}/{user_id}");
    let page = browser_page(&harness, &owner, &user_path).await;
    assert!(page.contains("new-publisher"));
    assert!(!page.contains(NEW_PASSWORD));
    let response = login(&harness, "new-publisher", NEW_PASSWORD).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let publisher = session_cookies(response.headers());
    let agent = SigningKey::from_bytes(&[51; 32]).unwrap();
    let registered = harness.send_json(Method::POST, ADMIN_AGENT_CREDENTIALS_PATH, json!({
        "owner_user_id":user_id, "public_key":public_key(&agent), "label":"publisher helper",
        "scopes":["content_read"], "expires_at":null,
    })).await;
    assert_eq!(registered.status(), StatusCode::CREATED);
    for path in ["/admin/users", &user_path, "/admin/profile", "/admin/tips"] {
        assert_eq!(
            browser_request(&harness, &publisher, Method::GET, path, &[])
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    for path in [
        "/admin/users".to_owned(),
        format!("{user_path}/status"),
        format!("{user_path}/roles"),
        format!("{user_path}/password"),
        format!("{user_path}/nostr"),
        format!("{user_path}/credentials/password/remove"),
    ] {
        assert_eq!(
            browser_request(&harness, &publisher, Method::POST, &path, &[])
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let role_fields = [
        ("operation_id", operation_id(&page)),
        ("expected_version", "1"),
        ("role", "administrator"),
    ];
    assert_eq!(
        browser_request(
            &harness,
            &owner,
            Method::POST,
            &format!("{user_path}/roles"),
            &role_fields
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let page = browser_page(&harness, &owner, &user_path).await;
    let stale_fields = [
        ("operation_id", operation_id(&page)),
        ("expected_version", "1"),
        ("role", "publisher"),
    ];
    let stale = browser_request(
        &harness,
        &owner,
        Method::POST,
        &format!("{user_path}/roles"),
        &stale_fields,
    )
    .await;
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
    assert!(stale.text().await.unwrap().contains("page is out of date"));
    let user = response_json(harness.get(&api_path).await).await;
    assert_eq!(user["version"], 2);
    assert_eq!(user["roles"], json!(["administrator"]));
    // The same session sees current roles; it cannot assign an Owner or manage one.
    let admin_page = browser_page(&harness, &publisher, "/admin/users").await;
    assert!(!admin_page.contains("value=\"owner\""));
    assert!(!admin_page.contains("value=\"administrator\""));
    let escalate = [
        ("operation_id", operation_id(&admin_page)),
        ("provider", "nostr"),
        ("role", "owner"),
        ("public_key", &test_public_key(52)),
    ];
    assert_eq!(
        browser_request(
            &harness,
            &publisher,
            Method::POST,
            "/admin/users",
            &escalate
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let owner_path = format!("/admin/users/{}/status", harness.owner_user_id);
    assert_eq!(
        browser_request(
            &harness,
            &publisher,
            Method::POST,
            &owner_path,
            &[
                ("operation_id", &Uuid::new_v4().to_string()),
                ("expected_version", "1"),
                ("status", "disabled"),
            ]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let fields = [
        ("operation_id", operation_id(&page)),
        ("expected_version", "2"),
        ("status", "disabled"),
    ];
    assert_eq!(
        browser_request(
            &harness,
            &owner,
            Method::POST,
            &format!("{user_path}/status"),
            &fields
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        browser_request(&harness, &publisher, Method::GET, "/admin/users", &[])
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        harness
            .send_json_as(&agent, Method::GET, "/api/admin/v1/posts", json!({}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let page = browser_page(&harness, &owner, &user_path).await;
    assert!(page.contains("Enable user"));
    assert_eq!(
        browser_request(
            &harness,
            &owner,
            Method::POST,
            &format!("{user_path}/status"),
            &[
                ("operation_id", operation_id(&page)),
                ("expected_version", "3"),
                ("status", "enabled"),
            ]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        login(&harness, "new-publisher", NEW_PASSWORD)
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        harness
            .send_json_as(&agent, Method::GET, "/api/admin/v1/posts", json!({}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        response_json(harness.get(ADMIN_USERS_PATH).await).await["users"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    harness.stop();
}

#[tokio::test]
async fn browser_password_rotation_preserves_stale_sessions_and_revokes_them_only_after_acceptance()
{
    let harness = AdminProcessHarness::start().await;
    let (_, first) = password_login(&harness.client, &harness.admin_url).await;
    let (_, second) = password_login(&harness.client, &harness.admin_url).await;
    let user_path = format!("/admin/users/{}", harness.owner_user_id);
    let page = browser_page(&harness, &first, &user_path).await;
    let replacement = "🦀".repeat(128);
    assert!(page.contains("maxlength=\"256\""));
    let operation = operation_id(&page);
    let fields = [
        ("operation_id", operation),
        ("expected_version", "99"),
        ("username", OWNER_USERNAME),
        ("password", replacement.as_str()),
        ("confirmation", replacement.as_str()),
    ];
    assert_eq!(
        browser_request(
            &harness,
            &first,
            Method::POST,
            &format!("{user_path}/password"),
            &fields
        )
        .await
        .status(),
        StatusCode::PRECONDITION_FAILED
    );
    browser_page(&harness, &first, &user_path).await;
    browser_page(&harness, &second, &user_path).await;
    let fields = [
        ("operation_id", operation),
        ("expected_version", "1"),
        ("username", OWNER_USERNAME),
        ("password", replacement.as_str()),
        ("confirmation", replacement.as_str()),
    ];
    assert_eq!(
        browser_request(
            &harness,
            &first,
            Method::POST,
            &format!("{user_path}/password"),
            &fields
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    for session in [&first, &second] {
        let response = browser_request(&harness, session, Method::GET, &user_path, &[]).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()["location"], "/admin/login");
    }
    assert_eq!(
        login(&harness, OWNER_USERNAME, OWNER_PASSWORD)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = login(&harness, OWNER_USERNAME, replacement.as_str()).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let session = session_cookies(response.headers());
    // Receipts bind to the authorizing session. After signing in again, inspect
    // the accepted version instead of resubmitting the previous session's form.
    assert_eq!(
        browser_request(
            &harness,
            &session,
            Method::POST,
            &format!("{user_path}/password"),
            &fields
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let page = browser_page(&harness, &session, &user_path).await;
    assert!(!page.contains(replacement.as_str()));
    let user = response_json(
        harness
            .get(&format!("{ADMIN_USERS_PATH}/{}", harness.owner_user_id))
            .await,
    )
    .await;
    assert_eq!(user["version"], 2);
    assert_eq!(user["credentials"][0]["version"], 2);
    let response = browser_request(
        &harness,
        &session,
        Method::POST,
        &format!("{user_path}/credentials/password/remove"),
        &[
            ("operation_id", operation_id(&page)),
            ("expected_version", "2"),
            ("confirm", "true"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        login(&harness, OWNER_USERNAME, replacement.as_str())
            .await
            .status(),
        StatusCode::CREATED
    );
    harness.stop();
}

#[tokio::test]
async fn browser_nostr_credentials_preserve_the_last_usable_login_method() {
    let harness = AdminProcessHarness::start().await;
    let (_, owner) = password_login(&harness.client, &harness.admin_url).await;
    let page = browser_page(&harness, &owner, "/admin/users").await;
    assert!(page.contains("Create with a Nostr key"));
    let first_key = "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9".to_owned();
    let response = browser_request(
        &harness,
        &owner,
        Method::POST,
        "/admin/users",
        &[
            ("operation_id", operation_id(&page)),
            ("provider", "nostr"),
            ("role", "publisher"),
            ("public_key", &first_key),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let user_path = response.headers()["location"].to_str().unwrap().to_owned();
    let user_id = user_path.strip_prefix("/admin/users/").unwrap();
    let page = browser_page(&harness, &owner, &user_path).await;
    assert!(page.contains(&first_key));
    assert!(page.contains("SHA256:fHnzBx4oNE6BU79sc8KU6+N1SuxOLLjLRHGy9Ey18i0"));
    let second_key = test_public_key(54);
    let replacement = [
        ("operation_id", operation_id(&page)),
        ("expected_version", "1"),
        ("public_key", &second_key),
    ];
    for _ in 0..2 {
        assert_eq!(
            browser_request(
                &harness,
                &owner,
                Method::POST,
                &format!("{user_path}/nostr"),
                &replacement
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
    }
    let page = browser_page(&harness, &owner, &user_path).await;
    assert!(page.contains(&second_key));
    assert!(page.contains("SHA-256 fingerprint"));
    assert!(!page.contains("SHA256:fHnzBx4oNE6BU79sc8KU6+N1SuxOLLjLRHGy9Ey18i0"));
    let removal = [
        ("operation_id", operation_id(&page)),
        ("expected_version", "2"),
        ("confirm", "true"),
    ];
    assert_eq!(
        browser_request(
            &harness,
            &owner,
            Method::POST,
            &format!("{user_path}/credentials/nostr/remove"),
            &removal
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let add_password_operation = Uuid::new_v4().to_string();
    let add_password = [
        ("operation_id", add_password_operation.as_str()),
        ("username", "nostr-with-password"),
        ("password", NEW_PASSWORD),
        ("confirmation", NEW_PASSWORD),
    ];
    assert_eq!(
        browser_request(
            &harness,
            &owner,
            Method::POST,
            &format!("{user_path}/password"),
            &add_password
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let page = browser_page(&harness, &owner, &user_path).await;
    let removal = [
        ("operation_id", operation_id(&page)),
        ("expected_version", "2"),
        ("confirm", "true"),
    ];
    assert_eq!(
        browser_request(
            &harness,
            &owner,
            Method::POST,
            &format!("{user_path}/credentials/nostr/remove"),
            &removal
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let user = response_json(harness.get(&format!("{ADMIN_USERS_PATH}/{user_id}")).await).await;
    assert_eq!(user["version"], 4);
    assert_eq!(user["credentials"].as_array().unwrap().len(), 1);
    assert_eq!(user["credentials"][0]["provider"], "password");
    let page = browser_page(&harness, &owner, &user_path).await;
    assert_eq!(
        browser_request(
            &harness,
            &owner,
            Method::POST,
            &format!("{user_path}/nostr"),
            &[
                ("operation_id", operation_id(&page)),
                ("public_key", &first_key),
            ]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    harness.stop();
}

#[tokio::test]
async fn browser_account_forms_reject_invalid_secrets_and_forbidden_requests_without_reflection() {
    let harness = AdminProcessHarness::start().await;
    let (_, owner) = password_login(&harness.client, &harness.admin_url).await;
    let user_path = format!("/admin/users/{}", harness.owner_user_id);
    let page = browser_page(&harness, &owner, &user_path).await;
    let operation = operation_id(&page);
    let forms = [
        (
            vec![
                ("operation_id", operation),
                ("expected_version", "1"),
                ("username", OWNER_USERNAME),
                ("password", NEW_PASSWORD),
                ("confirmation", "a mismatched password"),
            ],
            StatusCode::BAD_REQUEST,
        ),
        (
            vec![
                ("operation_id", operation),
                ("expected_version", "1"),
                ("username", OWNER_USERNAME),
                ("password", NEW_PASSWORD),
                ("confirmation", NEW_PASSWORD),
                ("unknown", "not accepted"),
            ],
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            vec![
                ("operation_id", operation),
                ("expected_version", "1"),
                ("username", OWNER_USERNAME),
                ("password", "short"),
                ("confirmation", "short"),
            ],
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (fields, expected) in forms {
        let response = browser_request(
            &harness,
            &owner,
            Method::POST,
            &format!("{user_path}/password"),
            &fields,
        )
        .await;
        assert_eq!(response.status(), expected);
        let body = response.text().await.unwrap();
        assert!(!body.contains(NEW_PASSWORD));
        assert!(!body.contains("a mismatched password"));
    }
    assert_eq!(
        browser_request(
            &harness,
            &owner,
            Method::POST,
            &format!("{user_path}/credentials/password/remove"),
            &[
                ("operation_id", operation),
                ("expected_version", "1"),
                ("confirm", "false"),
            ]
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        browser_request(
            &harness,
            &owner,
            Method::POST,
            &format!("{user_path}/status"),
            &[
                ("operation_id", operation),
                ("expected_version", "1"),
                ("status", "disabled"),
            ]
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let bad_csrf = HumanSession {
        cookie: owner.cookie.clone(),
        csrf: Zeroizing::new("wrong".to_owned()),
    };
    assert_eq!(
        browser_request(
            &harness,
            &bad_csrf,
            Method::POST,
            &format!("{user_path}/password"),
            &[]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let oversized = "x".repeat(40 * 1024);
    assert_eq!(
        browser_request(
            &harness,
            &owner,
            Method::POST,
            &format!("{user_path}/password"),
            &[("password", &oversized)]
        )
        .await
        .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    for path in ["/admin/users", &user_path] {
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
    assert_eq!(
        login(&harness, OWNER_USERNAME, OWNER_PASSWORD)
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        response_json(harness.get(ADMIN_USERS_PATH).await).await["users"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    harness.stop();
}
