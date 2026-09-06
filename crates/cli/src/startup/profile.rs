use std::{future::Future, io};

use maincopy_shared::profile_api::{
    ActiveTipRecipientResponse, PutActiveTipRecipientRequest, UpdateUserProfileRequest,
    UserProfileResponse,
};
use serde_json::json;
use uuid::Uuid;

use super::CliError;
use crate::{
    client::AdminClientError,
    models::{ProfileInvocation, TipRecipientInvocation},
};

pub(super) enum ProfileOutput {
    Profile(Option<UserProfileResponse>),
    ProfileChanged {
        idempotency_key: Uuid,
        profile: UserProfileResponse,
    },
    Recipient(ActiveTipRecipientResponse),
    RecipientChanged {
        idempotency_key: Uuid,
        recipient: ActiveTipRecipientResponse,
    },
}

pub(super) async fn execute<Show, ShowFuture, Save, SaveFuture>(
    command: ProfileInvocation,
    show: Show,
    save: Save,
) -> Result<ProfileOutput, CliError>
where
    Show: FnOnce() -> ShowFuture,
    ShowFuture: Future<Output = Result<Option<UserProfileResponse>, AdminClientError>>,
    Save: FnOnce(Uuid, UpdateUserProfileRequest) -> SaveFuture,
    SaveFuture: Future<Output = Result<UserProfileResponse, AdminClientError>>,
{
    let (idempotency_key, request) = match command {
        ProfileInvocation::Show => return Ok(ProfileOutput::Profile(show().await?)),
        ProfileInvocation::Change {
            idempotency_key,
            request,
        } => (idempotency_key, request),
    };
    let profile =
        save(idempotency_key, request)
            .await
            .map_err(|source| CliError::ProfileChange {
                idempotency_key,
                source,
            })?;
    Ok(ProfileOutput::ProfileChanged {
        idempotency_key,
        profile,
    })
}

pub(super) async fn execute_recipient<Show, ShowFuture, Save, SaveFuture>(
    command: TipRecipientInvocation,
    show: Show,
    save: Save,
) -> Result<ProfileOutput, CliError>
where
    Show: FnOnce() -> ShowFuture,
    ShowFuture: Future<Output = Result<ActiveTipRecipientResponse, AdminClientError>>,
    Save: FnOnce(Uuid, PutActiveTipRecipientRequest) -> SaveFuture,
    SaveFuture: Future<Output = Result<ActiveTipRecipientResponse, AdminClientError>>,
{
    let (idempotency_key, request) = match command {
        TipRecipientInvocation::Show => {
            return Ok(ProfileOutput::Recipient(show().await?));
        }
        TipRecipientInvocation::Change {
            idempotency_key,
            request,
        } => (idempotency_key, request),
    };
    let recipient =
        save(idempotency_key, request)
            .await
            .map_err(|source| CliError::ProfileChange {
                idempotency_key,
                source,
            })?;
    Ok(ProfileOutput::RecipientChanged {
        idempotency_key,
        recipient,
    })
}

pub(super) fn write_output(
    mut output: impl io::Write,
    result: ProfileOutput,
    json_output: bool,
) -> Result<(), CliError> {
    if json_output {
        let value = match result {
            ProfileOutput::Profile(profile) => json!({"profile": profile}),
            ProfileOutput::ProfileChanged {
                idempotency_key,
                profile,
            } => json!({"idempotency_key": idempotency_key, "profile": profile}),
            ProfileOutput::Recipient(recipient) => json!({"tip_recipient": recipient}),
            ProfileOutput::RecipientChanged {
                idempotency_key,
                recipient,
            } => json!({"idempotency_key": idempotency_key, "tip_recipient": recipient}),
        };
        writeln!(output, "{value}")?;
        return Ok(());
    }
    match result {
        ProfileOutput::Profile(profile) => write_profile(&mut output, profile.as_ref())?,
        ProfileOutput::ProfileChanged {
            idempotency_key,
            profile,
        } => {
            writeln!(output, "Accepted profile change: {idempotency_key}")?;
            write_profile(&mut output, Some(&profile))?;
            writeln!(output, "Inspect current state: maincopy profile show")?;
        }
        ProfileOutput::Recipient(recipient) => write_recipient(&mut output, &recipient)?,
        ProfileOutput::RecipientChanged {
            idempotency_key,
            recipient,
        } => {
            writeln!(output, "Accepted recipient change: {idempotency_key}")?;
            write_recipient(&mut output, &recipient)?;
            writeln!(output, "Inspect current state: maincopy tip-recipient show")?;
        }
    }
    Ok(())
}

