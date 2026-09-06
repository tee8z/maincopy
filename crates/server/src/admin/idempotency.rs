use axum::http::HeaderMap;
use maincopy_shared::publication::IDEMPOTENCY_KEY_HEADER;
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum IdempotencyKeyError {
    #[error("Idempotency-Key is required")]
    Missing,
    #[error("Idempotency-Key must contain one canonical UUID")]
    Invalid,
}

/// Parses retry identity without selecting a domain's HTTP error contract.
pub(crate) fn parse_idempotency_key(headers: &HeaderMap) -> Result<Uuid, IdempotencyKeyError> {
    let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER).iter();
    let value = values.next().ok_or(IdempotencyKeyError::Missing)?;
    if values.next().is_some() {
        return Err(IdempotencyKeyError::Invalid);
    }
    let encoded = value.to_str().map_err(|_| IdempotencyKeyError::Invalid)?;
    let key = Uuid::parse_str(encoded).map_err(|_| IdempotencyKeyError::Invalid)?;
    if key.hyphenated().to_string() != encoded {
        return Err(IdempotencyKeyError::Invalid);
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn retry_identity_rejects_non_ascii_and_repeated_identical_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_bytes(b"\xff").unwrap(),
        );
        assert_eq!(
            parse_idempotency_key(&headers),
            Err(IdempotencyKeyError::Invalid)
        );

        let value = HeaderValue::from_static("67e55044-10b1-426f-9247-bb680e5fe0c8");
        headers.insert(IDEMPOTENCY_KEY_HEADER, value.clone());
        headers.append(IDEMPOTENCY_KEY_HEADER, value);
        assert_eq!(
            parse_idempotency_key(&headers),
            Err(IdempotencyKeyError::Invalid)
        );
    }
}
