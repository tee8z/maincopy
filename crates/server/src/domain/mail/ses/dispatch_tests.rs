//! Exercise the concrete dispatcher through its supervised task and a local SES
//! peer. All recipient transitions use the application database's real writer.

use std::{
    fs::OpenOptions, io::Write as _, os::unix::fs::OpenOptionsExt as _, sync::Arc, time::Duration,
};

use axum::{Router, body::Bytes, routing::post};
use k256::schnorr::SigningKey;
use maincopy_shared::auth::{AdminAuditEventId, InstanceId, UserId};
use markdown_compiler::PublicationBaseUrl;
use sqlx::{ConnectOptions as _, Connection as _, SqliteConnection, sqlite::SqliteConnectOptions};
use time::OffsetDateTime;
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::*;
use crate::{
    config::{
        DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
        DatabaseWriterQueueCapacity,
    },
    database::{self, DatabaseStore},
    domain::{
        auth::{
            NostrPublicKey,
            store::{BootstrapIdentity, ConfiguredLoginProviders, NewHumanCredential},
        },
        mail::{
            config::{MailConfiguration, MailConfigurationCandidate, SesMailConfiguration},
            controls::MailControls,
            dispatch::{DispatchError, DispatchResources, MailDispatcher},
            subscriber::{
                BeginFeedbackRun, ClaimConfirmation, DeliveryAdmission, EnrollmentRequestResult,
                FeedbackObservation, RecordFeedbackObservation, RequestEnrollment,
                SubscriberDigest, SubscriberMode, SubscriberPolicy,
            },
        },
    },
};

const CREDENTIAL: &[u8] =
    br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"1234567890123456"}"#;
const ACCEPTED: &str = r#"{"MessageId":"010001-dispatch-fixture"}"#;
const SOURCE: [u8; 32] = [9; 32];

#[path = "campaign_tests.rs"]
mod campaign_tests;

struct CapturedRequest {
    body: serde_json::Value,
    reply: oneshot::Sender<&'static str>,
}

struct DispatcherTask {
    stop: CancellationToken,
    task: JoinHandle<Result<(), DispatchError>>,
}

