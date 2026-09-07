use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::*;

const QUEUE: &str = "https://sqs.us-east-1.amazonaws.com/123456789012/maincopy-feedback";
const TOPIC: &str = "arn:aws:sns:us-east-1:123456789012:maincopy-feedback";
const QUEUE_ARN: &str = "arn:aws:sqs:us-east-1:123456789012:maincopy-feedback";
const DEAD_ARN: &str = "arn:aws:sqs:us-east-1:123456789012:maincopy-feedback-dead";
const CAMPAIGN: &str = "11111111-1111-4111-8111-111111111111";
const EPOCH: &str = "44444444-4444-4444-8444-444444444444";
const ATTEMPT: &str = "22222222-2222-4222-8222-222222222222";
const EVENT: &str = "33333333-3333-4333-8333-333333333333";
const CREDENTIALS: &[u8] = br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"}"#;

fn configuration() -> FeedbackConfiguration {
    FeedbackConfiguration::new(QUEUE, TOPIC, "us-east-1", "newsletter").unwrap()
}

fn now() -> OffsetDateTime {
    OffsetDateTime::parse(
        "2026-09-07T12:02:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap()
}

fn provider_event(kind: &str) -> Value {
    let mut event = json!({
        "eventType": kind,
        "mail": {
            "timestamp": "2026-09-07T12:00:00Z",
            "sourceArn": "arn:aws:ses:us-east-1:123456789012:identity/newsletter@example.com",
            "sendingAccountId": "123456789012",
            "messageId": "010001-accepted-message",
            "destination": ["Reader@EXAMPLE.COM"],
            "tags": {"ses:configuration-set": ["newsletter"], "maincopy-campaign": [CAMPAIGN], "maincopy-attempt": [ATTEMPT], "maincopy-epoch": [EPOCH]},
            "headers": [{"name": "List-Unsubscribe", "value": "<https://example.com/email/unsubscribe/private-fixture-control>"}],
            "futureMetadata": {"opaque": true}
        }
    });
    match kind {
        "Bounce" => {
            event["bounce"] = json!({
                "bounceType":"Permanent", "timestamp":"2026-09-07T12:01:00Z",
                "bouncedRecipients":[{"emailAddress":"forwarded@example.net", "diagnosticCode":"private-fixture-diagnostic"}]
            })
        }
        "Complaint" => {
            event["complaint"] = json!({
                "timestamp":"2026-09-07T12:01:00Z", "complaintFeedbackType":"abuse",
                "complainedRecipients":[{"emailAddress":"Reader@example.com"}]
            })
        }
        "Delivery" => {
            event["delivery"] = json!({
                "timestamp":"2026-09-07T12:01:00Z", "recipients":["Reader@example.com"],
                "smtpResponse":"private-fixture-diagnostic"
            })
        }
        "Send" => event["send"] = json!({}),
        "Reject" => event["reject"] = json!({"reason":"Bad content"}),
        "Rendering Failure" => {
            event["failure"] = json!({"templateName":"newsletter","errorMessage":"private-fixture-rendering-error"})
        }
        _ => {}
    }
    event
}

fn envelope(event: Value) -> Value {
    json!({
        "Type":"Notification", "MessageId":EVENT, "TopicArn":TOPIC,
        "Timestamp":"2026-09-07T12:01:30Z", "Message":event.to_string(),
        // SQS authentication and restricted queue publication are the trust
        // boundary. These URLs are never fetched or used as credential sources.
        "SigningCertURL":"https://untrusted.invalid/certificate",
        "UnsubscribeURL":"https://untrusted.invalid/unsubscribe"
    })
}

fn parse_envelope(value: Value) -> Result<AuthenticatedFeedback, FeedbackRejection> {
    event::parse(
        &serde_json::to_vec(&value).unwrap(),
        &configuration(),
        now(),
    )
}

#[test]
fn queue_configuration_rejects_endpoint_credentials_encodings_and_cross_account_topics() {
    for queue in [
        "http://sqs.us-east-1.amazonaws.com/123456789012/feedback",
        "https://sqs.us-east-1.amazonaws.com.attacker.invalid/123456789012/feedback",
        "https://name@sqs.us-east-1.amazonaws.com/123456789012/feedback",
        "https://sqs.us-east-1.amazonaws.com:444/123456789012/feedback",
        "https://sqs.us-east-1.amazonaws.com/123456789012/feedback?other=1",
        "https://sqs.us-east-1.amazonaws.com/123456789012/feedback#fragment",
        "https://sqs.us-east-1.amazonaws.com/123456789012/%66eedback",
        "https://sqs.us-east-1.amazonaws.com/123456789012/feedback.fifo",
        "https://sqs.us-east-1.amazonaws.com/123456789012/feedback/extra",
    ] {
        assert!(FeedbackConfiguration::new(queue, TOPIC, "us-east-1", "newsletter").is_err());
    }
    for topic in [
        "arn:aws:sns:us-west-2:123456789012:feedback",
        "arn:aws:sns:us-east-1:999999999999:feedback",
        "arn:aws:sns:us-east-1:123456789012:feedback.fifo",
        "arn:aws:sns:us-east-1:123456789012:*",
    ] {
        assert_eq!(
            FeedbackConfiguration::new(QUEUE, topic, "us-east-1", "newsletter").err(),
            Some(FeedbackConfigurationError::Topic)
        );
    }
}

#[test]
fn configured_feedback_preserves_exact_attempt_and_original_recipient_with_closed_outcomes() {
    for (provider, kind) in [
        ("Send", FeedbackKind::Accepted),
        ("Delivery", FeedbackKind::Delivered),
        ("Bounce", FeedbackKind::HardBounce),
        ("Complaint", FeedbackKind::Complaint),
        ("Reject", FeedbackKind::DeliveryFailed),
        ("Rendering Failure", FeedbackKind::DeliveryFailed),
    ] {
        let feedback = parse_envelope(envelope(provider_event(provider))).unwrap();
        assert_eq!(feedback.kind, kind);
        assert_eq!(feedback.recipient.as_str(), "Reader@example.com");
        assert_eq!(feedback.attempt_id, Uuid::parse_str(ATTEMPT).unwrap());
        assert_eq!(feedback.mail_epoch, Uuid::parse_str(EPOCH).unwrap());
        assert_eq!(
            feedback.campaign_id,
            Some(Uuid::parse_str(CAMPAIGN).unwrap())
        );
        assert_eq!(
            feedback.provider_message_id.as_str(),
            "010001-accepted-message"
        );
        assert!(feedback.sent_at <= feedback.occurred_at);
    }
    let mut confirmation = provider_event("Send");
    confirmation["mail"]["tags"]
        .as_object_mut()
        .unwrap()
        .remove("maincopy-campaign");
    assert!(
        parse_envelope(envelope(confirmation))
            .unwrap()
            .campaign_id
            .is_none()
    );
}

#[test]
fn foreign_providers_ambiguous_recipients_and_malformed_identifiers_cannot_select_consent() {
    let changes: [fn(&mut Value); 7] = [
        |event| event["mail"]["sendingAccountId"] = json!("999999999999"),
        |event| event["mail"]["tags"]["ses:configuration-set"] = json!(["other"]),
        |event| event["mail"]["destination"] = json!(["Reader@example.com", "other@example.com"]),
        |event| event["mail"]["tags"]["maincopy-attempt"] = json!([ATTEMPT, ATTEMPT]),
        |event| event["mail"]["tags"]["maincopy-attempt"] = json!([Uuid::nil().to_string()]),
        |event| event["mail"]["messageId"] = json!("private-fixture@example.com"),
        |event| event["delivery"]["recipients"] = json!(["other@example.com"]),
    ];
    for change in changes {
        let mut event = provider_event("Delivery");
        change(&mut event);
        let error = parse_envelope(envelope(event)).err().unwrap();
        assert!(!format!("{error:?} {error}").contains("private-fixture"));
    }
    let mut foreign = envelope(provider_event("Send"));
    foreign["TopicArn"] = json!("arn:aws:sns:us-east-1:999999999999:other");
    assert_eq!(
        parse_envelope(foreign).err(),
        Some(FeedbackRejection::Topic)
    );
    let mut confirmation = envelope(provider_event("Send"));
    confirmation["Type"] = json!("SubscriptionConfirmation");
    confirmation["SubscribeURL"] = json!("http://127.0.0.1/private-fixture");
    assert_eq!(
        parse_envelope(confirmation).err(),
        Some(FeedbackRejection::Topic)
    );
}

#[test]
fn late_complaints_remain_actionable_and_corrections_or_soft_bounces_never_unsuppress() {
    let mut complaint = provider_event("Complaint");
    complaint["mail"]["timestamp"] = json!("2026-07-01T12:00:00Z");
    complaint["complaint"]["timestamp"] = json!("2026-08-01T12:00:00Z");
    assert_eq!(
        parse_envelope(envelope(complaint.clone())).unwrap().kind,
        FeedbackKind::Complaint
    );
    complaint["complaint"]["complaintFeedbackType"] = json!("not-spam");
    assert_eq!(
        parse_envelope(envelope(complaint)).unwrap().kind,
        FeedbackKind::Accepted
    );
    let mut bounce = provider_event("Bounce");
    bounce["bounce"]["bounceType"] = json!("Transient");
    assert_eq!(
        parse_envelope(envelope(bounce)).unwrap().kind,
        FeedbackKind::DeliveryFailed
    );
    let mut future = provider_event("Delivery");
    future["delivery"]["timestamp"] = json!("2026-09-07T12:08:00Z");
    assert_eq!(
        parse_envelope(envelope(future)).err(),
        Some(FeedbackRejection::Timestamp)
    );
}

pub(super) fn source_attributes() -> Value {
    json!({"Attributes":{
        "QueueArn":QUEUE_ARN, "MessageRetentionPeriod":"86400", "MaximumMessageSize":"65536",
        "SqsManagedSseEnabled":"true", "ApproximateNumberOfMessagesNotVisible":"0", "ApproximateNumberOfMessagesDelayed":"0", "ApproximateNumberOfMessages":"2",
        "Policy":json!({"Version":"2012-10-17","Statement":[{
            "Effect":"Allow", "Principal":{"Service":"sns.amazonaws.com"},
            "Action":"sqs:SendMessage", "Resource":QUEUE_ARN,
            "Condition":{"ArnEquals":{"aws:SourceArn":TOPIC},"StringEquals":{"aws:SourceAccount":"123456789012"}}
        },{
            "Effect":"Deny", "Principal":"*", "Action":"sqs:SendMessage", "Resource":QUEUE_ARN,
            "Condition":{"ArnNotEquals":{"aws:SourceArn":TOPIC}}
        }]}).to_string(),
        "RedrivePolicy":json!({"deadLetterTargetArn":DEAD_ARN,"maxReceiveCount":5}).to_string()
    }})
}

pub(super) fn dead_attributes() -> Value {
    json!({"Attributes":{
        "QueueArn":DEAD_ARN, "MessageRetentionPeriod":"345600", "MaximumMessageSize":"65536",
        "SqsManagedSseEnabled":"true", "ApproximateNumberOfMessagesNotVisible":"0", "ApproximateNumberOfMessagesDelayed":"0", "ApproximateNumberOfMessages":"0",
        "RedriveAllowPolicy":json!({"redrivePermission":"byQueue","sourceQueueArns":[QUEUE_ARN]}).to_string()
    }})
}

fn received(envelope: Value) -> Value {
    json!({"Messages":[{"Body":envelope.to_string(), "ReceiptHandle":"protected-fixture-receipt",
        "Attributes":{"ApproximateReceiveCount":"2"}}]})
}

struct Reply {
    status: StatusCode,
    body: String,
    location: Option<String>,
    date: &'static str,
}
impl From<Value> for Reply {
    fn from(value: Value) -> Self {
        Self {
            status: StatusCode::OK,
            body: value.to_string(),
            location: None,
            date: "Mon, 07 Sep 2026 12:02:00 GMT",
        }
    }
}
struct Captured {
    headers: HeaderMap,
    body: Bytes,
}
struct PeerState {
    replies: VecDeque<Reply>,
    captured: Vec<Captured>,
}
struct Peer {
    client: FeedbackClient,
    state: Arc<Mutex<PeerState>>,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

async fn respond(
    State(state): State<Arc<Mutex<PeerState>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut state = state.lock().unwrap();
    state.captured.push(Captured { headers, body });
    let reply = state
        .replies
        .pop_front()
        .expect("only the expected protocol requests occur");
    let mut response = (reply.status, reply.body).into_response();
    response
        .headers_mut()
        .insert("date", reply.date.parse().unwrap());
    if let Some(location) = reply.location {
        response
            .headers_mut()
            .insert("location", location.parse().unwrap());
    }
    response
}

impl Peer {
    async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let state = Arc::new(Mutex::new(PeerState {
            replies: replies.into(),
            captured: Vec::new(),
        }));
        let router = Router::new()
            .route("/", post(respond))
            .with_state(Arc::clone(&state));
        let stop = CancellationToken::new();
        let stopping = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopping.cancelled_owned())
                .await
                .unwrap();
        });
        let mut client = FeedbackClient::new(
            configuration(),
            SesCredentials::parse(CREDENTIALS).unwrap().into(),
        )
        .unwrap();
        client.endpoint = endpoint;
        client.http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        Self {
            client,
            state,
            stop,
            task,
        }
    }

    async fn finish(self) -> Vec<Captured> {
        drop(self.client);
        self.stop.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap();
        let mut state = self.state.lock().unwrap();
        assert!(state.replies.is_empty());
        std::mem::take(&mut state.captured)
    }
}

