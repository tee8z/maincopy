use std::fmt;

use k256::schnorr::Signature;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;

use super::identity::{NostrPublicKey, decode_lower_hex, encode_lower_hex};

pub const NIP98_EVENT_KIND: u64 = 27_235;
pub const NIP98_FRESHNESS_SECONDS: u64 = 60;
pub const MAX_NIP98_EVENT_BYTES: usize = 16 * 1024;
const EVENT_ID_BYTES: usize = 32;
const SIGNATURE_BYTES: usize = 64;
const MAX_METHOD_BYTES: usize = 16;

/// Defines whether a signed request must bind a serialized HTTP body.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Nip98Payload<'a> {
    /// Require one payload tag containing the SHA-256 hash of these exact bytes.
    Exact(&'a [u8]),
    /// Reject a payload tag. Used when a one-time challenge supplies freshness.
    Absent,
}

impl fmt::Debug for Nip98Payload<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(body) => formatter
                .debug_tuple("Exact")
                .field(&format_args!("<redacted {} bytes>", body.len()))
                .finish(),
            Self::Absent => formatter.write_str("Absent"),
        }
    }
}

/// Trusted request facts that a NIP-98 event must bind exactly.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct Nip98Request<'a> {
    pub now: OffsetDateTime,
    pub url: &'a str,
    pub method: &'a str,
    pub payload: Nip98Payload<'a>,
    pub challenge: Option<&'a str>,
    pub idempotency_key: Option<&'a str>,
}

impl fmt::Debug for Nip98Request<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Nip98Request")
            .field("now", &self.now)
            .field("url", &self.url)
            .field("method", &self.method)
            .field("payload", &self.payload)
            .field("challenge", &self.challenge.map(|_| "<redacted>"))
            .field(
                "idempotency_key",
                &self.idempotency_key.map(|_| "<present>"),
            )
            .finish()
    }
}

/// A verified event that is ready for credential lookup and replay rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedNip98 {
    pub(crate) public_key: NostrPublicKey,
    pub(crate) event_id: Nip98EventId,
    pub(crate) created_at: OffsetDateTime,
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Nip98EventId([u8; EVENT_ID_BYTES]);

impl Nip98EventId {
    #[cfg(test)]
    pub const fn from_bytes(bytes: [u8; EVENT_ID_BYTES]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; EVENT_ID_BYTES] {
        &self.0
    }

    #[cfg(test)]
    pub fn parse_bytes(bytes: &[u8]) -> Result<Self, Nip98EventIdParseError> {
        let bytes: [u8; EVENT_ID_BYTES] = bytes.try_into().map_err(|_| Nip98EventIdParseError)?;
        Ok(Self(bytes))
    }
}

impl fmt::Debug for Nip98EventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("Nip98EventId")
            .field(&encode_lower_hex(&self.0))
            .finish()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireEvent {
    id: String,
    pubkey: String,
    created_at: i64,
    kind: u64,
    tags: Vec<Vec<String>>,
    content: String,
    sig: String,
}

