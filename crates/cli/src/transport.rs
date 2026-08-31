//! Bounded HTTPS execution for administration requests.

use std::time::Duration;

use bytes::Bytes;
use reqwest::{Method, StatusCode, Url, header::HeaderMap};
use thiserror::Error;
use zeroize::Zeroizing;

const MAX_RESPONSE_HEADERS: usize = 64;
const MAX_RESPONSE_HEADER_BYTES: usize = 32 * 1024;
const MAX_ERROR_RESPONSE_BYTES: usize = 16 * 1024;

pub(crate) struct HttpRequest {
    pub(crate) method: Method,
    pub(crate) url: Url,
    pub(crate) headers: HeaderMap,
    pub(crate) body: RequestBody,
}

/// Request bytes whose backing allocation is erased when the HTTP body is released.
pub(crate) struct RequestBody(Zeroizing<Vec<u8>>);

impl RequestBody {
    fn into_bytes(self) -> Bytes {
        let Self(bytes) = self;
        Bytes::from_owner(bytes)
    }
}

impl From<Vec<u8>> for RequestBody {
    fn from(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }
}

pub(crate) struct HttpResponse {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Vec<u8>,
}

pub(crate) struct ReqwestExecutor {
    client: reqwest::Client,
}

impl ReqwestExecutor {
    pub(crate) fn new() -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("maincopy-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(TransportError::ClientConfiguration)?;
        Ok(Self { client })
    }

    pub(crate) async fn execute(
        &self,
        request: HttpRequest,
        maximum_response_bytes: usize,
    ) -> Result<HttpResponse, TransportError> {
        if request.url.scheme() != "https" {
            return Err(TransportError::NonHttpsRequest);
        }
        let mut response = self
            .client
            .request(request.method, request.url)
            .headers(request.headers)
            .body(request.body.into_bytes())
            .send()
            .await
            .map_err(TransportError::Request)?;
        let status = response.status();
        let maximum_response_bytes = if status.is_success() {
            maximum_response_bytes
        } else {
            MAX_ERROR_RESPONSE_BYTES
        };
        validate_headers(response.headers(), maximum_response_bytes)?;
        let headers = response.headers().clone();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(TransportError::ResponseBody)?
        {
            let new_length = body
                .len()
                .checked_add(chunk.len())
                .ok_or(TransportError::ResponseBodyTooLarge)?;
            if new_length > maximum_response_bytes {
                return Err(TransportError::ResponseBodyTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

fn validate_headers(
    headers: &HeaderMap,
    maximum_response_bytes: usize,
) -> Result<(), TransportError> {
    if headers.len() > MAX_RESPONSE_HEADERS {
        return Err(TransportError::ResponseHeadersTooLarge);
    }
    let encoded_bytes = headers.iter().try_fold(0_usize, |total, (name, value)| {
        total
            .checked_add(name.as_str().len())
            .and_then(|total| total.checked_add(value.as_bytes().len()))
    });
    if encoded_bytes.is_none_or(|bytes| bytes > MAX_RESPONSE_HEADER_BYTES) {
        return Err(TransportError::ResponseHeadersTooLarge);
    }

    let mut lengths = headers.get_all(reqwest::header::CONTENT_LENGTH).iter();
    if let Some(length) = lengths.next() {
        if lengths.next().is_some() {
            return Err(TransportError::InvalidContentLength);
        }
        let length = length
            .to_str()
            .ok()
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or(TransportError::InvalidContentLength)?;
        if length > maximum_response_bytes {
            return Err(TransportError::ResponseBodyTooLarge);
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum TransportError {
    #[error("the HTTPS client could not be configured: {0}")]
    ClientConfiguration(#[source] reqwest::Error),
    #[error("an administration request was not HTTPS")]
    NonHttpsRequest,
    #[error("the administration HTTPS request failed: {0}")]
    Request(#[source] reqwest::Error),
    #[error("the administration HTTPS response body could not be read: {0}")]
    ResponseBody(#[source] reqwest::Error),
    #[error("the administration HTTPS response headers exceeded the safety limit")]
    ResponseHeadersTooLarge,
    #[error("the administration HTTPS response Content-Length is invalid")]
    InvalidContentLength,
    #[error("the administration HTTPS response body exceeded the safety limit")]
    ResponseBodyTooLarge,
}

#[cfg(test)]
mod tests {
    use reqwest::header::{CONTENT_LENGTH, HeaderValue};

    use super::*;

    #[test]
    fn response_limits_reject_oversized_or_ambiguous_lengths() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("1025"));
        assert!(matches!(
            validate_headers(&headers, 1024),
            Err(TransportError::ResponseBodyTooLarge)
        ));

        headers.clear();
        headers.append(CONTENT_LENGTH, HeaderValue::from_static("10"));
        headers.append(CONTENT_LENGTH, HeaderValue::from_static("10"));
        assert!(matches!(
            validate_headers(&headers, 1024),
            Err(TransportError::InvalidContentLength)
        ));
    }
}
