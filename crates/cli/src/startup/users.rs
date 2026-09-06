use std::{future::Future, io};
use uuid::Uuid;

use maincopy_shared::auth_api::{
    HumanCredentialResponse, ListUsersResponse, ReplaceUserRolesRequest, SetUserStatusRequest,
    UserMutationResponse, UserResponse,
};
use serde_json::json;

use super::CliError;
use crate::{
    client::AdminClientError,
    models::{UserCommand, UserTarget},
};

pub(super) enum UserOutput {
    List(ListUsersResponse),
    Inspect(UserResponse),
    Changed {
        idempotency_key: Uuid,
        receipt: UserMutationResponse,
    },
}

pub(super) async fn execute<ListFuture, InspectFuture, StatusFuture, RolesFuture>(
    command: UserCommand,
    list: impl FnOnce(Option<Uuid>) -> ListFuture,
    inspect: impl FnOnce(Uuid) -> InspectFuture,
    status: impl FnOnce(Uuid, Uuid, SetUserStatusRequest) -> StatusFuture,
    roles: impl FnOnce(Uuid, Uuid, ReplaceUserRolesRequest) -> RolesFuture,
) -> Result<UserOutput, CliError>
where
    ListFuture: Future<Output = Result<ListUsersResponse, AdminClientError>>,
    InspectFuture: Future<Output = Result<UserResponse, AdminClientError>>,
    StatusFuture: Future<Output = Result<UserMutationResponse, AdminClientError>>,
    RolesFuture: Future<Output = Result<UserMutationResponse, AdminClientError>>,
{
    match command {
        UserCommand::List { cursor } => Ok(UserOutput::List(list(cursor).await?)),
        UserCommand::Inspect { user_id } => Ok(UserOutput::Inspect(inspect(user_id).await?)),
        UserCommand::Status {
            target,
            status: selected_status,
        } => {
            let user_id = target.user_id;
            let request = SetUserStatusRequest {
                expected_version: target.expected_version,
                status: selected_status,
            };
            change(target, |operation| status(user_id, operation, request)).await
        }
        UserCommand::Roles {
            target,
            roles: selected_roles,
        } => {
            let user_id = target.user_id;
            let request = ReplaceUserRolesRequest {
                expected_version: target.expected_version,
                roles: selected_roles,
            };
            change(target, |operation| roles(user_id, operation, request)).await
        }
    }
}

async fn change<Send, SendFuture>(target: UserTarget, send: Send) -> Result<UserOutput, CliError>
where
    Send: FnOnce(Uuid) -> SendFuture,
    SendFuture: Future<Output = Result<UserMutationResponse, AdminClientError>>,
{
    let idempotency_key = target.idempotency_key.unwrap_or_else(Uuid::new_v4);
    let receipt = send(idempotency_key)
        .await
        .map_err(|source| CliError::UserChange {
            user_id: target.user_id,
            idempotency_key,
            source,
        })?;
    Ok(UserOutput::Changed {
        idempotency_key,
        receipt,
    })
}

pub(super) fn write_output(
    mut output: impl io::Write,
    result: UserOutput,
    json_output: bool,
) -> Result<(), CliError> {
    if json_output {
        let value = match result {
            UserOutput::List(page) => serde_json::to_value(page),
            UserOutput::Inspect(user) => Ok(json!({"user": user})),
            UserOutput::Changed {
                idempotency_key,
                receipt,
            } => Ok(json!({"idempotency_key": idempotency_key, "receipt": receipt})),
        }?;
        writeln!(output, "{value}")?;
        return Ok(());
    }
    match result {
        UserOutput::List(page) => {
            if page.users.is_empty() {
                writeln!(output, "No accounts on this page.")?;
            }
            for user in page.users {
                writeln!(
                    output,
                    "{}  {}  version {}",
                    user.user_id,
                    user.status.as_str(),
                    user.version
                )?;
            }
            if let Some(cursor) = page.next_cursor {
                writeln!(output, "Next page: maincopy users list --cursor {cursor}")?;
            }
        }
        UserOutput::Inspect(user) => write_user(output, user)?,
        UserOutput::Changed {
            idempotency_key,
            receipt,
        } => {
            writeln!(output, "Accepted account change: {idempotency_key}")?;
            writeln!(
                output,
                "User: {} (version {})",
                receipt.user_id, receipt.version
            )?;
            writeln!(
                output,
                "Inspect current state: maincopy users inspect {}",
                receipt.user_id
            )?;
        }
    }
    Ok(())
}

