use std::fmt;

use axum::{
    body::Body,
    extract::Path,
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_NONE_MATCH},
    },
    response::Response,
};

const ADMIN_STYLESHEET: &[u8] = include_bytes!("../../frontend/admin/site.css");
const ADMIN_ASSET_CONTEXT: &str = "maincopy admin frontend bundle v1";
const ADMIN_ASSET_PREFIX: &str = "admin-b3-v1-";
const STYLESHEET_NAME: &str = "site.css";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AdminAssetBundleDigest([u8; 32]);

impl AdminAssetBundleDigest {
    fn current() -> Self {
        let mut hasher = blake3::Hasher::new_derive_key(ADMIN_ASSET_CONTEXT);
        hasher.update(&(ADMIN_STYLESHEET.len() as u64).to_be_bytes());
        hasher.update(ADMIN_STYLESHEET);
        Self(*hasher.finalize().as_bytes())
    }

    fn parse(value: &str) -> Option<Self> {
        let encoded = value.strip_prefix(ADMIN_ASSET_PREFIX)?;
        if encoded.len() != 64 {
            return None;
        }
        let decoded = blake3::Hash::from_hex(encoded).ok()?;
        (decoded.to_hex().as_str() == encoded).then(|| Self(*decoded.as_bytes()))
    }
}

impl fmt::Display for AdminAssetBundleDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{ADMIN_ASSET_PREFIX}{}",
            blake3::Hash::from_bytes(self.0).to_hex()
        )
    }
}

pub(crate) fn stylesheet_path() -> String {
    format!(
        "/admin/assets/{}/{STYLESHEET_NAME}",
        AdminAssetBundleDigest::current()
    )
}

pub(crate) async fn get(
    Path((digest, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(digest) = AdminAssetBundleDigest::parse(&digest) else {
        return not_found();
    };
    if digest != AdminAssetBundleDigest::current() || name != STYLESHEET_NAME {
        return not_found();
    }

    let etag = HeaderValue::from_str(&format!("\"{digest}\""))
        .expect("an admin asset digest always forms a valid ETag");
    if headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .any(|candidate| candidate == etag)
    {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        response.headers_mut().insert(ETAG, etag);
        return response;
    }

    let mut response = Response::new(Body::from(ADMIN_STYLESHEET));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/css; charset=utf-8"),
    );
    response.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&ADMIN_STYLESHEET.len().to_string())
            .expect("the bounded stylesheet length is a valid header value"),
    );
    response.headers_mut().insert(ETAG, etag);
    response
}

fn not_found() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

#[cfg(test)]
mod tests {
    use axum::{Router, http::Request, routing::get as route_get};
    use tower::ServiceExt as _;

    use super::*;

    #[test]
    fn stylesheet_path_is_content_addressed_and_stable() {
        let path = stylesheet_path();
        let expected = AdminAssetBundleDigest::current().to_string();

        assert_eq!(path, format!("/admin/assets/{expected}/{STYLESHEET_NAME}"));
        assert_eq!(
            AdminAssetBundleDigest::parse(&expected),
            Some(AdminAssetBundleDigest::current())
        );
        assert!(ADMIN_STYLESHEET.len() < 16 * 1024);
        let stylesheet = std::str::from_utf8(ADMIN_STYLESHEET).unwrap();
        assert!(!stylesheet.contains("@import"));
        assert!(!stylesheet.contains("url("));
    }

    #[tokio::test]
    async fn only_the_exact_embedded_stylesheet_is_served() {
        let router = Router::new().route("/admin/assets/{digest}/{name}", route_get(get));
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(stylesheet_path())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/css; charset=utf-8");

        let response = router
            .oneshot(
                Request::builder()
                    .uri(stylesheet_path().replace(STYLESHEET_NAME, "other.css"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