/// Parses and cryptographically verifies one NIP-98 event.
///
/// Replay rejection and credential state checks consume the returned event ID
/// and public key; they are deliberately outside this pure verifier.
pub fn verify_nip98_event(
    encoded: &[u8],
    request: Nip98Request<'_>,
) -> Result<VerifiedNip98, Nip98VerificationError> {
    validate_request(&request)?;
    if encoded.len() > MAX_NIP98_EVENT_BYTES {
        return Err(Nip98VerificationError::EventTooLarge {
            actual: encoded.len(),
            maximum: MAX_NIP98_EVENT_BYTES,
        });
    }

    let event: WireEvent =
        serde_json::from_slice(encoded).map_err(|_| Nip98VerificationError::MalformedEvent)?;
    let public_key = NostrPublicKey::parse(&event.pubkey)
        .map_err(|_| Nip98VerificationError::InvalidPublicKey)?;
    let claimed_id = decode_lower_hex::<EVENT_ID_BYTES>(&event.id)
        .ok_or(Nip98VerificationError::InvalidEventIdEncoding)?;
    let signature_bytes = decode_lower_hex::<SIGNATURE_BYTES>(&event.sig)
        .ok_or(Nip98VerificationError::InvalidSignatureEncoding)?;
    let signature = Signature::try_from(signature_bytes.as_slice())
        .map_err(|_| Nip98VerificationError::InvalidSignatureEncoding)?;

    let computed_id =
        compute_event_id(&event).map_err(|_| Nip98VerificationError::EventIdConstructionFailed)?;
    if claimed_id != computed_id {
        return Err(Nip98VerificationError::EventIdMismatch);
    }
    public_key
        .verifying_key()
        .verify_raw(&computed_id, &signature)
        .map_err(|_| Nip98VerificationError::InvalidSignature)?;

    if event.kind != NIP98_EVENT_KIND {
        return Err(Nip98VerificationError::WrongKind);
    }
    if !event.content.is_empty() {
        return Err(Nip98VerificationError::NonEmptyContent);
    }
    validate_freshness(request.now.unix_timestamp(), event.created_at)?;
    validate_tags(&event.tags, &request)?;

    let created_at = OffsetDateTime::from_unix_timestamp(event.created_at)
        .map_err(|_| Nip98VerificationError::TimestampOutOfRange)?;
    Ok(VerifiedNip98 {
        public_key,
        event_id: Nip98EventId(computed_id),
        created_at,
    })
}

fn validate_request(request: &Nip98Request<'_>) -> Result<(), Nip98VerificationError> {
    let url = Url::parse(request.url).map_err(|_| Nip98VerificationError::InvalidExpectedUrl)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Nip98VerificationError::InvalidExpectedUrl);
    }

    if request.method.is_empty()
        || request.method.len() > MAX_METHOD_BYTES
        || !request.method.bytes().all(|byte| byte.is_ascii_uppercase())
    {
        return Err(Nip98VerificationError::InvalidExpectedMethod);
    }
    if request.challenge.is_some_and(str::is_empty) {
        return Err(Nip98VerificationError::InvalidExpectedChallenge);
    }
    if request.idempotency_key.is_some_and(str::is_empty) {
        return Err(Nip98VerificationError::InvalidExpectedIdempotencyKey);
    }
    Ok(())
}

fn validate_freshness(now: i64, created_at: i64) -> Result<(), Nip98VerificationError> {
    if now.abs_diff(created_at) > NIP98_FRESHNESS_SECONDS {
        return Err(Nip98VerificationError::StaleEvent);
    }
    Ok(())
}

fn validate_tags(
    tags: &[Vec<String>],
    request: &Nip98Request<'_>,
) -> Result<(), Nip98VerificationError> {
    let mut url = None;
    let mut method = None;
    let mut payload = None;
    let mut challenge = None;
    let mut idempotency = None;

    for tag in tags {
        let [name, value] = tag.as_slice() else {
            return Err(Nip98VerificationError::MalformedTag);
        };

        match name.as_str() {
            "u" => set_once(&mut url, value, Nip98Tag::Url)?,
            "method" => set_once(&mut method, value, Nip98Tag::Method)?,
            "payload" => set_once(&mut payload, value, Nip98Tag::Payload)?,
            "challenge" => set_once(&mut challenge, value, Nip98Tag::Challenge)?,
            "idempotency" => set_once(&mut idempotency, value, Nip98Tag::Idempotency)?,
            _ => return Err(Nip98VerificationError::UnknownTag),
        }
    }

    require_exact(url, request.url, Nip98Tag::Url)?;
    require_exact(method, request.method, Nip98Tag::Method)?;

    match request.payload {
        Nip98Payload::Exact(body) => {
            let expected = encode_lower_hex(&Sha256::digest(body));
            require_exact(payload, &expected, Nip98Tag::Payload)?;
        }
        Nip98Payload::Absent => reject_present(payload, Nip98Tag::Payload)?,
    }
    match request.challenge {
        Some(expected) => require_exact(challenge, expected, Nip98Tag::Challenge)?,
        None => reject_present(challenge, Nip98Tag::Challenge)?,
    }
    match request.idempotency_key {
        Some(expected) => require_exact(idempotency, expected, Nip98Tag::Idempotency)?,
        None => reject_present(idempotency, Nip98Tag::Idempotency)?,
    }
    Ok(())
}