fn write_profile(
    mut output: impl io::Write,
    profile: Option<&UserProfileResponse>,
) -> io::Result<()> {
    let Some(profile) = profile else {
        return writeln!(
            output,
            "Profile is not configured. Use maincopy profile create."
        );
    };
    writeln!(output, "User: {}", profile.user_id)?;
    writeln!(output, "Profile version: {}", profile.version.into_u64())?;
    writeln!(
        output,
        "Display name: {}",
        profile
            .display_name
            .as_ref()
            .map_or("(unset)", |name| name.as_str())
    )?;
    writeln!(
        output,
        "Lightning Address: {}",
        profile
            .lightning_address
            .as_ref()
            .map_or("(unset)", |address| address.as_str())
    )?;
    writeln!(output, "Tips enabled: {}", profile.tips_enabled)
}

fn write_recipient(
    mut output: impl io::Write,
    recipient: &ActiveTipRecipientResponse,
) -> io::Result<()> {
    writeln!(
        output,
        "Recipient setting version: {}",
        recipient.version.into_u64()
    )?;
    match recipient.user_id {
        Some(user_id) => {
            writeln!(output, "Selected user: {user_id}")?;
            writeln!(
                output,
                "Tip links require an enabled account, tips enabled, and a valid Lightning Address."
            )
        }
        None => writeln!(
            output,
            "No active recipient. Articles remain readable without tip links."
        ),
    }
}

#[cfg(test)]
mod tests {
    use maincopy_shared::profile::ProfileVersion;
    use std::{cell::Cell, future::ready};

    use super::*;
    use crate::{
        client::{AdminClientError, AdminProblem},
        startup::{error_exit, write_error},
    };
    use reqwest::StatusCode;

    fn profile() -> UserProfileResponse {
        serde_json::from_value(json!({"user_id":Uuid::from_u128(1), "display_name":"Alice", "lightning_address":"alice@example.test", "tips_enabled":true, "version":2, "updated_at":"2026-09-05T12:00:00Z"})).unwrap()
    }

    fn recipient(selected: bool) -> ActiveTipRecipientResponse {
        serde_json::from_value(json!({"user_id":selected.then_some(Uuid::from_u128(1)), "version":3, "updated_at":"2026-09-05T12:00:00Z"})).unwrap()
    }

    #[tokio::test]
    async fn profile_reads_never_submit_a_mutation() {
        let saved = Cell::new(false);
        let result = execute(
            ProfileInvocation::Show,
            || ready(Ok(None)),
            |_, _| {
                saved.set(true);
                ready(Ok(profile()))
            },
        )
        .await
        .unwrap();
        assert!(matches!(result, ProfileOutput::Profile(None)));
        assert!(!saved.get());
        let result = execute_recipient(
            TipRecipientInvocation::Show,
            || ready(Ok(recipient(false))),
            |_, _| {
                saved.set(true);
                ready(Ok(recipient(true)))
            },
        )
        .await
        .unwrap();
        assert!(matches!(result, ProfileOutput::Recipient(value) if value.user_id.is_none()));
        assert!(!saved.get());
    }

    #[tokio::test]
    async fn profile_mutations_submit_the_prepared_command_and_preserve_uncertain_outcomes() {
        for succeeded in [true, false] {
            let key = Uuid::new_v4();
            let loaded = Cell::new(false);
            let submitted = Cell::new(false);
            let command = ProfileInvocation::Change {
                idempotency_key: key,
                request: UpdateUserProfileRequest {
                    expected_version: Some(ProfileVersion::new(1).unwrap()),
                    display_name: Some("Alice".parse().unwrap()),
                    lightning_address: Some("alice@example.test".parse().unwrap()),
                    tips_enabled: true,
                },
            };
            let result = execute(command,
                || { loaded.set(true); ready(Ok(None)) },
                |operation, request| {
                    submitted.set(true);
                    assert_eq!(operation, key);
                    assert_eq!(serde_json::to_value(request).unwrap(), json!({"expected_version":1, "display_name":"Alice", "lightning_address":"alice@example.test", "tips_enabled":true}));
                    ready(if succeeded { Ok(profile()) } else { Err(AdminClientError::HumanCredentialsMissing) })
                },
            ).await;
            assert!(!loaded.get());
            assert!(submitted.get());
            match result {
                Ok(ProfileOutput::ProfileChanged {
                    idempotency_key,
                    profile: value,
                }) => {
                    assert!(succeeded);
                    assert_eq!(idempotency_key, key);
                    assert_eq!(value, profile());
                }
                Err(CliError::ProfileChange {
                    idempotency_key,
                    source: AdminClientError::HumanCredentialsMissing,
                }) => {
                    assert!(!succeeded);
                    assert_eq!(idempotency_key, key);
                }
                _ => panic!("unexpected mutation result"),
            }
        }
    }

