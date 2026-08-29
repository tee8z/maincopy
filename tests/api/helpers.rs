use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request},
    response::Response,
};
use serde_json::Value;
use tower::ServiceExt;

pub async fn get(app: Router, path: &str) -> Response {
    let request = Request::builder()
        .method(Method::GET)
        .uri(path)
        .body(Body::empty())
        .expect("test request must be valid");

    app.oneshot(request)
        .await
        .expect("router must produce a response")
}

pub async fn json_body(response: Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body must be readable");

    serde_json::from_slice(&body).expect("response body must be valid JSON")
}
