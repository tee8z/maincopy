//! Versioned wire contracts for profile-backed Lightning tip settings.

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use time::{OffsetDateTime, UtcOffset};

use crate::{
    auth::UserId,
    profile::{LightningAddress, ProfileDisplayName, ProfileVersion},
};

pub const CURRENT_USER_PROFILE_PATH: &str = "/api/admin/v1/profile";
pub const ACTIVE_TIP_RECIPIENT_PATH: &str = "/api/admin/v1/lightning/tip-recipient";

/// The authenticated user's mutable public profile.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct UserProfileResponse {
    pub user_id: UserId,
    pub display_name: Option<ProfileDisplayName>,
    pub lightning_address: Option<LightningAddress>,
    pub tips_enabled: bool,
    pub version: ProfileVersion,
    #[serde(
        serialize_with = "time::serde::rfc3339::serialize",
        deserialize_with = "deserialize_utc_timestamp"
    )]
    #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
    pub updated_at: OffsetDateTime,
}

/// Creates or replaces the authenticated user's mutable public profile.
///
/// A missing expected version is a create-only precondition. A positive version
/// is an update-only compare-and-swap precondition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct UpdateUserProfileRequest {
    pub display_name: Option<ProfileDisplayName>,
    pub lightning_address: Option<LightningAddress>,
    pub tips_enabled: bool,
    #[serde(default)]
    pub expected_version: Option<ProfileVersion>,
}

/// The versioned site setting selecting the only active tip recipient.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct ActiveTipRecipientResponse {
    pub user_id: Option<UserId>,
    pub version: ProfileVersion,
    #[serde(
        serialize_with = "time::serde::rfc3339::serialize",
        deserialize_with = "deserialize_utc_timestamp"
    )]
    #[cfg_attr(feature = "schema", schema(value_type = String, format = DateTime))]
    pub updated_at: OffsetDateTime,
}

/// Selects or clears the active site tip recipient at one exact setting version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct PutActiveTipRecipientRequest {
    pub user_id: Option<UserId>,
    pub expected_version: ProfileVersion,
}

