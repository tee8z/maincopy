//! Owner-managed channel settings. Credentials are accepted from the admin
//! portal, stored in the application database, and never rendered back.

use std::fmt;

use serde::Deserialize;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::domain::auth::store::MutationAuditContext;

const MAX_SUBSTACK_SESSION_BYTES: usize = 1024;
const MAX_X_CREDENTIAL_BYTES: usize = 256;
const MAX_SUBDOMAIN_BYTES: usize = 63;
const SUBSTACK_HOST_SUFFIX: &str = ".substack.com";
/// Substack signs its session cookie in this URL-encoded form.
const SUBSTACK_SESSION_PREFIX: &str = "s%3A";

/// Every place a teaser can be posted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Channel {
    Substack,
    X,
}

impl Channel {
    pub(crate) const ALL: [Self; 2] = [Self::Substack, Self::X];

    /// The stable storage and route name.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Substack => "substack",
            Self::X => "x",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|channel| channel.as_str() == value)
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Substack => "Substack",
            Self::X => "X",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChannelMode {
    Paused,
    Enabled,
}

impl ChannelMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::Enabled => "enabled",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "paused" => Some(Self::Paused),
            "enabled" => Some(Self::Enabled),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SettingsError {
    #[error(
        "Enter the publication's Substack address, for example example or example.substack.com."
    )]
    Subdomain,
    #[error(
        "Paste the value of the substack.sid cookie from a signed-in browser. It starts with s%3A."
    )]
    SubstackSession,
    #[error("Enter the API key, API secret, access token, and access token secret together.")]
    XCredentials,
    #[error("Save the channel's credentials before changing whether it is enabled.")]
    CredentialsRequired,
}

/// Provider credential text: bounded, printable ASCII, erased on drop.
pub(crate) struct ChannelSecret(Zeroizing<String>);

impl ChannelSecret {
    fn parse(value: &str, max_bytes: usize) -> Option<Self> {
        (!value.is_empty()
            && value.len() <= max_bytes
            && value.bytes().all(|byte| byte.is_ascii_graphic()))
        .then(|| Self(Zeroizing::new(value.to_owned())))
    }

    pub(super) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ChannelSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ChannelSecret(<redacted>)")
    }
}

/// The `<name>` in `<name>.substack.com`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SubstackSubdomain(String);

