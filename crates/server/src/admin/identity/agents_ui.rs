use std::{collections::BTreeSet, str::FromStr};

use axum::{
    Extension, Form, Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query,
        rejection::{FormRejection, PathRejection, QueryRejection},
    },
    http::StatusCode,
    middleware,
    response::Response,
    routing::{get, post},
};
use maincopy_shared::{
    auth::{AdminScope, AgentCredentialId, UserId},
    auth_api::{
        AgentCredentialResponse, ExpectedVersionRequest, RegisterAgentCredentialRequest,
        ReplaceAgentScopesRequest, SecretString,
    },
};
use maud::{Markup, html};
use serde::Deserialize;
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use uuid::Uuid;

use super::{
    PageQuery, agent_credential_path, agent_response, identity_page, load_problem, not_found,
    register_agent_credential, replace_agent_scopes, revoke_agent_credential,
    ui::{mutation_fields, mutation_response, operation_headers, registered_nostr_key},
};
use crate::admin::{
    AdminRuntimeState, AdminSecurityState, BrowserFormSession, BrowserSessionContext,
    RequiredBrowserSession, browser_scoped_router,
    principal::AdminPrincipal,
    request_id::RequestId,
    ui::{
        PageKind, adapt_security_response, error_response, mutation_error_response, page_response,
    },
};

const AGENT_FORM_LIMIT: usize = 4096;

pub(super) fn browser_router(security: &AdminSecurityState) -> Router<AdminRuntimeState> {
    browser_scoped_router(
        Router::new()
            .route("/admin/agents", get(show_agents).post(register_agent))
            .route("/admin/agents/{agent_id}", get(show_agent))
            .route("/admin/agents/{agent_id}/scopes", post(save_scopes))
            .route("/admin/agents/{agent_id}/revoke", post(revoke_agent)),
        security,
        AdminScope::CredentialManage,
    )
    .layer(DefaultBodyLimit::max(AGENT_FORM_LIMIT))
    .layer(middleware::from_fn(adapt_security_response))
}

async fn show_agents(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    principal: AdminPrincipal,
    browser: BrowserFormSession,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let (cursor, limit) = match identity_page(query, request_id) {
        Ok(page) => page,
        Err(response) => return mutation_response(response, "/admin/agents", request_id),
    };
    let page = match security
        .store
        .agent_credentials_page(cursor.map(AgentCredentialId::from_uuid), limit)
        .await
    {
        Ok(page) => page,
        Err(error) => {
            return mutation_response(load_problem(error, request_id), "/admin/agents", request_id);
        }
    };
    let fresh = OffsetDateTime::now_utc() < browser.session.fresh_until;
    page_response(
        StatusCode::OK,
        "Agents",
        PageKind::Authenticated,
        html! {
            section class="panel" {
                h1 { "Agents" }
                p { "Grants delegate selected permissions to a public key. Account roles still limit each grant's effective scopes." }
                @if page.items.is_empty() { p { "No agent grants on this page." } }
                @for agent in &page.items {
                    article class="panel" {
                        h2 { a href=(format!("/admin/agents/{}", agent.credential_id)) { (agent.label) } }
                        p { code { (agent.credential_id) } " · version " (agent.version) }
                        p { "Owner: " code { (agent.owner_user_id) } }
                    }
                }
                @if let Some(cursor) = page.next_cursor { a class="button" href=(format!("/admin/agents?cursor={cursor}")) { "Next page" } }
            }
            section class="panel" {
                h2 { "Register an agent" }
                (fresh_notice(fresh))
                form method="post" action="/admin/agents" {
                    fieldset disabled[!fresh] {
                        legend { "New grant" }
                        (mutation_fields(&browser, None))
                        label for="owner-user-id" { "Owner account UUID" }
                        input id="owner-user-id" name="owner_user_id" required value=(browser.session.user_id);
                        label for="agent-label" { "Label (up to 96 bytes)" }
                        input id="agent-label" name="label" required maxlength="96";
                        label for="agent-public-key" { "Public key (64 lowercase hexadecimal characters)" }
                        input id="agent-public-key" name="public_key" required minlength="64" maxlength="64";
                        label for="agent-expiry" { "Expiry in UTC (optional, for example 2026-12-31T23:59:59Z)" }
                        input id="agent-expiry" name="expires_at" maxlength="40";
                        (scope_choices(&principal, &[]))
                        button type="submit" { "Register agent" }
                    }
                }
                p { "Inspect your local agent key with maincopy agent-key inspect, then copy its public key here." }
            }
        },
    )
}

