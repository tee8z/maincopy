use std::fmt;

use markdown_compiler::{LogicalAssetPath, SiteSnapshotDigest};
use serde::{Serialize, Serializer};
use thiserror::Error;

/// The public, immutable path of a content asset in one site snapshot.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SnapshotAssetPath {
    public: String,
    storage_relative: String,
}

impl SnapshotAssetPath {
    pub(crate) fn new(
        snapshot: &SiteSnapshotDigest,
        asset: &LogicalAssetPath,
    ) -> Result<Self, SnapshotAssetPathError> {
        let Some(relative) = asset.as_str().strip_prefix("assets/") else {
            return Err(SnapshotAssetPathError);
        };
        if relative.is_empty() {
            return Err(SnapshotAssetPathError);
        }
        let storage_relative = format!("{}/{relative}", snapshot.as_str());
        Ok(Self {
            public: format!("/assets/{storage_relative}"),
            storage_relative,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.public
    }

    pub fn storage_relative(&self) -> &str {
        &self.storage_relative
    }
}

impl fmt::Display for SnapshotAssetPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for SnapshotAssetPath {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("logical asset path is outside the assets namespace")]
pub struct SnapshotAssetPathError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immutable_asset_paths_are_snapshot_scoped() {
        let snapshot = SiteSnapshotDigest::parse(&format!("site-b3-v1-{}", "11".repeat(32)))
            .expect("fixture digest must parse");
        let asset =
            LogicalAssetPath::parse("assets/images/cover.webp").expect("fixture path must parse");

        assert_eq!(
            SnapshotAssetPath::new(&snapshot, &asset).unwrap().as_str(),
            format!("/assets/{snapshot}/images/cover.webp")
        );
        assert_eq!(
            SnapshotAssetPath::new(&snapshot, &asset)
                .unwrap()
                .storage_relative(),
            format!("{snapshot}/images/cover.webp")
        );
    }
}
