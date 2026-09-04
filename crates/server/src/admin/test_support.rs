use std::{collections::BTreeSet, path::Path};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{
        HeaderMap, Method, Request, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, HOST, ORIGIN, SET_COOKIE},
    },
};
use base64::{Engine as _, engine::general_purpose};
use k256::schnorr::SigningKey;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;
use uuid::Uuid;
use zeroize::Zeroizing;

use maincopy_shared::{
    auth::{AdminAuditEventId, AdminScope, AgentCredentialId, InstanceId, UserId},
    auth_api::{CSRF_COOKIE_NAME, SESSION_COOKIE_NAME},
    publication::IDEMPOTENCY_KEY_HEADER,
};

use super::{
    AdminSecurityState, AdminSessionPolicy, admin_router, origin::AdminOrigin, runtime_admin_router,
};
use crate::{
    config::{
        DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
        DatabaseWriterQueueCapacity,
    },
    database::{self, store::DatabaseStore},
    domain::{
        auth::{
            Argon2idPolicy, CanonicalUsername, NIP98_EVENT_KIND, NostrPublicKey,
            store::{
                AdminMutationKey, AuditPrincipalReference, BootstrapIdentity,
                ConfiguredLoginProviders, MutationAuditContext, NewHumanCredential,
                RegisterAgentCredential,
            },
        },
        publication::activation::PublicationCoordinatorHandle,
    },
    source_sync::{SourceRuntimeMode, SourceSyncHandle},
};

pub(crate) const ADMIN_ORIGIN: &str = "https://admin.example.test";
pub(crate) const ADMIN_AUTHORITY: &str = "admin.example.test";
pub(crate) const OWNER_USERNAME: &str = "owner";
pub(crate) const OWNER_PASSWORD: &str = "correct horse battery staple";

#[derive(Clone, Copy)]
enum OwnerCredential {
    Nostr,
    Password,
}

pub(crate) struct BrowserSession {
    cookie_header: Zeroizing<String>,
    csrf_token: Zeroizing<String>,
}

pub(crate) struct ProtectedAdminHarness {
    _root: tempfile::TempDir,
    state: AdminSecurityState,
    store: DatabaseStore,
    signing_key: SigningKey,
    shutdown: CancellationToken,
    writer: JoinHandle<()>,
}

impl ProtectedAdminHarness {
    pub(crate) async fn start() -> Self {
        Self::start_with_owner_credential(OwnerCredential::Nostr).await
    }

    pub(crate) async fn start_with_password() -> Self {
        Self::start_with_owner_credential(OwnerCredential::Password).await
    }

    async fn start_with_owner_credential(owner_credential: OwnerCredential) -> Self {
        let root = tempfile::tempdir().expect("admin test root must be created");
        let path = root.path().join("state/maincopy.db");
        let database = database::bootstrap(database_configuration(&path))
            .await
            .expect("admin test database must start");
        let (store, writer) = database.into_store(64);
        let shutdown = CancellationToken::new();
        let writer_shutdown = shutdown.clone();
        let writer = tokio::spawn(async move {
            writer
                .run(writer_shutdown)
                .await
                .expect("admin test writer must stop cleanly");
        });

        let providers = ConfiguredLoginProviders::new(true, true)
            .expect("password and Nostr are a valid provider set");
        let owner_user_id = UserId::from_uuid(Uuid::new_v4());
        let password_policy = Argon2idPolicy::v1();
        let credential = match owner_credential {
            OwnerCredential::Nostr => {
                let owner_key = SigningKey::from_bytes(&[4_u8; 32])
                    .expect("the fixed owner test key must be valid");
                NewHumanCredential::Nostr {
                    public_key: public_key(&owner_key),
                }
            }
            OwnerCredential::Password => NewHumanCredential::Password {
                username: CanonicalUsername::parse(OWNER_USERNAME)
                    .expect("the fixed owner username must be valid"),
                password_hash: password_policy
                    .hash_password(OWNER_PASSWORD)
                    .expect("the fixed owner password must hash"),
                policy_version: 1,
            },
        };
        store
            .auth
            .bootstrap_identity(BootstrapIdentity {
                instance_id: InstanceId::from_uuid(Uuid::new_v4()),
                owner_user_id,
                credential,
                configured_providers: providers,
                occurred_at: OffsetDateTime::now_utc(),
                audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
            })
            .await
            .expect("admin test identity must bootstrap");

        let signing_key =
            SigningKey::from_bytes(&[3_u8; 32]).expect("the fixed agent test key must be valid");
        store
            .auth
            .register_agent_credential(RegisterAgentCredential {
                credential_id: AgentCredentialId::from_uuid(Uuid::new_v4()),
                owner_user_id,
                issuer_user_id: owner_user_id,
                public_key: public_key(&signing_key),
                label: "protected router contract test agent".into(),
                scopes: AdminScope::ALL.into_iter().collect::<BTreeSet<_>>(),
                created_at: OffsetDateTime::now_utc(),
                expires_at: None,
                audit: MutationAuditContext {
                    audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
                    principal: AuditPrincipalReference::Offline {
                        user_id: Some(owner_user_id),
                    },
                    request_id: None,
                    idempotency_key: AdminMutationKey(Uuid::new_v4()),
                },
            })
            .await
            .expect("admin test agent must register");

        let state = AdminSecurityState::new(
            AdminOrigin::parse(ADMIN_ORIGIN).expect("the test origin must be valid"),
            store.auth.clone(),
            providers,
            AdminSessionPolicy::default(),
            password_policy,
        )
        .await
        .expect("admin test security must initialize");

        Self {
            _root: root,
            state,
            store,
            signing_key,
            shutdown,
            writer,
        }
    }

