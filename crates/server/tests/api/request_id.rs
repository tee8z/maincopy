use axum::{
    Router,
    body::Body,
    http::{Request, Response, StatusCode},
};
use maincopy_server::admin::admin_router;
use maincopy_shared::{ADMIN_CAPABILITIES_PATH, CAPABILITIES_PATH};
use tower::ServiceExt;
use uuid::Uuid;

use crate::helpers::get;

const REQUEST_ID_HEADER: &str = "x-request-id";
const CANONICAL_REQUEST_ID: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";

fn response_request_id(response: &Response<Body>) -> String {
    response
        .headers()
        .get(REQUEST_ID_HEADER)
        .expect("admin response must have a request ID")
        .to_str()
        .expect("request ID must be visible ASCII")
        .to_owned()
}

fn assert_canonical_uuid(value: &str) {
    let uuid = Uuid::parse_str(value).expect("request ID must be a UUID");
    assert_eq!(uuid.hyphenated().to_string(), value);
}

async fn get_with_request_id(app: Router, path: &str, request_id: &str) -> Response<Body> {
    let request = Request::builder()
        .uri(path)
        .header(REQUEST_ID_HEADER, request_id)
        .body(Body::empty())
        .unwrap();

    app.oneshot(request).await.unwrap()
}

#[tokio::test]
async fn every_admin_response_has_a_canonical_request_id() {
    let app = admin_router();

    for (path, expected_status) in [
        (ADMIN_CAPABILITIES_PATH, StatusCode::OK),
        (CAPABILITIES_PATH, StatusCode::OK),
        ("/api/admin/v1/openapi.json", StatusCode::OK),
        ("/api/admin/not-found", StatusCode::NOT_FOUND),
    ] {
        let response = get(app.clone(), path).await;

        assert_eq!(response.status(), expected_status, "{path}");
        assert_canonical_uuid(&response_request_id(&response));
    }
}

#[tokio::test]
async fn a_supplied_canonical_request_id_is_preserved() {
    let response = get_with_request_id(
        admin_router(),
        ADMIN_CAPABILITIES_PATH,
        CANONICAL_REQUEST_ID,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_request_id(&response), CANONICAL_REQUEST_ID);
}

#[tokio::test]
async fn invalid_or_noncanonical_request_ids_are_replaced() {
    let app = admin_router();

    for supplied in [
        "not-a-uuid",
        "67E55044-10B1-426F-9247-BB680E5FE0C8",
        "67e5504410b1426f9247bb680e5fe0c8",
    ] {
        let response = get_with_request_id(app.clone(), ADMIN_CAPABILITIES_PATH, supplied).await;
        let generated = response_request_id(&response);

        assert_ne!(generated, supplied);
        assert_canonical_uuid(&generated);
    }
}

#[tokio::test]
async fn concurrent_requests_receive_different_generated_ids() {
    let app = admin_router();
    let (left, right) = tokio::join!(
        get(app.clone(), ADMIN_CAPABILITIES_PATH),
        get(app, CAPABILITIES_PATH),
    );

    let left = response_request_id(&left);
    let right = response_request_id(&right);
    assert_canonical_uuid(&left);
    assert_canonical_uuid(&right);
    assert_ne!(left, right);
}
