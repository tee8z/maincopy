use std::fmt;

use serde::{Deserialize, Serialize};

const HOST_AUTHORITY: &str = "host";

/// Stable machine-readable host configuration validation codes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationValidationCode {
    WorkingDirectoryUnavailable,
    HostFileUnreadable,
    HostDocumentTooLarge,
    HostTextInvalidUtf8,
    HostTomlInvalid,
    AdminBindInvalid,
    AdminOriginInvalid,
    SourceModeConflict,
    PathInvalid,
    SecretReferenceInvalid,
    LimitOutOfRange,
    DurationInvalid,
    PageSizeInvalid,
    ContentLimitRelationshipInvalid,
}

impl ConfigurationValidationCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkingDirectoryUnavailable => "working_directory_unavailable",
            Self::HostFileUnreadable => "host_file_unreadable",
            Self::HostDocumentTooLarge => "host_document_too_large",
            Self::HostTextInvalidUtf8 => "host_text_invalid_utf8",
            Self::HostTomlInvalid => "host_toml_invalid",
            Self::AdminBindInvalid => "admin_bind_invalid",
            Self::AdminOriginInvalid => "admin_origin_invalid",
            Self::SourceModeConflict => "source_mode_conflict",
            Self::PathInvalid => "path_invalid",
            Self::SecretReferenceInvalid => "secret_reference_invalid",
            Self::LimitOutOfRange => "limit_out_of_range",
            Self::DurationInvalid => "duration_invalid",
            Self::PageSizeInvalid => "page_size_invalid",
            Self::ContentLimitRelationshipInvalid => "content_limit_relationship_invalid",
        }
    }
}

impl fmt::Display for ConfigurationValidationCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One redaction-safe host configuration diagnostic.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[non_exhaustive]
pub struct ConfigurationDiagnostic {
    pub authority: &'static str,
    pub field: Box<str>,
    pub code: ConfigurationValidationCode,
    pub message: &'static str,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

impl ConfigurationDiagnostic {
    pub(crate) fn new(
        field: impl Into<Box<str>>,
        code: ConfigurationValidationCode,
        message: &'static str,
    ) -> Self {
        Self {
            authority: HOST_AUTHORITY,
            field: field.into(),
            code,
            message,
            line: None,
            column: None,
        }
    }

    pub(crate) const fn at(mut self, line: usize, column: usize) -> Self {
        self.line = Some(line);
        self.column = Some(column);
        self
    }
}

impl fmt::Display for ConfigurationDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}:{}", self.authority, self.field, self.code)?;
        if let (Some(line), Some(column)) = (self.line, self.column) {
            write!(formatter, " at {line}:{column}")?;
        }
        write!(formatter, ": {}", self.message)
    }
}

/// A non-empty, deterministically ordered host configuration error set.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConfigurationErrors {
    diagnostics: Vec<ConfigurationDiagnostic>,
}

impl ConfigurationErrors {
    pub(crate) fn from_diagnostics(mut diagnostics: Vec<ConfigurationDiagnostic>) -> Self {
        debug_assert!(!diagnostics.is_empty());
        diagnostics.sort();
        diagnostics.dedup();
        Self { diagnostics }
    }

    pub fn diagnostics(&self) -> &[ConfigurationDiagnostic] {
        &self.diagnostics
    }
}

