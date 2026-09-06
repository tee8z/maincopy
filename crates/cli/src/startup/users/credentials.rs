use std::io;

use maincopy_shared::auth_api::{
    CreateUserRequest, ExpectedVersionRequest, HumanCredentialInput, PutHumanCredentialRequest,
    SecretString,
};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    client::UserMutation,
    models::{
        CreateUserArguments, InitialCredentials, LoginCredentialArguments, UserCredentialCommand,
    },
    startup::CliError,
};

pub(super) fn prepare_create(
    arguments: CreateUserArguments,
    mut prompt: impl FnMut(&str) -> io::Result<SecretString>,
) -> Result<(Uuid, UserMutation), CliError> {
    let operation = arguments.idempotency_key.unwrap_or_else(Uuid::new_v4);
    let credentials =
        initial_credentials(arguments.credentials, &mut prompt).map_err(|source| {
            CliError::AccountInput {
                user_id: None,
                idempotency_key: operation,
                source,
            }
        })?;
    Ok((
        operation,
        UserMutation::Create(CreateUserRequest {
            status: arguments.status,
            roles: arguments.roles,
            credentials,
        }),
    ))
}

fn initial_credentials(
    arguments: InitialCredentials,
    prompt: &mut impl FnMut(&str) -> io::Result<SecretString>,
) -> Result<Vec<HumanCredentialInput>, CredentialInputError> {
    match arguments {
        InitialCredentials::Password { username } => {
            Ok(vec![password_credential(username, prompt)?])
        }
        InitialCredentials::Nostr { public_key } => {
            Ok(vec![HumanCredentialInput::Nostr { public_key }])
        }
        InitialCredentials::Both {
            username,
            public_key,
        } => Ok(vec![
            password_credential(username, prompt)?,
            HumanCredentialInput::Nostr { public_key },
        ]),
    }
}

pub(super) fn prepare_change(
    user_id: Uuid,
    command: UserCredentialCommand,
    mut prompt: impl FnMut(&str) -> io::Result<SecretString>,
) -> Result<(Uuid, UserMutation), CliError> {
    match command {
        UserCredentialCommand::Add {
            idempotency_key,
            credential,
        } => {
            let operation = idempotency_key.unwrap_or_else(Uuid::new_v4);
            let credential = prepare_credential(user_id, operation, credential, &mut prompt)?;
            Ok((
                operation,
                UserMutation::PutCredential {
                    user_id,
                    request: PutHumanCredentialRequest::Create { credential },
                },
            ))
        }
        UserCredentialCommand::Replace { target, credential } => {
            let operation = target.idempotency_key.unwrap_or_else(Uuid::new_v4);
            let credential = prepare_credential(user_id, operation, credential, &mut prompt)?;
            Ok((
                operation,
                UserMutation::PutCredential {
                    user_id,
                    request: PutHumanCredentialRequest::Replace {
                        expected_version: target.expected_version,
                        credential,
                    },
                },
            ))
        }
        UserCredentialCommand::Remove { target, provider } => Ok((
            target.idempotency_key.unwrap_or_else(Uuid::new_v4),
            UserMutation::RemoveCredential {
                user_id,
                provider,
                request: ExpectedVersionRequest {
                    expected_version: target.expected_version,
                },
            },
        )),
    }
}

fn prepare_credential(
    user_id: Uuid,
    operation: Uuid,
    arguments: LoginCredentialArguments,
    prompt: &mut impl FnMut(&str) -> io::Result<SecretString>,
) -> Result<HumanCredentialInput, CliError> {
    match arguments {
        LoginCredentialArguments::Password { username } => password_credential(username, prompt),
        LoginCredentialArguments::Nostr { public_key } => {
            Ok(HumanCredentialInput::Nostr { public_key })
        }
    }
    .map_err(|source| CliError::AccountInput {
        user_id: Some(user_id),
        idempotency_key: operation,
        source,
    })
}

fn password_credential(
    username: Box<str>,
    prompt: &mut impl FnMut(&str) -> io::Result<SecretString>,
) -> Result<HumanCredentialInput, CredentialInputError> {
    let password = prompt("New password: ").map_err(CredentialInputError::Read)?;
    validate_password(&password)?;
    let confirmation = prompt("Confirm new password: ").map_err(CredentialInputError::Read)?;
    validate_password(&confirmation)?;
    if password.expose_secret() != confirmation.expose_secret() {
        return Err(CredentialInputError::Mismatch);
    }
    Ok(HumanCredentialInput::Password { username, password })
}

