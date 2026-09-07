//! Compose mail from protected host configuration and the shared database.
//! Construction never contacts the provider; supervised workers own network I/O.

use std::sync::Arc;

use axum::Router;
use markdown_compiler::PublicationBaseUrl;
use thiserror::Error;
use tokio::task::spawn_blocking;

use super::{
    config::{MailConfiguration, SesMailConfiguration, SubscriptionMode, SubscriptionPolicy},
    controls::{ControlKeyError, MailControls},
    dispatch::{DispatchResources, MailDispatcher},
    feedback::{FeedbackClient, FeedbackWorker},
    identity::EmailAddress,
    message::{MessagePreparationError, validate_origin},
    public::{self as public_mail, PublicMailState},
    ses::{CredentialError, ResourceName, SesClient, SesConfiguration, SesCredentials, SesRegion},
    subscriber::{
        SubscriberMode, SubscriberPolicy,
        store::{SubscriberLoadError, SubscriberMutationError},
    },
    ui::{MailReviewBinding, MailUiAccess},
};

use crate::{database::DatabaseStore, domain::auth::store::AuthLoadError};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum MailReviewAccessError {
    #[error("the SES credential file must be a private regular file owned by this service")]
    CredentialProtection,
    #[error("the SES credential file could not be read")]
    CredentialRead,
    #[error("the SES credential document is malformed or exceeds its size limit")]
    CredentialInvalid,
    #[error("the mail review credential loader did not complete")]
    LoaderFailed,
}

impl From<CredentialError> for MailReviewAccessError {
    fn from(error: CredentialError) -> Self {
        match error {
            CredentialError::Protection => Self::CredentialProtection,
            CredentialError::Read => Self::CredentialRead,
            CredentialError::Invalid => Self::CredentialInvalid,
        }
    }
}

/// Load the actual protected identity off the reactor, then retain only its
/// configuration binding. No client or worker starts, and no control key is
/// loaded: these settings alone cannot establish subscriber or send readiness.
pub(crate) async fn prepare_review_access(
    configuration: &MailConfiguration,
) -> Result<MailUiAccess, MailReviewAccessError> {
    let configuration = match configuration {
        MailConfiguration::Disabled => return Ok(MailUiAccess::Unavailable),
        MailConfiguration::Ses(configuration) => configuration.as_ref().clone(),
    };
    spawn_blocking(move || {
        let credentials =
            SesCredentials::load_protected_file(configuration.view().credential_file.path())?;
        Ok(MailUiAccess::ReviewOnly(
            MailReviewBinding::from_configuration(configuration, &credentials),
        ))
    })
    .await
    .map_err(|_| MailReviewAccessError::LoaderFailed)?
}

pub(crate) struct PreparedMail {
    pub access: MailUiAccess,
    pub public_routes: Option<Router>,
    pub dispatcher: Option<MailDispatcher>,
    pub feedback: Option<FeedbackWorker>,
}

