use axum::http::StatusCode;
use maincopy_server::admin::admin_router;
use maincopy_shared::publication::PUBLICATIONS_PATH;
use serde_json::json;

use crate::helpers::{get, json_body};

#[tokio::test]
async fn admin_openapi_is_generated_from_typed_contracts() {
    let response = get(admin_router(), "/api/admin/v1/openapi.json").await;

    assert_eq!(response.status(), StatusCode::OK);

    let document = json_body(response).await;
    assert_eq!(document["openapi"], "3.1.0");
    assert_eq!(document["info"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(document["paths"]["/api/admin/capabilities"]["get"].is_object());
    assert!(document["paths"]["/api/admin/v1/capabilities"]["get"].is_object());
    assert!(document["paths"]["/api/admin/v1/openapi.json"]["get"].is_object());
    let publication = &document["paths"][PUBLICATIONS_PATH]["post"];
    assert!(publication.is_object());
    let idempotency_key = publication["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|parameter| parameter["name"] == "Idempotency-Key")
        .unwrap();
    assert_eq!(idempotency_key["required"], true);
    assert_eq!(idempotency_key["schema"]["format"], "uuid");
    for status in ["200", "400", "404", "409", "412", "413", "500", "503"] {
        assert!(publication["responses"][status]["headers"]["x-request-id"].is_object());
        assert_eq!(
            publication["responses"][status]["headers"]["x-request-id"]["schema"]["format"],
            "uuid"
        );
    }
    assert_eq!(
        publication["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/PublishNowRequest"
    );
    assert_eq!(
        publication["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/PublishNowResponse"
    );
    assert_eq!(
        publication["responses"]["413"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/PublicationErrorEnvelope"
    );
    assert!(document["paths"]["/health/live"].is_null());
    assert_eq!(
        document["components"]["schemas"]["AdminApiVersion"]["enum"],
        json!(["v1"])
    );
    assert_eq!(
        document["components"]["schemas"]["CapabilityContractVersion"]["enum"],
        json!(["v1"])
    );
    assert_eq!(
        document["components"]["schemas"]["AdminApiCapabilities"]["required"],
        json!(["api_versions", "feature_contracts"])
    );
    assert_eq!(
        document["components"]["schemas"]["SupportedFeatureContracts"]["required"],
        json!(["capabilities"])
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
