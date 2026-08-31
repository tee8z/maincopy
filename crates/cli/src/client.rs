use std::{path::PathBuf, time::Duration};

#[cfg(windows)]
use maincopy_shared::is_valid_windows_admin_pipe_name;
use maincopy_shared::{
    CAPABILITIES_PATH, Capabilities,
    posts::{ListPostsResponse, POSTS_PATH},
    publication::{
        CONTENT_DIGEST_HEADER, IDEMPOTENCY_KEY_HEADER, POST_REVISION_HEADER, PREVIEW_DIGEST_HEADER,
        PUBLICATIONS_PATH, PreviewDigest, PublishNowRequest, PublishNowResponse,
    },
};
use reqwest::{
    StatusCode,
    header::{ACCEPT, HeaderMap, LINK},
    redirect::Policy,
};
use thiserror::Error;
use uuid::Uuid;

const ADMIN_ORIGIN: &str = "http://maincopy.local";
const REQUEST_ID_HEADER: &str = "x-request-id";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_PREVIEW_RESPONSE_BYTES: usize = 40 * 1024 * 1024;
const MAX_ERROR_CODE_BYTES: usize = 64;
const MAX_ERROR_MESSAGE_BYTES: usize = 512;
const POST_REVISION_PREFIX: &str = "post-b3-v1-";
const CONTENT_DIGEST_PREFIX: &str = "content-b3-v1-";

/// Concrete HTTP client for Maincopy's private local admin API.
#[derive(Clone, Debug)]
pub struct AdminClient {
    http: reqwest::Client,
    socket_path: PathBuf,
    max_response_bytes: usize,
}

impl AdminClient {
    /// Creates a client that sends every request through `socket_path`.
    pub fn new(socket_path: PathBuf) -> Result<Self, AdminClientError> {
        Self::with_limits(
            socket_path,
            DEFAULT_REQUEST_TIMEOUT,
            DEFAULT_MAX_RESPONSE_BYTES,
        )
    }

