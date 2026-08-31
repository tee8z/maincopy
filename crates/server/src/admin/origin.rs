use std::{fmt, net::SocketAddr, str::FromStr};

use axum::http::uri::PathAndQuery;
use thiserror::Error;
use url::Url;

/// The externally visible HTTPS origin used for every admin trust decision.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct AdminOrigin {
    serialized: Box<str>,
    authority: Box<str>,
}

impl AdminOrigin {
    pub(crate) fn parse(value: &str) -> Result<Self, AdminOriginError> {
        let url = Url::parse(value).map_err(|_| AdminOriginError::Invalid)?;
        if url.scheme() != "https"
            || url.cannot_be_a_base()
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(AdminOriginError::Invalid);
        }

        let serialized = url.origin().ascii_serialization();
        if value != serialized {
            return Err(AdminOriginError::NonCanonical);
        }
        let authority = serialized
            .strip_prefix("https://")
            .ok_or(AdminOriginError::Invalid)?;
        Ok(Self {
            authority: authority.into(),
            serialized: serialized.into_boxed_str(),
        })
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.serialized
    }

    pub(crate) fn authority(&self) -> &str {
        &self.authority
    }

    pub(crate) fn absolute_request_url(&self, target: &PathAndQuery) -> String {
        let mut url = String::with_capacity(self.serialized.len() + target.as_str().len());
        url.push_str(self.as_str());
        url.push_str(target.as_str());
        url
    }
}

impl fmt::Display for AdminOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for AdminOrigin {
    type Err = AdminOriginError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

/// A loopback TCP address for the gateway-facing admin backend.
///
/// Any local process can connect to this listener, so loopback binding is not
/// an authentication boundary; every route still enforces admin credentials.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct AdminBind(SocketAddr);

impl AdminBind {
    pub(crate) fn new(address: SocketAddr) -> Result<Self, AdminBindError> {
        if !address.ip().is_loopback() {
            return Err(AdminBindError::NotLoopback);
        }
        Ok(Self(address))
    }

    pub(crate) const fn into_socket_addr(self) -> SocketAddr {
        self.0
    }
}

impl FromStr for AdminBind {
    type Err = AdminBindParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let address = value
            .parse()
            .map_err(|_| AdminBindParseError::InvalidAddress)?;
        Self::new(address).map_err(AdminBindParseError::InvalidBind)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum AdminBindError {
    #[error("the admin listener address must be loopback")]
    NotLoopback,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum AdminBindParseError {
    #[error("the admin listener address is invalid")]
    InvalidAddress,
    #[error(transparent)]
    InvalidBind(AdminBindError),
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum AdminOriginError {
    #[error(
        "the admin origin must be an HTTPS origin without credentials, path, query, or fragment"
    )]
    Invalid,
    #[error("the admin origin is not in canonical origin form")]
    NonCanonical,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_one_canonical_https_origin() {
        let origin = AdminOrigin::parse("https://admin.example.test:8443").unwrap();
        assert_eq!(origin.as_str(), "https://admin.example.test:8443");
        assert_eq!(origin.authority(), "admin.example.test:8443");
        assert_eq!(
            origin.absolute_request_url(&"/api/admin/v1/posts?limit=10".parse().unwrap()),
            "https://admin.example.test:8443/api/admin/v1/posts?limit=10"
        );
    }

    #[test]
    fn rejects_unsafe_or_noncanonical_values() {
        for value in [
            "http://admin.example.test",
            "https://admin.example.test/",
            "https://ADMIN.example.test",
            "https://user@admin.example.test",
            "https://admin.example.test/path",
            "https://admin.example.test?query",
            "https://admin.example.test#fragment",
            "not a URL",
        ] {
            assert!(AdminOrigin::parse(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn admin_bind_accepts_loopback_ephemeral_and_fixed_ports() {
        let bind: AdminBind = "127.0.0.1:3443".parse().unwrap();
        assert_eq!(bind.into_socket_addr(), "127.0.0.1:3443".parse().unwrap());
        assert!("[::1]:3443".parse::<AdminBind>().is_ok());
        assert!("127.0.0.1:0".parse::<AdminBind>().is_ok());
        for value in ["0.0.0.0:3443", "192.0.2.1:3443"] {
            assert!(value.parse::<AdminBind>().is_err(), "accepted {value}");
        }
    }
}
