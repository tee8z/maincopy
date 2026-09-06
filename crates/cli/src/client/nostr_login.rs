use std::io;

use maincopy_shared::{
    auth::{HumanLoginProvider, LoginChallengeId},
    auth_api::{
        ADMIN_SESSIONS_PATH, AdminSessionResponse, CreateLoginChallengeRequest,
        CreateLoginChallengeResponse, LOGIN_CHALLENGES_PATH, SecretString,
    },
};
use reqwest::{Method, StatusCode, Url, header::ORIGIN};
use serde::Serialize;
use time::OffsetDateTime;
use zeroize::Zeroizing;

use super::{
    AdminClient, AdminClientError, AdminOrigin, CredentialKey, MAX_JSON_RESPONSE_BYTES,
    PreparedHumanLogin, complete_human_login, origin_header, require_content_type, require_status,
    standard_headers,
};
use crate::{
    credentials::{CredentialStoreError, SecretValue},
    models::AuthenticationContext,
    nip98::validate_human_login_proof,
    transport::{HttpRequest, HttpResponse, RequestBody},
};

/// A single-origin challenge kept in memory until its external signer responds.
pub(crate) struct HumanNostrLogin {
    challenge: CreateLoginChallengeResponse,
    session_url: Url,
    created_at: i64,
}

impl HumanNostrLogin {
    fn from_response(
        origin: &AdminOrigin,
        response: HttpResponse,
        now: OffsetDateTime,
    ) -> Result<Self, AdminClientError> {
        let response = require_status(response, StatusCode::CREATED)?;
        let body = Zeroizing::new(response.body);
        require_content_type(&response.headers, "application/json")?;
        if body.len() > 8192 {
            return Err(invalid_challenge());
        }
        let challenge: CreateLoginChallengeResponse =
            serde_json::from_slice(&body).map_err(|_| invalid_challenge())?;
        validate_challenge(&challenge, now)?;
        Ok(Self {
            challenge,
            session_url: origin.request_url(ADMIN_SESSIONS_PATH)?,
            created_at: now.unix_timestamp(),
        })
    }

    fn prepare_submission(
        self,
        origin: &AdminOrigin,
        proof: SecretString,
        now: OffsetDateTime,
        load: impl FnOnce(&CredentialKey) -> Result<Option<SecretValue>, CredentialStoreError>,
    ) -> Result<PreparedHumanLogin, AdminClientError> {
        if self.session_url.origin().ascii_serialization() != origin.as_str() {
            return Err(AdminClientError::InvalidRequestTarget);
        }
        let credential_key = CredentialKey::human(origin.as_str());
        // Recheck before submission to avoid intentionally replacing an existing session.
        if load(&credential_key)?.is_some() {
            return Err(AdminClientError::HumanSessionAlreadyStored);
        }
        let body = self.session_request(&proof, now)?;
        let mut headers = standard_headers(true);
        headers.insert(ORIGIN, origin_header(origin)?);
        Ok(PreparedHumanLogin {
            credential_key,
            request: HttpRequest {
                method: Method::POST,
                url: self.session_url,
                headers,
                body,
            },
        })
    }

    pub(crate) fn write_signing_request(
        &self,
        output: impl io::Write,
    ) -> Result<(), serde_json::Error> {
        #[derive(Serialize)]
        struct UnsignedEvent<'a> {
            kind: u64,
            created_at: i64,
            tags: [[&'a str; 2]; 3],
            content: &'a str,
        }
        serde_json::to_writer(
            output,
            &UnsignedEvent {
                kind: 27_235,
                created_at: self.created_at,
                tags: [
                    ["u", self.session_url.as_str()],
                    ["method", "POST"],
                    ["challenge", self.challenge.challenge.expose_secret()],
                ],
                content: "",
            },
        )
    }

    fn session_request(
        &self,
        proof: &SecretString,
        now: OffsetDateTime,
    ) -> Result<RequestBody, AdminClientError> {
        if now >= self.challenge.expires_at || now.unix_timestamp().abs_diff(self.created_at) > 60 {
            return Err(AdminClientError::NostrLoginProof {
                message: "the signing challenge expired; start login-nostr again",
            });
        }
        validate_human_login_proof(proof.expose_secret(), self.session_url.as_str(), self.challenge.challenge.expose_secret(), self.created_at)
            .map_err(|_| AdminClientError::NostrLoginProof { message: "the signed event is invalid or changed the requested login; no session request was sent" })?;
        #[derive(Serialize)]
        struct LoginRequest<'a> {
            provider: HumanLoginProvider,
            challenge_id: LoginChallengeId,
            challenge: &'a SecretString,
            event: &'a str,
        }
        RequestBody::json(&LoginRequest {
            provider: HumanLoginProvider::Nostr,
            challenge_id: self.challenge.challenge_id,
            challenge: &self.challenge.challenge,
            event: proof.expose_secret(),
        })
        .map_err(AdminClientError::RequestEncoding)
    }
}