#[tokio::test]
async fn preflight_retains_invisible_and_delayed_backlog_in_both_queues() {
    let mut source = source_attributes();
    source["Attributes"]["ApproximateNumberOfMessages"] = json!("0");
    source["Attributes"]["ApproximateNumberOfMessagesNotVisible"] = json!("3");
    source["Attributes"]["ApproximateNumberOfMessagesDelayed"] = json!("2");
    let mut dead = dead_attributes();
    dead["Attributes"]["ApproximateNumberOfMessagesNotVisible"] = json!("4");
    dead["Attributes"]["ApproximateNumberOfMessagesDelayed"] = json!("1");
    let peer = Peer::start(vec![source.into(), dead.into()]).await;
    let status = peer.client.preflight().await.unwrap();
    assert_eq!((status.queued, status.in_flight, status.delayed), (0, 3, 2));
    assert_eq!(status.dead_lettered, 5);
    assert_eq!(status.source_retention_seconds, 86_400);
    assert!(!status.is_drained());
    assert_eq!(peer.finish().await.len(), 2);
}

#[tokio::test]
async fn incomplete_queue_counts_cannot_claim_a_drained_source() {
    let mut source = source_attributes();
    source["Attributes"]
        .as_object_mut()
        .unwrap()
        .remove("ApproximateNumberOfMessagesNotVisible");
    let peer = Peer::start(vec![source.into()]).await;
    assert_eq!(
        peer.client.preflight().await.err(),
        Some(FeedbackError::InvalidResponse)
    );
    assert_eq!(peer.finish().await.len(), 1);
}

