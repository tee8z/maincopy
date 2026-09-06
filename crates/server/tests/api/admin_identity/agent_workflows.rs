use super::{
    profile_workflows::{browser_page, browser_request, operation_id},
    *,
};

#[tokio::test]
async fn browser_agents_register_replay_replace_scopes_and_revoke_access() {
    let harness = AdminProcessHarness::start().await;
    let (_, owner) = password_login(&harness.client, &harness.admin_url).await;
    let signing_key = SigningKey::from_bytes(&[62; 32]).unwrap();
    let key = public_key(&signing_key);
    let page = browser_page(&harness, &owner, "/admin/agents").await;
    let registration = [
        ("operation_id", operation_id(&page)),
        ("owner_user_id", harness.owner_user_id.as_str()),
        ("public_key", &key),
        ("label", "browser <helper>"),
        ("scope", "content_read"),
        ("scope", "release_manage"),
        ("expires_at", ""),
    ];
    for _ in 0..2 {
        let response = browser_request(
            &harness,
            &owner,
            Method::POST,
            "/admin/agents",
            &registration,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()["location"], "/admin/agents");
    }
    let agents = response_json(harness.get(ADMIN_AGENT_CREDENTIALS_PATH).await).await;
    let agent = agents["agent_credentials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["public_key"] == key)
        .unwrap();
    let id = agent["agent_credential_id"].as_str().unwrap();
    let path = format!("/admin/agents/{id}");
    let page = browser_page(&harness, &owner, &path).await;
    assert!(page.contains("browser &lt;helper&gt;"));
    assert!(page.contains(&key));
    assert!(page.contains("SHA256:"));
    assert!(page.contains("Requested scopes"));
    assert!(page.contains("Effective scopes"));
    assert!(page.contains("Issuer account"));
    let scopes = [
        ("operation_id", operation_id(&page)),
        ("expected_version", "1"),
        ("scope", "content_read"),
    ];
    for _ in 0..2 {
        assert_eq!(
            browser_request(
                &harness,
                &owner,
                Method::POST,
                &format!("{path}/scopes"),
                &scopes
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
    }
    let api_path = format!("{ADMIN_AGENT_CREDENTIALS_PATH}/{id}");
    let current = response_json(harness.get(&api_path).await).await;
    assert_eq!(current["version"], 2);
    assert_eq!(current["scopes"], json!(["content_read"]));
    let page = browser_page(&harness, &owner, &path).await;
    let stale = [
        ("operation_id", operation_id(&page)),
        ("expected_version", "1"),
        ("scope", "release_manage"),
    ];
    let response = browser_request(
        &harness,
        &owner,
        Method::POST,
        &format!("{path}/scopes"),
        &stale,
    )
    .await;
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("page is out of date")
    );
    assert_eq!(
        harness
            .send_json_as(&signing_key, Method::GET, "/api/admin/v1/posts", json!({}))
            .await
            .status(),
        StatusCode::OK
    );
    let page = browser_page(&harness, &owner, &path).await;
    let revoke = [
        ("operation_id", operation_id(&page)),
        ("expected_version", "2"),
        ("confirm", "true"),
    ];
    for _ in 0..2 {
        assert_eq!(
            browser_request(
                &harness,
                &owner,
                Method::POST,
                &format!("{path}/revoke"),
                &revoke
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
    }
    assert_eq!(
        harness
            .send_json_as(&signing_key, Method::GET, "/api/admin/v1/posts", json!({}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let page = browser_page(&harness, &owner, &path).await;
    assert!(page.contains("This grant is revoked"));
    assert!(!page.contains("Replace scopes"));
    let current = response_json(harness.get(&api_path).await).await;
    assert_eq!(current["version"], 3);
    assert!(current["revoked_at"].is_string());
    harness.stop();
}

#[tokio::test]
async fn browser_agents_reject_unsafe_forms_and_require_a_human_session() {
    let harness = AdminProcessHarness::start().await;
    let (_, owner) = password_login(&harness.client, &harness.admin_url).await;
    let id = Uuid::from_u128(1);
    let item = format!("/admin/agents/{id}");
    for (path, status) in [
        ("/admin/agents?limit=0", StatusCode::BAD_REQUEST),
        ("/admin/agents?cursor=bad", StatusCode::BAD_REQUEST),
        ("/admin/agents/bad", StatusCode::BAD_REQUEST),
        (&item, StatusCode::NOT_FOUND),
    ] {
        assert_eq!(
            browser_request(&harness, &owner, Method::GET, path, &[])
                .await
                .status(),
            status
        );
    }
    let operation = Uuid::new_v4().to_string();
    let registration = vec![
        ("operation_id", operation.as_str()),
        ("owner_user_id", harness.owner_user_id.as_str()),
        ("public_key", "bad-key"),
        ("label", "helper"),
        ("scope", "content_read"),
    ];
    assert_eq!(
        browser_request(
            &harness,
            &owner,
            Method::POST,
            "/admin/agents",
            &registration
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    for extra in [
        ("scope", "content_read"),
        ("operation_id", operation.as_str()),
        ("unexpected", "true"),
    ] {
        let mut fields = registration.clone();
        fields.push(extra);
        assert_eq!(
            browser_request(&harness, &owner, Method::POST, "/admin/agents", &fields)
                .await
                .status(),
            if extra.0 == "unexpected" {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::BAD_REQUEST
            }
        );
    }
    for route in [
        "/admin/agents".to_owned(),
        format!("{item}/scopes"),
        format!("{item}/revoke"),
    ] {
        assert_eq!(
            browser_request(&harness, &owner, Method::POST, &route, &[])
                .await
                .status(),
            if route.ends_with("/revoke") {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::BAD_REQUEST
            }
        );
        let bad_csrf = HumanSession {
            cookie: owner.cookie.clone(),
            csrf: Zeroizing::new("wrong".to_owned()),
        };
        assert_eq!(
            browser_request(&harness, &bad_csrf, Method::POST, &route, &[])
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        browser_request(
            &harness,
            &owner,
            Method::POST,
            &format!("{item}/revoke"),
            &[
                ("operation_id", &operation),
                ("expected_version", "1"),
                ("confirm", "false")
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
            "/admin/agents",
            &[("label", &"x".repeat(20 * 1024))]
        )
        .await
        .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    for route in ["/admin/agents", &item] {
        assert_eq!(harness.get(route).await.status(), StatusCode::FORBIDDEN);
        let response = harness
            .client
            .get(format!("{}{route}", harness.admin_url))
            .header(HOST, ADMIN_AUTHORITY)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()["location"], "/admin/login");
    }
    harness.stop();
}
