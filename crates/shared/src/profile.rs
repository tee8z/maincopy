//! Validated public profile values shared by the admin server and its clients.

use std::{fmt, net::IpAddr, str::FromStr};

use serde::{Deserialize, Serialize, de};

pub const MAX_LIGHTNING_ADDRESS_BYTES: usize = 320;
pub const MAX_PROFILE_DISPLAY_NAME_BYTES: usize = 160;

const MAX_PROFILE_VERSION: u64 = i64::MAX as u64;

const MAX_DNS_DOMAIN_BYTES: usize = 253;
const MAX_DNS_LABEL_BYTES: usize = 63;

/// A positive profile resource version representable by SQLite.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProfileVersion(u64);

impl ProfileVersion {
    pub const fn new(value: u64) -> Result<Self, ProfileVersionError> {
        match value {
            0 => Err(ProfileVersionError::Zero),
            value if value > MAX_PROFILE_VERSION => Err(ProfileVersionError::OutsideStorageRange),
            value => Ok(Self(value)),
        }
    }

    pub const fn into_u64(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for ProfileVersion {
    type Error = ProfileVersionError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ProfileVersion> for i64 {
    fn from(value: ProfileVersion) -> Self {
        value.0 as i64
    }
}

impl Serialize for ProfileVersion {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: serde::Serializer,
    {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for ProfileVersion {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: serde::Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileVersionError {
    Zero,
    OutsideStorageRange,
}

impl fmt::Display for ProfileVersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => formatter.write_str("a profile resource version must be positive"),
            Self::OutsideStorageRange => {
                formatter.write_str("a profile resource version is outside the storage range")
            }
        }
    }
}

impl std::error::Error for ProfileVersionError {}

#[cfg(feature = "schema")]
impl utoipa::PartialSchema for ProfileVersion {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::schema::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::Integer)
            .minimum(Some(1))
            .maximum(Some(MAX_PROFILE_VERSION))
            .into()
    }
}

#[cfg(feature = "schema")]
impl utoipa::ToSchema for ProfileVersion {}

/// A canonical clearnet LUD-16 internet identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

        validate_username(&value[..separator])?;
        validate_domain(&value[separator + 1..])?;