#[tokio::test]
async fn preflight_checks_both_protected_queues_and_returns_only_aggregate_counts() {
    let peer = Peer::start(vec![source_attributes().into(), dead_attributes().into()]).await;
    let status = peer.client.preflight().await.unwrap();
    assert_eq!((status.queued, status.dead_lettered), (2, 0));
    let requests = peer.finish().await;
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(
            request.headers["x-amz-target"],
            "AmazonSQS.GetQueueAttributes"
        );
        assert!(
            request.headers["authorization"]
                .to_str()
                .unwrap()
                .contains("/us-east-1/sqs/aws4_request")
        );
    }
    let dead_request: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(dead_request["QueueUrl"], format!("{QUEUE}-dead"));
}

#[tokio::test]
async fn broad_queue_policies_unencrypted_state_and_missing_dead_letters_fail_preflight() {
    let changes: [fn(&mut Value); 5] = [
        |reply| {
            reply["Attributes"]["Policy"] = json!(
                reply["Attributes"]["Policy"]
                    .as_str()
                    .unwrap()
                    .replace("sns.amazonaws.com", "*")
            )
        },
        |reply| {
            reply["Attributes"]["Policy"] = json!(
                reply["Attributes"]["Policy"]
                    .as_str()
                    .unwrap()
                    .replace(TOPIC, "arn:aws:sns:us-east-1:123456789012:other")
            )
        },
        |reply| reply["Attributes"]["SqsManagedSseEnabled"] = json!("false"),
        |reply| reply["Attributes"]["MessageRetentionPeriod"] = json!("86401"),
        |reply| {
            reply["Attributes"]
                .as_object_mut()
                .unwrap()
                .remove("RedrivePolicy");
        },
    ];
    for change in changes {
        let mut attributes = source_attributes();
        change(&mut attributes);
        let peer = Peer::start(vec![attributes.into()]).await;
        assert!(peer.client.preflight().await.is_err());
        assert_eq!(peer.finish().await.len(), 1);
    }
    let mut dead = dead_attributes();
    dead["Attributes"]["RedriveAllowPolicy"] =
        json!(json!({"redrivePermission":"allowAll","sourceQueueArns":[QUEUE_ARN]}).to_string());
    let peer = Peer::start(vec![source_attributes().into(), dead.into()]).await;
    assert_eq!(
        peer.client.preflight().await.err(),
        Some(FeedbackError::DeadLetterPolicy)
    );
    peer.finish().await;
}

