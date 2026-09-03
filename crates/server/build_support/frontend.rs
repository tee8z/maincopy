use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fmt, io,
    path::{Component, Path, PathBuf},
};

use crate::frontend_digest_contract::{
    FRONTEND_ASSET_PREFIX, FRONTEND_BUNDLE_PREFIX, FrontendAssetKind, FrontendAssetName,
    FrontendDigestInput, frontend_asset_digest, frontend_bundle_digest,
};
use crate::frontend_io::{
    ConfinedDirectory, ConfinedDirectoryIdentity, ConfinedEntry, ConfinedEntryKind,
    ConfinedFileIdentity,
};
use lightningcss::{
    dependencies::DependencyOptions,
    stylesheet::{MinifyOptions, ParserOptions, PrinterOptions, StyleSheet},
};

const FRONTEND_ROOT: &str = "frontend";
const CSS_ROOT: &str = "frontend/css";
const JAVASCRIPT_ROOT: &str = "frontend/js";
const REQUIRED_CSS_INPUT: &str = "site.css";
const JAVASCRIPT_INPUT: &str = "site.js";
const OUTPUT_ROOT: &str = "maincopy-frontend";
const CSS_OUTPUT: &str = "site.css";
const JAVASCRIPT_OUTPUT: &str = "site.js";
const GENERATED_MANIFEST: &str = "frontend_manifest.rs";
const MAX_INPUT_ENTRIES: usize = 256;
const MAX_INPUT_DEPTH: usize = 16;
const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_BUNDLE_BYTES: usize = 8 * 1024 * 1024;
const MAX_LOGICAL_PATH_BYTES: usize = 1_024;
const MAX_SEGMENT_BYTES: usize = 255;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FrontendBuildOperation {
    Inspect,
    Open,
    Enumerate,
    Read,
    CreateOutputDirectory,
    Write,
    Sync,
    Rename,
}

