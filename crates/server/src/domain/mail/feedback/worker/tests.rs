use std::{fs::OpenOptions, io::Write as _, os::unix::fs::OpenOptionsExt as _};

use axum::{Router, body::Bytes, extract::State, http::HeaderMap, routing::post};
use maincopy_shared::auth::InstanceId;
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;
use tokio::{
    net::TcpListener,
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
};
use url::Url;
use uuid::Uuid;

use super::*;
use crate::{
    database::store::Mutation,
    domain::mail::{
        feedback::{
            FeedbackConfiguration, FeedbackRejection,
            tests::{dead_attributes, source_attributes},
        },
        identity::EmailAddress,
        ses::SesCredentials,
        subscriber::{ControlOutcome, FeedbackRunAdmission},
    },
};

const BINDING: [u8; 32] = [7; 32];
const CAMPAIGN: &str = "11111111-1111-4111-8111-111111111111";
const EPOCH: &str = "44444444-4444-4444-8444-444444444444";
const ATTEMPT: &str = "22222222-2222-4222-8222-222222222222";
const QUEUE: &str = "https://sqs.us-east-1.amazonaws.com/123456789012/maincopy-feedback";
const TOPIC: &str = "arn:aws:sns:us-east-1:123456789012:maincopy-feedback";

#[derive(Clone)]
struct PeerState {
    requests: mpsc::UnboundedSender<String>,
    receive: String,
    receive_gate: Option<Arc<Notify>>,
    invalid_policy: bool,
    source_messages: u32,
}

async fn respond(State(state): State<PeerState>, headers: HeaderMap, body: Bytes) -> String {
    let target = headers["x-amz-target"].to_str().unwrap();
    state.requests.send(target.into()).unwrap();
    match target {
        "AmazonSQS.ReceiveMessage" => {
            if let Some(gate) = state.receive_gate {
                gate.notified().await;
            }
            state.receive
        }
        "AmazonSQS.DeleteMessage" => String::new(),
        "AmazonSQS.GetQueueAttributes" => {
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let source = request["QueueUrl"] == QUEUE;
            let mut value = if source {
                source_attributes()
            } else {
                dead_attributes()
            };
            if source {
                value["Attributes"]["ApproximateNumberOfMessages"] =
                    json!(state.source_messages.to_string());
            }
            if source && state.invalid_policy {
                value["Attributes"]["Policy"] = json!("{}");
            }
            value.to_string()
        }
        _ => panic!("unexpected queue operation"),
    }
}

