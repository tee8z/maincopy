use std::{
    fmt,
    io::{self, Read},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use zeroize::{Zeroize as _, ZeroizeOnDrop};

const MAX_ENVIRONMENT_VARIABLE_BYTES: usize = 128;
const MAX_RESOLVED_SECRET_BYTES: usize = 64 * 1024;
const RESOLVED_SECRET_BUFFER_BYTES: usize = MAX_RESOLVED_SECRET_BYTES + 1;
const RESOLVED_SECRET_TOO_LARGE: &str = "resolved secret exceeds the inclusive 64 KiB hard limit";

macro_rules! redacted_path_type {
    ($(#[$attribute:meta])* $name:ident, $debug:literal, $display:literal) => {
        $(#[$attribute])*
        #[derive(Clone, Eq, PartialEq)]
        pub struct $name(PathBuf);

        impl $name {
            pub fn new(path: PathBuf) -> Option<Self> {
                (!path.as_os_str().is_empty()).then_some(Self(path))
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str($debug)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str($display)
            }
        }
    };
}

redacted_path_type!(
    /// A redacted reference to a file that contains protected bytes.
    SecretFileReference,
    "SecretFileReference(<redacted>)",
    "<redacted-secret-file-reference>"
);

/// Bytes in one fixed allocation that is wiped before deallocation.
///
/// This type intentionally has no cloning, serialization, dereference, slice
/// conversion, or inner-value extraction API. A consuming callback is the only
/// way to inspect the initialized bytes.
struct ResolvedSecret {
    storage: Box<[u8]>,
    len: usize,
    #[cfg(test)]
    drop_probe: Option<SecretDropProbe>,
}

impl ResolvedSecret {
    /// Reads at most 64 KiB directly into one fixed allocation.
    ///
    /// The extra byte is a sentinel that detects input beyond the inclusive
    /// hard limit. The allocation already belongs to `ResolvedSecret` while
    /// reads occur, so read failures also take the zeroizing drop path.
    fn read_from(reader: &mut impl Read) -> io::Result<Self> {
        Self::empty().fill_from(reader)
    }

    /// Gives one callback a scoped view and wipes the allocation afterwards.
    fn expose_to<Output>(
        self,
        use_secret: impl for<'secret> FnOnce(&'secret [u8]) -> Output,
    ) -> Output {
        use_secret(&self.storage[..self.len])
    }

    fn empty() -> Self {
        Self {
            // This vector contains only zeros. Secret bytes enter storage only
            // after it becomes a fixed-size boxed slice that cannot reallocate.
            storage: vec![0; RESOLVED_SECRET_BUFFER_BYTES].into_boxed_slice(),
            len: 0,
            #[cfg(test)]
            drop_probe: None,
        }
    }

    fn fill_from(mut self, reader: &mut impl Read) -> io::Result<Self> {
        loop {
            match reader.read(&mut self.storage[self.len..]) {
                Ok(0) => return Ok(self),
                Ok(read) => {
                    self.len += read;
                    if self.len > MAX_RESOLVED_SECRET_BYTES {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            RESOLVED_SECRET_TOO_LARGE,
                        ));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(test)]
    fn read_from_with_probe(
        reader: &mut impl Read,
        drop_probe: SecretDropProbe,
    ) -> io::Result<Self> {
        let mut secret = Self::empty();
        secret.drop_probe = Some(drop_probe);
        secret.fill_from(reader)
    }
}

impl Drop for ResolvedSecret {
    fn drop(&mut self) {
        self.storage.zeroize();
        self.len = 0;

        #[cfg(test)]
        if let Some(probe) = &self.drop_probe {
            probe.store(
                self.storage.iter().all(|byte| *byte == 0),
                std::sync::atomic::Ordering::SeqCst,
            );
        }
    }
}

impl ZeroizeOnDrop for ResolvedSecret {}

/// Reads from an already secured source and gives one callback a scoped view.
///
/// The provider-composition slice must open and validate the credential file
/// before it calls this boundary. This function does not define file-opening
/// policy.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "reserved for the pending secure Lexe provider-composition slice"
    )
)]
pub(crate) fn with_resolved_secret<Output>(
    reader: &mut impl Read,
    use_secret: impl for<'secret> FnOnce(&'secret [u8]) -> Output,
) -> io::Result<Output> {
    let secret = ResolvedSecret::read_from(reader)?;
    Ok(secret.expose_to(use_secret))
}

redacted_path_type!(
    /// A path whose existence can reveal protected provider metadata.
    SensitivePath,
    "SensitivePath(<redacted>)",
    "<redacted-sensitive-path>"
);