impl DispatcherTask {
    async fn finish(self) {
        self.stop.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

struct Fixture {
    root: tempfile::TempDir,
    store: DatabaseStore,
    owner: UserId,
    reader: SqliteConnection,
    controls: Arc<MailControls>,
    configuration: SesMailConfiguration,
    binding: [u8; 32],
    endpoint: Url,
    requests: mpsc::Receiver<CapturedRequest>,
    stop_writer: CancellationToken,
    writer: JoinHandle<()>,
    stop_peer: CancellationToken,
    peer: JoinHandle<()>,
}

impl Fixture {
    async fn start(confirmation_budget: u64) -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/maincopy.db");
        let database = database::bootstrap(DatabaseConfigurationView {
            path: &path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(32).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        })
        .await
        .unwrap();
        let (store, writer) = database.into_store(32);
        let stop_writer = CancellationToken::new();
        let stopped = stop_writer.clone();
        let writer = tokio::spawn(async move {
            writer.run(stopped).await.unwrap();
        });
        let (instance, owner) = bootstrap_owner(&store).await;
        let controls = load_controls(root.path(), instance);
        let configuration = configuration(root.path(), confirmation_budget);
        let binding = configuration.provider_binding(&SesCredentials::parse(CREDENTIAL).unwrap());
        store
            .subscribers
            .initialize_controls(controls.identity_binding(&origin()))
            .await
            .unwrap();
        store
            .subscribers
            .set_policy(SubscriberPolicy {
                configuration_binding: binding,
                mode: SubscriberMode::Enabled,
                max_daily_messages: 10,
                max_daily_confirmations: confirmation_budget,
                max_campaign_recipients: 10,
            })
            .await
            .unwrap();
        ready_feedback(&store, binding).await;
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .read_only(true)
            .disable_statement_logging();
        let reader = SqliteConnection::connect_with(&options).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (sender, requests) = mpsc::channel(4);
        let router = Router::new().route(
            "/v2/email/outbound-emails",
            post(move |body: Bytes| {
                let sender = sender.clone();
                async move {
                    let (reply, response) = oneshot::channel();
                    sender
                        .send(CapturedRequest {
                            body: serde_json::from_slice(&body).unwrap(),
                            reply,
                        })
                        .await
                        .unwrap();
                    response.await.unwrap()
                }
            }),
        );
        let stop_peer = CancellationToken::new();
        let stopped = stop_peer.clone();
        let peer = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
                .unwrap();
        });
        Self {
            root,
            store,
            owner,
            reader,
            controls,
            configuration,
            binding,
            endpoint,
            requests,
            stop_writer,
            writer,
            stop_peer,
            peer,
        }
    }

    fn dispatch(&self) -> DispatcherTask {
        let mut client = SesClient::new(
            SesRegion::parse("us-east-1").unwrap(),
            SesCredentials::parse(CREDENTIAL).unwrap().into(),
            SesConfiguration {
                sender: EmailAddress::parse("sender@example.com").unwrap(),
                configuration_set: ResourceName::parse("maincopy-newsletter").unwrap(),
            },
        )
        .unwrap();
        client.endpoint = self.endpoint.clone();
        client.http = http_client(false).unwrap();
        let dispatcher = MailDispatcher::new(DispatchResources {
            campaigns: self.store.mail.clone(),
            subscribers: self.store.subscribers.clone(),
            client,
            controls: self.controls.clone(),
            origin: origin(),
            configuration_binding: self.binding,
            configuration: self.configuration.clone(),
            policy: self.configuration.view().subscriptions.unwrap().clone(),
        });
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        DispatcherTask {
            stop,
            task: tokio::spawn(dispatcher.run(stopped)),
        }
    }

    async fn queue(&self, mailbox: &str) -> Uuid {
        let (attempt, result) = self.enroll(mailbox).await;
        assert_eq!(result, EnrollmentRequestResult::Queued);
        attempt
    }

    async fn enroll(&self, mailbox: &str) -> (Uuid, EnrollmentRequestResult) {
        let address = EmailAddress::parse(mailbox).unwrap();
        let attempt = Uuid::new_v4();
        let result = self
            .store
            .subscribers
            .request_enrollment(RequestEnrollment {
                mailbox_digest: SubscriberDigest::from_bytes(
                    self.controls.mailbox_digest(&address),
                ),
                address,
                enrollment: Uuid::new_v4(),
                generation: Uuid::new_v4(),
                confirmation_attempt: attempt,
                configuration_binding: self.binding,
            })
            .await
            .unwrap();
        (attempt, result)
    }

    async fn request(&mut self) -> CapturedRequest {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn outcome(&mut self, attempt: Uuid) -> String {
        sqlx::query_scalar("SELECT outcome FROM mail_attempts WHERE attempt_id=?")
            .bind(attempt.as_bytes().as_slice())
            .fetch_one(&mut self.reader)
            .await
            .unwrap()
    }

    async fn finish(self) {
        self.stop_peer.cancel();
        self.peer.await.unwrap();
        self.stop_writer.cancel();
        self.writer.await.unwrap();
        self.reader.close().await.unwrap();
        drop(self.store);
        drop(self.root);
    }
}

fn origin() -> PublicationBaseUrl {
    PublicationBaseUrl::parse("https://example.com").unwrap()
}

fn configuration(root: &std::path::Path, budget: u64) -> SesMailConfiguration {
    let source = format!(
        r#"
mode = "ses"
sender = "sender@example.com"
region = "us-east-1"
configuration_set = "maincopy-newsletter"
credential_file = "ses.json"
control_signing_key_file = "control.key"
max_campaign_recipients = 10
max_daily_messages = 10
max_daily_confirmation_messages = {budget}
send_interval_milliseconds = 100
[subscriptions]
mode = "enabled"
operator_name = "Example Publisher"
postal_address = "123 Example Street, Example City"
purpose = "New public articles"
privacy_url = "https://example.com/privacy"
contact_address = "contact@example.com"
[feedback]
queue_url = "https://sqs.us-east-1.amazonaws.com/123456789012/maincopy-feedback"
topic_arn = "arn:aws:sns:us-east-1:123456789012:maincopy-feedback"
"#
    );
    let candidate: MailConfigurationCandidate = toml::from_str(&source).unwrap();
    let MailConfiguration::Ses(configuration) = candidate.validate(root).unwrap() else {
        panic!("expected SES");
    };
    *configuration
}

