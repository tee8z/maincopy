use axum::http::{Method, StatusCode};
use maincopy_server::web::{Readiness, public_router};
use maincopy_shared::{
    ADMIN_CAPABILITIES_PATH,
    posts::POSTS_PATH,
    profile_api::{ACTIVE_TIP_RECIPIENT_PATH, CURRENT_USER_PROFILE_PATH},
    publication::PUBLICATIONS_PATH,
    source::{SOURCE_PATH, SOURCE_SYNCS_PATH},
};

use crate::helpers::{get, public_state, request};

#[tokio::test]
async fn public_router_does_not_expose_durable_releases_or_receipts() {
    let app = public_router(public_state(Readiness::new(true)));
    for path in [
        "/api/admin/v1/releases",
        "/api/admin/v1/releases/11111111-1111-4111-8111-111111111111",
        "/api/admin/v1/release-operations/11111111-1111-4111-8111-111111111111",
    ] {
        assert_eq!(
            get(app.clone(), path).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
        assert_eq!(
            request(app.clone(), Method::POST, path).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test]
async fn public_router_does_not_expose_version_neutral_admin_discovery() {
    let response = get(
        public_router(public_state(Readiness::new(true))),
        ADMIN_CAPABILITIES_PATH,
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_router_does_not_expose_admin_routes() {
    let response = get(
        public_router(public_state(Readiness::new(true))),
        "/api/admin/v1/capabilities",
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_router_does_not_expose_browser_admin_routes() {
    const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const ASSET_DIGEST: &str =
        "admin-b3-v1-4444444444444444444444444444444444444444444444444444444444444444";

    let app = public_router(public_state(Readiness::new(true)));
    for path in [
        "/admin".to_owned(),
        "/admin/login".to_owned(),
        "/admin/mail".to_owned(),
        "/admin/mail/recovery".to_owned(),
        format!("/admin/mail/posts/{POST_ID}/review"),
        format!("/admin/mail/campaigns/{POST_ID}"),
        "/admin/agents".to_owned(),
        format!("/admin/agents/{POST_ID}"),
        "/admin/users".to_owned(),
        format!("/admin/users/{POST_ID}"),
        format!("/admin/posts/{POST_ID}/review"),
        format!("/admin/posts/{POST_ID}/confirm"),
        format!("/admin/assets/{ASSET_DIGEST}/site.css"),
        format!("/admin/assets/{ASSET_DIGEST}/nostr-login.js"),
    ] {
        let response = get(app.clone(), &path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }

    let publish_path = format!("/admin/posts/{POST_ID}/publish");
    let response = request(app.clone(), Method::POST, &publish_path).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "{publish_path}");

    for path in ["/admin/login", "/admin/logout"] {
        let response = request(app.clone(), Method::POST, path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    for path in [
        "/admin/mail/recovery".to_owned(),
        "/admin/users".to_owned(),
        format!("/admin/mail/posts/{POST_ID}/review"),
        format!("/admin/mail/campaigns/{POST_ID}/approve"),
        format!("/admin/mail/campaigns/{POST_ID}/cancel"),
        "/admin/agents".to_owned(),
        format!("/admin/agents/{POST_ID}/scopes"),
        format!("/admin/agents/{POST_ID}/revoke"),
        format!("/admin/users/{POST_ID}/status"),
        format!("/admin/users/{POST_ID}/roles"),
        format!("/admin/users/{POST_ID}/password"),
        format!("/admin/users/{POST_ID}/nostr"),
        format!("/admin/users/{POST_ID}/credentials/password/remove"),
        format!("/admin/users/{POST_ID}/credentials/nostr/remove"),
    ] {
        assert_eq!(
            request(app.clone(), Method::POST, &path).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test]
async fn public_router_does_not_expose_profile_or_tip_recipient_resources() {
    let app = public_router(public_state(Readiness::new(true)));

    for path in [
        CURRENT_USER_PROFILE_PATH,
        ACTIVE_TIP_RECIPIENT_PATH,
        "/admin/profile",
        "/admin/tips",
    ] {
        let response = get(app.clone(), path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(
            request(app.clone(), Method::POST, path).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test]
async fn public_router_does_not_expose_source_resources_or_controls() {
    let app = public_router(public_state(Readiness::new(true)));
    for path in [
        SOURCE_PATH,
        SOURCE_SYNCS_PATH,
        "/api/admin/v1/source-syncs/11111111-1111-4111-8111-111111111111",
        "/admin/source",
    ] {
        let response = get(app.clone(), path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    for path in [SOURCE_SYNCS_PATH, "/admin/source/sync"] {
        let response = request(app.clone(), Method::POST, path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn public_router_does_not_expose_publication_commands() {
    let response = request(
        public_router(public_state(Readiness::new(true))),
        Method::POST,
        PUBLICATIONS_PATH,
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_router_does_not_expose_loaded_post_revisions() {
    let response = get(
        public_router(public_state(Readiness::new(true))),
        POSTS_PATH,
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_router_does_not_expose_candidate_post_previews() {
    let response = get(
        public_router(public_state(Readiness::new(true))),
        &format!("{POSTS_PATH}/11111111-1111-4111-8111-111111111111/preview"),
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_router_does_not_expose_candidate_preview_assets() {
    let response = get(
        public_router(public_state(Readiness::new(true))),
        "/api/admin/v1/preview-assets/content-b3-v1-4444444444444444444444444444444444444444444444444444444444444444?path=assets/preview.png",
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_router_does_not_expose_admin_openapi() {
    let response = get(
        public_router(public_state(Readiness::new(true))),
        "/api/admin/v1/openapi.json",
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_router_does_not_expose_metrics() {
    let app = public_router(public_state(Readiness::new(true)));
    for method in [Method::GET, Method::HEAD] {
        assert_eq!(
            request(app.clone(), method, "/metrics").await.status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn disabled_mail_exposes_no_public_capture_or_control_routes() {
    let app = public_router(public_state(Readiness::new(true)));
    for path in [
        "/email/subscribe",
        "/email/confirm/private-token",
        "/email/unsubscribe/private-token",
    ] {
        for method in [Method::GET, Method::HEAD, Method::POST] {
            assert_eq!(
                request(app.clone(), method, path).await.status(),
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
    }
}
