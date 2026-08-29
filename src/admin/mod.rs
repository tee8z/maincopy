use std::sync::Arc;

use axum::{Extension, Router};
use utoipa::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};

mod capabilities;
mod openapi;

use openapi::AdminApi;

/// Dependencies shared by private administration handlers.
///
/// The foundation routes are stateless. Database and application-command
/// handles will be added here as their implementation slices land.
#[derive(Clone, Debug)]
pub struct AdminState {
    _private: (),
}

impl AdminState {
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for AdminState {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds the private administration router without binding a listener.
///
/// The caller must serve this router only on the configured administration
/// transport. In production, that transport is a Unix domain socket.
pub fn admin_router(state: AdminState) -> Router {
    let (router, document) = OpenApiRouter::<AdminState>::with_openapi(AdminApi::openapi())
        .routes(routes!(capabilities::get_capabilities))
        .routes(routes!(openapi::get_openapi))
        .with_state::<()>(state)
        .split_for_parts();

    router.layer(Extension(Arc::new(document)))
}
