use axum::http::StatusCode;
use maincopy_server::admin::admin_router;
use maincopy_shared::{
    ADMIN_CAPABILITIES_PATH, AdminApiCapabilities, AdminApiVersion, CAPABILITIES_PATH,
    Capabilities, CapabilityContractVersion, FeatureVersions, SupportedFeatureContracts,
};
use serde_json::json;

use crate::helpers::{get, json_body};

#[tokio::test]
async fn clients_can_discover_supported_admin_contracts_without_selecting_an_api_version() {
    let response = get(admin_router(), ADMIN_CAPABILITIES_PATH).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(
        body,
        json!({
            "api_versions": ["v1"],
            "feature_contracts": {
                "capabilities": ["v1"]
            }
        })
    );
    assert_eq!(
        serde_json::from_value::<AdminApiCapabilities>(body).unwrap(),
        AdminApiCapabilities {
            api_versions: vec![AdminApiVersion::V1],
            feature_contracts: SupportedFeatureContracts {
                capabilities: vec![CapabilityContractVersion::V1],
            },
        }
    );
}

#[tokio::test]
async fn agents_can_discover_the_admin_api_version() {
    let response = get(admin_router(), CAPABILITIES_PATH).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(
        body,
        json!({
            "api_version": "v1",
            "features": {
                "capabilities": "v1"
            }
        })
    );
    assert_eq!(
        serde_json::from_value::<Capabilities>(body).unwrap(),
        Capabilities {
            api_version: AdminApiVersion::V1,
            features: FeatureVersions {
                capabilities: CapabilityContractVersion::V1,
            },
        }
    );
}
