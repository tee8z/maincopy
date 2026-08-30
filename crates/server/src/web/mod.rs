use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::Router;

mod health;
mod server;

use crate::{domain::publication::web::router as publication_router, render::SiteSnapshotReader};
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
        .merge(publication_router(state.snapshots))
        .merge(health_router(state.readiness))
}