struct Harness {
    root: tempfile::TempDir,
    worker: FeedbackWorker,
    sender: mpsc::Sender<Mutation>,
    mutations: mpsc::Receiver<Mutation>,
    requests: mpsc::UnboundedReceiver<String>,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Harness {
    async fn start() -> Self {
        Self::start_with_response(None).await
    }

    async fn start_with_response(response: Option<&str>) -> Self {
        Self::start_with_receive_gate(response, None).await
    }

    async fn start_with_receive_gate(
        response: Option<&str>,
        receive_gate: Option<Arc<Notify>>,
    ) -> Self {
        Self::start_with_contract(response, receive_gate, false, 2).await
    }

    async fn start_with_contract(
        response: Option<&str>,
        receive_gate: Option<Arc<Notify>>,
        invalid_policy: bool,
        source_messages: u32,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        let key_path = root.path().join("control.key");
        let mut key = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_path)
            .unwrap();
        key.write_all("ab".repeat(32).as_bytes()).unwrap();
        drop(key);
        let controls =
            Arc::new(MailControls::load(&key_path, InstanceId::from_uuid(Uuid::new_v4())).unwrap());
        let configuration =
            FeedbackConfiguration::new(QUEUE, TOPIC, "us-east-1", "newsletter").unwrap();
        let credentials = SesCredentials::parse(
            br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"protected-fixture-secret"}"#,
        )
        .unwrap();
        let mut client = FeedbackClient::new(configuration, credentials.into()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        client.endpoint =
            Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        client.http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let (requests, received) = mpsc::unbounded_channel();
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let event = json!({
            "eventType":"Complaint", "mail": {
                "sendingAccountId":"123456789012",
                "sourceArn":"arn:aws:ses:us-east-1:123456789012:identity/sender@example.com",
                "timestamp":now, "messageId":"010001-feedback-fixture", "destination":["reader@example.com"],
                "tags":{"ses:configuration-set":["newsletter"],"maincopy-campaign":[CAMPAIGN],"maincopy-attempt":[ATTEMPT],"maincopy-epoch":[EPOCH]}
            },
            "complaint":{"timestamp":now,"complainedRecipients":[{"emailAddress":"reader@example.com"}],"complaintFeedbackType":"abuse"}
        });
        let envelope = json!({"Type":"Notification","TopicArn":TOPIC,"MessageId":Uuid::new_v4(),"Timestamp":now,"Message":event.to_string()});
        let receive = json!({"Messages":[{"Body":envelope.to_string(),"ReceiptHandle":"protected-fixture-receipt","Attributes":{"ApproximateReceiveCount":"1"}}]}).to_string();
        let receive = response.map(str::to_owned).unwrap_or(receive);
        let router = Router::new()
            .route("/", post(respond))
            .with_state(PeerState {
                requests,
                receive,
                receive_gate,
                invalid_policy,
                source_messages,
            });
        let stop = CancellationToken::new();
        let stopping = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopping.cancelled_owned())
                .await
                .unwrap();
        });
        let (sender, mutations) = mpsc::channel(1);
        let readers = SqlitePoolOptions::new()
            .connect_lazy("sqlite::memory:")
            .unwrap();
        let subscribers = SubscriberStore::new(readers, sender.clone());
        let worker = FeedbackWorker::new(client, controls, subscribers, BINDING);
        Self {
            root,
            worker,
            sender,
            mutations,
            requests: received,
            stop,
            task,
        }
    }

    async fn delivery(&mut self) -> FeedbackDelivery {
        let delivery = self
            .worker
            .client
            .receive(OffsetDateTime::now_utc())
            .await
            .unwrap()
            .delivery
            .unwrap();
        assert_eq!(
            self.requests.recv().await.unwrap(),
            "AmazonSQS.ReceiveMessage"
        );
        delivery
    }

    async fn finish(self) {
        drop(self.worker);
        self.stop.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap();
        drop(self.root);
    }
}

#[tokio::test]
async fn feedback_acknowledgement_waits_for_the_correlated_writer_commit() {
    let mut harness = Harness::start().await;
    let delivery = harness.delivery().await;
    {
        let applying = harness.worker.apply_delivery(delivery);
        tokio::pin!(applying);
        let mutation = tokio::select! {
            biased;
            _ = &mut applying => panic!("feedback cannot finish before the writer replies"),
            mutation = harness.mutations.recv() => mutation.unwrap(),
        };
        let Mutation::ApplyMailFeedback {
            command,
            respond_to,
        } = mutation
        else {
            panic!("the recipient transition precedes acknowledgement");
        };
        assert_eq!(command.configuration_binding, BINDING);
        assert_eq!(command.attempt_id, Uuid::parse_str(ATTEMPT).unwrap());
        assert_eq!(
            command.campaign_id,
            Some(CampaignId(Uuid::parse_str(CAMPAIGN).unwrap()))
        );
        assert_eq!(
            command.provider_message_id.as_str(),
            "010001-feedback-fixture"
        );
        assert_eq!(command.kind, StoredFeedbackKind::Complaint);
        let recipient = EmailAddress::parse("reader@example.com").unwrap();
        assert_eq!(
            command.mailbox_digest.as_bytes(),
            &harness.worker.controls.mailbox_digest(&recipient)
        );
        assert!(harness.requests.try_recv().is_err());
        respond_to.send(Ok(ControlOutcome::Changed)).unwrap();
        assert_eq!(applying.await.unwrap(), DeliveryOutcome::Acknowledged);
    }
    assert_eq!(
        harness.requests.recv().await.unwrap(),
        "AmazonSQS.DeleteMessage"
    );
    harness.finish().await;
}