    pub(crate) fn runtime_router(&self, publications: PublicationCoordinatorHandle) -> Router {
        runtime_admin_router(
            publications,
            self.state.clone(),
            self.store.profiles.clone(),
            SourceSyncHandle::new(
                self.store.source.clone(),
                SourceRuntimeMode::ExternalCheckout,
            ),
        )
    }

    pub(crate) fn router(&self) -> Router {
        admin_router(self.state.clone())
    }

    pub(crate) fn request(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
        idempotency_key: Option<&str>,
    ) -> Request<Body> {
        let generated_key = Uuid::new_v4().to_string();
        let proof_key = idempotency_key.or(Some(generated_key.as_str()));
        let authorization = agent_authorization(&self.signing_key, &method, path, &body, proof_key);
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, ADMIN_AUTHORITY)
            .header(ORIGIN, ADMIN_ORIGIN)
            .header(AUTHORIZATION, authorization);
        if let Some(key) = idempotency_key {
            request = request.header(IDEMPOTENCY_KEY_HEADER, key);
        } else {
            request = request.header(IDEMPOTENCY_KEY_HEADER, generated_key);
        }
        request
            .body(Body::from(body))
            .expect("the admin test request must build")
    }

    pub(crate) async fn password_login(&self, router: &Router) -> BrowserSession {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("username", OWNER_USERNAME)
            .append_pair("password", OWNER_PASSWORD)
            .finish();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/admin/login")
            .header(HOST, ADMIN_AUTHORITY)
            .header(ORIGIN, ADMIN_ORIGIN)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .expect("the password login request must build");
        let response = router
            .clone()
            .oneshot(request)
            .await
            .expect("the password login request must complete");
        assert_eq!(
            response.status(),
            StatusCode::SEE_OTHER,
            "the fixed owner password must create a browser session"
        );

        BrowserSession::from_login_headers(response.headers())
    }

    pub(crate) async fn stop(self) {
        self.shutdown.cancel();
        drop(self.state);
        drop(self.store);
        self.writer.await.expect("admin test writer task must join");
    }
}

impl BrowserSession {
    fn from_login_headers(headers: &HeaderMap) -> Self {
        let session_cookie = required_cookie_pair(headers, SESSION_COOKIE_NAME);
        let csrf_cookie = required_cookie_pair(headers, CSRF_COOKIE_NAME);
        let csrf_token = Zeroizing::new(
            csrf_cookie
                .strip_prefix(&format!("{CSRF_COOKIE_NAME}="))
                .expect("the selected CSRF cookie must have its required name")
                .to_owned(),
        );
        let cookie_header = Zeroizing::new(format!(
            "{}; {}",
            session_cookie.as_str(),
            csrf_cookie.as_str()
        ));
        Self {
            cookie_header,
            csrf_token,
        }
    }