fn load_controls(root: &std::path::Path, instance: InstanceId) -> Arc<MailControls> {
    let path = root.join("control.key");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.write_all("7a".repeat(32).as_bytes()).unwrap();
    drop(file);
    Arc::new(MailControls::load(&path, instance).unwrap())
}

async fn bootstrap_owner(store: &DatabaseStore) -> (InstanceId, UserId) {
    let instance = InstanceId::from_uuid(Uuid::new_v4());
    let owner = UserId::from_uuid(Uuid::new_v4());
    let key = SigningKey::from_bytes(&[7; 32]).unwrap();
    store
        .auth
        .bootstrap_identity(BootstrapIdentity {
            instance_id: instance,
            owner_user_id: owner,
            credential: NewHumanCredential::Nostr {
                public_key: NostrPublicKey::from_bytes(key.verifying_key().to_bytes().into())
                    .unwrap(),
            },
            configured_providers: ConfiguredLoginProviders::new(false, true).unwrap(),
            occurred_at: OffsetDateTime::now_utc(),
            audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
        })
        .await
        .unwrap();
    (instance, owner)
}

async fn ready_feedback(store: &DatabaseStore, binding: [u8; 32]) {
    let provider_now = OffsetDateTime::now_utc();
    let run = store
        .subscribers
        .begin_feedback_run(BeginFeedbackRun {
            provider_now,
            configuration_binding: binding,
            source_binding: SOURCE,
            retention_seconds: 86_400,
        })
        .await
        .unwrap();
    store
        .subscribers
        .record_feedback_observation(RecordFeedbackObservation {
            configuration_binding: binding,
            run_id: run.run_id,
            source_binding: SOURCE,
            retention_seconds: 86_400,
            observation: FeedbackObservation::Observed {
                provider_now,
                drained: true,
            },
        })
        .await
        .unwrap();
}

fn assert_confirmation(request: &CapturedRequest, attempt: Uuid, mailbox: &str) {
    assert_eq!(
        request.body["Destination"]["ToAddresses"],
        serde_json::json!([mailbox])
    );
    assert_eq!(request.body["EmailTags"][0]["Name"], "maincopy-attempt");
    assert_eq!(request.body["EmailTags"][0]["Value"], attempt.to_string());
    assert_eq!(request.body["EmailTags"][1]["Name"], "maincopy-epoch");
    assert!(request.body["Content"]["Simple"].get("Headers").is_none());
    let text = request.body["Content"]["Simple"]["Body"]["Text"]["Data"]
        .as_str()
        .unwrap();
    assert!(text.contains("https://example.com/email/confirm/"));
    assert!(text.contains("https://example.com/email/unsubscribe/"));
    assert!(text.contains("123 Example Street"));
}

#[tokio::test]
async fn accepted_confirmation_commits_and_restart_sends_only_new_work() {
    let mut fixture = Fixture::start(10).await;
    let first = fixture.queue("First@example.com").await;
    let task = fixture.dispatch();
    let request = fixture.request().await;
    assert_confirmation(&request, first, "First@example.com");
    request.reply.send(ACCEPTED).unwrap();
    task.finish().await;
    assert_eq!(fixture.outcome(first).await, "accepted");
    assert_eq!(
        fixture
            .store
            .subscribers
            .status()
            .await
            .unwrap()
            .active_enrollments,
        0
    );
    assert_eq!(
        fixture
            .store
            .subscribers
            .quarantine_interrupted()
            .await
            .unwrap(),
        0
    );
    let second = fixture.queue("second@example.com").await;
    let task = fixture.dispatch();
    let request = fixture.request().await;
    assert_confirmation(&request, second, "second@example.com");
    request.reply.send(ACCEPTED).unwrap();
    task.finish().await;
    assert_eq!(fixture.outcome(second).await, "accepted");
    assert!(fixture.requests.try_recv().is_err());
    fixture.finish().await;
}