    fn with_limits(
        socket_path: PathBuf,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<Self, AdminClientError> {
        if socket_path.as_os_str().is_empty() {
            return Err(AdminClientError::InvalidSocketPath);
        }
        #[cfg(windows)]
        if !socket_path
            .to_str()
            .is_some_and(is_valid_windows_admin_pipe_name)
        {
            return Err(AdminClientError::InvalidSocketPath);
        }

        let builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(timeout);
        #[cfg(unix)]
        let builder = builder.unix_socket(socket_path.clone());
        #[cfg(windows)]
        let builder = builder.windows_named_pipe(socket_path.clone());
        let http = builder.build().map_err(AdminClientError::Build)?;

        Ok(Self {
            http,
            socket_path,
            max_response_bytes,
        })
    }

    /// Fetches the versions advertised by the running server.
    pub async fn capabilities(&self) -> Result<Capabilities, AdminClientError> {
        let body = self
            .response_body(
                self.http
                    .get(format!("{ADMIN_ORIGIN}{CAPABILITIES_PATH}"))
                    .header(ACCEPT, "application/json"),
            )
            .await?;
        serde_json::from_slice(&body).map_err(AdminClientError::InvalidResponse)
    }

    /// Lists one bounded page of post revisions loaded by the running server.
    pub async fn list_posts_page(
        &self,
        cursor: Option<Uuid>,
        limit: u16,
    ) -> Result<ListPostsResponse, AdminClientError> {
        let mut url = format!("{ADMIN_ORIGIN}{POSTS_PATH}?limit={limit}");
        if let Some(cursor) = cursor {
            url.push_str("&cursor=");
            url.push_str(&cursor.hyphenated().to_string());
        }
        let body = self
            .response_body(self.http.get(url).header(ACCEPT, "application/json"))
            .await?;
        serde_json::from_slice(&body).map_err(AdminClientError::InvalidResponse)
    }

    /// Fetches one exact private preview and its server-authenticated metadata.
    pub async fn preview_post(
        &self,
        post_id: Uuid,
        revision: Option<&str>,
        content_digest: Option<&str>,
    ) -> Result<PostPreview, AdminClientError> {
        let mut url = reqwest::Url::parse(&format!("{ADMIN_ORIGIN}{POSTS_PATH}/{post_id}/preview"))
            .expect("the fixed admin origin and UUID preview path form a valid URL");
        if revision.is_some() || content_digest.is_some() {
            let mut query = url.query_pairs_mut();
            if let Some(revision) = revision {
                query.append_pair("revision", revision);
            }
            if let Some(content_digest) = content_digest {
                query.append_pair("content_digest", content_digest);
            }
        }
        let response = self
            .response_with_limit(
                self.http.get(url).header(ACCEPT, "text/html"),
                MAX_PREVIEW_RESPONSE_BYTES,
            )
            .await?;
        decode_preview_response(response)
    }

    /// Approves one exact post revision for immediate or scheduled publication.
    pub async fn approve_publication(
        &self,
        idempotency_key: Uuid,
        request: &PublishNowRequest,
    ) -> Result<PublishNowResponse, AdminClientError> {
        let body = self
            .response_body(
                self.http
                    .post(format!("{ADMIN_ORIGIN}{PUBLICATIONS_PATH}"))
                    .header(ACCEPT, "application/json")
                    .header(
                        IDEMPOTENCY_KEY_HEADER,
                        idempotency_key.hyphenated().to_string(),
                    )
                    .json(request),
            )
            .await?;
        decode_publication_response(&body, &request.preview_digest)
    }

    /// Publishes one post immediately through the canonical activation workflow.
    pub async fn publish_now(
        &self,
        idempotency_key: Uuid,
        request: &PublishNowRequest,
    ) -> Result<PublishNowResponse, AdminClientError> {
        self.approve_publication(idempotency_key, request).await
    }

    async fn response_body(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<Vec<u8>, AdminClientError> {
        Ok(self.response(request).await?.body)
    }

    async fn response(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<AdminResponse, AdminClientError> {
        self.response_with_limit(request, self.max_response_bytes)
            .await
    }

    async fn response_with_limit(
        &self,
        request: reqwest::RequestBuilder,
        max_response_bytes: usize,
    ) -> Result<AdminResponse, AdminClientError> {
        let mut response = request
            .send()
            .await
            .map_err(|source| AdminClientError::Request {
                socket_path: self.socket_path.clone(),
                source,
            })?;
        let status = response.status();
        let header_request_id = response_request_id(response.headers());
        let headers = response.headers().clone();
        let mut body = Vec::new();

        while let Some(chunk) =
            response
                .chunk()
                .await
                .map_err(|source| AdminClientError::Request {
                    socket_path: self.socket_path.clone(),
                    source,
                })?
        {
            let Some(length) = body.len().checked_add(chunk.len()) else {
                return Err(AdminClientError::ResponseTooLarge {
                    limit: max_response_bytes,
                });
            };
            if length > max_response_bytes {
                return Err(AdminClientError::ResponseTooLarge {
                    limit: max_response_bytes,
                });
            }
            body.extend_from_slice(&chunk);
        }

        if !status.is_success() {
            let (problem, body_request_id) = decode_problem(&body);
            return Err(AdminClientError::HttpStatus {
                status,
                problem,
                request_id: consistent_request_id(header_request_id, body_request_id),
            });
        }

        Ok(AdminResponse { headers, body })
    }
}

struct AdminResponse {
    headers: HeaderMap,
    body: Vec<u8>,
}

/// Exact private preview representation returned by the administration API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostPreview {
    pub html: Box<str>,
    pub preview_digest: PreviewDigest,
    pub revision: Box<str>,
    pub content_digest: Box<str>,
    pub canonical_url: Box<str>,
}

fn decode_preview_response(response: AdminResponse) -> Result<PostPreview, AdminClientError> {
    let preview_digest =
        required_header(&response.headers, PREVIEW_DIGEST_HEADER).and_then(|value| {
            PreviewDigest::parse(value)
                .map_err(|_| invalid_preview("invalid preview digest header"))
        })?;
    let revision = required_header(&response.headers, POST_REVISION_HEADER).and_then(|value| {
        typed_digest(value, POST_REVISION_PREFIX, "invalid post revision header")
    })?;
    let content_digest =
        required_header(&response.headers, CONTENT_DIGEST_HEADER).and_then(|value| {
            typed_digest(
                value,
                CONTENT_DIGEST_PREFIX,
                "invalid content digest header",
            )
        })?;
    let canonical_url = canonical_link(&response.headers)?;
    let html = std::str::from_utf8(&response.body)
        .map_err(|_| invalid_preview("preview body is not valid UTF-8"))?
        .into();
    Ok(PostPreview {
        html,
        preview_digest,
        revision,
        content_digest,
        canonical_url,
    })
}

fn decode_publication_response(
    body: &[u8],
    expected_preview: &PreviewDigest,
) -> Result<PublishNowResponse, AdminClientError> {
    let response: PublishNowResponse =
        serde_json::from_slice(body).map_err(AdminClientError::InvalidResponse)?;
    if response.preview_digest != *expected_preview {
        return Err(AdminClientError::InvalidPublicationResponse {
            message: "preview_digest does not echo the approved preview",
        });
    }
    Ok(response)
}

fn required_header<'headers>(
    headers: &'headers HeaderMap,
    name: &'static str,
) -> Result<&'headers str, AdminClientError> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .ok_or_else(|| invalid_preview("required preview metadata header is missing"))?;
    if values.next().is_some() {
        return Err(invalid_preview("preview metadata header is repeated"));
    }
    value
        .to_str()
        .map_err(|_| invalid_preview("preview metadata header is not visible ASCII"))
}

