use std::collections::BTreeSet;

use maincopy_shared::auth_api::{
    ADMIN_AGENT_CREDENTIAL_PATH, ADMIN_AGENT_CREDENTIALS_PATH, ADMIN_AGENT_SCOPES_PATH,
    AgentCredentialMutationResponse, AgentCredentialResponse, ExpectedVersionRequest,
    ListAgentCredentialsResponse, MAX_IDENTITY_PAGE_LIMIT, RegisterAgentCredentialRequest,
    ReplaceAgentScopesRequest,
};
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use uuid::Uuid;

use super::{AdminClient, AdminClientError, AdminOrigin, decode_status_json};
use crate::nip98::inspect_public_key;

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum AgentMutation {
    Register(RegisterAgentCredentialRequest),
    Scopes {
        #[serde(skip)]
        agent_id: Uuid,
        #[serde(flatten)]
        request: ReplaceAgentScopesRequest,
    },
    Revoke {
        #[serde(skip)]
        agent_id: Uuid,
        #[serde(flatten)]
        request: ExpectedVersionRequest,
    },
}

impl AdminClient {
    pub(crate) async fn list_agents(
        &self,
        cursor: Option<Uuid>,
    ) -> Result<ListAgentCredentialsResponse, AdminClientError> {
        let page = self.get_json(agents_url(&self.origin, cursor)?).await?;
        validate_page(page, cursor)
    }

    pub(crate) async fn inspect_agent(
        &self,
        agent_id: Uuid,
    ) -> Result<AgentCredentialResponse, AdminClientError> {
        let path =
            ADMIN_AGENT_CREDENTIAL_PATH.replace("{agent_credential_id}", &agent_id.to_string());
        let agent: AgentCredentialResponse = self.get_json(self.origin.request_url(&path)?).await?;
        validate_inspected_agent(agent, agent_id)
    }

    pub(crate) async fn change_agent(
        &self,
        operation: Uuid,
        mutation: &AgentMutation,
    ) -> Result<AgentCredentialMutationResponse, AdminClientError> {
        let (method, path, status) = mutation.route();
        let response = self
            .json_mutation(method, &path, mutation, operation)
            .await?;
        mutation.accept_receipt(decode_status_json(response, status)?)
    }
}

impl AgentMutation {
    fn route(&self) -> (Method, String, StatusCode) {
        match self {
            Self::Register(_) => (
                Method::POST,
                ADMIN_AGENT_CREDENTIALS_PATH.to_owned(),
                StatusCode::CREATED,
            ),
            Self::Scopes { agent_id, .. } => (
                Method::PUT,
                ADMIN_AGENT_SCOPES_PATH.replace("{agent_credential_id}", &agent_id.to_string()),
                StatusCode::OK,
            ),
            Self::Revoke { agent_id, .. } => (
                Method::DELETE,
                ADMIN_AGENT_CREDENTIAL_PATH.replace("{agent_credential_id}", &agent_id.to_string()),
                StatusCode::OK,
            ),
        }
    }

    fn accept_receipt(
        &self,
        receipt: AgentCredentialMutationResponse,
    ) -> Result<AgentCredentialMutationResponse, AdminClientError> {
        match self {
            Self::Register(_) => {
                if receipt.version != 1 {
                    return Err(invalid_response());
                }
                Ok(receipt)
            }
            Self::Scopes { agent_id, request } => {
                validate_receipt(receipt, *agent_id, request.expected_version)
            }
            Self::Revoke { agent_id, request } => {
                validate_receipt(receipt, *agent_id, request.expected_version)
            }
        }
    }
}

fn agents_url(origin: &AdminOrigin, cursor: Option<Uuid>) -> Result<Url, AdminClientError> {
    let mut url = origin.request_url(ADMIN_AGENT_CREDENTIALS_PATH)?;
    url.query_pairs_mut()
        .append_pair("limit", &MAX_IDENTITY_PAGE_LIMIT.to_string());
    if let Some(cursor) = cursor {
        url.query_pairs_mut()
            .append_pair("cursor", &cursor.to_string());
    }
    Ok(url)
}

fn invalid_response() -> AdminClientError {
    AdminClientError::InvalidIdentityResponse {
        message: "agent identifiers, public keys, versions, scopes, or pagination are inconsistent",
    }
}

