use axum::http::StatusCode;
use maincopy_server::admin::admin_router;
use maincopy_shared::{posts::POSTS_PATH, publication::PUBLICATIONS_PATH};
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
    let posts = &document["paths"][POSTS_PATH]["get"];
    assert!(posts.is_object());
    for parameter in ["cursor", "limit"] {
        assert!(
            posts["parameters"]
                .as_array()
                .unwrap()
                .iter()
                .any(|candidate| candidate["name"] == parameter),
            "missing {parameter} query parameter"
        );
    }
    assert_eq!(
        posts["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/ListPostsResponse"
    );
    for status in ["200", "400", "503"] {
        assert!(posts["responses"][status]["headers"]["x-request-id"].is_object());
    }
    let preview_path = format!("{POSTS_PATH}/{{post_id}}/preview");
    let preview = &document["paths"][preview_path.as_str()]["get"];
    assert!(preview.is_object());
    let post_id = preview["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|parameter| parameter["name"] == "post_id")
        .unwrap();
    assert_eq!(post_id["in"], "path");
    assert_eq!(post_id["required"], true);
    assert_eq!(post_id["schema"]["format"], "uuid");
    assert_eq!(
        preview["responses"]["200"]["content"]["text/html"]["schema"]["type"],
        "string"
    );
    for status in ["200", "400", "404", "500", "503"] {
        assert!(preview["responses"][status]["headers"]["cache-control"].is_object());
        assert!(preview["responses"][status]["headers"]["x-request-id"].is_object());
    }
    let preview_assets = &document["paths"]["/api/admin/v1/preview-assets/{content_digest}"]["get"];
    assert!(preview_assets.is_object());
    let parameters = preview_assets["parameters"].as_array().unwrap();
    let content_digest = parameters
        .iter()
        .find(|parameter| parameter["name"] == "content_digest")
        .unwrap();
    assert_eq!(content_digest["in"], "path");
    assert_eq!(content_digest["required"], true);
    let asset_path = parameters
        .iter()
        .find(|parameter| parameter["name"] == "path")
        .unwrap();
    assert_eq!(asset_path["in"], "query");
    assert_eq!(asset_path["required"], true);
    assert!(preview_assets["responses"]["200"]["content"]["application/octet-stream"].is_object());
    for status in ["200", "400", "404", "503"] {
        assert!(preview_assets["responses"][status]["headers"]["cache-control"].is_object());
        assert!(preview_assets["responses"][status]["headers"]["x-request-id"].is_object());
    }
    assert!(preview_assets["responses"]["200"]["headers"]["x-content-type-options"].is_object());
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
        document["components"]["schemas"]["PostPublicationState"]["enum"],
        json!(["draft", "unpublished", "unpublished_change", "published"])
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