        Ok(Self {
            encoded: value.into(),
            separator,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    pub fn as_username(&self) -> &str {
        &self.encoded[..self.separator]
    }

    pub fn as_domain(&self) -> &str {
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
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LightningAddress {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: serde::Deserializer<'de>,
    {
        let value = Box::<str>::deserialize(deserializer)?;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightningAddressError {
    Empty,
    TooLong { actual: usize, maximum: usize },
    MissingSeparator,
    MultipleSeparators,
    DefaultIdentifierShorthand,
    InvalidUsername,
    InvalidDomain,
    IpLiteralDomain,
    OnionDomain,
}

impl fmt::Display for LightningAddressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("a Lightning Address must not be empty"),
            Self::TooLong { actual, maximum } => write!(
                formatter,
                "the Lightning Address is {actual} bytes; the maximum is {maximum}"
            ),
            Self::MissingSeparator => {
                formatter.write_str("a Lightning Address must use the username@domain form")
            }
            Self::MultipleSeparators => {
                formatter.write_str("a Lightning Address must contain exactly one @ separator")
            }
            Self::DefaultIdentifierShorthand => {
                formatter.write_str("the optional LUD-16 @domain shorthand is unsupported")
            }
            Self::InvalidUsername => {
                formatter.write_str("the Lightning Address username is not canonical")
            }
            Self::InvalidDomain => formatter
                .write_str("the Lightning Address domain is not a canonical lowercase DNS name"),
            Self::IpLiteralDomain => {
                formatter.write_str("an IP literal cannot be a Lightning Address domain")
            }
            Self::OnionDomain => formatter.write_str("onion Lightning Addresses are unsupported"),
        }
    }
}

impl std::error::Error for LightningAddressError {}

#[cfg(feature = "schema")]
impl utoipa::PartialSchema for LightningAddress {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::schema::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .min_length(Some(1))
            .max_length(Some(MAX_LIGHTNING_ADDRESS_BYTES))
            .into()
    }
}

#[cfg(feature = "schema")]
impl utoipa::ToSchema for LightningAddress {}

/// A bounded, visible profile name preserved exactly as entered.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProfileDisplayName(Box<str>);

impl ProfileDisplayName {
    pub fn parse(value: &str) -> Result<Self, ProfileDisplayNameError> {
        if value.is_empty() {
            return Err(ProfileDisplayNameError::Empty);
        }
        if value.len() > MAX_PROFILE_DISPLAY_NAME_BYTES {
            return Err(ProfileDisplayNameError::TooLong {
                actual: value.len(),
                maximum: MAX_PROFILE_DISPLAY_NAME_BYTES,
            });
        }
        if value.trim() != value {
            return Err(ProfileDisplayNameError::SurroundingWhitespace);
        }
        if value.chars().any(char::is_control) {
            return Err(ProfileDisplayNameError::ControlCharacter);
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProfileDisplayName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ProfileDisplayName {
    type Err = ProfileDisplayNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for ProfileDisplayName {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProfileDisplayName {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: serde::Deserializer<'de>,
    {
        let value = Box::<str>::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileDisplayNameError {
    Empty,
    TooLong { actual: usize, maximum: usize },
    SurroundingWhitespace,
    ControlCharacter,
}

impl fmt::Display for ProfileDisplayNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("a profile display name must not be empty"),
            Self::TooLong { actual, maximum } => write!(
                formatter,
                "the profile display name is {actual} bytes; the maximum is {maximum}"
            ),
            Self::SurroundingWhitespace => {
                formatter.write_str("a profile display name must not have surrounding whitespace")
            }
            Self::ControlCharacter => {
                formatter.write_str("a profile display name must not contain control characters")
            }
        }
    }
}

impl std::error::Error for ProfileDisplayNameError {}

#[cfg(feature = "schema")]
impl utoipa::PartialSchema for ProfileDisplayName {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::schema::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .min_length(Some(1))
            .description(Some(
                "A public display name containing at most 160 UTF-8 bytes; surrounding whitespace and control characters are rejected.",
            ))
            .into()
    }
}

#[cfg(feature = "schema")]
impl utoipa::ToSchema for ProfileDisplayName {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_versions_reject_values_sqlite_cannot_store() {
        assert_eq!(ProfileVersion::new(0), Err(ProfileVersionError::Zero));
        assert_eq!(
            ProfileVersion::new(MAX_PROFILE_VERSION + 1),
            Err(ProfileVersionError::OutsideStorageRange)
        );
        let maximum = ProfileVersion::new(MAX_PROFILE_VERSION).unwrap();
        assert_eq!(maximum.into_u64(), MAX_PROFILE_VERSION);
        assert_eq!(
            serde_json::from_str::<ProfileVersion>(&MAX_PROFILE_VERSION.to_string()).unwrap(),
            maximum
        );
    }

    #[test]
    fn documented_lud16_username_characters_are_accepted() {
        let address = LightningAddress::parse("name-._+tag123@example.com").unwrap();

        assert_eq!(address.as_username(), "name-._+tag123");
        assert_eq!(address.as_domain(), "example.com");
        assert_eq!(address.as_str(), "name-._+tag123@example.com");
    }

    #[test]
    fn lightning_address_round_trips_as_a_visible_non_secret_string() {
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
    fn complete_lightning_address_byte_limit_is_inclusive() {
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
    fn unsupported_lightning_address_forms_fail_closed() {
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
            ("alice@[::1]", LightningAddressError::InvalidDomain),
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
    fn lightning_address_deserialization_reuses_canonical_validation() {
        let error = serde_json::from_str::<LightningAddress>("\"Alice@example.com\"")
            .unwrap_err()
            .to_string();

        assert!(error.contains("username is not canonical"));
    }

    #[test]
    fn profile_display_name_preserves_unicode_and_round_trips() {
        let name = ProfileDisplayName::parse("Alice 文").unwrap();
        let encoded = serde_json::to_string(&name).unwrap();

        assert_eq!(name.as_str(), "Alice 文");
        assert_eq!(encoded, "\"Alice 文\"");
        assert_eq!(
            serde_json::from_str::<ProfileDisplayName>(&encoded).unwrap(),
            name
        );
    }

    #[test]
    fn profile_display_name_byte_limit_is_inclusive() {
        let maximum = "é".repeat(MAX_PROFILE_DISPLAY_NAME_BYTES / 2);
        assert_eq!(maximum.len(), MAX_PROFILE_DISPLAY_NAME_BYTES);
        assert!(ProfileDisplayName::parse(&maximum).is_ok());

        let oversized = format!("{maximum}a");
        assert_eq!(
            ProfileDisplayName::parse(&oversized),
            Err(ProfileDisplayNameError::TooLong {
                actual: MAX_PROFILE_DISPLAY_NAME_BYTES + 1,
                maximum: MAX_PROFILE_DISPLAY_NAME_BYTES,
            })
        );
    }

    #[cfg(feature = "schema")]
    #[test]
    fn profile_display_name_schema_does_not_misstate_the_byte_limit_as_characters() {
        let schema =
            serde_json::to_value(<ProfileDisplayName as utoipa::PartialSchema>::schema()).unwrap();

        assert_eq!(schema["minLength"], 1);
        assert_eq!(schema.get("maxLength"), None);
        assert_eq!(
            schema["description"],
            "A public display name containing at most 160 UTF-8 bytes; surrounding whitespace and control characters are rejected."
        );
    }

    #[test]
    fn invalid_profile_display_names_are_rejected() {
        for (value, expected) in [
            ("", ProfileDisplayNameError::Empty),
            (" Alice", ProfileDisplayNameError::SurroundingWhitespace),
            ("Alice ", ProfileDisplayNameError::SurroundingWhitespace),
            ("Alice\nWriter", ProfileDisplayNameError::ControlCharacter),
        ] {
            assert_eq!(ProfileDisplayName::parse(value), Err(expected), "{value:?}");
        }
    }
}