fn deserialize_utc_timestamp<'de, DeserializerType>(
    deserializer: DeserializerType,
) -> Result<OffsetDateTime, DeserializerType::Error>
where
    DeserializerType: Deserializer<'de>,
{
    let timestamp = time::serde::rfc3339::deserialize(deserializer)?;
    if timestamp.offset() == UtcOffset::UTC {
        Ok(timestamp)
    } else {
        Err(DeserializerType::Error::custom(
            "updated_at must use the UTC offset",
        ))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    fn user_id() -> UserId {
        UserId::from_uuid(Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").unwrap())
    }

    fn updated_at() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn version(value: u64) -> ProfileVersion {
        ProfileVersion::new(value).unwrap()
    }

    #[test]
    fn profile_and_recipient_paths_are_versioned_admin_resources() {
        assert_eq!(CURRENT_USER_PROFILE_PATH, "/api/admin/v1/profile");
        assert_eq!(
            ACTIVE_TIP_RECIPIENT_PATH,
            "/api/admin/v1/lightning/tip-recipient"
        );
    }

    #[test]
    fn user_profile_response_has_a_stable_bidirectional_wire_contract() {
        let response = UserProfileResponse {
            user_id: user_id(),
            display_name: Some(ProfileDisplayName::parse("Alice Writer").unwrap()),
            lightning_address: Some(LightningAddress::parse("alice@example.com").unwrap()),
            tips_enabled: true,
            version: version(7),
            updated_at: updated_at(),
        };

        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(
            value,
            json!({
                "user_id": "123e4567-e89b-42d3-a456-426614174000",
                "display_name": "Alice Writer",
                "lightning_address": "alice@example.com",
                "tips_enabled": true,
                "version": 7,
                "updated_at": "2023-11-14T22:13:20Z"
            })
        );
        assert_eq!(
            serde_json::from_value::<UserProfileResponse>(value).unwrap(),
            response
        );
    }

    #[test]
    fn response_readers_ignore_fields_added_by_a_newer_server() {
        let profile = serde_json::from_value::<UserProfileResponse>(json!({
            "user_id": "123e4567-e89b-42d3-a456-426614174000",
            "display_name": "Alice Writer",
            "lightning_address": "alice@example.com",
            "tips_enabled": true,
            "version": 7,
            "updated_at": "2023-11-14T22:13:20Z",
            "future_profile_field": {"version": 2}
        }))
        .unwrap();
        assert_eq!(profile.version, version(7));

        let recipient = serde_json::from_value::<ActiveTipRecipientResponse>(json!({
            "user_id": null,
            "version": 3,
            "updated_at": "2023-11-14T22:13:20Z",
            "future_recipient_field": true
        }))
        .unwrap();
        assert_eq!(recipient.version, version(3));
    }

    #[test]
    fn profile_update_request_supports_create_only_and_cleared_profile_fields() {
        let value = json!({
            "display_name": null,
            "lightning_address": null,
            "tips_enabled": false,
            "expected_version": null
        });
        let request = serde_json::from_value::<UpdateUserProfileRequest>(value.clone()).unwrap();

        assert_eq!(
            request,
            UpdateUserProfileRequest {
                display_name: None,
                lightning_address: None,
                tips_enabled: false,
                expected_version: None,
            }
        );
        assert_eq!(serde_json::to_value(request).unwrap(), value);

        let omitted = serde_json::from_value::<UpdateUserProfileRequest>(json!({
            "display_name": null,
            "lightning_address": null,
            "tips_enabled": false
        }))
        .unwrap();
        assert_eq!(omitted.expected_version, None);
    }

    #[test]
    fn active_recipient_contract_selects_or_clears_one_typed_user() {
        let selected = ActiveTipRecipientResponse {
            user_id: Some(user_id()),
            version: version(3),
            updated_at: updated_at(),
        };
        let selected_value = serde_json::to_value(&selected).unwrap();
        assert_eq!(
            selected_value,
            json!({
                "user_id": "123e4567-e89b-42d3-a456-426614174000",
                "version": 3,
                "updated_at": "2023-11-14T22:13:20Z"
            })
        );
        assert_eq!(
            serde_json::from_value::<ActiveTipRecipientResponse>(selected_value).unwrap(),
            selected
        );

        let cleared = PutActiveTipRecipientRequest {
            user_id: None,
            expected_version: version(3),
        };
        assert_eq!(
            serde_json::to_value(cleared).unwrap(),
            json!({"user_id": null, "expected_version": 3})
        );
    }

    #[test]
    fn contracts_reject_unknown_fields_and_invalid_nested_values() {
        assert!(
            serde_json::from_value::<UpdateUserProfileRequest>(json!({
                "display_name": "Alice",
                "lightning_address": "alice@example.com",
                "tips_enabled": true,
                "expected_version": 1,
                "invoice": "not accepted"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<UpdateUserProfileRequest>(json!({
                "display_name": "Alice",
                "lightning_address": "Alice@example.com",
                "tips_enabled": true,
                "expected_version": 1
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<PutActiveTipRecipientRequest>(json!({
                "user_id": "not-a-uuid",
                "expected_version": 1
            }))
            .is_err()
        );
    }

    #[test]
    fn zero_versions_and_non_utc_timestamps_are_rejected() {
        assert!(
            serde_json::from_value::<UpdateUserProfileRequest>(json!({
                "display_name": null,
                "lightning_address": null,
                "tips_enabled": false,
                "expected_version": 0
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<PutActiveTipRecipientRequest>(json!({
                "user_id": null,
                "expected_version": 0
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ActiveTipRecipientResponse>(json!({
                "user_id": null,
                "version": 1,
                "updated_at": "2023-11-14T17:13:20-05:00"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<PutActiveTipRecipientRequest>(json!({
                "user_id": null,
                "expected_version": 9223372036854775808_u64
            }))
            .is_err()
        );
    }
}
