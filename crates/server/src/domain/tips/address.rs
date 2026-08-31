use std::{fmt, net::IpAddr, str::FromStr};

use serde::{Deserialize, Serialize, de};
use thiserror::Error;

pub const MAX_LIGHTNING_ADDRESS_BYTES: usize = 320;
const MAX_DNS_DOMAIN_BYTES: usize = 253;
const MAX_DNS_LABEL_BYTES: usize = 63;

/// A canonical clearnet LUD-16 internet identifier.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LightningAddress {
    encoded: Box<str>,
    separator: usize,
}

impl LightningAddress {
    pub fn parse(value: &str) -> Result<Self, LightningAddressError> {
        if value.is_empty() {
            return Err(LightningAddressError::Empty);
        }
        if value.len() > MAX_LIGHTNING_ADDRESS_BYTES {
            return Err(LightningAddressError::TooLong {
                actual: value.len(),
                maximum: MAX_LIGHTNING_ADDRESS_BYTES,
            });
        }

        let Some(separator) = value.find('@') else {
            return Err(LightningAddressError::MissingSeparator);
        };
        if value[separator + 1..].contains('@') {
            return Err(LightningAddressError::MultipleSeparators);
        }

        let username = &value[..separator];
        let domain = &value[separator + 1..];
        validate_username(username)?;
        validate_domain(domain)?;

        Ok(Self {
            encoded: value.into(),
            separator,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    pub fn username(&self) -> &str {
        &self.encoded[..self.separator]
    }

    pub fn domain(&self) -> &str {
        &self.encoded[self.separator + 1..]
    }
}

impl fmt::Display for LightningAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for LightningAddress {
    type Err = LightningAddressError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for LightningAddress {
    fn serialize<Serializer>(
        &self,
        serializer: Serializer,
    ) -> Result<Serializer::Ok, Serializer::Error>
    where
        Serializer: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LightningAddress {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

fn validate_username(username: &str) -> Result<(), LightningAddressError> {
    if username.is_empty() {
        return Err(LightningAddressError::DefaultIdentifierShorthand);
    }
    if !username
        .bytes()
        .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'+'))
    {
        return Err(LightningAddressError::InvalidUsername);
    }
    Ok(())
}

fn validate_domain(domain: &str) -> Result<(), LightningAddressError> {
    if domain.is_empty() || domain.len() > MAX_DNS_DOMAIN_BYTES {
        return Err(LightningAddressError::InvalidDomain);
    }
    if domain == "onion" || domain.ends_with(".onion") {
        return Err(LightningAddressError::OnionDomain);
    }
    if domain.parse::<IpAddr>().is_ok() {
        return Err(LightningAddressError::IpLiteralDomain);
    }
    if domain.ends_with('.')
        || !domain.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        })
    {
        return Err(LightningAddressError::InvalidDomain);
    }
    if domain.split('.').any(|label| {
        label.is_empty()
            || label.len() > MAX_DNS_LABEL_BYTES
            || !label.as_bytes()[0].is_ascii_alphanumeric()
            || !label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
    }) {
        return Err(LightningAddressError::InvalidDomain);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum LightningAddressError {
    #[error("a Lightning Address must not be empty")]
    Empty,
    #[error("the Lightning Address is {actual} bytes; the maximum is {maximum}")]
    TooLong { actual: usize, maximum: usize },
    #[error("a Lightning Address must use the username@domain form")]
    MissingSeparator,
    #[error("a Lightning Address must contain exactly one @ separator")]
    MultipleSeparators,
    #[error("the optional LUD-16 @domain shorthand is unsupported")]
    DefaultIdentifierShorthand,
    #[error("the Lightning Address username is not canonical")]
    InvalidUsername,
    #[error("the Lightning Address domain is not a canonical lowercase DNS name")]
    InvalidDomain,
    #[error("an IP literal cannot be a Lightning Address domain")]
    IpLiteralDomain,
    #[error("onion Lightning Addresses are unsupported")]
    OnionDomain,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_lud16_username_characters_are_accepted() {
        let address = LightningAddress::parse("name-._+tag123@example.com").unwrap();

        assert_eq!(address.username(), "name-._+tag123");
        assert_eq!(address.domain(), "example.com");
        assert_eq!(address.to_string(), "name-._+tag123@example.com");
    }

    #[test]
    fn address_round_trips_as_a_visible_non_secret_string() {
        let address = LightningAddress::parse("alice@example.com").unwrap();
        let encoded = serde_json::to_string(&Some(address.clone())).unwrap();

        assert_eq!(encoded, "\"alice@example.com\"");
        assert_eq!(
            serde_json::from_str::<Option<LightningAddress>>(&encoded).unwrap(),
            Some(address.clone())
        );
        assert!(format!("{address:?}").contains("alice@example.com"));
    }

    #[test]
    fn complete_address_byte_limit_is_inclusive() {
        let domain = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61)
        );
        let maximum = format!("{}@{domain}", "u".repeat(66));
        assert_eq!(maximum.len(), MAX_LIGHTNING_ADDRESS_BYTES);
        assert!(LightningAddress::parse(&maximum).is_ok());

        let oversized = format!("u{maximum}");
        assert_eq!(
            LightningAddress::parse(&oversized),
            Err(LightningAddressError::TooLong {
                actual: MAX_LIGHTNING_ADDRESS_BYTES + 1,
                maximum: MAX_LIGHTNING_ADDRESS_BYTES,
            })
        );
    }

    #[test]
    fn unsupported_address_forms_fail_closed() {
        let cases = [
            ("", LightningAddressError::Empty),
            ("alice", LightningAddressError::MissingSeparator),
            (
                "@example.com",
                LightningAddressError::DefaultIdentifierShorthand,
            ),
            ("a@b@example.com", LightningAddressError::MultipleSeparators),
            ("Alice@example.com", LightningAddressError::InvalidUsername),
            ("ali ce@example.com", LightningAddressError::InvalidUsername),
            (
                "alice@example.com:443",
                LightningAddressError::InvalidDomain,
            ),
            (
                "alice@example.com/path",
                LightningAddressError::InvalidDomain,
            ),
            (
                "alice@example.com?x=1",
                LightningAddressError::InvalidDomain,
            ),
            (
                "alice@example.com#fragment",
                LightningAddressError::InvalidDomain,
            ),
            ("alice@Example.com", LightningAddressError::InvalidDomain),
            ("alice@example.com.", LightningAddressError::InvalidDomain),
            ("alice@example..com", LightningAddressError::InvalidDomain),
            ("alice@-example.com", LightningAddressError::InvalidDomain),
            ("alice@example-.com", LightningAddressError::InvalidDomain),
            ("alice@127.0.0.1", LightningAddressError::IpLiteralDomain),
            ("alice@example.onion", LightningAddressError::OnionDomain),
            ("álîçé@example.com", LightningAddressError::InvalidUsername),
        ];

        for (value, expected) in cases {
            assert_eq!(LightningAddress::parse(value), Err(expected), "{value}");
        }
    }

    #[test]
    fn dns_label_and_domain_limits_are_enforced() {
        let long_label = "a".repeat(MAX_DNS_LABEL_BYTES + 1);
        assert_eq!(
            LightningAddress::parse(&format!("alice@{long_label}.com")),
            Err(LightningAddressError::InvalidDomain)
        );

        let long_domain = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(62)
        );
        assert_eq!(long_domain.len(), MAX_DNS_DOMAIN_BYTES + 1);
        assert_eq!(
            LightningAddress::parse(&format!("alice@{long_domain}")),
            Err(LightningAddressError::InvalidDomain)
        );
    }

    #[test]
    fn deserialization_reuses_canonical_validation() {
        let error = serde_json::from_str::<LightningAddress>("\"Alice@example.com\"")
            .unwrap_err()
            .to_string();

        assert!(error.contains("username is not canonical"));
    }
}
