//! Standalone validator process startup, execution, and output handling.

use std::{
    fmt,
    fs::{self, File},
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::Parser;

use crate::{
    ContentTreeLimits, ContentValidationCode, ContentValidationErrors, PostCollection,
    PostDocument,
    cli::{Arguments, JsonError, JsonErrorReport, JsonReport},
    validate_post_document_bytes,
};

const SUCCESS: u8 = 0;
const INVALID_DOCUMENT: u8 = 65;
const INPUT_UNAVAILABLE: u8 = 66;
const INTERNAL_ERROR: u8 = 70;
const PERMISSION_DENIED: u8 = 77;

#[derive(Debug)]
enum CliError {
    InputUnavailable { path: PathBuf, detail: String },
    PermissionDenied { path: PathBuf, source: io::Error },
    Input { path: PathBuf, source: io::Error },
    Output(io::Error),
    Json(serde_json::Error),
    Internal(&'static str),
}

impl CliError {
    const fn exit_code(&self) -> u8 {
        match self {
            Self::InputUnavailable { .. } => INPUT_UNAVAILABLE,
            Self::PermissionDenied { .. } => PERMISSION_DENIED,
            Self::Input { .. } | Self::Output(_) | Self::Json(_) | Self::Internal(_) => {
                INTERNAL_ERROR
            }
        }
    }

    const fn json_code(&self) -> &'static str {
        match self {
            Self::InputUnavailable { .. } => "input_unavailable",
            Self::PermissionDenied { .. } => "permission_denied",
            Self::Input { .. } | Self::Output(_) | Self::Json(_) | Self::Internal(_) => {
                "internal_error"
            }
        }
    }

    fn path(&self) -> Option<&Path> {
        match self {
            Self::InputUnavailable { path, .. }
            | Self::PermissionDenied { path, .. }
            | Self::Input { path, .. } => Some(path.as_path()),
            Self::Output(_) | Self::Json(_) | Self::Internal(_) => None,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputUnavailable { path, detail } => {
                write!(formatter, "{}: input unavailable: {detail}", path.display())
            }
            Self::PermissionDenied { path, source } => {
                write!(formatter, "{}: permission denied: {source}", path.display())
            }
            Self::Input { path, source } => {
                write!(
                    formatter,
                    "{}: failed to read input: {source}",
                    path.display()
                )
            }
            Self::Output(source) => write!(formatter, "failed to write output: {source}"),
            Self::Json(source) => write!(formatter, "failed to encode JSON output: {source}"),
            Self::Internal(message) => formatter.write_str(message),
        }
    }
}

/// Runs the standalone single-document validator to completion.
pub fn run() -> ExitCode {
    let arguments = Arguments::parse();
    let json = arguments.json;
    match execute(arguments) {
        Ok(exit) => ExitCode::from(exit),
        Err(error) => {
            let exit = error.exit_code();
            let reported = if json {
                write_json_error(&error)
            } else {
                write_human_error(&error)
            };
            if reported.is_err() {
                return ExitCode::from(INTERNAL_ERROR);
            }
            ExitCode::from(exit)
        }
    }
}

fn execute(arguments: Arguments) -> Result<u8, CliError> {
    let contents = read_markdown(&arguments.markdown)?;
    let path_label = arguments.markdown.to_string_lossy();
    let validation = validate_post_document_bytes(
        path_label.as_ref(),
        &contents,
        PostCollection::from(arguments.collection),
    );

    if arguments.json {
        return write_json_result(path_label.as_ref(), validation);
    }

    match validation {
        Ok(_) => {
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            writeln!(stdout, "{path_label}: valid")
                .and_then(|()| stdout.flush())
                .map_err(CliError::Output)?;
            Ok(SUCCESS)
        }
        Err(diagnostics) => {
            let stderr = io::stderr();
            let mut stderr = stderr.lock();
            write_human_diagnostics(&mut stderr, path_label.as_ref(), &diagnostics)?;
            Ok(INVALID_DOCUMENT)
        }
    }
}

fn read_markdown(path: &Path) -> Result<Vec<u8>, CliError> {
    let metadata = fs::metadata(path).map_err(|source| classify_input_error(path, source))?;
    if !metadata.is_file() {
        return Err(CliError::InputUnavailable {
            path: path.to_owned(),
            detail: "path is not a regular file".to_owned(),
        });
    }

    let file = File::open(path).map_err(|source| classify_input_error(path, source))?;
    let limit = ContentTreeLimits::default().post_file_bytes.get();
    let read_limit = limit.saturating_add(1);
    let capacity = usize::try_from(metadata.len().min(read_limit)).map_err(|_| {
        CliError::Internal("the configured post byte limit does not fit this platform")
    })?;
    let mut contents = Vec::with_capacity(capacity);
    file.take(read_limit)
        .read_to_end(&mut contents)
        .map_err(|source| classify_input_error(path, source))?;
    Ok(contents)
}

fn classify_input_error(path: &Path, source: io::Error) -> CliError {
    match source.kind() {
        io::ErrorKind::NotFound => CliError::InputUnavailable {
            path: path.to_owned(),
            detail: source.to_string(),
        },
        io::ErrorKind::PermissionDenied => CliError::PermissionDenied {
            path: path.to_owned(),
            source,
        },
        _ => CliError::Input {
            path: path.to_owned(),
            source,
        },
    }
}

fn write_json_result(
    path: &str,
    validation: Result<PostDocument, ContentValidationErrors>,
) -> Result<u8, CliError> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    match validation {
        Ok(_) => {
            write_json_report(
                &mut stdout,
                &JsonReport {
                    path,
                    valid: true,
                    diagnostics: &[],
                },
            )?;
            Ok(SUCCESS)
        }
        Err(diagnostics) => {
            write_json_report(
                &mut stdout,
                &JsonReport {
                    path,
                    valid: false,
                    diagnostics: diagnostics.errors(),
                },
            )?;
            Ok(INVALID_DOCUMENT)
        }
    }
}