impl AdminClient {
    pub(crate) async fn begin_nostr_login(&self) -> Result<HumanNostrLogin, AdminClientError> {
        let request = prepare_challenge_request(&self.origin, self.authentication, |key| {
            self.credentials.load(key)
        })?;
        let response = self.execute(request, 8192).await?;
        HumanNostrLogin::from_response(&self.origin, response, OffsetDateTime::now_utc())
    }

    pub(crate) async fn complete_nostr_login(
        &self,
        login: HumanNostrLogin,
        proof: SecretString,
    ) -> Result<AdminSessionResponse, AdminClientError> {
        let PreparedHumanLogin {
            credential_key,
            request,
        } = login.prepare_submission(&self.origin, proof, OffsetDateTime::now_utc(), |key| {
            self.credentials.load(key)
        })?;
        let response = self.execute(request, MAX_JSON_RESPONSE_BYTES).await?;
        complete_human_login(
            &self.origin,
            response,
            credential_key,
            |key, value| self.credentials.save(key, value),
            |request| self.execute(request, MAX_JSON_RESPONSE_BYTES),
        )
        .await
    }
}

fn prepare_challenge_request(
    origin: &AdminOrigin,
    context: AuthenticationContext,
    load: impl FnOnce(&CredentialKey) -> Result<Option<SecretValue>, CredentialStoreError>,
) -> Result<HttpRequest, AdminClientError> {
    if context != AuthenticationContext::Human {
        return Err(AdminClientError::HumanContextRequired);
    }
    if load(&CredentialKey::human(origin.as_str()))?.is_some() {
        return Err(AdminClientError::HumanSessionAlreadyStored);
    }
    let mut headers = standard_headers(true);
    headers.insert(ORIGIN, origin_header(origin)?);
    Ok(HttpRequest {
        method: Method::POST,
        url: origin.request_url(LOGIN_CHALLENGES_PATH)?,
        headers,
        body: RequestBody::json(&CreateLoginChallengeRequest {
            provider: HumanLoginProvider::Nostr,
        })
        .map_err(AdminClientError::RequestEncoding)?,
    })
}

fn invalid_challenge() -> AdminClientError {
    AdminClientError::InvalidAuthenticationResponse {
        message: "the Nostr challenge has invalid provider, size, or expiry",
    }
}

