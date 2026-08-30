use std::fmt;

use axum::{
    extract::Request,
    http::{HeaderMap, HeaderValue},
    middleware::Next,
    response::Response,
};
use uuid::Uuid;

pub(crate) const REQUEST_ID_HEADER: &str = "x-request-id";

/// A canonical request identifier that is safe to include in diagnostics.
///
/// This type never retains an invalid or otherwise untrusted header value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RequestId(Uuid);

impl RequestId {
    fn from_headers(headers: &HeaderMap) -> Self {
        Self::supplied(headers).unwrap_or_else(|| Self(Uuid::new_v4()))
    }

    fn supplied(headers: &HeaderMap) -> Option<Self> {
        let mut values = headers.get_all(REQUEST_ID_HEADER).iter();
        let value = values.next()?;
        if values.next().is_some() {
            return None;
        }

        let value = value.to_str().ok()?;
        let uuid = Uuid::parse_str(value).ok()?;
        (uuid.hyphenated().to_string() == value).then_some(Self(uuid))
    }

    fn header_value(self) -> HeaderValue {
        HeaderValue::from_str(&self.to_string()).expect("a UUID is always a valid header value")
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0.hyphenated())
    }
}

pub(super) async fn assign(mut request: Request, next: Next) -> Response {
    let request_id = RequestId::from_headers(request.headers());
    request.extensions_mut().insert(request_id);

    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(REQUEST_ID_HEADER, request_id.header_value());
    response
}

#[cfg(test)]
mod tests {
    use axum::{Extension, Router, http::StatusCode, middleware, routing::get};
    use tower::ServiceExt;

    use super::*;

    const CANONICAL_REQUEST_ID: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";

    #[tokio::test]
    async fn middleware_stores_the_typed_request_id_in_extensions() {
        async fn handler(Extension(request_id): Extension<RequestId>) -> StatusCode {
            assert_eq!(request_id.to_string(), CANONICAL_REQUEST_ID);
            StatusCode::NO_CONTENT
        }

        let app = Router::new()
            .route("/", get(handler))
            .layer(middleware::from_fn(assign));
        let request = Request::builder()
            .uri("/")
            .header(REQUEST_ID_HEADER, CANONICAL_REQUEST_ID)
            .body(axum::body::Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
}