fn validate_password(password: &SecretString) -> Result<(), CredentialInputError> {
    let value = password.expose_secret();
    if value.len() > 1024 || !(15..=128).contains(&value.chars().count()) {
        return Err(CredentialInputError::InvalidPassword);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub(in crate::startup) enum CredentialInputError {
    #[error("could not read the protected terminal; no account change was submitted")]
    Read(#[source] io::Error),
    #[error(
        "password must contain 15–128 Unicode characters and at most 1024 bytes; no account change was submitted"
    )]
    InvalidPassword,
    #[error("password confirmation does not match; no account change was submitted")]
    Mismatch,
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, collections::VecDeque, future::ready};

    use clap::Parser;
    use maincopy_shared::{
        auth::{HumanLoginProvider, UserRole},
        auth_api::UserMutationResponse,
    };
    use serde_json::json;

    use super::*;
    use crate::{
        client::AdminClientError,
        models::{Arguments, Command, UserCommand},
        startup::{
            error_exit,
            users::{execute, write_output},
            write_error,
        },
        transport::RequestBody,
    };

    const PUBLIC_KEY: &str = "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9";
    const PASSWORD: &str = "a sufficiently long fixture password";
    const OPERATION: &str = "00000000-0000-0000-0000-000000000002";
    const USER: &str = "00000000-0000-0000-0000-000000000001";

    fn command(arguments: &[&str]) -> UserCommand {
        let mut args = vec!["maincopy", "users"];
        args.extend(arguments);
        let Command::Users { command } = Arguments::try_parse_from(args).unwrap().command else {
            panic!("account command")
        };
        command
    }

    #[tokio::test]
    async fn account_creation_submits_password_nostr_and_combined_credentials_atomically() {
        for provider in ["password", "nostr", "both"] {
            let mut args = vec![
                "create",
                "--roles",
                "publisher",
                "--idempotency-key",
                OPERATION,
                provider,
            ];
            if provider != "nostr" {
                args.extend(["--username", "fixture"]);
            }
            if provider != "password" {
                args.extend(["--public-key", PUBLIC_KEY]);
            }
            let prompts = Cell::new(0);
            let sends = Cell::new(0);
            let result = execute(
                command(&args),
                |_| async { panic!("no list") },
                |_| async { panic!("no inspect") },
                |operation, change| {
                    sends.set(sends.get() + 1);
                    assert_eq!(operation.to_string(), OPERATION);
                    let UserMutation::Create(request) = change else {
                        panic!("create")
                    };
                    assert_eq!(request.roles, [UserRole::Publisher]);
                    let providers: Vec<_> = request
                        .credentials
                        .iter()
                        .map(HumanCredentialInput::provider)
                        .collect();
                    let expected = match provider {
                        "password" => vec![HumanLoginProvider::Password],
                        "nostr" => vec![HumanLoginProvider::Nostr],
                        _ => vec![HumanLoginProvider::Password, HumanLoginProvider::Nostr],
                    };
                    assert_eq!(providers, expected);
                    let body = RequestBody::json(&request).unwrap();
                    let wire: serde_json::Value = serde_json::from_slice(body.as_ref()).unwrap();
                    if provider != "nostr" {
                        assert_eq!(wire["credentials"][0]["password"], PASSWORD);
                    }
                    ready(Ok(UserMutationResponse {
                        user_id: Uuid::from_u128(1).into(),
                        version: 1,
                    }))
                },
                |_| {
                    prompts.set(prompts.get() + 1);
                    Ok(SecretString::new(PASSWORD))
                },
            )
            .await
            .unwrap();
            assert_eq!(sends.get(), 1);
            assert_eq!(prompts.get(), if provider == "nostr" { 0 } else { 2 });
            let mut output = Vec::new();
            write_output(&mut output, result, true).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(!output.contains(PASSWORD));
            assert!(output.contains(OPERATION));
        }
    }

    #[tokio::test]
    async fn rejected_or_cancelled_password_input_never_submits_an_account_change() {
        let long = "p".repeat(129);
        for input in [
            vec![Ok("too short")],
            vec![Ok(long.as_str())],
            vec![Ok(PASSWORD), Ok("different long fixture password")],
            vec![Err(io::Error::from(io::ErrorKind::Interrupted))],
            vec![
                Ok(PASSWORD),
                Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            ],
        ] {
            let mut input = VecDeque::from(input);
            let sends = Cell::new(0);
            let result = execute(
                command(&[
                    "create",
                    "--roles",
                    "publisher",
                    "--idempotency-key",
                    OPERATION,
                    "password",
                    "--username",
                    "fixture",
                ]),
                |_| async { panic!("no list") },
                |_| async { panic!("no inspect") },
                |_, _| {
                    sends.set(sends.get() + 1);
                    ready(Err(AdminClientError::HumanCredentialsMissing))
                },
                |_| input.pop_front().unwrap().map(SecretString::new),
            )
            .await;
            assert_eq!(sends.get(), 0);
            let Err(error) = result else {
                panic!("input rejected")
            };
            assert_eq!(error_exit(&error), 65);
            for json_output in [false, true] {
                let mut output = Vec::new();
                write_error(&mut output, &error, 65, json_output).unwrap();
                let text = String::from_utf8(output).unwrap();
                assert!(text.contains(OPERATION));
                assert!(!text.contains(PASSWORD));
                assert!(!text.contains("different long fixture password"));
            }
        }
    }

    #[test]
    fn password_policy_counts_unicode_scalars_and_confirmation_is_exact() {
        for length in [15, 128] {
            let password = "🦀".repeat(length);
            let mut prompt = |_: &str| Ok(SecretString::new(password.clone().into_boxed_str()));
            let credential = password_credential("fixture".into(), &mut prompt).unwrap();
            let HumanCredentialInput::Password {
                password: actual, ..
            } = credential
            else {
                panic!("password")
            };
            assert_eq!(actual.expose_secret(), password);
        }
        for password in ["🦀".repeat(129), "p".repeat(1025)] {
            assert!(matches!(
                validate_password(&SecretString::new(password.into_boxed_str())),
                Err(CredentialInputError::InvalidPassword)
            ));
        }
    }

    #[tokio::test]
    async fn credential_changes_use_provider_versions_and_report_the_separate_user_version() {
        for provider in ["password", "nostr"] {
            for mode in ["add", "replace", "remove"] {
                let mut args = vec!["credentials", USER, mode, "--idempotency-key", OPERATION];
                if mode != "add" {
                    args.extend(["--expected-version", "2"]);
                }
                if mode == "remove" {
                    args.extend(["--provider", provider]);
                } else {
                    args.push(provider);
                    if provider == "password" {
                        args.extend(["--username", "fixture"]);
                    } else {
                        args.extend(["--public-key", PUBLIC_KEY]);
                    }
                }
                let prompts = Cell::new(0);
                let result = execute(
                    command(&args),
                    |_| async { panic!("no list") },
                    |_| async { panic!("no inspect") },
                    |operation, change| {
                        assert_eq!(operation.to_string(), OPERATION);
                        let (user_id, wire) = match change {
                            UserMutation::PutCredential { user_id, request } => {
                                let body = RequestBody::json(&request).unwrap();
                                (
                                    user_id,
                                    serde_json::from_slice::<serde_json::Value>(body.as_ref())
                                        .unwrap(),
                                )
                            }
                            UserMutation::RemoveCredential {
                                user_id,
                                provider: selected,
                                request,
                            } => {
                                assert_eq!(selected.as_str(), provider);
                                (user_id, serde_json::to_value(request).unwrap())
                            }
                            _ => panic!("credential mutation"),
                        };
                        assert_eq!(user_id.to_string(), USER);
                        match mode {
                            "add" => {
                                assert_eq!(wire["mode"], "create");
                                assert!(wire.get("expected_version").is_none());
                            }
                            "replace" => {
                                assert_eq!(wire["mode"], "replace");
                                assert_eq!(wire["expected_version"], 2);
                            }
                            _ => assert_eq!(wire, json!({"expected_version": 2})),
                        }
                        ready(Ok(UserMutationResponse {
                            user_id: user_id.into(),
                            version: 9,
                        }))
                    },
                    |_| {
                        prompts.set(prompts.get() + 1);
                        Ok(SecretString::new(PASSWORD))
                    },
                )
                .await
                .unwrap();
                assert_eq!(
                    prompts.get(),
                    if provider == "password" && mode != "remove" {
                        2
                    } else {
                        0
                    }
                );
                let mut output = Vec::new();
                write_output(&mut output, result, true).unwrap();
                let wire: serde_json::Value = serde_json::from_slice(&output).unwrap();
                assert_eq!(wire["receipt"]["version"], 9);
                assert_eq!(wire["idempotency_key"], OPERATION);
                assert!(!String::from_utf8(output).unwrap().contains(PASSWORD));
            }
        }
    }

    #[test]
    fn credential_arguments_reject_unknown_providers_versions_keys_usernames_and_secret_flags() {
        for args in [
            vec![
                "users",
                "credentials",
                USER,
                "remove",
                "--expected-version",
                "0",
                "--provider",
                "password",
            ],
            vec![
                "users",
                "credentials",
                USER,
                "remove",
                "--expected-version",
                "2",
                "--provider",
                "unknown",
            ],
            vec![
                "users",
                "credentials",
                USER,
                "add",
                "nostr",
                "--public-key",
                "bad-key",
            ],
            vec![
                "users",
                "credentials",
                USER,
                "add",
                "password",
                "--username",
                "Mixed.Case",
            ],
            vec![
                "users",
                "credentials",
                USER,
                "add",
                "password",
                "--username",
                "fixture",
                "--password",
                PASSWORD,
            ],
            vec![
                "users",
                "create",
                "--roles",
                "publisher",
                "password",
                "--username",
                "_fixture",
            ],
        ] {
            assert!(Arguments::try_parse_from(std::iter::once("maincopy").chain(args)).is_err());
        }
    }
}