impl fmt::Display for FrontendBuildOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Inspect => "inspect",
            Self::Open => "open",
            Self::Enumerate => "enumerate",
            Self::Read => "read",
            Self::CreateOutputDirectory => "create output directory",
            Self::Write => "write",
            Self::Sync => "synchronize",
            Self::Rename => "atomically replace",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FrontendBuildError {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[error("the frontend build requires descriptor-relative I/O on Linux or macOS")]
    UnsupportedHost,
    #[error("frontend input root is missing: {}", .path.display())]
    MissingInputRoot { path: PathBuf },
    #[error("frontend input root is not a directory: {}", .path.display())]
    InputRootNotDirectory { path: PathBuf },
    #[error("frontend input escaped its declared root: {}", .path.display())]
    PathEscape { path: PathBuf },
    #[error("frontend inputs cannot contain symlinks: {}", .path.display())]
    Symlink { path: PathBuf },
    #[error(
        "frontend inputs must be regular files or directories: {}",
        .path.display()
    )]
    SpecialFile { path: PathBuf },
    #[error("frontend input path is not portable ASCII: {}", .path.display())]
    NonPortablePath { path: PathBuf },
    #[error("CSS input root contains a non-CSS file: {}", .path.display())]
    UnexpectedInputType { path: PathBuf },
    #[error("frontend input path is duplicated: {}", .path.display())]
    DuplicatePath { path: PathBuf },
    #[error(
        "frontend input paths collide by ASCII case: {} and {}",
        .first.display(),
        .second.display()
    )]
    CaseCollision { first: PathBuf, second: PathBuf },
    #[error("frontend input tree contains {entries} entries; limit is {limit}")]
    InputEntryLimit { entries: usize, limit: usize },
    #[error(
        "frontend input {} has depth {depth}; limit is {limit}",
        .path.display()
    )]
    InputDepthLimit {
        path: PathBuf,
        depth: usize,
        limit: usize,
    },
    #[error("frontend/css/site.css is required")]
    MissingStylesheet,
    #[error("frontend/js must contain exactly one site.js regular file")]
    InvalidJavascriptInput,
    #[error(
        "frontend input {} contains {bytes} bytes; limit is {limit}",
        .path.display()
    )]
    InputTooLarge {
        path: PathBuf,
        bytes: u64,
        limit: usize,
    },
    #[error("combined frontend input contains {bytes} bytes; limit is {limit}")]
    BundleTooLarge { bytes: usize, limit: usize },
    #[error("frontend input is not valid UTF-8: {}", .path.display())]
    InvalidUtf8 { path: PathBuf },
    #[error("cannot parse frontend CSS: {message}")]
    CssParse { message: Box<str> },
    #[error("cannot minify frontend CSS: {message}")]
    CssMinify { message: Box<str> },
    #[error("cannot serialize frontend CSS: {message}")]
    CssPrint { message: Box<str> },
    #[error("frontend CSS contains {count} unsupported import or URL dependencies")]
    CssDependency { count: usize },
    #[error("minified frontend stylesheet is empty")]
    EmptyStylesheet,
    #[error("frontend digest contract failed: {message}")]
    DigestContract { message: Box<str> },
    #[error(
        "frontend output path is a symlink or special file: {}",
        .path.display()
    )]
    UnsafeOutputPath { path: PathBuf },
    #[error("frontend input changed while the build held it: {}", .path.display())]
    InputChanged { path: PathBuf },
    #[error(
        "frontend output changed before its atomic replacement: {}",
        .path.display()
    )]
    OutputChanged { path: PathBuf },
    #[error(
        "frontend output {} has {links} hard links; exactly one is required",
        .path.display()
    )]
    OutputHardlink { path: PathBuf, links: u64 },
    #[error(
        "cannot reserve a temporary output for {} after {attempts} attempts",
        .path.display()
    )]
    TemporaryOutputExhausted { path: PathBuf, attempts: u64 },
    #[error("cannot {operation} frontend path {}: {source}", .path.display())]
    Io {
        operation: FrontendBuildOperation,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

struct InputFile {
    logical_path: String,
    disk_path: PathBuf,
    bytes: Vec<u8>,
    identity: ConfinedFileIdentity,
}

struct DiscoveredCss {
    inputs: Vec<InputFile>,
    directories: Vec<ConfinedDirectoryIdentity>,
}

struct DiscoveredJavascript {
    input: InputFile,
    directory: ConfinedDirectoryIdentity,
}

struct PreparedFrontend {
    css: DiscoveredCss,
    javascript: Option<DiscoveredJavascript>,
    stylesheet: Vec<u8>,
    manifest: String,
}

struct DiscoveryState {
    inputs: Vec<InputFile>,
    directories: Vec<ConfinedDirectoryIdentity>,
    exact_paths: BTreeSet<String>,
    case_paths: BTreeMap<String, PathBuf>,
    entry_count: usize,
    parser_stream_bytes: usize,
}

impl DiscoveryState {
    fn new(root: &ConfinedDirectory) -> Self {
        Self {
            inputs: Vec::new(),
            directories: vec![root.identity()],
            exact_paths: BTreeSet::new(),
            case_paths: BTreeMap::new(),
            entry_count: 0,
            parser_stream_bytes: 0,
        }
    }
}

impl DiscoveredCss {
    fn verify_unchanged(&self) -> Result<(), FrontendBuildError> {
        for directory in &self.directories {
            directory.verify_unchanged()?;
        }
        for input in &self.inputs {
            input.identity.verify_unchanged()?;
        }
        Ok(())
    }
}

impl DiscoveredJavascript {
    fn verify_unchanged(&self) -> Result<(), FrontendBuildError> {
        self.directory.verify_unchanged()?;
        self.input.identity.verify_unchanged()
    }
}

pub(crate) fn compile_frontend(
    manifest_dir: &Path,
    out_dir: &Path,
) -> Result<Vec<PathBuf>, FrontendBuildError> {
    PreparedFrontend::compile(manifest_dir)?.write(out_dir)
}

impl PreparedFrontend {
    fn compile(manifest_dir: &Path) -> Result<Self, FrontendBuildError> {
        let (mut css, javascript) = discover_frontend(manifest_dir)?;
        let combined = read_and_combine(&mut css.inputs)?;
        let stylesheet = minify_css(&combined)?;
        if stylesheet.is_empty() {
            return Err(FrontendBuildError::EmptyStylesheet);
        }
        let javascript_bytes = javascript
            .as_ref()
            .map(|javascript| javascript.input.bytes.as_slice());
        enforce_bundle_size(combined_bundle_size(stylesheet.len(), javascript_bytes)?)?;
        let manifest = prepare_manifest(&stylesheet, javascript_bytes)?;
        Ok(Self {
            css,
            javascript,
            stylesheet,
            manifest,
        })
    }

    fn write(self, out_dir: &Path) -> Result<Vec<PathBuf>, FrontendBuildError> {
        let output_directory = ConfinedDirectory::open_absolute(out_dir)?;
        let output_root = out_dir.join(OUTPUT_ROOT);
        let bundle_directory =
            output_directory.ensure_output_directory(OsStr::new(OUTPUT_ROOT), &output_root)?;
        bundle_directory
            .write_atomic_with_hook(CSS_OUTPUT, &self.stylesheet, || self.css.verify_unchanged())?;
        if let Some(javascript) = &self.javascript {
            bundle_directory.write_atomic_with_hook(
                JAVASCRIPT_OUTPUT,
                &javascript.input.bytes,
                || {
                    self.css.verify_unchanged()?;
                    javascript.verify_unchanged()
                },
            )?;
        }
        output_directory.write_atomic_with_hook(
            GENERATED_MANIFEST,
            self.manifest.as_bytes(),
            || {
                self.css.verify_unchanged()?;
                if let Some(javascript) = &self.javascript {
                    javascript.verify_unchanged()?;
                }
                Ok(())
            },
        )?;
        Ok(self.input_paths())
    }

    fn input_paths(self) -> Vec<PathBuf> {
        let mut inputs = self
            .css
            .inputs
            .into_iter()
            .map(|input| {
                Path::new(FRONTEND_ROOT)
                    .join("css")
                    .join(input.logical_path)
            })
            .collect::<Vec<_>>();
        if let Some(javascript) = self.javascript {
            inputs.push(
                Path::new(FRONTEND_ROOT)
                    .join("js")
                    .join(javascript.input.logical_path),
            );
        }
        inputs
    }
}

fn discover_frontend(
    manifest_dir: &Path,
) -> Result<(DiscoveredCss, Option<DiscoveredJavascript>), FrontendBuildError> {
    let manifest_root = ConfinedDirectory::open_absolute(manifest_dir)?;
    let frontend_path = manifest_dir.join(FRONTEND_ROOT);
    let frontend_root =
        manifest_root.open_required_directory(OsStr::new(FRONTEND_ROOT), &frontend_path)?;
    let css_root = manifest_dir.join(CSS_ROOT);
    let css_directory = frontend_root.open_required_directory(OsStr::new("css"), &css_root)?;
    let javascript_root = manifest_dir.join(JAVASCRIPT_ROOT);
    let javascript_directory =
        match frontend_root.open_required_directory(OsStr::new("js"), &javascript_root) {
            Ok(directory) => Some(directory),
            Err(FrontendBuildError::MissingInputRoot { .. }) => None,
            Err(error) => return Err(error),
        };
    drop(frontend_root);
    drop(manifest_root);
    let discovered = discover_css(&css_root, &css_directory)?;
    let javascript = javascript_directory
        .as_ref()
        .map(|directory| discover_javascript(&javascript_root, directory))
        .transpose()?;
    Ok((discovered, javascript))
}

fn combined_bundle_size(
    stylesheet_bytes: usize,
    javascript: Option<&[u8]>,
) -> Result<usize, FrontendBuildError> {
    match javascript {
        Some(javascript) => stylesheet_bytes.checked_add(javascript.len()).ok_or(
            FrontendBuildError::BundleTooLarge {
                bytes: usize::MAX,
                limit: MAX_BUNDLE_BYTES,
            },
        ),
        None => Ok(stylesheet_bytes),
    }
}

fn prepare_manifest(
    stylesheet: &[u8],
    javascript: Option<&[u8]>,
) -> Result<String, FrontendBuildError> {
    let css_digest = frontend_asset_digest(FrontendAssetKind::Css, stylesheet);
    let javascript_digest =
        javascript.map(|bytes| frontend_asset_digest(FrontendAssetKind::JavaScript, bytes));
    let bundle_digest = calculate_bundle_digest(stylesheet, javascript)?;
    let bundle_name = encoded_digest(FRONTEND_BUNDLE_PREFIX, &bundle_digest);
    let css_public_path = format!(
        "/app-assets/{bundle_name}/{}",
        FrontendAssetName::Stylesheet
    );
    let javascript_public_path = javascript_digest.map(|_| {
        format!(
            "/app-assets/{bundle_name}/{}",
            FrontendAssetName::JavaScript
        )
    });
    Ok(generated_manifest(
        &bundle_digest,
        &css_digest,
        &css_public_path,
        javascript_digest
            .as_ref()
            .zip(javascript_public_path.as_deref()),
    ))
}

fn calculate_bundle_digest(
    css: &[u8],
    javascript: Option<&[u8]>,
) -> Result<[u8; 32], FrontendBuildError> {
    let mut inputs = vec![FrontendDigestInput {
        kind: FrontendAssetKind::Css,
        bytes: css,
    }];
    if let Some(javascript) = javascript {
        inputs.push(FrontendDigestInput {
            kind: FrontendAssetKind::JavaScript,
            bytes: javascript,
        });
    }
    frontend_bundle_digest(&inputs).map_err(|error| FrontendBuildError::DigestContract {
        message: error.to_string().into_boxed_str(),
    })
}

fn discover_css(
    root: &Path,
    directory: &ConfinedDirectory,
) -> Result<DiscoveredCss, FrontendBuildError> {
    let mut state = DiscoveryState::new(directory);
    let identity = directory.identity();
    discover_css_directory(root, directory, &identity, Path::new(""), &mut state)?;
    state.inputs.sort_by(|left, right| {
        left.logical_path
            .as_bytes()
            .cmp(right.logical_path.as_bytes())
    });
    if !state
        .inputs
        .iter()
        .any(|input| input.logical_path == REQUIRED_CSS_INPUT)
    {
        return Err(FrontendBuildError::MissingStylesheet);
    }
    Ok(DiscoveredCss {
        inputs: state.inputs,
        directories: state.directories,
    })
}

fn discover_javascript(
    root: &Path,
    directory: &ConfinedDirectory,
) -> Result<DiscoveredJavascript, FrontendBuildError> {
    let mut entries = directory.entries(0, MAX_INPUT_ENTRIES)?;
    if entries.len() != 1 {
        return Err(FrontendBuildError::InvalidJavascriptInput);
    }
    let source = entries
        .pop()
        .ok_or(FrontendBuildError::InvalidJavascriptInput)?;
    if source.name() != OsStr::new(JAVASCRIPT_INPUT) || source.kind() != ConfinedEntryKind::File {
        return Err(FrontendBuildError::InvalidJavascriptInput);
    }

    let path = root.join(JAVASCRIPT_INPUT);
    let identity = directory.identity();
    let file_identity = identity.file(&source, &path)?;
    let mut confined = directory.open_entry_file(&source, &path)?;
    let captured_bytes = confined.byte_length()?;
    enforce_input_size(&path, captured_bytes)?;
    let bytes = confined.read_verified(MAX_INPUT_BYTES + 1)?;
    enforce_input_size(&path, bytes.len() as u64)?;
    if bytes.is_empty() || std::str::from_utf8(&bytes).is_err() {
        return Err(FrontendBuildError::InvalidJavascriptInput);
    }
    directory.verify_unchanged()?;

    Ok(DiscoveredJavascript {
        input: InputFile {
            logical_path: JAVASCRIPT_INPUT.to_owned(),
            disk_path: path,
            bytes,
            identity: file_identity,
        },
        directory: identity,
    })
}

fn discover_css_directory(
    root: &Path,
    directory: &ConfinedDirectory,
    identity: &ConfinedDirectoryIdentity,
    parent: &Path,
    state: &mut DiscoveryState,
) -> Result<(), FrontendBuildError> {
    let entries = directory.entries(state.entry_count, MAX_INPUT_ENTRIES)?;
    state.entry_count = state.entry_count.saturating_add(entries.len());
    enforce_entry_count(state.entry_count)?;
    for entry in entries {
        let relative = parent.join(entry.name());
        let path = root.join(&relative);
        enforce_depth(&path, relative.components().count())?;
        match entry.kind() {
            ConfinedEntryKind::File | ConfinedEntryKind::Directory => {}
            ConfinedEntryKind::Symlink => {
                return Err(FrontendBuildError::Symlink { path });
            }
            ConfinedEntryKind::Special => {
                return Err(FrontendBuildError::SpecialFile { path });
            }
        }

        let logical_path = normalize_relative_path(&relative, &path)?;
        if !state.exact_paths.insert(logical_path.clone()) {
            return Err(FrontendBuildError::DuplicatePath { path });
        }
        let folded = logical_path.to_ascii_lowercase();
        if let Some(first) = state.case_paths.insert(folded, path.clone()) {
            return Err(FrontendBuildError::CaseCollision {
                first,
                second: path,
            });
        }
        if entry.kind() == ConfinedEntryKind::Directory {
            let child_identity = identity.child(&entry, &path)?;
            let child = directory.open_entry_directory(&entry, &path)?;
            state.directories.push(child_identity.clone());
            discover_css_directory(root, &child, &child_identity, &relative, state)?;
            child.verify_unchanged()?;
            continue;
        }
        collect_css_file(directory, identity, entry, logical_path, path, state)?;
    }
    directory.verify_unchanged()
}

fn collect_css_file(
    directory: &ConfinedDirectory,
    directory_identity: &ConfinedDirectoryIdentity,
    source: ConfinedEntry,
    logical_path: String,
    disk_path: PathBuf,
    state: &mut DiscoveryState,
) -> Result<(), FrontendBuildError> {
    if disk_path
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("css")
    {
        return Err(FrontendBuildError::UnexpectedInputType { path: disk_path });
    }
    let identity = directory_identity.file(&source, &disk_path)?;
    let mut confined = directory.open_entry_file(&source, &disk_path)?;
    let captured_bytes = confined.byte_length()?;
    enforce_input_size(&disk_path, captured_bytes)?;
    let captured_bytes =
        usize::try_from(captured_bytes).map_err(|_| FrontendBuildError::InputTooLarge {
            path: disk_path.clone(),
            bytes: captured_bytes,
            limit: MAX_INPUT_BYTES,
        })?;
    parser_stream_size(state.parser_stream_bytes, captured_bytes)?;

    let bytes = confined.read_verified(MAX_INPUT_BYTES + 1)?;
    enforce_input_size(&disk_path, bytes.len() as u64)?;
    state.parser_stream_bytes = parser_stream_size(state.parser_stream_bytes, bytes.len())?;
    state.inputs.push(InputFile {
        logical_path,
        disk_path,
        bytes,
        identity,
    });
    Ok(())
}

fn parser_stream_size(current: usize, input: usize) -> Result<usize, FrontendBuildError> {
    let bytes = current
        .checked_add(input)
        .and_then(|value| value.checked_add(1))
        .ok_or(FrontendBuildError::BundleTooLarge {
            bytes: usize::MAX,
            limit: MAX_BUNDLE_BYTES,
        })?;
    enforce_bundle_size(bytes)?;
    Ok(bytes)
}

fn normalize_relative_path(relative: &Path, full: &Path) -> Result<String, FrontendBuildError> {
    let mut logical = String::new();
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            return Err(FrontendBuildError::NonPortablePath {
                path: full.to_owned(),
            });
        };
        let Some(segment) = segment.to_str() else {
            return Err(FrontendBuildError::NonPortablePath {
                path: full.to_owned(),
            });
        };
        if !portable_segment(segment) {
            return Err(FrontendBuildError::NonPortablePath {
                path: full.to_owned(),
            });
        }
        if !logical.is_empty() {
            logical.push('/');
        }
        logical.push_str(segment);
    }
    if logical.is_empty() || logical.len() > MAX_LOGICAL_PATH_BYTES {
        return Err(FrontendBuildError::NonPortablePath {
            path: full.to_owned(),
        });
    }
    Ok(logical)
}