#[derive(Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SecretReferenceCandidate {
    File { path: PathBuf },
    Environment { variable: Box<str> },
}

impl fmt::Debug for SecretReferenceCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File { .. } => formatter.write_str("SecretReferenceCandidate::File(<redacted>)"),
            Self::Environment { .. } => {
                formatter.write_str("SecretReferenceCandidate::Environment(<redacted>)")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SecretReferenceCandidateError {
    Invalid,
    EnvironmentUnsupported,
}

impl SecretReferenceCandidate {
    pub(crate) fn finalize_file(
        self,
        file_base: &Path,
    ) -> Result<SecretFileReference, SecretReferenceCandidateError> {
        match self {
            Self::File { path } => {
                let path = super::host::resolve_path(file_base, &path)
                    .ok_or(SecretReferenceCandidateError::Invalid)?;
                SecretFileReference::new(path).ok_or(SecretReferenceCandidateError::Invalid)
            }
            Self::Environment { variable } if is_environment_variable_name(&variable) => {
                Err(SecretReferenceCandidateError::EnvironmentUnsupported)
            }
            Self::Environment { .. } => Err(SecretReferenceCandidateError::Invalid),
        }
    }
}

fn is_environment_variable_name(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_ENVIRONMENT_VARIABLE_BYTES {
        return false;
    }
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    matches!(first, b'A'..=b'Z' | b'_')
        && bytes.all(|byte| matches!(byte, b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

#[cfg(test)]
type SecretDropProbe = std::sync::Arc<std::sync::atomic::AtomicBool>;

#[cfg(test)]
mod tests {
    use std::{
        borrow::Borrow,
        io::Cursor,
        ops::Deref,
        panic::{AssertUnwindSafe, catch_unwind},
    };

    use serde::{Serialize, de::DeserializeOwned};

    use super::*;

    macro_rules! assert_not_impl {
        ($value:ty: $bound:path) => {
            const _: fn() = || {
                trait AmbiguousIfImpl<Marker> {
                    fn marker() {}
                }

                impl<Value: ?Sized> AmbiguousIfImpl<()> for Value {}
                impl<Value: ?Sized + $bound> AmbiguousIfImpl<u8> for Value {}

                let _ = <$value as AmbiguousIfImpl<_>>::marker;
            };
        };
    }

    assert_not_impl!(ResolvedSecret: Copy);
    assert_not_impl!(ResolvedSecret: Clone);
    assert_not_impl!(ResolvedSecret: Serialize);
    assert_not_impl!(ResolvedSecret: DeserializeOwned);
    assert_not_impl!(ResolvedSecret: Eq);
    assert_not_impl!(ResolvedSecret: PartialEq);
    assert_not_impl!(ResolvedSecret: Deref);
    assert_not_impl!(ResolvedSecret: AsRef<[u8]>);
    assert_not_impl!(ResolvedSecret: Borrow<[u8]>);

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _output: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic read failure"))
        }
    }

    fn probed_secret() -> (ResolvedSecret, SecretDropProbe) {
        let probe = SecretDropProbe::default();
        let mut reader = io::repeat(0xa5).take(MAX_RESOLVED_SECRET_BYTES as u64);
        let secret = ResolvedSecret::read_from_with_probe(&mut reader, probe.clone()).unwrap();
        (secret, probe)
    }

    fn assert_zeroized_after_drop(probe: &SecretDropProbe) {
        assert!(probe.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn references_candidates_and_sensitive_paths_are_redacted() {
        let file = SecretFileReference::new(PathBuf::from("/secret/lexe.json")).unwrap();
        let environment = SecretReferenceCandidate::Environment {
            variable: "MAINCOPY_TOKEN".into(),
        };
        let cache = SensitivePath::new(PathBuf::from("/secret/lexe-cache")).unwrap();

        let rendered = format!("{file:?} {file} {environment:?} {cache:?} {cache}");
        for protected in ["/secret", "MAINCOPY_TOKEN", "lexe.json", "lexe-cache"] {
            assert!(!rendered.contains(protected));
        }
    }

    #[test]
    fn environment_candidates_distinguish_unsupported_from_invalid() {
        for accepted in ["A", "_TOKEN", "MAINCOPY_LEXE_CREDENTIAL"] {
            let candidate = SecretReferenceCandidate::Environment {
                variable: accepted.into(),
            };
            assert_eq!(
                candidate.finalize_file(Path::new("host")).unwrap_err(),
                SecretReferenceCandidateError::EnvironmentUnsupported
            );
        }
        for rejected in ["", "lower", "1TOKEN", "HAS-DASH", "HAS=VALUE"] {
            let candidate = SecretReferenceCandidate::Environment {
                variable: rejected.into(),
            };
            assert_eq!(
                candidate.finalize_file(Path::new("host")).unwrap_err(),
                SecretReferenceCandidateError::Invalid
            );
        }
        let candidate = SecretReferenceCandidate::Environment {
            variable: "A".repeat(MAX_ENVIRONMENT_VARIABLE_BYTES + 1).into(),
        };
        assert_eq!(
            candidate.finalize_file(Path::new("host")).unwrap_err(),
            SecretReferenceCandidateError::Invalid
        );
    }

    #[test]
    fn file_candidate_finalizes_against_the_host_file_base() {
        let candidate = SecretReferenceCandidate::File {
            path: PathBuf::from("secret/credential.json"),
        };
        let reference = candidate.finalize_file(Path::new("host")).unwrap();
        assert_eq!(reference.path(), Path::new("host/secret/credential.json"));
    }

    #[test]
    fn empty_file_candidate_is_invalid() {
        let candidate = SecretReferenceCandidate::File {
            path: PathBuf::new(),
        };
        assert_eq!(
            candidate.finalize_file(Path::new("host")).unwrap_err(),
            SecretReferenceCandidateError::Invalid
        );
    }

    #[test]
    fn candidate_tables_reject_unknown_fields() {
        let source = "source = \"file\"\npath = \"credential.json\"\nvalue = \"secret\"";
        assert!(toml::from_str::<SecretReferenceCandidate>(source).is_err());
    }

    #[test]
    fn resolved_secret_boundary_exposes_only_a_scoped_borrow() {
        let length =
            with_resolved_secret(&mut Cursor::new(b"protected"), |bytes| bytes.len()).unwrap();

        assert_eq!(length, 9);
    }

    #[test]
    fn normal_callback_return_zeroizes_the_complete_allocation() {
        let (secret, probe) = probed_secret();
        assert!(!probe.load(std::sync::atomic::Ordering::SeqCst));

        let length = secret.expose_to(|bytes| bytes.len());

        assert_eq!(length, MAX_RESOLVED_SECRET_BYTES);
        assert_zeroized_after_drop(&probe);
    }

    #[test]
    fn ordinary_drop_zeroizes_the_complete_allocation() {
        let (secret, probe) = probed_secret();

        drop(secret);

        assert_zeroized_after_drop(&probe);
    }

    #[test]
    fn callback_error_zeroizes_the_complete_allocation() {
        let (secret, probe) = probed_secret();

        let result: Result<(), &'static str> = secret.expose_to(|_| Err("synthetic parse error"));

        assert_eq!(result, Err("synthetic parse error"));
        assert_zeroized_after_drop(&probe);
    }

    #[test]
    fn callback_panic_zeroizes_the_complete_allocation_during_unwind() {
        let (secret, probe) = probed_secret();

        let result = catch_unwind(AssertUnwindSafe(|| {
            secret.expose_to::<()>(|_| panic!("synthetic callback panic"));
        }));

        assert!(result.is_err());
        assert_zeroized_after_drop(&probe);
    }

    #[test]
    fn partial_read_error_zeroizes_initialized_bytes_before_deallocation() {
        let probe = SecretDropProbe::default();
        let mut reader = Cursor::new([0xa5; 32]).chain(FailingReader);
        let error = ResolvedSecret::read_from_with_probe(&mut reader, probe.clone())
            .err()
            .unwrap();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_zeroized_after_drop(&probe);
    }

    #[test]
    fn inclusive_hard_limit_is_accepted() {
        let mut reader = io::repeat(0xa5).take(MAX_RESOLVED_SECRET_BYTES as u64);
        let length = ResolvedSecret::read_from(&mut reader)
            .unwrap()
            .expose_to(|bytes| bytes.len());
        assert_eq!(length, MAX_RESOLVED_SECRET_BYTES);
    }

    #[test]
    fn sentinel_rejects_one_byte_over_limit_and_zeroizes_it() {
        let probe = SecretDropProbe::default();
        let mut reader = io::repeat(0xa5).take(RESOLVED_SECRET_BUFFER_BYTES as u64);

        let error = ResolvedSecret::read_from_with_probe(&mut reader, probe.clone())
            .err()
            .unwrap();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), RESOLVED_SECRET_TOO_LARGE);
        assert_zeroized_after_drop(&probe);
    }
}