#[tokio::test]
async fn unknown_provider_reply_and_interrupted_admission_are_never_resent() {
    let mut fixture = Fixture::start(10).await;
    let unknown = fixture.queue("unknown@example.com").await;
    let task = fixture.dispatch();
    let request = fixture.request().await;
    request.reply.send("{incomplete").unwrap();
    task.finish().await;
    assert_eq!(fixture.outcome(unknown).await, "unknown");
    let interrupted = fixture.queue("interrupted@example.com").await;
    let handle = fixture
        .store
        .subscribers
        .queued_confirmations(fixture.binding, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(handle.attempt_id, interrupted);
    let permit = fixture
        .store
        .subscribers
        .claim_confirmation(ClaimConfirmation {
            attempt_id: interrupted,
            nonce_digest: SubscriberDigest::from_bytes([8; 32]),
            expires_at: handle.pending_expires_at,
            configuration_binding: fixture.binding,
        })
        .await
        .unwrap();
    assert!(matches!(permit, DeliveryAdmission::Ready(_)));
    drop(permit);
    assert_eq!(
        fixture
            .store
            .subscribers
            .quarantine_interrupted()
            .await
            .unwrap(),
        1
    );
    assert_eq!(fixture.outcome(interrupted).await, "unknown");
    let next = fixture.queue("next@example.com").await;
    let task = fixture.dispatch();
    let request = fixture.request().await;
    assert_confirmation(&request, next, "next@example.com");
    request.reply.send(ACCEPTED).unwrap();
    task.finish().await;
    assert_eq!(fixture.outcome(unknown).await, "unknown");
    assert_eq!(fixture.outcome(interrupted).await, "unknown");
    assert!(fixture.requests.try_recv().is_err());
    fixture.finish().await;
}

#[tokio::test]
async fn cancellation_drains_an_accepted_http_request_and_commits_its_reply() {
    let mut fixture = Fixture::start(10).await;
    let attempt = fixture.queue("reader@example.com").await;
    let task = fixture.dispatch();
    let request = fixture.request().await;
    assert_eq!(fixture.outcome(attempt).await, "admitted");
    task.stop.cancel();
    tokio::task::yield_now().await;
    assert!(
        !task.task.is_finished(),
        "accepted HTTP work must drain during shutdown"
    );
    request.reply.send(ACCEPTED).unwrap();
    task.finish().await;
    assert_eq!(fixture.outcome(attempt).await, "accepted");
    assert!(fixture.requests.try_recv().is_err());
    fixture.finish().await;
}

#[tokio::test]
async fn confirmation_budget_prevents_capture_and_provider_requests_after_restart() {
    let mut fixture = Fixture::start(1).await;
    let first = fixture.queue("first@example.com").await;
    let task = fixture.dispatch();
    fixture.request().await.reply.send(ACCEPTED).unwrap();
    task.finish().await;
    let (_, result) = fixture.enroll("second@example.com").await;
    assert_eq!(result, EnrollmentRequestResult::Unchanged);
    let task = fixture.dispatch();
    assert!(
        tokio::time::timeout(Duration::from_millis(400), fixture.requests.recv())
            .await
            .is_err()
    );
    task.finish().await;
    assert_eq!(fixture.outcome(first).await, "accepted");
    assert_eq!(
        fixture
            .store
            .subscribers
            .status()
            .await
            .unwrap()
            .addressed_enrollments,
        1
    );
    assert!(
        fixture
            .store
            .subscribers
            .queued_confirmations(fixture.binding, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let (total, confirmations): (i64, i64) =
        sqlx::query_as("SELECT SUM(total),SUM(confirmations) FROM mail_daily_budget")
            .fetch_one(&mut fixture.reader)
            .await
            .unwrap();
    assert_eq!((total, confirmations), (1, 1));
    fixture.finish().await;
}
