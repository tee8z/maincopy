use std::{future::Future, io};

use maincopy_shared::auth_api::{
    AgentCredentialMutationResponse, AgentCredentialResponse, ExpectedVersionRequest,
    ListAgentCredentialsResponse, RegisterAgentCredentialRequest, ReplaceAgentScopesRequest,
};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::CliError;
use crate::{
    client::{AdminClientError, AgentMutation},
    models::AgentCommand,
    nip98::inspect_public_key,
};

pub(super) enum AgentOutput {
    List(ListAgentCredentialsResponse),
    Inspect(AgentCredentialResponse),
    Changed {
        idempotency_key: Uuid,
        receipt: AgentCredentialMutationResponse,
    },
}

pub(super) async fn execute<ListFuture, InspectFuture, ChangeFuture>(
    command: AgentCommand,
    list: impl FnOnce(Option<Uuid>) -> ListFuture,
    inspect: impl FnOnce(Uuid) -> InspectFuture,
    change: impl FnOnce(Uuid, AgentMutation) -> ChangeFuture,
) -> Result<AgentOutput, CliError>
where
    ListFuture: Future<Output = Result<ListAgentCredentialsResponse, AdminClientError>>,
    InspectFuture: Future<Output = Result<AgentCredentialResponse, AdminClientError>>,
    ChangeFuture: Future<Output = Result<AgentCredentialMutationResponse, AdminClientError>>,
{
    let (operation, mutation) = match command {
        AgentCommand::List { cursor } => return Ok(AgentOutput::List(list(cursor).await?)),
        AgentCommand::Inspect { agent_id } => {
            return Ok(AgentOutput::Inspect(inspect(agent_id).await?));
        }
        AgentCommand::Register {
            owner_user_id,
            public_key,
            label,
            scopes,
            expires_at,
            idempotency_key,
        } => (
            idempotency_key,
            AgentMutation::Register(RegisterAgentCredentialRequest {
                owner_user_id: owner_user_id.into(),
                public_key,
                label,
                scopes,
                expires_at,
            }),
        ),
        AgentCommand::Scopes { target, scopes } => (
            target.idempotency_key,
            AgentMutation::Scopes {
                agent_id: target.agent_id,
                request: ReplaceAgentScopesRequest {
                    expected_version: target.expected_version,
                    scopes,
                },
            },
        ),
        AgentCommand::Revoke(target) => (
            target.idempotency_key,
            AgentMutation::Revoke {
                agent_id: target.agent_id,
                request: ExpectedVersionRequest {
                    expected_version: target.expected_version,
                },
            },
        ),
    };
    let idempotency_key = operation.unwrap_or_else(Uuid::new_v4);
    let receipt =
        change(idempotency_key, mutation)
            .await
            .map_err(|source| CliError::AgentChange {
                idempotency_key,
                source,
            })?;
    Ok(AgentOutput::Changed {
        idempotency_key,
        receipt,
    })
}

pub(super) fn write_output(
    mut output: impl io::Write,
    result: AgentOutput,
    json_output: bool,
) -> Result<(), CliError> {
    if json_output {
        let value = match result {
            AgentOutput::List(page) => {
                json!({"agent_credentials":page.agent_credentials.into_iter().map(public_agent).collect::<Result<Vec<_>, _>>()?, "next_cursor":page.next_cursor})
            }
            AgentOutput::Inspect(agent) => json!({"agent":public_agent(agent)?}),
            AgentOutput::Changed {
                idempotency_key,
                receipt,
            } => json!({"idempotency_key": idempotency_key, "receipt": receipt}),
        };
        writeln!(output, "{value}")?;
        return Ok(());
    }
    match result {
        AgentOutput::List(page) => {
            if page.agent_credentials.is_empty() {
                writeln!(output, "No agent grants on this page.")?;
            }
            for agent in page.agent_credentials {
                writeln!(
                    output,
                    "{}  {}  version {}  {}",
                    agent.agent_credential_id,
                    agent.label.escape_default(),
                    agent.version,
                    grant_state(&agent, OffsetDateTime::now_utc())
                )?;
            }
            if let Some(cursor) = page.next_cursor {
                writeln!(output, "Next page: maincopy agents list --cursor {cursor}")?;
            }
        }
        AgentOutput::Inspect(agent) => write_agent(output, agent)?,
        AgentOutput::Changed {
            idempotency_key,
            receipt,
        } => {
            writeln!(output, "Accepted agent change: {idempotency_key}")?;
            writeln!(
                output,
                "Grant: {} (version {})",
                receipt.agent_credential_id, receipt.version
            )?;
            writeln!(
                output,
                "Inspect current state: maincopy agents inspect {}",
                receipt.agent_credential_id
            )?;
        }
    }
    Ok(())
}

