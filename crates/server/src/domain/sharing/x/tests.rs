use markdown_compiler::PostId;

use super::*;
use crate::domain::sharing::{http::client, test_peer::Peer};

fn credentials() -> XCredentials {
    XCredentials::parse_optional(
        "xvz1evFS4wEEPTGEFPHBog",
        "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw",
        "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb",
        "LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE",
    )
    .unwrap()
    .unwrap()
}

fn teaser() -> Teaser {
    Teaser::compose(
        PostId::parse("11111111-1111-4111-8111-111111111111").unwrap(),
        "A title",
        "A summary.",
        "https://example.com/posts/article",
    )
    .unwrap()
}

async fn deliver(replies: Vec<(&'static str, &'static str)>) -> (DeliveryOutcome, Vec<String>) {
    let peer = Peer::start(replies).await;
    let client = XClient {
        http: client(false).unwrap(),
        endpoint: Url::parse(&peer.origin).unwrap().join("2/tweets").unwrap(),
    };
    let outcome = client.deliver(&credentials(), &teaser()).await;
    (outcome, peer.requests().await)
}

#[test]
fn signature_matches_the_published_oauth_example() {
    // The worked example from X's "Creating a signature" documentation.
    let parameters = [
        ("include_entities", "true"),
        ("oauth_consumer_key", "xvz1evFS4wEEPTGEFPHBog"),
        ("oauth_nonce", "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg"),
        ("oauth_signature_method", "HMAC-SHA1"),
        ("oauth_timestamp", "1318622958"),
        (
            "oauth_token",
            "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb",
        ),
        ("oauth_version", "1.0"),
        (
            "status",
            "Hello Ladies + Gentlemen, a signed OAuth request!",
        ),
    ];
    assert_eq!(
        signature(
            "POST",
            "https://api.twitter.com/1.1/statuses/update.json",
            &parameters,
            "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw",
            "LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE",
        ),
        "hCtSmYh+iHYCEqBWrE7C7hYmtUk="
    );
}

#[test]
fn authorization_signs_only_the_oauth_parameters_of_a_json_request() {
    let header = authorization(
        "POST",
        POSTS_ENDPOINT,
        &credentials(),
        "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg",
        1_318_622_958,
    );
    assert_eq!(
        header,
        "OAuth oauth_consumer_key=\"xvz1evFS4wEEPTGEFPHBog\", \
         oauth_nonce=\"kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg\", \
         oauth_signature_method=\"HMAC-SHA1\", oauth_timestamp=\"1318622958\", \
         oauth_token=\"370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb\", \
         oauth_version=\"1.0\", oauth_signature=\"lr%2BtV%2FDKclEvXKVjG6tgaSSLV0k%3D\""
    );
}

#[tokio::test]
async fn a_created_post_is_recorded_with_its_link() {
    let (outcome, requests) = deliver(vec![(
        "201 Created",
        r#"{"data":{"id":"1234567890","text":"x"}}"#,
    )])
    .await;
    assert_eq!(
        outcome,
        DeliveryOutcome::Posted {
            url: "https://x.com/i/status/1234567890".to_owned()
        }
    );
    let request = &requests[0];
    assert!(request.starts_with("POST /2/tweets HTTP/1.1\r\n"));
    assert!(request.contains("authorization: OAuth oauth_consumer_key="));
    assert!(
        request
            .ends_with(r#"{"text":"A title\n\nA summary.\n\nhttps://example.com/posts/article"}"#)
    );
}

#[tokio::test]
async fn provider_answers_map_to_hold_retry_or_fail() {
    for (status, expected) in [
        (
            "401 Unauthorized",
            DeliveryOutcome::CredentialsRejected { draft: None },
        ),
        (
            "402 Payment Required",
            DeliveryOutcome::Retry { draft: None },
        ),
        (
            "429 Too Many Requests",
            DeliveryOutcome::Retry { draft: None },
        ),
        (
            "503 Service Unavailable",
            DeliveryOutcome::Retry { draft: None },
        ),
        (
            "403 Forbidden",
            DeliveryOutcome::Failed(DeliveryFailure::Refused),
        ),
        (
            "302 Found",
            DeliveryOutcome::Failed(DeliveryFailure::UnexpectedResponse),
        ),
    ] {
        let (outcome, _) = deliver(vec![(status, "{}")]).await;
        assert_eq!(outcome, expected, "{status}");
    }
}

#[tokio::test]
async fn success_without_a_readable_id_is_still_posted() {
    let (outcome, _) = deliver(vec![("201 Created", r#"{"data":{"id":"../evil"}}"#)]).await;
    assert_eq!(
        outcome,
        DeliveryOutcome::Posted {
            url: "https://x.com/".to_owned()
        }
    );
}

#[tokio::test]
async fn an_unreachable_provider_is_retried() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/2/tweets", listener.local_addr().unwrap());
    drop(listener);
    let client = XClient {
        http: client(false).unwrap(),
        endpoint: Url::parse(&endpoint).unwrap(),
    };
    assert_eq!(
        client.deliver(&credentials(), &teaser()).await,
        DeliveryOutcome::Retry { draft: None }
    );
}
