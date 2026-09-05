use std::sync::Arc;

use axum::{Extension, Router, middleware};
use utoipa::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::domain::{
    profile::ProfileStore,
    publication::{
        activation::PublicationCoordinatorHandle, admin as publication_admin, ui as publication_ui,
    },
    source::{admin as source_admin, ui as source_ui},
};
use crate::source_sync::SourceSyncHandle;
use maincopy_shared::auth::AdminScope;

mod assets;
mod capabilities;
mod identity;
mod openapi;
pub(crate) mod origin;
pub(crate) mod principal;
pub(crate) mod problem;
mod profile;
pub(crate) mod request_id;
mod security;
mod server;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod ui;

use openapi::AdminApi;
pub(crate) use security::{
    AdminSecurityState, AdminSessionPolicy, BrowserFormSession, BrowserSessionContext,
    RequiredBrowserSession, browser_scoped_router,
};
pub(crate) use server::AdminServer;

/// Builds the protected administration router without binding a listener.
pub(crate) fn admin_router(security: AdminSecurityState) -> Router {
    let (router, document) = registered_router(&security);
    router
        .layer(Extension(Arc::new(document)))
        .layer(Extension(security.clone()))
        .layer(middleware::from_fn_with_state(
            security.clone(),
            security::validate_host,
        ))
        .layer(middleware::from_fn_with_state(
            security,
            security::admit_request,
        ))
        .layer(middleware::from_fn(request_id::assign))
        .layer(middleware::from_fn(security::harden_private_response))
}

fn registered_router(security: &AdminSecurityState) -> (Router, utoipa::openapi::OpenApi) {
    let (api, document) = OpenApiRouter::<()>::with_openapi(AdminApi::openapi())
        .routes(scoped_routes(
            routes!(capabilities::get_admin_capabilities),
            security,
            AdminScope::StatusRead,
        ))
        .routes(security::login_challenge_routes())
        .routes(security::login_session_routes())
        .routes(security::authenticate_layer(
            security::current_session_routes(),
            security,
        ))
        .routes(scoped_routes(
            routes!(capabilities::get_capabilities),
            security,
            AdminScope::StatusRead,
        ))
        .routes(scoped_routes(
            identity::user_read_routes(),
            security,
            AdminScope::UserManage,
        ))
        .routes(scoped_routes(
            identity::user_item_read_routes(),
            security,
            AdminScope::UserManage,
        ))
        .routes(scoped_routes(
            identity::user_create_routes(),
            security,
            AdminScope::UserManage,
        ))
        .routes(scoped_routes(
            identity::user_status_routes(),
            security,
            AdminScope::UserManage,
        ))
        .routes(scoped_routes(
            identity::user_role_routes(),
            security,
            AdminScope::RoleAssign,
        ))
        .routes(scoped_routes(
            identity::human_credential_routes(),
            security,
            AdminScope::CredentialManage,
        ))
        .routes(scoped_routes(
            identity::agent_read_routes(),
            security,
            AdminScope::CredentialManage,
        ))
        .routes(scoped_routes(
            identity::agent_item_read_routes(),
            security,
            AdminScope::CredentialManage,
        ))
        .routes(scoped_routes(
            identity::agent_registration_routes(),
            security,
            AdminScope::CredentialManage,
        ))
        .routes(scoped_routes(
            identity::agent_scope_routes(),
            security,
            AdminScope::CredentialManage,
        ))
        .routes(scoped_routes(
            identity::agent_revocation_routes(),
            security,
            AdminScope::CredentialManage,
        ))
        .routes(scoped_routes(
            identity::audit_routes(),
            security,
            AdminScope::AuditRead,
        ))
        .routes(scoped_routes(
            profile::profile_routes(),
            security,
            AdminScope::ProfileManage,
        ))
        .routes(scoped_routes(
            profile::tip_recipient_routes(),
            security,
            AdminScope::LightningManage,
        ))
        .routes(scoped_routes(
            publication_admin::list_routes(),
            security,
            AdminScope::ContentRead,
        ))
        .routes(scoped_routes(
            publication_admin::preview_routes(),
            security,
            AdminScope::PreviewRead,
        ))
        .routes(scoped_routes(
            publication_admin::preview_asset_routes(),
            security,
            AdminScope::PreviewRead,
        ))
        .routes(scoped_routes(
            publication_admin::releases::list_routes(),
            security,
            AdminScope::ReleaseManage,
        ))
        .routes(scoped_routes(
            publication_admin::releases::item_routes(),
            security,
            AdminScope::ReleaseManage,
        ))
        .routes(scoped_routes(
            publication_admin::releases::operation_routes(),
            security,
            AdminScope::ReleaseManage,
        ))
        .routes(scoped_routes(
            publication_admin::routes(),
            security,
            AdminScope::ReleaseManage,
        ))
        .routes(scoped_routes(
            source_admin::status_routes(),
            security,
            AdminScope::StatusRead,
        ))
        .routes(scoped_routes(
            source_admin::sync_list_routes(),
            security,
            AdminScope::StatusRead,
        ))
        .routes(scoped_routes(
            source_admin::sync_item_routes(),
            security,
            AdminScope::StatusRead,
        ))
        .routes(scoped_routes(
            source_admin::sync_mutation_routes(),
            security,
            AdminScope::SourceSync,
        ))
        .routes(scoped_routes(
            routes!(openapi::get_openapi),
            security,
            AdminScope::StatusRead,
        ))
        .split_for_parts();
    (
        api.merge(ui::public_router())
            .merge(ui::protected_router(security))
            .merge(publication_ui::router(security))
            .merge(source_ui::router(security)),
        document,
    )
}

