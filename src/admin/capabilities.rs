use axum::{Json, response::IntoResponse};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub(super) struct Capabilities {
    api_version: AdminApiVersion,
    features: FeatureVersions,
}

#[derive(Debug, Serialize, ToSchema)]
enum AdminApiVersion {
    #[serde(rename = "v1")]
    V1,
}

#[derive(Debug, Serialize, ToSchema)]
struct FeatureVersions {
    capabilities: CapabilityContractVersion,
}

#[derive(Debug, Serialize, ToSchema)]
enum CapabilityContractVersion {
    #[serde(rename = "v1")]
    V1,
}

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
