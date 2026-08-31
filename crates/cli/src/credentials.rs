//! Protected local credential storage.

use std::fmt;

use sha2::{Digest as _, Sha256};
use thiserror::Error;
use zeroize::Zeroizing;

const KEYRING_SERVICE: &str = "maincopy-cli-v1";

/// One credential value whose diagnostics never expose its contents.
pub(crate) struct SecretValue(Zeroizing<String>);

impl SecretValue {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    pub(crate) fn expose_secret(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue(<redacted>)")
    }
}

/// A non-secret, origin-bound identifier for one protected credential entry.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct CredentialKey(Box<str>);

impl CredentialKey {
    pub(crate) fn human(admin_origin: &str) -> Self {
        Self::for_kind("human", admin_origin)
    }

    pub(crate) fn agent(admin_origin: &str) -> Self {
        Self::for_kind("agent", admin_origin)
    }

    fn for_kind(kind: &str, admin_origin: &str) -> Self {
        let digest = Sha256::digest(admin_origin.as_bytes());
        let mut account = String::with_capacity(kind.len() + 1 + digest.len() * 2);
        account.push_str(kind);
        account.push(':');
        encode_lower_hex_into(&digest, &mut account);
        Self(account.into_boxed_str())
    }
}

/// Production storage backed by Keychain, Credential Manager, or Secret Service.
pub(crate) struct PlatformCredentialStore;

impl PlatformCredentialStore {
    fn entry(key: &CredentialKey) -> Result<keyring::Entry, CredentialStoreError> {
        keyring::Entry::new(KEYRING_SERVICE, &key.0).map_err(CredentialStoreError::EntryCreation)
    }

    pub(crate) fn load(
        &self,
        key: &CredentialKey,
    ) -> Result<Option<SecretValue>, CredentialStoreError> {
        match Self::entry(key)?.get_password() {
            Ok(value) => Ok(Some(SecretValue::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(CredentialStoreError::Load(error)),
        }
    }

    pub(crate) fn save(
        &self,
        key: &CredentialKey,
        value: &SecretValue,
    ) -> Result<(), CredentialStoreError> {
        Self::entry(key)?
            .set_password(value.expose_secret())
            .map_err(CredentialStoreError::Save)
    }

    pub(crate) fn delete(&self, key: &CredentialKey) -> Result<(), CredentialStoreError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(CredentialStoreError::Delete(error)),
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum CredentialStoreError {
    #[error("the operating system credential entry could not be created")]
    EntryCreation(#[source] keyring::Error),
    #[error("the protected credential could not be loaded")]
    Load(#[source] keyring::Error),
    #[error("the protected credential could not be saved")]
    Save(#[source] keyring::Error),
    #[error("the protected credential could not be deleted")]
    Delete(#[source] keyring::Error),
}

fn encode_lower_hex_into(bytes: &[u8], output: &mut String) {
    const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        output.push(char::from(LOWER_HEX[usize::from(byte >> 4)]));
        output.push(char::from(LOWER_HEX[usize::from(byte & 0x0f)]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_keys_are_origin_and_kind_bound_without_embedding_the_origin() {
        let one = CredentialKey::human("https://admin.example.test");
        let two = CredentialKey::human("https://other.example.test");
        let agent = CredentialKey::agent("https://admin.example.test");
        assert_ne!(one, two);
        assert_ne!(one, agent);
        assert!(!one.0.contains("example.test"));
    }

    #[test]
    fn secret_diagnostics_are_redacted() {
        let secret = SecretValue::new("do-not-print");
        assert_eq!(format!("{secret:?}"), "SecretValue(<redacted>)");
        assert!(!format!("{secret:?}").contains(secret.expose_secret()));
    }
}