fn portable_segment(segment: &str) -> bool {
    if segment.is_empty()
        || segment.len() > MAX_SEGMENT_BYTES
        || segment.starts_with('.')
        || segment.ends_with('.')
        || !segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return false;
    }

    let stem = segment.split('.').next().unwrap_or(segment);
    let folded = stem.to_ascii_uppercase();
    !matches!(folded.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !(folded.len() == 4
            && (folded.starts_with("COM") || folded.starts_with("LPT"))
            && folded.as_bytes()[3].is_ascii_digit()
            && folded.as_bytes()[3] != b'0')
}

fn read_and_combine(inputs: &mut [InputFile]) -> Result<String, FrontendBuildError> {
    read_and_combine_with_hook(inputs, |_| {})
}

fn read_and_combine_with_hook<Hook>(
    inputs: &mut [InputFile],
    mut after_input: Hook,
) -> Result<String, FrontendBuildError>
where
    Hook: FnMut(usize),
{
    let mut combined = String::new();
    let mut total = 0_usize;
    for (index, input) in inputs.iter_mut().enumerate() {
        total = total
            .checked_add(input.bytes.len())
            .and_then(|value| value.checked_add(1))
            .ok_or(FrontendBuildError::BundleTooLarge {
                bytes: usize::MAX,
                limit: MAX_BUNDLE_BYTES,
            })?;
        enforce_bundle_size(total)?;
        let source =
            std::str::from_utf8(&input.bytes).map_err(|_| FrontendBuildError::InvalidUtf8 {
                path: input.disk_path.clone(),
            })?;
        combined.push_str(source);
        combined.push('\n');
        after_input(index);
    }
    Ok(combined)
}

