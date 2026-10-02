// Frozen v0.1.0 frontend, reproduced by its build script and pinned Cargo.lock.
// Used only to verify retained site identity before upgrading presentation.
use super::{
    FrontendAsset, FrontendAssetDigest, FrontendAssetKind, FrontendAssetManifest,
    FrontendBundleDigest,
};
// CSS ETag: "frontend-asset-b3-v1-fb51cc84663e9b203f7faed8604ada2e2137882ebfc43e532db92d4957ce3f1b"
pub(super) const GENERATED_FRONTEND_MANIFEST: FrontendAssetManifest = FrontendAssetManifest {
    bundle_digest: FrontendBundleDigest::from_generated([
        75, 241, 158, 173, 235, 213, 215, 229, 158, 251, 242, 212, 246, 25, 200, 135, 129, 3, 1,
        235, 166, 111, 122, 76, 186, 235, 91, 27, 232, 96, 208, 162,
    ]),
    css: FrontendAsset {
        kind: FrontendAssetKind::Css,
        digest: FrontendAssetDigest::from_generated([
            251, 81, 204, 132, 102, 62, 155, 32, 63, 127, 174, 216, 96, 74, 218, 46, 33, 55, 136,
            46, 191, 196, 62, 83, 45, 185, 45, 73, 87, 206, 63, 27,
        ]),
        public_path: "/app-assets/frontend-b3-v1-4bf19eadebd5d7e59efbf2d4f619c887810301eba66f7a4cbaeb5b1be860d0a2/site.css",
        bytes: include_bytes!("v0_1_0/site.css"),
    },
    javascript: Some(FrontendAsset {
        kind: FrontendAssetKind::JavaScript,
        digest: FrontendAssetDigest::from_generated([
            137, 172, 234, 72, 236, 112, 209, 139, 94, 119, 226, 79, 87, 250, 235, 15, 62, 115,
            221, 196, 39, 169, 140, 126, 131, 155, 22, 67, 246, 84, 236, 54,
        ]),
        public_path: "/app-assets/frontend-b3-v1-4bf19eadebd5d7e59efbf2d4f619c887810301eba66f7a4cbaeb5b1be860d0a2/site.js",
        bytes: include_bytes!("v0_1_0/site.js"),
    }),
};
