use axum::http::StatusCode;
use maincopy_server::admin::admin_router;
use maincopy_shared::{AdminApiVersion, Capabilities, CapabilityContractVersion, FeatureVersions};
use serde_json::json;

use crate::helpers::{get, json_body};

#[tokio::test]
async fn agents_can_discover_the_admin_api_version() {
    let response = get(admin_router(), "/api/admin/v1/capabilities").await;

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