#[tokio::test]
async fn queue_authentication_requires_an_unconditional_deny_for_other_or_absent_sources() {
    let changes: [fn(&mut Value); 5] = [
        |policy| {
            policy["Statement"].as_array_mut().unwrap().truncate(1);
        },
        |policy| policy["Statement"][1]["Condition"] = json!({}),
        |policy| {
            policy["Statement"][1]["Condition"]["ArnNotEquals"]["aws:SourceArn"] =
                json!("arn:aws:sns:us-east-1:123456789012:other")
        },
        |policy| {
            policy["Statement"][1]["Condition"]["StringEquals"] =
                json!({"aws:SourceAccount":"123456789012"})
        },
        |policy| policy["Statement"][1]["Principal"] = json!({"Service":"sns.amazonaws.com"}),
    ];
    for change in changes {
        let mut attributes = source_attributes();
        let mut policy: Value =
            serde_json::from_str(attributes["Attributes"]["Policy"].as_str().unwrap()).unwrap();
        change(&mut policy);
        attributes["Attributes"]["Policy"] = json!(policy.to_string());
        let peer = Peer::start(vec![attributes.into()]).await;
        assert_eq!(
            peer.client.preflight().await.err(),
            Some(FeedbackError::QueuePolicy)
        );
        assert_eq!(peer.finish().await.len(), 1);
    }
}