    #[tokio::test]
    async fn recipient_mutations_submit_the_selected_state_and_preserve_retry_identity() {
        for succeeded in [true, false] {
            let key = Uuid::new_v4();
            let loaded = Cell::new(false);
            let submitted = Cell::new(false);
            let command = TipRecipientInvocation::Change {
                idempotency_key: key,
                request: PutActiveTipRecipientRequest {
                    expected_version: ProfileVersion::new(2).unwrap(),
                    user_id: None,
                },
            };
            let result = execute_recipient(
                command,
                || {
                    loaded.set(true);
                    ready(Ok(recipient(true)))
                },
                |operation, request| {
                    submitted.set(true);
                    assert_eq!(operation, key);
                    assert_eq!(
                        serde_json::to_value(request).unwrap(),
                        json!({"user_id":null, "expected_version":2})
                    );
                    ready(if succeeded {
                        Ok(recipient(false))
                    } else {
                        Err(AdminClientError::HumanCredentialsMissing)
                    })
                },
            )
            .await;
            assert!(!loaded.get());
            assert!(submitted.get());
            match result {
                Ok(ProfileOutput::RecipientChanged {
                    idempotency_key,
                    recipient: value,
                }) => {
                    assert!(succeeded);
                    assert_eq!(idempotency_key, key);
                    assert_eq!(value, recipient(false));
                }
                Err(CliError::ProfileChange {
                    idempotency_key,
                    source: AdminClientError::HumanCredentialsMissing,
                }) => {
                    assert!(!succeeded);
                    assert_eq!(idempotency_key, key);
                }
                _ => panic!("unexpected mutation result"),
            }
        }
    }

    #[test]
    fn profile_output_distinguishes_unconfigured_current_and_accepted_state() {
        for json_output in [false, true] {
            for (result, expected) in [
                (
                    ProfileOutput::Profile(None),
                    if json_output {
                        "\"profile\":null"
                    } else {
                        "not configured"
                    },
                ),
                (ProfileOutput::Profile(Some(profile())), "Alice"),
                (
                    ProfileOutput::ProfileChanged {
                        idempotency_key: Uuid::nil(),
                        profile: profile(),
                    },
                    "00000000-0000-0000-0000-000000000000",
                ),
                (
                    ProfileOutput::Recipient(recipient(false)),
                    if json_output {
                        "\"user_id\":null"
                    } else {
                        "No active recipient"
                    },
                ),
                (
                    ProfileOutput::RecipientChanged {
                        idempotency_key: Uuid::nil(),
                        recipient: recipient(true),
                    },
                    "00000000-0000-0000-0000-000000000000",
                ),
            ] {
                let mut output = Vec::new();
                write_output(&mut output, result, json_output).unwrap();
                let output = String::from_utf8(output).unwrap();
                assert!(output.contains(expected), "{output}");
                if json_output {
                    assert!(serde_json::from_str::<serde_json::Value>(&output).is_ok());
                }
            }
        }
    }

    #[test]
    fn failed_profile_changes_preserve_retry_identity_and_error_category() {
        for (status, expected_exit) in [
            (StatusCode::BAD_REQUEST, 65),
            (StatusCode::FORBIDDEN, 77),
            (StatusCode::PRECONDITION_FAILED, 75),
            (StatusCode::SERVICE_UNAVAILABLE, 69),
        ] {
            let operation = Uuid::new_v4();
            let error = CliError::ProfileChange {
                idempotency_key: operation,
                source: AdminClientError::HttpStatus {
                    status,
                    problem: Some(AdminProblem {
                        code: "profile_error".into(),
                        message: "safe failure".into(),
                    }),
                    request_id: Some(Uuid::nil()),
                },
            };
            assert_eq!(error_exit(&error), expected_exit);
            for json_output in [false, true] {
                let mut output = Vec::new();
                write_error(&mut output, &error, expected_exit, json_output).unwrap();
                let output = String::from_utf8(output).unwrap();
                assert!(output.contains(&operation.to_string()));
                assert!(output.contains("safe failure"));
                if json_output {
                    assert!(serde_json::from_str::<serde_json::Value>(&output).is_ok());
                }
            }
        }
    }
}
