use axum::http::StatusCode;
use maincopy_server::admin::admin_router;
use serde_json::json;

use crate::helpers::{get, json_body};

#[tokio::test]
async fn admin_openapi_is_generated_from_typed_contracts() {
    let response = get(admin_router(), "/api/admin/v1/openapi.json").await;

    assert_eq!(response.status(), StatusCode::OK);

    let document = json_body(response).await;
    assert_eq!(document["openapi"], "3.1.0");
    assert_eq!(document["info"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(document["paths"]["/api/admin/v1/capabilities"]["get"].is_object());
    assert!(document["paths"]["/api/admin/v1/openapi.json"]["get"].is_object());
    assert!(document["paths"]["/health/live"].is_null());
    assert_eq!(
        document["components"]["schemas"]["AdminApiVersion"]["enum"],
        json!(["v1"])
    );
    assert_eq!(
        document["components"]["schemas"]["CapabilityContractVersion"]["enum"],
        json!(["v1"])
    );
}

#[test]
fn admin_operations_use_the_shared_openapi_router_registry() {
    let registry = include_str!("../../src/admin/mod.rs");

    assert!(
        !registry.contains(".route("),
        "admin API operations must use OpenApiRouter::routes"
    );
}
