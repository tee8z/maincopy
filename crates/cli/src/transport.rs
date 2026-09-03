//! Bounded HTTPS execution for administration requests.

use std::{
    fs::{self, File},
    io::{self, Read as _},
    path::{Path, PathBuf},
    time::Duration,
};

use bytes::Bytes;
use reqwest::{Method, StatusCode, Url, header::HeaderMap};
use thiserror::Error;
use zeroize::Zeroizing;

const MAX_RESPONSE_HEADERS: usize = 64;
const MAX_RESPONSE_HEADER_BYTES: usize = 32 * 1024;
const MAX_ERROR_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_ADDITIONAL_ROOT_BYTES: usize = 64 * 1024;
const MAX_ADDITIONAL_ROOTS: usize = 16;

/// A bounded, parsed set of additive certificate authorities.
#[derive(Default)]
pub(crate) struct AdditionalRootCertificates(Vec<reqwest::Certificate>);

impl AdditionalRootCertificates {
    pub(crate) fn from_file(path: &Path) -> Result<Self, AdditionalRootCertificateError> {
        let (file, metadata) = open_additional_root_file(path)?;
        if metadata.len() > MAX_ADDITIONAL_ROOT_BYTES as u64 {
            return Err(AdditionalRootCertificateError::TooLarge {
                path: path.to_path_buf(),
            });
        }

        let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
        file.take((MAX_ADDITIONAL_ROOT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|source| AdditionalRootCertificateError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        if bytes.len() > MAX_ADDITIONAL_ROOT_BYTES {
            return Err(AdditionalRootCertificateError::TooLarge {
                path: path.to_path_buf(),
            });
        }
        if contains_unexpected_pem_section(&bytes) {
            return Err(AdditionalRootCertificateError::UnexpectedPemSection {
                path: path.to_path_buf(),
            });
        }

        let certificates = reqwest::Certificate::from_pem_bundle(&bytes).map_err(|source| {
            AdditionalRootCertificateError::InvalidBundle {
                path: path.to_path_buf(),
                source,
            }
        })?;
        if certificates.is_empty() || certificates.len() > MAX_ADDITIONAL_ROOTS {
            return Err(AdditionalRootCertificateError::InvalidCount {
                path: path.to_path_buf(),
                count: certificates.len(),
            });
        }

        let mut validation = reqwest::Client::builder()
            .tls_built_in_root_certs(false)
            .no_proxy();
        for certificate in certificates.iter().cloned() {
            validation = validation.add_root_certificate(certificate);
        }
        validation
            .build()
            .map_err(|source| AdditionalRootCertificateError::InvalidBundle {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(Self(certificates))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_additional_root_file(
    path: &Path,
) -> Result<(File, fs::Metadata), AdditionalRootCertificateError> {
    let before =
        fs::symlink_metadata(path).map_err(|source| AdditionalRootCertificateError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    if !before.file_type().is_file() {
        return Err(AdditionalRootCertificateError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }

    let file: File = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(|source| AdditionalRootCertificateError::Open {
        path: path.to_path_buf(),
        source: source.into(),
    })?
    .into();

    let after = file
        .metadata()
        .map_err(|source| AdditionalRootCertificateError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if !after.file_type().is_file() {
        return Err(AdditionalRootCertificateError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }
    validate_same_file(path, &before, &after)?;
    Ok((file, after))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn open_additional_root_file(
    path: &Path,
) -> Result<(File, fs::Metadata), AdditionalRootCertificateError> {
    Err(AdditionalRootCertificateError::UnsupportedPlatform {
        path: path.to_path_buf(),
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn validate_same_file(
    path: &Path,
    before: &fs::Metadata,
    after: &fs::Metadata,
) -> Result<(), AdditionalRootCertificateError> {
    use std::os::unix::fs::MetadataExt as _;

    if before.dev() != after.dev() || before.ino() != after.ino() {
        return Err(AdditionalRootCertificateError::ChangedDuringOpen {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn contains_unexpected_pem_section(bytes: &[u8]) -> bool {
    bytes.split(|byte| *byte == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        line.starts_with(b"-----BEGIN ") && line != b"-----BEGIN CERTIFICATE-----"
    })
}

#[derive(Debug, Error)]
pub(crate) enum AdditionalRootCertificateError {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[error(
        "additional certificate-authority files are supported only on Linux and macOS; cannot use {path:?}"
    )]
    UnsupportedPlatform { path: PathBuf },
    #[error("could not open the PEM certificate file {path:?}")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not read the PEM certificate file {path:?}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the PEM certificate path {path:?} is not a regular file")]
    NotRegularFile { path: PathBuf },
    #[error("the PEM certificate file {path:?} changed while it was being opened")]
    ChangedDuringOpen { path: PathBuf },
    #[error("the PEM certificate file {path:?} exceeds the 64 KiB safety limit")]
    TooLarge { path: PathBuf },
    #[error("the PEM certificate file {path:?} contains a non-certificate PEM section")]
    UnexpectedPemSection { path: PathBuf },
    #[error("the PEM certificate file {path:?} is not a valid certificate bundle")]
    InvalidBundle {
        path: PathBuf,
        #[source]
        source: reqwest::Error,
    },
    #[error(
        "the PEM certificate file {path:?} contains {count} certificates; it must contain between 1 and 16"
    )]
    InvalidCount { path: PathBuf, count: usize },
}

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
    pub(crate) fn new(
        additional_roots: AdditionalRootCertificates,
    ) -> Result<Self, TransportError> {
        let mut builder = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("maincopy-cli/", env!("CARGO_PKG_VERSION")));
        for certificate in additional_roots.0 {
            builder = builder.add_root_certificate(certificate);
        }
        let client = builder
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
        let response = self
            .client
            .request(request.method, request.url)
            .headers(request.headers)
            .body(request.body.into_bytes())
            .send()
            .await
            .map_err(TransportError::Request)?;
        receive_response(response, maximum_response_bytes).await
    }
}

async fn receive_response(
    mut response: reqwest::Response,
    maximum_success_bytes: usize,
) -> Result<HttpResponse, TransportError> {
    let mut bounded =
        BoundedResponse::new(response.status(), response.headers(), maximum_success_bytes)?;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(TransportError::ResponseBody)?
    {
        bounded.append(&chunk)?;
    }
    Ok(bounded.finish())
}

struct BoundedResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
    budget: BodyBudget,
}

impl BoundedResponse {
    fn new(
        status: StatusCode,
        headers: &HeaderMap,
        maximum_success_bytes: usize,
    ) -> Result<Self, TransportError> {
        let maximum_body_bytes = if status.is_success() {
            maximum_success_bytes
        } else {
            MAX_ERROR_RESPONSE_BYTES
        };
        validate_headers(headers, maximum_body_bytes)?;
        Ok(Self {
            status,
            headers: headers.clone(),
            body: Vec::new(),
            budget: BodyBudget::new(maximum_body_bytes),
        })
    }

    fn append(&mut self, chunk: &[u8]) -> Result<(), TransportError> {
        self.budget.accept(chunk.len())?;
        self.body.extend_from_slice(chunk);
        Ok(())
    }

    fn finish(self) -> HttpResponse {
        HttpResponse {
            status: self.status,
            headers: self.headers,
            body: self.body,
        }
    }
}

struct BodyBudget {
    received: usize,
    maximum: usize,
}

impl BodyBudget {
    const fn new(maximum: usize) -> Self {
        Self {
            received: 0,
            maximum,
        }
    }

    fn accept(&mut self, chunk_bytes: usize) -> Result<(), TransportError> {
        let received = self
            .received
            .checked_add(chunk_bytes)
            .ok_or(TransportError::ResponseBodyTooLarge)?;
        if received > self.maximum {
            return Err(TransportError::ResponseBodyTooLarge);
        }
        self.received = received;
        Ok(())
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
    use reqwest::header::{CONTENT_LENGTH, HeaderName, HeaderValue};
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::{TcpListener, TcpStream},
        time::timeout,
    };

    use super::*;

    fn response_headers(content_length: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(content_length) = content_length {
            headers.insert(
                CONTENT_LENGTH,
                HeaderValue::from_str(content_length).unwrap(),
            );
        }
        headers
    }

    async fn local_response(raw_response: &'static [u8]) -> reqwest::Response {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            timeout(Duration::from_secs(5), async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request_head(&mut stream).await;
                assert!(request.starts_with(b"GET / HTTP/1.1\r\n"));
                stream.write_all(raw_response).await.unwrap();
                stream.shutdown().await.unwrap();
            })
            .await
            .expect("the local HTTP fixture should complete before its deadline");
        });
        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        server.await.unwrap();
        response
    }

    async fn read_request_head(stream: &mut TcpStream) -> Vec<u8> {
        const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;
        let mut request = Vec::new();
        loop {
            assert!(request.len() < MAX_REQUEST_HEAD_BYTES);
            let mut chunk = [0_u8; 1024];
            let remaining = MAX_REQUEST_HEAD_BYTES - request.len();
            let read_limit = remaining.min(chunk.len());
            let read = stream.read(&mut chunk[..read_limit]).await.unwrap();
            assert_ne!(read, 0, "request ended before the complete HTTP head");
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                return request;
            }
        }
    }

    #[test]
    fn response_header_limits_accept_one_canonical_bounded_length() {
        for content_length in [None, Some("0"), Some("1024")] {
            assert!(validate_headers(&response_headers(content_length), 1024).is_ok());
        }
    }

    #[test]
    fn response_header_limits_reject_oversized_ambiguous_or_invalid_lengths() {
        let mut headers = response_headers(Some("1025"));
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

        for value in ["", "+1", "01x", "184467440737095516160"] {
            let headers = response_headers(Some(value));
            assert!(matches!(
                validate_headers(&headers, usize::MAX),
                Err(TransportError::InvalidContentLength)
            ));
        }
    }

    #[test]
    fn response_header_count_and_encoded_size_are_bounded() {
        let mut count = HeaderMap::new();
        for index in 0..=MAX_RESPONSE_HEADERS {
            count.insert(
                HeaderName::from_bytes(format!("x-test-{index}").as_bytes()).unwrap(),
                HeaderValue::from_static("x"),
            );
        }
        assert!(matches!(
            validate_headers(&count, 1),
            Err(TransportError::ResponseHeadersTooLarge)
        ));

        let mut encoded = HeaderMap::new();
        encoded.insert(
            HeaderName::from_static("x-large"),
            HeaderValue::from_bytes(&vec![b'x'; MAX_RESPONSE_HEADER_BYTES]).unwrap(),
        );
        assert!(matches!(
            validate_headers(&encoded, 1),
            Err(TransportError::ResponseHeadersTooLarge)
        ));
    }

    #[test]
    fn bounded_response_accumulates_chunks_and_preserves_metadata() {
        let mut headers = response_headers(Some("4"));
        headers.insert("x-result", HeaderValue::from_static("retained"));
        let mut response = BoundedResponse::new(StatusCode::OK, &headers, 4).unwrap();
        response.append(b"ab").unwrap();
        response.append(b"cd").unwrap();

        let response = response.finish();
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.headers["x-result"], "retained");
        assert_eq!(response.body, b"abcd");
    }

    #[test]
    fn bounded_response_enforces_success_error_and_overflow_limits() {
        let mut success = BoundedResponse::new(StatusCode::OK, &HeaderMap::new(), 3).unwrap();
        success.append(b"abc").unwrap();
        assert!(matches!(
            success.append(b"d"),
            Err(TransportError::ResponseBodyTooLarge)
        ));

        let mut error =
            BoundedResponse::new(StatusCode::BAD_GATEWAY, &HeaderMap::new(), usize::MAX).unwrap();
        error.append(&vec![b'x'; MAX_ERROR_RESPONSE_BYTES]).unwrap();
        assert!(matches!(
            error.append(b"x"),
            Err(TransportError::ResponseBodyTooLarge)
        ));

        let mut overflow = BodyBudget {
            received: usize::MAX,
            maximum: usize::MAX,
        };
        assert!(matches!(
            overflow.accept(1),
            Err(TransportError::ResponseBodyTooLarge)
        ));
    }

    #[tokio::test]
    async fn concrete_response_reader_preserves_exact_http_response() {
        let response =
            local_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-Result: exact\r\n\r\nhello")
                .await;
        let response = receive_response(response, 5).await.unwrap();

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.headers["x-result"], "exact");
        assert_eq!(response.body, b"hello");
    }

    #[tokio::test]
    async fn executor_rejects_plain_http_before_sending() {
        let executor = ReqwestExecutor::new(AdditionalRootCertificates::default()).unwrap();
        let request = HttpRequest {
            method: Method::GET,
            url: Url::parse("http://127.0.0.1:9/").unwrap(),
            headers: HeaderMap::new(),
            body: Vec::new().into(),
        };

        assert!(matches!(
            executor.execute(request, 1).await,
            Err(TransportError::NonHttpsRequest)
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    const VALID_ROOT: &[u8] = include_bytes!("../tests/fixtures/tls/root-ca.pem");
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    const PRIVATE_KEY: &[u8] = include_bytes!("../tests/fixtures/tls/localhost-key.pem");

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn additional_root_files_are_bounded_and_require_certificates() {
        let root = tempfile::tempdir().unwrap();
        let valid = root.path().join("valid.pem");
        std::fs::write(&valid, VALID_ROOT).unwrap();
        let roots = AdditionalRootCertificates::from_file(&valid).unwrap();
        assert_eq!(roots.0.len(), 1);
        ReqwestExecutor::new(roots).unwrap();

        let empty = root.path().join("empty.pem");
        std::fs::write(&empty, []).unwrap();
        assert!(matches!(
            AdditionalRootCertificates::from_file(&empty),
            Err(AdditionalRootCertificateError::InvalidCount { count: 0, .. })
        ));

        let oversized = root.path().join("oversized.pem");
        std::fs::write(&oversized, vec![b'x'; MAX_ADDITIONAL_ROOT_BYTES + 1]).unwrap();
        assert!(matches!(
            AdditionalRootCertificates::from_file(&oversized),
            Err(AdditionalRootCertificateError::TooLarge { .. })
        ));

        assert!(matches!(
            AdditionalRootCertificates::from_file(root.path()),
            Err(AdditionalRootCertificateError::NotRegularFile { .. })
        ));

        let mixed = root.path().join("certificate-and-key.pem");
        let mut mixed_bytes = VALID_ROOT.to_vec();
        mixed_bytes.extend_from_slice(PRIVATE_KEY);
        std::fs::write(&mixed, mixed_bytes).unwrap();
        assert!(matches!(
            AdditionalRootCertificates::from_file(&mixed),
            Err(AdditionalRootCertificateError::UnexpectedPemSection { .. })
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn additional_root_files_reject_special_files_without_opening_them() {
        let root = tempfile::tempdir().unwrap();
        let fifo = root.path().join("root-ca.fifo");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();

        assert!(matches!(
            AdditionalRootCertificates::from_file(&fifo),
            Err(AdditionalRootCertificateError::NotRegularFile { .. })
        ));

        let certificate = root.path().join("root-ca.pem");
        let symlink = root.path().join("root-ca-link.pem");
        std::fs::write(&certificate, VALID_ROOT).unwrap();
        std::os::unix::fs::symlink(certificate, &symlink).unwrap();
        assert!(matches!(
            AdditionalRootCertificates::from_file(&symlink),
            Err(AdditionalRootCertificateError::NotRegularFile { .. })
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn additional_root_files_reject_invalid_or_excessive_pem_bundles() {
        let root = tempfile::tempdir().unwrap();
        let invalid = root.path().join("invalid.pem");
        std::fs::write(&invalid, b"-----BEGIN CERTIFICATE-----\ninvalid\n").unwrap();
        assert!(matches!(
            AdditionalRootCertificates::from_file(&invalid),
            Err(AdditionalRootCertificateError::InvalidBundle { .. })
                | Err(AdditionalRootCertificateError::InvalidCount { count: 0, .. })
        ));

        let invalid_der = root.path().join("invalid-der.pem");
        std::fs::write(
            &invalid_der,
            b"-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        assert!(matches!(
            AdditionalRootCertificates::from_file(&invalid_der),
            Err(AdditionalRootCertificateError::InvalidBundle { .. })
        ));

        let excessive = root.path().join("excessive.pem");
        std::fs::write(&excessive, VALID_ROOT.repeat(MAX_ADDITIONAL_ROOTS + 1)).unwrap();
        assert!(matches!(
            AdditionalRootCertificates::from_file(&excessive),
            Err(AdditionalRootCertificateError::InvalidCount { count, .. })
                if count == MAX_ADDITIONAL_ROOTS + 1
        ));

        let missing = root.path().join("missing.pem");
        assert!(matches!(
            AdditionalRootCertificates::from_file(&missing),
            Err(AdditionalRootCertificateError::Open { .. })
        ));
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn additional_root_files_fail_closed_on_unsupported_platforms() {
        let path = Path::new("development-ca.pem");
        assert!(matches!(
            AdditionalRootCertificates::from_file(path),
            Err(AdditionalRootCertificateError::UnsupportedPlatform { path: rejected })
                if rejected == path
        ));
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod trust_boundary_tests;
