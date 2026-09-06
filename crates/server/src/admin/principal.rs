use std::{collections::BTreeSet, sync::Arc};

use axum::{
    Extension,
    extract::{FromRequestParts, rejection::ExtensionRejection},
    http::request::Parts,
};
use maincopy_shared::auth::{
    AdminAuditEventId, AdminScope, AdminSessionId, AgentCredentialId, UserId,
};
use uuid::Uuid;

use super::request_id::RequestId;
use crate::domain::auth::store::{AdminMutationKey, AuditPrincipalReference, MutationAuditContext};

/// Fully resolved authority for one authenticated admin request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AdminPrincipal {
    pub(crate) user_id: UserId,
    pub(crate) scopes: Arc<BTreeSet<AdminScope>>,
    pub(crate) authentication: AdminAuthentication,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdminAuthentication {
    BrowserSession { session_id: AdminSessionId },
    AgentCredential { credential_id: AgentCredentialId },
}

impl AdminPrincipal {
    pub(crate) fn allows(&self, scope: AdminScope) -> bool {
        self.scopes.contains(&scope)
    }

    pub(crate) fn mutation_audit(
        &self,
        request_id: RequestId,
        idempotency_key: AdminMutationKey,
    ) -> MutationAuditContext {
        let principal = match self.authentication {
            AdminAuthentication::BrowserSession { session_id } => {
                AuditPrincipalReference::BrowserSession {
                    user_id: self.user_id,
                    session_id,
                }
            }
            AdminAuthentication::AgentCredential { credential_id } => {
                AuditPrincipalReference::AgentCredential {
                    user_id: self.user_id,
                    credential_id,
                }
            }
        };
        MutationAuditContext {
            audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
            principal,
            request_id: Some(request_id.0),
            idempotency_key,
        }
    }
}

impl<S> FromRequestParts<S> for AdminPrincipal
where
    S: Send + Sync,
{
    type Rejection = ExtensionRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Extension(principal) = Extension::<Self>::from_request_parts(parts, state).await?;
        Ok(principal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, http::StatusCode, routing::get};
    use tower::ServiceExt as _;

    #[test]
    fn mutation_audit_retains_the_authenticated_actor_and_retry_identity() {
        let user_id = UserId::from_uuid(Uuid::new_v4());
        let session_id = AdminSessionId::from_uuid(Uuid::new_v4());
        let credential_id = AgentCredentialId::from_uuid(Uuid::new_v4());
        let request_id = RequestId(Uuid::new_v4());
        let key = AdminMutationKey(Uuid::new_v4());
        for (authentication, expected) in [
            (
                AdminAuthentication::BrowserSession { session_id },
                AuditPrincipalReference::BrowserSession {
                    user_id,
                    session_id,
                },
            ),
            (
                AdminAuthentication::AgentCredential { credential_id },
                AuditPrincipalReference::AgentCredential {
                    user_id,
                    credential_id,
                },
            ),
        ] {
            let principal = AdminPrincipal {
                user_id,
                scopes: Arc::new(BTreeSet::new()),
                authentication,
            };
            let audit = principal.mutation_audit(request_id, key);
            assert_eq!(audit.principal, expected);
            assert_eq!(audit.request_id, Some(request_id.0));
            assert_eq!(audit.idempotency_key, key);
            assert_ne!(
                audit.audit_event_id,
                principal.mutation_audit(request_id, key).audit_event_id
            );
        }
    }

    #[test]
    fn principals_expose_only_their_resolved_scope_set() {
        let user_id = UserId::from_uuid(Uuid::new_v4());
        let session_id = AdminSessionId::from_uuid(Uuid::new_v4());
        let principal = AdminPrincipal {
            user_id,
            scopes: Arc::new(BTreeSet::from([
                AdminScope::ContentRead,
                AdminScope::PreviewRead,
            ])),
            authentication: AdminAuthentication::BrowserSession { session_id },
        };

        assert_eq!(principal.user_id, user_id);
        assert_eq!(
            principal.authentication,
            AdminAuthentication::BrowserSession { session_id }
        );
        assert!(principal.allows(AdminScope::PreviewRead));
        assert!(!principal.allows(AdminScope::ReleaseManage));
    }

    #[tokio::test]
    async fn principal_extracts_the_middleware_resolved_actor() {
        let user_id = UserId::from_uuid(Uuid::new_v4());
        let session_id = AdminSessionId::from_uuid(Uuid::new_v4());
        let principal = AdminPrincipal {
            user_id,
            scopes: Arc::new(BTreeSet::from([AdminScope::ContentRead])),
            authentication: AdminAuthentication::BrowserSession { session_id },
        };
        async fn handler(principal: AdminPrincipal) -> StatusCode {
            assert!(principal.allows(AdminScope::ContentRead));
            StatusCode::NO_CONTENT
        }

        let response = Router::new()
            .route("/", get(handler))
            .layer(Extension(principal))
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn missing_principal_remains_a_server_wiring_error() {
        async fn handler(_: AdminPrincipal) -> StatusCode {
            StatusCode::NO_CONTENT
        }

        let response = Router::new()
            .route("/", get(handler))
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