#[tokio::test]
async fn signed_poll_retains_receipt_until_explicit_post_commit_acknowledgement() {
    let peer = Peer::start(vec![
        received(envelope(provider_event("Delivery"))).into(),
        json!({}).into(),
    ])
    .await;
    let delivery = peer.client.receive(now()).await.unwrap().delivery.unwrap();
    assert_eq!(delivery.event.unwrap().kind, FeedbackKind::Delivered);
    assert_eq!(peer.state.lock().unwrap().captured.len(), 1);
    peer.client.acknowledge(delivery.receipt).await.unwrap();
    let requests = peer.finish().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].headers["x-amz-target"],
        "AmazonSQS.ReceiveMessage"
    );
    assert!(
        requests[0].headers["authorization"]
            .to_str()
            .unwrap()
            .contains("content-type;host;x-amz-date;x-amz-target")
    );
    let poll: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(poll["QueueUrl"], QUEUE);
    assert_eq!(poll["MaxNumberOfMessages"], 1);
    assert_eq!(poll["WaitTimeSeconds"], 20);
    assert_eq!(poll["VisibilityTimeout"], 120);
    assert_eq!(requests[0].headers["x-amz-date"], "20260907T120200Z");
    assert_eq!(
        requests[1].headers["x-amz-target"],
        "AmazonSQS.DeleteMessage"
    );
    let ack: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(ack["QueueUrl"], QUEUE);
    assert_eq!(ack["ReceiptHandle"], "protected-fixture-receipt");
}

