use axum::http::{StatusCode, header::CACHE_CONTROL};
use maincopy_server::admin::admin_router;
use maincopy_shared::posts::POSTS_PATH;

use crate::helpers::{get, json_body};

const FIRST_POST_ID: &str = "11111111-1111-4111-8111-111111111111";
const PREVIEW_ASSETS_PATH: &str = "/api/admin/v1/preview-assets";
const CANDIDATE_DIGEST: &str =
    "content-b3-v1-4444444444444444444444444444444444444444444444444444444444444444";

fn preview_path(post_id: &str) -> String {
    format!("{POSTS_PATH}/{post_id}/preview")
}

fn preview_asset_path(query: &str) -> String {
    format!("{PREVIEW_ASSETS_PATH}/{CANDIDATE_DIGEST}{query}")
}

#[tokio::test]
async fn posts_route_without_runtime_state_returns_a_stable_unavailable_error() {
    let response = get(admin_router(), POSTS_PATH).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = json_body(response).await;
    assert_eq!(body["error"]["code"], "publication_unavailable");
    assert_eq!(
        body["error"]["message"],
        "publication is temporarily unavailable"
    );
    assert_eq!(body["error"]["request_id"], request_id);
}

#[tokio::test]
async fn posts_route_rejects_invalid_pagination_before_runtime_lookup() {
    for query in [
        "?limit=0",
        "?limit=101",
        "?limit=not-a-number",
        "?cursor=not-a-uuid",
        "?unknown=value",
    ] {
        let response = get(admin_router(), &format!("{POSTS_PATH}{query}")).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        let body = json_body(response).await;
        assert!(
            matches!(
                body["error"]["code"].as_str(),
                Some("invalid_posts_limit" | "invalid_posts_query")
            ),
            "{query}: {body}"
        );
        assert!(body["error"]["request_id"].is_string(), "{query}");
    }
}

#[tokio::test]
async fn preview_route_without_runtime_state_returns_a_private_stable_error() {
    let response = get(admin_router(), &preview_path(FIRST_POST_ID)).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers().get(CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = json_body(response).await;
    assert_eq!(body["error"]["code"], "publication_unavailable");
    assert_eq!(
        body["error"]["message"],
        "publication is temporarily unavailable"
    );
    assert_eq!(body["error"]["request_id"], request_id);
}

#[tokio::test]
async fn preview_route_rejects_noncanonical_post_ids_before_runtime_lookup() {
    for post_id in [
        "not-a-uuid",
        "11111111111141118111111111111111",
        "11111111-1111-4111-8111-11111111111A",
    ] {
        let response = get(admin_router(), &preview_path(post_id)).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{post_id}");
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            "private, no-store"
        );
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "invalid_post_id", "{post_id}");
        assert!(body["error"]["request_id"].is_string(), "{post_id}");
    }
}

#[tokio::test]
async fn preview_asset_route_without_runtime_state_returns_a_private_stable_error() {
    let response = get(
        admin_router(),
        &preview_asset_path("?path=assets/preview.png"),
    )
    .await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers().get(CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = json_body(response).await;
    assert_eq!(body["error"]["code"], "publication_unavailable");
    assert_eq!(body["error"]["request_id"], request_id);
}

#[tokio::test]
async fn preview_asset_route_rejects_invalid_queries_before_runtime_lookup() {
    for query in [
        "",
        "?path=../secret.png",
        "?path=posts/not-an-asset.png",
        "?path=assets/preview.png&unknown=value",
    ] {
        let response = get(admin_router(), &preview_asset_path(query)).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            "private, no-store"
        );
        let body = json_body(response).await;
        assert!(
            matches!(
                body["error"]["code"].as_str(),
                Some("invalid_preview_asset_query" | "invalid_preview_asset_path")
            ),
            "{query}: {body}"
        );
        assert!(body["error"]["request_id"].is_string(), "{query}");
    }
}
