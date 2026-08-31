use std::sync::Arc;

use axum::{Extension, Router, middleware};
use utoipa::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::domain::publication::activation::PublicationCoordinatorHandle;

mod capabilities;
mod openapi;
pub(crate) mod request_id;
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
        .routes(routes!(capabilities::get_admin_capabilities))
        .routes(routes!(capabilities::get_capabilities))
        .routes(crate::domain::publication::admin::list_routes())
        .routes(crate::domain::publication::admin::preview_routes())
        .routes(crate::domain::publication::admin::preview_asset_routes())
        .routes(crate::domain::publication::admin::routes())
        .routes(routes!(openapi::get_openapi))
        .split_for_parts();

    router
        .layer(Extension(Arc::new(document)))
        .layer(middleware::from_fn(request_id::assign))
}

/// Builds the private administration router with live publication state.
pub(crate) fn runtime_admin_router(publications: PublicationCoordinatorHandle) -> Router {
    admin_router().layer(Extension(publications))
}
