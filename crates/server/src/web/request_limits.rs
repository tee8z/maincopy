use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{MatchedPath, Request, State},
    http::{
        Method, StatusCode,
        header::{CACHE_CONTROL, RETRY_AFTER},
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use tokio::{
    sync::Semaphore,
    time::{Instant, timeout},
};

const MAX_REQUEST_TARGET_BYTES: usize = 4096;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADER_COUNT: usize = 64;
const MAX_BODY_BYTES: usize = 8 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 256;
const REQUEST_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct RequestLimits {
    admission: Arc<Semaphore>,
    deadline: Duration,
}

/// Bounds complete request input and handler work for one isolated router.
pub(super) fn apply(router: Router) -> Router {
    router.layer(middleware::from_fn_with_state(
        RequestLimits {
            admission: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            deadline: REQUEST_DEADLINE,
        },
        bounded_request,
    ))
}

async fn bounded_request(
    State(limits): State<RequestLimits>,
    request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    // MatchedPath is router-authored. Never log the raw URI, query, host, or headers.
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_owned());
    let method = method_class(request.method());
    let response = handle_bounded_request(&limits, request, next).await;
    tracing::info!(target: "maincopy::public_access", method, route, status = response.status().as_u16(), elapsed_ms = started.elapsed().as_millis() as u64, "public request completed");
    response
}

fn method_class(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        _ => "OTHER",
    }
}

fn validate_head(request: &Request) -> Result<(), RequestLimit> {
    let uri = request.uri();
    let path_bytes = uri.path_and_query().map_or(0, |value| value.as_str().len());
    let authority_bytes = uri.authority().map_or(0, |value| value.as_str().len());
    let scheme_bytes = uri.scheme_str().map_or(0, |value| value.len() + 3);
    if path_bytes + authority_bytes + scheme_bytes > MAX_REQUEST_TARGET_BYTES {
        return Err(RequestLimit::Target);
    }
    if request.headers().len() > MAX_HEADER_COUNT {
        return Err(RequestLimit::Headers);
    }
    let bytes = request
        .headers()
        .iter()
        .map(|(name, value)| name.as_str().len() + value.as_bytes().len())
        .sum::<usize>();
    if bytes > MAX_HEADER_BYTES {
        return Err(RequestLimit::Headers);
    }
    Ok(())
}

async fn handle_bounded_request(limits: &RequestLimits, request: Request, next: Next) -> Response {
    if let Err(error) = validate_head(&request) {
        return error.into_response();
    }
    let Ok(_permit) = limits.admission.try_acquire() else {
        return RequestLimit::Busy.into_response();
    };
    match timeout(limits.deadline, dispatch(request, next)).await {
        Ok(response) => response,
        Err(_) => RequestLimit::Deadline.into_response(),
    }
}

async fn dispatch(request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => return RequestLimit::Body.into_response(),
    };
    next.run(Request::from_parts(parts, Body::from(body))).await
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
enum RequestLimit {
    #[error("request target exceeds the public limit")]
    Target,
    #[error("request headers exceed the public limit")]
    Headers,
    #[error("request body exceeds the public limit or could not be read")]
    Body,
    #[error("public request capacity is exhausted")]
    Busy,
    #[error("public request deadline elapsed")]
    Deadline,
}

