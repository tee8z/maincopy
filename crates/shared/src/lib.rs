//! Wire contracts shared by Maincopy's server and operator client.

pub mod auth;
pub mod auth_api;
pub mod posts;
pub mod profile;
pub mod profile_api;
pub mod publication;

use serde::{Deserialize, Serialize};

/// Stable path for discovering the private admin contracts supported by a server.
pub const ADMIN_CAPABILITIES_PATH: &str = "/api/admin/capabilities";

/// Versioned compatibility path for discovering the v1 private admin contract.
pub const CAPABILITIES_PATH: &str = "/api/admin/v1/capabilities";

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
}
