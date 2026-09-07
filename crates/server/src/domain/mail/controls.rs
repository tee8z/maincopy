//! Stable subscriber controls. Provider credentials, releases and restore
//! instance versions never change the site binding of an existing removal link.

use std::{io::Read as _, path::Path};

use maincopy_shared::auth::InstanceId;
use markdown_compiler::PublicationBaseUrl;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    control::{
        ControlClaims, ControlPurpose, ControlSigningKey, ControlTokenError, EncodedControlToken,
    },
    identity::EmailAddress,
};
use crate::config::secret::{ProtectedSecretFileError, open_protected_secret_file};

const MAX_KEY_FILE_BYTES: usize = 65;

pub(super) struct MailControls {
    signing: ControlSigningKey,
    site_binding: [u8; 32],
    mailbox_key: Zeroizing<[u8; 32]>,
    identity: [u8; 32],
}

impl MailControls {
    /// Blocking protected-file work belongs in startup's spawn_blocking task.
    /// The file contains 64 lowercase hexadecimal digits and an optional LF.
    pub(super) fn load(path: &Path, instance: InstanceId) -> Result<Self, ControlKeyError> {
        let mut file = open_protected_secret_file(path, MAX_KEY_FILE_BYTES as u64)?;
        let mut encoded = Zeroizing::new([0_u8; MAX_KEY_FILE_BYTES + 1]);
        let mut length = 0;
        while length < encoded.len() {
            match file.read(&mut encoded[length..]) {
                Ok(0) => break,
                Ok(read) => length += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(ControlKeyError::Read),
            }
        }
        let bytes = encoded[..length]
            .strip_suffix(b"\n")
            .unwrap_or(&encoded[..length]);
        let key = decode_key(bytes)?;
        Ok(Self::from_key_bytes(key, instance))
    }

    fn from_key_bytes(key: Zeroizing<[u8; 32]>, instance: InstanceId) -> Self {
        let site_binding = blake3::derive_key(
            "maincopy mail control site v1",
            instance.as_uuid().as_bytes(),
        );
        let mailbox_key = derive(&key, b"maincopy mailbox lookup v1\0", &site_binding);
        let identity = *derive(&key, b"maincopy control identity v1\0", &site_binding);
        Self {
            signing: ControlSigningKey::from_bytes(key),
            site_binding,
            mailbox_key,
            identity,
        }
    }

    pub(super) fn issue(
        &self,
        claims: ControlClaims,
        now: OffsetDateTime,
    ) -> Result<EncodedControlToken, ControlTokenError> {
        self.signing.issue(&self.site_binding, claims, now)
    }

    pub(super) fn verify(
        &self,
        purpose: ControlPurpose,
        encoded: &str,
        now: OffsetDateTime,
    ) -> Result<ControlClaims, ControlTokenError> {
        self.signing
            .verify(&self.site_binding, purpose, encoded, now)
    }

    /// Treat case variants as one enrollment while preserving the supplied
    /// delivery spelling. Only the lookup identity uses this wiped buffer.
    pub(super) fn mailbox_digest(&self, address: &EmailAddress) -> [u8; 32] {
        let mut folded = Zeroizing::new([0_u8; 254]);
        let bytes = address.as_str().as_bytes();
        for (output, input) in folded.iter_mut().zip(bytes) {
            *output = input.to_ascii_lowercase();
        }
        *blake3::keyed_hash(&self.mailbox_key, &folded[..bytes.len()]).as_bytes()
    }

    pub(super) fn confirmation_digest(nonce: &Uuid) -> [u8; 32] {
        blake3::derive_key("maincopy confirmation nonce v1", nonce.as_bytes())
    }

    /// Persist this binding before admitting controls. A different key must not
    /// silently orphan existing removal links or bypass mailbox suppression.
    pub(super) fn identity_binding(&self, origin: &PublicationBaseUrl) -> [u8; 32] {
        // Existing messages contain absolute URLs. Moving the origin must not
        // strand removal links, while mailbox lookup stays independent of it.
        let mut hasher = blake3::Hasher::new_derive_key("maincopy mail removal identity v1");
        hasher.update(&self.identity);
        hasher.update(origin.as_str().as_bytes());
        *hasher.finalize().as_bytes()
    }
}

fn derive(key: &[u8; 32], context: &[u8], site: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    let mut hasher = blake3::Hasher::new_keyed(key);
    hasher.update(context);
    hasher.update(site);
    Zeroizing::new(*hasher.finalize().as_bytes())
}

