use std::{
    env,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
    process::ExitCode,
};

use mermaid_rs_renderer::{RenderOptions, render_strict};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
use thiserror::Error;

use crate::cli::Invocation;
use crate::protocol::{
    DETERMINISTIC_ENVIRONMENT, HelperExit, MAX_ADDRESS_SPACE_BYTES, MAX_CPU_SECONDS,
    MAX_RAW_SVG_BYTES, MAX_SOURCE_BYTES, MAX_STACK_BYTES, PROTOCOL_VERSION,
};

/// Runs the isolated Mermaid rendering helper once.
pub fn run() -> ExitCode {
    match run_inner() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("maincopy Mermaid renderer failed: {error}");
            ExitCode::from(error.exit().code())
        }
    }
}

fn run_inner() -> Result<(), HelperError> {
    match Invocation::parse_process_arguments()? {
        Invocation::ProtocolVersion { output } => write_protocol_version(&output),
        Invocation::Render { input, output } => render_file(&input, &output),
    }
}

fn write_protocol_version(output: &Path) -> Result<(), HelperError> {
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(HelperError::CreateOutput)?;
    writeln!(destination, "{PROTOCOL_VERSION}").map_err(HelperError::WriteOutput)
}

fn render_file(input: &Path, output: &Path) -> Result<(), HelperError> {
    validate_environment()?;
    install_resource_limits()?;
    render_prepared_file(input, output)
}

fn render_prepared_file(input: &Path, output: &Path) -> Result<(), HelperError> {
    let source = read_source(input)?;
    let svg = render_source(&source)?;
    write_svg(output, &svg)
}

fn render_source(source: &str) -> Result<String, HelperError> {
    if source.contains("%%{") {
        return Err(HelperError::UnsupportedDirective);
    }
    let mut options = RenderOptions::default();
    options.layout.fast_text_metrics = true;
    let svg = render_strict(source, options).map_err(|_| HelperError::InvalidDiagram)?;
    if svg.len() > MAX_RAW_SVG_BYTES {
        return Err(HelperError::OutputTooLarge);
    }
    Ok(svg)
}

fn write_svg(output: &Path, svg: &str) -> Result<(), HelperError> {
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(HelperError::CreateOutput)?;
    destination
        .write_all(svg.as_bytes())
        .map_err(HelperError::WriteOutput)?;
    destination.sync_all().map_err(HelperError::WriteOutput)
}

fn read_source(path: &Path) -> Result<String, HelperError> {
    let mut file = File::open(path).map_err(HelperError::OpenInput)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(HelperError::ReadInput)?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(HelperError::InputTooLarge);
    }
    String::from_utf8(bytes).map_err(|_| HelperError::InvalidUtf8)
}

fn install_resource_limits() -> Result<(), HelperError> {
    for (resource, value) in [
        (Resource::Core, 0),
        (Resource::Fsize, MAX_RAW_SVG_BYTES as u64),
        (Resource::As, MAX_ADDRESS_SPACE_BYTES),
        (Resource::Stack, MAX_STACK_BYTES),
        (Resource::Cpu, MAX_CPU_SECONDS),
    ] {
        set_limit(resource, value)?;
    }
    Ok(())
}

fn validate_environment() -> Result<(), HelperError> {
    let environment = env::var("MAINCOPY_MERMAID_ENVIRONMENT").ok();
    let fontconfig = env::var("FONTCONFIG_FILE").ok();
    let cache = env::var("XDG_CACHE_HOME").ok();
    validate_environment_values(
        environment.as_deref(),
        fontconfig.as_deref(),
        cache.as_deref(),
    )
}

fn validate_environment_values(
    environment: Option<&str>,
    fontconfig: Option<&str>,
    cache: Option<&str>,
) -> Result<(), HelperError> {
    if environment != Some(DETERMINISTIC_ENVIRONMENT) || fontconfig.is_none() || cache.is_none() {
        return Err(HelperError::DeterministicEnvironmentMissing);
    }
    Ok(())
}

fn set_limit(resource: Resource, value: u64) -> Result<(), HelperError> {
    let inherited = getrlimit(resource);
    let value = inherited
        .current
        .map_or(value, |current| current.min(value));
    let value = inherited
        .maximum
        .map_or(value, |maximum| maximum.min(value));
    setrlimit(
        resource,
        Rlimit {
            current: Some(value),
            maximum: Some(value),
        },
    )
    .map_err(|source| HelperError::ResourceLimit {
        resource,
        source: std::io::Error::from_raw_os_error(source.raw_os_error()),
    })
}

