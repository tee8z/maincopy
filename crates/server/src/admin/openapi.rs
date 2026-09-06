use std::sync::Arc;

use axum::{Extension, Json, http::Method};
use maincopy_shared::{
    auth::AdminScope,
    auth_api::{CSRF_COOKIE_NAME, CSRF_HEADER_NAME, SESSION_COOKIE_NAME},
};
use utoipa::{
    OpenApi, PartialSchema,
    openapi::{
        OpenApi as OpenApiDocument, Required,
        path::{Operation, ParameterBuilder, ParameterIn},
        security::{ApiKey, ApiKeyValue, SecurityRequirement, SecurityScheme},
    },
};
use utoipa_axum::router::UtoipaMethodRouter;

use super::{AdminRuntimeState, security::is_mutation};

const SESSION: &str = "SessionCookie";
const NIP98: &str = "Nip98Authorization";
const CSRF_COOKIE: &str = "CsrfCookie";
const CSRF_HEADER: &str = "CsrfHeader";

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Maincopy Admin API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Private API for Maincopy operators, CLI clients, and agents. \
            Host must match the configured admin origin. Use either a human session cookie \
            or a registered agent's NIP-98 Authorization proof; sending both is rejected. \
            Cookie mutations require the exact configured HTTPS Origin and matching CSRF \
            cookie/header values bound to that session. Login needs that Origin but no \
            existing session or CSRF token. Identity mutations require recent human \
            authentication (before the session's fresh_until) or a fresh agent proof. \
            Creating an account with a password, and adding, replacing, or removing a \
            password credential, always require recent human authentication. \
            Role and grant authorities are separate from authentication schemes; \
            x-maincopy-required-scope records the route's required authority. \
            Creating a user with roles other than Publisher also requires role_assign."
    ),
)]
pub(super) struct AdminApi;

impl AdminApi {
    pub(super) fn document() -> OpenApiDocument {
        let mut document = Self::openapi();
        let components = document.components.get_or_insert_default();
        for (name, scheme) in [
            (
                SESSION,
                ApiKey::Cookie(ApiKeyValue::with_description(
                    SESSION_COOKIE_NAME,
                    "Opaque human session returned by password or Nostr login. \
                        Secure, HttpOnly, SameSite=Strict; never send with Authorization.",
                )),
            ),
            (
                NIP98,
                ApiKey::Header(ApiKeyValue::with_description(
                    "Authorization",
                    "The complete header value is Nostr <base64-encoded signed event JSON>. \
                        Use a registered agent key to sign a fresh NIP-98 event for each request. \
                        The proof binds the HTTP method, configured HTTPS URL including query, \
                        and a payload tag with the SHA-256 hash of the exact body, even when empty. \
                        Mutations require Idempotency-Key and a matching idempotency event tag. \
                        Proof replay is rejected. \
                        This is not a bearer token; never send the private key or a session cookie.",
                )),
            ),
            (
                CSRF_COOKIE,
                ApiKey::Cookie(ApiKeyValue::with_description(
                    CSRF_COOKIE_NAME,
                    "Required with SessionCookie for mutations. Its value must equal \
                        x-maincopy-csrf and be bound to the authenticated session.",
                )),
            ),
            (
                CSRF_HEADER,
                ApiKey::Header(ApiKeyValue::with_description(
                    CSRF_HEADER_NAME,
                    "Required with SessionCookie for API mutations. Copy the CSRF cookie value \
                        exactly; also send Origin equal to the configured HTTPS admin origin.",
                )),
            ),
        ] {
            components.add_security_scheme(name, SecurityScheme::ApiKey(scheme));
        }
        document
    }
}

/// These policies accompany the registered runtime layers. HumanMutations adds
/// the source handler's stricter requirement without changing its read routes.
#[derive(Clone, Copy)]
pub(super) enum RouteAuthentication {
    PublicLogin,
    BrowserSession,
    Scoped(AdminScope),
    HumanMutations,
}

pub(super) fn describe_authentication(
    mut routes: UtoipaMethodRouter<AdminRuntimeState>,
    policy: RouteAuthentication,
) -> UtoipaMethodRouter<AdminRuntimeState> {
    for path in routes.1.paths.values_mut() {
        for (method, operation) in [
            (Method::GET, &mut path.get),
            (Method::HEAD, &mut path.head),
            (Method::OPTIONS, &mut path.options),
            (Method::POST, &mut path.post),
            (Method::PUT, &mut path.put),
            (Method::PATCH, &mut path.patch),
            (Method::DELETE, &mut path.delete),
            (Method::TRACE, &mut path.trace),
        ] {
            if let Some(operation) = operation {
                policy.describe(operation, &method);
            }
        }
    }
    routes
}

impl RouteAuthentication {
    fn describe(self, operation: &mut Operation, method: &Method) {
        if let Self::Scoped(scope) = self {
            operation
                .extensions
                .get_or_insert_default()
                .insert("x-maincopy-required-scope".into(), scope.as_str().into());
        }
        // A human-only handler can narrow the outer authentication layer.
        if operation.security.is_some() {
            return;
        }
        let mutation = is_mutation(method);
        let mut session = SecurityRequirement::new(SESSION, [] as [&str; 0]);
        if mutation {
            session = session
                .add(CSRF_COOKIE, [] as [&str; 0])
                .add(CSRF_HEADER, [] as [&str; 0]);
        }
        let (requirements, origin_required) = match self {
            Self::PublicLogin => (Vec::new(), Required::True),
            Self::BrowserSession => (vec![session], Required::True),
            Self::Scoped(_) => (
                vec![session, SecurityRequirement::new(NIP98, [] as [&str; 0])],
                Required::False,
            ),
            Self::HumanMutations if mutation => {
                operation.description.get_or_insert_default().push_str(
                    "\n\nRequires recent human sign-in before the session's fresh_until. \
                        Agent proofs are not accepted for this operation.",
                );
                (vec![session], Required::True)
            }
            Self::HumanMutations => return,
        };
        operation.security = Some(requirements);
        if mutation {
            operation.parameters.get_or_insert_default().push(
                ParameterBuilder::new()
                    .name("Origin")
                    .parameter_in(ParameterIn::Header)
                    .required(origin_required)
                    .schema(Some(String::schema()))
                    .description(Some(
                        "Must equal the configured HTTPS admin origin for login or session-cookie \
                            mutations. Not required for NIP-98 agent authentication. Cookie mutations \
                            also require equal CSRF cookie/header values bound to the session.",
                    ))
                    .build(),
            );
        }
    }
}

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