fn write_user(mut output: impl io::Write, user: UserResponse) -> io::Result<()> {
    writeln!(output, "User: {}", user.user_id)?;
    writeln!(output, "Status: {}", user.status.as_str())?;
    writeln!(output, "User version: {}", user.version)?;
    writeln!(
        output,
        "Roles: {}",
        user.roles
            .iter()
            .map(|role| role.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    writeln!(
        output,
        "Scopes: {}",
        user.scopes
            .iter()
            .map(|scope| scope.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    if user.credentials.is_empty() {
        writeln!(output, "No login credentials.")?;
    }
    for credential in user.credentials {
        match credential {
            HumanCredentialResponse::Password {
                username, version, ..
            } => {
                writeln!(
                    output,
                    "Password username: {} (credential version {version})",
                    username.escape_default()
                )?;
            }
            HumanCredentialResponse::Nostr {
                public_key,
                version,
                ..
            } => {
                writeln!(
                    output,
                    "Nostr public key: {} (credential version {version})",
                    public_key.escape_default()
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Arguments, Command};
    use crate::{
        client::AdminProblem,
        startup::{error_exit, write_error},
    };
    use clap::Parser;
    use maincopy_shared::{
        auth::{UserRole, UserStatus},
        auth_api::UserSummaryResponse,
    };
    use reqwest::StatusCode;
    use std::{cell::Cell, future::ready};
    use uuid::Uuid;

    fn user() -> UserResponse {
        serde_json::from_value(json!({"user_id":Uuid::from_u128(1), "status":"enabled", "version":5, "roles":["administrator"], "scopes":["user_manage"], "credentials":[
            {"provider":"password", "username":"alice", "version":2, "created_at":"2026-09-06T12:00:00Z", "updated_at":"2026-09-06T12:00:00Z"},
            {"provider":"nostr", "public_key":"f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9", "version":3, "created_at":"2026-09-06T12:00:00Z", "updated_at":"2026-09-06T12:00:00Z"}
        ], "created_at":"2026-09-06T12:00:00Z", "updated_at":"2026-09-06T12:00:00Z"})).unwrap()
    }

    #[test]
    fn account_commands_parse_explicit_cursor_and_target_without_secret_arguments() {
        let id = Uuid::from_u128(1).to_string();
        let args =
            Arguments::try_parse_from(["maincopy", "users", "list", "--cursor", &id]).unwrap();
        assert!(
            matches!(args.command, Command::Users { command: UserCommand::List { cursor: Some(value) } } if value.to_string() == id)
        );
        let args = Arguments::try_parse_from(["maincopy", "users", "inspect", &id]).unwrap();
        assert!(
            matches!(args.command, Command::Users { command: UserCommand::Inspect { user_id } } if user_id.to_string() == id)
        );
        assert!(Arguments::try_parse_from(["maincopy", "users", "inspect", "bad-id"]).is_err());
        assert!(
            Arguments::try_parse_from(["maincopy", "users", "list", "--password", "secret"])
                .is_err()
        );
    }

    #[test]
    fn account_output_separates_user_and_credential_versions_and_preserves_json_metadata() {
        for json_output in [false, true] {
            let mut output = Vec::new();
            write_output(&mut output, UserOutput::Inspect(user()), json_output).unwrap();
            let text = String::from_utf8(output).unwrap();
            if json_output {
                let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(value["user"], serde_json::to_value(user()).unwrap());
            } else {
                assert!(text.contains("User version: 5"));
                assert!(text.contains("alice (credential version 2)"));
                assert!(text.contains("(credential version 3)"));
                assert!(text.contains("Roles: administrator"));
                assert!(text.contains("Scopes: user_manage"));
            }
        }
    }

    #[test]
    fn account_output_reports_empty_pages_and_an_explicit_continuation() {
        let id = Uuid::from_u128(1);
        let mut value = serde_json::to_value(user()).unwrap();
        value["credential_providers"] = json!(["password", "nostr"]);
        let summary: UserSummaryResponse = serde_json::from_value(value).unwrap();
        for json_output in [false, true] {
            for page in [
                ListUsersResponse {
                    users: Vec::new(),
                    next_cursor: None,
                },
                ListUsersResponse {
                    users: vec![summary.clone()],
                    next_cursor: Some(id.into()),
                },
            ] {
                let mut output = Vec::new();
                let empty = page.users.is_empty();
                let expected = serde_json::to_value(&page).unwrap();
                write_output(&mut output, UserOutput::List(page), json_output).unwrap();
                let text = String::from_utf8(output).unwrap();
                if json_output {
                    assert_eq!(
                        serde_json::from_str::<serde_json::Value>(&text).unwrap(),
                        expected
                    );
                } else if empty {
                    assert!(text.contains("No accounts"));
                } else {
                    assert!(text.contains(&format!("maincopy users list --cursor {id}")));
                }
            }
        }
        let mut account = user();
        account.credentials.clear();
        let mut output = Vec::new();
        write_output(&mut output, UserOutput::Inspect(account), false).unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("No login credentials")
        );
    }
    #[test]
    fn account_mutations_require_current_versions_and_closed_status_and_role_values() {
        let id = Uuid::from_u128(1).to_string();
        for (command, option, value) in [
            ("status", "--status", "disabled"),
            ("roles", "--roles", "publisher"),
        ] {
            let args = Arguments::try_parse_from([
                "maincopy",
                "users",
                command,
                &id,
                "--expected-version",
                "5",
                "--idempotency-key",
                &id,
                option,
                value,
            ])
            .unwrap();
            let target = match args.command {
                Command::Users {
                    command: UserCommand::Status { target, status },
                } => {
                    assert_eq!(status.as_str(), "disabled");
                    target
                }
                Command::Users {
                    command: UserCommand::Roles { target, roles },
                } => {
                    assert_eq!(roles.len(), 1);
                    assert_eq!(roles[0].as_str(), "publisher");
                    target
                }
                _ => panic!("expected an account mutation"),
            };
            assert_eq!(target.expected_version, 5);
            assert_eq!(target.idempotency_key, Some(target.user_id));
            assert!(
                Arguments::try_parse_from(["maincopy", "users", command, &id, option, value])
                    .is_err()
            );
            assert!(
                Arguments::try_parse_from([
                    "maincopy",
                    "users",
                    command,
                    &id,
                    "--expected-version",
                    "0",
                    option,
                    value
                ])
                .is_err()
            );
            assert!(
                Arguments::try_parse_from([
                    "maincopy",
                    "users",
                    command,
                    &id,
                    "--expected-version",
                    "5",
                    option,
                    "unknown"
                ])
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn account_changes_keep_the_operation_identity_in_success_and_failure_output() {
        let user_id = Uuid::from_u128(1);
        for supplied in [None, Some(Uuid::from_u128(2))] {
            let calls = Cell::new(0);
            let result = change(
                UserTarget {
                    user_id,
                    expected_version: 4,
                    idempotency_key: supplied,
                },
                |operation| {
                    calls.set(calls.get() + 1);
                    if let Some(expected) = supplied {
                        assert_eq!(operation, expected);
                    } else {
                        assert_eq!(operation.get_version_num(), 4);
                    }
                    ready(Ok(UserMutationResponse {
                        user_id: user_id.into(),
                        version: 5,
                    }))
                },
            )
            .await
            .unwrap();
            assert_eq!(calls.get(), 1);
            let UserOutput::Changed {
                idempotency_key,
                receipt,
            } = result
            else {
                panic!("expected receipt")
            };
            for json_output in [false, true] {
                let mut output = Vec::new();
                write_output(
                    &mut output,
                    UserOutput::Changed {
                        idempotency_key,
                        receipt,
                    },
                    json_output,
                )
                .unwrap();
                let text = String::from_utf8(output).unwrap();
                assert!(text.contains(&idempotency_key.to_string()));
                assert!(text.contains(&user_id.to_string()));
                if json_output {
                    assert_eq!(
                        serde_json::from_str::<serde_json::Value>(&text).unwrap()["receipt"]["version"],
                        5
                    );
                }
            }
        }
        for (status, exit) in [
            (StatusCode::FORBIDDEN, 77),
            (StatusCode::UNAUTHORIZED, 77),
            (StatusCode::PRECONDITION_FAILED, 75),
            (StatusCode::CONFLICT, 75),
            (StatusCode::SERVICE_UNAVAILABLE, 69),
        ] {
            let operation = Uuid::from_u128(2);
            let error = change(
                UserTarget {
                    user_id,
                    expected_version: 4,
                    idempotency_key: Some(operation),
                },
                |actual| {
                    assert_eq!(actual, operation);
                    ready(Err(AdminClientError::HttpStatus {
                        status,
                        problem: Some(AdminProblem {
                            code: "account_error".into(),
                            message: "safe account failure".into(),
                        }),
                        request_id: Some(Uuid::from_u128(3)),
                    }))
                },
            )
            .await
            .err()
            .unwrap();
            assert_eq!(error_exit(&error), exit);
            for json_output in [false, true] {
                let mut output = Vec::new();
                write_error(&mut output, &error, exit, json_output).unwrap();
                let text = String::from_utf8(output).unwrap();
                assert!(text.contains(&operation.to_string()));
                assert!(text.contains(&user_id.to_string()));
                assert!(text.contains("safe account failure"));
                if json_output {
                    assert_eq!(
                        serde_json::from_str::<serde_json::Value>(&text).unwrap()["error"]["idempotency_key"],
                        operation.to_string()
                    );
                }
            }
        }
    }

    #[test]
    fn credential_metadata_cannot_inject_terminal_controls() {
        let mut account = user();
        let HumanCredentialResponse::Password { username, .. } = &mut account.credentials[0] else {
            panic!("password fixture")
        };
        *username = "alice\x1b[2J\nforged status".into();
        let mut output = Vec::new();
        write_output(&mut output, UserOutput::Inspect(account), false).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains('\x1b'));
        assert!(!text.contains("\nforged status"));
    }
    #[tokio::test]
    async fn account_dispatch_submits_only_the_selected_read_or_versioned_mutation() {
        let id = Uuid::from_u128(1);
        let operation = Uuid::from_u128(2);
        let target = || UserTarget {
            user_id: id,
            expected_version: 4,
            idempotency_key: Some(operation),
        };
        let commands = [
            UserCommand::List { cursor: Some(id) },
            UserCommand::Inspect { user_id: id },
            UserCommand::Status {
                target: target(),
                status: UserStatus::Disabled,
            },
            UserCommand::Roles {
                target: target(),
                roles: vec![UserRole::Publisher],
            },
        ];
        for (selected, command) in commands.into_iter().enumerate() {
            let calls = Cell::new(0);
            let result = execute(
                command,
                |cursor| {
                    calls.set(calls.get() + 1);
                    assert_eq!(selected, 0);
                    assert_eq!(cursor, Some(id));
                    ready(Ok(ListUsersResponse {
                        users: Vec::new(),
                        next_cursor: None,
                    }))
                },
                |user_id| {
                    calls.set(calls.get() + 1);
                    assert_eq!(selected, 1);
                    assert_eq!(user_id, id);
                    ready(Ok(user()))
                },
                |user_id, key, request| {
                    calls.set(calls.get() + 1);
                    assert_eq!(selected, 2);
                    assert_eq!((user_id, key), (id, operation));
                    assert_eq!(
                        serde_json::to_value(request).unwrap(),
                        json!({"expected_version":4, "status":"disabled"})
                    );
                    ready(Ok(UserMutationResponse {
                        user_id: id.into(),
                        version: 5,
                    }))
                },
                |user_id, key, request| {
                    calls.set(calls.get() + 1);
                    assert_eq!(selected, 3);
                    assert_eq!((user_id, key), (id, operation));
                    assert_eq!(
                        serde_json::to_value(request).unwrap(),
                        json!({"expected_version":4, "roles":["publisher"]})
                    );
                    ready(Ok(UserMutationResponse {
                        user_id: id.into(),
                        version: 5,
                    }))
                },
            )
            .await
            .unwrap();
            assert_eq!(calls.get(), 1);
            match result {
                UserOutput::List(_) => assert_eq!(selected, 0),
                UserOutput::Inspect(_) => assert_eq!(selected, 1),
                UserOutput::Changed {
                    idempotency_key,
                    receipt,
                } => {
                    assert!(selected >= 2);
                    assert_eq!(idempotency_key, operation);
                    assert_eq!(receipt.user_id.into_uuid(), id);
                }
            }
        }
    }
}
