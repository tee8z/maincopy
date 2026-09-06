use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{
    Router,
    extract::{Request, State},
    http::HeaderValue,
    http::header::{
        CONTENT_SECURITY_POLICY, REFERRER_POLICY as REFERRER_HEADER, X_CONTENT_TYPE_OPTIONS,
    },
    middleware::{self, Next},
    response::Response,
};

mod health;
mod server;

use crate::{
    domain::publication::web::router as publication_router,
    render::{REFERRER_POLICY, SiteSnapshotReader},
};
use health::router as health_router;
pub(crate) use server::PublicServer;

/// Shared readiness state for the public health endpoint.
///
/// Startup keeps the service unready until its required components are
/// available. Any critical component can make the service unready again.
#[derive(Clone, Debug, Default)]
pub struct Readiness {
    ready: Arc<AtomicBool>,
}

impl Readiness {
    pub fn new(ready: bool) -> Self {
        Self {
            ready: Arc::new(AtomicBool::new(ready)),
        }
    }

    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }

    pub fn mark_not_ready(&self) {
        self.ready.store(false, Ordering::Release);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

/// Explicit request-facing dependencies for the public listener.
#[derive(Clone, Debug)]
pub struct PublicState {
    pub snapshots: SiteSnapshotReader,
    pub readiness: Readiness,
}

/// Builds the public router without binding a listener.
pub fn public_router(state: PublicState) -> Router {
    Router::new()
        .merge(publication_router(state.snapshots.clone()))
        .merge(health_router(state.readiness))
        .layer(middleware::from_fn_with_state(
            state.snapshots,
            public_response_policy,
        ))
}

async fn public_response_policy(
    State(snapshots): State<SiteSnapshotReader>,
    mut request: Request,
    next: Next,
) -> Response {
    let snapshot = snapshots.load_full();
    request.extensions_mut().insert(snapshot.clone());
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers
        .entry(CONTENT_SECURITY_POLICY)
        .or_insert_with(|| snapshot.response_policy.content_security_policy.clone());
    headers.insert(REFERRER_HEADER, REFERRER_POLICY);
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::Request as HttpRequest,
    };
    use markdown_compiler::prepare_content;
    use tokio::sync::Mutex;
    use tower::ServiceExt as _;

    use super::*;
    use crate::{
        content_fixtures::{content_tree, publication},
        domain::publication::PublicLedgerProjection,
        frontend_assets::embedded_manifest,
        render::{SiteSnapshot, compile_content_catalog, render_site_shell, snapshot_store},
    };

    fn snapshot(title: &str, origin: &str) -> SiteSnapshot {
        let source = format!(
            "[site]\ntitle = {title:?}\nbase_url = \"https://example.com/\"\ndescription = \"Policy fixture.\"\n[author]\nname = \"Author\"\n[assets]\nallowed_https_origins = [{origin:?}]\n"
        );
        let tree = content_tree(publication("publication.toml", source), vec![], vec![], 0);
        let catalog = Arc::new(compile_content_catalog(&prepare_content(&tree).unwrap()).unwrap());
        render_site_shell(
            catalog,
            embedded_manifest(),
            &PublicLedgerProjection::empty(),
        )
        .unwrap()
        .into_snapshot()
        .unwrap()
    }

    #[tokio::test]
    async fn activation_during_dispatch_keeps_body_and_policy_from_one_snapshot() {
        let original = snapshot("Original", "https://original.example");
        let expected = original.digest.clone();
        let original_policy = original.response_policy.content_security_policy.clone();
        let replacement = snapshot("Replacement", "https://replacement.example");
        let (snapshots, activator) = snapshot_store(original);
        let activation = Arc::new(Mutex::new((activator, Some(replacement))));
        let app = Router::new()
            .merge(publication_router(snapshots.clone()))
            .layer(middleware::from_fn(move |request: Request, next: Next| {
                let activation = Arc::clone(&activation);
                let expected = expected.clone();
                async move {
                    let mut state = activation.lock().await;
                    let replacement = state.1.take().unwrap();
                    state.0.activate(&expected, replacement).unwrap();
                    drop(state);
                    next.run(request).await
                }
            }))
            .layer(middleware::from_fn_with_state(
                snapshots.clone(),
                public_response_policy,
            ));
        let response = app
            .oneshot(HttpRequest::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.headers()[CONTENT_SECURITY_POLICY], original_policy);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("Original"));
        assert!(!body.contains("Replacement"));
        assert!(snapshots.load_full().index_page().contains("Replacement"));
    }
}