fn public_agent(agent: AgentCredentialResponse) -> Result<serde_json::Value, CliError> {
    let identity = inspect_public_key(&agent.public_key).map_err(|_| {
        AdminClientError::InvalidIdentityResponse {
            message: "agent public key is invalid",
        }
    })?;
    let mut value = serde_json::to_value(&agent)?;
    value["fingerprint"] = json!(identity.fingerprint);
    value["state"] = json!(grant_state(&agent, OffsetDateTime::now_utc()));
    Ok(value)
}

fn grant_state(agent: &AgentCredentialResponse, now: OffsetDateTime) -> &'static str {
    if agent.revoked_at.is_some() {
        "revoked"
    } else if agent.expires_at.is_some_and(|expiry| now >= expiry) {
        "expired"
    } else if agent.effective_scopes.is_empty() {
        "no effective scopes"
    } else {
        "unexpired"
    }
}

fn write_agent(mut output: impl io::Write, agent: AgentCredentialResponse) -> Result<(), CliError> {
    let identity = inspect_public_key(&agent.public_key).map_err(|_| {
        AdminClientError::InvalidIdentityResponse {
            message: "agent public key is invalid",
        }
    })?;
    writeln!(
        output,
        "Grant: {} (version {})",
        agent.agent_credential_id, agent.version
    )?;
    writeln!(output, "Label: {}", agent.label.escape_default())?;
    writeln!(
        output,
        "State: {}",
        grant_state(&agent, OffsetDateTime::now_utc())
    )?;
    writeln!(output, "Owner: {}", agent.owner_user_id)?;
    writeln!(output, "Issuer: {}", agent.issuer_user_id)?;
    writeln!(output, "Public key: {}", identity.public_key)?;
    writeln!(output, "Fingerprint: {}", identity.fingerprint)?;
    writeln!(
        output,
        "Requested scopes: {}",
        agent
            .scopes
            .iter()
            .map(|scope| scope.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    writeln!(
        output,
        "Effective scopes: {}",
        agent
            .effective_scopes
            .iter()
            .map(|scope| scope.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    writeln!(output, "Created: {}", agent.created_at)?;
    for (label, timestamp, absent) in [
        ("Expires", agent.expires_at, "No expiry"),
        ("Last used", agent.last_used_at, "Never"),
        ("Revoked", agent.revoked_at, "No"),
    ] {
        match timestamp {
            Some(timestamp) => writeln!(output, "{label}: {timestamp}")?,
            None => writeln!(output, "{label}: {absent}")?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        client::AdminProblem,
        models::{Arguments, Command},
        startup::{error_exit, write_error},
    };
    use clap::Parser;
    use maincopy_shared::auth::AdminScope;
    use reqwest::StatusCode;
    use std::{cell::Cell, future::ready};

    fn grant() -> AgentCredentialResponse {
        serde_json::from_value(json!({"agent_credential_id":Uuid::from_u128(1), "owner_user_id":Uuid::from_u128(2), "issuer_user_id":Uuid::from_u128(3), "public_key":"f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9", "label":"helper\n\u{1b}[2J", "scopes":["content_read", "release_manage"], "effective_scopes":["content_read"], "version":3, "created_at":"2026-09-06T12:00:00Z", "expires_at":null, "last_used_at":null, "revoked_at":null})).unwrap()
    }

    fn command(values: &[&str]) -> AgentCommand {
        let arguments = Arguments::try_parse_from(
            ["maincopy", "agents"]
                .into_iter()
                .chain(values.iter().copied()),
        )
        .unwrap();
        let Command::Agents { command } = arguments.command else {
            panic!("expected agents")
        };
        command
    }

    #[test]
    fn agent_arguments_require_versions_known_scopes_and_utc_expiry() {
        let id = Uuid::from_u128(1).to_string();
        assert!(matches!(
            command(&["list", "--cursor", &id]),
            AgentCommand::List { cursor: Some(_) }
        ));
        assert!(matches!(
            command(&["inspect", &id]),
            AgentCommand::Inspect { .. }
        ));
        assert!(matches!(
            command(&["revoke", &id, "--expected-version", "3"]),
            AgentCommand::Revoke(_)
        ));
        for values in [
            vec!["revoke", &id],
            vec!["revoke", &id, "--expected-version", "0"],
            vec![
                "scopes",
                &id,
                "--expected-version",
                "3",
                "--scopes",
                "unknown",
            ],
            vec![
                "register",
                "--owner-user-id",
                &id,
                "--label",
                "helper",
                "--public-key",
                "key",
                "--scopes",
                "content_read",
                "--expires-at",
                "2026-12-31T12:00:00+01:00",
            ],
        ] {
            assert!(
                Arguments::try_parse_from(["maincopy", "agents"].into_iter().chain(values))
                    .is_err()
            );
        }
    }

    #[test]
    fn agent_output_exposes_public_fingerprints_ownership_and_distinct_scope_sets() {
        for json_output in [false, true] {
            let mut output = Vec::new();
            write_output(&mut output, AgentOutput::Inspect(grant()), json_output).unwrap();
            let text = String::from_utf8(output).unwrap();
            assert!(text.contains("SHA256:fHnzBx4oNE6BU79sc8KU6+N1SuxOLLjLRHGy9Ey18i0"));
            assert!(!text.contains('\x1b'));
            if json_output {
                let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(
                    value["agent"]["scopes"],
                    json!(["content_read", "release_manage"])
                );
                assert_eq!(value["agent"]["effective_scopes"], json!(["content_read"]));
                assert_eq!(
                    value["agent"]["owner_user_id"],
                    Uuid::from_u128(2).to_string()
                );
                assert_eq!(value["agent"]["state"], "unexpired");
            } else {
                assert!(text.contains("Requested scopes: content_read, release_manage"));
                assert!(text.contains("Effective scopes: content_read"));
                assert!(text.contains("No expiry"));
                assert!(text.contains("Never"));
            }
        }
        let now = OffsetDateTime::now_utc();
        let mut agent = grant();
        agent.effective_scopes.clear();
        assert_eq!(grant_state(&agent, now), "no effective scopes");
        agent.expires_at = Some(now);
        assert_eq!(grant_state(&agent, now), "expired");
        agent.revoked_at = Some(now);
        assert_eq!(grant_state(&agent, now), "revoked");
        agent.last_used_at = Some(now);
        let mut output = Vec::new();
        write_output(&mut output, AgentOutput::Inspect(agent), false).unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("State: revoked")
        );
    }

    #[test]
    fn agent_pages_report_empty_and_explicit_next_cursor_without_terminal_controls() {
        for json_output in [false, true] {
            for agents in [vec![], vec![grant()]] {
                let next_cursor = agents.last().map(|agent| agent.agent_credential_id);
                let mut output = Vec::new();
                write_output(
                    &mut output,
                    AgentOutput::List(ListAgentCredentialsResponse {
                        agent_credentials: agents,
                        next_cursor,
                    }),
                    json_output,
                )
                .unwrap();
                let text = String::from_utf8(output).unwrap();
                assert!(!text.contains('\x1b'));
                if json_output {
                    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(value["next_cursor"], json!(next_cursor));
                } else if next_cursor.is_some() {
                    assert!(text.contains("maincopy agents list --cursor"));
                } else {
                    assert!(text.contains("No agent grants"));
                }
            }
        }
        let mut invalid = grant();
        invalid.public_key = "bad".into();
        for json_output in [false, true] {
            assert!(
                write_output(
                    Vec::new(),
                    AgentOutput::Inspect(invalid.clone()),
                    json_output
                )
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn agent_dispatch_submits_only_the_selected_operation_and_retains_retry_identity() {
        let id = Uuid::from_u128(1).to_string();
        let operation = Uuid::from_u128(4).to_string();
        let commands = [
            command(&["list", "--cursor", &id]),
            command(&["inspect", &id]),
            command(&[
                "register",
                "--owner-user-id",
                &id,
                "--label",
                "helper",
                "--public-key",
                "key",
                "--scopes",
                "content_read,release_manage",
                "--idempotency-key",
                &operation,
            ]),
            command(&[
                "scopes",
                &id,
                "--expected-version",
                "3",
                "--scopes",
                "content_read",
                "--idempotency-key",
                &operation,
            ]),
            command(&["revoke", &id, "--expected-version", "3"]),
        ];
        for (selected, command) in commands.into_iter().enumerate() {
            let calls = Cell::new(0);
            let result = execute(
                command,
                |cursor| {
                    calls.set(calls.get() + 1);
                    assert_eq!(selected, 0);
                    assert_eq!(cursor, Some(Uuid::from_u128(1)));
                    ready(Ok(ListAgentCredentialsResponse {
                        agent_credentials: vec![],
                        next_cursor: None,
                    }))
                },
                |agent_id| {
                    calls.set(calls.get() + 1);
                    assert_eq!(selected, 1);
                    assert_eq!(agent_id, Uuid::from_u128(1));
                    ready(Ok(grant()))
                },
                |key, mutation| {
                    calls.set(calls.get() + 1);
                    match mutation {
                        AgentMutation::Register(request) => {
                            assert_eq!(selected, 2);
                            assert_eq!(request.owner_user_id.into_uuid(), Uuid::from_u128(1));
                            assert_eq!(request.scopes.len(), 2);
                            assert_eq!(key.to_string(), operation);
                        }
                        AgentMutation::Scopes { agent_id, request } => {
                            assert_eq!(selected, 3);
                            assert_eq!(agent_id, Uuid::from_u128(1));
                            assert_eq!(request.expected_version, 3);
                            assert_eq!(request.scopes, vec![AdminScope::ContentRead]);
                            assert_eq!(key.to_string(), operation);
                        }
                        AgentMutation::Revoke { agent_id, request } => {
                            assert_eq!(selected, 4);
                            assert_eq!(agent_id, Uuid::from_u128(1));
                            assert_eq!(request.expected_version, 3);
                            assert_eq!(key.get_version_num(), 4);
                        }
                    }
                    ready(Ok(AgentCredentialMutationResponse {
                        agent_credential_id: Uuid::from_u128(1).into(),
                        version: 4,
                    }))
                },
            )
            .await
            .unwrap();
            assert_eq!(calls.get(), 1);
            if let AgentOutput::Changed {
                idempotency_key,
                receipt,
            } = result
            {
                for json_output in [false, true] {
                    let mut output = Vec::new();
                    write_output(
                        &mut output,
                        AgentOutput::Changed {
                            idempotency_key,
                            receipt,
                        },
                        json_output,
                    )
                    .unwrap();
                    assert!(
                        String::from_utf8(output)
                            .unwrap()
                            .contains(&idempotency_key.to_string())
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn agent_failures_preserve_operation_identity_and_stable_error_categories() {
        let id = Uuid::from_u128(1).to_string();
        let operation = Uuid::from_u128(2).to_string();
        for (status, exit) in [
            (StatusCode::FORBIDDEN, 77),
            (StatusCode::UNAUTHORIZED, 77),
            (StatusCode::CONFLICT, 75),
            (StatusCode::PRECONDITION_FAILED, 75),
            (StatusCode::SERVICE_UNAVAILABLE, 69),
        ] {
            let result = execute(
                command(&[
                    "revoke",
                    &id,
                    "--expected-version",
                    "3",
                    "--idempotency-key",
                    &operation,
                ]),
                |_| ready(Err(AdminClientError::HumanCredentialsMissing)),
                |_| ready(Err(AdminClientError::HumanCredentialsMissing)),
                |_, _| {
                    ready(Err(AdminClientError::HttpStatus {
                        status,
                        problem: Some(AdminProblem {
                            code: "grant_error".into(),
                            message: "safe grant failure".into(),
                        }),
                        request_id: Some(Uuid::from_u128(3)),
                    }))
                },
            )
            .await;
            let error = result.err().unwrap();
            assert_eq!(error_exit(&error), exit);
            for json_output in [false, true] {
                let mut output = Vec::new();
                write_error(&mut output, &error, exit, json_output).unwrap();
                let text = String::from_utf8(output).unwrap();
                assert!(text.contains(&operation));
                assert!(text.contains("safe grant failure"));
                if json_output {
                    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(value["error"]["idempotency_key"], operation);
                }
            }
        }
    }
}
