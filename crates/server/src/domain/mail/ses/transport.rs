use super::{ServiceRejection, SesClient, SesError};
use aws_sigv4::{
    http_request::{SignableBody, SignableRequest, SigningSettings, sign},
    sign::v4,
};
use bytes::Bytes;
use reqwest::{
    Method, Request, StatusCode,
    header::{CONTENT_TYPE, HeaderName, HeaderValue},
};
use serde::{Deserialize, Deserializer, Serialize, de::Visitor};
use std::{fmt, io, time::SystemTime};
use tracing::subscriber::{NoSubscriber, with_default};
use zeroize::Zeroizing;

pub(super) const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_RESPONSE_BYTES: usize = 512 * 1024;

/// Response text may contain escaped PII. Even the owned-string Serde path must
/// enter protected storage before validation. Serde scratch allocations remain
/// a library boundary rather than a promise of complete memory sanitization.
pub(in crate::domain::mail) struct ProtectedText(Zeroizing<String>);

impl ProtectedText {
    pub(in crate::domain::mail) fn from_owned(value: Zeroizing<String>) -> Self {
        Self(value)
    }

    pub(in crate::domain::mail) fn new(value: &str) -> Self {
        Self(Zeroizing::new(value.to_owned()))
    }
    pub(in crate::domain::mail) fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ProtectedText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ProtectedVisitor;
        impl Visitor<'_> for ProtectedVisitor {
            type Value = ProtectedText;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a protected string")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(ProtectedText::new(value))
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(ProtectedText(Zeroizing::new(value)))
            }
        }
        deserializer.deserialize_string(ProtectedVisitor)
    }
}

/// Fixed allocation: serialized PII cannot be left in a Vec's previous capacity
/// after reallocation. Bytes keeps this owner alive until HTTP releases its body.
pub(in crate::domain::mail) struct ProtectedBytes {
    storage: Zeroizing<Box<[u8]>>,
    len: usize,
}

impl ProtectedBytes {
    fn new(limit: usize) -> Self {
        Self {
            storage: Zeroizing::new(vec![0; limit].into_boxed_slice()),
            len: 0,
        }
    }

    pub(in crate::domain::mail) fn json(value: &impl Serialize) -> Result<Self, SesError> {
        let mut bytes = Self::new(MAX_REQUEST_BYTES);
        serde_json::to_writer(&mut bytes, value).map_err(|_| SesError::Preparation)?;
        Ok(bytes)
    }

    fn extend(&mut self, bytes: &[u8]) -> Result<(), SesError> {
        let end = self
            .len
            .checked_add(bytes.len())
            .ok_or(SesError::InvalidResponse)?;
        if end > self.storage.len() {
            return Err(SesError::InvalidResponse);
        }
        self.storage[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }
}

impl AsRef<[u8]> for ProtectedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.storage[..self.len]
    }
}

impl io::Write for ProtectedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.extend(bytes)
            .map_err(|_| io::Error::other("protected JSON body exceeds its bound"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) struct Reply {
    pub(super) status: StatusCode,
    pub(super) body: ProtectedBytes,
    rejection: Option<ServiceRejection>,
}

impl SesClient {
    pub(super) async fn request(&self, body: ProtectedBytes) -> Result<Reply, SesError> {
        let mut url = self.endpoint.clone();
        url.set_path("/v2/email/outbound-emails");
        let mut request = Request::new(Method::POST, url);
        request
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        sign_request(
            &mut request,
            body.as_ref(),
            &self.credentials,
            &self.region.0,
            SigningService::Ses,
            SystemTime::now(),
        )?;
        *request.body_mut() = Some(Bytes::from_owner(body).into());
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|_| SesError::Unknown)?;
        let status = response.status();
        let header_rejection = response
            .headers()
            .get("x-amzn-errortype")
            .and_then(|value| value.to_str().ok())
            .and_then(rejection_code);
        let body = read_response(response)
            .await
            .map_err(|_| SesError::Unknown)?;
        let rejection = header_rejection.or_else(|| rejection_body(body.as_ref()));
        if status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT {
            return Err(SesError::Unknown);
        }
        Ok(Reply {
            status,
            body,
            rejection,
        })
    }
}

#[derive(Clone, Copy)]
pub(in crate::domain::mail) enum SigningService {
    Ses,
    Sqs,
}

