use std::sync::Arc;

use axum::{Extension, Router};
use utoipa::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};

mod capabilities;
mod openapi;

use openapi::AdminApi;

/// Builds the private administration router without binding a listener.
///
/// The caller must serve this router only on the configured administration
/// transport. In production, that transport is a Unix domain socket.
pub fn admin_router() -> Router {
    let (router, document) = OpenApiRouter::<()>::with_openapi(AdminApi::openapi())
        .routes(routes!(capabilities::get_capabilities))
        .routes(routes!(openapi::get_openapi))
        .split_for_parts();

    router.layer(Extension(Arc::new(document)))
}
