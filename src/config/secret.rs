use std::{fmt, path::Path, path::PathBuf};

use serde::Deserialize;

const MAX_ENVIRONMENT_VARIABLE_BYTES: usize = 128;

/// Identifies the external source of a protected value without containing it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretReferenceKind {
    File,
    Environment,
}

/// Resolves only the explicitly supplied secret reference.
pub trait SecretResolver {
    type Secret;
    type Error;

    fn resolve_file(&self, reference: &SecretFileReference) -> Result<Self::Secret, Self::Error>;

    fn resolve_environment(
        &self,
        reference: &EnvironmentSecretReference,
    ) -> Result<Self::Secret, Self::Error>;
}

/// A redacted reference to a file that contains protected bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretFileReference {
    path: PathBuf,
}

impl SecretFileReference {
    pub fn new(path: PathBuf) -> Option<Self> {
        (!path.as_os_str().is_empty()).then_some(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn resolve_with<Resolver>(
        &self,
        resolver: &Resolver,
    ) -> Result<Resolver::Secret, Resolver::Error>
    where
        Resolver: SecretResolver,
    {
        resolver.resolve_file(self)
    }
}

impl fmt::Debug for SecretFileReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretFileReference(<redacted>)")
    }
}

impl fmt::Display for SecretFileReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted-secret-file-reference>")
    }
}

/// A redacted reference to one explicitly named environment variable.
#[derive(Clone, Eq, PartialEq)]
pub struct EnvironmentSecretReference {
    variable: Box<str>,
}

impl EnvironmentSecretReference {
    pub fn new(variable: impl Into<Box<str>>) -> Option<Self> {
        let variable = variable.into();
        is_environment_variable_name(&variable).then_some(Self { variable })
    }

    pub fn variable(&self) -> &str {
        &self.variable
    }

    pub fn resolve_with<Resolver>(
        &self,
        resolver: &Resolver,
    ) -> Result<Resolver::Secret, Resolver::Error>
    where
        Resolver: SecretResolver,
    {
        resolver.resolve_environment(self)
    }
}

impl fmt::Debug for EnvironmentSecretReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EnvironmentSecretReference(<redacted>)")
    }
}

impl fmt::Display for EnvironmentSecretReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted-environment-secret-reference>")
    }
}

/// A typed external secret reference. It never contains the secret value.
#[derive(Clone, Eq, PartialEq)]
pub enum SecretReference {
    File(SecretFileReference),
    Environment(EnvironmentSecretReference),
}

impl SecretReference {
    pub const fn kind(&self) -> SecretReferenceKind {
        match self {
            Self::File(_) => SecretReferenceKind::File,
            Self::Environment(_) => SecretReferenceKind::Environment,
        }
    }

    pub fn into_file(self) -> Result<SecretFileReference, EnvironmentSecretReference> {
        match self {
            Self::File(reference) => Ok(reference),
            Self::Environment(reference) => Err(reference),
        }
    }

    pub fn resolve_with<Resolver>(
        &self,
        resolver: &Resolver,
    ) -> Result<Resolver::Secret, Resolver::Error>
    where
        Resolver: SecretResolver,
    {
        match self {
            Self::File(reference) => reference.resolve_with(resolver),
            Self::Environment(reference) => reference.resolve_with(resolver),
        }
    }
}

impl fmt::Debug for SecretReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(_) => formatter.write_str("SecretReference::File(<redacted>)"),
            Self::Environment(_) => formatter.write_str("SecretReference::Environment(<redacted>)"),
        }
    }
}

impl fmt::Display for SecretReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted-secret-reference>")
    }
}

/// A path whose existence can reveal protected provider metadata.
#[derive(Clone, Eq, PartialEq)]
pub struct SensitivePath {
    path: PathBuf,
}

impl SensitivePath {
    pub fn new(path: PathBuf) -> Option<Self> {
        (!path.as_os_str().is_empty()).then_some(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for SensitivePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SensitivePath(<redacted>)")
    }
}

impl fmt::Display for SensitivePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted-sensitive-path>")
    }
}

#[derive(Clone, Deserialize)]
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

impl SecretReferenceCandidate {
    pub(crate) fn finalize(self, file_base: &Path) -> Option<SecretReference> {
        match self {
            Self::File { path } if !path.as_os_str().is_empty() => {
                let path = super::host::resolve_path(file_base, &path)?;
                SecretFileReference::new(path).map(SecretReference::File)
            }
            Self::Environment { variable } => {
                EnvironmentSecretReference::new(variable).map(SecretReference::Environment)
            }
            Self::File { .. } => None,
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
mod tests {
    use super::*;

    #[test]
    fn references_and_sensitive_paths_are_redacted() {
        let file = SecretReference::File(
            SecretFileReference::new(PathBuf::from("/secret/lexe.json")).unwrap(),
        );
        let environment = SecretReference::Environment(
            EnvironmentSecretReference::new("MAINCOPY_TOKEN").unwrap(),
        );
        let cache = SensitivePath::new(PathBuf::from("/secret/lexe-cache")).unwrap();

        let rendered = format!("{file:?} {file} {environment:?} {environment} {cache:?} {cache}");
        for protected in ["/secret", "MAINCOPY_TOKEN", "lexe.json", "lexe-cache"] {
            assert!(!rendered.contains(protected));
        }
    }

    #[test]
    fn environment_names_are_bounded_and_portable() {
        for accepted in ["A", "_TOKEN", "MAINCOPY_LEXE_CREDENTIAL"] {
            assert!(EnvironmentSecretReference::new(accepted).is_some());
        }
        for rejected in ["", "lower", "1TOKEN", "HAS-DASH", "HAS=VALUE"] {
            assert!(EnvironmentSecretReference::new(rejected).is_none());
        }
        assert!(
            EnvironmentSecretReference::new("A".repeat(MAX_ENVIRONMENT_VARIABLE_BYTES + 1))
                .is_none()
        );
    }

    #[test]
    fn candidate_tables_reject_unknown_fields() {
        let source = "source = \"file\"\npath = \"credential.json\"\nvalue = \"secret\"";
        assert!(toml::from_str::<SecretReferenceCandidate>(source).is_err());
    }

    #[test]
    fn explicit_resolver_hook_dispatches_without_an_environment_overlay() {
        struct Resolver;

        impl SecretResolver for Resolver {
            type Secret = SecretReferenceKind;
            type Error = std::convert::Infallible;

            fn resolve_file(
                &self,
                _reference: &SecretFileReference,
            ) -> Result<Self::Secret, Self::Error> {
                Ok(SecretReferenceKind::File)
            }

            fn resolve_environment(
                &self,
                _reference: &EnvironmentSecretReference,
            ) -> Result<Self::Secret, Self::Error> {
                Ok(SecretReferenceKind::Environment)
            }
        }

        let file = SecretReference::File(
            SecretFileReference::new(PathBuf::from("credential.json")).unwrap(),
        );
        let environment = SecretReference::Environment(
            EnvironmentSecretReference::new("MAINCOPY_TOKEN").unwrap(),
        );
        assert_eq!(
            file.resolve_with(&Resolver).unwrap(),
            SecretReferenceKind::File
        );
        assert_eq!(
            environment.resolve_with(&Resolver).unwrap(),
            SecretReferenceKind::Environment
        );
    }
}