impl IntoResponse for RequestLimit {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Target => StatusCode::URI_TOO_LONG,
            Self::Headers => StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            Self::Body => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Busy => StatusCode::SERVICE_UNAVAILABLE,
            Self::Deadline => StatusCode::REQUEST_TIMEOUT,
        };
        let mut response = status.into_response();
        response
            .headers_mut()
            .insert(CACHE_CONTROL, "no-store".parse().expect("static header"));
        if matches!(self, Self::Busy) {
            response
                .headers_mut()
                .insert(RETRY_AFTER, "1".parse().expect("static header"));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        http::{HeaderValue, Request as HttpRequest},
        routing::get,
    };
    use std::{io, sync::Mutex};
    use tower::ServiceExt as _;
    use tracing_subscriber::fmt::MakeWriter;

    fn router(limits: RequestLimits) -> Router {
        Router::new()
            .route("/posts/{slug}", get(|| async { "public page" }))
            .layer(middleware::from_fn_with_state(limits, bounded_request))
    }

    fn limits() -> RequestLimits {
        RequestLimits {
            admission: Arc::new(Semaphore::new(1)),
            deadline: REQUEST_DEADLINE,
        }
    }

    #[tokio::test]
    async fn public_requests_enforce_exact_target_header_and_body_limits() {
        let app = router(limits());
        for (size, expected) in [
            (MAX_BODY_BYTES, StatusCode::OK),
            (MAX_BODY_BYTES + 1, StatusCode::PAYLOAD_TOO_LARGE),
        ] {
            let response = app
                .clone()
                .oneshot(
                    HttpRequest::builder()
                        .uri("/posts/article")
                        .body(Body::from(vec![b'x'; size]))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        for (size, expected) in [
            (MAX_REQUEST_TARGET_BYTES, StatusCode::OK),
            (MAX_REQUEST_TARGET_BYTES + 1, StatusCode::URI_TOO_LONG),
        ] {
            let uri = format!("/posts/{}", "x".repeat(size - 7));
            let response = app
                .clone()
                .oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        for (size, expected) in [
            (MAX_HEADER_BYTES - 1, StatusCode::OK),
            (
                MAX_HEADER_BYTES,
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    HttpRequest::builder()
                        .uri("/posts/article")
                        .header("x", HeaderValue::from_bytes(&vec![b'x'; size]).unwrap())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        let mut request = HttpRequest::builder()
            .uri("/posts/article")
            .body(Body::empty())
            .unwrap();
        for _ in 0..=MAX_HEADER_COUNT {
            request
                .headers_mut()
                .append("x", HeaderValue::from_static("a"));
        }
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
        );
    }

    #[tokio::test]
    async fn overload_rejects_without_queueing_and_recovers_after_permit_release() {
        let limits = limits();
        let permit = limits.admission.clone().acquire_owned().await.unwrap();
        let app = router(limits);
        let request = || {
            HttpRequest::builder()
                .uri("/posts/article")
                .body(Body::empty())
                .unwrap()
        };
        let response = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[RETRY_AFTER], "1");
        drop(permit);
        assert_eq!(
            app.oneshot(request()).await.unwrap().status(),
            StatusCode::OK
        );
    }

    #[tokio::test(start_paused = true)]
    async fn request_deadline_cancels_stalled_handlers_and_releases_capacity() {
        let limits = limits();
        let admission = limits.admission.clone();
        let app = Router::new()
            .route(
                "/slow",
                get(|| async { std::future::pending::<()>().await }),
            )
            .layer(middleware::from_fn_with_state(limits, bounded_request));
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/slow")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(admission.available_permits(), 1);
    }

    #[derive(Clone)]
    struct CapturedLog(Arc<Mutex<Vec<u8>>>);
    impl io::Write for CapturedLog {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for CapturedLog {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[tokio::test]
    async fn access_events_contain_fixed_route_templates_and_no_user_values() {
        let captured = CapturedLog(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(captured.clone())
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);
        let app = router(limits());
        for (method, uri, expected) in [
            (
                Method::GET,
                "/posts/private-slug?secret=private-query",
                StatusCode::OK,
            ),
            (Method::HEAD, "/posts/private-slug", StatusCode::OK),
            (
                Method::POST,
                "/posts/private-slug",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (Method::GET, "/private-path", StatusCode::NOT_FOUND),
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(
                        HttpRequest::builder()
                            .method(method)
                            .uri(uri)
                            .header("authorization", "private-credential")
                            .body(Body::empty())
                            .unwrap()
                    )
                    .await
                    .unwrap()
                    .status(),
                expected
            );
        }
        let bytes = captured.0.lock().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("public request completed"));
        assert!(text.contains("route=\"/posts/{slug}\""));
        assert!(text.contains("method=\"HEAD\""));
        assert!(text.contains("status=405"));
        assert!(text.contains("route=\"unmatched\""));
        assert!(!text.contains("private-"));
    }
}