fn enforce_entry_count(entries: usize) -> Result<(), FrontendBuildError> {
    if entries > MAX_INPUT_ENTRIES {
        return Err(FrontendBuildError::InputEntryLimit {
            entries,
            limit: MAX_INPUT_ENTRIES,
        });
    }
    Ok(())
}

fn enforce_depth(path: &Path, depth: usize) -> Result<(), FrontendBuildError> {
    if depth > MAX_INPUT_DEPTH {
        return Err(FrontendBuildError::InputDepthLimit {
            path: path.to_owned(),
            depth,
            limit: MAX_INPUT_DEPTH,
        });
    }
    Ok(())
}

fn enforce_input_size(path: &Path, bytes: u64) -> Result<(), FrontendBuildError> {
    if bytes > MAX_INPUT_BYTES as u64 {
        return Err(FrontendBuildError::InputTooLarge {
            path: path.to_owned(),
            bytes,
            limit: MAX_INPUT_BYTES,
        });
    }
    Ok(())
}

fn enforce_bundle_size(bytes: usize) -> Result<(), FrontendBuildError> {
    if bytes > MAX_BUNDLE_BYTES {
        return Err(FrontendBuildError::BundleTooLarge {
            bytes,
            limit: MAX_BUNDLE_BYTES,
        });
    }
    Ok(())
}

fn minify_css(source: &str) -> Result<Vec<u8>, FrontendBuildError> {
    let mut stylesheet = StyleSheet::parse(
        source,
        ParserOptions {
            filename: "maincopy-frontend.css".to_owned(),
            error_recovery: false,
            ..ParserOptions::default()
        },
    )
    .map_err(|error| FrontendBuildError::CssParse {
        message: error.to_string().into_boxed_str(),
    })?;
    stylesheet
        .minify(MinifyOptions::default())
        .map_err(|error| FrontendBuildError::CssMinify {
            message: error.to_string().into_boxed_str(),
        })?;
    let dependency_scan = stylesheet
        .to_css(PrinterOptions {
            minify: true,
            analyze_dependencies: Some(DependencyOptions::default()),
            ..PrinterOptions::default()
        })
        .map_err(|error| FrontendBuildError::CssPrint {
            message: error.to_string().into_boxed_str(),
        })?;
    let dependency_count = dependency_scan.dependencies.map_or(0, |items| items.len());
    if dependency_count != 0 {
        return Err(FrontendBuildError::CssDependency {
            count: dependency_count,
        });
    }

    let output = stylesheet
        .to_css(PrinterOptions {
            minify: true,
            ..PrinterOptions::default()
        })
        .map_err(|error| FrontendBuildError::CssPrint {
            message: error.to_string().into_boxed_str(),
        })?;
    Ok(output.code.into_bytes())
}

fn generated_manifest(
    bundle_digest: &[u8; 32],
    css_digest: &[u8; 32],
    css_public_path: &str,
    javascript: Option<(&[u8; 32], &str)>,
) -> String {
    let css_etag = encoded_digest(FRONTEND_ASSET_PREFIX, css_digest);
    let javascript = javascript.map_or_else(
        || "None".to_owned(),
        |(digest, public_path)| {
            format!(
                "Some(FrontendAsset {{\n\
                 \x20           kind: FrontendAssetKind::JavaScript,\n\
                 \x20           digest: FrontendAssetDigest::from_generated({}),\n\
                 \x20           public_path: {public_path:?},\n\
                 \x20           bytes: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/maincopy-frontend/site.js\")),\n\
                 \x20       }})",
                byte_array(digest),
            )
        },
    );
    format!(
        "// @generated by build.rs; do not edit.\n\
         // CSS ETag: \"{css_etag}\"\n\
         pub(super) const GENERATED_FRONTEND_MANIFEST: FrontendAssetManifest =\n\
         \x20   FrontendAssetManifest {{\n\
         \x20       bundle_digest: FrontendBundleDigest::from_generated({}),\n\
         \x20       css: FrontendAsset {{\n\
         \x20           kind: FrontendAssetKind::Css,\n\
         \x20           digest: FrontendAssetDigest::from_generated({}),\n\
         \x20           public_path: {css_public_path:?},\n\
         \x20           bytes: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/maincopy-frontend/site.css\")),\n\
         \x20       }},\n\
         \x20       javascript: {javascript},\n\
         \x20   }};\n",
        byte_array(bundle_digest),
        byte_array(css_digest),
    )
}

