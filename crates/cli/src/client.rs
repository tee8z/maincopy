use std::{path::PathBuf, time::Duration};

#[cfg(windows)]
use maincopy_shared::is_valid_windows_admin_pipe_name;
use maincopy_shared::{
    CAPABILITIES_PATH, Capabilities,
    publication::{
        IDEMPOTENCY_KEY_HEADER, PUBLICATIONS_PATH, PublishNowRequest, PublishNowResponse,
    },
};
use reqwest::{
    StatusCode,
    header::{ACCEPT, HeaderMap},
    redirect::Policy,
};
use thiserror::Error;
use uuid::Uuid;

const ADMIN_ORIGIN: &str = "http://maincopy.local";
const REQUEST_ID_HEADER: &str = "x-request-id";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_ERROR_CODE_BYTES: usize = 64;
const MAX_ERROR_MESSAGE_BYTES: usize = 512;

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

    /// Publishes one post through the server's canonical activation workflow.
    pub async fn publish_now(
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
        serde_json::from_slice(&body).map_err(AdminClientError::InvalidResponse)
    }

    async fn response_body(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<Vec<u8>, AdminClientError> {
        let mut response = request
            .send()
            .await
            .map_err(|source| AdminClientError::Request {
                socket_path: self.socket_path.clone(),
                source,
            })?;
        let status = response.status();
        let header_request_id = response_request_id(response.headers());
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
                    limit: self.max_response_bytes,
                });
            };
            if length > self.max_response_bytes {
                return Err(AdminClientError::ResponseTooLarge {
                    limit: self.max_response_bytes,
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

        Ok(body)
    }
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
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const REQUEST_ID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

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
}