fn validate_inspected_agent(
    agent: AgentCredentialResponse,
    expected: Uuid,
) -> Result<AgentCredentialResponse, AdminClientError> {
    if agent.agent_credential_id.into_uuid() != expected {
        return Err(invalid_response());
    }
    validate_agent(&agent)?;
    Ok(agent)
}

fn validate_agent(agent: &AgentCredentialResponse) -> Result<(), AdminClientError> {
    if agent.version == 0 || inspect_public_key(&agent.public_key).is_err() {
        return Err(invalid_response());
    }
    let requested: BTreeSet<_> = agent.scopes.iter().copied().collect();
    let effective: BTreeSet<_> = agent.effective_scopes.iter().copied().collect();
    if requested.is_empty()
        || requested.len() != agent.scopes.len()
        || effective.len() != agent.effective_scopes.len()
        || !effective.is_subset(&requested)
    {
        return Err(invalid_response());
    }
    Ok(())
}

fn validate_page(
    page: ListAgentCredentialsResponse,
    cursor: Option<Uuid>,
) -> Result<ListAgentCredentialsResponse, AdminClientError> {
    if page.agent_credentials.len() > usize::from(MAX_IDENTITY_PAGE_LIMIT) {
        return Err(invalid_response());
    }
    let mut previous = cursor;
    for agent in &page.agent_credentials {
        validate_agent(agent)?;
        let current = agent.agent_credential_id.into_uuid();
        if previous.is_some_and(|previous| previous >= current) {
            return Err(invalid_response());
        }
        previous = Some(current);
    }
    if let Some(next) = page.next_cursor
        && page
            .agent_credentials
            .last()
            .map(|agent| agent.agent_credential_id)
            != Some(next)
    {
        return Err(invalid_response());
    }
    Ok(page)
}