#[derive(Debug, Error)]
enum HelperError {
    #[error(transparent)]
    Invocation(#[from] crate::cli::InvocationError),
    #[error("could not install the {resource:?} resource limit")]
    ResourceLimit {
        resource: Resource,
        #[source]
        source: std::io::Error,
    },
    #[error("could not open the input")]
    OpenInput(#[source] std::io::Error),
    #[error("could not read the input")]
    ReadInput(#[source] std::io::Error),
    #[error("input exceeds the 256 KiB limit")]
    InputTooLarge,
    #[error("input is not UTF-8")]
    InvalidUtf8,
    #[error("the deterministic renderer environment is missing")]
    DeterministicEnvironmentMissing,
    #[error("Mermaid initialization directives are not supported")]
    UnsupportedDirective,
    #[error("diagram syntax is invalid or unsupported")]
    InvalidDiagram,
    #[error("renderer output exceeds the 2 MiB limit")]
    OutputTooLarge,
    #[error("could not create the output")]
    CreateOutput(#[source] std::io::Error),
    #[error("could not write the output")]
    WriteOutput(#[source] std::io::Error),
}

impl HelperError {
    const fn exit(&self) -> HelperExit {
        match self {
            Self::Invocation(_) | Self::DeterministicEnvironmentMissing => HelperExit::Usage,
            Self::InvalidUtf8 | Self::UnsupportedDirective | Self::InvalidDiagram => {
                HelperExit::InvalidDiagram
            }
            Self::InputTooLarge => HelperExit::InputRejected,
            Self::CreateOutput(_) => HelperExit::CannotCreate,
            Self::OpenInput(_) | Self::ReadInput(_) | Self::WriteOutput(_) => HelperExit::Io,
            Self::OutputTooLarge => HelperExit::ResourceLimit,
            Self::ResourceLimit { .. } => HelperExit::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn representative_core_diagrams_render_with_fixed_options() {
        for source in [
            "flowchart LR\n    A[Source] --> B[Preview]\n",
            "sequenceDiagram\n    Author->>Maincopy: Preview\n    Maincopy-->>Author: Page\n",
            "stateDiagram-v2\n    [*] --> Draft\n    Draft --> Published\n",
            "classDiagram\n    Publication *-- Post\n",
        ] {
            let first = render_source(source).unwrap();
            let second = render_source(source).unwrap();
            assert_eq!(first, second);
            assert!(first.starts_with("<svg "));
            assert!(first.ends_with("</svg>"));
        }
    }

    #[test]
    fn strict_renderer_rejects_invalid_and_unknown_diagrams() {
        for source in ["", "notMermaid\nA --> B\n", "flowchart LR\nA -->\n"] {
            assert!(matches!(
                render_source(source),
                Err(HelperError::InvalidDiagram)
            ));
        }
    }

    #[test]
    fn fixed_renderer_rejects_initialization_directives() {
        let error =
            render_source("%%{init: { 'theme': 'dark' }}%%\nflowchart LR\nA-->B\n").unwrap_err();

        assert!(matches!(error, HelperError::UnsupportedDirective));
    }

    #[test]
    fn environment_contract_rejects_missing_or_mismatched_values() {
        assert!(
            validate_environment_values(
                Some(DETERMINISTIC_ENVIRONMENT),
                Some("/tmp/fonts.conf"),
                Some("/tmp/cache"),
            )
            .is_ok()
        );

        for (environment, fontconfig, cache) in [
            (Some("wrong-version"), Some("fonts"), Some("cache")),
            (None, Some("fonts"), Some("cache")),
            (Some(DETERMINISTIC_ENVIRONMENT), None, Some("cache")),
            (Some(DETERMINISTIC_ENVIRONMENT), Some("fonts"), None),
        ] {
            assert!(matches!(
                validate_environment_values(environment, fontconfig, cache),
                Err(HelperError::DeterministicEnvironmentMissing)
            ));
        }
    }

    #[cfg(feature = "client")]
    #[test]
    fn source_reader_enforces_byte_and_utf8_boundaries() {
        let workspace = tempfile::tempdir().unwrap();
        let exact = workspace.path().join("exact.mmd");
        let oversized = workspace.path().join("oversized.mmd");
        let invalid_utf8 = workspace.path().join("invalid-utf8.mmd");
        let missing = workspace.path().join("missing.mmd");
        std::fs::write(&exact, vec![b'x'; MAX_SOURCE_BYTES]).unwrap();
        std::fs::write(&oversized, vec![b'x'; MAX_SOURCE_BYTES + 1]).unwrap();
        std::fs::write(&invalid_utf8, [0xff]).unwrap();

        assert_eq!(read_source(&exact).unwrap().len(), MAX_SOURCE_BYTES);
        assert!(matches!(
            read_source(&oversized),
            Err(HelperError::InputTooLarge)
        ));
        assert!(matches!(
            read_source(&invalid_utf8),
            Err(HelperError::InvalidUtf8)
        ));
        assert!(matches!(
            read_source(&missing),
            Err(HelperError::OpenInput(_))
        ));
        assert!(matches!(
            read_source(workspace.path()),
            Err(HelperError::ReadInput(_))
        ));
    }

    #[cfg(feature = "client")]
    #[test]
    fn prepared_render_writes_svg_without_replacing_an_existing_output() {
        let workspace = tempfile::tempdir().unwrap();
        let input = workspace.path().join("source.mmd");
        let output = workspace.path().join("diagram.svg");
        std::fs::write(&input, "flowchart LR\nA[Draft] --> B[Published]\n").unwrap();

        render_prepared_file(&input, &output).unwrap();
        let svg = std::fs::read_to_string(&output).unwrap();
        assert!(svg.starts_with("<svg "));
        assert!(svg.ends_with("</svg>"));

        let error = render_prepared_file(&input, &output).unwrap_err();
        assert!(matches!(error, HelperError::CreateOutput(_)));
        assert_eq!(std::fs::read_to_string(output).unwrap(), svg);
    }
}