fn decode_key(encoded: &[u8]) -> Result<Zeroizing<[u8; 32]>, ControlKeyError> {
    if encoded.len() != 64 {
        return Err(ControlKeyError::Encoding);
    }
    let mut key = Zeroizing::new([0; 32]);
    for (output, pair) in key.iter_mut().zip(encoded.as_chunks::<2>().0) {
        *output = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
    }
    if key.iter().all(|byte| *byte == 0) {
        return Err(ControlKeyError::Encoding);
    }
    Ok(key)
}

fn hex_digit(byte: u8) -> Result<u8, ControlKeyError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(ControlKeyError::Encoding),
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ControlKeyError {
    #[error("the mail control key must be a private regular file owned by this service")]
    Protection,
    #[error("the protected mail control key could not be read")]
    Read,
    #[error("the mail control key must contain a nonzero 256-bit key as lowercase hexadecimal")]
    Encoding,
}

impl From<ProtectedSecretFileError> for ControlKeyError {
    fn from(error: ProtectedSecretFileError) -> Self {
        match error {
            ProtectedSecretFileError::Protection => Self::Protection,
            ProtectedSecretFileError::Read => Self::Read,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_survive_reconstruction_but_cannot_cross_site_or_key_boundaries() {
        let instance = InstanceId::from_uuid(Uuid::new_v4());
        let original = MailControls::from_key_bytes(Zeroizing::new([7; 32]), instance);
        let reloaded = MailControls::from_key_bytes(Zeroizing::new([7; 32]), instance);
        let other_site = MailControls::from_key_bytes(
            Zeroizing::new([7; 32]),
            InstanceId::from_uuid(Uuid::new_v4()),
        );
        let rotated = MailControls::from_key_bytes(Zeroizing::new([8; 32]), instance);
        let now = OffsetDateTime::now_utc();
        let token = original
            .issue(
                ControlClaims::Manage {
                    enrollment: Uuid::new_v4(),
                    generation: Uuid::new_v4(),
                },
                now,
            )
            .unwrap();
        assert!(
            reloaded
                .verify(
                    ControlPurpose::Manage,
                    token.as_str(),
                    now + time::Duration::days(365)
                )
                .is_ok()
        );
        assert_eq!(
            original.identity_binding(&PublicationBaseUrl::parse("https://example.com/").unwrap()),
            reloaded.identity_binding(&PublicationBaseUrl::parse("https://example.com/").unwrap())
        );
        for incompatible in [other_site, rotated] {
            assert!(
                incompatible
                    .verify(ControlPurpose::Manage, token.as_str(), now)
                    .is_err()
            );
            assert_ne!(
                original
                    .identity_binding(&PublicationBaseUrl::parse("https://example.com/").unwrap()),
                incompatible
                    .identity_binding(&PublicationBaseUrl::parse("https://example.com/").unwrap())
            );
        }
        let a = EmailAddress::parse("Case@EXAMPLE.com").unwrap();
        let b = EmailAddress::parse("case@example.COM").unwrap();
        assert_eq!(original.mailbox_digest(&a), original.mailbox_digest(&b));
        assert_eq!(a.as_str(), "Case@example.com");
        assert_ne!(
            original.identity_binding(&PublicationBaseUrl::parse("https://example.com/").unwrap()),
            original
                .identity_binding(&PublicationBaseUrl::parse("https://moved.example/").unwrap())
        );
        assert_ne!(
            original.mailbox_digest(&a),
            original.mailbox_digest(&EmailAddress::parse("different@example.com").unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn key_loader_rejects_unsafe_files_and_noncanonical_bytes_without_disclosure() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("private-control-key");
        let instance = InstanceId::from_uuid(Uuid::new_v4());
        std::fs::write(&path, format!("{}\n", "ab".repeat(32))).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(MailControls::load(&path, instance).is_ok());
        let link = root.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(MailControls::load(&link, instance).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            MailControls::load(&path, instance),
            Err(ControlKeyError::Protection)
        ));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        for invalid in [
            "AB".repeat(32),
            "00".repeat(32),
            "ab".repeat(31),
            format!("{}\r\n", "ab".repeat(32)),
            "private-malformed-key".to_owned(),
        ] {
            std::fs::write(&path, &invalid).unwrap();
            let error = match MailControls::load(&path, instance) {
                Ok(_) => panic!("invalid key accepted"),
                Err(error) => error,
            };
            let diagnostic = format!("{error} {error:?}");
            assert!(!diagnostic.contains(&invalid));
            assert!(!diagnostic.contains("private-control-key"));
        }
    }
}