async fn show_agent(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    principal: AdminPrincipal,
    browser: BrowserFormSession,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let id = match agent_credential_path(path, request_id) {
        Ok(id) => id,
        Err(response) => return mutation_response(response, "/admin/agents", request_id),
    };
    let agent = match security.store.agent_credential_by_id(id).await {
        Ok(Some(agent)) => agent_response(&agent),
        Ok(None) => return mutation_response(not_found(request_id), "/admin/agents", request_id),
        Err(error) => {
            return mutation_response(load_problem(error, request_id), "/admin/agents", request_id);
        }
    };
    page_response(
        StatusCode::OK,
        "Agent grant",
        PageKind::Authenticated,
        agent_page(&agent, &principal, &browser, OffsetDateTime::now_utc()),
    )
}

fn agent_page(
    agent: &AgentCredentialResponse,
    principal: &AdminPrincipal,
    browser: &BrowserFormSession,
    now: OffsetDateTime,
) -> Markup {
    let fresh = now < browser.session.fresh_until;
    let revoked = agent.revoked_at.is_some();
    let expired = agent.expires_at.is_some_and(|expiry| now >= expiry);
    html! {
        section class="panel" {
            h1 { (agent.label) }
            p { a href="/admin/agents" { "All agents" } }
            dl {
                dt { "Grant UUID" } dd { code { (agent.agent_credential_id) } }
                dt { "Version" } dd { (agent.version) }
                dt { "Owner account" } dd { code { (agent.owner_user_id) } }
                dt { "Issuer account" } dd { code { (agent.issuer_user_id) } }
                dt { "Created" } dd { (agent.created_at) }
                dt { "Expires" } dd { @match agent.expires_at { Some(at) => { (at) }, None => { "No expiry" } } }
                dt { "Last used" } dd { @match agent.last_used_at { Some(at) => { (at) }, None => { "Never" } } }
                dt { "Revoked" } dd { @match agent.revoked_at { Some(at) => { (at) }, None => { "No" } } }
                dt { "Requested scopes" } dd { (scope_list(&agent.scopes)) }
                dt { "Effective scopes" } dd { (scope_list(&agent.effective_scopes)) }
            }
            (registered_nostr_key(&agent.public_key))
            @if revoked { p class="notice" { "This grant is revoked. Register a new key to restore agent access." } }
            @if expired { p class="notice" { "This grant has expired and cannot authenticate." } }
            p { "Effective scopes intersect this grant with the owner's current account roles. Expired or revoked grants cannot authenticate." }
        }
        @if !revoked {
            section class="panel" {
                h2 { "Manage grant" }
                (fresh_notice(fresh))
                form method="post" action=(format!("/admin/agents/{}/scopes", agent.agent_credential_id)) {
                    fieldset disabled[!fresh || expired] {
                        legend { "Replace all requested scopes" }
                        (mutation_fields(browser, Some(agent.version)))
                        (scope_choices(principal, &agent.scopes))
                        button type="submit" { "Replace scopes" }
                    }
                }
                form method="post" action=(format!("/admin/agents/{}/revoke", agent.agent_credential_id)) {
                    fieldset disabled[!fresh] {
                        legend { "Revoke this grant" }
                        (mutation_fields(browser, Some(agent.version)))
                        label { input type="checkbox" name="confirm" value="true" required; "Revoke this agent's access" }
                        button type="submit" { "Revoke agent" }
                    }
                }
            }
        }
    }
}

fn scope_list(scopes: &[AdminScope]) -> Markup {
    html! { @if scopes.is_empty() { "None" } @for scope in scopes { code { (scope.as_str()) } " " } }
}

fn scope_choices(principal: &AdminPrincipal, selected: &[AdminScope]) -> Markup {
    html! {
        fieldset {
            legend { "Requested scopes (select at least one)" }
            @for scope in &*principal.scopes {
                label { input type="checkbox" name="scope" value=(scope.as_str()) checked[selected.contains(scope)]; (scope.as_str()) }
            }
        }
    }
}

