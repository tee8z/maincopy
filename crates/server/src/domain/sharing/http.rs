//! One bounded HTTPS transport for every channel. Providers are fixed hosts,
//! so redirects, proxies, and automatic retries are refused.

use std::time::Duration;

use reqwest::{Client, Request, StatusCode};

const CONNECT_LIMIT: Duration = Duration::from_secs(5);
const REQUEST_LIMIT: Duration = Duration::from_secs(20);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Substack's edge refuses clients that do not present a browser-style agent.
const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; Maincopy/",
    env!("CARGO_PKG_VERSION"),
    ")"
);

pub(super) fn client(https_only: bool) -> Result<Client, reqwest::Error> {
    Client::builder()
        .https_only(https_only)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(CONNECT_LIMIT)
        .timeout(REQUEST_LIMIT)
        .referer(false)
        .user_agent(USER_AGENT)
        .build()
}

pub(super) enum Exchange {
    Replied {
        status: StatusCode,
        body: Vec<u8>,
    },
    /// No connection was made, so nothing reached the provider.
    Unreachable,
    /// The request may have arrived; its effect is unknown.
    Unknown,
}

pub(super) async fn exchange(http: &Client, request: Request) -> Exchange {
    let mut response = match http.execute(request).await {
        Ok(response) => response,
        Err(error) if error.is_connect() => return Exchange::Unreachable,
        Err(_) => return Exchange::Unknown,
    };
    let status = response.status();
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= MAX_RESPONSE_BYTES => {
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Exchange::Replied { status, body },
            Ok(Some(_)) | Err(_) => return Exchange::Unknown,
        }
    }
}