#[tokio::test]
async fn lost_writer_reply_and_poison_feedback_never_acknowledge_the_queue() {
    let mut harness = Harness::start().await;
    let delivery = harness.delivery().await;
    {
        let applying = harness.worker.apply_delivery(delivery);
        tokio::pin!(applying);
        let mutation = tokio::select! {
            biased;
            _ = &mut applying => panic!("feedback must await writer confirmation"),
            mutation = harness.mutations.recv() => mutation.unwrap(),
        };
        let Mutation::ApplyMailFeedback { respond_to, .. } = mutation else {
            panic!("expected feedback transition")
        };
        drop(respond_to);
        assert!(matches!(
            applying.await,
            Err(WriterFailure::Fatal(
                FeedbackWorkerError::DatabaseUnavailable
            ))
        ));
    }
    assert!(harness.requests.try_recv().is_err());
    let mut delivery = harness.delivery().await;
    delivery.event = Err(FeedbackRejection::Correlation);
    assert_eq!(
        harness.worker.apply_delivery(delivery).await.unwrap(),
        DeliveryOutcome::Poison
    );
    assert!(harness.mutations.try_recv().is_err());
    assert!(harness.requests.try_recv().is_err());
    harness.finish().await;
}

#[tokio::test(start_paused = true)]
async fn writer_backpressure_cannot_refresh_old_health_or_discard_pending_reconciliation() {
    let mut harness = Harness::start().await;
    for (observation, expected) in [
        (
            FeedbackObservation::Observed {
                provider_now: provider_now(),
                drained: true,
            },
            FeedbackObservation::Unavailable,
        ),
        (
            FeedbackObservation::ReconciliationRequired,
            FeedbackObservation::ReconciliationRequired,
        ),
    ] {
        let (respond_to, _reply) = oneshot::channel();
        assert!(
            harness
                .sender
                .try_send(Mutation::PauseMailSubscribers { respond_to })
                .is_ok()
        );
        let run = consumer_run(false);
        let publishing = harness.worker.publish_health(&run, observation);
        tokio::pin!(publishing);
        tokio::select! {
            biased;
            _ = &mut publishing => panic!("a full writer queue cannot record feedback health"),
            _ = tokio::task::yield_now() => {},
        }
        drop(harness.mutations.recv().await.unwrap());
        tokio::time::advance(INITIAL_BACKOFF).await;
        let mutation = tokio::select! {
            biased;
            _ = &mut publishing => panic!("health must wait for its writer reply"),
            mutation = harness.mutations.recv() => mutation.unwrap(),
        };
        let Mutation::RecordMailFeedbackObservation {
            command,
            respond_to,
        } = mutation
        else {
            panic!("expected health observation")
        };
        assert_eq!(command.configuration_binding, BINDING);
        assert_eq!(command.run_id, run.run_id);
        assert_eq!(command.observation, expected);
        respond_to.send(Ok(())).unwrap();
        assert_eq!(publishing.await.unwrap(), expected);
    }
    assert!(harness.requests.try_recv().is_err());
    harness.finish().await;
}

