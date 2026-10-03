// Frozen frontend of releases 0.1.1 through 0.1.6, as their build script produced it.
// Used only to verify retained site identity before upgrading presentation.
use super::{
    FrontendAsset, FrontendAssetDigest, FrontendAssetKind, FrontendAssetManifest,
    FrontendBundleDigest,
};
// CSS ETag: "frontend-asset-b3-v1-4ec372a4f25b1a64c0f217f257cfc53c6e0149950cc538791f82610e1d5d713c"
pub(super) const GENERATED_FRONTEND_MANIFEST: FrontendAssetManifest = FrontendAssetManifest {
    bundle_digest: FrontendBundleDigest::from_generated([
        252, 121, 144, 1, 225, 157, 247, 245, 156, 39, 23, 223, 42, 202, 234, 228, 159, 159, 138,
        183, 223, 41, 89, 46, 198, 247, 92, 31, 104, 99, 80, 237,
    ]),
    css: FrontendAsset {
        kind: FrontendAssetKind::Css,
        digest: FrontendAssetDigest::from_generated([
            78, 195, 114, 164, 242, 91, 26, 100, 192, 242, 23, 242, 87, 207, 197, 60, 110, 1, 73,
            149, 12, 197, 56, 121, 31, 130, 97, 14, 29, 93, 113, 60,
        ]),
        public_path: "/app-assets/frontend-b3-v1-fc799001e19df7f59c2717df2acaeae49f9f8ab7df29592ec6f75c1f686350ed/site.css",
        bytes: include_bytes!("v0_1_1/site.css"),
    },
    // The script did not change between these releases and 0.1.0.
    javascript: Some(FrontendAsset {
        kind: FrontendAssetKind::JavaScript,
        digest: FrontendAssetDigest::from_generated([
            137, 172, 234, 72, 236, 112, 209, 139, 94, 119, 226, 79, 87, 250, 235, 15, 62, 115,
            221, 196, 39, 169, 140, 126, 131, 155, 22, 67, 246, 84, 236, 54,
        ]),
        public_path: "/app-assets/frontend-b3-v1-fc799001e19df7f59c2717df2acaeae49f9f8ab7df29592ec6f75c1f686350ed/site.js",
        bytes: include_bytes!("v0_1_0/site.js"),
    }),
};
