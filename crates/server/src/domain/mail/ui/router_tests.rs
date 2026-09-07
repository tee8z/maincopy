use std::sync::Arc;

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    http::{
        Method, Request, StatusCode,
        header::{CONTENT_TYPE, HOST, LOCATION, ORIGIN},
    },
    response::Response,
};
use markdown_compiler::{PostCollection, PostId, prepare_content};
use time::OffsetDateTime;
use tower::ServiceExt as _;
use uuid::Uuid;

use super::{CampaignId, CampaignState, MailUiAccess, binding};
use crate::{
    admin::test_support::{ADMIN_AUTHORITY, ADMIN_ORIGIN, BrowserSession, ProtectedAdminHarness},
    content_fixtures::{content_tree, post, publication},
    domain::{
        mail::subscriber::{
            BeginFeedbackRun, FeedbackHealth, FeedbackObservation, RecordFeedbackObservation,
            SubscriberMode, SubscriberPolicy,
        },
        publication::activation::PublishNow,
    },
    render::{compile_content_catalog, render_bound_post_preview},
};

const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
const REVIEW_PATH: &str = "/admin/mail/posts/11111111-1111-4111-8111-111111111111/review";
const FIXTURE_PASSWORD: &str = "another correct horse battery staple";

async fn get(router: &Router, browser: &BrowserSession, path: &str) -> Response {
    router
        .clone()
        .oneshot(browser.request(Method::GET, path, Bytes::new()))
        .await
        .unwrap()
}

async fn submit(router: &Router, browser: &BrowserSession, path: &str, body: Bytes) -> Response {
    router
        .clone()
        .oneshot(browser.request(Method::POST, path, body))
        .await
        .unwrap()
}

