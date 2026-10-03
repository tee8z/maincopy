//! Post a teaser through the X API as the account that owns the saved access
//! token. Each request is signed with OAuth 1.0a; the four values never expire.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, KeyInit as _, Mac as _};
use reqwest::{
    Client, Method, Request, StatusCode, Url,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue},
};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    http::{Exchange, exchange},
    settings::XCredentials,
    store::{DeliveryFailure, DeliveryOutcome},
    teaser::Teaser,
};

const POSTS_ENDPOINT: &str = "https://api.x.com/2/tweets";
const MAX_POST_ID_BYTES: usize = 32;
const UNEXPECTED: DeliveryOutcome = DeliveryOutcome::Failed(DeliveryFailure::UnexpectedResponse);

pub(super) struct XClient {
    http: Client,
    endpoint: Url,
}

#[derive(Serialize)]
struct NewPost<'a> {
    text: &'a str,
}

#[derive(Deserialize)]
struct Created {
    data: CreatedPost,
}

#[derive(Deserialize)]
struct CreatedPost {
    id: String,
}

impl XClient {
    pub(super) fn new(http: Client) -> Result<Self, url::ParseError> {
        Ok(Self {
            http,
            endpoint: Url::parse(POSTS_ENDPOINT)?,
        })
    }

    pub(super) async fn deliver(
        &self,
        credentials: &XCredentials,
        teaser: &Teaser,
    ) -> DeliveryOutcome {
        let text = teaser.text();
        let Ok(body) = serde_json::to_vec(&NewPost { text: &text }) else {
            return UNEXPECTED;
        };
        let authorization = authorization(
            Method::POST.as_str(),
            self.endpoint.as_str(),
            credentials,
            &Uuid::new_v4().simple().to_string(),
            OffsetDateTime::now_utc().unix_timestamp(),
        );
        let Ok(mut authorization) = HeaderValue::from_str(&authorization) else {
            return UNEXPECTED;
        };
        authorization.set_sensitive(true);
        let mut request = Request::new(Method::POST, self.endpoint.clone());
        let headers = request.headers_mut();
        headers.insert(AUTHORIZATION, authorization);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        *request.body_mut() = Some(body.into());
        let (status, body) = match exchange(&self.http, request).await {
            Exchange::Replied { status, body } => (status, body),
            Exchange::Unreachable => return DeliveryOutcome::Retry { draft: None },
            // The post may exist. Sending it again could publish a duplicate.
            Exchange::Unknown => return DeliveryOutcome::Failed(DeliveryFailure::Interrupted),
        };
        match status {
            StatusCode::CREATED | StatusCode::OK => posted(&body),
            StatusCode::UNAUTHORIZED => DeliveryOutcome::CredentialsRejected { draft: None },
            // 402 is an account without API credit; it recovers once topped up.
            StatusCode::PAYMENT_REQUIRED
            | StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS => DeliveryOutcome::Retry { draft: None },
            status if status.is_server_error() => DeliveryOutcome::Retry { draft: None },
            // 403 covers duplicate text and apps without write permission.
            status if status.is_client_error() => DeliveryOutcome::Failed(DeliveryFailure::Refused),
            _ => UNEXPECTED,
        }
    }
}

/// The post exists once X answers success; an unreadable id only loses the deep link.
fn posted(body: &[u8]) -> DeliveryOutcome {
    let id = serde_json::from_slice::<Created>(body)
        .ok()
        .map(|created| created.data.id)
        .filter(|id| {
            !id.is_empty()
                && id.len() <= MAX_POST_ID_BYTES
                && id.bytes().all(|byte| byte.is_ascii_digit())
        });
    DeliveryOutcome::Posted {
        url: match id {
            Some(id) => format!("https://x.com/i/status/{id}"),
            None => "https://x.com/".to_owned(),
        },
    }
}

/// RFC 3986 unreserved characters pass through; every other byte is escaped.
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// The OAuth 1.0a `Authorization` value for a request with a JSON body, which
/// takes no part in the signature.
fn authorization(
    method: &str,
    url: &str,
    credentials: &XCredentials,
    nonce: &str,
    timestamp: i64,
) -> String {
    let timestamp = timestamp.to_string();
    // Already in the sorted order the signature base requires.
    let mut parameters = vec![
        ("oauth_consumer_key", credentials.api_key.expose()),
        ("oauth_nonce", nonce),
        ("oauth_signature_method", "HMAC-SHA1"),
        ("oauth_timestamp", timestamp.as_str()),
        ("oauth_token", credentials.access_token.expose()),
        ("oauth_version", "1.0"),
    ];
    let signature = signature(
        method,
        url,
        &parameters,
        credentials.api_secret.expose(),
        credentials.access_token_secret.expose(),
    );
    parameters.push(("oauth_signature", &signature));
    let fields: Vec<_> = parameters
        .iter()
        .map(|(name, value)| format!("{name}=\"{}\"", percent_encode(value)))
        .collect();
    format!("OAuth {}", fields.join(", "))
}

/// `parameters` must already be sorted by name.
fn signature(
    method: &str,
    url: &str,
    parameters: &[(&str, &str)],
    consumer_secret: &str,
    token_secret: &str,
) -> String {
    let parameters: Vec<_> = parameters
        .iter()
        .map(|(name, value)| format!("{}={}", percent_encode(name), percent_encode(value)))
        .collect();
    let base = format!(
        "{method}&{}&{}",
        percent_encode(url),
        percent_encode(&parameters.join("&"))
    );
    let key = format!(
        "{}&{}",
        percent_encode(consumer_secret),
        percent_encode(token_secret)
    );
    let mut mac =
        Hmac::<Sha1>::new_from_slice(key.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(base.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests;
