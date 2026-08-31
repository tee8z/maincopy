use std::{collections::BTreeSet, path::Path};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{
        Method, Request,
        header::{AUTHORIZATION, HOST, ORIGIN},
    },
};
use base64::{Engine as _, engine::general_purpose};
use k256::schnorr::SigningKey;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use maincopy_shared::{
    auth::{AdminAuditEventId, AdminScope, AgentCredentialId, InstanceId, UserId},
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
            Argon2idPolicy, NIP98_EVENT_KIND, NostrPublicKey,
            store::{
                AdminMutationKey, AuditPrincipalReference, BootstrapIdentity,
                ConfiguredLoginProviders, MutationAuditContext, NewHumanCredential,
                RegisterAgentCredential,
            },
        },
        publication::activation::PublicationCoordinatorHandle,
    },
};

pub(crate) const ADMIN_ORIGIN: &str = "https://admin.example.test";
pub(crate) const ADMIN_AUTHORITY: &str = "admin.example.test";

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
        let owner_key =
            SigningKey::from_bytes(&[4_u8; 32]).expect("the fixed owner test key must be valid");
        store
            .auth
            .bootstrap_identity(BootstrapIdentity {
                instance_id: InstanceId::from_uuid(Uuid::new_v4()),
                owner_user_id,
                credential: NewHumanCredential::Nostr {
                    public_key: public_key(&owner_key),
                },
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
            Argon2idPolicy::v1(),
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

    pub(crate) async fn stop(self) {
        drop(self.state);
        drop(self.store);
        self.shutdown.cancel();
        self.writer.await.expect("admin test writer task must join");
    }
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