fn scoped_routes(
    routes: utoipa_axum::router::UtoipaMethodRouter,
    security: &AdminSecurityState,
    scope: AdminScope,
) -> utoipa_axum::router::UtoipaMethodRouter {
    security::scoped_layer(routes, security, scope)
}

/// Builds the private administration router with live publication state.
pub(crate) fn runtime_admin_router(
    publications: PublicationCoordinatorHandle,
    security: AdminSecurityState,
    profiles: ProfileStore,
    source: SourceSyncHandle,
) -> Router {
    admin_router(security)
        .layer(Extension(publications))
        .layer(Extension(profiles))
        .layer(Extension(source))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use axum::{
        body::{Bytes, to_bytes},
        http::{HeaderValue, Method, StatusCode, header::CACHE_CONTROL},
    };
    use serde_json::{Value, json};
    use tower::ServiceExt as _;
    use uuid::Uuid;

    use maincopy_shared::{
        ADMIN_CAPABILITIES_PATH, CAPABILITIES_PATH,
        posts::POSTS_PATH,
        profile_api::{ACTIVE_TIP_RECIPIENT_PATH, CURRENT_USER_PROFILE_PATH},
        publication::PUBLICATIONS_PATH,
        source::{SOURCE_PATH, SOURCE_SYNCS_PATH},
    };

    use super::test_support::ProtectedAdminHarness;

    #[tokio::test]
    async fn login_page_preserves_the_origin_of_native_form_posts() {
        let harness = ProtectedAdminHarness::start().await;
        let request = axum::http::Request::builder()
            .uri("/admin/login")
            .header("host", super::test_support::ADMIN_AUTHORITY)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = harness.router().oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["referrer-policy"], "same-origin");
        harness.stop().await;
    }

    #[tokio::test]
    async fn protected_registry_serves_contracts_only_after_authentication() {
        let harness = ProtectedAdminHarness::start().await;
        let router = harness.router();

        for path in [
            ADMIN_CAPABILITIES_PATH,
            CAPABILITIES_PATH,
            "/api/admin/v1/openapi.json",
        ] {
            let unauthenticated = axum::http::Request::builder()
                .uri(path)
                .header("host", super::test_support::ADMIN_AUTHORITY)
                .body(axum::body::Body::empty())
                .unwrap();
            assert_eq!(
                router
                    .clone()
                    .oneshot(unauthenticated)
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED,
                "{path}"
            );

            let authenticated = harness.request(Method::GET, path, Bytes::new(), None);
            let response = router.clone().oneshot(authenticated).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(response.headers()["cache-control"], "private, no-store");
            assert_eq!(response.headers()["x-content-type-options"], "nosniff");
            assert_eq!(response.headers()["x-frame-options"], "DENY");
            assert_eq!(response.headers()["referrer-policy"], "same-origin");
            assert!(response.headers().get("content-security-policy").is_some());

            let body: Value = serde_json::from_slice(
                &to_bytes(response.into_body(), 4 * 1024 * 1024)
                    .await
                    .unwrap(),
            )
            .unwrap();
            match path {
                ADMIN_CAPABILITIES_PATH => assert_eq!(
                    body,
                    json!({
                        "api_versions": ["v1"],
                        "feature_contracts": { "capabilities": ["v1"] }
                    })
                ),
                CAPABILITIES_PATH => assert_eq!(
                    body,
                    json!({
                        "api_version": "v1",
                        "features": { "capabilities": "v1" }
                    })
                ),
                _ => assert_openapi_contract(&body),
            }
        }

        harness.stop().await;
    }

    #[tokio::test]
    async fn protected_registry_hardens_not_found_responses() {
        let harness = ProtectedAdminHarness::start().await;
        let response = harness
            .router()
            .oneshot(harness.request(Method::GET, "/not-found", Bytes::new(), None))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        assert!(response.headers().get("x-request-id").is_some());
        harness.stop().await;
    }

    #[tokio::test]
    async fn protected_router_preserves_and_replaces_request_ids() {
        const REQUEST_ID: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";

        let harness = ProtectedAdminHarness::start().await;
        let router = harness.router();
        let mut supplied =
            harness.request(Method::GET, ADMIN_CAPABILITIES_PATH, Bytes::new(), None);
        supplied
            .headers_mut()
            .insert("x-request-id", HeaderValue::from_static(REQUEST_ID));
        let response = router.clone().oneshot(supplied).await.unwrap();
        assert_eq!(response.headers()["x-request-id"], REQUEST_ID);

        let mut generated = Vec::new();
        for invalid in [
            "not-a-uuid",
            "67E55044-10B1-426F-9247-BB680E5FE0C8",
            "67e5504410b1426f9247bb680e5fe0c8",
        ] {
            let mut request =
                harness.request(Method::GET, ADMIN_CAPABILITIES_PATH, Bytes::new(), None);
            request.headers_mut().insert(
                "x-request-id",
                HeaderValue::from_str(invalid).expect("fixture header must be valid"),
            );
            let response = router.clone().oneshot(request).await.unwrap();
            let request_id = response.headers()["x-request-id"].to_str().unwrap();
            assert_ne!(request_id, invalid);
            let parsed = Uuid::parse_str(request_id).unwrap();
            assert_eq!(parsed.hyphenated().to_string(), request_id);
            generated.push(request_id.to_owned());
        }
        assert_eq!(
            generated.len(),
            generated.iter().collect::<BTreeSet<_>>().len()
        );
        harness.stop().await;
    }

    #[tokio::test]
    async fn protected_publication_routes_validate_inputs_before_runtime_lookup() {
        const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
        const CANDIDATE: &str =
            "content-b3-v1-4444444444444444444444444444444444444444444444444444444444444444";

        let harness = ProtectedAdminHarness::start().await;
        let router = harness.router();

        let response = router
            .clone()
            .oneshot(harness.request(Method::GET, POSTS_PATH, Bytes::new(), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_problem(response, "publication_unavailable").await;

        for path in [
            format!("{POSTS_PATH}?limit=0"),
            format!("{POSTS_PATH}?limit=101"),
            format!("{POSTS_PATH}?limit=not-a-number"),
            format!("{POSTS_PATH}?cursor=not-a-uuid"),
            format!("{POSTS_PATH}?unknown=value"),
            format!("{POSTS_PATH}/not-a-uuid/preview"),
            format!("{POSTS_PATH}/11111111111141118111111111111111/preview"),
            format!("/api/admin/v1/preview-assets/{CANDIDATE}"),
            format!("/api/admin/v1/preview-assets/{CANDIDATE}?path=../secret.png"),
            format!("/api/admin/v1/preview-assets/{CANDIDATE}?path=assets/a.png&unknown=value"),
        ] {
            let response = router
                .clone()
                .oneshot(harness.request(Method::GET, &path, Bytes::new(), None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            assert!(response.headers().get("x-request-id").is_some(), "{path}");
        }

        for path in [
            format!("{POSTS_PATH}/{POST_ID}/preview"),
            format!("/api/admin/v1/preview-assets/{CANDIDATE}?path=assets/preview.png"),
        ] {
            let response = router
                .clone()
                .oneshot(harness.request(Method::GET, &path, Bytes::new(), None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            assert_eq!(response.headers()[CACHE_CONTROL], "private, no-store");
            assert_problem(response, "publication_unavailable").await;
        }
        harness.stop().await;
    }

    #[tokio::test]
    async fn status_mutation_without_publication_coordinator_is_rejected_before_commit() {
        const USERS_PATH: &str = "/api/admin/v1/identity/users";

        let harness = ProtectedAdminHarness::start().await;
        let router = harness.router();
        let response = router
            .clone()
            .oneshot(harness.request(Method::GET, USERS_PATH, Bytes::new(), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let users: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap())
                .unwrap();
        let user = users["users"].as_array().unwrap().first().unwrap();
        let user_id = user["user_id"].as_str().unwrap();
        let version = user["version"].as_u64().unwrap();
        assert_eq!(user["status"], "enabled");

        let path = format!("{USERS_PATH}/{user_id}/status");
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "expected_version": version,
                "status": "disabled"
            }))
            .unwrap(),
        );
        let mut request = harness.request(Method::PUT, &path, body, None);
        request
            .headers_mut()
            .insert("content-type", HeaderValue::from_static("application/json"));
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_problem(response, "identity_unavailable").await;

        let response = router
            .oneshot(harness.request(
                Method::GET,
                &format!("{USERS_PATH}/{user_id}"),
                Bytes::new(),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let unchanged: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(unchanged["status"], "enabled");
        assert_eq!(unchanged["version"], version);
        harness.stop().await;
    }

    fn assert_openapi_contract(document: &Value) {
        assert_eq!(document["openapi"], "3.1.0");
        assert_eq!(document["info"]["version"], env!("CARGO_PKG_VERSION"));
        let encoded = serde_json::to_string(document).unwrap();
        for removed_contract in [
            "publication_jobs",
            "target_job",
            "TargetJob",
            "subscriptions",
            "distribution",
        ] {
            assert!(
                !encoded.contains(removed_contract),
                "OpenAPI retained removed contract {removed_contract}"
            );
        }
        assert!(document["paths"][ADMIN_CAPABILITIES_PATH]["get"].is_object());
        assert!(document["paths"][CAPABILITIES_PATH]["get"].is_object());
        assert!(document["paths"][POSTS_PATH]["get"].is_object());
        assert!(document["paths"][PUBLICATIONS_PATH]["post"].is_object());
        assert!(document["paths"]["/api/admin/v1/releases/{publication_id}"]["post"].is_object());
        for path in [
            "/api/admin/v1/releases",
            "/api/admin/v1/releases/{publication_id}",
            "/api/admin/v1/release-operations/{operation_id}",
        ] {
            assert!(document["paths"][path]["get"].is_object(), "{path}");
        }

        assert!(document["paths"][CURRENT_USER_PROFILE_PATH]["get"].is_object());
        assert!(document["paths"][CURRENT_USER_PROFILE_PATH]["put"].is_object());
        assert!(document["paths"][ACTIVE_TIP_RECIPIENT_PATH]["get"].is_object());
        assert!(document["paths"][ACTIVE_TIP_RECIPIENT_PATH]["put"].is_object());
        assert!(document["paths"][SOURCE_PATH]["get"].is_object());
        assert!(document["paths"][SOURCE_SYNCS_PATH]["get"].is_object());
        assert!(document["paths"][SOURCE_SYNCS_PATH]["post"].is_object());
        assert!(
            document["paths"]["/api/admin/v1/source-syncs/{source_sync_id}"]["get"].is_object()
        );
        assert!(document["paths"]["/api/admin/v1/auth/sessions"]["post"].is_object());
        assert!(document["paths"]["/health/live"].is_null());
        let posts = &document["paths"][POSTS_PATH]["get"];
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
        let post_id = preview["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|parameter| parameter["name"] == "post_id")
            .unwrap();
        assert_eq!(post_id["in"], "path");
        assert_eq!(post_id["required"], true);
        assert_eq!(post_id["schema"]["format"], "uuid");
        for status in ["200", "400", "404", "500", "503"] {
            assert!(preview["responses"][status]["headers"]["cache-control"].is_object());
            assert!(preview["responses"][status]["headers"]["x-request-id"].is_object());
        }
        let preview_asset =
            &document["paths"]["/api/admin/v1/preview-assets/{content_digest}"]["get"];
        assert!(preview_asset["responses"]["200"]["content"]["*/*"].is_object());
        for header in [
            "cache-control",
            "content-disposition",
            "content-security-policy",
            "x-content-type-options",
            "x-request-id",
        ] {
            assert!(
                preview_asset["responses"]["200"]["headers"][header].is_object(),
                "preview asset response is missing {header}"
            );
        }
        let publication = &document["paths"][PUBLICATIONS_PATH]["post"];
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
        }
        for path in [CURRENT_USER_PROFILE_PATH, ACTIVE_TIP_RECIPIENT_PATH] {
            let mutation = &document["paths"][path]["put"];
            let idempotency_key = mutation["parameters"]
                .as_array()
                .unwrap()
                .iter()
                .find(|parameter| parameter["name"] == "Idempotency-Key")
                .unwrap();
            assert_eq!(idempotency_key["required"], true);
            assert_eq!(idempotency_key["schema"]["format"], "uuid");
            for status in ["200", "400", "403", "404", "409", "412", "413", "503"] {
                assert!(mutation["responses"][status]["headers"]["x-request-id"].is_object());
            }
        }
        let source_sync = &document["paths"][SOURCE_SYNCS_PATH]["post"];
        let idempotency_key = source_sync["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|parameter| parameter["name"] == "Idempotency-Key")
            .unwrap();
        assert_eq!(idempotency_key["required"], true);
        assert_eq!(idempotency_key["schema"]["format"], "uuid");
        assert_eq!(
            source_sync["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/BeginSourceSyncRequest"
        );
        for status in ["200", "202", "400", "409", "413", "500", "503"] {
            assert!(
                source_sync["responses"][status]["headers"]["x-request-id"].is_object(),
                "source sync response {status} is missing x-request-id"
            );
        }
        assert_eq!(
            document["components"]["schemas"]["PostPublicationState"]["enum"],
            json!(["draft", "unpublished", "unpublished_change", "published"])
        );
        assert_eq!(
            document["paths"][PUBLICATIONS_PATH]["post"]["requestBody"]["content"]["application/json"]
                ["schema"]["$ref"],
            "#/components/schemas/PublishNowRequest"
        );
    }

    async fn assert_problem(response: axum::response::Response, expected_code: &str) {
        let request_id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], expected_code);
        assert_eq!(body["error"]["request_id"], request_id);
    }
}
