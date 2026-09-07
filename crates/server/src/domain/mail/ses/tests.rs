use super::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};
use uuid::Uuid;

const CREDENTIAL: &[u8] = br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"}"#;
const ACCEPTED: &str = r#"{"MessageId":"010001-1234-abcd","FutureMetadata":{"enabled":true}}"#;

struct Peer {
    client: SesClient,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Vec<Vec<u8>>>,
    requests: Arc<AtomicUsize>,
}

impl Peer {
    async fn start(status: &str, body: &str, extra_headers: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let extra_headers = extra_headers.replace("{peer}", &address.to_string());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
            body.len()
        );
        let (stop, mut stopped) = oneshot::channel();
        let requests = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            let mut captured = Vec::new();
            loop {
                let connection = tokio::select! {
                    biased;
                    _ = &mut stopped => break,
                    connection = listener.accept() => connection,
                };
                let (mut stream, _) = connection.unwrap();
                captured.push(read_request(&mut stream).await);
                count.fetch_add(1, Ordering::SeqCst);
                if let Err(error) = stream.write_all(response.as_bytes()).await {
                    assert!(matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ));
                }
            }
            captured
        });
        let mut client = fixture_client();
        client.endpoint = Url::parse(&format!("http://{address}/")).unwrap();
        client.http = http_client(false).unwrap();
        Self {
            client,
            stop,
            task,
            requests,
        }
    }

    async fn finish(self) -> Vec<Vec<u8>> {
        self.stop.send(()).unwrap();
        self.task.await.unwrap()
    }
}

fn fixture_client() -> SesClient {
    SesClient::new(
        SesRegion::parse("us-east-1").unwrap(),
        SesCredentials::parse(CREDENTIAL).unwrap().into(),
        SesConfiguration {
            sender: EmailAddress::parse("sender@example.com").unwrap(),
            configuration_set: ResourceName::parse("maincopy-newsletter").unwrap(),
        },
    )
    .unwrap()
}

fn message(recipient: &EmailAddress) -> EmailMessage<'_> {
    EmailMessage {
        recipient,
        subject: "Published article",
        text: "A public article.",
        html: "<p>A public article.</p>",
        campaign_id: Some(Uuid::from_u128(1)),
        mail_epoch: Uuid::from_u128(3),
        attempt_id: Uuid::from_u128(2),
        one_click: None,
    }
}

async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let end = loop {
        let mut chunk = [0; 4096];
        let read = stream.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "peer closed before request headers");
        bytes.extend_from_slice(&chunk[..read]);
        assert!(bytes.len() <= 300 * 1024);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..end]).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    while bytes.len() < end + length {
        let mut chunk = [0; 4096];
        let read = stream.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "peer closed before request body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    bytes
}

#[tokio::test]
async fn sends_one_signed_recipient_with_control_headers_and_never_creates_contacts() {
    let peer = Peer::start("200 OK", ACCEPTED, "").await;
    let recipient = EmailAddress::parse("Case+tag@example.com").unwrap();
    let control = OneClickUrl::parse("https://example.com/email/unsubscribe/opaque").unwrap();
    let mut email = message(&recipient);
    email.one_click = Some(&control);
    let outcome = peer.client.send(&email).await.unwrap();
    assert!(matches!(outcome, SendOutcome::Accepted(id) if id.as_str() == "010001-1234-abcd"));
    let requests = peer.finish().await;
    assert_eq!(requests.len(), 1);
    let request = String::from_utf8(requests.into_iter().next().unwrap()).unwrap();
    let (headers, body) = request.split_once("\r\n\r\n").unwrap();
    assert!(headers.starts_with("POST /v2/email/outbound-emails HTTP/1.1"));
    assert!(headers.contains("/us-east-1/ses/aws4_request"));
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(
        json["Destination"]["ToAddresses"],
        serde_json::json!(["Case+tag@example.com"])
    );
    assert!(json.get("ListManagementOptions").is_none());
    assert_eq!(
        json["EmailTags"][2],
        serde_json::json!({"Name":"maincopy-epoch","Value":Uuid::from_u128(3).to_string()})
    );
    assert!(json["Destination"].get("CcAddresses").is_none());
    assert!(json["Destination"].get("BccAddresses").is_none());
    assert_eq!(
        json["EmailTags"][0]["Value"],
        Uuid::from_u128(1).to_string()
    );
    assert_eq!(
        json["EmailTags"][1]["Value"],
        Uuid::from_u128(2).to_string()
    );
    assert_eq!(
        json["Content"]["Simple"]["Headers"][1]["Value"],
        "List-Unsubscribe=One-Click"
    );
}