#[tokio::test]
async fn cancellation_waits_for_admitted_health_and_stops_before_any_receive() {
    let harness = Harness::start().await;
    let Harness {
        worker,
        mut mutations,
        mut requests,
        stop,
        task,
        root,
        sender,
    } = harness;
    let shutdown = CancellationToken::new();
    let stopped = shutdown.clone();
    let mut running = tokio::spawn(worker.run(stopped));
    let Mutation::RecordMailFeedbackHealth {
        health, respond_to, ..
    } = mutations.recv().await.unwrap()
    else {
        panic!("startup must first pause admissions")
    };
    assert_eq!(health, FeedbackHealth::Unavailable);
    shutdown.cancel();
    tokio::select! {
        biased;
        _ = &mut running => panic!("cancellation must not discard an admitted writer reply"),
        _ = tokio::task::yield_now() => {},
    }
    respond_to.send(Ok(())).unwrap();
    tokio::time::timeout(Duration::from_secs(1), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(requests.try_recv().is_err());
    drop(sender);
    stop.cancel();
    task.await.unwrap();
    drop(root);
}

fn consumer_run(drained: bool) -> ConsumerRun {
    ConsumerRun {
        run_id: Uuid::new_v4(),
        source_binding: [8; 32],
        retention_seconds: 86_400,
        preflight_at: Instant::now() + PREFLIGHT_INTERVAL,
        drained,
        provider_now: provider_now(),
        recover_after: None,
        reconciliation_required: false,
    }
}

fn provider_now() -> OffsetDateTime {
    OffsetDateTime::now_utc().replace_nanosecond(0).unwrap()
}

fn poll_intent() -> FeedbackPollIntent {
    let signed_at = provider_now();
    FeedbackPollIntent {
        poll_id: Uuid::new_v4(),
        signed_at,
        recover_after: signed_at + time::Duration::seconds(1042),
    }
}

async fn apply_empty_poll(harness: &mut Harness, run: &mut ConsumerRun) -> PollObservation {
    let intent = poll_intent();
    let poll = harness
        .worker
        .client
        .receive(intent.signed_at)
        .await
        .unwrap();
    assert!(poll.delivery.is_none());
    assert_eq!(
        harness.requests.recv().await.unwrap(),
        "AmazonSQS.ReceiveMessage"
    );
    let applying = harness.worker.apply_poll(run, intent, poll);
    tokio::pin!(applying);
    let mutation = tokio::select! {
        biased;
        _ = &mut applying => panic!("empty receive still needs its durable intent closed"),
        mutation = harness.mutations.recv() => mutation.unwrap(),
    };
    let Mutation::CompleteMailFeedbackPoll {
        poll_id,
        respond_to,
        ..
    } = mutation
    else {
        panic!("expected completion")
    };
    assert_eq!(poll_id, intent.poll_id);
    respond_to.send(Ok(())).unwrap();
    applying.await.unwrap()
}

#[tokio::test]
async fn empty_receive_cannot_clear_known_backlog_or_reconciliation() {
    let mut harness = Harness::start_with_response(Some("{}")).await;
    let mut run = consumer_run(false);
    for (queued, in_flight, delayed) in [(1, 0, 0), (0, 1, 0), (0, 0, 1)] {
        assert!(run.observe_queue(FeedbackQueueStatus {
            provider_now: provider_now(),
            queued,
            in_flight,
            delayed,
            dead_lettered: 0,
            source_retention_seconds: 86_400
        }));
        assert!(matches!(
            apply_empty_poll(&mut harness, &mut run).await,
            PollObservation::Observed(FeedbackObservation::Observed { drained: false, .. })
        ));
    }
    assert!(run.observe_queue(FeedbackQueueStatus {
        provider_now: provider_now(),
        queued: 0,
        in_flight: 0,
        delayed: 0,
        dead_lettered: 0,
        source_retention_seconds: 86_400
    }));
    assert!(matches!(
        apply_empty_poll(&mut harness, &mut run).await,
        PollObservation::Observed(FeedbackObservation::Observed { drained: true, .. })
    ));
    assert_eq!(
        run.preserve_reconciliation(FeedbackObservation::ReconciliationRequired),
        FeedbackObservation::ReconciliationRequired
    );
    assert_eq!(
        run.preserve_reconciliation(FeedbackObservation::Observed {
            provider_now: provider_now(),
            drained: true
        }),
        FeedbackObservation::ReconciliationRequired
    );
    harness.finish().await;
}

#[tokio::test]
async fn malformed_receive_retains_a_recovery_lease_without_discarding_consent() {
    let mut harness = Harness::start_with_response(Some("{incomplete")).await;
    let shutdown = CancellationToken::new();
    let mut run = consumer_run(true);
    let intent = poll_intent();
    {
        let cycling = harness.worker.cycle(&mut run, &shutdown);
        tokio::pin!(cycling);
        let mutation = tokio::select! {
            biased;
            _ = &mut cycling => panic!("receive requires a durable intent"),
            mutation = harness.mutations.recv() => mutation.unwrap(),
        };
        let Mutation::BeginMailFeedbackPoll { respond_to, .. } = mutation else {
            panic!("expected poll admission")
        };
        assert!(harness.requests.try_recv().is_err());
        respond_to
            .send(Ok(FeedbackPollAdmission::Ready(intent)))
            .unwrap();
        let mutation = tokio::select! {
            biased;
            _ = &mut cycling => panic!("lost reply must leave a durable recovery bound"),
            mutation = harness.mutations.recv() => mutation.unwrap(),
        };
        let Mutation::DeferMailFeedbackPoll {
            poll_id,
            respond_to,
            ..
        } = mutation
        else {
            panic!("expected deferred intent")
        };
        assert_eq!(poll_id, intent.poll_id);
        respond_to.send(Ok(())).unwrap();
        assert_eq!(
            cycling.await.unwrap(),
            Some(PollObservation::Retry(FeedbackObservation::Unavailable))
        );
    }
    assert_eq!(run.recover_after, Some(intent.recover_after));
    assert!(!run.reconciliation_required);
    assert_eq!(
        harness.requests.recv().await.unwrap(),
        "AmazonSQS.ReceiveMessage"
    );
    assert_eq!(
        harness.worker.cycle(&mut run, &shutdown).await.unwrap(),
        Some(PollObservation::Retry(FeedbackObservation::Unavailable))
    );
    assert!(harness.mutations.try_recv().is_err());
    assert!(harness.requests.try_recv().is_err());
    harness.finish().await;
}

#[tokio::test]
async fn regressing_provider_preflight_cannot_release_a_pending_receive_lease() {
    let mut harness = Harness::start_with_response(Some("{}")).await;
    let mut run = consumer_run(true);
    // The last trusted provider observation precedes a provider clock rollback.
    // Socket I/O still uses real time; the fresh Date is older than that record.
    run.provider_now += time::Duration::days(1);
    let last_observed = run.provider_now;
    let deadline = last_observed + time::Duration::seconds(1042);
    run.recover_after = Some(deadline);
    run.preflight_at = Instant::now();
    assert_eq!(
        harness
            .worker
            .cycle(&mut run, &CancellationToken::new())
            .await
            .unwrap(),
        Some(PollObservation::Retry(FeedbackObservation::Unavailable))
    );
    assert_eq!(run.provider_now, last_observed);
    assert_eq!(run.recover_after, Some(deadline));
    assert!(!run.drained);
    assert!(!run.reconciliation_required);
    assert!(harness.mutations.try_recv().is_err());
    for _ in 0..2 {
        assert_eq!(
            harness.requests.try_recv().unwrap(),
            "AmazonSQS.GetQueueAttributes"
        );
    }
    assert!(harness.requests.try_recv().is_err());
    harness.finish().await;
}

#[tokio::test]
async fn empty_preflight_cannot_override_the_writers_pending_receive_lease() {
    let mut harness = Harness::start_with_contract(Some("{}"), None, false, 0).await;
    let mut run = consumer_run(false);
    // Keep the previous observation older than HTTP Date's second precision.
    run.provider_now -= time::Duration::seconds(30);
    run.preflight_at = Instant::now();
    let shutdown = CancellationToken::new();
    {
        let cycling = harness.worker.cycle(&mut run, &shutdown);
        tokio::pin!(cycling);
        let mutation = tokio::select! {
            biased;
            _ = &mut cycling => panic!("preflight must commit provider time before attempting receive admission"),
            mutation = harness.mutations.recv() => mutation.unwrap(),
        };
        let Mutation::RecordMailFeedbackObservation {
            command,
            respond_to,
        } = mutation
        else {
            panic!("expected checked queue observation")
        };
        assert_eq!(command.configuration_binding, BINDING);
        assert!(matches!(
            command.observation,
            FeedbackObservation::Checked { .. }
        ));
        respond_to.send(Ok(())).unwrap();
        let mutation = tokio::select! {
            biased;
            _ = &mut cycling => panic!("the writer still owns the durable receive lease"),
            mutation = harness.mutations.recv() => mutation.unwrap(),
        };
        let Mutation::BeginMailFeedbackPoll {
            binding,
            respond_to,
            ..
        } = mutation
        else {
            panic!("expected poll admission")
        };
        assert_eq!(binding, BINDING);
        respond_to
            .send(Ok(FeedbackPollAdmission::Recovering))
            .unwrap();
        assert_eq!(
            cycling.await.unwrap(),
            Some(PollObservation::Retry(FeedbackObservation::Unavailable))
        );
    }
    for _ in 0..2 {
        assert_eq!(
            harness.requests.try_recv().unwrap(),
            "AmazonSQS.GetQueueAttributes"
        );
    }
    assert!(harness.requests.try_recv().is_err());
    assert!(harness.mutations.try_recv().is_err());
    assert!(!run.reconciliation_required);
    harness.finish().await;
}

#[tokio::test]
async fn changed_queue_policy_during_a_run_commits_reconciliation_before_another_receive() {
    let mut harness = Harness::start_with_contract(None, None, true, 2).await;
    let mut run = consumer_run(true);
    run.preflight_at = Instant::now();
    let shutdown = CancellationToken::new();
    {
        let cycling = harness.worker.cycle(&mut run, &shutdown);
        tokio::pin!(cycling);
        let mutation = tokio::select! {
            biased;
            _ = &mut cycling => panic!("an observed policy breach must reach the writer"),
            mutation = harness.mutations.recv() => mutation.unwrap(),
        };
        let Mutation::RecordMailFeedbackIntegrityFailure {
            binding,
            respond_to,
        } = mutation
        else {
            panic!("expected durable integrity transition")
        };
        assert_eq!(binding, BINDING);
        assert_eq!(
            harness.requests.try_recv().unwrap(),
            "AmazonSQS.GetQueueAttributes"
        );
        assert!(harness.requests.try_recv().is_err());
        // Shutdown cannot abandon the observed breach while its write is admitted.
        shutdown.cancel();
        tokio::select! {
            biased;
            _ = &mut cycling => panic!("the admitted reconciliation write must drain"),
            _ = tokio::task::yield_now() => {},
        }
        respond_to
            .send(Ok(FeedbackHealth::ReconciliationRequired))
            .unwrap();
        assert_eq!(
            cycling.await.unwrap(),
            Some(PollObservation::Retry(
                FeedbackObservation::ReconciliationRequired
            ))
        );
    }
    assert!(!run.drained);
    assert!(harness.requests.try_recv().is_err());
    assert!(harness.mutations.try_recv().is_err());
    harness.finish().await;
}

#[tokio::test]
async fn writer_backpressure_retains_received_feedback_until_admission_succeeds() {
    let mut harness = Harness::start().await;
    let delivery = harness.delivery().await;
    // Pause only the local retry timer, not loopback socket I/O.
    tokio::time::pause();
    let (respond_to, _reply) = oneshot::channel();
    assert!(
        harness
            .sender
            .try_send(Mutation::PauseMailSubscribers { respond_to })
            .is_ok()
    );
    {
        let applying = harness.worker.apply_delivery(delivery);
        tokio::pin!(applying);
        tokio::select! {
            biased;
            _ = &mut applying => panic!("a full writer queue must retain its received feedback"),
            _ = tokio::task::yield_now() => {},
        }
        assert!(harness.requests.try_recv().is_err());
        drop(harness.mutations.recv().await.unwrap());
        tokio::time::advance(INITIAL_BACKOFF).await;
        let mutation = tokio::select! {
            biased;
            _ = &mut applying => panic!("feedback needs a confirmed writer commit"),
            mutation = harness.mutations.recv() => mutation.unwrap(),
        };
        let Mutation::ApplyMailFeedback {
            command,
            respond_to,
        } = mutation
        else {
            panic!("expected retained feedback")
        };
        assert_eq!(command.attempt_id, Uuid::parse_str(ATTEMPT).unwrap());
        assert!(harness.requests.try_recv().is_err());
        tokio::time::resume();
        respond_to.send(Ok(ControlOutcome::Changed)).unwrap();
        assert_eq!(applying.await.unwrap(), DeliveryOutcome::Acknowledged);
    }
    assert_eq!(
        harness.requests.recv().await.unwrap(),
        "AmazonSQS.DeleteMessage"
    );
    harness.finish().await;
}

#[tokio::test]
async fn shutdown_drains_an_accepted_receive_before_clearing_the_consumer_marker() {
    let gate = Arc::new(Notify::new());
    let Harness {
        worker,
        mut mutations,
        mut requests,
        stop,
        task,
        root,
        sender,
    } = Harness::start_with_receive_gate(None, Some(gate.clone())).await;
    let shutdown = CancellationToken::new();
    let mut running = tokio::spawn(worker.run(shutdown.clone()));
    let Mutation::RecordMailFeedbackHealth {
        health, respond_to, ..
    } = mutations.recv().await.unwrap()
    else {
        panic!("startup first pauses admission")
    };
    assert_eq!(health, FeedbackHealth::Unavailable);
    respond_to.send(Ok(())).unwrap();
    let Mutation::BeginMailFeedbackRun {
        command,
        respond_to,
    } = mutations.recv().await.unwrap()
    else {
        panic!("queue polling requires a durable consumer marker")
    };
    assert_eq!(command.configuration_binding, BINDING);
    assert_eq!(command.retention_seconds, 86_400);
    let run_id = Uuid::new_v4();
    respond_to
        .send(Ok(FeedbackRunAdmission {
            run_id,
            health: FeedbackHealth::Unavailable,
            recover_after: None,
        }))
        .unwrap();
    let Mutation::RecordMailFeedbackObservation {
        command,
        respond_to,
    } = mutations.recv().await.unwrap()
    else {
        panic!("initial run remains unavailable until drain")
    };
    assert_eq!(command.run_id, run_id);
    assert!(matches!(
        command.observation,
        FeedbackObservation::Observed { drained: false, .. }
    ));
    respond_to.send(Ok(())).unwrap();
    let Mutation::RecordMailFeedbackObservation {
        command,
        respond_to,
    } = mutations.recv().await.unwrap()
    else {
        panic!("preflight records provider time before poll admission")
    };
    assert!(matches!(
        command.observation,
        FeedbackObservation::Observed { drained: false, .. }
    ));
    respond_to.send(Ok(())).unwrap();
    let Mutation::BeginMailFeedbackPoll {
        binding,
        run_id: polled_run,
        respond_to,
    } = mutations.recv().await.unwrap()
    else {
        panic!("every receive is preceded by durable intent")
    };
    assert_eq!(binding, BINDING);
    assert_eq!(polled_run, run_id);
    let intent = poll_intent();
    respond_to
        .send(Ok(FeedbackPollAdmission::Ready(intent)))
        .unwrap();
    loop {
        let target = requests.recv().await.unwrap();
        if target == "AmazonSQS.ReceiveMessage" {
            break;
        }
        assert_eq!(target, "AmazonSQS.GetQueueAttributes");
    }
    // The peer accepted the receive but has not replied. Cancellation must
    // not lose that visibility lease or clear the run marker behind it.
    shutdown.cancel();
    tokio::select! {
        biased;
        _ = &mut running => panic!("an accepted receive must drain"),
        _ = tokio::task::yield_now() => {},
    }
    assert!(mutations.try_recv().is_err());
    gate.notify_one();
    let Mutation::ApplyMailFeedback { respond_to, .. } = mutations.recv().await.unwrap() else {
        panic!("received feedback must reach the writer")
    };
    assert!(requests.try_recv().is_err());
    respond_to.send(Ok(ControlOutcome::Changed)).unwrap();
    assert_eq!(requests.recv().await.unwrap(), "AmazonSQS.DeleteMessage");
    let Mutation::CompleteMailFeedbackPoll {
        poll_id,
        respond_to,
        ..
    } = mutations.recv().await.unwrap()
    else {
        panic!("confirmed feedback closes its exact poll intent")
    };
    assert_eq!(poll_id, intent.poll_id);
    respond_to.send(Ok(())).unwrap();
    let Mutation::RecordMailFeedbackObservation {
        command,
        respond_to,
    } = mutations.recv().await.unwrap()
    else {
        panic!("drained feedback records unavailable health before exit")
    };
    assert!(matches!(
        command.observation,
        FeedbackObservation::Observed { drained: false, .. }
    ));
    respond_to.send(Ok(())).unwrap();
    let Mutation::FinishMailFeedbackRun {
        configuration_binding,
        run_id: finished,
        respond_to,
    } = mutations.recv().await.unwrap()
    else {
        panic!("only completed feedback work releases the marker")
    };
    assert_eq!(configuration_binding, BINDING);
    assert_eq!(finished, run_id);
    assert!(!running.is_finished());
    respond_to.send(Ok(())).unwrap();
    tokio::time::timeout(Duration::from_secs(1), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(requests.try_recv().is_err());
    drop(sender);
    stop.cancel();
    task.await.unwrap();
    drop(root);
}

#[tokio::test]
async fn observed_queue_contract_failure_commits_the_gap_before_startup_can_stop() {
    let Harness {
        worker,
        mut mutations,
        mut requests,
        stop,
        task,
        root,
        sender,
    } = Harness::start_with_contract(None, None, true, 2).await;
    let shutdown = CancellationToken::new();
    let stopped = shutdown.clone();
    let mut running = tokio::spawn(async move { worker.start_run(&stopped).await });
    let Mutation::RecordMailFeedbackHealth { respond_to, .. } = mutations.recv().await.unwrap()
    else {
        panic!("startup first clears admission freshness");
    };
    respond_to.send(Ok(())).unwrap();
    assert_eq!(
        requests.recv().await.unwrap(),
        "AmazonSQS.GetQueueAttributes"
    );
    let Mutation::RecordMailFeedbackIntegrityFailure {
        binding,
        respond_to,
    } = mutations.recv().await.unwrap()
    else {
        panic!("an observed invalid resource policy must reach the writer");
    };
    assert_eq!(binding, BINDING);
    shutdown.cancel();
    tokio::select! {
        biased;
        _ = &mut running => panic!("shutdown cannot discard the admitted integrity transition"),
        _ = tokio::task::yield_now() => {},
    }
    respond_to
        .send(Ok(FeedbackHealth::ReconciliationRequired))
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_none()
    );
    assert!(mutations.try_recv().is_err());
    assert!(requests.try_recv().is_err());
    drop(sender);
    stop.cancel();
    task.await.unwrap();
    drop(root);
}

#[tokio::test]
async fn transient_preflight_failure_does_not_invent_a_breach_and_empty_setup_stays_unavailable() {
    let mut harness = Harness::start().await;
    for error in [
        FeedbackError::Transport,
        FeedbackError::AccessDenied,
        FeedbackError::Throttled,
        FeedbackError::InvalidResponse,
        FeedbackError::ServiceFailure,
    ] {
        assert_eq!(
            harness.worker.preflight_failure(error).await.unwrap(),
            FeedbackObservation::Unavailable
        );
        assert!(harness.mutations.try_recv().is_err());
    }
    for error in [
        FeedbackError::QueuePolicy,
        FeedbackError::QueueBounds,
        FeedbackError::DeadLetterPolicy,
    ] {
        let failure = harness.worker.preflight_failure(error);
        tokio::pin!(failure);
        let mutation = tokio::select! {
            biased;
            _ = &mut failure => panic!("only the writer decides whether prior consent requires a gap"),
            mutation = harness.mutations.recv() => mutation.unwrap(),
        };
        let Mutation::RecordMailFeedbackIntegrityFailure { respond_to, .. } = mutation else {
            panic!("expected integrity transition");
        };
        respond_to.send(Ok(FeedbackHealth::Unavailable)).unwrap();
        assert_eq!(failure.await.unwrap(), FeedbackObservation::Unavailable);
    }
    assert!(harness.requests.try_recv().is_err());
    harness.finish().await;
}