pub(in crate::domain::mail) fn sign_request(
    request: &mut Request,
    body: &[u8],
    credentials: &super::SesCredentials,
    region: &str,
    service: SigningService,
    now: SystemTime,
) -> Result<(), SesError> {
    let (service, content_type) = match service {
        SigningService::Ses => ("ses", "application/json"),
        SigningService::Sqs => ("sqs", "application/x-amz-json-1.0"),
    };
    request
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    let mut headers = vec![("content-type", content_type)];
    if service == "sqs" {
        let target = request
            .headers()
            .get("x-amz-target")
            .and_then(|value| value.to_str().ok())
            .ok_or(SesError::Preparation)?;
        headers.push(("x-amz-target", target));
    }
    let identity = credentials.signing_credentials().into();
    let parameters = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(service)
        .time(now)
        .settings(SigningSettings::default())
        .build()
        .map_err(|_| SesError::Preparation)?
        .into();
    let signable = SignableRequest::new(
        request.method().as_str(),
        request.url().as_str(),
        headers.into_iter(),
        SignableBody::Bytes(body),
    )
    .map_err(|_| SesError::Preparation)?;
    // The upstream signer can emit raw request bodies under TRACE when
    // LOG_SIGNABLE_BODY=true. Suppress its synchronous tracing scope regardless
    // of operator environment. This guard never crosses an await boundary.
    let (instructions, _) = with_default(NoSubscriber::default(), || sign(signable, &parameters))
        .map_err(|_| SesError::Preparation)?
        .into_parts();
    if !instructions.params().is_empty() {
        return Err(SesError::Preparation);
    }
    for (name, value) in instructions.headers() {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| SesError::Preparation)?;
        let mut value = HeaderValue::from_str(value).map_err(|_| SesError::Preparation)?;
        value.set_sensitive(true);
        request.headers_mut().insert(name, value);
    }
    Ok(())
}

pub(in crate::domain::mail) async fn read_response(
    mut response: reqwest::Response,
) -> Result<ProtectedBytes, SesError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(SesError::InvalidResponse);
    }
    let mut body = ProtectedBytes::new(MAX_RESPONSE_BYTES);
    while let Some(chunk) = response.chunk().await.map_err(|_| SesError::Unknown)? {
        body.extend(&chunk)?;
    }
    Ok(body)
}

impl Reply {
    pub(super) fn require_ok(self) -> Result<ProtectedBytes, SesError> {
        if self.status == StatusCode::OK {
            return Ok(self.body);
        }
        if self.status.is_client_error() {
            return Err(SesError::Rejected(
                self.rejection
                    .unwrap_or_else(|| rejection_status(self.status)),
            ));
        }
        Err(SesError::Unknown)
    }
}

fn rejection_body(bytes: &[u8]) -> Option<ServiceRejection> {
    #[derive(Deserialize)]
    struct Problem {
        #[serde(rename = "__type", alias = "code", alias = "Code")]
        code: ProtectedText,
    }
    let problem: Problem = serde_json::from_slice(bytes).ok()?;
    rejection_code(problem.code.as_str())
}

fn rejection_code(code: &str) -> Option<ServiceRejection> {
    let code = code.rsplit('#').next()?.split(':').next()?;
    match code {
        "AccessDeniedException" | "InvalidSignatureException" | "UnrecognizedClientException" => {
            Some(ServiceRejection::AccessDenied)
        }
        "BadRequestException" => Some(ServiceRejection::InvalidRequest),
        "NotFoundException" => Some(ServiceRejection::NotFound),
        "AlreadyExistsException" => Some(ServiceRejection::AlreadyExists),
        "AccountSuspendedException" | "SendingPausedException" => {
            Some(ServiceRejection::SendingDisabled)
        }
        "TooManyRequestsException" | "ThrottlingException" => Some(ServiceRejection::Throttled),
        "LimitExceededException" => Some(ServiceRejection::LimitExceeded),
        "MessageRejected" | "MailFromDomainNotVerifiedException" => {
            Some(ServiceRejection::MessageRejected)
        }
        _ => None,
    }
}

fn rejection_status(status: StatusCode) -> ServiceRejection {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ServiceRejection::AccessDenied,
        StatusCode::NOT_FOUND => ServiceRejection::NotFound,
        StatusCode::TOO_MANY_REQUESTS => ServiceRejection::Throttled,
        _ => ServiceRejection::InvalidRequest,
    }
}

#[cfg(test)]
mod tests {
    use super::super::SesCredentials;
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::{Duration, UNIX_EPOCH};
    use tracing::{Event, Subscriber};
    use tracing_subscriber::{
        Layer,
        layer::{Context, SubscriberExt as _},
    };

    const CREDENTIAL: &[u8] = br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"}"#;