fn fresh_notice(fresh: bool) -> Markup {
    html! { @if !fresh { p class="notice" { "Changing agent grants requires a recent sign-in. " a href="/admin/login" { "Sign in again" } } } }
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Field {
    #[serde(rename = "_csrf")]
    Csrf,
    OperationId,
    OwnerUserId,
    PublicKey,
    Label,
    Scope,
    ExpiresAt,
    ExpectedVersion,
}

// Native checkboxes repeat the same field name. Decode a bounded pair list,
// reject duplicate scalar fields, and consume every accepted field explicitly.
struct AgentFields(Vec<(Field, SecretString)>);

#[derive(Debug, thiserror::Error)]
#[error("agent form fields are missing, duplicated, or invalid")]
struct InvalidAgentForm;

impl AgentFields {
    fn new(pairs: Vec<(Field, SecretString)>) -> Result<Self, InvalidAgentForm> {
        if pairs.len() > 24 {
            return Err(InvalidAgentForm);
        }
        let mut fields = Self(pairs);
        fields.required(Field::Csrf)?;
        Ok(fields)
    }

    fn take(&mut self, field: Field) -> Result<Option<SecretString>, InvalidAgentForm> {
        let Some(index) = self.0.iter().position(|(name, _)| *name == field) else {
            return Ok(None);
        };
        let (_, value) = self.0.remove(index);
        if self.0.iter().any(|(name, _)| *name == field) {
            return Err(InvalidAgentForm);
        }
        Ok(Some(value))
    }

    fn required(&mut self, field: Field) -> Result<SecretString, InvalidAgentForm> {
        self.take(field)?.ok_or(InvalidAgentForm)
    }

    fn parsed<T: FromStr>(&mut self, field: Field) -> Result<T, InvalidAgentForm> {
        self.required(field)?
            .expose_secret()
            .parse()
            .map_err(|_| InvalidAgentForm)
    }

    fn scopes(&mut self) -> Result<Vec<AdminScope>, InvalidAgentForm> {
        let mut scopes = BTreeSet::new();
        for (field, value) in self.0.drain(..) {
            if field != Field::Scope {
                return Err(InvalidAgentForm);
            }
            let scope = AdminScope::parse(value.expose_secret()).ok_or(InvalidAgentForm)?;
            if !scopes.insert(scope) {
                return Err(InvalidAgentForm);
            }
        }
        if scopes.is_empty() {
            return Err(InvalidAgentForm);
        }
        Ok(scopes.into_iter().collect())
    }

    fn registration(mut self) -> Result<(Uuid, RegisterAgentCredentialRequest), InvalidAgentForm> {
        let operation = self.parsed(Field::OperationId)?;
        let owner_user_id: UserId = self.parsed(Field::OwnerUserId)?;
        let public_key = self.required(Field::PublicKey)?.expose_secret().into();
        let label = self.required(Field::Label)?.expose_secret().into();
        let expires_at = self
            .take(Field::ExpiresAt)?
            .filter(|value| !value.expose_secret().is_empty())
            .map(|value| {
                let expiry = OffsetDateTime::parse(value.expose_secret(), &Rfc3339)
                    .map_err(|_| InvalidAgentForm)?;
                if expiry.offset() != UtcOffset::UTC {
                    return Err(InvalidAgentForm);
                }
                Ok(expiry)
            })
            .transpose()?;
        let scopes = self.scopes()?;
        Ok((
            operation,
            RegisterAgentCredentialRequest {
                owner_user_id,
                public_key,
                label,
                scopes,
                expires_at,
            },
        ))
    }

    fn replacement(mut self) -> Result<(Uuid, ReplaceAgentScopesRequest), InvalidAgentForm> {
        let operation = self.parsed(Field::OperationId)?;
        let expected_version = self.parsed(Field::ExpectedVersion)?;
        Ok((
            operation,
            ReplaceAgentScopesRequest {
                expected_version,
                scopes: self.scopes()?,
            },
        ))
    }
}

fn malformed_form(request_id: RequestId) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "Invalid agent form",
        "Select unique scopes and complete the current form. Use UTC for an optional expiry.",
        request_id,
    )
}

async fn register_agent(
    RequiredBrowserSession {
        request_id,
        security,
        session,
    }: RequiredBrowserSession,
    principal: AdminPrincipal,
    form: Result<Form<Vec<(Field, SecretString)>>, FormRejection>,
) -> Response {
    let pairs = match form {
        Ok(Form(pairs)) => pairs,
        Err(error) => return mutation_error_response(error.status(), "/admin/agents", request_id),
    };
    let (operation, request) = match AgentFields::new(pairs).and_then(AgentFields::registration) {
        Ok(value) => value,
        Err(_) => return malformed_form(request_id),
    };
    let response = register_agent_credential(
        request_id,
        Extension(security),
        principal,
        Some(Extension(BrowserSessionContext { session })),
        operation_headers(operation),
        Ok(Json(request)),
    )
    .await;
    mutation_response(response, "/admin/agents", request_id)
}

