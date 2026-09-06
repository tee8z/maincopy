use std::{future::Future, io};

use maincopy_shared::auth_api::{AdminSessionResponse, SecretString};

use super::CliError;
use crate::client::{AdminClientError, HumanNostrLogin};

pub(super) async fn execute<BeginFuture, CompleteFuture>(
    begin: impl FnOnce() -> BeginFuture,
    complete: impl FnOnce(HumanNostrLogin, SecretString) -> CompleteFuture,
    prompt: impl FnOnce(&str) -> io::Result<SecretString>,
    output: impl io::Write,
) -> Result<AdminSessionResponse, CliError>
where
    BeginFuture: Future<Output = Result<HumanNostrLogin, AdminClientError>>,
    CompleteFuture: Future<Output = Result<AdminSessionResponse, AdminClientError>>,
{
    let login = begin().await?;
    let proof = read_proof(|output| login.write_signing_request(output), prompt, output)?;
    complete(login, proof).await.map_err(CliError::from)
}

fn read_proof<Output: io::Write>(
    write_request: impl FnOnce(&mut Output) -> Result<(), serde_json::Error>,
    prompt: impl FnOnce(&str) -> io::Result<SecretString>,
    mut output: Output,
) -> Result<SecretString, CliError> {
    writeln!(
        output,
        "Sign this exact event with your human Nostr signer within 60 seconds. Keep its private key in that signer."
    )?;
    write_request(&mut output)?;
    writeln!(output)?;
    output.flush()?;
    prompt("Paste the signed event JSON as one line (hidden): ").map_err(CliError::SecretInput)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_signing_prints_only_the_request_and_reads_the_proof_from_the_protected_prompt() {
        let mut output = Vec::new();
        let proof = read_proof(
            |output| {
                serde_json::to_writer(
                    output,
                    &serde_json::json!({"kind":27235,"challenge":"fixture challenge"}),
                )
            },
            |prompt| {
                assert!(prompt.contains("hidden"));
                Ok(SecretString::new("fixture signed proof"))
            },
            &mut output,
        )
        .unwrap();
        assert_eq!(proof.expose_secret(), "fixture signed proof");
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("fixture challenge"));
        assert!(!text.contains("fixture signed proof"));
        assert!(text.contains("60 seconds"));
    }

    #[test]
    fn cancelled_signing_and_output_failure_return_before_a_proof_is_submitted() {
        let error = read_proof(
            |output| serde_json::to_writer(output, &serde_json::json!({"kind":27235})),
            |_| Err(io::Error::from(io::ErrorKind::Interrupted)),
            Vec::new(),
        )
        .unwrap_err();
        assert!(
            matches!(error, CliError::SecretInput(source) if source.kind() == io::ErrorKind::Interrupted)
        );
        let error = read_proof(
            |_| panic!("request cannot be shown after output fails"),
            |_| panic!("must not prompt"),
            io::Cursor::new(&mut [0_u8; 0][..]),
        )
        .unwrap_err();
        assert!(matches!(error, CliError::Output(_)));
    }
}
