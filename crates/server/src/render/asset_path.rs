use markdown_compiler::{
    ContentTreeLimits, DigestParseError, LogicalAssetPath, LogicalTreePathError, SiteSnapshotDigest,
};
use thiserror::Error;

/// The public, immutable path of a content asset in one site snapshot.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SnapshotAssetPath {
    public: String,
}

impl SnapshotAssetPath {
    pub(crate) fn new(
        snapshot: &SiteSnapshotDigest,
        asset: &LogicalAssetPath,
    ) -> Result<Self, SnapshotAssetPathError> {
        let Some(relative) = asset.as_str().strip_prefix("assets/") else {
            return Err(SnapshotAssetPathError::WrongNamespace);
        };
        if relative.is_empty() {
            return Err(SnapshotAssetPathError::MissingAssetPath);
        }
        validate_relative_path(relative)?;
        Ok(Self::from_validated(snapshot, relative))
    }

    /// Parses one exact public asset URL path without decoding or normalizing it.
    pub(crate) fn parse(value: &str) -> Result<Self, SnapshotAssetPathError> {
        let remainder = value
            .strip_prefix("/assets/")
            .ok_or(SnapshotAssetPathError::WrongNamespace)?;
        let (snapshot, relative) = remainder
            .split_once('/')
            .ok_or(SnapshotAssetPathError::MissingAssetPath)?;
        if relative.is_empty() {
            return Err(SnapshotAssetPathError::MissingAssetPath);
        }
        validate_relative_path(relative)?;
        let snapshot = SiteSnapshotDigest::parse(snapshot)
            .map_err(SnapshotAssetPathError::InvalidSnapshotDigest)?;
        LogicalAssetPath::parse(&format!("assets/{relative}"))
            .map_err(SnapshotAssetPathError::InvalidLogicalPath)?;
        Ok(Self::from_validated(&snapshot, relative))
    }

    fn from_validated(snapshot: &SiteSnapshotDigest, relative: &str) -> Self {
        Self {
            public: format!("/assets/{}/{relative}", snapshot.as_str()),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.public
    }
}

fn validate_relative_path(relative: &str) -> Result<(), SnapshotAssetPathError> {
    let limits = ContentTreeLimits::default();
    if "assets/"
        .len()
        .checked_add(relative.len())
        .is_none_or(|length| length > limits.path_bytes.get())
    {
        return Err(SnapshotAssetPathError::PathTooLong);
    }
    if relative.split('/').count() >= limits.depth.get() {
        return Err(SnapshotAssetPathError::PathTooDeep);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum SnapshotAssetPathError {
    #[error("public asset path must start with /assets/")]
    WrongNamespace,
    #[error("public asset path must include a snapshot digest and an asset path")]
    MissingAssetPath,
    #[error("public asset path exceeds the logical path byte limit")]
    PathTooLong,
    #[error("public asset path exceeds the logical path depth limit")]
    PathTooDeep,
    #[error("public asset path contains an invalid snapshot digest")]
    InvalidSnapshotDigest(#[source] DigestParseError),
    #[error("public asset path contains an invalid logical asset path")]
    InvalidLogicalPath(#[source] LogicalTreePathError),
}

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
            SnapshotAssetPath::parse(&format!("/assets/{snapshot}/images/cover.webp")).unwrap(),
            SnapshotAssetPath::new(&snapshot, &asset).unwrap()
        );
    }

    #[test]
    fn public_asset_paths_reject_aliases_and_traversal_forms() {
        let digest = format!("site-b3-v1-{}", "11".repeat(32));
        for path in [
            format!("assets/{digest}/image.png"),
            format!("/assets/{digest}"),
            format!("/assets/{digest}/"),
            format!("/assets/{digest}//image.png"),
            format!("/assets/{digest}/./image.png"),
            format!("/assets/{digest}/../image.png"),
            format!("/assets/{digest}/%69mage.png"),
            format!("/assets/{digest}/%2e%2e/image.png"),
            format!("/assets/{digest}/folder\\image.png"),
            format!("/assets/{}/image.png", digest.to_uppercase()),
            "/assets/not-a-digest/image.png".to_owned(),
        ] {
            assert!(
                SnapshotAssetPath::parse(&path).is_err(),
                "unsafe or noncanonical path unexpectedly parsed: {path}"
            );
        }
    }

    #[test]
    fn public_asset_paths_enforce_logical_length_and_depth_ceilings() {
        let digest = SiteSnapshotDigest::parse(&format!("site-b3-v1-{}", "11".repeat(32)))
            .expect("fixture digest must parse");
        let assert_rejected = |relative: &str, expected: SnapshotAssetPathError| {
            let logical = LogicalAssetPath::parse(&format!("assets/{relative}"))
                .expect("portable over-limit fixture must parse as a logical asset path");
            assert_eq!(
                SnapshotAssetPath::new(&digest, &logical),
                Err(expected.clone())
            );
            assert_eq!(
                SnapshotAssetPath::parse(&format!("/assets/{digest}/{relative}")),
                Err(expected)
            );
        };

        let maximum_path = "a".repeat(ContentTreeLimits::default().path_bytes.get() - 7);
        assert_eq!(format!("assets/{maximum_path}").len(), 1_024);
        let maximum_logical = LogicalAssetPath::parse(&format!("assets/{maximum_path}")).unwrap();
        assert!(SnapshotAssetPath::new(&digest, &maximum_logical).is_ok());
        assert!(SnapshotAssetPath::parse(&format!("/assets/{digest}/{maximum_path}")).is_ok());

        let too_long = "a".repeat(ContentTreeLimits::default().path_bytes.get() - 6);
        assert_eq!(format!("assets/{too_long}").len(), 1_025);
        assert_rejected(&too_long, SnapshotAssetPathError::PathTooLong);

        let maximum_depth = std::iter::repeat_n("a", 15).collect::<Vec<_>>().join("/");
        let maximum_logical = LogicalAssetPath::parse(&format!("assets/{maximum_depth}")).unwrap();
        assert!(SnapshotAssetPath::new(&digest, &maximum_logical).is_ok());
        assert!(SnapshotAssetPath::parse(&format!("/assets/{digest}/{maximum_depth}")).is_ok());
        let too_deep = std::iter::repeat_n("a", 16).collect::<Vec<_>>().join("/");
        assert_rejected(&too_deep, SnapshotAssetPathError::PathTooDeep);
    }
}
