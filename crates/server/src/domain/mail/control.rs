//! Address-free, purpose-bound subscriber controls. This protocol authenticates
//! UUIDv4 enrollment identities; it does not encrypt them or consume a nonce.
//! The caller must atomically check the current enrollment generation and, for
//! confirmation, the pending nonce and consent state before changing state.
//!
//! Times have whole-second precision. Confirmation lasts at most 24 hours;
//! management has no time expiry so an old message can still remove an active
//! enrollment. Keep its signing key and stable mail-site configuration digest
//! available until every corresponding generation is revoked. Ordinary content
//! changes must not change that digest. Key selection and rotation belong to the
//! caller; this module never tries fallback keys. Persist the site binding across
//! releases and provider credential changes. Never generate a replacement signing
//! key at startup; retain verification keys for existing links or explicitly
//! migrate those links before retiring a key.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use subtle::ConstantTimeEq as _;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::{Uuid, Variant, Version};
use zeroize::Zeroizing;

const PREFIX: &str = "mct1.";
const VERSION: u8 = 1;
const MAC_CONTEXT: &[u8] = b"maincopy mail control token\0";
const MAC_BYTES: usize = 32;
const MANAGE_PAYLOAD_BYTES: usize = 42;
const CONFIRM_PAYLOAD_BYTES: usize = 66;
const MAX_BINARY_BYTES: usize = CONFIRM_PAYLOAD_BYTES + MAC_BYTES;
pub(super) const MAX_TOKEN_BYTES: usize = PREFIX.len() + (MAX_BINARY_BYTES * 4).div_ceil(3);
const MAX_CONFIRMATION_SECONDS: i64 = 24 * 60 * 60;

/// The protected loader transfers ownership; it must validate file permissions
/// and decode its configured format before constructing this exact-width key.
pub(super) struct ControlSigningKey(Zeroizing<[u8; 32]>);

