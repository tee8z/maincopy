use axum::http::{HeaderValue, header::InvalidHeaderValue};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use markdown_compiler::ExternalAssetOrigin;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::frontend_assets::FrontendAssetManifest;

const MAX_POLICY_BYTES: usize = 16 * 1024;
pub(crate) const REFERRER_POLICY: HeaderValue = HeaderValue::from_static("no-referrer");

/// Response policy is compiled with the same immutable candidate as page bytes.
#[derive(Clone, Debug)]
pub(crate) struct PublicResponsePolicy {
    pub(crate) content_security_policy: HeaderValue,
    pub(crate) script_integrity: Option<String>,
}

impl PublicResponsePolicy {
    pub(super) fn new(
        origins: &[ExternalAssetOrigin],
        frontend: &FrontendAssetManifest,
    ) -> Result<Self, ResponsePolicyError> {
        let script_integrity = frontend
            .javascript
            .as_ref()
            .map(|asset| format!("sha256-{}", STANDARD.encode(Sha256::digest(asset.bytes))));
        let script_source = script_integrity
            .as_ref()
            .map_or_else(|| "'none'".to_owned(), |integrity| format!("'{integrity}'"));
        let mut asset_sources = String::from("'self'");
        for origin in origins {
            asset_sources.push(' ');
            // Parse normalized HTTPS origins only; authored strings never enter a directive.
            asset_sources.push_str(origin.as_str().trim_end_matches('/'));
            if asset_sources.len() > MAX_POLICY_BYTES / 2 {
                return Err(ResponsePolicyError::TooLarge);
            }
        }
        // Sanitized diagrams can contain bounded embedded PNGs. No other directive allows data:.
        let policy = format!(
            "default-src 'none'; base-uri 'none'; object-src 'none'; frame-src 'none'; frame-ancestors 'none'; form-action 'none'; connect-src 'none'; script-src {script_source}; script-src-attr 'none'; style-src 'self'; img-src {asset_sources} data:; media-src {asset_sources}; font-src 'self'"
        );
        if policy.len() > MAX_POLICY_BYTES {
            return Err(ResponsePolicyError::TooLarge);
        }
        Ok(Self {
            content_security_policy: HeaderValue::from_str(&policy)
                .map_err(ResponsePolicyError::InvalidHeader)?,
            script_integrity,
        })
    }
}

#[derive(Debug, Error)]
pub(super) enum ResponsePolicyError {
    #[error("public content security policy exceeds 16384 bytes")]
    TooLarge,
    #[error("public content security policy is not a valid HTTP header")]
    InvalidHeader(#[source] InvalidHeaderValue),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend_assets::embedded_manifest;

    #[test]
    fn policy_pins_only_the_embedded_script_and_validated_asset_origins() {
        let origins = [ExternalAssetOrigin::parse("https://CDN.example:443/").unwrap()];
        let policy = PublicResponsePolicy::new(&origins, embedded_manifest()).unwrap();
        let header = policy.content_security_policy.to_str().unwrap();
        assert_eq!(
            header,
            concat!(
                "default-src 'none'; base-uri 'none'; object-src 'none'; frame-src 'none'; ",
                "frame-ancestors 'none'; form-action 'none'; connect-src 'none'; ",
                "script-src 'sha256-0z+0nPbFzJ8Ke1UM9Rn3k3hy20NfoVXBH2dMPP6ufO0='; script-src-attr 'none'; style-src 'self'; ",
                "img-src 'self' https://cdn.example data:; media-src 'self' https://cdn.example; font-src 'self'"
            )
        );
        assert!(!header.contains("unsafe-inline"));
        assert!(!header.contains("unsafe-eval"));
    }

    #[test]
    fn missing_javascript_disables_every_script_source() {
        let embedded = embedded_manifest();
        let frontend = FrontendAssetManifest {
            bundle_digest: embedded.bundle_digest,
            css: embedded.css.clone(),
            javascript: None,
        };
        let policy = PublicResponsePolicy::new(&[], &frontend).unwrap();
        assert!(policy.script_integrity.is_none());
        assert!(
            policy
                .content_security_policy
                .to_str()
                .unwrap()
                .contains("script-src 'none';")
        );
    }

    #[test]
    fn oversized_origin_policy_rejects_the_candidate() {
        let origins: Vec<_> = (0..200)
            .map(|index| {
                ExternalAssetOrigin::parse(&format!("https://{index}.{}.example", "a".repeat(50)))
                    .unwrap()
            })
            .collect();
        assert!(matches!(
            PublicResponsePolicy::new(&origins, embedded_manifest()),
            Err(ResponsePolicyError::TooLarge)
        ));
    }
}