async fn save_scopes(
    RequiredBrowserSession {
        request_id,
        security,
        session,
    }: RequiredBrowserSession,
    principal: AdminPrincipal,
    path: Result<Path<String>, PathRejection>,
    form: Result<Form<Vec<(Field, SecretString)>>, FormRejection>,
) -> Response {
    let pairs = match form {
        Ok(Form(pairs)) => pairs,
        Err(error) => return mutation_error_response(error.status(), "/admin/agents", request_id),
    };
    let (operation, request) = match AgentFields::new(pairs).and_then(AgentFields::replacement) {
        Ok(value) => value,
        Err(_) => return malformed_form(request_id),
    };
    let response = replace_agent_scopes(
        request_id,
        Extension(security),
        principal,
        Some(Extension(BrowserSessionContext { session })),
        operation_headers(operation),
        path,
        Ok(Json(request)),
    )
    .await;
    mutation_response(response, "/admin/agents", request_id)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeForm {
    _csrf: SecretString,
    operation_id: Uuid,
    expected_version: u64,
    confirm: bool,
}

async fn revoke_agent(
    RequiredBrowserSession {
        request_id,
        security,
        session,
    }: RequiredBrowserSession,
    principal: AdminPrincipal,
    path: Result<Path<String>, PathRejection>,
    form: Result<Form<RevokeForm>, FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(error) => return mutation_error_response(error.status(), "/admin/agents", request_id),
    };
    if !form.confirm {
        return malformed_form(request_id);
    }
    let response = revoke_agent_credential(
        request_id,
        Extension(security),
        principal,
        Some(Extension(BrowserSessionContext { session })),
        operation_headers(form.operation_id),
        path,
        Ok(Json(ExpectedVersionRequest {
            expected_version: form.expected_version,
        })),
    )
    .await;
    mutation_response(response, "/admin/agents", request_id)
}

#[cfg(test)]
mod tests {
    use crate::{
        admin::principal::AdminAuthentication,
        domain::auth::{CsrfToken, store::StoredBrowserSession},
    };
    use maincopy_shared::auth::{AdminSessionId, HumanLoginProvider, UserRole, UserStatus};
    use std::sync::Arc;

    use super::*;

    fn fields(values: &[(&str, &str)]) -> Result<AgentFields, InvalidAgentForm> {
        let pairs = values
            .iter()
            .map(|(name, value)| {
                let field = Field::deserialize(serde::de::value::StrDeserializer::<
                    serde::de::value::Error,
                >::new(name))
                .map_err(|_| InvalidAgentForm)?;
                Ok((field, SecretString::new(*value)))
            })
            .collect::<Result<Vec<_>, InvalidAgentForm>>()?;
        AgentFields::new(pairs)
    }

    #[test]
    fn agent_forms_decode_native_repeated_scopes_and_consume_all_scalar_fields() {
        let operation = Uuid::from_u128(1).to_string();
        let values = [
            ("_csrf", "csrf"),
            ("operation_id", &operation),
            ("owner_user_id", &operation),
            ("public_key", "public"),
            ("label", "helper"),
            ("scope", "content_read"),
            ("scope", "release_manage"),
            ("expires_at", "2026-12-31T23:59:59Z"),
        ];
        let (actual, request) = fields(&values).unwrap().registration().unwrap();
        assert_eq!(actual.to_string(), operation);
        assert_eq!(request.owner_user_id.to_string(), operation);
        assert_eq!(request.public_key.as_ref(), "public");
        assert_eq!(request.label.as_ref(), "helper");
        assert_eq!(request.scopes.len(), 2);
        assert_eq!(request.expires_at.unwrap().offset(), UtcOffset::UTC);
        for expiry in ["", "2026-12-31T23:59:59+00:00"] {
            let mut values = values.to_vec();
            values[7].1 = expiry;
            assert!(fields(&values).unwrap().registration().is_ok());
        }
        let replacement = [
            ("_csrf", "csrf"),
            ("operation_id", &operation),
            ("expected_version", "3"),
            ("scope", "content_read"),
            ("scope", "release_manage"),
        ];
        let (_, request) = fields(&replacement).unwrap().replacement().unwrap();
        assert_eq!(request.expected_version, 3);
        assert_eq!(request.scopes.len(), 2);
    }