pub(super) struct EncodedControlToken(Zeroizing<String>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ControlPurpose {
    Confirm,
    Manage,
}

#[derive(Eq, PartialEq)]
pub(super) enum ControlClaims {
    Confirm {
        enrollment: Uuid,
        generation: Uuid,
        confirmation_nonce: Uuid,
        expires_at: OffsetDateTime,
    },
    Manage {
        enrollment: Uuid,
        generation: Uuid,
    },
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ControlTokenError {
    #[error("the subscriber control token is malformed")]
    Malformed,
    #[error("the subscriber control token version is unsupported")]
    UnsupportedVersion,
    #[error("the subscriber control token has the wrong purpose")]
    PurposeMismatch,
    #[error("the subscriber control token could not be authenticated")]
    AuthenticationFailed,
    #[error("the subscriber control claims are invalid")]
    InvalidClaims,
    #[error("the subscriber confirmation token has expired")]
    Expired,
    #[error("the subscriber control token was issued in the future")]
    IssuedInFuture,
}

impl ControlSigningKey {
    pub(super) fn from_bytes(bytes: Zeroizing<[u8; 32]>) -> Self {
        Self(bytes)
    }

    pub(super) fn issue(
        &self,
        site_binding: &[u8; 32],
        claims: ControlClaims,
        now: OffsetDateTime,
    ) -> Result<EncodedControlToken, ControlTokenError> {
        claims.validate(now.unix_timestamp(), now.unix_timestamp())?;
        let mut bytes = claims.encode(now.unix_timestamp());
        let mac = self.authenticate(site_binding, &bytes);
        bytes.extend_from_slice(mac.as_ref());
        let mut encoded = Zeroizing::new(String::with_capacity(MAX_TOKEN_BYTES));
        encoded.push_str(PREFIX);
        URL_SAFE_NO_PAD.encode_string(&bytes, &mut encoded);
        Ok(EncodedControlToken(encoded))
    }

    pub(super) fn verify(
        &self,
        site_binding: &[u8; 32],
        expected_purpose: ControlPurpose,
        input: &str,
        now: OffsetDateTime,
    ) -> Result<ControlClaims, ControlTokenError> {
        let decoded = DecodedControlToken::parse(input)?;
        let (payload, supplied_mac) =
            decoded.bytes[..decoded.len].split_at(decoded.len - MAC_BYTES);
        let expected_mac = self.authenticate(site_binding, payload);
        if !bool::from(expected_mac.as_slice().ct_eq(supplied_mac)) {
            return Err(ControlTokenError::AuthenticationFailed);
        }
        decoded.claims(expected_purpose, now.unix_timestamp())
    }

    fn authenticate(&self, site_binding: &[u8; 32], payload: &[u8]) -> Zeroizing<[u8; 32]> {
        let mut input = Zeroizing::new(Vec::with_capacity(
            MAC_CONTEXT.len() + site_binding.len() + payload.len(),
        ));
        input.extend_from_slice(MAC_CONTEXT);
        input.extend_from_slice(site_binding);
        input.extend_from_slice(payload);
        Zeroizing::new(*blake3::keyed_hash(&self.0, &input).as_bytes())
    }
}

impl EncodedControlToken {
    /// Borrow only while composing a protected URL or response. Do not persist
    /// the bearer token, place it in diagnostics, or retain its containing URL.
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl ControlClaims {
    fn encode(&self, issued_at: i64) -> Zeroizing<Vec<u8>> {
        let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_BINARY_BYTES));
        let (purpose, enrollment, generation) = self.common();
        bytes.extend_from_slice(&[VERSION, purpose.wire()]);
        bytes.extend_from_slice(&issued_at.to_be_bytes());
        bytes.extend_from_slice(enrollment.as_bytes());
        bytes.extend_from_slice(generation.as_bytes());
        if let Self::Confirm {
            confirmation_nonce,
            expires_at,
            ..
        } = self
        {
            bytes.extend_from_slice(confirmation_nonce.as_bytes());
            bytes.extend_from_slice(&expires_at.unix_timestamp().to_be_bytes());
        }
        bytes
    }

    fn common(&self) -> (ControlPurpose, &Uuid, &Uuid) {
        match self {
            Self::Confirm {
                enrollment,
                generation,
                ..
            } => (ControlPurpose::Confirm, enrollment, generation),
            Self::Manage {
                enrollment,
                generation,
            } => (ControlPurpose::Manage, enrollment, generation),
        }
    }

    fn validate(&self, issued_at: i64, now: i64) -> Result<(), ControlTokenError> {
        let (_, enrollment, generation) = self.common();
        if !random_uuid(enrollment) || !random_uuid(generation) {
            return Err(ControlTokenError::InvalidClaims);
        }
        if issued_at > now {
            return Err(ControlTokenError::IssuedInFuture);
        }
        match self {
            Self::Confirm {
                confirmation_nonce,
                expires_at,
                ..
            } => validate_confirmation(
                confirmation_nonce,
                expires_at.unix_timestamp(),
                issued_at,
                now,
            ),
            Self::Manage { .. } => Ok(()),
        }
    }
}

impl ControlPurpose {
    const fn wire(self) -> u8 {
        match self {
            Self::Confirm => 1,
            Self::Manage => 2,
        }
    }

    const fn payload_bytes(self) -> usize {
        match self {
            Self::Confirm => CONFIRM_PAYLOAD_BYTES,
            Self::Manage => MANAGE_PAYLOAD_BYTES,
        }
    }
}

struct DecodedControlToken {
    bytes: Zeroizing<[u8; MAX_BINARY_BYTES]>,
    len: usize,
}

impl DecodedControlToken {
    fn parse(input: &str) -> Result<Self, ControlTokenError> {
        if input.len() > MAX_TOKEN_BYTES {
            return Err(ControlTokenError::Malformed);
        }
        let encoded = input
            .strip_prefix(PREFIX)
            .ok_or(ControlTokenError::Malformed)?;
        let mut bytes = Zeroizing::new([0_u8; MAX_BINARY_BYTES]);
        // This engine rejects padding and nonzero unused bits; decode_slice
        // bounds output without allocating an attacker-sized intermediate.
        let len = URL_SAFE_NO_PAD
            .decode_slice(encoded, bytes.as_mut_slice())
            .map_err(|_| ControlTokenError::Malformed)?;
        if len != MANAGE_PAYLOAD_BYTES + MAC_BYTES && len != MAX_BINARY_BYTES {
            return Err(ControlTokenError::Malformed);
        }
        Ok(Self { bytes, len })
    }

    fn claims(
        &self,
        expected: ControlPurpose,
        now: i64,
    ) -> Result<ControlClaims, ControlTokenError> {
        if self.bytes[0] != VERSION {
            return Err(ControlTokenError::UnsupportedVersion);
        }
        if self.bytes[1] != expected.wire() {
            return Err(ControlTokenError::PurposeMismatch);
        }
        if self.len != expected.payload_bytes() + MAC_BYTES {
            return Err(ControlTokenError::Malformed);
        }
        let issued_at = decode_timestamp(&self.bytes[2..10])?.unix_timestamp();
        let enrollment = decode_uuid(&self.bytes[10..26])?;
        let generation = decode_uuid(&self.bytes[26..42])?;
        let claims = match expected {
            ControlPurpose::Confirm => ControlClaims::Confirm {
                enrollment,
                generation,
                confirmation_nonce: decode_uuid(&self.bytes[42..58])?,
                expires_at: decode_timestamp(&self.bytes[58..66])?,
            },
            ControlPurpose::Manage => ControlClaims::Manage {
                enrollment,
                generation,
            },
        };
        claims.validate(issued_at, now)?;
        Ok(claims)
    }
}

fn random_uuid(value: &Uuid) -> bool {
    value.get_variant() == Variant::RFC4122 && value.get_version() == Some(Version::Random)
}

fn decode_uuid(bytes: &[u8]) -> Result<Uuid, ControlTokenError> {
    Uuid::from_slice(bytes).map_err(|_| ControlTokenError::InvalidClaims)
}

fn decode_timestamp(bytes: &[u8]) -> Result<OffsetDateTime, ControlTokenError> {
    let bytes = bytes
        .try_into()
        .map_err(|_| ControlTokenError::InvalidClaims)?;
    OffsetDateTime::from_unix_timestamp(i64::from_be_bytes(bytes))
        .map_err(|_| ControlTokenError::InvalidClaims)
}

fn validate_confirmation(
    nonce: &Uuid,
    expires_at: i64,
    issued_at: i64,
    now: i64,
) -> Result<(), ControlTokenError> {
    let lifetime = expires_at
        .checked_sub(issued_at)
        .ok_or(ControlTokenError::InvalidClaims)?;
    if !random_uuid(nonce) || !(1..=MAX_CONFIRMATION_SECONDS).contains(&lifetime) {
        return Err(ControlTokenError::InvalidClaims);
    }
    if expires_at <= now {
        return Err(ControlTokenError::Expired);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fmt::Debug, str::from_utf8};

    use serde::Serialize;
    use time::Duration;

    use super::*;

    macro_rules! assert_not_impl {
        ($value:ty: $bound:path) => {
            const _: fn() = || {
                trait AmbiguousIfImpl<Marker> {
                    fn marker() {}
                }
                impl<Value: ?Sized> AmbiguousIfImpl<()> for Value {}
                impl<Value: ?Sized + $bound> AmbiguousIfImpl<u8> for Value {}
                let _ = <$value as AmbiguousIfImpl<_>>::marker;
            };
        };
    }

    assert_not_impl!(ControlSigningKey: Clone);
    assert_not_impl!(ControlSigningKey: Debug);
    assert_not_impl!(ControlSigningKey: Serialize);
    assert_not_impl!(EncodedControlToken: Clone);
    assert_not_impl!(EncodedControlToken: Debug);
    assert_not_impl!(EncodedControlToken: Serialize);
    assert_not_impl!(ControlClaims: Debug);
    assert_not_impl!(ControlClaims: Serialize);

    fn key() -> ControlSigningKey {
        ControlSigningKey::from_bytes(Zeroizing::new([17; 32]))
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    fn confirm(expires_at: OffsetDateTime) -> ControlClaims {
        ControlClaims::Confirm {
            enrollment: Uuid::from_u128(0x01010101_0101_4101_8101_010101010101),
            generation: Uuid::from_u128(0x02020202_0202_4202_8202_020202020202),
            confirmation_nonce: Uuid::from_u128(0x03030303_0303_4303_8303_030303030303),
            expires_at,
        }
    }

    fn manage() -> ControlClaims {
        let ControlClaims::Confirm {
            enrollment,
            generation,
            ..
        } = confirm(now())
        else {
            unreachable!()
        };
        ControlClaims::Manage {
            enrollment,
            generation,
        }
    }

    fn sign_payload(key: &ControlSigningKey, mut bytes: Vec<u8>) -> String {
        let mac = key.authenticate(&[9; 32], &bytes);
        bytes.extend_from_slice(mac.as_ref());
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
    }

    #[test]
    fn confirmation_round_trip_preserves_replay_identity_without_consuming_it() {
        let key = key();
        let expires_at = now() + Duration::hours(24);
        let token = key.issue(&[9; 32], confirm(expires_at), now()).unwrap();
        assert!(token.as_str().len() <= MAX_TOKEN_BYTES);
        assert!(!token.as_str().contains('='));
        for _ in 0..2 {
            let claims = key
                .verify(&[9; 32], ControlPurpose::Confirm, token.as_str(), now())
                .unwrap();
            assert!(claims == confirm(expires_at));
        }
        assert_eq!(
            key.verify(&[9; 32], ControlPurpose::Manage, token.as_str(), now())
                .err(),
            Some(ControlTokenError::PurposeMismatch)
        );
        assert_eq!(
            key.verify(
                &[9; 32],
                ControlPurpose::Confirm,
                token.as_str(),
                expires_at
            )
            .err(),
            Some(ControlTokenError::Expired)
        );
    }

    #[test]
    fn management_has_no_expiry_and_is_distinct_from_confirmation() {
        let key = key();
        let token = key.issue(&[9; 32], manage(), now()).unwrap();
        let claims = key
            .verify(
                &[9; 32],
                ControlPurpose::Manage,
                token.as_str(),
                now() + Duration::days(36500),
            )
            .unwrap();
        assert!(claims == manage());
        assert_eq!(
            key.verify(&[9; 32], ControlPurpose::Confirm, token.as_str(), now())
                .err(),
            Some(ControlTokenError::PurposeMismatch)
        );
    }

    #[test]
    fn rejects_wrong_site_rotated_key_and_changes_to_every_authenticated_byte() {
        let key = key();
        let token = key.issue(&[9; 32], manage(), now()).unwrap();
        assert_eq!(
            key.verify(&[8; 32], ControlPurpose::Manage, token.as_str(), now())
                .err(),
            Some(ControlTokenError::AuthenticationFailed)
        );
        let rotated = ControlSigningKey::from_bytes(Zeroizing::new([18; 32]));
        assert_eq!(
            rotated
                .verify(&[9; 32], ControlPurpose::Manage, token.as_str(), now())
                .err(),
            Some(ControlTokenError::AuthenticationFailed)
        );
        let original = URL_SAFE_NO_PAD
            .decode(token.as_str().strip_prefix(PREFIX).unwrap())
            .unwrap();
        for index in 0..original.len() {
            let mut altered = original.clone();
            altered[index] ^= 1;
            let encoded = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(altered));
            assert_eq!(
                key.verify(&[9; 32], ControlPurpose::Manage, &encoded, now())
                    .err(),
                Some(ControlTokenError::AuthenticationFailed)
            );
        }
    }

    #[test]
    fn rejects_noncanonical_encoding_truncation_and_oversized_input() {
        let key = key();
        let token = key.issue(&[9; 32], manage(), now()).unwrap();
        for invalid in [
            String::new(),
            format!("{}=", token.as_str()),
            format!("{}\n", token.as_str()),
            format!("{PREFIX}{}", "A".repeat(MAX_TOKEN_BYTES)),
            format!("{PREFIX}{}", "+".repeat(99)),
            format!("{PREFIX}é"),
            token.as_str().replacen(PREFIX, "mct2.", 1),
        ] {
            assert_eq!(
                key.verify(&[9; 32], ControlPurpose::Manage, &invalid, now())
                    .err(),
                Some(ControlTokenError::Malformed)
            );
        }
        for length in 0..token.as_str().len() {
            assert!(
                key.verify(
                    &[9; 32],
                    ControlPurpose::Manage,
                    &token.as_str()[..length],
                    now()
                )
                .is_err()
            );
        }
        let mut noncanonical = token.as_str().as_bytes().to_vec();
        // 74 decoded bytes leave two unused bits in the final base64 character.
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let last = noncanonical.last_mut().unwrap();
        let position = alphabet.iter().position(|byte| byte == last).unwrap();
        *last = alphabet[position | 1];
        assert_eq!(
            key.verify(
                &[9; 32],
                ControlPurpose::Manage,
                from_utf8(&noncanonical).unwrap(),
                now()
            )
            .err(),
            Some(ControlTokenError::Malformed)
        );
    }

    #[test]
    fn authenticated_version_purpose_shape_and_future_time_still_require_validation() {
        let key = key();
        let mut version = manage().encode(now().unix_timestamp()).to_vec();
        version[0] = 2;
        assert_eq!(
            key.verify(
                &[9; 32],
                ControlPurpose::Manage,
                &sign_payload(&key, version),
                now()
            )
            .err(),
            Some(ControlTokenError::UnsupportedVersion)
        );
        let mut shape = manage().encode(now().unix_timestamp()).to_vec();
        shape[1] = ControlPurpose::Confirm.wire();
        assert_eq!(
            key.verify(
                &[9; 32],
                ControlPurpose::Confirm,
                &sign_payload(&key, shape),
                now()
            )
            .err(),
            Some(ControlTokenError::Malformed)
        );
        let future = key
            .issue(&[9; 32], manage(), now() + Duration::seconds(1))
            .unwrap();
        assert_eq!(
            key.verify(&[9; 32], ControlPurpose::Manage, future.as_str(), now())
                .err(),
            Some(ControlTokenError::IssuedInFuture)
        );
    }

    #[test]
    fn refuses_unbounded_confirmation_lifetimes_and_nonrandom_claim_identities() {
        let key = key();
        for expires_at in [
            now(),
            now() - Duration::seconds(1),
            now() + Duration::hours(24) + Duration::seconds(1),
        ] {
            assert_eq!(
                key.issue(&[9; 32], confirm(expires_at), now()).err(),
                Some(ControlTokenError::InvalidClaims)
            );
        }
        for field in 0..3 {
            let ControlClaims::Confirm {
                mut enrollment,
                mut generation,
                mut confirmation_nonce,
                expires_at,
            } = confirm(now() + Duration::hours(1))
            else {
                unreachable!()
            };
            match field {
                0 => enrollment = Uuid::nil(),
                1 => generation = Uuid::nil(),
                _ => confirmation_nonce = Uuid::nil(),
            }
            assert_eq!(
                key.issue(
                    &[9; 32],
                    ControlClaims::Confirm {
                        enrollment,
                        generation,
                        confirmation_nonce,
                        expires_at,
                    },
                    now()
                )
                .err(),
                Some(ControlTokenError::InvalidClaims)
            );
        }
        let nonstandard_variant = Uuid::from_u128(0x01010101_0101_4101_0101_010101010101);
        assert_eq!(
            key.issue(
                &[9; 32],
                ControlClaims::Manage {
                    enrollment: nonstandard_variant,
                    generation: Uuid::new_v4(),
                },
                now(),
            )
            .err(),
            Some(ControlTokenError::InvalidClaims)
        );
    }

    #[test]
    fn authenticated_claims_still_reject_impossible_dates_and_excessive_lifetimes() {
        let key = key();
        let mut invalid_date = manage().encode(now().unix_timestamp()).to_vec();
        invalid_date[2..10].copy_from_slice(&i64::MAX.to_be_bytes());
        assert_eq!(
            key.verify(
                &[9; 32],
                ControlPurpose::Manage,
                &sign_payload(&key, invalid_date),
                now(),
            )
            .err(),
            Some(ControlTokenError::InvalidClaims)
        );
        let too_long = confirm(now() + Duration::hours(25))
            .encode(now().unix_timestamp())
            .to_vec();
        assert_eq!(
            key.verify(
                &[9; 32],
                ControlPurpose::Confirm,
                &sign_payload(&key, too_long),
                now(),
            )
            .err(),
            Some(ControlTokenError::InvalidClaims)
        );
    }

    #[test]
    fn errors_never_include_tokens_keys_or_claim_values() {
        for error in [
            ControlTokenError::Malformed,
            ControlTokenError::UnsupportedVersion,
            ControlTokenError::PurposeMismatch,
            ControlTokenError::AuthenticationFailed,
            ControlTokenError::InvalidClaims,
            ControlTokenError::Expired,
            ControlTokenError::IssuedInFuture,
        ] {
            assert!(!format!("{error:?}: {error}").contains(PREFIX));
            assert!(!format!("{error:?}: {error}").contains("01010101"));
        }
    }
}
