use std::{
    ffi::OsStr,
    fmt, fs,
    io::{self, Read},
    path::Path,
    process::{Child, Command, Stdio},
    str::FromStr,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

const SHA1_PREFIX: &str = "git-sha1:";
const SHA256_PREFIX: &str = "git-sha256:";
const GIT_STDOUT_READ_LIMIT: u64 = 67;
const GIT_STDOUT_VALID_LIMIT: usize = 66;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceCommitAlgorithm {
    Sha1,
    Sha256,
}

impl SourceCommitAlgorithm {
    const fn byte_length(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceCommit {
    algorithm: SourceCommitAlgorithm,
    bytes: Box<[u8]>,
    encoded: Box<str>,
}

impl SourceCommit {
    pub fn parse(value: &str) -> Result<Self, SourceCommitParseError> {
        let (algorithm, hex) = if let Some(hex) = value.strip_prefix(SHA1_PREFIX) {
            (SourceCommitAlgorithm::Sha1, hex)
        } else if let Some(hex) = value.strip_prefix(SHA256_PREFIX) {
            (SourceCommitAlgorithm::Sha256, hex)
        } else {
            return Err(SourceCommitParseError::InvalidPrefix);
        };
        if hex.len() != algorithm.byte_length() * 2 {
            return Err(SourceCommitParseError::InvalidLength { algorithm });
        }
        if !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(SourceCommitParseError::InvalidEncoding { algorithm });
        }
        let mut bytes = Vec::with_capacity(algorithm.byte_length());
        for pair in hex.as_bytes().as_chunks::<2>().0 {
            let high = decode_nibble(pair[0])
                .ok_or(SourceCommitParseError::InvalidEncoding { algorithm })?;
            let low = decode_nibble(pair[1])
                .ok_or(SourceCommitParseError::InvalidEncoding { algorithm })?;
            bytes.push(high << 4 | low);
        }
        Ok(Self {
            algorithm,
            bytes: bytes.into_boxed_slice(),
            encoded: value.into(),
        })
    }

    fn from_git_hex(value: &str) -> Result<Self, SourceCommitParseError> {
        match value.len() {
            40 => Self::parse(&format!("{SHA1_PREFIX}{value}")),
            64 => Self::parse(&format!("{SHA256_PREFIX}{value}")),
            _ => Err(SourceCommitParseError::UnsupportedObjectFormat),
        }
    }

    pub const fn algorithm(&self) -> SourceCommitAlgorithm {
        self.algorithm
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn as_str(&self) -> &str {
        &self.encoded
    }
}

impl fmt::Display for SourceCommit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SourceCommit {
    type Err = SourceCommitParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for SourceCommit {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SourceCommit {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SourceCommitParseError {
    #[error("source commit must start with git-sha1: or git-sha256:")]
    InvalidPrefix,
    #[error("{algorithm:?} source commit has the wrong encoded length")]
    InvalidLength { algorithm: SourceCommitAlgorithm },
    #[error("{algorithm:?} source commit must use lowercase hexadecimal")]
    InvalidEncoding { algorithm: SourceCommitAlgorithm },
    #[error("Git object format is not supported")]
    UnsupportedObjectFormat,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceCommitUnavailableReason {
    RepositoryMetadataAbsent,
    RepositoryMetadataUnreadable,
    GitExecutableUnavailable,
    GitCommandFailed,
    InvalidCommandOutput,
    UnsupportedObjectFormat,
}

/// Advisory Git metadata. It is never an input to a content digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceCommitDiscovery {
    Discovered(SourceCommit),
    Unavailable(SourceCommitUnavailableReason),
}

pub fn discover_source_commit(root: &Path) -> SourceCommitDiscovery {
    discover_source_commit_with(root, run_git_rev_parse)
}

#[derive(Debug)]
struct GitCommandOutput {
    stdout: Vec<u8>,
    succeeded: bool,
}

fn discover_source_commit_with<Runner>(root: &Path, run_git: Runner) -> SourceCommitDiscovery
where
    Runner: FnOnce(&Path) -> Result<GitCommandOutput, SourceCommitUnavailableReason>,
{
    if let Err(reason) = require_repository_metadata(root) {
        return SourceCommitDiscovery::Unavailable(reason);
    }
    match run_git(root) {
        Ok(output) => classify_git_output(output),
        Err(reason) => SourceCommitDiscovery::Unavailable(reason),
    }
}

fn require_repository_metadata(root: &Path) -> Result<(), SourceCommitUnavailableReason> {
    match fs::symlink_metadata(root.join(".git")) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(SourceCommitUnavailableReason::RepositoryMetadataAbsent)
        }
        Err(_) => Err(SourceCommitUnavailableReason::RepositoryMetadataUnreadable),
    }
}

fn run_git_rev_parse(root: &Path) -> Result<GitCommandOutput, SourceCommitUnavailableReason> {
    run_git_rev_parse_with_program(root, OsStr::new("git"))
}

fn run_git_rev_parse_with_program(
    root: &Path,
    program: &OsStr,
) -> Result<GitCommandOutput, SourceCommitUnavailableReason> {
    let child = spawn_git_rev_parse(root, program)?;
    collect_git_output(child)
}

fn sanitized_git_rev_parse_command(root: &Path, program: &OsStr) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .args(["--no-pager", "-C"])
        .arg(root)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped());
    command
}

fn spawn_git_rev_parse(
    root: &Path,
    program: &OsStr,
) -> Result<Child, SourceCommitUnavailableReason> {
    match sanitized_git_rev_parse_command(root, program).spawn() {
        Ok(child) => Ok(child),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(SourceCommitUnavailableReason::GitExecutableUnavailable)
        }
        Err(_) => Err(SourceCommitUnavailableReason::GitCommandFailed),
    }
}

fn collect_git_output(mut child: Child) -> Result<GitCommandOutput, SourceCommitUnavailableReason> {
    let stdout = child
        .stdout
        .take()
        .ok_or(SourceCommitUnavailableReason::GitCommandFailed)?;
    collect_bounded_git_output(stdout, || child.wait().map(|status| status.success()))
}

fn collect_bounded_git_output(
    stdout: impl Read,
    wait: impl FnOnce() -> io::Result<bool>,
) -> Result<GitCommandOutput, SourceCommitUnavailableReason> {
    let mut stdout = stdout.take(GIT_STDOUT_READ_LIMIT);
    let mut output = Vec::with_capacity(GIT_STDOUT_READ_LIMIT as usize);
    let read_result = stdout.read_to_end(&mut output);
    drop(stdout);
    let succeeded = wait().map_err(|_| SourceCommitUnavailableReason::GitCommandFailed)?;
    read_result.map_err(|_| SourceCommitUnavailableReason::GitCommandFailed)?;
    Ok(GitCommandOutput {
        stdout: output,
        succeeded,
    })
}

fn classify_git_output(output: GitCommandOutput) -> SourceCommitDiscovery {
    if output.stdout.len() > GIT_STDOUT_VALID_LIMIT {
        return SourceCommitDiscovery::Unavailable(
            SourceCommitUnavailableReason::InvalidCommandOutput,
        );
    }
    if !output.succeeded {
        return SourceCommitDiscovery::Unavailable(SourceCommitUnavailableReason::GitCommandFailed);
    }
    let output = match std::str::from_utf8(&output.stdout) {
        Ok(output) => output.strip_suffix('\n').unwrap_or(output),
        Err(_) => {
            return SourceCommitDiscovery::Unavailable(
                SourceCommitUnavailableReason::InvalidCommandOutput,
            );
        }
    };
    match SourceCommit::from_git_hex(output) {
        Ok(commit) => SourceCommitDiscovery::Discovered(commit),
        Err(SourceCommitParseError::UnsupportedObjectFormat) => SourceCommitDiscovery::Unavailable(
            SourceCommitUnavailableReason::UnsupportedObjectFormat,
        ),
        Err(_) => {
            SourceCommitDiscovery::Unavailable(SourceCommitUnavailableReason::InvalidCommandOutput)
        }
    }
}

const fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, io::Cursor};

    use super::*;

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("injected read failure"))
        }
    }

    fn repository_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        root
    }

    fn command_output(stdout: impl Into<Vec<u8>>, succeeded: bool) -> GitCommandOutput {
        GitCommandOutput {
            stdout: stdout.into(),
            succeeded,
        }
    }

    #[test]
    fn source_commits_are_strict_and_algorithm_typed() {
        let sha1 = SourceCommit::parse(&format!("git-sha1:{}", "ab".repeat(20))).unwrap();
        assert_eq!(sha1.algorithm(), SourceCommitAlgorithm::Sha1);
        assert_eq!(sha1.as_bytes().len(), 20);

        let sha256 = SourceCommit::parse(&format!("git-sha256:{}", "cd".repeat(32))).unwrap();
        assert_eq!(sha256.algorithm(), SourceCommitAlgorithm::Sha256);
        assert_eq!(sha256.as_bytes().len(), 32);

        for invalid in [
            "ab".repeat(20),
            format!("git-sha1:{}", "AB".repeat(20)),
            format!("git-sha1:{}", "ab".repeat(19)),
            format!("git-sha256:{}", "gg".repeat(32)),
        ] {
            assert!(SourceCommit::parse(&invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn source_commit_serde_preserves_the_versioned_wire_value() {
        let value = format!("git-sha1:{}", "01".repeat(20));
        let commit = SourceCommit::parse(&value).unwrap();
        assert_eq!(serde_json::to_value(&commit).unwrap(), value);
        assert_eq!(
            serde_json::from_value::<SourceCommit>(serde_json::json!(value)).unwrap(),
            commit
        );
    }

    #[test]
    fn provenance_enum_wire_names_are_stable() {
        for (value, expected) in [
            (
                serde_json::to_value(SourceCommitAlgorithm::Sha1).unwrap(),
                "sha1",
            ),
            (
                serde_json::to_value(SourceCommitAlgorithm::Sha256).unwrap(),
                "sha256",
            ),
            (
                serde_json::to_value(SourceCommitUnavailableReason::RepositoryMetadataAbsent)
                    .unwrap(),
                "repository_metadata_absent",
            ),
            (
                serde_json::to_value(SourceCommitUnavailableReason::RepositoryMetadataUnreadable)
                    .unwrap(),
                "repository_metadata_unreadable",
            ),
            (
                serde_json::to_value(SourceCommitUnavailableReason::GitExecutableUnavailable)
                    .unwrap(),
                "git_executable_unavailable",
            ),
            (
                serde_json::to_value(SourceCommitUnavailableReason::GitCommandFailed).unwrap(),
                "git_command_failed",
            ),
            (
                serde_json::to_value(SourceCommitUnavailableReason::InvalidCommandOutput).unwrap(),
                "invalid_command_output",
            ),
            (
                serde_json::to_value(SourceCommitUnavailableReason::UnsupportedObjectFormat)
                    .unwrap(),
                "unsupported_object_format",
            ),
        ] {
            assert_eq!(value, serde_json::json!(expected));
        }
    }

    #[test]
    fn content_only_directories_have_typed_advisory_provenance() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            discover_source_commit(root.path()),
            SourceCommitDiscovery::Unavailable(
                SourceCommitUnavailableReason::RepositoryMetadataAbsent
            )
        );
    }

    #[test]
    fn discovery_accepts_strict_sha1_and_sha256_command_output() {
        for (hex, expected) in [
            (
                "ab".repeat(20),
                SourceCommit::parse(&format!("{SHA1_PREFIX}{}", "ab".repeat(20))).unwrap(),
            ),
            (
                "cd".repeat(32),
                SourceCommit::parse(&format!("{SHA256_PREFIX}{}", "cd".repeat(32))).unwrap(),
            ),
        ] {
            let root = repository_root();
            let output = format!("{hex}\n");
            assert_eq!(
                discover_source_commit_with(root.path(), |_| Ok(command_output(output, true))),
                SourceCommitDiscovery::Discovered(expected)
            );
        }
    }

    #[test]
    fn discovery_distinguishes_a_missing_git_executable() {
        let root = repository_root();
        let missing_program = root.path().join("git-does-not-exist");

        assert_eq!(
            discover_source_commit_with(root.path(), |root| {
                run_git_rev_parse_with_program(root, missing_program.as_os_str())
            }),
            SourceCommitDiscovery::Unavailable(
                SourceCommitUnavailableReason::GitExecutableUnavailable
            )
        );
    }

    #[test]
    fn git_output_collection_is_bounded_and_retains_exit_status() {
        let output = collect_bounded_git_output(
            Cursor::new(vec![b'a'; GIT_STDOUT_READ_LIMIT as usize + 8]),
            || Ok(false),
        )
        .unwrap();

        assert_eq!(output.stdout.len(), GIT_STDOUT_READ_LIMIT as usize);
        assert!(!output.succeeded);
    }

    #[test]
    fn git_output_collection_waits_after_read_failure_and_maps_io_failures() {
        let waited = Cell::new(false);
        assert_eq!(
            collect_bounded_git_output(FailingReader, || {
                waited.set(true);
                Ok(true)
            })
            .unwrap_err(),
            SourceCommitUnavailableReason::GitCommandFailed
        );
        assert!(waited.get());

        assert_eq!(
            collect_bounded_git_output(Cursor::new(b"commit"), || {
                Err(io::Error::other("injected wait failure"))
            })
            .unwrap_err(),
            SourceCommitUnavailableReason::GitCommandFailed
        );
    }

    #[test]
    fn discovery_rejects_a_nonzero_git_exit() {
        let root = repository_root();
        let output = format!("{}\n", "ab".repeat(20));

        assert_eq!(
            discover_source_commit_with(root.path(), |_| Ok(command_output(output, false))),
            SourceCommitDiscovery::Unavailable(SourceCommitUnavailableReason::GitCommandFailed)
        );
    }

    #[test]
    fn discovery_rejects_overlong_git_output() {
        let root = repository_root();

        assert_eq!(
            discover_source_commit_with(root.path(), |_| {
                Ok(command_output(
                    vec![b'a'; GIT_STDOUT_READ_LIMIT as usize],
                    true,
                ))
            }),
            SourceCommitDiscovery::Unavailable(SourceCommitUnavailableReason::InvalidCommandOutput)
        );
    }

    #[test]
    fn discovery_rejects_non_utf8_git_output() {
        let root = repository_root();

        assert_eq!(
            discover_source_commit_with(root.path(), |_| {
                Ok(command_output(vec![0xff; 40], true))
            }),
            SourceCommitDiscovery::Unavailable(SourceCommitUnavailableReason::InvalidCommandOutput)
        );
    }

    #[test]
    fn discovery_reports_unsupported_git_object_lengths() {
        let root = repository_root();

        assert_eq!(
            discover_source_commit_with(root.path(), |_| {
                Ok(command_output(format!("{}\n", "ab".repeat(24)), true))
            }),
            SourceCommitDiscovery::Unavailable(
                SourceCommitUnavailableReason::UnsupportedObjectFormat
            )
        );
    }
}
