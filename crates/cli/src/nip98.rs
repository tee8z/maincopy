//! Canonical NIP-98 proof construction for scoped agent credentials.

use std::fmt;

use crate::transport::RequestBody;
use base64::{Engine as _, engine::general_purpose};
use k256::schnorr::{Signature, SigningKey, VerifyingKey, signature::hazmat::PrehashVerifier as _};
use maincopy_shared::auth_api::SecretString;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use zeroize::Zeroizing;

const NIP98_EVENT_KIND: u64 = 27_235;
const PRIVATE_KEY_BYTES: usize = 32;

pub(crate) struct AgentPrivateKey {
    signing_key: SigningKey,
}

/// Public values used to compare the local agent key with a server grant.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub(crate) struct AgentPublicIdentity {
    pub(crate) public_key: Box<str>,
    pub(crate) fingerprint: Box<str>,
}

/// Validates a canonical public key and derives its public comparison fingerprint.
pub(crate) fn inspect_public_key(value: &str) -> Result<AgentPublicIdentity, NostrPublicKeyError> {
    let bytes = decode_lower_hex::<32>(value).ok_or(NostrPublicKeyError)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| NostrPublicKeyError)?;
    Ok(AgentPublicIdentity {
        public_key: value.into(),
        fingerprint: format!(
            "SHA256:{}",
            general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(bytes))
        )
        .into_boxed_str(),
    })
}

#[derive(Debug, Error)]
#[error(
    "the Nostr public key must be a valid x-only secp256k1 key in 64 lowercase hexadecimal characters"
)]
pub(crate) struct NostrPublicKeyError;

impl AgentPrivateKey {
    pub(crate) fn parse(encoded: &str) -> Result<Self, AgentPrivateKeyError> {
        let bytes = Zeroizing::new(
            decode_lower_hex::<PRIVATE_KEY_BYTES>(encoded)
                .ok_or(AgentPrivateKeyError::InvalidEncoding)?,
        );
        let signing_key =
            SigningKey::from_bytes(&*bytes).map_err(|_| AgentPrivateKeyError::InvalidScalar)?;
        Ok(Self { signing_key })
    }

    pub(crate) fn public_key_hex(&self) -> String {
        encode_lower_hex(&self.signing_key.verifying_key().to_bytes())
    }

    pub(crate) fn public_identity(&self) -> AgentPublicIdentity {
        let bytes = self.signing_key.verifying_key().to_bytes();
        AgentPublicIdentity {
            public_key: encode_lower_hex(&bytes).into_boxed_str(),
            fingerprint: format!(
                "SHA256:{}",
                general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(bytes))
            )
            .into_boxed_str(),
        }
    }
}

impl fmt::Debug for AgentPrivateKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AgentPrivateKey(<redacted>)")
    }
}

#[derive(Debug, Error)]
pub(crate) enum AgentPrivateKeyError {
    #[error("the Nostr private key must be exactly 64 lowercase hexadecimal characters")]
    InvalidEncoding,
    #[error("the Nostr private key is not a valid secp256k1 scalar")]
    InvalidScalar,
}

#[derive(Serialize)]
struct WireEvent {
    id: String,
    pubkey: String,
    created_at: i64,
    kind: u64,
    tags: Vec<[String; 2]>,
    content: String,
    sig: String,
}

/// Produces canonical unpadded-base64 JSON accepted by the server's NIP-98 verifier.
pub(crate) fn authorization_proof(
    signing_key: &AgentPrivateKey,
    created_at: i64,
    absolute_url: &str,
    method: &str,
    body: &[u8],
    idempotency_key: &str,
) -> Result<Zeroizing<String>, Nip98SigningError> {
    let public_key = signing_key.public_key_hex();
    let mut tags = vec![
        ["u".to_owned(), absolute_url.to_owned()],
        ["method".to_owned(), method.to_owned()],
        [
            "payload".to_owned(),
            encode_lower_hex(&Sha256::digest(body)),
        ],
    ];
    tags.push(["idempotency".to_owned(), idempotency_key.to_owned()]);

    let serialized = serde_json::to_vec(&(0, &public_key, created_at, NIP98_EVENT_KIND, &tags, ""))
        .map_err(|_| Nip98SigningError::Serialization)?;
    let event_id: [u8; 32] = Sha256::digest(serialized).into();
    let mut auxiliary_randomness = Zeroizing::new([0_u8; 32]);
    getrandom::fill(&mut *auxiliary_randomness).map_err(|_| Nip98SigningError::Randomness)?;
    let signature = signing_key
        .signing_key
        .sign_raw(&event_id, &auxiliary_randomness)
        .map_err(|_| Nip98SigningError::Signature)?;
    let event = WireEvent {
        id: encode_lower_hex(&event_id),
        pubkey: public_key,
        created_at,
        kind: NIP98_EVENT_KIND,
        tags,
        content: String::new(),
        sig: encode_lower_hex(&signature.to_bytes()),
    };
    let json = serde_json::to_vec(&event).map_err(|_| Nip98SigningError::Serialization)?;
    Ok(Zeroizing::new(
        general_purpose::STANDARD_NO_PAD.encode(json),
    ))
}

