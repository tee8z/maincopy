use std::sync::Arc;

use axum::{Extension, Router};
use utoipa::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};

mod capabilities;
mod openapi;
mod socket;

use openapi::AdminApi;
pub(crate) use socket::AdminSocket;

/// Builds the private administration router without binding a listener.
///
/// The caller must serve this router only on the configured administration
/// transport. Production uses a Unix domain socket or a protected local
/// Windows named pipe.
pub fn admin_router() -> Router {
    let (router, document) = OpenApiRouter::<()>::with_openapi(AdminApi::openapi())
        .routes(routes!(capabilities::get_capabilities))
        .routes(routes!(openapi::get_openapi))
        .split_for_parts();

    router.layer(Extension(Arc::new(document)))
}
