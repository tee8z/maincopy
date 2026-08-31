//! Canonical NIP-98 proof construction for scoped agent credentials.

use std::fmt;

use base64::{Engine as _, engine::general_purpose};
use k256::schnorr::SigningKey;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use zeroize::Zeroizing;

const NIP98_EVENT_KIND: u64 = 27_235;
const PRIVATE_KEY_BYTES: usize = 32;

pub(crate) struct AgentPrivateKey {
    signing_key: SigningKey,
}

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
    for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
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
    use base64::{Engine as _, engine::general_purpose};
    use k256::schnorr::{Signature, VerifyingKey, signature::hazmat::PrehashVerifier as _};
    use serde_json::Value;

    use super::*;

    const KEY: &str = "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a";

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