fn write_json_report(writer: &mut impl io::Write, report: &JsonReport<'_>) -> Result<(), CliError> {
    serde_json::to_writer(&mut *writer, report).map_err(CliError::Json)?;
    writer.write_all(b"\n").map_err(CliError::Output)?;
    writer.flush().map_err(CliError::Output)
}

fn write_json_error(error: &CliError) -> Result<(), CliError> {
    let path = error.path().map(|path| path.to_string_lossy().into_owned());
    let report = JsonErrorReport {
        error: JsonError {
            path: path.as_deref(),
            code: error.json_code(),
            message: error.to_string(),
        },
    };
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, &report).map_err(CliError::Json)?;
    stdout.write_all(b"\n").map_err(CliError::Output)?;
    stdout.flush().map_err(CliError::Output)
}

fn write_human_error(error: &CliError) -> Result<(), CliError> {
    let stderr = io::stderr();
    let mut stderr = stderr.lock();
    writeln!(stderr, "markdowncompiler: {error}")
        .and_then(|()| stderr.flush())
        .map_err(CliError::Output)
}

fn write_human_diagnostics(
    writer: &mut impl io::Write,
    path: &str,
    diagnostics: &ContentValidationErrors,
) -> Result<(), CliError> {
    for diagnostic in diagnostics.errors() {
        let code = validation_code_name(diagnostic.code)?;
        writeln!(
            writer,
            "{path}: {} [{code}]: {}",
            diagnostic.field, diagnostic.message
        )
        .map_err(CliError::Output)?;
        if let Some(related) = &diagnostic.related {
            writeln!(writer, "  related: {}: {}", related.path, related.field)
                .map_err(CliError::Output)?;
        }
    }

    let count = diagnostics.errors().len();
    let noun = if count == 1 {
        "diagnostic"
    } else {
        "diagnostics"
    };
    writeln!(writer, "{path}: invalid ({count} {noun})").map_err(CliError::Output)?;
    writer.flush().map_err(CliError::Output)
}

fn validation_code_name(code: ContentValidationCode) -> Result<String, CliError> {
    match serde_json::to_value(code).map_err(CliError::Json)? {
        serde_json::Value::String(value) => Ok(value),
        _ => Err(CliError::Internal(
            "validation code did not serialize as a JSON string",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io_error(kind: io::ErrorKind) -> io::Error {
        io::Error::new(kind, "fixture")
    }

    #[test]
    fn cli_errors_have_stable_exit_and_json_categories() {
        let path = PathBuf::from("post.md");
        let errors = [
            (
                CliError::InputUnavailable {
                    path: path.clone(),
                    detail: "missing".to_owned(),
                },
                INPUT_UNAVAILABLE,
                "input_unavailable",
            ),
            (
                CliError::PermissionDenied {
                    path: path.clone(),
                    source: io_error(io::ErrorKind::PermissionDenied),
                },
                PERMISSION_DENIED,
                "permission_denied",
            ),
            (
                CliError::Input {
                    path,
                    source: io_error(io::ErrorKind::Other),
                },
                INTERNAL_ERROR,
                "internal_error",
            ),
            (
                CliError::Output(io_error(io::ErrorKind::BrokenPipe)),
                INTERNAL_ERROR,
                "internal_error",
            ),
            (
                CliError::Json(
                    serde_json::from_str::<serde_json::Value>("{")
                        .expect_err("fixture JSON must be incomplete"),
                ),
                INTERNAL_ERROR,
                "internal_error",
            ),
            (
                CliError::Internal("fixture"),
                INTERNAL_ERROR,
                "internal_error",
            ),
        ];

        for (error, exit, json_code) in errors {
            assert_eq!(error.exit_code(), exit);
            assert_eq!(error.json_code(), json_code);
        }
    }
}