fn typed_digest(
    value: &str,
    prefix: &str,
    message: &'static str,
) -> Result<Box<str>, AdminClientError> {
    let valid = value.strip_prefix(prefix).is_some_and(|encoded| {
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if valid {
        Ok(value.into())
    } else {
        Err(invalid_preview(message))
    }
}

fn canonical_link(headers: &HeaderMap) -> Result<Box<str>, AdminClientError> {
    let value = required_header(headers, LINK.as_str())?;
    let Some(value) = value.strip_prefix('<') else {
        return Err(invalid_preview("invalid canonical Link header"));
    };
    let Some((target, parameters)) = value.split_once('>') else {
        return Err(invalid_preview("invalid canonical Link header"));
    };
    if parameters != "; rel=\"canonical\"" {
        return Err(invalid_preview("invalid canonical Link header"));
    }
    let url = reqwest::Url::parse(target)
        .map_err(|_| invalid_preview("invalid canonical Link target"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.as_str() != target
    {
        return Err(invalid_preview("invalid canonical Link target"));
    }
    Ok(target.into())
}

const fn invalid_preview(message: &'static str) -> AdminClientError {
    AdminClientError::InvalidPreviewResponse { message }
}

fn response_request_id(headers: &HeaderMap) -> Option<Uuid> {
    let mut values = headers.get_all(REQUEST_ID_HEADER).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    canonical_uuid(value)
}

fn consistent_request_id(header: Option<Uuid>, body: Option<Uuid>) -> Option<Uuid> {
    match (header, body) {
        (Some(header), Some(body)) if header == body => Some(header),
        (Some(_), Some(_)) => None,
        (Some(request_id), None) | (None, Some(request_id)) => Some(request_id),
        (None, None) => None,
    }
}

fn decode_problem(body: &[u8]) -> (Option<AdminProblem>, Option<Uuid>) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (None, None);
    };
    let Some(error) = value.get("error").and_then(serde_json::Value::as_object) else {
        return (None, None);
    };
    let request_id = error
        .get("request_id")
        .and_then(serde_json::Value::as_str)
        .and_then(canonical_uuid);
    let problem = error
        .get("code")
        .and_then(serde_json::Value::as_str)
        .zip(error.get("message").and_then(serde_json::Value::as_str))
        .filter(|(code, message)| safe_error_code(code) && safe_error_message(message))
        .map(|(code, message)| AdminProblem {
            code: code.into(),
            message: message.into(),
        });
    (problem, request_id)
}

fn canonical_uuid(value: &str) -> Option<Uuid> {
    let uuid = Uuid::parse_str(value).ok()?;
    (uuid.hyphenated().to_string() == value).then_some(uuid)
}

fn safe_error_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= MAX_ERROR_CODE_BYTES
        && code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn safe_error_message(message: &str) -> bool {
    !message.is_empty()
        && message.len() <= MAX_ERROR_MESSAGE_BYTES
        && message.bytes().all(|byte| (b' '..=b'~').contains(&byte))
}

/// Safe diagnostic details decoded from an admin error response.
#[derive(Debug)]
pub struct AdminProblem {
    pub code: Box<str>,
    pub message: Box<str>,
}

/// Failures produced while preparing or sending a local admin request.
#[derive(Debug, Error)]
pub enum AdminClientError {
    #[error("the local admin endpoint is invalid")]
    InvalidSocketPath,

    #[error("failed to construct the admin HTTP client: {0}")]
    Build(#[source] reqwest::Error),

    #[error("admin request through {socket_path:?} failed: {source}")]
    Request {
        socket_path: PathBuf,
        #[source]
        source: reqwest::Error,
    },

    #[error("the admin server returned HTTP {status}")]
    HttpStatus {
        status: StatusCode,
        problem: Option<AdminProblem>,
        request_id: Option<Uuid>,
    },

    #[error("the admin response exceeded the {limit}-byte limit")]
    ResponseTooLarge { limit: usize },

    #[error("the admin server returned an invalid response: {0}")]
    InvalidResponse(#[source] serde_json::Error),

    #[error("the admin server returned invalid preview metadata: {message}")]
    InvalidPreviewResponse { message: &'static str },

    #[error("the admin server returned an inconsistent publication response: {message}")]
    InvalidPublicationResponse { message: &'static str },
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;
    use serde_json::json;

    use super::*;

    const REQUEST_ID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    const PREVIEW_DIGEST: &str =
        "preview-b3-v1-1111111111111111111111111111111111111111111111111111111111111111";
    const REVISION: &str =
        "post-b3-v1-2222222222222222222222222222222222222222222222222222222222222222";
    const CONTENT_DIGEST: &str =
        "content-b3-v1-3333333333333333333333333333333333333333333333333333333333333333";

    fn preview_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            PREVIEW_DIGEST_HEADER,
            HeaderValue::from_static(PREVIEW_DIGEST),
        );
        headers.insert(POST_REVISION_HEADER, HeaderValue::from_static(REVISION));
        headers.insert(
            CONTENT_DIGEST_HEADER,
            HeaderValue::from_static(CONTENT_DIGEST),
        );
        headers.insert(
            LINK,
            HeaderValue::from_static("<https://example.test/posts/ready>; rel=\"canonical\""),
        );
        headers
    }

    #[test]
    fn bounded_problem_decoder_keeps_only_safe_operator_details() {
        let body = serde_json::to_vec(&json!({
            "error": {
                "code": "idempotency_conflict",
                "message": "Idempotency-Key is already bound to another command",
                "request_id": REQUEST_ID
            }
        }))
        .unwrap();

        let (problem, request_id) = decode_problem(&body);
        let problem = problem.unwrap();
        assert_eq!(&*problem.code, "idempotency_conflict");
        assert_eq!(
            &*problem.message,
            "Idempotency-Key is already bound to another command"
        );
        assert_eq!(request_id.unwrap().hyphenated().to_string(), REQUEST_ID);

        let unsafe_body = serde_json::to_vec(&json!({
            "error": {
                "code": "not safe",
                "message": "contains\na newline",
                "request_id": REQUEST_ID
            }
        }))
        .unwrap();
        assert!(decode_problem(&unsafe_body).0.is_none());
    }

    #[test]
    fn conflicting_header_and_body_request_ids_are_discarded() {
        let header = Uuid::parse_str(REQUEST_ID).unwrap();
        let body = Uuid::parse_str("dddddddd-dddd-4ddd-8ddd-dddddddddddd").unwrap();

        assert_eq!(
            consistent_request_id(Some(header), Some(header)),
            Some(header)
        );
        assert_eq!(consistent_request_id(Some(header), Some(body)), None);
    }

    #[test]
    fn preview_decoder_requires_exact_typed_metadata_and_utf8_html() {
        let preview = decode_preview_response(AdminResponse {
            headers: preview_headers(),
            body: b"<!doctype html><title>Ready</title>".to_vec(),
        })
        .unwrap();

        assert_eq!(preview.preview_digest.as_str(), PREVIEW_DIGEST);
        assert_eq!(preview.revision.as_ref(), REVISION);
        assert_eq!(preview.content_digest.as_ref(), CONTENT_DIGEST);
        assert_eq!(
            preview.canonical_url.as_ref(),
            "https://example.test/posts/ready"
        );
        assert_eq!(preview.html.as_ref(), "<!doctype html><title>Ready</title>");
    }

    #[test]
    fn preview_decoder_rejects_missing_repeated_or_malformed_metadata() {
        for (name, malformed) in [
            (PREVIEW_DIGEST_HEADER, REVISION),
            (POST_REVISION_HEADER, CONTENT_DIGEST),
            (CONTENT_DIGEST_HEADER, REVISION),
        ] {
            let mut headers = preview_headers();
            headers.insert(name, HeaderValue::from_static(malformed));
            assert!(
                decode_preview_response(AdminResponse {
                    headers,
                    body: b"<html></html>".to_vec(),
                })
                .is_err(),
                "{name}"
            );
        }

        let mut missing = preview_headers();
        missing.remove(PREVIEW_DIGEST_HEADER);
        assert!(
            decode_preview_response(AdminResponse {
                headers: missing,
                body: b"<html></html>".to_vec(),
            })
            .is_err()
        );

        let mut repeated = preview_headers();
        repeated.append(
            PREVIEW_DIGEST_HEADER,
            HeaderValue::from_static(PREVIEW_DIGEST),
        );
        assert!(
            decode_preview_response(AdminResponse {
                headers: repeated,
                body: b"<html></html>".to_vec(),
            })
            .is_err()
        );

        for link in [
            "https://example.test/posts/ready",
            "<https://example.test/posts/ready>; rel=canonical",
            "<https://user@example.test/posts/ready>; rel=\"canonical\"",
            "<https://example.test/posts/ready#fragment>; rel=\"canonical\"",
            "<https://example.test/posts/ready>; rel=\"canonical\", <https://example.test/other>; rel=\"alternate\"",
        ] {
            let mut headers = preview_headers();
            headers.insert(LINK, HeaderValue::from_str(link).unwrap());
            assert!(
                decode_preview_response(AdminResponse {
                    headers,
                    body: b"<html></html>".to_vec(),
                })
                .is_err(),
                "{link}"
            );
        }

        assert!(
            decode_preview_response(AdminResponse {
                headers: preview_headers(),
                body: vec![0xff],
            })
            .is_err()
        );
    }

    #[test]
    fn publication_decoder_requires_the_server_to_echo_the_approved_preview() {
        let expected = PreviewDigest::parse(PREVIEW_DIGEST).unwrap();
        let matching = serde_json::to_vec(&json!({
            "publication_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "post_id": "11111111-1111-4111-8111-111111111111",
            "preview_digest": PREVIEW_DIGEST,
            "revision": REVISION,
            "state": "published",
            "published_at": "2026-08-30T12:00:00Z",
            "site_digest":
                "site-b3-v1-5555555555555555555555555555555555555555555555555555555555555555",
            "site_version": 2
        }))
        .unwrap();
        assert!(decode_publication_response(&matching, &expected).is_ok());

        let mismatched = matching
            .windows(PREVIEW_DIGEST.len())
            .position(|window| window == PREVIEW_DIGEST.as_bytes())
            .map(|offset| {
                let mut mismatched = matching.clone();
                let last = offset + PREVIEW_DIGEST.len() - 1;
                mismatched[last] = b'5';
                mismatched
            })
            .unwrap();
        assert!(matches!(
            decode_publication_response(&mismatched, &expected),
            Err(AdminClientError::InvalidPublicationResponse { .. })
        ));
    }
}