async fn text(response: Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

fn fields(values: &[(&str, &str)]) -> Bytes {
    let mut body = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in values {
        body.append_pair(name, value);
    }
    Bytes::from(body.finish())
}

fn input(markup: &str, name: &str) -> String {
    let prefix = format!("name=\"{name}\" value=\"");
    let mut values = markup.split(&prefix).skip(1);
    let value = values
        .next()
        .expect("the form contains this required input")
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    assert!(values.next().is_none(), "the input is unambiguous");
    value
}

fn draft_body(markup: &str) -> Bytes {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    for name in [
        "_csrf",
        "idempotency_key",
        "proposed_id",
        "revision",
        "snapshot",
        "site_version",
        "content_digest",
        "configuration_binding",
    ] {
        form.append_pair(name, &input(markup, name));
    }
    Bytes::from(form.finish())
}

async fn apply_article(harness: &ProtectedAdminHarness, title: &str) {
    let tree = content_tree(
        publication("publication.toml", "[site]\ntitle = \"Admin test\"\nbase_url = \"https://example.test/\"\ndescription = \"Admin router fixture.\"\n[author]\nname = \"Test author\"\n".into()),
        vec![post("posts/article.md", PostCollection::Posts, format!("+++\nid = \"{POST_ID}\"\ntitle = \"{title}\"\nslug = \"article\"\nauthored_at = 2026-09-01T00:00:00Z\ndescription = \"A <script>description & summary.\"\ndraft = false\n+++\n\nA public article.\n"))],
        vec![], 0,
    );
    let catalog = Arc::new(compile_content_catalog(&prepare_content(&tree).unwrap()).unwrap());
    harness
        .runtime
        .state
        .publications
        .apply_content_catalog(catalog, tree.digest(), None)
        .await
        .unwrap();
}

async fn publish_article(harness: &ProtectedAdminHarness) {
    let handle = &harness.runtime.state.publications;
    let projection = handle.read();
    let post_id = PostId::parse(POST_ID).unwrap();
    let preview = render_bound_post_preview(
        &projection.catalog,
        projection.frontend,
        &post_id,
        projection.tip_recipient.as_ref(),
        &format!("/api/admin/v1/preview-assets/{}", projection.content_digest),
        projection
            .ledger
            .published_post(&post_id)
            .map(|entry| entry.published_at),
    )
    .unwrap()
    .unwrap();
    handle
        .publish_now(PublishNow {
            creation_key: Uuid::new_v4(),
            publication_id: Uuid::new_v4(),
            stable_post_id: post_id,
            expected_revision: Some(preview.revision),
            accepted_preview_digest: preview.digest,
        })
        .await
        .unwrap();
}

async fn create_owner(router: &Router, browser: &BrowserSession) -> String {
    let body = fields(&[
        ("_csrf", browser.as_csrf_token()),
        ("operation_id", &Uuid::new_v4().to_string()),
        ("provider", "password"),
        ("role", "owner"),
        ("username", "second-owner"),
        ("password", FIXTURE_PASSWORD),
        ("confirmation", FIXTURE_PASSWORD),
    ]);
    let response = submit(router, browser, "/admin/users", body).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    response.headers()[LOCATION].to_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mail_history_observes_policy_changes_and_latched_feedback_without_rebuilding_the_router() {
    let mut harness = ProtectedAdminHarness::start_with_password().await;
    let binding = binding();
    let view = binding.configuration.view();
    let mut policy = SubscriberPolicy {
        configuration_binding: binding.configuration_binding,
        mode: SubscriberMode::Enabled,
        max_daily_messages: view.max_daily_messages,
        max_daily_confirmations: view.max_daily_confirmation_messages,
        max_campaign_recipients: view.max_campaign_recipients,
    };
    harness.runtime.mail.access = MailUiAccess::ReviewOnly(binding);
    let router = harness.router();
    let browser = harness.password_login(&router).await;
    let subscribers = &harness.runtime.mail.subscribers;
    let initial = text(get(&router, &browser, "/admin/mail").await).await;
    assert!(initial.contains("Subscription controls have not been configured."));
    subscribers.initialize_controls([4; 32]).await.unwrap();
    subscribers.set_policy(policy).await.unwrap();
    let run = subscribers
        .begin_feedback_run(BeginFeedbackRun {
            provider_now: OffsetDateTime::now_utc(),
            configuration_binding: policy.configuration_binding,
            source_binding: [6; 32],
            retention_seconds: 1_209_600,
        })
        .await
        .unwrap();
    subscribers
        .record_feedback_observation(RecordFeedbackObservation {
            configuration_binding: policy.configuration_binding,
            run_id: run.run_id,
            source_binding: [6; 32],
            retention_seconds: 1_209_600,
            observation: FeedbackObservation::Observed {
                provider_now: OffsetDateTime::now_utc(),
                drained: true,
            },
        })
        .await
        .unwrap();
    let healthy = text(get(&router, &browser, "/admin/mail").await).await;
    assert!(healthy.contains("Mail policy is enabled."));
    assert!(healthy.contains("Delivery feedback is current."));
    assert!(healthy.contains("Last successful feedback check:"));
    assert!(healthy.contains("<dt>Confirmed subscriptions</dt><dd>0</dd>"));
    assert!(!healthy.contains("Sending is ready."));

    subscribers
        .record_feedback_health(policy.configuration_binding, FeedbackHealth::Unavailable)
        .await
        .unwrap();
    let unavailable = text(get(&router, &browser, "/admin/mail").await).await;
    assert!(unavailable.contains("Delivery feedback is unavailable or out of date."));
    assert!(!unavailable.contains("Delivery feedback is current."));

    policy.mode = SubscriberMode::Paused;
    subscribers.set_policy(policy).await.unwrap();
    let paused = text(get(&router, &browser, "/admin/mail").await).await;
    assert!(paused.contains("New subscriptions and sending are paused."));
    assert!(!paused.contains("Mail policy is enabled."));

    policy.mode = SubscriberMode::Enabled;
    subscribers.set_policy(policy).await.unwrap();
    subscribers
        .record_feedback_health(
            policy.configuration_binding,
            FeedbackHealth::ReconciliationRequired,
        )
        .await
        .unwrap();
    // A later successful poll cannot erase a missing or rejected feedback event.
    subscribers
        .record_feedback_observation(RecordFeedbackObservation {
            configuration_binding: policy.configuration_binding,
            run_id: run.run_id,
            source_binding: [6; 32],
            retention_seconds: 1_209_600,
            observation: FeedbackObservation::Observed {
                provider_now: OffsetDateTime::now_utc(),
                drained: true,
            },
        })
        .await
        .unwrap();
    let latched = text(get(&router, &browser, "/admin/mail").await).await;
    assert!(latched.contains("Delivery feedback requires reconciliation."));
    assert!(!latched.contains("Delivery feedback is current."));
    drop(router);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mail_routes_require_a_browser_and_recheck_the_current_owner_role() {
    let harness = ProtectedAdminHarness::start_with_password().await;
    let router = harness.router();
    let unauthenticated = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/mail")
                .header(HOST, ADMIN_AUTHORITY)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::SEE_OTHER);
    assert_eq!(unauthenticated.headers()[LOCATION], "/admin/login");
    let agent = router
        .clone()
        .oneshot(harness.request(Method::GET, "/admin/mail", Bytes::new(), None))
        .await
        .unwrap();
    assert_eq!(agent.status(), StatusCode::FORBIDDEN);
    for method in [Method::GET, Method::POST] {
        let response = router
            .clone()
            .oneshot(harness.request(method, "/admin/mail/recovery", Bytes::new(), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let owner = harness.password_login(&router).await;
    for path in [
        "/email/subscribe",
        "/email/confirm/private-token",
        "/email/unsubscribe/private-token",
    ] {
        assert_eq!(
            get(&router, &owner, path).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            submit(
                &router,
                &owner,
                path,
                fields(&[("_csrf", owner.as_csrf_token())])
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
    let response = get(&router, &owner, "/admin/mail").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    assert!(
        response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("script-src 'none'")
    );
    let account = create_owner(&router, &owner).await;
    let second = harness
        .password_login_as(&router, "second-owner", FIXTURE_PASSWORD)
        .await;
    assert_eq!(
        get(&router, &second, "/admin/mail").await.status(),
        StatusCode::OK
    );
    let demote = fields(&[
        ("_csrf", owner.as_csrf_token()),
        ("operation_id", &Uuid::new_v4().to_string()),
        ("expected_version", "1"),
        ("role", "publisher"),
    ]);
    assert_eq!(
        submit(&router, &owner, &format!("{account}/roles"), demote)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        get(&router, &second, "/admin/mail").await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(&router, &second, REVIEW_PATH).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(&router, &second, "/admin/mail/recovery").await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        submit(
            &router,
            &second,
            "/admin/mail/recovery",
            fields(&[("_csrf", second.as_csrf_token())])
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let disable = fields(&[
        ("_csrf", owner.as_csrf_token()),
        ("operation_id", &Uuid::new_v4().to_string()),
        ("expected_version", "2"),
        ("status", "disabled"),
    ]);
    assert_eq!(
        submit(&router, &owner, &format!("{account}/status"), disable)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        get(&router, &second, "/admin/mail").await.status(),
        StatusCode::SEE_OTHER
    );
    drop(router);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_mail_mutation_enforces_origin_csrf_session_and_native_form_bounds() {
    let harness = ProtectedAdminHarness::start_with_password().await;
    let router = harness.router();
    let browser = harness.password_login(&router).await;
    for path in [
        REVIEW_PATH.to_owned(),
        "/admin/mail/recovery".to_owned(),
        format!("/admin/mail/campaigns/{}/approve", Uuid::new_v4()),
        format!("/admin/mail/campaigns/{}/cancel", Uuid::new_v4()),
    ] {
        let no_session = Request::builder()
            .method(Method::POST)
            .uri(&path)
            .header(HOST, ADMIN_AUTHORITY)
            .header(ORIGIN, ADMIN_ORIGIN)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("_csrf=missing-session"))
            .unwrap();
        assert_eq!(
            router.clone().oneshot(no_session).await.unwrap().status(),
            StatusCode::SEE_OTHER
        );
        assert_eq!(
            submit(&router, &browser, &path, Bytes::new())
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            submit(&router, &browser, &path, Bytes::from("_csrf=wrong-token"))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        let mut wrong_origin = browser.request(
            Method::POST,
            &path,
            fields(&[("_csrf", browser.as_csrf_token())]),
        );
        wrong_origin
            .headers_mut()
            .insert(ORIGIN, "https://untrusted.example".parse().unwrap());
        assert_eq!(
            router.clone().oneshot(wrong_origin).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let duplicate = fields(&[
            ("_csrf", browser.as_csrf_token()),
            ("_csrf", browser.as_csrf_token()),
        ]);
        assert_eq!(
            submit(&router, &browser, &path, duplicate).await.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            submit(
                &router,
                &browser,
                &path,
                fields(&[("_csrf", browser.as_csrf_token())])
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        let oversized = fields(&[
            ("_csrf", browser.as_csrf_token()),
            ("padding", &"x".repeat(4096)),
        ]);
        assert_eq!(
            submit(&router, &browser, &path, oversized).await.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
    assert_eq!(
        get(&router, &browser, "/admin/mail?ignored=1")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(
        harness
            .runtime
            .mail
            .campaigns
            .list(None, 20)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    drop(router);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewed_public_draft_retries_and_cancellation_work_while_sending_is_unavailable() {
    let mut harness = ProtectedAdminHarness::start_with_password().await;
    harness.runtime.mail.access = MailUiAccess::ReviewOnly(binding());
    apply_article(&harness, "Reviewed <public> title").await;
    let router = harness.router();
    let browser = harness.password_login(&router).await;
    assert_eq!(
        get(&router, &browser, REVIEW_PATH).await.status(),
        StatusCode::NOT_FOUND
    );
    publish_article(&harness).await;
    let response = get(&router, &browser, REVIEW_PATH).await;
    assert_eq!(response.status(), StatusCode::OK);
    let markup = text(response).await;
    assert!(markup.contains("Reviewed &lt;public&gt; title"));
    assert!(!markup.contains("<script>description"));
    assert!(markup.contains("sending is not ready"));
    let body = draft_body(&markup);
    let id = CampaignId(Uuid::parse_str(&input(&markup, "proposed_id")).unwrap());
    // A later private candidate must not leak into the public announcement.
    apply_article(&harness, "PRIVATE candidate title").await;
    let choices = text(get(&router, &browser, "/admin/mail").await).await;
    assert!(choices.contains("Reviewed &lt;public&gt; title"));
    assert!(!choices.contains("PRIVATE candidate title"));
    let created = submit(&router, &browser, REVIEW_PATH, body.clone()).await;
    assert_eq!(created.status(), StatusCode::SEE_OTHER);
    let path = created.headers()[LOCATION].to_str().unwrap().to_owned();
    assert_eq!(path, format!("/admin/mail/campaigns/{}", id.0));
    let draft = harness
        .runtime
        .mail
        .campaigns
        .campaign(id)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(draft.state, CampaignState::Draft));
    assert_eq!(draft.content.subject, "Reviewed <public> title");
    let approve = fields(&[
        ("_csrf", browser.as_csrf_token()),
        ("idempotency_key", &Uuid::new_v4().to_string()),
        ("expected_version", "1"),
        (
            "configuration_binding",
            &input(&markup, "configuration_binding"),
        ),
    ]);
    assert_eq!(
        submit(
            &router,
            &browser,
            &format!("{path}/approve"),
            approve.clone()
        )
        .await
        .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let other_session = harness.password_login(&router).await;
    let other_body = Bytes::from(
        std::str::from_utf8(&body)
            .unwrap()
            .replace(browser.as_csrf_token(), other_session.as_csrf_token()),
    );
    assert_eq!(
        submit(&router, &other_session, REVIEW_PATH, other_body)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    // Losing mail configuration must not lose an accepted operation or prevent cancellation.
    harness.runtime.mail.access = MailUiAccess::Unavailable;
    let unavailable = harness.router();
    assert_eq!(
        submit(&unavailable, &browser, REVIEW_PATH, body.clone())
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        submit(&unavailable, &browser, &format!("{path}/approve"), approve)
            .await
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let cancel = fields(&[
        ("_csrf", browser.as_csrf_token()),
        ("idempotency_key", &Uuid::new_v4().to_string()),
        ("expected_version", "1"),
    ]);
    for _ in 0..2 {
        let response = submit(
            &unavailable,
            &browser,
            &format!("{path}/cancel"),
            cancel.clone(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[LOCATION], path);
    }
    assert_eq!(
        submit(&unavailable, &browser, REVIEW_PATH, body)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let cancelled = harness
        .runtime
        .mail
        .campaigns
        .campaign(id)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(cancelled.state, CampaignState::Cancelled { .. }));
    assert_eq!(u64::from(cancelled.version), 2);
    let current = text(get(&unavailable, &browser, &path).await).await;
    assert!(current.contains("This campaign is cancelled"));
    assert!(!current.contains("Approve sending"));
    drop((router, unavailable));
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_public_review_and_tampered_binding_cannot_create_campaigns() {
    let mut harness = ProtectedAdminHarness::start_with_password().await;
    harness.runtime.mail.access = MailUiAccess::ReviewOnly(binding());
    apply_article(&harness, "Original article").await;
    publish_article(&harness).await;
    let router = harness.router();
    let browser = harness.password_login(&router).await;
    let markup = text(get(&router, &browser, REVIEW_PATH).await).await;
    let body = draft_body(&markup);
    let tampered = Bytes::from(
        std::str::from_utf8(&body)
            .unwrap()
            .replace(&input(&markup, "configuration_binding"), &"e".repeat(64)),
    );
    assert_eq!(
        submit(&router, &browser, REVIEW_PATH, tampered)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    apply_article(&harness, "Newly published title").await;
    publish_article(&harness).await;
    assert_eq!(
        submit(&router, &browser, REVIEW_PATH, body).await.status(),
        StatusCode::CONFLICT
    );
    assert!(
        harness
            .runtime
            .mail
            .campaigns
            .list(None, 20)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let current = text(get(&router, &browser, REVIEW_PATH).await).await;
    assert!(current.contains("Newly published title"));
    drop(router);
    harness.stop().await;
}

#[tokio::test]
async fn feedback_reset_requires_explicit_owner_confirmation_and_recovers_the_original_receipt() {
    let mut harness = ProtectedAdminHarness::start_with_password().await;
    let binding = binding();
    let view = binding.configuration.view();
    let policy = SubscriberPolicy {
        configuration_binding: binding.configuration_binding,
        mode: SubscriberMode::Enabled,
        max_daily_messages: view.max_daily_messages,
        max_daily_confirmations: view.max_daily_confirmation_messages,
        max_campaign_recipients: view.max_campaign_recipients,
    };
    harness.runtime.mail.access = MailUiAccess::ReviewOnly(binding);
    let router = harness.router();
    let browser = harness.password_login(&router).await;
    let subscribers = &harness.runtime.mail.subscribers;
    subscribers.initialize_controls([4; 32]).await.unwrap();
    subscribers.set_policy(policy).await.unwrap();
    let available = text(get(&router, &browser, "/admin/mail/recovery").await).await;
    assert!(!available.contains("name=\"confirmation\""));
    subscribers
        .record_feedback_health(
            policy.configuration_binding,
            FeedbackHealth::ReconciliationRequired,
        )
        .await
        .unwrap();
    let before = subscribers.status().await.unwrap();
    let review = text(get(&router, &browser, "/admin/mail/recovery").await).await;
    assert_eq!(subscribers.status().await.unwrap(), before);
    assert!(review.contains("REMOVE SUBSCRIBERS"));
    let csrf = input(&review, "_csrf");
    let key = input(&review, "idempotency_key");
    let version = input(&review, "expected_version");
    let binding = input(&review, "configuration_binding");
    let form = |confirmation: &str| {
        fields(&[
            ("_csrf", &csrf),
            ("idempotency_key", &key),
            ("expected_version", &version),
            ("configuration_binding", &binding),
            ("confirmation", confirmation),
        ])
    };
    assert_eq!(
        submit(&router, &browser, "/admin/mail/recovery", form("yes"))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(subscribers.status().await.unwrap(), before);
    let body = form("REMOVE SUBSCRIBERS");
    let response = submit(&router, &browser, "/admin/mail/recovery", body.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        text(response)
            .await
            .contains("Subscriptions removed; mail is paused")
    );
    let after = subscribers.status().await.unwrap();
    assert_ne!(before.mail_epoch, after.mail_epoch);
    assert_eq!(after.policy.unwrap().mode, SubscriberMode::Paused);
    assert_eq!(
        submit(&router, &browser, "/admin/mail/recovery", body)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(subscribers.status().await.unwrap(), after);
    harness.stop().await;
}
