//! Wire-safe identity and authorization contracts for the private admin plane.

use std::{collections::BTreeSet, fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_identifier {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
        )]
        #[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
        #[cfg_attr(feature = "schema", schema(value_type = Uuid, format = Uuid))]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            pub const fn into_uuid(self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }

        impl From<$name> for Uuid {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

uuid_identifier!(InstanceId);
uuid_identifier!(UserId);
uuid_identifier!(AdminSessionId);
uuid_identifier!(LoginChallengeId);
uuid_identifier!(AgentCredentialId);
uuid_identifier!(AdminAuditEventId);

/// Whether a human account can authenticate and exercise its assigned roles.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum UserStatus {
    Enabled,
    Disabled,
}

impl UserStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "enabled" => Some(Self::Enabled),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// A built-in bundle of fixed admin scopes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    Owner,
    Administrator,
    Publisher,
}

impl UserRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Administrator => "administrator",
            Self::Publisher => "publisher",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "administrator" => Some(Self::Administrator),
            "publisher" => Some(Self::Publisher),
            _ => None,
        }
    }

    /// Returns the immutable v1 scope bundle for this role.
    pub const fn scopes(self) -> &'static [AdminScope] {
        match self {
            Self::Owner => &AdminScope::ALL,
            Self::Administrator => &AdminScope::ADMINISTRATOR,
            Self::Publisher => &AdminScope::PUBLISHER,
        }
    }
}

/// One independently enforced authority in the private admin plane.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum AdminScope {
    ContentRead,
    StatusRead,
    PreviewRead,
    ReleaseManage,
    ProfileManage,
    LightningManage,
    UserManage,
    CredentialManage,
    RoleAssign,
    AuditRead,
}

impl AdminScope {
    pub const ALL: [Self; 10] = [
        Self::ContentRead,
        Self::StatusRead,
        Self::PreviewRead,
        Self::ReleaseManage,
        Self::ProfileManage,
        Self::LightningManage,
        Self::UserManage,
        Self::CredentialManage,
        Self::RoleAssign,
        Self::AuditRead,
    ];

    pub const ADMINISTRATOR: [Self; 9] = [
        Self::ContentRead,
        Self::StatusRead,
        Self::PreviewRead,
        Self::ReleaseManage,
        Self::ProfileManage,
        Self::LightningManage,
        Self::UserManage,
        Self::CredentialManage,
        Self::AuditRead,
    ];

    pub const PUBLISHER: [Self; 4] = [
        Self::ContentRead,
        Self::StatusRead,
        Self::PreviewRead,
        Self::ReleaseManage,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContentRead => "content_read",
            Self::StatusRead => "status_read",
            Self::PreviewRead => "preview_read",
            Self::ReleaseManage => "release_manage",
            Self::ProfileManage => "profile_manage",
            Self::LightningManage => "lightning_manage",
            Self::UserManage => "user_manage",
            Self::CredentialManage => "credential_manage",
            Self::RoleAssign => "role_assign",
            Self::AuditRead => "audit_read",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "content_read" => Some(Self::ContentRead),
            "status_read" => Some(Self::StatusRead),
            "preview_read" => Some(Self::PreviewRead),
            "release_manage" => Some(Self::ReleaseManage),
            "profile_manage" => Some(Self::ProfileManage),
            "lightning_manage" => Some(Self::LightningManage),
            "user_manage" => Some(Self::UserManage),
            "credential_manage" => Some(Self::CredentialManage),
            "role_assign" => Some(Self::RoleAssign),
            "audit_read" => Some(Self::AuditRead),
            _ => None,
        }
    }
}

impl fmt::Display for AdminScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Configurable provider used to authenticate a human user.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum HumanLoginProvider {
    Password,
    Nostr,
}

impl HumanLoginProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Nostr => "nostr",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "password" => Some(Self::Password),
            "nostr" => Some(Self::Nostr),
            _ => None,
        }
    }
}

/// Returns the union of the immutable scope bundles for a user's roles.
pub fn effective_scopes(roles: impl IntoIterator<Item = UserRole>) -> BTreeSet<AdminScope> {
    roles
        .into_iter()
        .flat_map(UserRole::scopes)
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn identifiers_have_canonical_uuid_wire_values() {
        let uuid = Uuid::parse_str("12345678-1234-4234-8234-123456789abc").unwrap();
        let user_id = UserId::from_uuid(uuid);

        assert_eq!(serde_json::to_value(user_id).unwrap(), json!(uuid));
        assert_eq!(
            serde_json::from_value::<UserId>(json!(uuid)).unwrap(),
            user_id
        );
        assert_eq!(user_id.to_string(), uuid.to_string());
    }

    #[test]
    fn fixed_role_scope_mapping_enforces_the_publisher_boundary() {
        assert_eq!(UserRole::Owner.scopes(), AdminScope::ALL);
        assert!(
            UserRole::Administrator
                .scopes()
                .contains(&AdminScope::UserManage)
        );
        assert!(
            !UserRole::Administrator
                .scopes()
                .contains(&AdminScope::RoleAssign)
        );

        for allowed in [
            AdminScope::ContentRead,
            AdminScope::StatusRead,
            AdminScope::PreviewRead,
            AdminScope::ReleaseManage,
        ] {
            assert!(UserRole::Publisher.scopes().contains(&allowed), "{allowed}");
        }
        for denied in [
            AdminScope::ProfileManage,
            AdminScope::LightningManage,
            AdminScope::UserManage,
            AdminScope::CredentialManage,
            AdminScope::RoleAssign,
            AdminScope::AuditRead,
        ] {
            assert!(!UserRole::Publisher.scopes().contains(&denied), "{denied}");
        }
    }

    #[test]
    fn scope_storage_names_are_stable_and_exhaustive() {
        for scope in AdminScope::ALL {
            assert_eq!(AdminScope::parse(scope.as_str()), Some(scope));
            assert_eq!(scope.to_string(), scope.as_str());
        }
        assert_eq!(AdminScope::parse("publisher"), None);
    }

    #[test]
    fn role_unions_do_not_invent_authority() {
        let publisher = effective_scopes([UserRole::Publisher]);
        assert_eq!(publisher.len(), AdminScope::PUBLISHER.len());

        let combined = effective_scopes([UserRole::Publisher, UserRole::Administrator]);
        assert_eq!(combined.len(), AdminScope::ADMINISTRATOR.len());
        assert!(!combined.contains(&AdminScope::RoleAssign));
        assert_eq!(
            effective_scopes([UserRole::Owner]).len(),
            AdminScope::ALL.len()
        );
    }

    #[test]
    fn enums_have_closed_snake_case_wire_values() {
        assert_eq!(
            serde_json::to_value(UserStatus::Enabled).unwrap(),
            json!("enabled")
        );
        assert_eq!(
            serde_json::to_value(UserRole::Administrator).unwrap(),
            json!("administrator")
        );
        assert_eq!(
            serde_json::to_value(HumanLoginProvider::Nostr).unwrap(),
            json!("nostr")
        );
        assert!(serde_json::from_value::<UserRole>(json!("editor")).is_err());
    }
}
