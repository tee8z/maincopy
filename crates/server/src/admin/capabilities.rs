use axum::{Json, response::IntoResponse};
use maincopy_shared::{
    AdminApiCapabilities, AdminApiVersion, Capabilities, CapabilityContractVersion,
    FeatureVersions, SupportedFeatureContracts,
};

#[utoipa::path(
    get,
    path = "/api/admin/capabilities",
    responses(
        (status = OK, description = "Supported admin API and feature contract versions", body = AdminApiCapabilities)
    ),
    tag = "Administration"
)]
pub(super) async fn get_admin_capabilities() -> impl IntoResponse {
    Json(AdminApiCapabilities {
        api_versions: vec![AdminApiVersion::V1],
        feature_contracts: SupportedFeatureContracts {
            capabilities: vec![CapabilityContractVersion::V1],
        },
    })
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