fn validate_challenge(
    challenge: &CreateLoginChallengeResponse,
    now: OffsetDateTime,
) -> Result<(), AdminClientError> {
    let value = challenge.challenge.expose_secret();
    if challenge.provider != HumanLoginProvider::Nostr
        || !(1..=512).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        || challenge.expires_at <= now
        || challenge.expires_at > now + time::Duration::minutes(5)
    {
        return Err(invalid_challenge());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use k256::schnorr::SigningKey;
    use serde_json::json;
    use sha2::{Digest as _, Sha256};
    use uuid::Uuid;

    use super::*;

    fn login() -> HumanNostrLogin {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        HumanNostrLogin {
            challenge: CreateLoginChallengeResponse {
                challenge_id: Uuid::from_u128(1).into(),
                provider: HumanLoginProvider::Nostr,
                challenge: SecretString::new("mcl1_fixture"),
                expires_at: now + time::Duration::minutes(5),
            },
            session_url: Url::parse("https://admin.example.test:8443/api/admin/v1/auth/sessions")
                .unwrap(),
            created_at: now.unix_timestamp(),
        }
    }

    fn sign(login: &HumanNostrLogin) -> SecretString {
        let mut bytes = Vec::new();
        login.write_signing_request(&mut bytes).unwrap();
        let mut event: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let key = SigningKey::from_bytes(&[3; 32]).unwrap();
        let hex = |value: &[u8]| {
            value
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        event["pubkey"] = json!(hex(&key.verifying_key().to_bytes()));
        let canonical = serde_json::to_vec(&(
            0,
            &event["pubkey"],
            &event["created_at"],
            &event["kind"],
            &event["tags"],
            &event["content"],
        ))
        .unwrap();
        let digest: [u8; 32] = Sha256::digest(canonical).into();
        event["id"] = json!(hex(&digest));
        event["sig"] = json!(hex(&key.sign_raw(&digest, &[0; 32]).unwrap().to_bytes()));
        SecretString::new(event.to_string().into_boxed_str())
    }

    #[test]
    fn challenge_response_validation_checks_http_metadata_before_binding_the_origin() {
        let origin = AdminOrigin::parse("https://admin.example.test:8443").unwrap();
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let response = || HttpResponse {
            status: StatusCode::CREATED,
            headers: reqwest::header::HeaderMap::from_iter([(
                reqwest::header::CONTENT_TYPE,
                reqwest::header::HeaderValue::from_static("application/json"),
            )]),
            body: serde_json::to_vec(&login().challenge).unwrap(),
        };
        let bound = HumanNostrLogin::from_response(&origin, response(), now).unwrap();
        assert_eq!(
            bound.session_url.as_str(),
            "https://admin.example.test:8443/api/admin/v1/auth/sessions"
        );
        for invalid in 0..4 {
            let mut response = response();
            match invalid {
                0 => response.status = StatusCode::OK,
                1 => response.headers.clear(),
                2 => response.body = b"not JSON".to_vec(),
                _ => response.body = vec![b' '; 8193],
            }
            assert!(HumanNostrLogin::from_response(&origin, response, now).is_err());
        }
    }

    #[test]
    fn signed_submission_rechecks_the_human_store_and_binds_a_complete_secret_request() {
        let origin = AdminOrigin::parse("https://admin.example.test:8443").unwrap();
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let challenge = login();
        let proof = sign(&challenge);
        let prepared = challenge
            .prepare_submission(&origin, proof, now, |key| {
                assert_eq!(key, &CredentialKey::human(origin.as_str()));
                Ok(None)
            })
            .unwrap();
        assert_eq!(
            prepared.credential_key,
            CredentialKey::human(origin.as_str())
        );
        assert_eq!(prepared.request.method, Method::POST);
        assert_eq!(prepared.request.headers[ORIGIN], origin.as_str());
        assert!(!prepared.request.headers.contains_key("authorization"));
        assert!(!prepared.request.headers.contains_key("cookie"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(prepared.request.body.as_ref()).unwrap()["provider"],
            "nostr"
        );
        let challenge = login();
        let proof = sign(&challenge);
        assert!(matches!(
            challenge.prepare_submission(&origin, proof, now, |_| Ok(Some(SecretValue::new(
                "stored human session"
            )))),
            Err(AdminClientError::HumanSessionAlreadyStored)
        ));
        let other = AdminOrigin::parse("https://other.example.test").unwrap();
        assert!(matches!(
            login().prepare_submission(&other, SecretString::new("not used"), now, |_| panic!(
                "origin mismatch must not read credentials"
            )),
            Err(AdminClientError::InvalidRequestTarget)
        ));
    }

    #[test]
    fn human_nostr_login_checks_only_the_origin_bound_human_store_and_sends_no_saved_credentials() {
        let origin = AdminOrigin::parse("https://admin.example.test:8443").unwrap();
        let request = prepare_challenge_request(&origin, AuthenticationContext::Human, |key| {
            assert_eq!(key, &CredentialKey::human(origin.as_str()));
            Ok(None)
        })
        .unwrap();
        assert_eq!(request.method, Method::POST);
        assert_eq!(
            request.url.as_str(),
            "https://admin.example.test:8443/api/admin/v1/auth/challenges"
        );
        assert_eq!(request.headers[ORIGIN], origin.as_str());
        assert!(!request.headers.contains_key("authorization"));
        assert!(!request.headers.contains_key("cookie"));
        assert_eq!(request.body.as_ref(), br#"{"provider":"nostr"}"#);
        assert!(matches!(
            prepare_challenge_request(&origin, AuthenticationContext::Agent, |_| panic!(
                "agent context must not load any private key"
            )),
            Err(AdminClientError::HumanContextRequired)
        ));
        assert!(matches!(
            prepare_challenge_request(&origin, AuthenticationContext::Human, |_| Ok(Some(
                SecretValue::new("existing human session")
            ))),
            Err(AdminClientError::HumanSessionAlreadyStored)
        ));
    }

    #[test]
    fn external_human_signing_request_binds_the_exact_origin_and_creates_the_human_wire_proof() {
        let login = login();
        let mut output = Vec::new();
        login.write_signing_request(&mut output).unwrap();
        let event: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            event,
            json!({"kind":27235, "created_at":1800000000_i64, "content":"", "tags":[["u", "https://admin.example.test:8443/api/admin/v1/auth/sessions"], ["method","POST"], ["challenge","mcl1_fixture"]]})
        );
        let proof = sign(&login);
        let body = login
            .session_request(
                &proof,
                OffsetDateTime::from_unix_timestamp(login.created_at + 30).unwrap(),
            )
            .unwrap();
        let wire: serde_json::Value = serde_json::from_slice(body.as_ref()).unwrap();
        assert_eq!(wire["provider"], "nostr");
        assert_eq!(wire["challenge_id"], Uuid::from_u128(1).to_string());
        assert_eq!(wire["challenge"], "mcl1_fixture");
        assert_eq!(wire["event"], proof.expose_secret());
        assert!(!String::from_utf8_lossy(body.as_ref()).contains("private_key"));
    }

    #[test]
    fn expired_or_altered_human_proofs_fail_locally_with_safe_errors() {
        let login = login();
        let proof = sign(&login);
        for now in [
            login.created_at + 61,
            login.challenge.expires_at.unix_timestamp(),
        ] {
            let error = login
                .session_request(&proof, OffsetDateTime::from_unix_timestamp(now).unwrap())
                .err()
                .unwrap();
            assert!(matches!(error, AdminClientError::NostrLoginProof { .. }));
            assert!(!format!("{error:?}").contains("mcl1_fixture"));
        }
        let error = login
            .session_request(
                &SecretString::new("malformed signed event"),
                OffsetDateTime::from_unix_timestamp(login.created_at).unwrap(),
            )
            .err()
            .unwrap();
        assert!(!format!("{error:?}").contains("malformed signed event"));
    }

    #[test]
    fn login_challenge_validation_rejects_wrong_provider_bounds_and_expiry() {
        let login = login();
        let now = OffsetDateTime::from_unix_timestamp(login.created_at).unwrap();
        validate_challenge(&login.challenge, now).unwrap();
        for (provider, value, expires_at) in [
            (
                HumanLoginProvider::Password,
                "mcl1_fixture".to_string(),
                now + time::Duration::minutes(1),
            ),
            (
                HumanLoginProvider::Nostr,
                "".to_string(),
                now + time::Duration::minutes(1),
            ),
            (
                HumanLoginProvider::Nostr,
                "a".repeat(513),
                now + time::Duration::minutes(1),
            ),
            (
                HumanLoginProvider::Nostr,
                "bad\nchallenge".to_string(),
                now + time::Duration::minutes(1),
            ),
            (HumanLoginProvider::Nostr, "mcl1_fixture".to_string(), now),
            (
                HumanLoginProvider::Nostr,
                "mcl1_fixture".to_string(),
                now + time::Duration::minutes(6),
            ),
        ] {
            let challenge = CreateLoginChallengeResponse {
                challenge_id: login.challenge.challenge_id,
                provider,
                challenge: SecretString::new(value.into_boxed_str()),
                expires_at,
            };
            assert!(matches!(
                validate_challenge(&challenge, now),
                Err(AdminClientError::InvalidAuthenticationResponse { .. })
            ));
        }
    }
}