fn set_once<'a>(
    slot: &mut Option<&'a str>,
    value: &'a str,
    tag: Nip98Tag,
) -> Result<(), Nip98VerificationError> {
    if slot.replace(value).is_some() {
        return Err(Nip98VerificationError::DuplicateTag { tag });
    }
    Ok(())
}

fn require_exact(
    actual: Option<&str>,
    expected: &str,
    tag: Nip98Tag,
) -> Result<(), Nip98VerificationError> {
    let actual = actual.ok_or(Nip98VerificationError::MissingTag { tag })?;
    if actual != expected {
        return Err(Nip98VerificationError::TagMismatch { tag });
    }
    Ok(())
}

fn reject_present(actual: Option<&str>, tag: Nip98Tag) -> Result<(), Nip98VerificationError> {
    if actual.is_some() {
        return Err(Nip98VerificationError::UnexpectedTag { tag });
    }
    Ok(())
}

fn compute_event_id(event: &WireEvent) -> Result<[u8; EVENT_ID_BYTES], serde_json::Error> {
    let serialized = serde_json::to_vec(&(
        0,
        &event.pubkey,
        event.created_at,
        event.kind,
        &event.tags,
        &event.content,
    ))?;
    Ok(Sha256::digest(serialized).into())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Nip98Tag {
    Url,
    Method,
    Payload,
    Challenge,
    Idempotency,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("a NIP-98 event ID must be exactly 32 bytes")]
pub struct Nip98EventIdParseError;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum Nip98VerificationError {
    #[error("the NIP-98 event is {actual} bytes; the maximum is {maximum}")]
    EventTooLarge { actual: usize, maximum: usize },
    #[error("the NIP-98 event is malformed")]
    MalformedEvent,
    #[error("the expected NIP-98 URL is not an absolute credential-free HTTPS URL")]
    InvalidExpectedUrl,
    #[error("the expected NIP-98 method is not canonical")]
    InvalidExpectedMethod,
    #[error("the expected NIP-98 challenge is empty")]
    InvalidExpectedChallenge,
    #[error("the expected NIP-98 idempotency key is empty")]
    InvalidExpectedIdempotencyKey,
    #[error("the NIP-98 public key is invalid")]
    InvalidPublicKey,
    #[error("the NIP-98 event ID is not canonical lowercase hexadecimal")]
    InvalidEventIdEncoding,
    #[error("the NIP-98 signature is not canonical lowercase hexadecimal")]
    InvalidSignatureEncoding,
    #[error("the NIP-98 event ID could not be constructed")]
    EventIdConstructionFailed,
    #[error("the NIP-98 event ID does not match the signed event fields")]
    EventIdMismatch,
    #[error("the NIP-98 signature is invalid")]
    InvalidSignature,
    #[error("the NIP-98 event kind is not 27235")]
    WrongKind,
    #[error("the NIP-98 event content is not empty")]
    NonEmptyContent,
    #[error("the NIP-98 event is outside the freshness window")]
    StaleEvent,
    #[error("the NIP-98 event timestamp is outside the supported range")]
    TimestampOutOfRange,
    #[error("a NIP-98 tag is malformed")]
    MalformedTag,
    #[error("the NIP-98 event contains an unsupported tag")]
    UnknownTag,
    #[error("the NIP-98 event contains duplicate {tag:?} tags")]
    DuplicateTag { tag: Nip98Tag },
    #[error("the NIP-98 event is missing the {tag:?} tag")]
    MissingTag { tag: Nip98Tag },
    #[error("the NIP-98 {tag:?} tag does not match the request")]
    TagMismatch { tag: Nip98Tag },
    #[error("the NIP-98 event contains an unexpected {tag:?} tag")]
    UnexpectedTag { tag: Nip98Tag },
}

#[cfg(test)]
mod tests {
    use k256::schnorr::SigningKey;
    use serde_json::{Value, json};

    use super::*;

    const NOW: i64 = 1_800_000_000;
    const URL: &str = "https://admin.example.com/api/admin/v1/posts?state=draft&limit=20";
    const METHOD: &str = "POST";
    const BODY: &[u8] = br#"{"title":"Exact bytes"}"#;
    const CHALLENGE: &str = "mcl1_1111111111111111111111111111111111111111111111111111111111111111";
    const IDEMPOTENCY_KEY: &str = "publish-11111111-1111-4111-8111-111111111111";

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[3_u8; 32]).unwrap()
    }

    fn public_key(signing_key: &SigningKey) -> String {
        encode_lower_hex(&signing_key.verifying_key().to_bytes())
    }

    fn signed_event(created_at: i64, kind: u64, content: &str, tags: Vec<Vec<String>>) -> Vec<u8> {
        let signing_key = signing_key();
        let mut event = WireEvent {
            id: String::new(),
            pubkey: public_key(&signing_key),
            created_at,
            kind,
            tags,
            content: content.into(),
            sig: String::new(),
        };
        let id = compute_event_id(&event).unwrap();
        event.id = encode_lower_hex(&id);
        event.sig = encode_lower_hex(&signing_key.sign_raw(&id, &[7_u8; 32]).unwrap().to_bytes());
        serde_json::to_vec(&event).unwrap()
    }

    fn exact_tags() -> Vec<Vec<String>> {
        vec![
            vec!["u".into(), URL.into()],
            vec!["method".into(), METHOD.into()],
            vec!["payload".into(), encode_lower_hex(&Sha256::digest(BODY))],
            vec!["idempotency".into(), IDEMPOTENCY_KEY.into()],
        ]
    }

    fn exact_request() -> Nip98Request<'static> {
        Nip98Request {
            now: OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
            url: URL,
            method: METHOD,
            payload: Nip98Payload::Exact(BODY),
            challenge: None,
            idempotency_key: Some(IDEMPOTENCY_KEY),
        }
    }

    #[test]
    fn valid_exact_request_returns_replay_and_credential_keys() {
        let encoded = signed_event(NOW, NIP98_EVENT_KIND, "", exact_tags());
        let verified = verify_nip98_event(&encoded, exact_request()).unwrap();

        assert_eq!(verified.public_key.as_str(), public_key(&signing_key()));
        assert_eq!(verified.created_at.unix_timestamp(), NOW);
        assert_ne!(verified.event_id.as_bytes(), &[0; EVENT_ID_BYTES]);
        assert_eq!(
            Nip98EventId::parse_bytes(verified.event_id.as_bytes()).unwrap(),
            verified.event_id
        );
        assert!(Nip98EventId::parse_bytes(&[0; EVENT_ID_BYTES - 1]).is_err());
    }

    #[test]
    fn nip01_serialization_matches_an_independently_hashed_fixture() {
        let event = WireEvent {
            id: String::new(),
            pubkey: "63fe6318dc58583cfe16810f86dd09e18bfd76aabc24a0081ce2856f330504ed".into(),
            created_at: 1_682_327_852,
            kind: NIP98_EVENT_KIND,
            tags: vec![
                vec![
                    "u".into(),
                    "https://api.snort.social/api/v1/n5sp/list".into(),
                ],
                vec!["method".into(), "GET".into()],
            ],
            content: String::new(),
            sig: String::new(),
        };

        assert_eq!(
            encode_lower_hex(&compute_event_id(&event).unwrap()),
            "2dd2dfec3df85dd0d4c32af50241f56a077b0969cb508f987afac1e25b0d4c76"
        );
    }

    #[test]
    fn login_proof_requires_challenge_and_rejects_payload() {
        let tags = vec![
            vec!["u".into(), URL.into()],
            vec!["method".into(), METHOD.into()],
            vec!["challenge".into(), CHALLENGE.into()],
        ];
        let encoded = signed_event(NOW, NIP98_EVENT_KIND, "", tags);
        let request = Nip98Request {
            payload: Nip98Payload::Absent,
            challenge: Some(CHALLENGE),
            idempotency_key: None,
            ..exact_request()
        };
        assert!(verify_nip98_event(&encoded, request).is_ok());

        let mut payload_tags = exact_tags();
        payload_tags.retain(|tag| tag[0] != "idempotency");
        payload_tags.push(vec!["challenge".into(), CHALLENGE.into()]);
        let payload_event = signed_event(NOW, NIP98_EVENT_KIND, "", payload_tags);
        assert_eq!(
            verify_nip98_event(&payload_event, request),
            Err(Nip98VerificationError::UnexpectedTag {
                tag: Nip98Tag::Payload
            })
        );
    }

    #[test]
    fn exact_url_includes_query_order_and_method_is_case_sensitive() {
        let encoded = signed_event(NOW, NIP98_EVENT_KIND, "", exact_tags());
        let reordered = Nip98Request {
            url: "https://admin.example.com/api/admin/v1/posts?limit=20&state=draft",
            ..exact_request()
        };
        assert_eq!(
            verify_nip98_event(&encoded, reordered),
            Err(Nip98VerificationError::TagMismatch { tag: Nip98Tag::Url })
        );

        let lowercase = Nip98Request {
            method: "post",
            ..exact_request()
        };
        assert_eq!(
            verify_nip98_event(&encoded, lowercase),
            Err(Nip98VerificationError::InvalidExpectedMethod)
        );
    }

    #[test]
    fn wrong_semantic_fields_are_rejected_even_when_validly_signed() {
        let wrong_kind = signed_event(NOW, NIP98_EVENT_KIND + 1, "", exact_tags());
        assert_eq!(
            verify_nip98_event(&wrong_kind, exact_request()),
            Err(Nip98VerificationError::WrongKind)
        );

        let content = signed_event(NOW, NIP98_EVENT_KIND, "not empty", exact_tags());
        assert_eq!(
            verify_nip98_event(&content, exact_request()),
            Err(Nip98VerificationError::NonEmptyContent)
        );

        for created_at in [
            NOW - i64::try_from(NIP98_FRESHNESS_SECONDS).unwrap() - 1,
            NOW + i64::try_from(NIP98_FRESHNESS_SECONDS).unwrap() + 1,
        ] {
            let stale = signed_event(created_at, NIP98_EVENT_KIND, "", exact_tags());
            assert_eq!(
                verify_nip98_event(&stale, exact_request()),
                Err(Nip98VerificationError::StaleEvent)
            );
        }
    }

    #[test]
    fn payload_challenge_and_idempotency_are_presence_sensitive() {
        let base = vec![
            vec!["u".into(), URL.into()],
            vec!["method".into(), METHOD.into()],
        ];
        let missing_payload = signed_event(NOW, NIP98_EVENT_KIND, "", base.clone());
        assert_eq!(
            verify_nip98_event(&missing_payload, exact_request()),
            Err(Nip98VerificationError::MissingTag {
                tag: Nip98Tag::Payload
            })
        );

        let mut wrong_payload = exact_tags();
        wrong_payload[2][1] = "00".repeat(EVENT_ID_BYTES);
        assert_eq!(
            verify_nip98_event(
                &signed_event(NOW, NIP98_EVENT_KIND, "", wrong_payload),
                exact_request()
            ),
            Err(Nip98VerificationError::TagMismatch {
                tag: Nip98Tag::Payload
            })
        );

        let absent = Nip98Request {
            payload: Nip98Payload::Absent,
            challenge: None,
            idempotency_key: None,
            ..exact_request()
        };
        let unexpected_idempotency = vec![
            base[0].clone(),
            base[1].clone(),
            vec!["idempotency".into(), IDEMPOTENCY_KEY.into()],
        ];
        assert_eq!(
            verify_nip98_event(
                &signed_event(NOW, NIP98_EVENT_KIND, "", unexpected_idempotency),
                absent
            ),
            Err(Nip98VerificationError::UnexpectedTag {
                tag: Nip98Tag::Idempotency
            })
        );
    }

    #[test]
    fn duplicate_malformed_and_unknown_tags_fail_closed() {
        let mut duplicate = exact_tags();
        duplicate.push(vec!["method".into(), METHOD.into()]);
        assert_eq!(
            verify_nip98_event(
                &signed_event(NOW, NIP98_EVENT_KIND, "", duplicate),
                exact_request()
            ),
            Err(Nip98VerificationError::DuplicateTag {
                tag: Nip98Tag::Method
            })
        );

        let mut malformed = exact_tags();
        malformed.push(vec!["challenge".into()]);
        assert_eq!(
            verify_nip98_event(
                &signed_event(NOW, NIP98_EVENT_KIND, "", malformed),
                exact_request()
            ),
            Err(Nip98VerificationError::MalformedTag)
        );

        let mut unknown = exact_tags();
        unknown.push(vec!["x".into(), "value".into()]);
        assert_eq!(
            verify_nip98_event(
                &signed_event(NOW, NIP98_EVENT_KIND, "", unknown),
                exact_request()
            ),
            Err(Nip98VerificationError::UnknownTag)
        );
    }

    #[test]
    fn tampered_id_signature_and_unknown_json_fields_fail_closed() {
        let encoded = signed_event(NOW, NIP98_EVENT_KIND, "", exact_tags());
        let mut value: Value = serde_json::from_slice(&encoded).unwrap();
        value["id"] = json!("00".repeat(EVENT_ID_BYTES));
        assert_eq!(
            verify_nip98_event(&serde_json::to_vec(&value).unwrap(), exact_request()),
            Err(Nip98VerificationError::EventIdMismatch)
        );

        let mut value: Value = serde_json::from_slice(&encoded).unwrap();
        value["sig"] = json!("00".repeat(SIGNATURE_BYTES));
        assert_eq!(
            verify_nip98_event(&serde_json::to_vec(&value).unwrap(), exact_request()),
            Err(Nip98VerificationError::InvalidSignatureEncoding)
        );

        let mut value: Value = serde_json::from_slice(&encoded).unwrap();
        value["extra"] = json!(true);
        assert_eq!(
            verify_nip98_event(&serde_json::to_vec(&value).unwrap(), exact_request()),
            Err(Nip98VerificationError::MalformedEvent)
        );
    }

    #[test]
    fn event_and_trusted_request_inputs_are_bounded_and_canonical() {
        assert_eq!(
            verify_nip98_event(&vec![b' '; MAX_NIP98_EVENT_BYTES + 1], exact_request()),
            Err(Nip98VerificationError::EventTooLarge {
                actual: MAX_NIP98_EVENT_BYTES + 1,
                maximum: MAX_NIP98_EVENT_BYTES,
            })
        );
        assert_eq!(
            verify_nip98_event(
                b"{}",
                Nip98Request {
                    url: "http://admin.example.com/",
                    ..exact_request()
                }
            ),
            Err(Nip98VerificationError::InvalidExpectedUrl)
        );
        assert_eq!(
            verify_nip98_event(
                b"{}",
                Nip98Request {
                    challenge: Some(""),
                    ..exact_request()
                }
            ),
            Err(Nip98VerificationError::InvalidExpectedChallenge)
        );
    }

    #[test]
    fn trusted_request_debug_output_redacts_body_challenge_and_idempotency_values() {
        let request = Nip98Request {
            payload: Nip98Payload::Exact(BODY),
            challenge: Some(CHALLENGE),
            idempotency_key: Some(IDEMPOTENCY_KEY),
            ..exact_request()
        };
        let debug = format!("{request:?}");
        assert!(!debug.contains("Exact bytes"));
        assert!(!debug.contains(CHALLENGE));
        assert!(!debug.contains(IDEMPOTENCY_KEY));
        assert!(debug.contains("redacted"));
    }
}