    #[test]
    fn official_signer_matches_the_aws_get_vanilla_reference_fixture() {
        // aws-sigv4 1.5.1, aws-signing-test-suite/v4/get-vanilla:
        // context.json and header-signed-request.txt (AWS's published fixture).
        let credentials = SesCredentials::parse(CREDENTIAL).unwrap();
        let identity = credentials.signing_credentials().into();
        let parameters = v4::SigningParams::builder()
            .identity(&identity)
            .region("us-east-1")
            .name("service")
            .time(UNIX_EPOCH + Duration::from_secs(1_440_938_160))
            .settings(SigningSettings::default())
            .build()
            .unwrap()
            .into();
        let request = SignableRequest::new(
            "GET",
            "https://example.amazonaws.com/",
            std::iter::empty(),
            SignableBody::Bytes(&[]),
        )
        .unwrap();
        let (instructions, signature) = sign(request, &parameters).unwrap().into_parts();
        assert_eq!(
            signature,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        assert!(
            instructions
                .headers()
                .any(|(name, value)| name == "x-amz-date" && value == "20150830T123600Z")
        );
    }

    #[test]
    fn signing_binds_body_region_and_ses_service_without_disclosing_credentials() {
        let credentials = SesCredentials::parse(CREDENTIAL).unwrap();
        let mut request = Request::new(
            Method::POST,
            "https://email.us-east-1.amazonaws.com/v2/email/outbound-emails"
                .parse()
                .unwrap(),
        );
        let time = UNIX_EPOCH + Duration::from_secs(1_440_938_160);
        sign_request(
            &mut request,
            b"first",
            &credentials,
            "us-east-1",
            SigningService::Ses,
            time,
        )
        .unwrap();
        let authorization = request.headers()["authorization"].clone();
        assert!(authorization.is_sensitive());
        assert!(
            authorization
                .to_str()
                .unwrap()
                .contains("/us-east-1/ses/aws4_request")
        );
        sign_request(
            &mut request,
            b"second",
            &credentials,
            "us-east-1",
            SigningService::Ses,
            time,
        )
        .unwrap();
        assert_ne!(authorization, request.headers()["authorization"]);
        assert!(!format!("{:?}", request.headers()).contains("AKIDEXAMPLE"));
    }

    struct EventCounter(Arc<AtomicUsize>);

    impl<S: Subscriber> Layer<S> for EventCounter {
        fn on_event(&self, _event: &Event<'_>, _context: Context<'_, S>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn signing_cannot_emit_library_diagnostics_into_the_application_subscriber() {
        let count = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry().with(EventCounter(Arc::clone(&count)));
        with_default(subscriber, || {
            tracing::trace!("before signing");
            let credentials = SesCredentials::parse(CREDENTIAL).unwrap();
            let mut request = Request::new(
                Method::POST,
                "https://email.us-east-1.amazonaws.com/v2/email/outbound-emails"
                    .parse()
                    .unwrap(),
            );
            sign_request(
                &mut request,
                b"protected@example.com",
                &credentials,
                "us-east-1",
                SigningService::Ses,
                UNIX_EPOCH + Duration::from_secs(1_440_938_160),
            )
            .unwrap();
            tracing::trace!("after signing");
        });
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn explicit_signable_body_logging_cannot_bypass_the_scoped_privacy_boundary() {
        // Set the upstream debug switch only in a child test process. Mutating
        // the parent environment would race other tests and live runtime work.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact",
                "domain::mail::ses::transport::tests::signing_cannot_emit_library_diagnostics_into_the_application_subscriber",
                "--nocapture"])
            .env("LOG_SIGNABLE_BODY", "true")
            .output().unwrap();
        assert!(
            output.status.success(),
            "isolated signer privacy assertion failed"
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains("1 passed"),
            "isolated privacy test was not selected"
        );
        assert!(!stdout.contains("protected@example.com"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("protected@example.com"));
    }

    #[test]
    fn bounded_body_is_never_reallocated_and_http_owns_it_until_last_release() {
        use std::io::Write as _;
        let mut owner = ProtectedBytes::new(8);
        owner.write_all(b"private").unwrap();
        assert!(owner.write_all(b"XX").is_err());
        assert_eq!(owner.as_ref(), b"private");
        let bytes = Bytes::from_owner(owner);
        let retained = bytes.clone();
        drop(bytes);
        assert_eq!(retained.as_ref(), b"private");
    }

    #[test]
    fn service_errors_are_classified_without_copying_provider_diagnostics() {
        assert_eq!(rejection_body(br#"{"__type":"com.amazonaws.ses#MessageRejected","message":"private@example.com"}"#),
            Some(ServiceRejection::MessageRejected));
        assert_eq!(
            rejection_body(br#"{"code":"private\u0040example.com"}"#),
            None
        );
        assert_eq!(
            rejection_status(StatusCode::FORBIDDEN),
            ServiceRejection::AccessDenied
        );
        for (code, expected) in [
            ("AccessDeniedException", ServiceRejection::AccessDenied),
            ("BadRequestException", ServiceRejection::InvalidRequest),
            ("NotFoundException", ServiceRejection::NotFound),
            ("AlreadyExistsException", ServiceRejection::AlreadyExists),
            (
                "AccountSuspendedException",
                ServiceRejection::SendingDisabled,
            ),
            ("TooManyRequestsException", ServiceRejection::Throttled),
            ("LimitExceededException", ServiceRejection::LimitExceeded),
        ] {
            assert_eq!(rejection_code(code), Some(expected));
        }
    }
}
