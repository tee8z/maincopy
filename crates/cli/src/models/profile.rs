use clap::{Args, Subcommand};
use maincopy_shared::{
    auth::UserId,
    profile::{LightningAddress, ProfileDisplayName, ProfileVersion},
    profile_api::{PutActiveTipRecipientRequest, UpdateUserProfileRequest},
};
use uuid::Uuid;

#[derive(Debug, Subcommand)]
pub(crate) enum ProfileCommand {
    /// Show your profile, including whether it has been configured.
    Show,
    /// Create your profile only if it does not exist.
    Create(ProfileValues),
    /// Replace all profile fields at the exact inspected version.
    Update {
        #[arg(long, value_parser = parse_version)]
        expected_version: ProfileVersion,
        #[command(flatten)]
        values: ProfileValues,
    },
}

/// Complete command data, including retry identity, prepared before network execution.
pub(crate) enum ProfileInvocation {
    Show,
    Change {
        idempotency_key: Uuid,
        request: UpdateUserProfileRequest,
    },
}

impl ProfileCommand {
    pub(crate) fn into_invocation(self) -> ProfileInvocation {
        let (expected_version, values) = match self {
            Self::Show => return ProfileInvocation::Show,
            Self::Create(values) => (None, values),
            Self::Update {
                expected_version,
                values,
            } => (Some(expected_version), values),
        };
        ProfileInvocation::Change {
            idempotency_key: values.idempotency_key.unwrap_or_else(Uuid::new_v4),
            request: UpdateUserProfileRequest {
                expected_version,
                display_name: values.display_name,
                lightning_address: values.lightning_address,
                tips_enabled: values.tips_enabled,
            },
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct ProfileValues {
    /// Public display name. Omit to clear the name.
    #[arg(long)]
    pub(crate) display_name: Option<ProfileDisplayName>,
    /// Lowercase name@domain Lightning Address. Omit to clear the address.
    #[arg(long)]
    pub(crate) lightning_address: Option<LightningAddress>,
    /// Whether to accept tips when selected as the site's recipient.
    #[arg(long, required = true, action = clap::ArgAction::Set)]
    pub(crate) tips_enabled: bool,
    /// Retry identity; generated when omitted. Reuse with the identical command.
    #[arg(long)]
    pub(crate) idempotency_key: Option<Uuid>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TipRecipientCommand {
    /// Show the site's selected recipient and setting version.
    Show,
    /// Select an existing user. Tip links appear only while its profile is eligible.
    Set {
        user_id: Uuid,
        #[command(flatten)]
        mutation: TipRecipientVersion,
    },
    /// Remove the active recipient; articles remain readable.
    Clear(TipRecipientVersion),
}

pub(crate) enum TipRecipientInvocation {
    Show,
    Change {
        idempotency_key: Uuid,
        request: PutActiveTipRecipientRequest,
    },
}

impl TipRecipientCommand {
    pub(crate) fn into_invocation(self) -> TipRecipientInvocation {
        let (user_id, mutation) = match self {
            Self::Show => return TipRecipientInvocation::Show,
            Self::Set { user_id, mutation } => (Some(UserId::from_uuid(user_id)), mutation),
            Self::Clear(mutation) => (None, mutation),
        };
        TipRecipientInvocation::Change {
            idempotency_key: mutation.idempotency_key.unwrap_or_else(Uuid::new_v4),
            request: PutActiveTipRecipientRequest {
                user_id,
                expected_version: mutation.expected_version,
            },
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct TipRecipientVersion {
    #[arg(long, value_parser = parse_version)]
    pub(crate) expected_version: ProfileVersion,
    /// Retry identity; generated when omitted. Reuse with the identical command.
    #[arg(long)]
    pub(crate) idempotency_key: Option<Uuid>,
}

fn parse_version(value: &str) -> Result<ProfileVersion, &'static str> {
    let version = value
        .parse::<u64>()
        .map_err(|_| "expected a positive resource version")?;
    ProfileVersion::new(version)
        .map_err(|_| "expected a positive resource version within the SQLite range")
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;
    use crate::models::{Arguments, Command};

    #[test]
    fn profile_updates_require_an_explicit_version_and_tip_choice() {
        let arguments = Arguments::try_parse_from([
            "maincopy",
            "profile",
            "update",
            "--expected-version",
            "3",
            "--tips-enabled",
            "false",
        ])
        .unwrap();
        let Command::Profile {
            command:
                ProfileCommand::Update {
                    expected_version,
                    values,
                },
        } = arguments.command
        else {
            panic!("expected profile update")
        };
        assert_eq!(expected_version.into_u64(), 3);
        assert!(values.display_name.is_none());
        assert!(values.lightning_address.is_none());
        assert!(!values.tips_enabled);
        for arguments in [
            vec!["maincopy", "profile", "update", "--tips-enabled", "false"],
            vec!["maincopy", "profile", "create"],
            vec![
                "maincopy",
                "profile",
                "update",
                "--expected-version",
                "0",
                "--tips-enabled",
                "false",
            ],
            vec![
                "maincopy",
                "profile",
                "update",
                "--expected-version",
                "18446744073709551615",
                "--tips-enabled",
                "false",
            ],
            vec![
                "maincopy",
                "profile",
                "create",
                "--lightning-address",
                "UPPER@example.com",
                "--tips-enabled",
                "true",
            ],
            vec!["maincopy", "tip-recipient", "clear"],
            vec![
                "maincopy",
                "tip-recipient",
                "set",
                "not-a-uuid",
                "--expected-version",
                "1",
            ],
        ] {
            assert!(Arguments::try_parse_from(arguments).is_err());
        }
    }

    #[test]
    fn recipient_commands_preserve_selection_version_and_retry_identity() {
        let id = Uuid::new_v4().to_string();
        let arguments = Arguments::try_parse_from([
            "maincopy",
            "tip-recipient",
            "set",
            &id,
            "--expected-version",
            "4",
            "--idempotency-key",
            &id,
        ])
        .unwrap();
        let Command::TipRecipient {
            command: TipRecipientCommand::Set { user_id, mutation },
        } = arguments.command
        else {
            panic!("expected recipient selection")
        };
        assert_eq!(user_id.to_string(), id);
        assert_eq!(mutation.expected_version.into_u64(), 4);
        assert_eq!(mutation.idempotency_key, Some(user_id));
        let arguments = Arguments::try_parse_from([
            "maincopy",
            "tip-recipient",
            "clear",
            "--expected-version",
            "5",
        ])
        .unwrap();
        assert!(matches!(
            arguments.command,
            Command::TipRecipient {
                command: TipRecipientCommand::Clear(_)
            }
        ));
    }

    #[test]
    fn profile_preparation_preserves_replace_and_create_preconditions_in_the_wire_command() {
        let key = Uuid::new_v4();
        for version in [None, Some(ProfileVersion::new(5).unwrap())] {
            let values = ProfileValues {
                display_name: Some("Alice".parse().unwrap()),
                lightning_address: Some("alice@example.test".parse().unwrap()),
                tips_enabled: true,
                idempotency_key: Some(key),
            };
            let command = match version {
                None => ProfileCommand::Create(values),
                Some(expected_version) => ProfileCommand::Update {
                    expected_version,
                    values,
                },
            };
            let ProfileInvocation::Change {
                idempotency_key,
                request,
            } = command.into_invocation()
            else {
                panic!("expected mutation")
            };
            assert_eq!(idempotency_key, key);
            assert_eq!(request.expected_version, version);
            assert_eq!(
                serde_json::to_value(request).unwrap(),
                serde_json::json!({"display_name":"Alice", "lightning_address":"alice@example.test", "tips_enabled":true, "expected_version":version})
            );
        }
        let command = ProfileCommand::Create(ProfileValues {
            display_name: None,
            lightning_address: None,
            tips_enabled: false,
            idempotency_key: None,
        });
        let ProfileInvocation::Change {
            idempotency_key,
            request,
        } = command.into_invocation()
        else {
            panic!("expected mutation")
        };
        assert_eq!(idempotency_key.get_version_num(), 4);
        assert!(request.display_name.is_none());
        assert!(request.lightning_address.is_none());
        assert!(!request.tips_enabled);
        assert!(matches!(
            ProfileCommand::Show.into_invocation(),
            ProfileInvocation::Show
        ));
    }

    #[test]
    fn recipient_preparation_pins_set_and_clear_commands_before_submission() {
        let key = Uuid::new_v4();
        let user = Uuid::new_v4();
        for selected in [true, false] {
            let mutation = TipRecipientVersion {
                expected_version: ProfileVersion::new(6).unwrap(),
                idempotency_key: Some(key),
            };
            let command = if selected {
                TipRecipientCommand::Set {
                    user_id: user,
                    mutation,
                }
            } else {
                TipRecipientCommand::Clear(mutation)
            };
            let TipRecipientInvocation::Change {
                idempotency_key,
                request,
            } = command.into_invocation()
            else {
                panic!("expected mutation")
            };
            assert_eq!(idempotency_key, key);
            assert_eq!(request.user_id, selected.then_some(UserId::from_uuid(user)));
            assert_eq!(request.expected_version.into_u64(), 6);
        }
        let command = TipRecipientCommand::Clear(TipRecipientVersion {
            expected_version: ProfileVersion::new(1).unwrap(),
            idempotency_key: None,
        });
        let TipRecipientInvocation::Change {
            idempotency_key, ..
        } = command.into_invocation()
        else {
            panic!("expected mutation")
        };
        assert_eq!(idempotency_key.get_version_num(), 4);
        assert!(matches!(
            TipRecipientCommand::Show.into_invocation(),
            TipRecipientInvocation::Show
        ));
    }
}
