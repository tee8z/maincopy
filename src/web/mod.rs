use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{Router, routing::get};

mod health;

use health::{live, ready};

/// Shared readiness state for the public health endpoint.
///
/// Startup keeps the service unready until its required components are
/// available. Any critical component can make the service unready again.
#[derive(Clone, Debug)]
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

impl Default for Readiness {
    fn default() -> Self {
        Self::new(false)
    }
}

/// Builds the public router without binding a listener.
pub fn public_router(readiness: Readiness) -> Router {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .with_state(readiness)
}