#[tokio::test]
async fn poison_is_not_acknowledged_and_redirects_cannot_retarget_feedback_requests() {
    let mut foreign = envelope(provider_event("Complaint"));
    foreign["TopicArn"] = json!("arn:aws:sns:us-east-1:123456789012:foreign");
    let peer = Peer::start(vec![received(foreign).into()]).await;
    let delivery = peer.client.receive(now()).await.unwrap().delivery.unwrap();
    assert_eq!(delivery.event.err(), Some(FeedbackRejection::Topic));
    drop(delivery.receipt);
    assert_eq!(peer.finish().await.len(), 1);
    let peer = Peer::start(vec![Reply {
        status: StatusCode::TEMPORARY_REDIRECT,
        body: String::new(),
        location: Some("https://untrusted.invalid/feedback".into()),
        date: "Mon, 07 Sep 2026 12:02:00 GMT",
    }])
    .await;
    assert_eq!(
        peer.client.receive(now()).await.err(),
        Some(FeedbackError::QueueUnavailable)
    );
    assert_eq!(peer.finish().await.len(), 1);
}

#[tokio::test]
async fn oversized_or_multiple_notifications_are_bounded_without_silent_ack_or_retry() {
    let mut oversized = received(envelope(provider_event("Send")));
    oversized["Messages"][0]["Body"] = json!("x".repeat(65_537));
    let peer = Peer::start(vec![oversized.into()]).await;
    assert_eq!(
        peer.client
            .receive(now())
            .await
            .unwrap()
            .delivery
            .unwrap()
            .event
            .err(),
        Some(FeedbackRejection::Envelope)
    );
    peer.finish().await;
    let mut multiple = received(envelope(provider_event("Send")));
    let copy = multiple["Messages"][0].clone();
    multiple["Messages"].as_array_mut().unwrap().push(copy);
    let peer = Peer::start(vec![multiple.into()]).await;
    assert_eq!(
        peer.client.receive(now()).await.err(),
        Some(FeedbackError::InvalidResponse)
    );
    peer.finish().await;
}

#[tokio::test]
async fn empty_poll_and_unknown_acknowledgement_never_invent_delivery_or_automatic_retries() {
    let peer = Peer::start(vec![json!({}).into()]).await;
    assert!(peer.client.receive(now()).await.unwrap().delivery.is_none());
    peer.finish().await;
    let peer = Peer::start(vec![
        received(envelope(provider_event("Send"))).into(),
        Reply {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: "private-fixture-provider-error".into(),
            location: None,
            date: "Mon, 07 Sep 2026 12:02:00 GMT",
        },
    ])
    .await;
    let delivery = peer.client.receive(now()).await.unwrap().delivery.unwrap();
    let error = peer.client.acknowledge(delivery.receipt).await.unwrap_err();
    assert_eq!(error, FeedbackError::AcknowledgementUnknown);
    assert!(!format!("{error} {error:?}").contains("private-fixture"));
    assert_eq!(peer.finish().await.len(), 2);
}

#[tokio::test]
async fn receive_server_failure_is_an_ambiguous_outcome_without_retry() {
    for status in [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::REQUEST_TIMEOUT,
    ] {
        let peer = Peer::start(vec![Reply {
            status,
            body: "{}".into(),
            location: None,
            date: "Mon, 07 Sep 2026 12:02:00 GMT",
        }])
        .await;
        assert_eq!(
            peer.client.receive(now()).await.err(),
            Some(FeedbackError::ServiceFailure)
        );
        assert_eq!(peer.finish().await.len(), 1);
    }
}

#[test]
fn feedback_requires_one_canonical_non_nil_global_epoch() {
    for epoch in [
        json!([]),
        json!([EPOCH, EPOCH]),
        json!([Uuid::nil().to_string()]),
        json!(["not-an-epoch"]),
    ] {
        let mut event = provider_event("Complaint");
        event["mail"]["tags"]["maincopy-epoch"] = epoch;
        assert!(parse_envelope(envelope(event)).is_err());
    }
    let mut event = provider_event("Complaint");
    event["mail"]["tags"]
        .as_object_mut()
        .unwrap()
        .remove("maincopy-epoch");
    assert!(parse_envelope(envelope(event)).is_err());
}

#[tokio::test]
async fn invalid_provider_clock_cannot_release_an_uncertain_receive() {
    let peer = Peer::start(vec![Reply {
        status: StatusCode::OK,
        body: "{}".into(),
        location: None,
        date: "invalid-clock",
    }])
    .await;
    assert_eq!(
        peer.client.receive(now()).await.err(),
        Some(FeedbackError::InvalidResponse)
    );
    assert_eq!(peer.finish().await.len(), 1);
}
