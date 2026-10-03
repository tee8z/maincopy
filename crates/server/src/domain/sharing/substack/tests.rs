use markdown_compiler::PostId;

use super::*;
use crate::domain::sharing::{
    http::client,
    settings::{ChannelMode, SubstackSubdomain, substack_session},
    test_peer::Peer,
};

const PROFILE: (&str, &str) = ("200 OK", r#"{"id":42,"name":"Author"}"#);
const DRAFT: (&str, &str) = ("200 OK", r#"{"id":7001,"draft_title":"A title"}"#);
const PUBLISHED: (&str, &str) = ("200 OK", r#"{"id":7001,"slug":"a-title"}"#);

fn teaser() -> Teaser {
    Teaser::compose(
        PostId::parse("11111111-1111-4111-8111-111111111111").unwrap(),
        "A title",
        "A summary.",
        "https://example.com/posts/article",
    )
    .unwrap()
}

async fn deliver(
    replies: Vec<(&'static str, &'static str)>,
    draft: Option<u64>,
) -> (DeliveryOutcome, Vec<String>, String) {
    let peer = Peer::start(replies).await;
    let mut client = SubstackClient::new(
        client(false).unwrap(),
        &SubstackSettings {
            mode: ChannelMode::Enabled,
            subdomain: SubstackSubdomain::parse("example").unwrap(),
            session: substack_session("s%3Asession.signature").unwrap(),
        },
    )
    .unwrap();
    assert_eq!(client.publication.as_str(), "https://example.substack.com/");
    client.account = Url::parse(&peer.origin).unwrap();
    client.publication = Url::parse(&peer.origin).unwrap();
    let outcome = client.deliver(&teaser(), draft).await;
    let origin = peer.origin.clone();
    (outcome, peer.requests().await, origin)
}

#[tokio::test]
async fn a_teaser_becomes_a_web_only_post_under_the_session_owner() {
    let (outcome, requests, origin) = deliver(vec![PROFILE, DRAFT, PUBLISHED], None).await;
    assert_eq!(
        outcome,
        DeliveryOutcome::Posted {
            url: format!("{origin}p/a-title")
        }
    );
    assert!(requests[0].starts_with("GET /api/v1/user/profile/self HTTP/1.1\r\n"));
    assert!(requests[1].starts_with("POST /api/v1/drafts HTTP/1.1\r\n"));
    assert!(requests[2].starts_with("POST /api/v1/drafts/7001/publish HTTP/1.1\r\n"));
    for request in &requests {
        assert!(request.contains("cookie: substack.sid=s%3Asession.signature\r\n"));
        assert!(request.contains("user-agent: Mozilla/5.0 (compatible; Maincopy/"));
    }
    let draft: serde_json::Value =
        serde_json::from_str(requests[1].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(draft["draft_title"], "A title");
    assert_eq!(
        draft["draft_bylines"],
        json!([{"id": 42, "is_guest": false}])
    );
    assert_eq!(draft["type"], "newsletter");
    assert_eq!(draft["section_chosen"], true);
    let body: serde_json::Value =
        serde_json::from_str(draft["draft_body"].as_str().unwrap()).unwrap();
    assert_eq!(
        body,
        json!({"type": "doc", "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "A summary."}]},
            {"type": "paragraph", "content": [{
                "type": "text",
                "text": "https://example.com/posts/article",
                "marks": [{"type": "link", "attrs": {"href": "https://example.com/posts/article"}}],
            }]},
        ]})
    );
    assert!(requests[2].ends_with(r#"{"send":false,"share_automatically":false}"#));
}

#[tokio::test]
async fn a_retry_publishes_the_existing_draft_without_creating_another() {
    let (outcome, requests, origin) = deliver(vec![PUBLISHED], Some(7001)).await;
    assert_eq!(
        outcome,
        DeliveryOutcome::Posted {
            url: format!("{origin}p/a-title")
        }
    );
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /api/v1/drafts/7001/publish HTTP/1.1\r\n"));
}

#[tokio::test]
async fn a_refused_session_holds_the_teaser_before_and_after_the_draft() {
    let (outcome, _, _) = deliver(vec![("401 Unauthorized", "{}")], None).await;
    assert_eq!(
        outcome,
        DeliveryOutcome::CredentialsRejected { draft: None }
    );
    let (outcome, _, _) = deliver(vec![PROFILE, DRAFT, ("403 Forbidden", "{}")], None).await;
    assert_eq!(
        outcome,
        DeliveryOutcome::CredentialsRejected { draft: Some(7001) }
    );
}

#[tokio::test]
async fn transient_answers_retry_and_keep_a_created_draft() {
    let (outcome, _, _) = deliver(vec![PROFILE, ("429 Too Many Requests", "slow")], None).await;
    assert_eq!(outcome, DeliveryOutcome::Retry { draft: None });
    let (outcome, _, _) = deliver(vec![PROFILE, DRAFT, ("502 Bad Gateway", "")], None).await;
    assert_eq!(outcome, DeliveryOutcome::Retry { draft: Some(7001) });
}

#[tokio::test]
async fn refusals_and_unreadable_answers_fail_closed() {
    let (outcome, _, _) = deliver(vec![PROFILE, ("400 Bad Request", "{}")], None).await;
    assert_eq!(outcome, DeliveryOutcome::Failed(DeliveryFailure::Refused));
    let (outcome, _, _) = deliver(vec![("200 OK", "<html>")], None).await;
    assert_eq!(
        outcome,
        DeliveryOutcome::Failed(DeliveryFailure::UnexpectedResponse)
    );
}

#[tokio::test]
async fn a_published_post_without_a_usable_slug_links_to_the_publication() {
    let (outcome, _, origin) =
        deliver(vec![("200 OK", r#"{"slug":"../admin"}"#)], Some(7001)).await;
    assert_eq!(outcome, DeliveryOutcome::Posted { url: origin });
}

#[test]
fn a_teaser_without_a_summary_is_only_its_link() {
    let teaser = Teaser::compose(
        PostId::parse("11111111-1111-4111-8111-111111111111").unwrap(),
        "A title",
        "",
        "https://example.com/posts/article",
    )
    .unwrap();
    let body: serde_json::Value = serde_json::from_str(&document(teaser.view())).unwrap();
    assert_eq!(body["content"].as_array().unwrap().len(), 1);
}
