use axum::{Json, response::IntoResponse};
use maincopy_shared::{AdminApiVersion, Capabilities, CapabilityContractVersion, FeatureVersions};

#[utoipa::path(
    get,
    path = "/api/admin/v1/capabilities",
    responses(
        (status = OK, description = "Admin API capabilities", body = Capabilities)
    ),
    tag = "Administration"
)]
pub(super) async fn get_capabilities() -> impl IntoResponse {
    Json(Capabilities {
        api_version: AdminApiVersion::V1,
        features: FeatureVersions {
            capabilities: CapabilityContractVersion::V1,
        },
    })
}
