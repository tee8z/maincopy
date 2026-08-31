//! SQLite-owned user profiles and the active Lightning tip recipient.

pub(crate) mod store;

pub(crate) use store::{
    ProfileCommandError, ProfileLoadError, ProfileMutationError, ProfileStore, SetTipRecipient,
    StoredTipRecipientSetting, StoredUserProfile, TipRecipientProjection, UpdateProfile,
};

use maincopy_shared::profile::ProfileVersion;

/// The exact durable state a profile update is allowed to replace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProfilePrecondition {
    Create,
    Replace(ProfileVersion),
}

impl From<Option<ProfileVersion>> for ProfilePrecondition {
    fn from(expected_version: Option<ProfileVersion>) -> Self {
        match expected_version {
            Some(version) => Self::Replace(version),
            None => Self::Create,
        }
    }
}