fn validate_receipt(
    receipt: AgentCredentialMutationResponse,
    agent_id: Uuid,
    expected_version: u64,
) -> Result<AgentCredentialMutationResponse, AdminClientError> {
    if receipt.agent_credential_id.into_uuid() != agent_id
        || expected_version.checked_add(1) != Some(receipt.version)
    {
        return Err(invalid_response());
    }
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use maincopy_shared::auth::{AdminScope, AgentCredentialId};
    use serde_json::json;

    fn grant(id: u128) -> AgentCredentialResponse {
        serde_json::from_value(json!({"agent_credential_id":Uuid::from_u128(id), "owner_user_id":Uuid::from_u128(1), "issuer_user_id":Uuid::from_u128(2), "public_key":"f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9", "label":"helper", "scopes":["content_read"], "effective_scopes":["content_read"], "version":1, "created_at":"2026-09-06T12:00:00Z", "expires_at":null, "last_used_at":null, "revoked_at":null})).unwrap()
    }

    #[test]
    fn agent_pages_preserve_origin_and_reject_backwards_unbounded_and_invalid_metadata() {
        let origin = AdminOrigin::parse("https://admin.example.test:8443").unwrap();
        let first = agents_url(&origin, None).unwrap();
        assert_eq!(
            first.as_str(),
            "https://admin.example.test:8443/api/admin/v1/identity/agents?limit=100"
        );
        let url = agents_url(&origin, Some(Uuid::from_u128(1))).unwrap();
        assert_eq!(url.origin().ascii_serialization(), origin.as_str());
        assert_eq!(
            url.query_pairs().last().unwrap().1,
            Uuid::from_u128(1).to_string()
        );
        let page = ListAgentCredentialsResponse {
            agent_credentials: vec![grant(2), grant(3)],
            next_cursor: Some(AgentCredentialId::from_uuid(Uuid::from_u128(3))),
        };
        assert_eq!(
            validate_page(page.clone(), Some(Uuid::from_u128(1))).unwrap(),
            page
        );
        assert!(validate_page(page.clone(), Some(Uuid::from_u128(2))).is_err());
        let mut backwards = page.clone();
        backwards.agent_credentials.reverse();
        let mut duplicate = page.clone();
        duplicate.agent_credentials[1] = grant(2);
        let mut next = page.clone();
        next.next_cursor = Some(AgentCredentialId::from_uuid(Uuid::from_u128(4)));
        let mut empty_next = page.clone();
        empty_next.agent_credentials.clear();
        let mut large = page.clone();
        large.agent_credentials = (1..=101).map(grant).collect();
        let mut key = page.clone();
        key.agent_credentials[0].public_key = "invalid".into();
        for bad in [backwards, duplicate, next, empty_next, large, key] {
            assert!(validate_page(bad, None).is_err());
        }
        assert!(
            validate_page(
                ListAgentCredentialsResponse {
                    agent_credentials: vec![],
                    next_cursor: None
                },
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn agent_metadata_requires_valid_versions_keys_and_effective_scope_intersections() {
        let mut agent = grant(1);
        assert!(validate_agent(&agent).is_ok());
        agent.effective_scopes.clear();
        assert!(validate_agent(&agent).is_ok());
        agent.effective_scopes = vec![AdminScope::ReleaseManage];
        assert!(validate_agent(&agent).is_err());
        agent.effective_scopes = vec![AdminScope::ContentRead; 2];
        assert!(validate_agent(&agent).is_err());
        agent.effective_scopes.clear();
        agent.scopes = vec![AdminScope::ContentRead; 2];
        assert!(validate_agent(&agent).is_err());
        agent.scopes.clear();
        assert!(validate_agent(&agent).is_err());
        let mut agent = grant(1);
        agent.version = 0;
        assert!(validate_agent(&agent).is_err());
        let mut agent = grant(1);
        agent.public_key = "ff".repeat(32).into();
        assert!(validate_agent(&agent).is_err());
    }

    #[test]
    fn agent_receipts_bind_the_grant_and_next_version() {
        let id = Uuid::from_u128(1);
        let receipt = AgentCredentialMutationResponse {
            agent_credential_id: id.into(),
            version: 3,
        };
        assert_eq!(validate_receipt(receipt, id, 2).unwrap(), receipt);
        assert!(validate_receipt(receipt, Uuid::from_u128(2), 2).is_err());
        assert!(validate_receipt(receipt, id, 1).is_err());
        assert!(validate_receipt(receipt, id, u64::MAX).is_err());
    }
    #[test]
    fn agent_mutations_bind_exact_routes_bodies_status_and_receipt_versions() {
        let id = Uuid::from_u128(1);
        let registration = RegisterAgentCredentialRequest {
            owner_user_id: id.into(),
            public_key: grant(1).public_key,
            label: "helper".into(),
            scopes: vec![AdminScope::ContentRead],
            expires_at: None,
        };
        let expected_registration = serde_json::to_value(&registration).unwrap();
        let commands = [
            (
                AgentMutation::Register(registration),
                Method::POST,
                ADMIN_AGENT_CREDENTIALS_PATH.to_owned(),
                StatusCode::CREATED,
                expected_registration,
                1,
            ),
            (
                AgentMutation::Scopes {
                    agent_id: id,
                    request: ReplaceAgentScopesRequest {
                        expected_version: 3,
                        scopes: vec![AdminScope::ContentRead],
                    },
                },
                Method::PUT,
                format!("{ADMIN_AGENT_CREDENTIALS_PATH}/{id}/scopes"),
                StatusCode::OK,
                json!({"expected_version":3, "scopes":["content_read"]}),
                4,
            ),
            (
                AgentMutation::Revoke {
                    agent_id: id,
                    request: ExpectedVersionRequest {
                        expected_version: 3,
                    },
                },
                Method::DELETE,
                format!("{ADMIN_AGENT_CREDENTIALS_PATH}/{id}"),
                StatusCode::OK,
                json!({"expected_version":3}),
                4,
            ),
        ];
        for (command, method, path, status, body, version) in commands {
            assert_eq!(command.route(), (method, path, status));
            assert_eq!(serde_json::to_value(&command).unwrap(), body);
            let receipt = AgentCredentialMutationResponse {
                agent_credential_id: id.into(),
                version,
            };
            assert_eq!(command.accept_receipt(receipt).unwrap(), receipt);
            assert!(
                command
                    .accept_receipt(AgentCredentialMutationResponse {
                        version: version + 1,
                        ..receipt
                    })
                    .is_err()
            );
        }
    }
    #[test]
    fn inspected_agent_metadata_must_match_the_requested_grant() {
        let id = Uuid::from_u128(1);
        assert_eq!(validate_inspected_agent(grant(1), id).unwrap(), grant(1));
        assert!(validate_inspected_agent(grant(2), id).is_err());
        let mut invalid = grant(1);
        invalid.public_key = "invalid".into();
        assert!(validate_inspected_agent(invalid, id).is_err());
    }
}