    pub(crate) fn request(&self, method: Method, path: &str, body: Bytes) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, ADMIN_AUTHORITY)
            .header(ORIGIN, ADMIN_ORIGIN)
            .header(COOKIE, self.cookie_header.as_str())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .expect("the browser test request must build")
    }

    pub(crate) fn as_csrf_token(&self) -> &str {
        self.csrf_token.as_str()
    }
}

fn required_cookie_pair(headers: &HeaderMap, name: &str) -> Zeroizing<String> {
    let prefix = format!("{name}=");
    let mut selected = None;
    for header in headers.get_all(SET_COOKIE) {
        let value = header
            .to_str()
            .expect("a login Set-Cookie header must be visible ASCII");
        let pair = value
            .split(';')
            .next()
            .expect("a login Set-Cookie header must contain a cookie pair");
        if pair.starts_with(&prefix) {
            assert!(selected.is_none(), "a login cookie name must occur once");
            selected = Some(Zeroizing::new(pair.to_owned()));
        }
    }
    selected.expect("the password login response must set each authentication cookie")
}

fn database_configuration(path: &Path) -> DatabaseConfigurationView<'_> {
    DatabaseConfigurationView {
        path,
        busy_timeout: DatabaseBusyTimeout::from_milliseconds(1_000)
            .expect("the test timeout must be valid"),
        writer_queue_capacity: DatabaseWriterQueueCapacity::new(64)
            .expect("the test queue capacity must be valid"),
        read_pool_size: DatabaseReadPoolSize::new(2)
            .expect("the test read pool size must be valid"),
    }
}

fn public_key(key: &SigningKey) -> NostrPublicKey {
    NostrPublicKey::from_bytes(key.verifying_key().to_bytes().into())
        .expect("a Schnorr verifying key must be a valid Nostr public key")
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Serialize)]
struct SignedEvent {
    id: String,
    pubkey: String,
    created_at: i64,
    kind: u64,
    tags: Vec<Vec<String>>,
    content: String,
    sig: String,
}

pub(crate) fn agent_authorization(
    signing_key: &SigningKey,
    method: &Method,
    path: &str,
    body: &[u8],
    idempotency_key: Option<&str>,
) -> String {
    let created_at = OffsetDateTime::now_utc().unix_timestamp();
    let mut tags = vec![
        vec!["u".into(), format!("{ADMIN_ORIGIN}{path}")],
        vec!["method".into(), method.as_str().into()],
        vec!["payload".into(), lower_hex(&Sha256::digest(body))],
    ];
    if let Some(key) = idempotency_key {
        tags.push(vec!["idempotency".into(), key.into()]);
    }
    let pubkey = lower_hex(&signing_key.verifying_key().to_bytes());
    let content = String::new();
    let encoded = serde_json::to_vec(&(0, &pubkey, created_at, NIP98_EVENT_KIND, &tags, &content))
        .expect("the test NIP-01 event must serialize");
    let event_id: [u8; 32] = Sha256::digest(encoded).into();
    let signature = signing_key
        .sign_raw(&event_id, &[7_u8; 32])
        .expect("the fixed test key must sign")
        .to_bytes();
    let event = SignedEvent {
        id: lower_hex(&event_id),
        pubkey,
        created_at,
        kind: NIP98_EVENT_KIND,
        tags,
        content,
        sig: lower_hex(&signature),
    };
    format!(
        "Nostr {}",
        general_purpose::STANDARD
            .encode(serde_json::to_vec(&event).expect("the signed test event must serialize"))
    )
}

#[cfg(test)]
mod tests {
    use maincopy_shared::CAPABILITIES_PATH;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn password_harness_creates_authenticated_browser_requests() {
        let harness = ProtectedAdminHarness::start_with_password().await;
        let router = harness.router();
        let browser = harness.password_login(&router).await;

        assert!(browser.as_csrf_token().starts_with("mcc1_"));
        let response = router
            .oneshot(browser.request(Method::GET, CAPABILITIES_PATH, Bytes::new()))
            .await
            .expect("the authenticated browser request must complete");
        assert_eq!(response.status(), StatusCode::OK);

        harness.stop().await;
    }
}
