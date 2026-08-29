use std::sync::Arc;

use axum::{Extension, Json};
use utoipa::{OpenApi, openapi::OpenApi as OpenApiDocument};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Maincopy Admin API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Private API for Maincopy operators, CLI clients, and agents"
    ),
)]
pub(super) struct AdminApi;

#[utoipa::path(
    get,
    path = "/api/admin/v1/openapi.json",
    responses(
        (status = OK, description = "Generated OpenAPI 3.1 document")
    ),
    tag = "Administration"
)]
pub(super) async fn get_openapi(
    Extension(document): Extension<Arc<OpenApiDocument>>,
) -> Json<OpenApiDocument> {
    Json((*document).clone())
}
