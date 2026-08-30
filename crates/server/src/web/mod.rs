use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{Router, routing::get};

mod health;
mod site;

use health::{live, ready};
use site::{application_asset, archive, index, method_not_allowed, not_found, post, tag};

use crate::render::SiteSnapshotReader;

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
        .route("/", get(index))
        .route("/posts/{slug}", get(post))
        .route("/tags/{tag}", get(tag))
        .route("/archive", get(archive))
        .route("/app-assets/{digest}/{name}", get(application_asset))
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
}