    #[test]
    fn agent_forms_reject_duplicate_missing_unknown_excess_and_cross_operation_fields() {
        let operation = Uuid::from_u128(1).to_string();
        let valid = vec![
            ("_csrf", "csrf"),
            ("operation_id", &operation),
            ("expected_version", "3"),
            ("scope", "content_read"),
        ];
        for duplicate in &valid {
            let mut bad = valid.clone();
            bad.push(*duplicate);
            assert!(fields(&bad).and_then(AgentFields::replacement).is_err());
        }
        for index in 0..valid.len() {
            let mut bad = valid.clone();
            bad.remove(index);
            assert!(fields(&bad).and_then(AgentFields::replacement).is_err());
        }
        for extra in [
            ("unknown", "value"),
            ("owner_user_id", &operation),
            ("scope", "unknown"),
        ] {
            let mut bad = valid.clone();
            bad.push(extra);
            assert!(fields(&bad).and_then(AgentFields::replacement).is_err());
        }
        assert!(fields(&vec![("scope", "content_read"); 25]).is_err());
        let registration = vec![
            ("_csrf", "csrf"),
            ("operation_id", &operation),
            ("owner_user_id", &operation),
            ("public_key", "public"),
            ("label", "label"),
            ("scope", "content_read"),
        ];
        for expiry in ["tomorrow", "2026-12-31T23:59:59+01:00"] {
            let mut bad = registration.clone();
            bad.push(("expires_at", expiry));
            assert!(fields(&bad).and_then(AgentFields::registration).is_err());
        }
        let mut malformed = valid;
        malformed[2].1 = "bad-version";
        assert!(
            fields(&malformed)
                .and_then(AgentFields::replacement)
                .is_err()
        );
    }
    #[test]
    fn grant_details_disable_expired_revoked_and_stale_operations_and_escape_metadata() {
        let now = OffsetDateTime::now_utc();
        let user_id = UserId::from_uuid(Uuid::from_u128(1));
        let session_id = AdminSessionId::from_uuid(Uuid::from_u128(2));
        let csrf_token = CsrfToken::generate().unwrap();
        let mut browser = BrowserFormSession {
            session: StoredBrowserSession {
                session_id,
                user_id,
                provider: HumanLoginProvider::Password,
                csrf_token_digest: csrf_token.digest(),
                instance_version: 1,
                current_instance_version: 1,
                version: 1,
                authenticated_at: now,
                fresh_until: now + time::Duration::minutes(5),
                expires_at: now + time::Duration::hours(1),
                revoked_at: None,
                last_seen_at: now,
                user_status: UserStatus::Enabled,
                user_version: 1,
                roles: BTreeSet::from([UserRole::Owner]),
            },
            csrf_token,
        };
        let principal = AdminPrincipal {
            user_id,
            scopes: Arc::new(BTreeSet::from([AdminScope::ContentRead])),
            authentication: AdminAuthentication::BrowserSession { session_id },
        };
        let mut agent = AgentCredentialResponse {
            agent_credential_id: Uuid::from_u128(3).into(),
            owner_user_id: user_id,
            issuer_user_id: user_id,
            public_key: "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9".into(),
            label: "<script>bad</script>".into(),
            scopes: vec![AdminScope::ContentRead],
            effective_scopes: vec![],
            version: 4,
            created_at: now,
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        };
        let page = agent_page(&agent, &principal, &browser, now).into_string();
        assert!(page.contains("&lt;script&gt;bad&lt;/script&gt;"));
        assert!(!page.contains("<script>"));
        assert!(page.contains("SHA256:fHnzBx4oNE6BU79sc8KU6+N1SuxOLLjLRHGy9Ey18i0"));
        assert!(page.contains("name=\"expected_version\" value=\"4\""));
        assert!(page.contains("value=\"content_read\" checked"));
        assert!(page.contains("Effective scopes</dt><dd>None"));
        assert!(!page.contains("<fieldset disabled>"));
        browser.session.fresh_until = now;
        let stale = agent_page(&agent, &principal, &browser, now).into_string();
        assert!(stale.contains("Sign in again"));
        assert!(stale.contains("<fieldset disabled>"));
        browser.session.fresh_until = now + time::Duration::minutes(1);
        agent.expires_at = Some(now);
        agent.last_used_at = Some(now);
        let expired = agent_page(&agent, &principal, &browser, now).into_string();
        assert!(expired.contains("has expired"));
        assert!(expired.contains("<fieldset disabled>"));
        assert!(expired.contains("Revoke agent"));
        agent.revoked_at = Some(now);
        let revoked = agent_page(&agent, &principal, &browser, now).into_string();
        assert!(revoked.contains("This grant is revoked"));
        assert!(!revoked.contains("Replace scopes"));
        assert!(!revoked.contains("Revoke agent"));
    }
}