impl SubstackSubdomain {
    /// Accept the bare name, the host, or a pasted publication address.
    pub(crate) fn parse(value: &str) -> Result<Self, SettingsError> {
        let value = value.trim().to_ascii_lowercase();
        let host = value
            .strip_prefix("https://")
            .unwrap_or(&value)
            .split('/')
            .next()
            .unwrap_or_default();
        let name = host.strip_suffix(SUBSTACK_HOST_SUFFIX).unwrap_or(host);
        let valid = !name.is_empty()
            && name.len() <= MAX_SUBDOMAIN_BYTES
            && !name.starts_with('-')
            && !name.ends_with('-')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        match valid {
            true => Ok(Self(name.to_owned())),
            false => Err(SettingsError::Subdomain),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug)]
pub(crate) struct SubstackSettings {
    pub mode: ChannelMode,
    pub subdomain: SubstackSubdomain,
    pub session: ChannelSecret,
}

/// Pasted cookies often carry their name, quotes, or a trailing separator.
pub(crate) fn substack_session(value: &str) -> Result<ChannelSecret, SettingsError> {
    let value = value.trim().trim_end_matches(';');
    let value = ["substack.sid=", "connect.sid="]
        .into_iter()
        .find_map(|name| value.strip_prefix(name))
        .unwrap_or(value)
        .trim_matches('"');
    if !value.starts_with(SUBSTACK_SESSION_PREFIX) {
        return Err(SettingsError::SubstackSession);
    }
    ChannelSecret::parse(value, MAX_SUBSTACK_SESSION_BYTES).ok_or(SettingsError::SubstackSession)
}

/// The four static values of an X app with read and write user access.
#[derive(Debug)]
pub(crate) struct XCredentials {
    pub api_key: ChannelSecret,
    pub api_secret: ChannelSecret,
    pub access_token: ChannelSecret,
    pub access_token_secret: ChannelSecret,
}

impl XCredentials {
    /// All four blank keeps the saved credentials; a partial set is an error.
    pub(crate) fn parse_optional(
        api_key: &str,
        api_secret: &str,
        access_token: &str,
        access_token_secret: &str,
    ) -> Result<Option<Self>, SettingsError> {
        let values = [api_key, api_secret, access_token, access_token_secret].map(str::trim);
        if values.iter().all(|value| value.is_empty()) {
            return Ok(None);
        }
        let [api_key, api_secret, access_token, access_token_secret] = values.map(|value| {
            ChannelSecret::parse(value, MAX_X_CREDENTIAL_BYTES).ok_or(SettingsError::XCredentials)
        });
        Ok(Some(Self {
            api_key: api_key?,
            api_secret: api_secret?,
            access_token: access_token?,
            access_token_secret: access_token_secret?,
        }))
    }
}

#[derive(Debug)]
pub(crate) struct XSettings {
    pub mode: ChannelMode,
    pub credentials: XCredentials,
}

/// Saved settings with the facts the worker and the admin page both need.
#[derive(Debug)]
pub(crate) struct StoredChannel<Settings> {
    pub version: u64,
    /// The provider refused these credentials; sending waits for new ones.
    pub credentials_rejected: bool,
    pub settings: Settings,
}

pub(crate) struct UpdateSubstack {
    pub expected_version: u64,
    pub mode: ChannelMode,
    pub subdomain: SubstackSubdomain,
    /// `None` keeps the saved session cookie.
    pub session: Option<ChannelSecret>,
    pub audit: MutationAuditContext,
}

pub(crate) struct UpdateX {
    pub expected_version: u64,
    pub mode: ChannelMode,
    /// `None` keeps the saved credentials.
    pub credentials: Option<XCredentials>,
    pub audit: MutationAuditContext,
}

/// Rehydrate stored text; a row that fails validation is corrupt.
pub(super) fn stored_substack(
    mode: &str,
    subdomain: &str,
    session: &str,
) -> Option<SubstackSettings> {
    let parsed = SubstackSubdomain::parse(subdomain).ok()?;
    (parsed.as_str() == subdomain).then_some(())?;
    Some(SubstackSettings {
        mode: ChannelMode::parse(mode)?,
        subdomain: parsed,
        session: ChannelSecret::parse(session, MAX_SUBSTACK_SESSION_BYTES)?,
    })
}

pub(super) fn stored_x(mode: &str, credentials: [&str; 4]) -> Option<XSettings> {
    let [api_key, api_secret, access_token, access_token_secret] =
        credentials.map(|value| ChannelSecret::parse(value, MAX_X_CREDENTIAL_BYTES));
    Some(XSettings {
        mode: ChannelMode::parse(mode)?,
        credentials: XCredentials {
            api_key: api_key?,
            api_secret: api_secret?,
            access_token: access_token?,
            access_token_secret: access_token_secret?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_names_round_trip_and_unknown_names_are_rejected() {
        for channel in Channel::ALL {
            assert_eq!(Channel::parse(channel.as_str()), Some(channel));
        }
        assert_eq!(Channel::parse("X"), None);
        assert_eq!(ChannelMode::parse("enabled"), Some(ChannelMode::Enabled));
        assert_eq!(ChannelMode::parse("on"), None);
    }

    #[test]
    fn substack_address_accepts_the_name_host_or_pasted_link() {
        for value in [
            "example",
            " Example.substack.com ",
            "https://example.substack.com/publish/home",
        ] {
            assert_eq!(SubstackSubdomain::parse(value).unwrap().as_str(), "example");
        }
        for value in ["", "-example", "exa mple", "example.com", "a.b"] {
            assert_eq!(
                SubstackSubdomain::parse(value),
                Err(SettingsError::Subdomain)
            );
        }
    }

    #[test]
    fn substack_session_strips_paste_wrappers_and_requires_the_signed_form() {
        for value in [
            "s%3Aabc.def",
            " substack.sid=s%3Aabc.def; ",
            "connect.sid=\"s%3Aabc.def\"",
        ] {
            assert_eq!(substack_session(value).unwrap().expose(), "s%3Aabc.def");
        }
        for value in ["", "abc", "s%3Aabc def", "substack.lli=s%3Aabc"] {
            assert_eq!(
                substack_session(value).unwrap_err(),
                SettingsError::SubstackSession
            );
        }
        assert_eq!(
            format!("{:?}", substack_session("s%3Aabc").unwrap()),
            "ChannelSecret(<redacted>)"
        );
    }

    #[test]
    fn x_credentials_are_all_blank_or_all_present() {
        assert!(
            XCredentials::parse_optional("", " ", "", "")
                .unwrap()
                .is_none()
        );
        let credentials = XCredentials::parse_optional("key", "secret", " token ", "token-secret")
            .unwrap()
            .unwrap();
        assert_eq!(credentials.access_token.expose(), "token");
        for partial in [["key", "", "token", "secret"], ["key", "se cret", "t", "s"]] {
            assert_eq!(
                XCredentials::parse_optional(partial[0], partial[1], partial[2], partial[3])
                    .unwrap_err(),
                SettingsError::XCredentials
            );
        }
    }

    #[test]
    fn stored_rows_must_already_be_normalized() {
        assert!(stored_substack("enabled", "example", "s%3Aabc").is_some());
        assert!(stored_substack("enabled", "Example", "s%3Aabc").is_none());
        assert!(stored_substack("on", "example", "s%3Aabc").is_none());
        assert!(stored_x("paused", ["a", "b", "c", "d"]).is_some());
        assert!(stored_x("paused", ["a", "", "c", "d"]).is_none());
    }
}