#[derive(Debug, Error)]
pub(crate) enum Nip98SigningError {
    #[error("the operating system could not generate NIP-98 signing randomness")]
    Randomness,
    #[error("the NIP-98 event could not be signed")]
    Signature,
    #[error("the NIP-98 event could not be serialized")]
    Serialization,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HumanLoginEvent {
    id: Box<str>,
    pubkey: Box<str>,
    created_at: i64,
    kind: u64,
    tags: Vec<[SecretString; 2]>,
    content: SecretString,
    sig: Box<str>,
}

pub(crate) fn validate_human_login_proof(
    json: &str,
    url: &str,
    challenge: &str,
    created_at: i64,
) -> Result<(), NostrLoginProofError> {
    if json.len() > 16 * 1024 {
        return Err(NostrLoginProofError);
    }
    let event: HumanLoginEvent = serde_json::from_str(json).map_err(|_| NostrLoginProofError)?;
    if event.kind != NIP98_EVENT_KIND
        || event.created_at != created_at
        || !event.content.expose_secret().is_empty()
    {
        return Err(NostrLoginProofError);
    }
    let expected = [["u", url], ["method", "POST"], ["challenge", challenge]];
    if event.tags.len() != expected.len()
        || !event.tags.iter().zip(expected).all(|(actual, expected)| {
            actual[0].expose_secret() == expected[0] && actual[1].expose_secret() == expected[1]
        })
    {
        return Err(NostrLoginProofError);
    }
    verify_human_login_signature(&event)
}

fn verify_human_login_signature(event: &HumanLoginEvent) -> Result<(), NostrLoginProofError> {
    let bytes = decode_lower_hex::<32>(&event.pubkey).ok_or(NostrLoginProofError)?;
    let key = VerifyingKey::from_bytes(&bytes).map_err(|_| NostrLoginProofError)?;
    let serialized = RequestBody::json(&(
        0,
        &event.pubkey,
        event.created_at,
        event.kind,
        &event.tags,
        &event.content,
    ))
    .map_err(|_| NostrLoginProofError)?;
    let digest: [u8; 32] = Sha256::digest(serialized.as_ref()).into();
    if Some(digest) != decode_lower_hex::<32>(&event.id) {
        return Err(NostrLoginProofError);
    }
    let signature = decode_lower_hex::<64>(&event.sig).ok_or(NostrLoginProofError)?;
    let signature = Signature::try_from(signature.as_slice()).map_err(|_| NostrLoginProofError)?;
    key.verify_prehash(&digest, &signature)
        .map_err(|_| NostrLoginProofError)
}

#[derive(Debug, Error)]
#[error("the signed event does not prove the requested human login")]
pub(crate) struct NostrLoginProofError;

fn encode_lower_hex(bytes: &[u8]) -> String {
    const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(LOWER_HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(LOWER_HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_lower_hex<const LENGTH: usize>(encoded: &str) -> Option<[u8; LENGTH]> {
    if encoded.len() != LENGTH * 2 {
        return None;
    }
    let mut decoded = [0_u8; LENGTH];
    for (index, pair) in encoded.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        decoded[index] = lower_hex_nibble(pair[0])?
            .checked_mul(16)?
            .checked_add(lower_hex_nibble(pair[1])?)?;
    }
    Some(decoded)
}

const fn lower_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    const KEY: &str = "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a";

    fn human_event() -> serde_json::Value {
        let signing_key = SigningKey::from_bytes(&[3; 32]).unwrap();
        let public_key = encode_lower_hex(&signing_key.verifying_key().to_bytes());
        let tags = [
            [
                "u",
                "https://admin.example.test:8443/api/admin/v1/auth/sessions",
            ],
            ["method", "POST"],
            ["challenge", "mcl1_fixture"],
        ];
        let canonical =
            serde_json::to_vec(&(0, &public_key, 1_800_000_000_i64, 27_235, tags, "")).unwrap();
        let id: [u8; 32] = Sha256::digest(canonical).into();
        let signature = signing_key.sign_raw(&id, &[0; 32]).unwrap();
        serde_json::json!({"id": encode_lower_hex(&id), "pubkey": public_key, "created_at": 1_800_000_000, "kind": 27_235, "tags": tags, "content":"", "sig": encode_lower_hex(&signature.to_bytes())})
    }

    #[test]
    fn public_key_inspection_rejects_noncanonical_or_invalid_curve_points() {
        let identity = AgentPrivateKey::parse(KEY).unwrap().public_identity();
        assert_eq!(inspect_public_key(&identity.public_key).unwrap(), identity);
        for value in [
            "".to_string(),
            "0".repeat(64),
            "f".repeat(64),
            identity.public_key.to_uppercase(),
        ] {
            assert!(inspect_public_key(&value).is_err());
        }
    }

    #[test]
    fn human_login_requires_the_requested_intent_and_a_valid_schnorr_signature() {
        let event = human_event();
        let validate = |value: &serde_json::Value| {
            validate_human_login_proof(
                &value.to_string(),
                "https://admin.example.test:8443/api/admin/v1/auth/sessions",
                "mcl1_fixture",
                1_800_000_000,
            )
        };
        validate(&event).unwrap();
        for (field, value) in [
            ("kind", serde_json::json!(1)),
            ("created_at", serde_json::json!(1_800_000_001)),
            ("content", serde_json::json!("changed")),
            ("id", serde_json::json!("0".repeat(64))),
            ("pubkey", serde_json::json!("f".repeat(64))),
            ("sig", serde_json::json!("0".repeat(128))),
            ("extra", serde_json::json!("ignored?")),
        ] {
            let mut altered = event.clone();
            altered[field] = value;
            assert!(validate(&altered).is_err(), "{field}");
        }
        for (index, value) in [
            (0, "https://other.example.test/api/admin/v1/auth/sessions"),
            (1, "GET"),
            (2, "other_challenge"),
        ] {
            let mut altered = event.clone();
            altered["tags"][index][1] = serde_json::json!(value);
            assert!(validate(&altered).is_err());
        }
        let mut altered = event.clone();
        altered["tags"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!(["payload", "0".repeat(64)]));
        assert!(validate(&altered).is_err());
        assert!(validate_human_login_proof(&" ".repeat(16 * 1024 + 1), "", "", 0).is_err());
        assert!(validate_human_login_proof("not JSON", "", "", 0).is_err());
    }

    #[test]
    fn public_identity_fingerprints_the_x_only_public_key_bytes() {
        // The first BIP-340 test vector uses private scalar 3.
        let key = AgentPrivateKey::parse(
            "0000000000000000000000000000000000000000000000000000000000000003",
        )
        .unwrap();
        let identity = key.public_identity();
        assert_eq!(
            identity.public_key.as_ref(),
            "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9"
        );
        assert_eq!(
            identity.fingerprint.as_ref(),
            "SHA256:fHnzBx4oNE6BU79sc8KU6+N1SuxOLLjLRHGy9Ey18i0"
        );
        assert_eq!(key.public_key_hex(), identity.public_key.as_ref());
    }

    fn decoded_event(proof: &str) -> Value {
        let bytes = general_purpose::STANDARD_NO_PAD.decode(proof).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn proof_binds_exact_url_method_payload_and_mutation_identity() {
        let key = AgentPrivateKey::parse(KEY).unwrap();
        let proof = authorization_proof(
            &key,
            1_800_000_000,
            "https://admin.example.test/api/admin/v1/publications",
            "POST",
            br#"{"post_id":"exact"}"#,
            "11111111-1111-4111-8111-111111111111",
        )
        .unwrap();
        assert!(!proof.ends_with('='));
        let event = decoded_event(&proof);
        assert_eq!(event["kind"], NIP98_EVENT_KIND);
        assert_eq!(event["content"], "");
        assert_eq!(event["created_at"], 1_800_000_000_i64);
        assert_eq!(
            event["tags"],
            serde_json::json!([
                ["u", "https://admin.example.test/api/admin/v1/publications"],
                ["method", "POST"],
                [
                    "payload",
                    encode_lower_hex(&Sha256::digest(br#"{"post_id":"exact"}"#))
                ],
                ["idempotency", "11111111-1111-4111-8111-111111111111"]
            ])
        );

        let event_id = decode_lower_hex::<32>(event["id"].as_str().unwrap()).unwrap();
        let signature_bytes = decode_lower_hex::<64>(event["sig"].as_str().unwrap()).unwrap();
        let signature = Signature::try_from(signature_bytes.as_slice()).unwrap();
        let public_bytes = decode_lower_hex::<32>(event["pubkey"].as_str().unwrap()).unwrap();
        let public_key = VerifyingKey::from_bytes(&public_bytes).unwrap();
        public_key.verify_prehash(&event_id, &signature).unwrap();
    }

    #[test]
    fn read_proofs_bind_a_unique_request_identity() {
        let key = AgentPrivateKey::parse(KEY).unwrap();
        let proof = authorization_proof(
            &key,
            1_800_000_001,
            "https://admin.example.test/api/admin/v1/posts",
            "GET",
            b"",
            "22222222-2222-4222-8222-222222222222",
        )
        .unwrap();
        let tags = decoded_event(&proof)["tags"].as_array().unwrap().clone();
        assert_eq!(tags.len(), 4);
        assert_eq!(
            tags[3],
            serde_json::json!(["idempotency", "22222222-2222-4222-8222-222222222222"])
        );
    }

    #[test]
    fn private_key_and_diagnostics_are_strict_and_redacted() {
        let key = AgentPrivateKey::parse(KEY).unwrap();
        assert_eq!(format!("{key:?}"), "AgentPrivateKey(<redacted>)");
        assert!(!format!("{key:?}").contains(KEY));
        assert!(AgentPrivateKey::parse(&KEY.to_uppercase()).is_err());
        assert!(AgentPrivateKey::parse(&"00".repeat(32)).is_err());
    }
}
