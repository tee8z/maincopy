use super::transport::ProtectedText;
use crate::config::secret::{
    ProtectedSecretFileError, open_protected_secret_file, with_resolved_secret,
};
use aws_credential_types::Credentials;
use serde::Deserialize;
use std::{fs::File, io::Read as _, path::Path};
use thiserror::Error;

const MAX_CREDENTIAL_BYTES: usize = 16 * 1024;

/// Maincopy retains one protected credential value. Request-scoped AWS signing
/// objects and HTTP/TLS allocations have their own lifetimes; this boundary does
/// not promise to wipe every allocation made by those libraries.
pub(in crate::domain::mail) struct SesCredentials {
    access_key_id: ProtectedText,
    secret_access_key: ProtectedText,
    session_token: Option<ProtectedText>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(in crate::domain::mail) enum CredentialError {
    #[error("the SES credential file must be a private regular file owned by this service")]
    Protection,
    #[error("the SES credential file could not be read")]
    Read,
    #[error("the SES credential document is malformed or exceeds its size limit")]
    Invalid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialDocument {
    access_key_id: ProtectedText,
    secret_access_key: ProtectedText,
    session_token: Option<ProtectedText>,
}

impl SesCredentials {
    /// Blocking startup operation. Callers that reload during service operation
    /// must perform it outside the Tokio reactor.
    pub(in crate::domain::mail) fn load_protected_file(
        path: &Path,
    ) -> Result<Self, CredentialError> {
        let file = open_credentials(path)?;
        let mut bounded = file.take((MAX_CREDENTIAL_BYTES + 1) as u64);
        with_resolved_secret(&mut bounded, Self::parse).map_err(|_| CredentialError::Read)?
    }

    pub(in crate::domain::mail) fn parse(bytes: &[u8]) -> Result<Self, CredentialError> {
        if bytes.len() > MAX_CREDENTIAL_BYTES {
            return Err(CredentialError::Invalid);
        }
        let document: CredentialDocument =
            serde_json::from_slice(bytes).map_err(|_| CredentialError::Invalid)?;
        validate_document(&document)?;
        Ok(Self {
            access_key_id: document.access_key_id,
            secret_access_key: document.secret_access_key,
            session_token: document.session_token,
        })
    }

    /// Bind approvals to the loaded identity without exposing any credential
    /// string. The digest belongs to provider configuration, never subscriber
    /// controls. Upstream hashing scratch storage has its own library lifetime.
    pub(in crate::domain::mail) fn bind_configuration(&self, hasher: &mut blake3::Hasher) {
        for value in [self.access_key_id.as_str(), self.secret_access_key.as_str()] {
            hasher.update(&(value.len() as u64).to_le_bytes());
            hasher.update(value.as_bytes());
        }
        match &self.session_token {
            Some(token) => {
                hasher.update(&[1]);
                hasher.update(&(token.as_str().len() as u64).to_le_bytes());
                hasher.update(token.as_str().as_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
    }

    pub(super) fn signing_credentials(&self) -> Credentials {
        Credentials::new(
            self.access_key_id.as_str(),
            self.secret_access_key.as_str(),
            self.session_token
                .as_ref()
                .map(|token| token.as_str().to_owned()),
            None,
            "maincopy-protected-runtime-file",
        )
    }
}

fn validate_document(document: &CredentialDocument) -> Result<(), CredentialError> {
    let access = document.access_key_id.as_str();
    let secret = document.secret_access_key.as_str();
    if !(8..=128).contains(&access.len())
        || !access
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        || !(16..=128).contains(&secret.len())
        || !printable(secret)
    {
        return Err(CredentialError::Invalid);
    }
    if let Some(token) = &document.session_token
        && (token.as_str().is_empty() || token.as_str().len() > 8192 || !printable(token.as_str()))
    {
        return Err(CredentialError::Invalid);
    }
    Ok(())
}

fn printable(value: &str) -> bool {
    value.bytes().all(|byte| (33..=126).contains(&byte))
}

fn open_credentials(path: &Path) -> Result<File, CredentialError> {
    open_protected_secret_file(path, MAX_CREDENTIAL_BYTES as u64).map_err(|error| match error {
        ProtectedSecretFileError::Protection => CredentialError::Protection,
        ProtectedSecretFileError::Read => CredentialError::Read,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const DOCUMENT: &[u8] = br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"}"#;

    #[test]
    fn credential_document_rejects_extra_fields_and_header_bytes() {
        assert!(SesCredentials::parse(DOCUMENT).is_ok());
        assert!(
            SesCredentials::parse(
                br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"private\nheader-data"}"#
            )
            .is_err()
        );
        assert!(SesCredentials::parse(br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"1234567890123456","endpoint":"http://localhost"}"#).is_err());
        assert!(SesCredentials::parse(&vec![b' '; MAX_CREDENTIAL_BYTES + 1]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn file_loading_rejects_world_access_symlinks_and_named_pipes() {
        use rustix::fs::{CWD, Mode, mkfifoat};
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credential");
        std::fs::write(&path, DOCUMENT).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(SesCredentials::load_protected_file(&path).is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            SesCredentials::load_protected_file(&path),
            Err(CredentialError::Protection)
        ));
        let link = directory.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(SesCredentials::load_protected_file(&link).is_err());
        let pipe = directory.path().join("pipe");
        mkfifoat(CWD, &pipe, Mode::RUSR | Mode::WUSR).unwrap();
        assert!(matches!(
            SesCredentials::load_protected_file(&pipe),
            Err(CredentialError::Protection)
        ));
    }
}
