use std::fmt;

use bech32::{Bech32, Hrp};
use thiserror::Error;

use super::LightningAddress;

const LNURL_HRP: Hrp = Hrp::parse_unchecked("lnurl");
const LNURL_PAY_PATH: &str = "/.well-known/lnurlp/";

/// Public display data for one effective static tip recipient.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TipRecipientProjection {
    pub lightning_address: LightningAddress,
    pub endpoint: LnurlPayEndpoint,
    pub lnurl: LnurlPayload,
    pub wallet_link: LightningWalletLink,
}

impl TipRecipientProjection {
    pub fn new(lightning_address: LightningAddress) -> Result<Self, LnurlEncodingError> {
        let endpoint = LnurlPayEndpoint::for_address(&lightning_address);
        let lnurl = LnurlPayload::for_endpoint(&endpoint)?;
        let wallet_link = LightningWalletLink::for_lnurl(&lnurl);
        Ok(Self {
            lightning_address,
            endpoint,
            lnurl,
            wallet_link,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LnurlPayEndpoint(Box<str>);

impl LnurlPayEndpoint {
    fn for_address(address: &LightningAddress) -> Self {
        Self(
            format!(
                "https://{}{LNURL_PAY_PATH}{}",
                address.domain(),
                address.username()
            )
            .into_boxed_str(),
        )
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LnurlPayEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LnurlPayload(Box<str>);

impl LnurlPayload {
    fn for_endpoint(endpoint: &LnurlPayEndpoint) -> Result<Self, LnurlEncodingError> {
        encode_lnurl(endpoint.as_str()).map(|value| Self(value.into_boxed_str()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LnurlPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightningWalletLink(Box<str>);

impl LightningWalletLink {
    fn for_lnurl(lnurl: &LnurlPayload) -> Self {
        Self(format!("lightning:{lnurl}").into_boxed_str())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LightningWalletLink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

fn encode_lnurl(endpoint: &str) -> Result<String, LnurlEncodingError> {
    bech32::encode_upper::<Bech32>(LNURL_HRP, endpoint.as_bytes()).map_err(LnurlEncodingError)
}

#[derive(Debug, Error)]
#[error("the local LNURL projection could not be encoded: {0}")]
pub struct LnurlEncodingError(bech32::EncodeError);

#[cfg(test)]
mod tests {
    use bech32::primitives::decode::CheckedHrpstring;

    use super::*;

    const LUD01_ENDPOINT: &str = "https://service.com/api?q=3fc3645b439ce8e7f2553a69e5267081d96dcd340693afabe04be7b0ccd178df";
    const LUD01_LNURL: &str = "LNURL1DP68GURN8GHJ7UM9WFMXJCM99E3K7MF0V9CXJ0M385EKVCENXC6R2C35XVUKXEFCV5MKVV34X5EKZD3EV56NYD3HXQURZEPEXEJXXEPNXSCRVWFNV9NXZCN9XQ6XYEFHVGCXXCMYXYMNSERXFQ5FNS";

    #[test]
    fn encoder_matches_the_lud01_reference_vector() {
        assert_eq!(encode_lnurl(LUD01_ENDPOINT).unwrap(), LUD01_LNURL);
    }

    #[test]
    fn lud16_address_derives_one_consistent_static_handoff() {
        let projection =
            TipRecipientProjection::new(LightningAddress::parse("satoshi@bitcoin.org").unwrap())
                .unwrap();

        assert_eq!(
            projection.endpoint.as_str(),
            "https://bitcoin.org/.well-known/lnurlp/satoshi"
        );
        assert!(projection.lnurl.as_str().starts_with("LNURL1"));
        assert!(
            projection
                .lnurl
                .as_str()
                .bytes()
                .all(|byte| !byte.is_ascii_lowercase())
        );
        assert_eq!(
            projection.wallet_link.as_str(),
            format!("lightning:{}", projection.lnurl)
        );

        let decoded = CheckedHrpstring::new::<Bech32>(projection.lnurl.as_str()).unwrap();
        assert_eq!(decoded.hrp(), LNURL_HRP);
        assert_eq!(
            decoded.byte_iter().collect::<Vec<_>>(),
            projection.endpoint.as_str().as_bytes()
        );
    }

    #[test]
    fn projection_contains_only_public_handoff_values() {
        let projection =
            TipRecipientProjection::new(LightningAddress::parse("alice@example.com").unwrap())
                .unwrap();
        let debug = format!("{projection:?}");

        assert!(debug.contains("alice@example.com"));
        assert!(debug.contains("https://example.com/.well-known/lnurlp/alice"));
        assert!(!debug.contains("invoice"));
        assert!(!debug.contains("provider"));
        assert!(!debug.contains("credential"));
    }
}
