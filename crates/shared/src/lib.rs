//! Wire contracts shared by Maincopy's server and operator client.

pub mod publication;

use serde::{Deserialize, Serialize};

/// Stable path for discovering the private admin contracts supported by a server.
pub const ADMIN_CAPABILITIES_PATH: &str = "/api/admin/capabilities";

/// Versioned compatibility path for discovering the v1 private admin contract.
pub const CAPABILITIES_PATH: &str = "/api/admin/v1/capabilities";

/// Default local named pipe used by the server and operator client on Windows.
pub const DEFAULT_WINDOWS_ADMIN_PIPE: &str = r"\\.\pipe\maincopy";

/// Reports whether `name` is a canonical local Windows named-pipe name.
///
/// Windows accepts pipe names in the `\\.\pipe\<name>` namespace. The leaf
/// cannot contain another backslash, and the complete UTF-16 name is limited
/// to 256 code units.
pub fn is_valid_windows_admin_pipe_name(name: &str) -> bool {
    const PREFIX: &str = r"\\.\pipe\";

    let Some(leaf) = name.strip_prefix(PREFIX) else {
        return false;
    };
    !leaf.is_empty() && !leaf.contains(['\\', '\0']) && name.encode_utf16().count() <= 256
}

/// Admin API versions understood by this contract crate.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum AdminApiVersion {
    #[serde(rename = "v1")]
    V1,
}

/// Versions of the currently advertised admin features.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct FeatureVersions {
    pub capabilities: CapabilityContractVersion,
}

/// Versions of the capabilities response contract.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub enum CapabilityContractVersion {
    #[serde(rename = "v1")]
    V1,
}

/// Feature contract versions supported by one running Maincopy server.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct SupportedFeatureContracts {
    pub capabilities: Vec<CapabilityContractVersion>,
}

/// Version-neutral discovery contract for one running Maincopy server.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct AdminApiCapabilities {
    pub api_versions: Vec<AdminApiVersion>,
    pub feature_contracts: SupportedFeatureContracts,
}

/// Contract versions supported by one running Maincopy server.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(utoipa::ToSchema))]
pub struct Capabilities {
    pub api_version: AdminApiVersion,
    pub features: FeatureVersions,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn admin_api_capabilities_have_a_stable_bidirectional_wire_contract() {
        let capabilities = AdminApiCapabilities {
            api_versions: vec![AdminApiVersion::V1],
            feature_contracts: SupportedFeatureContracts {
                capabilities: vec![CapabilityContractVersion::V1],
            },
        };

        let value = serde_json::to_value(&capabilities).unwrap();
        assert_eq!(
            value,
            json!({
                "api_versions": ["v1"],
                "feature_contracts": { "capabilities": ["v1"] }
            })
        );
        assert_eq!(
            serde_json::from_value::<AdminApiCapabilities>(value).unwrap(),
            capabilities
        );
    }

    #[test]
    fn capabilities_have_a_stable_bidirectional_wire_contract() {
        let capabilities = Capabilities {
            api_version: AdminApiVersion::V1,
            features: FeatureVersions {
                capabilities: CapabilityContractVersion::V1,
            },
        };

        let value = serde_json::to_value(capabilities).unwrap();
        assert_eq!(
            value,
            json!({
                "api_version": "v1",
                "features": { "capabilities": "v1" }
            })
        );
        assert_eq!(
            serde_json::from_value::<Capabilities>(value).unwrap(),
            capabilities
        );
    }

    #[test]
    fn windows_admin_pipe_names_are_local_and_bounded() {
        assert!(is_valid_windows_admin_pipe_name(DEFAULT_WINDOWS_ADMIN_PIPE));
        assert!(is_valid_windows_admin_pipe_name(r"\\.\pipe\maincopy.test"));

        for invalid in [
            "",
            r"maincopy",
            r"\\server\pipe\maincopy",
            r"\\.\pipe\",
            "\\\\.\\pipe\\nested\\name",
            "\\\\.\\pipe\\contains\0nul",
        ] {
            assert!(!is_valid_windows_admin_pipe_name(invalid), "{invalid:?}");
        }

        let mut overlong = format!(r"\\.\pipe\{}", "x".repeat(247));
        assert_eq!(overlong.encode_utf16().count(), 256);
        assert!(is_valid_windows_admin_pipe_name(&overlong));
        overlong.push('x');
        assert!(!is_valid_windows_admin_pipe_name(&overlong));
    }
}