fn encoded_digest(prefix: &str, digest: &[u8; 32]) -> String {
    const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(prefix.len() + digest.len() * 2);
    encoded.push_str(prefix);
    for byte in digest {
        encoded.push(char::from(LOWER_HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(LOWER_HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn byte_array(bytes: &[u8; 32]) -> String {
    let encoded = bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{encoded}]")
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::frontend_io::ConfinedInput;
    use std::{fs, io::Write as _};
    use tempfile::TempDir;

    fn fixture() -> (TempDir, PathBuf, PathBuf) {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let source = root.join("source");
        let css = source.join(CSS_ROOT);
        let output = root.join("output");
        fs::create_dir_all(&css).unwrap();
        fs::create_dir_all(&output).unwrap();
        (temp, source, output)
    }

    fn discover_fixture(source: &Path) -> Result<DiscoveredCss, FrontendBuildError> {
        let manifest = ConfinedDirectory::open_absolute(source)?;
        let frontend_path = source.join(FRONTEND_ROOT);
        let frontend =
            manifest.open_required_directory(OsStr::new(FRONTEND_ROOT), &frontend_path)?;
        let css_path = source.join(CSS_ROOT);
        let css = frontend.open_required_directory(OsStr::new("css"), &css_path)?;
        discover_css(&css_path, &css)
    }

    fn open_input_fixture(source: &Path, leaf: &str) -> ConfinedInput {
        let manifest = ConfinedDirectory::open_absolute(source).unwrap();
        let frontend_path = source.join(FRONTEND_ROOT);
        let frontend = manifest
            .open_required_directory(OsStr::new(FRONTEND_ROOT), &frontend_path)
            .unwrap();
        let css_path = source.join(CSS_ROOT);
        let css = frontend
            .open_required_directory(OsStr::new("css"), &css_path)
            .unwrap();
        let entry = css
            .entries(0, MAX_INPUT_ENTRIES)
            .unwrap()
            .into_iter()
            .find(|entry| entry.name() == OsStr::new(leaf))
            .unwrap();
        css.open_entry_file(&entry, &css_path.join(leaf)).unwrap()
    }

    #[test]
    fn input_discovery_order_does_not_change_outputs() {
        let (_first_temp, first_source, first_output) = fixture();
        fs::create_dir_all(first_source.join(CSS_ROOT).join("components")).unwrap();
        fs::write(
            first_source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            "body { margin: 0; }",
        )
        .unwrap();
        fs::write(
            first_source.join(CSS_ROOT).join("z.css"),
            ".z { color: blue; }",
        )
        .unwrap();
        fs::write(
            first_source.join(CSS_ROOT).join("components/a.css"),
            ".a { color: red; }",
        )
        .unwrap();

        let (_second_temp, second_source, second_output) = fixture();
        fs::create_dir_all(second_source.join(CSS_ROOT).join("components")).unwrap();
        fs::write(
            second_source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            "body { margin: 0; }",
        )
        .unwrap();
        fs::write(
            second_source.join(CSS_ROOT).join("components/a.css"),
            ".a { color: red; }",
        )
        .unwrap();
        fs::write(
            second_source.join(CSS_ROOT).join("z.css"),
            ".z { color: blue; }",
        )
        .unwrap();

        let first = compile_frontend(&first_source, &first_output).unwrap();
        let second = compile_frontend(&second_source, &second_output).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            fs::read(first_output.join(OUTPUT_ROOT).join(CSS_OUTPUT)).unwrap(),
            fs::read(second_output.join(OUTPUT_ROOT).join(CSS_OUTPUT)).unwrap()
        );
        assert_eq!(
            fs::read(first_output.join(GENERATED_MANIFEST)).unwrap(),
            fs::read(second_output.join(GENERATED_MANIFEST)).unwrap()
        );
    }

    #[test]
    fn optional_javascript_is_confined_embedded_and_bundle_bound() {
        let (_temp, source, output) = fixture();
        fs::write(
            source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            "body { margin: 0; }",
        )
        .unwrap();
        let javascript_root = source.join(JAVASCRIPT_ROOT);
        fs::create_dir_all(&javascript_root).unwrap();
        let javascript = b"document.documentElement.dataset.ready='true';\n";
        fs::write(javascript_root.join(JAVASCRIPT_INPUT), javascript).unwrap();

        let inputs = compile_frontend(&source, &output).unwrap();

        assert!(inputs.contains(&Path::new(JAVASCRIPT_ROOT).join(JAVASCRIPT_INPUT)));
        assert_eq!(
            fs::read(output.join(OUTPUT_ROOT).join(JAVASCRIPT_OUTPUT)).unwrap(),
            javascript
        );
        let generated = fs::read_to_string(output.join(GENERATED_MANIFEST)).unwrap();
        assert!(generated.contains("javascript: Some(FrontendAsset"));
        assert!(generated.contains("kind: FrontendAssetKind::JavaScript"));
        assert!(generated.contains("/site.js"));
    }

    #[test]
    fn javascript_directory_rejects_extra_or_misnamed_inputs() {
        let (_temp, source, output) = fixture();
        fs::write(
            source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            "body { margin: 0; }",
        )
        .unwrap();
        let javascript_root = source.join(JAVASCRIPT_ROOT);
        fs::create_dir_all(&javascript_root).unwrap();
        fs::write(javascript_root.join("other.js"), "void 0;").unwrap();

        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::InvalidJavascriptInput)
        ));
    }

    #[test]
    fn corrupt_css_fails_without_an_unminified_fallback() {
        let (_temp, source, output) = fixture();
        fs::write(source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT), "a {").unwrap();
        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::CssParse { .. }
                | FrontendBuildError::CssMinify { .. }
                | FrontendBuildError::CssPrint { .. }
                | FrontendBuildError::EmptyStylesheet)
        ));
        assert!(!output.join(OUTPUT_ROOT).join(CSS_OUTPUT).exists());
    }

    #[test]
    fn css_cannot_smuggle_unmanaged_imports_or_url_assets() {
        for source_css in [
            "@import url(\"https://example.com/theme.css\");",
            "body { background-image: url(\"/unmanaged.png\"); }",
        ] {
            let (_temp, source, output) = fixture();
            fs::write(source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT), source_css).unwrap();
            assert!(matches!(
                compile_frontend(&source, &output),
                Err(FrontendBuildError::CssDependency { .. })
            ));
            assert!(!output.join(OUTPUT_ROOT).join(CSS_OUTPUT).exists());
        }
    }

    #[test]
    fn missing_css_fails() {
        let (_temp, source, output) = fixture();
        fs::write(source.join(CSS_ROOT).join("other.css"), "a {} ").unwrap();
        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::MissingStylesheet)
        ));
    }

    #[test]
    fn entry_depth_and_byte_limits_are_inclusive() {
        assert!(enforce_entry_count(MAX_INPUT_ENTRIES).is_ok());
        assert!(matches!(
            enforce_entry_count(MAX_INPUT_ENTRIES + 1),
            Err(FrontendBuildError::InputEntryLimit { .. })
        ));

        let path = Path::new("frontend/css/site.css");
        assert!(enforce_depth(path, MAX_INPUT_DEPTH).is_ok());
        assert!(matches!(
            enforce_depth(path, MAX_INPUT_DEPTH + 1),
            Err(FrontendBuildError::InputDepthLimit { .. })
        ));
        assert!(enforce_input_size(path, MAX_INPUT_BYTES as u64).is_ok());
        assert!(matches!(
            enforce_input_size(path, MAX_INPUT_BYTES as u64 + 1),
            Err(FrontendBuildError::InputTooLarge { .. })
        ));
        assert!(enforce_bundle_size(MAX_BUNDLE_BYTES).is_ok());
        assert!(matches!(
            enforce_bundle_size(MAX_BUNDLE_BYTES + 1),
            Err(FrontendBuildError::BundleTooLarge { .. })
        ));
    }

    #[test]
    fn combined_parser_stream_limit_counts_inserted_separators() {
        let (_within_temp, within_source, _within_output) = fixture();
        fs::write(
            within_source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            vec![b' '; MAX_INPUT_BYTES - 1],
        )
        .unwrap();
        fs::write(
            within_source.join(CSS_ROOT).join("extra.css"),
            vec![b' '; MAX_INPUT_BYTES - 1],
        )
        .unwrap();
        let mut within = discover_fixture(&within_source).unwrap();
        assert_eq!(
            read_and_combine(&mut within.inputs).unwrap().len(),
            MAX_BUNDLE_BYTES
        );

        let (_over_temp, over_source, _over_output) = fixture();
        fs::write(
            over_source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            vec![b' '; MAX_INPUT_BYTES],
        )
        .unwrap();
        fs::write(
            over_source.join(CSS_ROOT).join("extra.css"),
            vec![b' '; MAX_INPUT_BYTES - 1],
        )
        .unwrap();
        assert!(matches!(
            discover_fixture(&over_source),
            Err(FrontendBuildError::BundleTooLarge {
                bytes,
                limit: MAX_BUNDLE_BYTES,
            }) if bytes == MAX_BUNDLE_BYTES + 1
        ));
    }

    #[test]
    fn actual_tree_depth_is_bounded_before_file_collection() {
        let (_temp, source, output) = fixture();
        let mut deep = source.join(CSS_ROOT);
        for index in 0..MAX_INPUT_DEPTH {
            deep = deep.join(format!("level{index}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT), "a {} ").unwrap();
        fs::write(deep.join("extra.css"), "b {} ").unwrap();
        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::InputDepthLimit { .. })
        ));
    }

    #[test]
    fn actual_tree_entry_limit_is_inclusive() {
        let (_within_temp, within_source, within_output) = fixture();
        fs::write(
            within_source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            "a { color: red; }",
        )
        .unwrap();
        for index in 0..(MAX_INPUT_ENTRIES - 1) {
            fs::create_dir(within_source.join(CSS_ROOT).join(format!("d{index}"))).unwrap();
        }
        assert!(compile_frontend(&within_source, &within_output).is_ok());

        let (_over_temp, over_source, over_output) = fixture();
        fs::write(
            over_source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            "a { color: red; }",
        )
        .unwrap();
        for index in 0..MAX_INPUT_ENTRIES {
            fs::create_dir(over_source.join(CSS_ROOT).join(format!("d{index}"))).unwrap();
        }
        assert!(matches!(
            compile_frontend(&over_source, &over_output),
            Err(FrontendBuildError::InputEntryLimit { .. })
        ));
    }

    #[test]
    fn traversal_components_are_rejected_by_normalization() {
        assert!(matches!(
            normalize_relative_path(Path::new("../site.css"), Path::new("../site.css")),
            Err(FrontendBuildError::NonPortablePath { .. })
        ));
        assert!(matches!(
            normalize_relative_path(Path::new("nested/../../site.css"), Path::new("site.css")),
            Err(FrontendBuildError::NonPortablePath { .. })
        ));
    }

    #[test]
    fn a_discovered_input_that_disappears_fails_closed() {
        let (_temp, source, _output) = fixture();
        let input = source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT);
        fs::write(&input, "a {} ").unwrap();
        let discovered = discover_fixture(&source).unwrap();
        fs::remove_file(input).unwrap();
        assert!(matches!(
            discovered.verify_unchanged(),
            Err(FrontendBuildError::Io { .. }) | Err(FrontendBuildError::InputChanged { .. })
        ));
    }

    #[test]
    fn a_discovered_input_cannot_be_swapped_after_read() {
        let (_temp, source, _output) = fixture();
        let input = source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT);
        let displaced = source.join("displaced.css");
        fs::write(&input, "a { color: red; }").unwrap();
        let discovered = discover_fixture(&source).unwrap();

        fs::rename(&input, displaced).unwrap();
        fs::write(&input, "a { color: blue; }").unwrap();

        assert!(matches!(
            discovered.verify_unchanged(),
            Err(FrontendBuildError::InputChanged { .. })
        ));
    }

    #[test]
    fn input_growth_during_read_fails_closed() {
        use std::fs::OpenOptions;

        let (_temp, source, _output) = fixture();
        let input = source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT);
        fs::write(&input, format!("/*{}*/a{{color:red}}", "x".repeat(80_000))).unwrap();
        let mut confined = open_input_fixture(&source, REQUIRED_CSS_INPUT);

        let result = confined.read_verified_with_hook(MAX_INPUT_BYTES + 1, || {
            OpenOptions::new()
                .append(true)
                .open(&input)
                .unwrap()
                .write_all(b" ")
                .unwrap();
        });
        assert!(matches!(
            result,
            Err(FrontendBuildError::InputChanged { .. })
        ));
    }

    #[test]
    fn an_earlier_input_change_blocks_the_output_commit() {
        let (_temp, source, output) = fixture();
        let earlier = source.join(CSS_ROOT).join("a.css");
        fs::write(&earlier, ".a { color: red; }").unwrap();
        fs::write(
            source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            "body { margin: 0; }",
        )
        .unwrap();
        let mut discovered = discover_fixture(&source).unwrap();
        let combined = read_and_combine_with_hook(&mut discovered.inputs, |index| {
            if index == 0 {
                fs::write(&earlier, ".a { color: tan; }").unwrap();
            }
        })
        .unwrap();

        let destination = output.join("bundle.css");
        fs::write(&destination, "old").unwrap();
        let output_directory = ConfinedDirectory::open_absolute(&output).unwrap();
        assert!(matches!(
            output_directory
                .write_atomic_with_hook("bundle.css", combined.as_bytes(), || discovered
                    .verify_unchanged(),),
            Err(FrontendBuildError::InputChanged { .. })
        ));
        assert_eq!(fs::read(destination).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_swap_is_rejected_without_opening_the_fifo() {
        use rustix::fs::{Mode, mkfifoat};

        let (_temp, source, _output) = fixture();
        let input = source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT);
        fs::write(&input, "a { color: red; }").unwrap();
        let discovered = discover_fixture(&source).unwrap();
        fs::rename(&input, source.join("original.css")).unwrap();
        mkfifoat(rustix::fs::CWD, &input, Mode::RUSR.union(Mode::WUSR)).unwrap();

        assert!(matches!(
            discovered.verify_unchanged(),
            Err(FrontendBuildError::InputChanged { .. })
        ));
    }

    #[test]
    fn nonportable_and_case_colliding_paths_fail() {
        let (_temp, source, output) = fixture();
        fs::write(source.join(CSS_ROOT).join("bad name.css"), "a{} ").unwrap();
        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::NonPortablePath { .. })
        ));

        fs::remove_file(source.join(CSS_ROOT).join("bad name.css")).unwrap();
        fs::create_dir(source.join(CSS_ROOT).join("trailing-dot.")).unwrap();
        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::NonPortablePath { .. })
        ));

        fs::remove_dir(source.join(CSS_ROOT).join("trailing-dot.")).unwrap();
        fs::write(source.join(CSS_ROOT).join("theme.css"), "a{} ").unwrap();
        fs::write(source.join(CSS_ROOT).join("THEME.css"), "b{} ").unwrap();
        let distinct_case_entries = fs::read_dir(source.join(CSS_ROOT))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("theme.css")
            })
            .count();
        if distinct_case_entries < 2 {
            return;
        }
        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::CaseCollision { .. })
        ));
    }

    #[test]
    fn non_utf8_css_bytes_fail_before_minification() {
        let (_temp, source, output) = fixture();
        fs::write(source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT), [0xff, 0xfe]).unwrap();
        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::InvalidUtf8 { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_input_names_fail() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

        let (_temp, source, output) = fixture();
        let name = OsString::from_vec(vec![0xff, b'.', b'c', b's', b's']);
        fs::write(source.join(CSS_ROOT).join(name), "a { color: red; }").unwrap();
        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::NonPortablePath { .. })
        ));
    }

    #[test]
    fn directory_segments_cannot_collide_by_ascii_case() {
        let (_temp, source, output) = fixture();
        fs::write(
            source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            "a { color: red; }",
        )
        .unwrap();
        fs::create_dir(source.join(CSS_ROOT).join("Components")).unwrap();
        match fs::create_dir(source.join(CSS_ROOT).join("components")) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return,
            Err(error) => panic!("cannot create case-collision fixture: {error}"),
        }
        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::CaseCollision { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_inputs_fail() {
        use std::os::unix::fs::symlink;

        let (_temp, source, output) = fixture();
        let outside = source.join("outside.css");
        fs::write(&outside, "a{} ").unwrap();
        symlink(&outside, source.join(CSS_ROOT).join("linked.css")).unwrap();
        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::Symlink { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_frontend_parent_cannot_escape_the_source_tree() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let source = root.join("source");
        let outside = root.join("outside");
        let output = root.join("output");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(outside.join("css")).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::write(outside.join("css/site.css"), "a { color: red; }").unwrap();
        symlink(&outside, source.join(FRONTEND_ROOT)).unwrap();

        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::Symlink { .. })
        ));
        assert!(!output.join(OUTPUT_ROOT).exists());
    }

    #[cfg(unix)]
    #[test]
    fn special_inputs_fail() {
        use std::os::unix::net::UnixListener;

        let (_temp, source, output) = fixture();
        let socket = source.join(CSS_ROOT).join("socket.css");
        let _listener = UnixListener::bind(&socket).unwrap();
        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::SpecialFile { .. })
        ));
    }

    #[test]
    fn non_file_output_destinations_fail_without_writing_through_them() {
        let (_temp, source, output) = fixture();
        fs::write(source.join(CSS_ROOT).join("site.css"), "a { color: red; }").unwrap();
        fs::create_dir_all(output.join(OUTPUT_ROOT)).unwrap();
        fs::create_dir_all(output.join(OUTPUT_ROOT).join(CSS_OUTPUT)).unwrap();
        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::UnsafeOutputPath { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn output_symlinks_fail_without_writing_through_them() {
        use std::os::unix::fs::symlink;

        let (_temp, source, output) = fixture();
        fs::write(
            source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            "a { color: red; }",
        )
        .unwrap();
        let external = output.join("external.css");
        fs::write(&external, "external").unwrap();
        fs::create_dir_all(output.join(OUTPUT_ROOT)).unwrap();
        symlink(&external, output.join(OUTPUT_ROOT).join(CSS_OUTPUT)).unwrap();

        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::UnsafeOutputPath { .. })
        ));
        assert_eq!(fs::read(external).unwrap(), b"external");
    }

    #[test]
    fn hardlinked_output_destinations_fail_without_writing_through_them() {
        let (_temp, source, output) = fixture();
        fs::write(
            source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            "a { color: red; }",
        )
        .unwrap();
        compile_frontend(&source, &output).unwrap();
        let destination = output.join(OUTPUT_ROOT).join(CSS_OUTPUT);
        let linked = output.join("linked.css");
        fs::hard_link(&destination, &linked).unwrap();
        let before = fs::read(&linked).unwrap();

        assert!(matches!(
            compile_frontend(&source, &output),
            Err(FrontendBuildError::OutputHardlink { .. })
        ));
        assert_eq!(fs::read(linked).unwrap(), before);
    }

    #[test]
    fn atomic_output_keeps_the_old_leaf_until_rename() {
        let (_temp, _source, output) = fixture();
        let destination = output.join("bundle.css");
        fs::write(&destination, "old").unwrap();
        let directory = ConfinedDirectory::open_absolute(&output).unwrap();

        directory
            .write_atomic_with_hook("bundle.css", b"new", || {
                assert_eq!(fs::read(&destination).unwrap(), b"old");
                Ok(())
            })
            .unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"new");
        assert!(fs::read_dir(&output).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".maincopy-")
        }));
    }

    #[test]
    fn build_writes_only_to_the_selected_output_directory() {
        let (_temp, source, output) = fixture();
        let input = source.join(CSS_ROOT).join("site.css");
        fs::write(&input, "body { color: red; }").unwrap();
        let before = fs::read(&input).unwrap();

        compile_frontend(&source, &output).unwrap();

        assert_eq!(fs::read(input).unwrap(), before);
        assert!(!source.join("static").exists());
        assert!(output.join(OUTPUT_ROOT).join(CSS_OUTPUT).is_file());
        assert!(output.join(GENERATED_MANIFEST).is_file());
    }

    #[test]
    fn generated_metadata_contains_full_distinct_digests() {
        let (_temp, source, output) = fixture();
        fs::write(
            source.join(CSS_ROOT).join("site.css"),
            "body { color: red; }",
        )
        .unwrap();
        compile_frontend(&source, &output).unwrap();

        let generated = fs::read_to_string(output.join(GENERATED_MANIFEST)).unwrap();
        let minified = fs::read(output.join(OUTPUT_ROOT).join(CSS_OUTPUT)).unwrap();
        let bundle = frontend_bundle_digest(&[FrontendDigestInput {
            kind: FrontendAssetKind::Css,
            bytes: &minified,
        }])
        .unwrap();
        let asset = frontend_asset_digest(FrontendAssetKind::Css, &minified);
        assert_ne!(bundle, asset);
        assert!(generated.contains(&encoded_digest(FRONTEND_BUNDLE_PREFIX, &bundle)));
        assert!(generated.contains(&encoded_digest(FRONTEND_ASSET_PREFIX, &asset)));
    }

    #[test]
    fn semantically_identical_css_retains_emitted_bytes_and_identity() {
        let (_first_temp, first_source, first_output) = fixture();
        fs::write(
            first_source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            "body { color: red; margin: 0; }",
        )
        .unwrap();
        compile_frontend(&first_source, &first_output).unwrap();

        let (_second_temp, second_source, second_output) = fixture();
        fs::write(
            second_source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            "body{color:red;margin:0}",
        )
        .unwrap();
        compile_frontend(&second_source, &second_output).unwrap();

        assert_eq!(
            fs::read(first_output.join(OUTPUT_ROOT).join(CSS_OUTPUT)).unwrap(),
            fs::read(second_output.join(OUTPUT_ROOT).join(CSS_OUTPUT)).unwrap()
        );
        assert_eq!(
            fs::read(first_output.join(GENERATED_MANIFEST)).unwrap(),
            fs::read(second_output.join(GENERATED_MANIFEST)).unwrap()
        );
    }

    #[test]
    fn output_replacement_between_capture_and_rename_fails_closed() {
        let (_temp, _source, output) = fixture();
        let destination = output.join("bundle.css");
        let displaced = output.join("displaced.css");
        fs::write(&destination, "old").unwrap();
        let directory = ConfinedDirectory::open_absolute(&output).unwrap();

        assert!(matches!(
            directory.write_atomic_with_hook("bundle.css", b"generated", || {
                fs::rename(&destination, &displaced).unwrap();
                fs::write(&destination, "replacement").unwrap();
                Ok(())
            }),
            Err(FrontendBuildError::OutputChanged { .. })
        ));
        assert_eq!(fs::read(destination).unwrap(), b"replacement");
        assert_eq!(fs::read(displaced).unwrap(), b"old");
    }

    #[test]
    fn temporary_output_is_revalidated_after_the_commit_hook() {
        let (_temp, _source, output) = fixture();
        let destination = output.join("bundle.css");
        fs::write(&destination, "old").unwrap();
        let directory = ConfinedDirectory::open_absolute(&output).unwrap();

        assert!(matches!(
            directory.write_atomic_with_hook("bundle.css", b"generated", || {
                let temporary = fs::read_dir(&output)
                    .unwrap()
                    .map(Result::unwrap)
                    .find(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".maincopy-bundle.css-")
                    })
                    .unwrap()
                    .path();
                fs::write(temporary, "tampered").unwrap();
                Ok(())
            }),
            Err(FrontendBuildError::UnsafeOutputPath { .. })
        ));
        assert_eq!(fs::read(destination).unwrap(), b"old");
        assert!(fs::read_dir(output).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".maincopy-")
        }));
    }

    #[test]
    fn emitted_css_change_changes_bundle_identity() {
        let (_first_temp, first_source, first_output) = fixture();
        fs::write(
            first_source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            "body { color: red; }",
        )
        .unwrap();
        compile_frontend(&first_source, &first_output).unwrap();

        let (_second_temp, second_source, second_output) = fixture();
        fs::write(
            second_source.join(CSS_ROOT).join(REQUIRED_CSS_INPUT),
            "body { color: blue; }",
        )
        .unwrap();
        compile_frontend(&second_source, &second_output).unwrap();

        let first_css = fs::read(first_output.join(OUTPUT_ROOT).join(CSS_OUTPUT)).unwrap();
        let second_css = fs::read(second_output.join(OUTPUT_ROOT).join(CSS_OUTPUT)).unwrap();
        assert_ne!(first_css, second_css);
        let digest = |css: &[u8]| {
            frontend_bundle_digest(&[FrontendDigestInput {
                kind: FrontendAssetKind::Css,
                bytes: css,
            }])
            .unwrap()
        };
        assert_ne!(digest(&first_css), digest(&second_css));
        assert_ne!(
            fs::read(first_output.join(GENERATED_MANIFEST)).unwrap(),
            fs::read(second_output.join(GENERATED_MANIFEST)).unwrap()
        );
    }
}

#[cfg(all(test, not(any(target_os = "linux", target_os = "macos"))))]
mod unsupported_tests {
    use super::*;

    #[test]
    fn frontend_build_fails_closed_on_an_unsupported_host() {
        assert!(matches!(
            compile_frontend(Path::new("manifest"), Path::new("output")),
            Err(FrontendBuildError::UnsupportedHost)
        ));
    }
}
