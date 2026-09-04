//! Managed Git source configuration and durable synchronization state.

use maincopy_shared::source::{
    GitBranchName, RepositoryContentSubdirectory, SourceConfigurationVersion, SourcePollInterval,
    SshCredentialName, SshRemote,
};

pub(crate) mod admin;
pub(crate) mod store;
pub(crate) mod ui;

/// Validated non-secret settings accepted by the offline source command.
#[derive(Clone, Debug)]
pub(crate) struct ManagedSourceConfigurationInput {
    pub(crate) remote: SshRemote,
    pub(crate) branch: GitBranchName,
    pub(crate) content_subdirectory: RepositoryContentSubdirectory,
    pub(crate) credential_name: SshCredentialName,
    pub(crate) poll_interval_seconds: SourcePollInterval,
    pub(crate) expected_version: Option<SourceConfigurationVersion>,
}
