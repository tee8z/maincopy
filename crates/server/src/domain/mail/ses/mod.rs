//! A bounded SES v2 adapter. Consent, admission and recovery belong to the mail
//! domain. Sending deliberately never enables SES ListManagementOptions, which
//! could recreate a contact after the domain removed it.

mod credentials;
mod send;
mod transport;

pub(super) use credentials::{CredentialError, SesCredentials};
pub(crate) use send::MessageId;
pub(super) use send::{EmailMessage, MAX_BODY_BYTES, OneClickUrl, SendOutcome};
pub(super) use transport::{
    ProtectedBytes, ProtectedText, SigningService, read_response, sign_request,
};

use super::identity::EmailAddress;
use reqwest::Client;
use std::{sync::Arc, time::Duration};
use thiserror::Error;
use url::Url;

const REQUEST_LIMIT: Duration = Duration::from_secs(20);
const CONNECT_LIMIT: Duration = Duration::from_secs(5);

pub(super) struct SesClient {
    http: Client,
    endpoint: Url,
    region: SesRegion,
    credentials: Arc<SesCredentials>,
    configuration: SesConfiguration,
}

pub(super) struct SesConfiguration {
    pub(super) sender: EmailAddress,
    pub(super) configuration_set: ResourceName,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ResourceName(String);

impl ResourceName {
    pub(super) fn parse(value: &str) -> Result<Self, InputError> {
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            return Err(InputError::ResourceName);
        }
        Ok(Self(value.to_owned()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug)]
pub(super) struct SesRegion(String);

impl SesRegion {
    /// Commercial AWS region names only; the endpoint suffix is never caller supplied.
    pub(super) fn parse(value: &str) -> Result<Self, InputError> {
        if value.len() > 32 {
            return Err(InputError::Region);
        }
        let parts: Vec<_> = value.split('-').collect();
        if parts.len() != 3
            || parts[0].len() != 2
            || parts[0] == "cn"
            || parts[1].is_empty()
            || parts[2].is_empty()
            || !parts[..2]
                .iter()
                .all(|part| part.bytes().all(|byte| byte.is_ascii_lowercase()))
            || !parts[2].bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(InputError::Region);
        }
        Ok(Self(value.to_owned()))
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum InputError {
    #[error("the SES region is invalid or outside the supported AWS partition")]
    Region,
    #[error("the SES resource name is invalid")]
    ResourceName,
    #[error("the email content exceeds the supported limits or contains invalid headers")]
    Message,
    #[error("the one-click URL must be a bounded ASCII HTTPS URL without credentials or fragment")]
    OneClickUrl,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum ServiceRejection {
    #[error("SES authentication or authorization failed")]
    AccessDenied,
    #[error("SES rejected the request")]
    InvalidRequest,
    #[error("the SES resource was not found")]
    NotFound,
    #[error("the SES resource already exists")]
    AlreadyExists,
    #[error("SES sending is paused or suspended")]
    SendingDisabled,
    #[error("SES has throttled the request")]
    Throttled,
    #[error("an SES service limit prevents the request")]
    LimitExceeded,
    #[error("SES rejected the email content or sender identity")]
    MessageRejected,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum SesError {
    #[error(transparent)]
    Input(#[from] InputError),
    #[error("the protected SES HTTP client could not be configured")]
    ClientConfiguration,
    #[error("the SES request could not be prepared or signed")]
    Preparation,
    #[error(transparent)]
    Rejected(ServiceRejection),
    #[error("SES returned an invalid or oversized response")]
    InvalidResponse,
    #[error("the SES write outcome is unknown; do not retry automatically")]
    Unknown,
}

impl SesClient {
    pub(super) fn new(
        region: SesRegion,
        credentials: Arc<SesCredentials>,
        configuration: SesConfiguration,
    ) -> Result<Self, SesError> {
        let endpoint = Url::parse(&format!("https://email.{}.amazonaws.com/", region.0))
            .map_err(|_| SesError::ClientConfiguration)?;
        let http = http_client(true)?;
        Ok(Self {
            http,
            endpoint,
            region,
            credentials,
            configuration,
        })
    }
}

fn http_client(https_only: bool) -> Result<Client, SesError> {
    Client::builder()
        .https_only(https_only)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(CONNECT_LIMIT)
        .timeout(REQUEST_LIMIT)
        .referer(false)
        .build()
        .map_err(|_| SesError::ClientConfiguration)
}

#[cfg(test)]
mod tests;

#[cfg(all(test, unix))]
mod dispatch_tests;
