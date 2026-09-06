//! Bounded extraction of an Ed25519 deploy key's public identity.

use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
use sha2::{Digest as _, Sha256};
use std::{path::Path, process::Stdio, time::Duration};
use thiserror::Error;
use tokio::io::AsyncReadExt as _;

const MAX_PUBLIC_KEY_BYTES: u64 = 16 * 1024;
const KEY_INSPECTION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub(crate) enum SourceKeyError {
    #[error("the deploy public key could not be derived")]
    Derivation,
    #[error("the deploy public key is not a valid Ed25519 identity")]
    InvalidPublicKey,
}

pub(crate) async fn derive_public_identity(
    private_key: &Path,
) -> Result<(String, String), SourceKeyError> {
    let executable = option_env!("MAINCOPY_SSH_KEYGEN").unwrap_or("ssh-keygen");
    let mut child = tokio::process::Command::new(executable)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .args(["-y", "-f"])
        .arg(private_key)
        .spawn()
        .map_err(|_| SourceKeyError::Derivation)?;
    let stdout = child.stdout.take().ok_or(SourceKeyError::Derivation)?;
    let mut output = Vec::new();
    let completed = tokio::time::timeout(KEY_INSPECTION_TIMEOUT, async {
        let read = stdout
            .take(MAX_PUBLIC_KEY_BYTES + 1)
            .read_to_end(&mut output)
            .await;
        (read, child.wait().await)
    })
    .await;
    if !matches!(completed, Ok((Ok(_), Ok(status))) if status.success())
        || output.len() as u64 > MAX_PUBLIC_KEY_BYTES
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(SourceKeyError::Derivation);
    }
    let source = std::str::from_utf8(&output).map_err(|_| SourceKeyError::InvalidPublicKey)?;
    parse_public_key(source)
}

pub(crate) fn parse_public_key(source: &str) -> Result<(String, String), SourceKeyError> {
    let mut lines = source.lines();
    let line = lines.next().unwrap_or_default();
    if lines.any(|line| !line.is_empty()) {
        return Err(SourceKeyError::InvalidPublicKey);
    }
    let mut fields = line.split_ascii_whitespace();
    if fields.next() != Some("ssh-ed25519") {
        return Err(SourceKeyError::InvalidPublicKey);
    }
    let encoded = fields.next().ok_or(SourceKeyError::InvalidPublicKey)?;
    if fields.any(|field| field.chars().any(char::is_control)) {
        return Err(SourceKeyError::InvalidPublicKey);
    }
    let blob = STANDARD_NO_PAD
        .decode(encoded)
        .map_err(|_| SourceKeyError::InvalidPublicKey)?;
    if !valid_ed25519_blob(&blob) {
        return Err(SourceKeyError::InvalidPublicKey);
    }
    let fingerprint = STANDARD_NO_PAD.encode(Sha256::digest(&blob));
    Ok((
        format!("ssh-ed25519 {encoded}"),
        format!("SHA256:{fingerprint}"),
    ))
}

fn valid_ed25519_blob(blob: &[u8]) -> bool {
    const NAME: &[u8] = b"ssh-ed25519";
    blob.len() == 4 + NAME.len() + 4 + 32
        && blob.get(..4) == Some(&(NAME.len() as u32).to_be_bytes())
        && blob.get(4..4 + NAME.len()) == Some(NAME)
        && blob.get(4 + NAME.len()..8 + NAME.len()) == Some(&32_u32.to_be_bytes())
}