#[tokio::test]
async fn confirmation_send_omits_unsubscribe_headers_and_retains_both_body_formats() {
    let peer = Peer::start("200 OK", ACCEPTED, "").await;
    let recipient = EmailAddress::parse("reader@example.com").unwrap();
    let mut confirmation = message(&recipient);
    confirmation.campaign_id = None;
    peer.client.send(&confirmation).await.unwrap();
    let requests = peer.finish().await;
    let request = std::str::from_utf8(&requests[0]).unwrap();
    let body = request.split_once("\r\n\r\n").unwrap().1;
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    assert!(json["Content"]["Simple"].get("Headers").is_none());
    assert_eq!(
        json["EmailTags"],
        serde_json::json!([{
            "Name": "maincopy-attempt", "Value": Uuid::from_u128(2).to_string()
        }, {
            "Name": "maincopy-epoch", "Value": Uuid::from_u128(3).to_string()
        }])
    );
    assert_eq!(
        json["Content"]["Simple"]["Body"]["Text"]["Data"],
        "A public article."
    );
    assert_eq!(
        json["Content"]["Simple"]["Body"]["Html"]["Data"],
        "<p>A public article.</p>"
    );
}

#[tokio::test]
async fn throttling_is_reported_without_retransmitting_the_message() {
    let peer = Peer::start(
        "429 Too Many Requests",
        r#"{"message":"reader@example.com"}"#,
        "x-amzn-errortype: TooManyRequestsException\r\n",
    )
    .await;
    let recipient = EmailAddress::parse("reader@example.com").unwrap();
    assert_eq!(
        peer.client.send(&message(&recipient)).await.unwrap(),
        SendOutcome::Retryable(ServiceRejection::Throttled)
    );
    assert_eq!(peer.requests.load(Ordering::SeqCst), 1);
    peer.finish().await;
}

#[tokio::test]
async fn provider_failure_and_malformed_success_preserve_unknown_outcomes() {
    for (status, body) in [
        (
            "503 Service Unavailable",
            r#"{"message":"reader@example.com"}"#,
        ),
        ("200 OK", r#"{"MessageId":"reader\u0040example.com"}"#),
        ("200 OK", "not-json"),
        ("200 OK", r#"{}"#),
    ] {
        let peer = Peer::start(status, body, "").await;
        let recipient = EmailAddress::parse("reader@example.com").unwrap();
        assert_eq!(
            peer.client.send(&message(&recipient)).await.unwrap(),
            SendOutcome::Unknown
        );
        assert_eq!(peer.requests.load(Ordering::SeqCst), 1);
        peer.finish().await;
    }
}

#[tokio::test]
async fn redirects_cannot_move_credentials_or_recipient_data_to_another_endpoint() {
    let peer = Peer::start(
        "307 Temporary Redirect",
        "",
        "Location: http://{peer}/collect\r\n",
    )
    .await;
    let recipient = EmailAddress::parse("reader@example.com").unwrap();
    assert_eq!(
        peer.client.send(&message(&recipient)).await.unwrap(),
        SendOutcome::Unknown
    );
    assert_eq!(peer.requests.load(Ordering::SeqCst), 1);
    peer.finish().await;
}

#[tokio::test]
async fn oversized_success_receipt_is_unknown_and_not_retained() {
    let body = "x".repeat(512 * 1024 + 1);
    let peer = Peer::start("200 OK", &body, "").await;
    let recipient = EmailAddress::parse("reader@example.com").unwrap();
    assert_eq!(
        peer.client.send(&message(&recipient)).await.unwrap(),
        SendOutcome::Unknown
    );
    peer.finish().await;
}

#[tokio::test]
async fn invalid_message_is_rejected_before_opening_a_connection() {
    let peer = Peer::start("200 OK", ACCEPTED, "").await;
    let recipient = EmailAddress::parse("reader@example.com").unwrap();
    let mut email = message(&recipient);
    email.subject = "Subject\r\nBcc: other@example.com";
    assert_eq!(
        peer.client.send(&email).await.unwrap_err(),
        SesError::Input(InputError::Message)
    );
    assert_eq!(peer.requests.load(Ordering::SeqCst), 0);
    peer.finish().await;
}

#[tokio::test]
async fn total_request_deadline_bounds_an_accepted_request_with_a_stalled_reply() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = fixture_client();
    client.endpoint = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    client.http = http_client(false).unwrap();
    let (release, released) = oneshot::channel();
    let (accepted, request_received) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
            .await
            .unwrap();
        accepted.send(()).unwrap();
        released.await.unwrap();
    });
    let sending = tokio::spawn(async move {
        let recipient = EmailAddress::parse("reader@example.com").unwrap();
        client.send(&message(&recipient)).await
    });
    request_received.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(REQUEST_LIMIT + Duration::from_secs(1)).await;
    assert_eq!(sending.await.unwrap().unwrap(), SendOutcome::Unknown);
    release.send(()).unwrap();
    server.await.unwrap();
}

#[test]
fn fixed_endpoint_rejects_partition_or_host_injection_and_resource_path_separators() {
    for region in [
        "us-east-1.attacker.example",
        "us-east-1/other",
        "cn-north-1",
        "us-gov-west-1",
        "",
        "us--1",
    ] {
        assert!(SesRegion::parse(region).is_err());
    }
    assert!(SesRegion::parse("eu-west-1").is_ok());
    assert!(ResourceName::parse("list/other").is_err());
    assert!(ResourceName::parse(&"x".repeat(65)).is_err());
    assert_eq!(
        fixture_client().endpoint.as_str(),
        "https://email.us-east-1.amazonaws.com/"
    );
}
