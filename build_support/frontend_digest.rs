//! Shared, versioned digest contract for embedded frontend assets.
//!
//! Cargo compiles this source into both the package build script and the
//! runtime library. Keep it independent from either crate's module graph.

use std::{error::Error, fmt};

use blake3::Hasher;

pub(crate) const FRONTEND_BUNDLE_PREFIX: &str = "frontend-b3-v1-";
pub(crate) const FRONTEND_ASSET_PREFIX: &str = "frontend-asset-b3-v1-";

const FRONTEND_BUNDLE_CONTEXT: &str = "maincopy frontend bundle digest v1";
const FRONTEND_ASSET_CONTEXT: &str = "maincopy frontend asset digest v1";
const DIGEST_KIND: &[u8] = b"maincopy-frontend";
const DIGEST_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FrontendAssetKind {
    Css,
    JavaScript,
}

impl FrontendAssetKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Css => "css",
            Self::JavaScript => "javascript",
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Self::Css => 0,
            Self::JavaScript => 1,
        }
    }
}

impl fmt::Display for FrontendAssetKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FrontendAssetName {
    Stylesheet,
    JavaScript,
}

impl FrontendAssetName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stylesheet => "site.css",
            Self::JavaScript => "site.js",
        }
    }

    pub fn parse(value: &str) -> Result<Self, FrontendAssetNameParseError> {
        match value {
            "site.css" => Ok(Self::Stylesheet),
            "site.js" => Ok(Self::JavaScript),
            _ => Err(FrontendAssetNameParseError),
        }
    }
}

impl fmt::Display for FrontendAssetName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for FrontendAssetName {
    type Err = FrontendAssetNameParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrontendAssetNameParseError;

impl fmt::Display for FrontendAssetNameParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("frontend asset name is not recognized")
    }
}

impl Error for FrontendAssetNameParseError {}

#[derive(Clone, Copy)]
pub(crate) struct FrontendDigestInput<'bytes> {
    kind: FrontendAssetKind,
    bytes: &'bytes [u8],
}

impl<'bytes> FrontendDigestInput<'bytes> {
    pub(crate) const fn new(kind: FrontendAssetKind, bytes: &'bytes [u8]) -> Self {
        Self { kind, bytes }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FrontendDigestContractError {
    EmptyBundle,
    MissingStylesheet,
    AssetOrder,
}

impl fmt::Display for FrontendDigestContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyBundle => "frontend bundle must contain at least one asset",
            Self::MissingStylesheet => "frontend bundle must start with one CSS asset",
            Self::AssetOrder => {
                "frontend bundle assets must be unique and ordered by their typed kind"
            }
        })
    }
}

impl Error for FrontendDigestContractError {}

pub(crate) fn frontend_asset_digest(kind: FrontendAssetKind, bytes: &[u8]) -> [u8; 32] {
    let mut transcript = Transcript::new(FRONTEND_ASSET_CONTEXT);
    transcript.tag(kind.tag());
    transcript.bytes(bytes);
    transcript.finish()
}

pub(crate) fn frontend_bundle_digest(
    assets: &[FrontendDigestInput<'_>],
) -> Result<[u8; 32], FrontendDigestContractError> {
    let Some(first) = assets.first() else {
        return Err(FrontendDigestContractError::EmptyBundle);
    };
    if first.kind != FrontendAssetKind::Css {
        return Err(FrontendDigestContractError::MissingStylesheet);
    }
    if assets.windows(2).any(|pair| pair[0].kind >= pair[1].kind) {
        return Err(FrontendDigestContractError::AssetOrder);
    }

    let mut transcript = Transcript::new(FRONTEND_BUNDLE_CONTEXT);
    transcript.sequence_len(assets.len());
    for asset in assets {
        transcript.tag(asset.kind.tag());
        transcript.bytes(asset.bytes);
    }
    Ok(transcript.finish())
}

struct Transcript(Hasher);

impl Transcript {
    fn new(context: &'static str) -> Self {
        let mut transcript = Self(Hasher::new_derive_key(context));
        transcript.bytes(DIGEST_KIND);
        transcript.0.update(&DIGEST_VERSION.to_be_bytes());
        transcript
    }

    fn tag(&mut self, value: u8) {
        self.0.update(&[value]);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.0.update(&(bytes.len() as u64).to_be_bytes());
        self.0.update(bytes);
    }

    fn sequence_len(&mut self, length: usize) {
        self.0.update(&(length as u64).to_be_bytes());
    }

    fn finish(self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_asset_framing_is_unambiguous() {
        let css = frontend_asset_digest(FrontendAssetKind::Css, b"a{color:red}");
        let javascript = frontend_asset_digest(FrontendAssetKind::JavaScript, b"a{color:red}");
        assert_ne!(css, javascript);

        let one =
            frontend_bundle_digest(&[FrontendDigestInput::new(FrontendAssetKind::Css, b"ab")])
                .unwrap();
        let another =
            frontend_bundle_digest(&[FrontendDigestInput::new(FrontendAssetKind::Css, b"a")])
                .unwrap();
        assert_ne!(one, another);
    }

    #[test]
    fn bundle_contract_rejects_missing_or_misordered_css() {
        assert_eq!(
            frontend_bundle_digest(&[]),
            Err(FrontendDigestContractError::EmptyBundle)
        );
        assert_eq!(
            frontend_bundle_digest(&[FrontendDigestInput::new(
                FrontendAssetKind::JavaScript,
                b"js",
            )]),
            Err(FrontendDigestContractError::MissingStylesheet)
        );
        assert_eq!(
            frontend_bundle_digest(&[
                FrontendDigestInput::new(FrontendAssetKind::Css, b"one"),
                FrontendDigestInput::new(FrontendAssetKind::Css, b"two"),
            ]),
            Err(FrontendDigestContractError::AssetOrder)
        );
    }
}
