use std::sync::Arc;

use axum::{
    body::{Bytes, to_bytes},
    http::Method,
};
use markdown_compiler::{PostCollection, prepare_content};
use time::Duration;
use tower::ServiceExt as _;

use super::*;
use crate::{
    admin::test_support::{BrowserSession, ProtectedAdminHarness},
    content_fixtures::{content_tree, post, publication},
    domain::{
        publication::activation::PublishNow,
        sharing::store::{ClaimDelivery, DeliveryOutcome, FinishDelivery},
    },
    render::{compile_content_catalog, render_bound_post_preview},
};

const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
const SHARE: &str = "/admin/sharing/teasers/11111111-1111-4111-8111-111111111111/substack";
const SUBSTACK_SESSION: &str = "s%3Asecret-cookie.signature";

async fn get(router: &Router, browser: &BrowserSession, path: &str) -> Response {
    router
        .clone()
        .oneshot(browser.request(Method::GET, path, Bytes::new()))
        .await
        .unwrap()
}

async fn submit(
    router: &Router,
    browser: &BrowserSession,
    path: &str,
    values: &[(&str, &str)],
) -> Response {
    let mut body = url::form_urlencoded::Serializer::new(String::new());
    body.append_pair("_csrf", browser.as_csrf_token());
    for (name, value) in values {
        body.append_pair(name, value);
    }
    router
        .clone()
        .oneshot(browser.request(Method::POST, path, Bytes::from(body.finish())))
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

fn key() -> String {
    Uuid::new_v4().to_string()
}

async fn save_substack(
    router: &Router,
    browser: &BrowserSession,
    key: &str,
    version: &str,
    mode: &str,
    session: &str,
) -> Response {
    submit(
        router,
        browser,
        "/admin/sharing/substack",
        &[
            ("idempotency_key", key),
            ("expected_version", version),
            ("mode", mode),
            ("subdomain", "https://example.substack.com/publish/home"),
            ("session", session),
        ],
    )
    .await
}

async fn share(router: &Router, browser: &BrowserSession, path: &str, key: &str) -> StatusCode {
    submit(router, browser, path, &[("idempotency_key", key)])
        .await
        .status()
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_settings_are_saved_from_the_page_and_credentials_are_never_rendered() {
    let harness = ProtectedAdminHarness::start_with_password().await;
    let router = harness.router();
    let browser = harness.password_login(&router).await;
    let store = harness.runtime.sharing.store.clone();

    let page = text(get(&router, &browser, "/admin/sharing").await).await;
    assert_eq!(page.matches("Not set up.").count(), 1);
    assert!(page.contains("No article has been published since sharing was added."));

    let response = save_substack(&router, &browser, &key(), "0", "enabled", "").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(text(response).await.contains("No settings were changed."));
    let response = save_substack(&router, &browser, &key(), "0", "enabled", "not-a-cookie").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(text(response).await.contains("substack.sid"));
    assert!(store.substack().await.unwrap().is_none());

    // Pasted wrappers are stripped, and an exact retry writes nothing new.
    let first = key();
    let pasted = format!("substack.sid={SUBSTACK_SESSION};");
    for _ in 0..2 {
        let response = save_substack(&router, &browser, &first, "0", "enabled", &pasted).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(text(response).await.contains("Substack settings saved"));
    }
    let stored = store.substack().await.unwrap().unwrap();
    assert_eq!(stored.version, 1);
    assert_eq!(stored.settings.mode, ChannelMode::Enabled);
    assert_eq!(stored.settings.subdomain.as_str(), "example");
    assert_eq!(stored.settings.session.expose(), SUBSTACK_SESSION);

    let page = text(get(&router, &browser, "/admin/sharing").await).await;
    assert!(page.contains("Enabled. Newly published articles are shared automatically."));
    assert!(page.contains("value=\"example\""));
    assert!(!page.contains("secret-cookie"));

    // A blank cookie keeps the saved one; a stale form is refused.
    let response = save_substack(&router, &browser, &key(), "1", "paused", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let stored = store.substack().await.unwrap().unwrap();
    assert_eq!(stored.version, 2);
    assert_eq!(stored.settings.mode, ChannelMode::Paused);
    assert_eq!(stored.settings.session.expose(), SUBSTACK_SESSION);
    let response = save_substack(&router, &browser, &key(), "1", "enabled", "").await;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let page = text(get(&router, &browser, "/admin/sharing").await).await;
    assert!(page.contains("Paused. Nothing is sent, and queued teasers wait."));
    assert!(!page.contains("secret-cookie"));
    // Posting to X is manual: the page offers no X form or route.
    assert!(!page.contains("/admin/sharing/x"));
    let response = submit(&router, &browser, "/admin/sharing/x", &[]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_publication_records_one_teaser_for_the_channels_enabled_then() {
    let harness = ProtectedAdminHarness::start_with_password().await;
    let router = harness.router();
    let browser = harness.password_login(&router).await;
    let store = harness.runtime.sharing.store.clone();

    apply_article(&harness, "First article").await;
    publish_article(&harness).await;
    apply_article(&harness, "Renamed article").await;
    publish_article(&harness).await;

    // No channel was enabled, so the teaser exists only for posting by hand.
    let recent = store.recent(RECENT_TEASERS).await.unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(
        recent[0].teaser.lead(),
        "First article\n\nA <script>description & summary."
    );
    assert!(recent[0].deliveries.is_empty());

    let page = text(get(&router, &browser, "/admin/sharing").await).await;
    assert!(
        page.contains("First article\n\nA &lt;script&gt;description &amp; summary.</textarea>")
    );
    assert!(page.contains("readonly value=\"https://example.test/posts/article\""));
    assert!(page.contains("Not shared. "));
    assert!(!page.contains("Share on Substack"));

    // An Owner can rewrite the text; text that no longer fits is refused whole.
    let edit_path = SHARE.trim_end_matches("/substack");
    let first = key();
    for _ in 0..2 {
        let response = submit(
            &router,
            &browser,
            edit_path,
            &[
                ("idempotency_key", &first),
                ("text", "A better title\r\n\r\nWritten by hand."),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    let too_long = format!("Title\r\n{}", "x".repeat(300));
    for refused in [too_long.as_str(), " "] {
        let response = submit(
            &router,
            &browser,
            edit_path,
            &[("idempotency_key", &key()), ("text", refused)],
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(text(response).await.contains("Nothing was saved"));
    }
    let recent = store.recent(RECENT_TEASERS).await.unwrap();
    assert_eq!(
        recent[0].teaser.lead(),
        "A better title\n\nWritten by hand."
    );
    assert_eq!(
        recent[0].teaser.view().url,
        "https://example.test/posts/article"
    );
    let page = text(get(&router, &browser, "/admin/sharing").await).await;
    assert!(page.contains("A better title\n\nWritten by hand.</textarea>"));
    let missing = "/admin/sharing/teasers/22222222-2222-4222-8222-222222222222";
    let response = submit(
        &router,
        &browser,
        missing,
        &[("idempotency_key", &key()), ("text", "Title")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // A channel set up later can still be given the teaser, once.
    let response = save_substack(&router, &browser, &key(), "0", "enabled", SUBSTACK_SESSION).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(get(&router, &browser, "/admin/sharing").await).await;
    assert!(page.contains("Share on Substack"));
    let first = key();
    for _ in 0..2 {
        assert_eq!(
            share(&router, &browser, SHARE, &first).await,
            StatusCode::OK
        );
    }
    assert_eq!(
        share(&router, &browser, SHARE, &key()).await,
        StatusCode::CONFLICT
    );
    let recent = store.recent(RECENT_TEASERS).await.unwrap();
    assert_eq!(recent[0].deliveries.len(), 1);
    assert!(matches!(
        recent[0].deliveries[0].state,
        DeliveryState::Queued { .. }
    ));
    let page = text(get(&router, &browser, "/admin/sharing").await).await;
    assert!(page.contains("Queued. "));

    let missing = "/admin/sharing/teasers/22222222-2222-4222-8222-222222222222/substack";
    assert_eq!(
        share(&router, &browser, missing, &key()).await,
        StatusCode::NOT_FOUND
    );
    let unknown = SHARE.replace("/substack", "/x");
    assert_eq!(
        share(&router, &browser, &unknown, &key()).await,
        StatusCode::BAD_REQUEST
    );
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delivery_is_claimed_once_and_follows_its_recorded_outcome() {
    let harness = ProtectedAdminHarness::start_with_password().await;
    let router = harness.router();
    let browser = harness.password_login(&router).await;
    let store = harness.runtime.sharing.store.clone();
    let response = save_substack(&router, &browser, &key(), "0", "enabled", SUBSTACK_SESSION).await;
    assert_eq!(response.status(), StatusCode::OK);
    apply_article(&harness, "First article").await;
    publish_article(&harness).await;

    let post_id = PostId::parse(POST_ID).unwrap();
    let now = OffsetDateTime::now_utc();
    let claim = |settings_version| ClaimDelivery {
        post_id: post_id.clone(),
        channel: Channel::Substack,
        settings_version,
    };
    let finish = |settings_version, outcome| FinishDelivery {
        post_id: post_id.clone(),
        channel: Channel::Substack,
        settings_version,
        outcome,
    };
    let due = |at| store.due(Channel::Substack, at);
    let stale = Err(SharingMutationError::Command(
        SharingCommandError::StaleVersion,
    ));
    let conflict = Err(SharingMutationError::Command(
        SharingCommandError::StateConflict,
    ));

    // One claim at a time, and only under the settings it was read with.
    assert_eq!(store.claim(claim(2)).await, stale);
    store.claim(claim(1)).await.unwrap();
    assert_eq!(store.claim(claim(1)).await, conflict);
    assert!(due(now).await.unwrap().is_none());

    // Refused credentials hold the teaser, uncounted, until new ones are saved.
    store
        .finish(finish(
            1,
            DeliveryOutcome::CredentialsRejected { draft: Some(9) },
        ))
        .await
        .unwrap();
    assert!(
        store
            .substack()
            .await
            .unwrap()
            .unwrap()
            .credentials_rejected
    );
    assert_eq!(store.claim(claim(1)).await, stale);
    let page = text(get(&router, &browser, "/admin/sharing").await).await;
    assert!(page.contains("Substack refused the saved credentials."));
    let response = save_substack(&router, &browser, &key(), "1", "enabled", SUBSTACK_SESSION).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !store
            .substack()
            .await
            .unwrap()
            .unwrap()
            .credentials_rejected
    );
    assert_eq!(due(now).await.unwrap().unwrap().draft, Some(9));

    // A refusal fails the delivery; an Owner can queue it again.
    store.claim(claim(2)).await.unwrap();
    store
        .finish(finish(2, DeliveryOutcome::Failed(DeliveryFailure::Refused)))
        .await
        .unwrap();
    let page = text(get(&router, &browser, "/admin/sharing").await).await;
    assert!(page.contains("Substack refused the post. "));
    assert!(page.contains("Try again on Substack"));
    assert_eq!(
        share(&router, &browser, SHARE, &key()).await,
        StatusCode::OK
    );

    // A delivery left mid-request by a restart fails closed.
    store.claim(claim(2)).await.unwrap();
    assert_eq!(store.fail_interrupted().await, Ok(1));
    assert_eq!(store.fail_interrupted().await, Ok(0));
    let page = text(get(&router, &browser, "/admin/sharing").await).await;
    assert!(page.contains("the post may already exist"));
    assert_eq!(
        share(&router, &browser, SHARE, &key()).await,
        StatusCode::OK
    );

    // A recorded post ends the delivery.
    store.claim(claim(2)).await.unwrap();
    let posted = DeliveryOutcome::Posted {
        url: "https://example.substack.com/p/first-article".to_owned(),
    };
    store.finish(finish(2, posted.clone())).await.unwrap();
    assert_eq!(store.finish(finish(2, posted)).await, conflict);
    let page = text(get(&router, &browser, "/admin/sharing").await).await;
    assert!(page.contains("Posted: <a href=\"https://example.substack.com/p/first-article\""));
    assert_eq!(
        share(&router, &browser, SHARE, &key()).await,
        StatusCode::CONFLICT
    );
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transient_failure_waits_for_its_retry_time() {
    let harness = ProtectedAdminHarness::start_with_password().await;
    let router = harness.router();
    let browser = harness.password_login(&router).await;
    let store = harness.runtime.sharing.store.clone();
    let response = save_substack(&router, &browser, &key(), "0", "enabled", SUBSTACK_SESSION).await;
    assert_eq!(response.status(), StatusCode::OK);
    apply_article(&harness, "First article").await;
    publish_article(&harness).await;

    let post_id = PostId::parse(POST_ID).unwrap();
    let now = OffsetDateTime::now_utc();
    let claim = || ClaimDelivery {
        post_id: post_id.clone(),
        channel: Channel::Substack,
        settings_version: 1,
    };
    store.claim(claim()).await.unwrap();
    store
        .finish(FinishDelivery {
            post_id: post_id.clone(),
            channel: Channel::Substack,
            settings_version: 1,
            outcome: DeliveryOutcome::Retry { draft: Some(7001) },
        })
        .await
        .unwrap();
    let due = |at| store.due(Channel::Substack, at);
    assert!(due(now + Duration::seconds(30)).await.unwrap().is_none());
    let later = due(now + Duration::hours(1)).await.unwrap().unwrap();
    assert_eq!(later.draft, Some(7001));
    assert_eq!(
        store.claim(claim()).await,
        Err(SharingMutationError::Command(
            SharingCommandError::StateConflict
        ))
    );
    let page = text(get(&router, &browser, "/admin/sharing").await).await;
    assert!(page.contains("Waiting to retry after "));
    assert!(!page.contains("Try again on Substack"));
    harness.stop().await;
}