#[derive(Debug, Error)]
pub(crate) enum MailStartupError {
    #[error(transparent)]
    Review(#[from] MailReviewAccessError),
    #[error(transparent)]
    Controls(#[from] ControlKeyError),
    #[error("subscriber startup transition failed")]
    SubscriberMutation(#[from] SubscriberMutationError),
    #[error("subscriber startup state could not be read")]
    SubscriberLoad(#[from] SubscriberLoadError),
    #[error("mail startup identity could not be read")]
    IdentityLoad(#[from] AuthLoadError),
    #[error("subscriber controls require the initialized application identity")]
    IdentityRequired,
    #[error(
        "retained subscriber addresses require subscriptions.mode = paused and their existing control key"
    )]
    RemovalRequired,
    #[error("the publication origin cannot support bounded email controls")]
    Origin(#[from] MessagePreparationError),
    #[error("the mail HTTP clients could not be configured")]
    ClientConfiguration,
}

pub(crate) async fn prepare_mail(
    configuration: &MailConfiguration,
    database: &DatabaseStore,
    origin: PublicationBaseUrl,
) -> Result<PreparedMail, MailStartupError> {
    database.subscribers.pause().await?;
    database.subscribers.quarantine_interrupted().await?;
    let subscription = match configuration {
        MailConfiguration::Disabled => None,
        MailConfiguration::Ses(configuration) => configuration.view().subscriptions.cloned(),
    };
    let Some(policy) = subscription else {
        if database.subscribers.status().await?.addressed_enrollments != 0 {
            return Err(MailStartupError::RemovalRequired);
        }
        return Ok(PreparedMail {
            access: prepare_review_access(configuration).await?,
            public_routes: None,
            dispatcher: None,
            feedback: None,
        });
    };
    let MailConfiguration::Ses(configuration) = configuration else {
        return Err(MailStartupError::ClientConfiguration);
    };
    validate_origin(&origin)?;
    let instance = database
        .auth
        .identity_state()
        .await?
        .instance
        .ok_or(MailStartupError::IdentityRequired)?;
    let configuration = configuration.as_ref().clone();
    let loaded_configuration = configuration.clone();
    // Bind approvals and both provider workers to one loaded credential snapshot.
    let (credentials, controls) = spawn_blocking(move || {
        let view = loaded_configuration.view();
        let credentials = SesCredentials::load_protected_file(view.credential_file.path())
            .map_err(MailReviewAccessError::from)?;
        let controls =
            MailControls::load(view.control_signing_key_file.path(), instance.instance_id)?;
        Ok::<_, MailStartupError>((Arc::new(credentials), Arc::new(controls)))
    })
    .await
    .map_err(|_| MailReviewAccessError::LoaderFailed)??;
    database
        .subscribers
        .initialize_controls(controls.identity_binding(&origin))
        .await?;
    let binding = configuration.provider_binding(&credentials);
    let view = configuration.view();
    database
        .subscribers
        .set_policy(SubscriberPolicy {
            configuration_binding: binding,
            mode: match policy.view().mode {
                SubscriptionMode::Paused => SubscriberMode::Paused,
                SubscriptionMode::Enabled => SubscriberMode::Enabled,
            },
            max_daily_messages: view.max_daily_messages,
            max_daily_confirmations: view.max_daily_confirmation_messages,
            max_campaign_recipients: view.max_campaign_recipients,
        })
        .await?;
    compose(
        configuration,
        policy,
        credentials,
        controls,
        database,
        origin,
    )
}

fn compose(
    configuration: SesMailConfiguration,
    policy: SubscriptionPolicy,
    credentials: Arc<SesCredentials>,
    controls: Arc<MailControls>,
    database: &DatabaseStore,
    origin: PublicationBaseUrl,
) -> Result<PreparedMail, MailStartupError> {
    let binding = configuration.provider_binding(&credentials);
    let access = MailUiAccess::DispatchReady(MailReviewBinding::from_configuration(
        configuration.clone(),
        &credentials,
    ));
    let public_routes = public_mail::router(PublicMailState::new(
        configuration.clone(),
        &credentials,
        policy.clone(),
        origin.clone(),
        controls.clone(),
        database.subscribers.clone(),
    ));
    let view = configuration.view();
    let client = SesClient::new(
        SesRegion::parse(view.region).map_err(|_| MailStartupError::ClientConfiguration)?,
        credentials.clone(),
        SesConfiguration {
            sender: EmailAddress::parse(view.sender.as_str())
                .map_err(|_| MailStartupError::ClientConfiguration)?,
            configuration_set: ResourceName::parse(view.configuration_set)
                .map_err(|_| MailStartupError::ClientConfiguration)?,
        },
    )
    .map_err(|_| MailStartupError::ClientConfiguration)?;
    let feedback = match view.feedback {
        Some(configuration) => Some(FeedbackWorker::new(
            FeedbackClient::new(configuration.clone(), credentials)
                .map_err(|_| MailStartupError::ClientConfiguration)?,
            controls.clone(),
            database.subscribers.clone(),
            binding,
        )),
        None => None,
    };
    let dispatcher = MailDispatcher::new(DispatchResources {
        campaigns: database.mail.clone(),
        subscribers: database.subscribers.clone(),
        client,
        controls,
        origin,
        configuration_binding: binding,
        configuration,
        policy,
    });
    Ok(PreparedMail {
        access,
        public_routes: Some(public_routes),
        dispatcher: Some(dispatcher),
        feedback,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{fs::OpenOptions, io::Write as _, os::unix::fs::OpenOptionsExt as _};

    use super::*;
    #[cfg(unix)]
    use crate::domain::mail::config::MailConfigurationCandidate;

    #[tokio::test]
    async fn disabled_mail_has_no_review_access() {
        assert!(matches!(
            prepare_review_access(&MailConfiguration::Disabled)
                .await
                .unwrap(),
            MailUiAccess::Unavailable
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn protected_credentials_allow_review_but_do_not_establish_dispatch_readiness() {
        let root = tempfile::tempdir().unwrap();
        let candidate: MailConfigurationCandidate = toml::from_str(
            r#"
mode = "ses"
sender = "newsletter@example.com"
region = "us-east-1"
configuration_set = "newsletter"
credential_file = "ses.json"
control_signing_key_file = "absent-control.key"
"#,
        )
        .unwrap();
        let configuration = candidate.validate(root.path()).unwrap();
        let credential_path = root.path().join("ses.json");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&credential_path)
            .unwrap();
        file.write_all(
            br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"1234567890123456"}"#,
        )
        .unwrap();
        drop(file);
        assert!(matches!(
            prepare_review_access(&configuration).await.unwrap(),
            MailUiAccess::ReviewOnly(_)
        ));
        assert!(!root.path().join("absent-control.key").exists());
        std::fs::remove_file(credential_path).unwrap();
        assert!(matches!(
            prepare_review_access(&configuration).await,
            Err(MailReviewAccessError::CredentialRead)
        ));
    }
}