impl fmt::Display for ConfigurationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("configuration validation failed")?;
        for diagnostic in &self.diagnostics {
            write!(formatter, "; {diagnostic}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigurationErrors {}

#[derive(Default)]
pub(crate) struct DiagnosticCollector {
    diagnostics: Vec<ConfigurationDiagnostic>,
}

impl DiagnosticCollector {
    pub(crate) fn push(&mut self, diagnostic: ConfigurationDiagnostic) {
        self.diagnostics.push(diagnostic);
    }

    pub(crate) fn into_result(self) -> Result<(), ConfigurationErrors> {
        if self.diagnostics.is_empty() {
            Ok(())
        } else {
            Err(ConfigurationErrors::from_diagnostics(self.diagnostics))
        }
    }
}

pub(crate) fn single_error(diagnostic: ConfigurationDiagnostic) -> ConfigurationErrors {
    ConfigurationErrors::from_diagnostics(vec![diagnostic])
}

pub(crate) fn toml_location(source: &str, error: &toml::de::Error) -> Option<(usize, usize)> {
    let offset = error.span()?.start.min(source.len());
    let prefix = source.get(..offset)?;
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix, |(_, suffix)| suffix)
        .chars()
        .count()
        + 1;
    Some((line, column))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_codes_match_the_serde_contract() {
        let cases = [
            (
                ConfigurationValidationCode::WorkingDirectoryUnavailable,
                "working_directory_unavailable",
            ),
            (
                ConfigurationValidationCode::HostFileUnreadable,
                "host_file_unreadable",
            ),
            (
                ConfigurationValidationCode::HostDocumentTooLarge,
                "host_document_too_large",
            ),
            (
                ConfigurationValidationCode::HostTextInvalidUtf8,
                "host_text_invalid_utf8",
            ),
            (
                ConfigurationValidationCode::HostTomlInvalid,
                "host_toml_invalid",
            ),
            (
                ConfigurationValidationCode::AdminBindInvalid,
                "admin_bind_invalid",
            ),
            (
                ConfigurationValidationCode::AdminOriginInvalid,
                "admin_origin_invalid",
            ),
            (
                ConfigurationValidationCode::SourceModeConflict,
                "source_mode_conflict",
            ),
            (ConfigurationValidationCode::PathInvalid, "path_invalid"),
            (
                ConfigurationValidationCode::SecretReferenceInvalid,
                "secret_reference_invalid",
            ),
            (
                ConfigurationValidationCode::LimitOutOfRange,
                "limit_out_of_range",
            ),
            (
                ConfigurationValidationCode::DurationInvalid,
                "duration_invalid",
            ),
            (
                ConfigurationValidationCode::PageSizeInvalid,
                "page_size_invalid",
            ),
            (
                ConfigurationValidationCode::ContentLimitRelationshipInvalid,
                "content_limit_relationship_invalid",
            ),
        ];

        for (code, expected) in cases {
            assert_eq!(code.as_str(), expected);
            assert_eq!(serde_json::to_value(code).unwrap(), expected);
        }
    }

    #[test]
    fn diagnostics_are_sorted_and_do_not_contain_source_text() {
        let errors = ConfigurationErrors::from_diagnostics(vec![
            ConfigurationDiagnostic::new(
                "paths.content_root",
                ConfigurationValidationCode::PathInvalid,
                "configured path must not be empty",
            ),
            ConfigurationDiagnostic::new(
                "$document",
                ConfigurationValidationCode::HostTomlInvalid,
                "host TOML does not match the schema",
            )
            .at(2, 3),
        ]);

        assert_eq!(errors.diagnostics()[0].field.as_ref(), "$document");
        let rendered = format!("{errors:?}");
        assert!(!rendered.contains("credential-value"));
        assert!(!rendered.contains("source ="));
    }

    #[test]
    fn diagnostic_wire_contract_keeps_the_host_authority() {
        let diagnostic = ConfigurationDiagnostic::new(
            "$document",
            ConfigurationValidationCode::HostTomlInvalid,
            "host TOML does not match the schema",
        )
        .at(2, 3);

        assert_eq!(
            serde_json::to_value(diagnostic).unwrap(),
            serde_json::from_str::<serde_json::Value>(
                r#"{"authority":"host","field":"$document","code":"host_toml_invalid","message":"host TOML does not match the schema","line":2,"column":3}"#,
            )
            .unwrap()
        );
    }

    #[test]
    fn toml_locations_use_one_based_unicode_scalar_columns() {
        #[derive(Debug, Deserialize)]
        struct Document {
            #[serde(rename = "root")]
            _root: Root,
        }

        #[derive(Debug, Deserialize)]
        struct Root {
            #[serde(rename = "label")]
            _label: String,
            #[serde(rename = "count")]
            _count: u64,
        }

        let source = "root = { label = \"é\", count = \"bad\" }\n";
        let error = toml::from_str::<Document>(source).unwrap_err();

        assert_eq!(toml_location(source, &error), Some((1, 31)));
    }
}
