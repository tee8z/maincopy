use axum::http::StatusCode;
use maincopy::admin::{AdminState, admin_router};
use serde_json::json;

use crate::helpers::{get, json_body};

#[tokio::test]
async fn agents_can_discover_the_admin_api_version() {
    let response = get(
        admin_router(AdminState::new()),
        "/api/admin/v1/capabilities",
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({
            "api_version": "v1",
            "features": {
                "capabilities": "v1"
            }
        })
    );
}
