use thiserror::Error;
use zeroize::Zeroizing;

/// An ASCII mailbox whose owned address is wiped on drop. The local part is
/// preserved; only DNS domain casing is normalized. No implicit logging or wire
/// serialization is available to callers.
pub(super) struct EmailAddress(Zeroizing<String>);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("the email address is not a supported ASCII mailbox")]
pub(super) struct InvalidEmailAddress;

impl EmailAddress {
    pub(super) fn parse(input: &str) -> Result<Self, InvalidEmailAddress> {
        if input.len() > 254 {
            return Err(InvalidEmailAddress);
        }
        let (local, domain) = input.split_once('@').ok_or(InvalidEmailAddress)?;
        if !valid_local(local) || !valid_domain(domain) {
            return Err(InvalidEmailAddress);
        }
        let mut address = Zeroizing::new(String::with_capacity(input.len()));
        address.push_str(local);
        address.push('@');
        address.extend(
            domain
                .chars()
                .map(|character| character.to_ascii_lowercase()),
        );
        Ok(Self(address))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

fn valid_local(local: &str) -> bool {
    !local.is_empty()
        && local.len() <= 64
        && local.split('.').all(|atom| {
            !atom.is_empty()
                && atom.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~".contains(&byte)
                })
        })
}

fn valid_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_local_identity_and_canonicalizes_only_the_domain() {
        let address = EmailAddress::parse("Case+tag/part?x#p%q@EXAMPLE.COM").unwrap();
        assert_eq!(address.as_str(), "Case+tag/part?x#p%q@example.com");
    }

    #[test]
    fn rejects_header_injection_ambiguous_mailboxes_and_invalid_domain_labels() {
        for address in [
            "a@example.com\r\nBcc: victim@example.com",
            "a..b@example.com",
            ".a@example.com",
            "a.@example.com",
            "a@@example.com",
            "a@-example.com",
            "a@example-.com",
            "a@example..com",
            "a@exämple.com",
            "a b@example.com",
            "\"a\"@example.com",
            "a@[127.0.0.1]",
            "@example.com",
            "a@",
        ] {
            assert!(EmailAddress::parse(address).is_err());
        }
    }

    #[test]
    fn enforces_local_and_domain_label_bounds() {
        assert!(EmailAddress::parse(&format!("{}@example.com", "a".repeat(64))).is_ok());
        assert!(EmailAddress::parse(&format!("{}@example.com", "a".repeat(65))).is_err());
        assert!(EmailAddress::parse(&format!("a@{}.com", "a".repeat(64))).is_err());
    }
}
