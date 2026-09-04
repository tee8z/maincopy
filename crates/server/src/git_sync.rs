//! Bounded, read-only synchronization from one structured SSH Git remote.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::{OsStr, OsString},
    fs,
    future::Future,
    io::{self, Read as _},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use maincopy_shared::source::{
    GitBranchName, RepositoryContentSubdirectory, SourceSyncFailureCode, SshCredentialName,
    SshRemote,
};
use markdown_compiler::{
    ContentTreeLimits, ContentValidationErrors, DiscoveredContentTree, discover_content_tree,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::process::{Pid, Resource, Rlimit, Signal, getrlimit, kill_process_group, setrlimit};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    process::{Child, Command},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;

use crate::{
    config::{GitProcessLimits, SensitivePath, SshCredentialReference},
    domain::publication::{SourceCommit, SourceCommitAlgorithm},
    git_ssh_contract::{
        EXPECTED_PORT_ENV, EXPECTED_REPOSITORY_ENV, EXPECTED_TARGET_ENV, KNOWN_HOSTS_ENV,
        PRIVATE_KEY_ENV, SSH_EXECUTABLE_ENV,
    },
    process_lock::{ProcessLock, prepare_private_directory, reject_symlink_components},
};

const GIT_EXECUTABLE_ENV: &str = "MAINCOPY_GIT_EXECUTABLE";
const MIRROR_DIRECTORY: &str = "repository.git";
const STAGING_REFERENCE: &str = "refs/maincopy/staging";
const MAX_CREDENTIAL_FILE_BYTES: u64 = 64 * 1024;
const MAX_MIRROR_CONFIGURATION_BYTES: u64 = 4 * 1024;
const MAX_MIRROR_FILESYSTEM_ENTRIES: usize = 65_536;
const MAX_BATCH_HEADER_BYTES: usize = 256;

/// The result of fetching and resolving the configured branch.
#[derive(Debug)]
pub(crate) enum GitSyncOutcome {
    NoChange { source_commit: SourceCommit },
    Candidate(GitContentCandidate),
}

/// An immutable content tree tied to the exact fetched source commit.
#[derive(Debug)]
pub(crate) struct GitContentCandidate {
    pub(crate) source_commit: SourceCommit,
    pub(crate) tree: DiscoveredContentTree,
}

/// One serialized managed-Git capability.
#[derive(Clone)]
pub(crate) struct GitSync {
    mirror_root: PathBuf,
    _mirror_ownership: Arc<ProcessLock>,
    credentials: BTreeMap<SshCredentialName, SshCredentialReference>,
    process_limits: GitProcessLimits,
    content_limits: ContentTreeLimits,
    git_executable: PathBuf,
    ssh_executable: PathBuf,
    ssh_helper: PathBuf,
    admission: Arc<Mutex<()>>,
}

impl GitSync {
    /// Discovers packaged executables and copies only redacted credential references.
    pub(crate) fn discover(
        mirror_root: &SensitivePath,
        credentials: &BTreeMap<SshCredentialName, SshCredentialReference>,
        process_limits: GitProcessLimits,
        content_limits: ContentTreeLimits,
    ) -> Result<Self, GitSyncError> {
        let git_executable = discover_executable(GIT_EXECUTABLE_ENV, "git")?;
        let ssh_executable = discover_executable(SSH_EXECUTABLE_ENV, "ssh")?;
        let ssh_helper = discover_sibling_helper()?;
        Self::from_executables(
            mirror_root.path().to_path_buf(),
            credentials.clone(),
            process_limits,
            content_limits,
            git_executable,
            ssh_executable,
            ssh_helper,
        )
    }

    fn from_executables(
        mirror_root: PathBuf,
        credentials: BTreeMap<SshCredentialName, SshCredentialReference>,
        process_limits: GitProcessLimits,
        content_limits: ContentTreeLimits,
        git_executable: PathBuf,
        ssh_executable: PathBuf,
        ssh_helper: PathBuf,
    ) -> Result<Self, GitSyncError> {
        if !mirror_root.is_absolute()
            || !git_executable.is_absolute()
            || !ssh_executable.is_absolute()
            || !ssh_helper.is_absolute()
        {
            return Err(GitSyncError::ExecutableOrMirrorPathNotAbsolute);
        }
        let mirror_ownership =
            ProcessLock::acquire(&mirror_root).map_err(|_| GitSyncError::MirrorUnavailable)?;
        Ok(Self {
            mirror_root,
            _mirror_ownership: Arc::new(mirror_ownership),
            credentials,
            process_limits,
            content_limits,
            git_executable,
            ssh_executable,
            ssh_helper,
            admission: Arc::new(Mutex::new(())),
        })
    }

    /// Fetches only the configured branch and materializes blobs from its exact commit.
    pub(crate) async fn synchronize(
        &self,
        remote: &SshRemote,
        branch: &GitBranchName,
        content_subdirectory: &RepositoryContentSubdirectory,
        credential_name: &SshCredentialName,
        installed_commit: Option<&SourceCommit>,
        cancellation: &CancellationToken,
    ) -> Result<GitSyncOutcome, GitSyncError> {
        let _admission = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(GitSyncError::Cancelled),
            guard = self.admission.lock() => guard,
        };
        require_supported_platform()?;
        let credential = self.resolve_credential(credential_name).await?;
        self.prepare_mirror_root().await?;
        self.require_mirror_within_limit().await?;

        let transport =
            SshTransport::new(remote, credential, &self.ssh_executable, &self.ssh_helper);
        let advertised = self
            .advertised_commit(&transport, branch, cancellation)
            .await?;
        if installed_commit == Some(&advertised.0) {
            return Ok(GitSyncOutcome::NoChange {
                source_commit: advertised.0,
            });
        }
        self.prepare_bare_mirror(advertised.algorithm_name(), cancellation)
            .await?;
        self.require_mirror_within_limit().await?;

        self.fetch_branch(&transport, branch, cancellation).await?;
        self.prune_mirror(cancellation).await?;
        self.require_mirror_within_limit().await?;
        let commit = self.resolve_fetched_commit(cancellation).await?;

        let entries = self
            .list_candidate_entries(&commit, content_subdirectory, cancellation)
            .await?;
        let tree = self.materialize_candidate(entries, cancellation).await?;
        Ok(GitSyncOutcome::Candidate(GitContentCandidate {
            source_commit: commit,
            tree,
        }))
    }

    async fn resolve_credential(
        &self,
        name: &SshCredentialName,
    ) -> Result<ResolvedCredential, GitSyncError> {
        let reference = self
            .credentials
            .get(name)
            .cloned()
            .ok_or(GitSyncError::CredentialUnavailable)?;
        tokio::task::spawn_blocking(move || validate_credential(reference))
            .await
            .map_err(|_| GitSyncError::CredentialValidationWorkerFailed)?
    }

    async fn prepare_mirror_root(&self) -> Result<(), GitSyncError> {
        let path = self.mirror_root.clone();
        tokio::task::spawn_blocking(move || prepare_private_directory(&path))
            .await
            .map_err(|_| GitSyncError::FilesystemWorkerFailed)?
            .map_err(|_| GitSyncError::MirrorUnavailable)
    }

    async fn prepare_bare_mirror(
        &self,
        object_format: &'static str,
        cancellation: &CancellationToken,
    ) -> Result<(), GitSyncError> {
        let mirror = self.mirror_path();
        let state = tokio::task::spawn_blocking({
            let mirror = mirror.clone();
            move || inspect_mirror_path(&mirror)
        })
        .await
        .map_err(|_| GitSyncError::FilesystemWorkerFailed)?
        .map_err(|_| GitSyncError::MirrorUnsafe)?;

        if state == MirrorState::Absent {
            let arguments = vec![
                OsString::from("init"),
                OsString::from("--bare"),
                OsString::from("--template="),
                OsString::from(format!("--object-format={object_format}")),
                mirror.as_os_str().to_owned(),
            ];
            self.run_git(
                GitCommandPhase::InitializeMirror,
                &self.mirror_root,
                arguments,
                None,
                cancellation,
            )
            .await?;
        }

        let output = self
            .run_git(
                GitCommandPhase::InspectMirror,
                &mirror,
                os_arguments(["rev-parse", "--is-bare-repository", "--show-object-format"]),
                None,
                cancellation,
            )
            .await?;
        let text = parse_ascii_line_output(&output.stdout)?;
        let mut lines = text.lines();
        if lines.next() != Some("true")
            || lines.next() != Some(object_format)
            || lines.next().is_some()
        {
            return Err(GitSyncError::MirrorObjectFormatMismatch);
        }
        Ok(())
    }

    async fn advertised_commit(
        &self,
        transport: &SshTransport<'_>,
        branch: &GitBranchName,
        cancellation: &CancellationToken,
    ) -> Result<AdvertisedCommit, GitSyncError> {
        let branch_reference = format!("refs/heads/{branch}");
        let output = self
            .run_git(
                GitCommandPhase::DiscoverBranch,
                &self.mirror_root,
                vec![
                    OsString::from("ls-remote"),
                    OsString::from("--exit-code"),
                    OsString::from("--heads"),
                    OsString::from("--refs"),
                    transport.remote_argument.clone(),
                    OsString::from(&branch_reference),
                ],
                Some(transport),
                cancellation,
            )
            .await
            .map_err(|error| match error {
                GitSyncError::CommandFailed {
                    phase: GitCommandPhase::DiscoverBranch,
                    status: Some(2),
                } => GitSyncError::BranchUnavailable,
                error => error,
            })?;
        parse_advertised_commit(&output.stdout, &branch_reference)
    }

    async fn fetch_branch(
        &self,
        transport: &SshTransport<'_>,
        branch: &GitBranchName,
        cancellation: &CancellationToken,
    ) -> Result<(), GitSyncError> {
        let refspec = format!("+refs/heads/{branch}:{STAGING_REFERENCE}");
        self.run_git(
            GitCommandPhase::FetchBranch,
            &self.mirror_path(),
            vec![
                OsString::from("-c"),
                OsString::from("fetch.unpackLimit=0"),
                OsString::from("fetch"),
                OsString::from("--force"),
                OsString::from("--depth=1"),
                OsString::from("--refetch"),
                OsString::from("--update-shallow"),
                OsString::from("--no-tags"),
                OsString::from("--no-recurse-submodules"),
                OsString::from("--no-write-fetch-head"),
                OsString::from("--no-auto-maintenance"),
                transport.remote_argument.clone(),
                OsString::from(refspec),
            ],
            Some(transport),
            cancellation,
        )
        .await?;
        Ok(())
    }

    async fn prune_mirror(&self, cancellation: &CancellationToken) -> Result<(), GitSyncError> {
        self.run_git(
            GitCommandPhase::PruneMirror,
            &self.mirror_path(),
            os_arguments([
                "-c",
                "gc.reflogExpire=now",
                "-c",
                "gc.reflogExpireUnreachable=now",
                "-c",
                "gc.cruftPacks=false",
                "gc",
                "--prune=now",
                "--no-cruft",
                "--no-detach",
                "--quiet",
            ]),
            None,
            cancellation,
        )
        .await?;
        Ok(())
    }

    async fn resolve_fetched_commit(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<SourceCommit, GitSyncError> {
        let output = self
            .run_git(
                GitCommandPhase::ResolveCommit,
                &self.mirror_path(),
                os_arguments([
                    "rev-parse",
                    "--verify",
                    &format!("{STAGING_REFERENCE}^{{commit}}"),
                ]),
                None,
                cancellation,
            )
            .await?;
        let value = parse_one_ascii_token(&output.stdout)?;
        SourceCommit::from_git_hex(value).map_err(|_| GitSyncError::CommitInvalid)
    }

    async fn list_candidate_entries(
        &self,
        commit: &SourceCommit,
        content_subdirectory: &RepositoryContentSubdirectory,
        cancellation: &CancellationToken,
    ) -> Result<Vec<TreeEntry>, GitSyncError> {
        let mut arguments = vec![
            OsString::from("ls-tree"),
            OsString::from("-r"),
            OsString::from("-z"),
            OsString::from("-l"),
            OsString::from("--full-tree"),
            OsString::from(commit_hex(commit)),
            OsString::from("--"),
        ];
        if content_subdirectory.as_str() != "." {
            arguments.push(OsString::from(content_subdirectory.as_str()));
        }
        let output = self
            .run_git(
                GitCommandPhase::ListCandidate,
                &self.mirror_path(),
                arguments,
                None,
                cancellation,
            )
            .await?;
        parse_tree_entries(
            &output.stdout,
            commit,
            content_subdirectory,
            self.content_limits,
            self.process_limits.file_bytes.get(),
        )
    }

    async fn materialize_candidate(
        &self,
        entries: Vec<TreeEntry>,
        cancellation: &CancellationToken,
    ) -> Result<DiscoveredContentTree, GitSyncError> {
        let parent = self
            .mirror_root
            .parent()
            .ok_or(GitSyncError::MirrorUnavailable)?;
        let workspace = tempfile::Builder::new()
            .prefix(".maincopy-git-candidate-")
            .tempdir_in(parent)
            .map_err(|_| GitSyncError::CandidateWorkspaceUnavailable)?;
        prepare_candidate_directories(workspace.path(), &entries)?;
        self.run_batch_materialization(workspace.path(), &entries, cancellation)
            .await?;

        let limits = self.content_limits;
        tokio::task::spawn_blocking(move || {
            let _workspace = workspace;
            discover_content_tree(_workspace.path(), limits)
        })
        .await
        .map_err(|_| GitSyncError::CandidateDiscoveryWorkerFailed)?
        .map_err(|error| GitSyncError::CandidateValidation(Box::new(error)))
    }

    async fn run_batch_materialization(
        &self,
        root: &Path,
        entries: &[TreeEntry],
        cancellation: &CancellationToken,
    ) -> Result<(), GitSyncError> {
        let mut command = self
            .prepare_git_command(
                GitCommandPhase::ReadCandidateBlobs,
                &self.mirror_path(),
                os_arguments(["cat-file", "--batch"]),
                None,
            )
            .await?;
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|_| GitSyncError::CommandSpawnFailed(GitCommandPhase::ReadCandidateBlobs))?;
        let process_group =
            ChildProcessGroup::capture(&child).ok_or(GitSyncError::CommandWaitFailed)?;
        let stdin = child
            .stdin
            .take()
            .ok_or(GitSyncError::CommandPipeUnavailable)?;
        let stdout = child
            .stdout
            .take()
            .ok_or(GitSyncError::CommandPipeUnavailable)?;
        let request = entries
            .iter()
            .flat_map(|entry| [entry.object.as_bytes(), b"\n"].concat())
            .collect::<Vec<_>>();
        let writer = async move {
            let mut stdin = stdin;
            stdin
                .write_all(&request)
                .await
                .map_err(|_| GitSyncError::CommandInputFailed)?;
            stdin
                .shutdown()
                .await
                .map_err(|_| GitSyncError::CommandInputFailed)
        };
        let reader = materialize_batch_output(stdout, root.to_path_buf(), entries.to_vec());
        let exchange = async move {
            tokio::try_join!(writer, reader)?;
            Ok::<(), GitSyncError>(())
        };
        let (status, ()) = supervise_child_output(
            &mut child,
            process_group,
            exchange,
            self.process_limits.wall_time.get(),
            cancellation,
            GitCommandPhase::ReadCandidateBlobs,
        )
        .await?;
        if !status.success() {
            return Err(GitSyncError::CommandFailed {
                phase: GitCommandPhase::ReadCandidateBlobs,
                status: status.code(),
            });
        }
        Ok(())
    }

    async fn run_git(
        &self,
        phase: GitCommandPhase,
        current_directory: &Path,
        arguments: Vec<OsString>,
        transport: Option<&SshTransport<'_>>,
        cancellation: &CancellationToken,
    ) -> Result<GitCommandOutput, GitSyncError> {
        let capture_transport_diagnostic = transport.is_some();
        let mut command = self
            .prepare_git_command(phase, current_directory, arguments, transport)
            .await?;
        command.stdout(Stdio::piped());
        if capture_transport_diagnostic {
            command.stderr(Stdio::piped());
        }
        let mut child = command
            .spawn()
            .map_err(|_| GitSyncError::CommandSpawnFailed(phase))?;
        let process_group =
            ChildProcessGroup::capture(&child).ok_or(GitSyncError::CommandWaitFailed)?;
        let stdout = child
            .stdout
            .take()
            .ok_or(GitSyncError::CommandPipeUnavailable)?;
        let stderr = if capture_transport_diagnostic {
            Some(
                child
                    .stderr
                    .take()
                    .ok_or(GitSyncError::CommandPipeUnavailable)?,
            )
        } else {
            None
        };
        let output_limit = self.process_limits.command_output_bytes.get();
        let output_budget = Arc::new(AtomicU64::new(output_limit));
        let read = async move {
            let (stdout, stderr) = if let Some(stderr) = stderr {
                tokio::try_join!(
                    read_bounded(stdout, output_budget.clone()),
                    read_bounded(stderr, output_budget)
                )?
            } else {
                (read_bounded(stdout, output_budget).await?, Vec::new())
            };
            Ok::<_, GitSyncError>((stdout, stderr))
        };
        let (status, (stdout, stderr)) = supervise_child_output(
            &mut child,
            process_group,
            read,
            self.process_limits.wall_time.get(),
            cancellation,
            phase,
        )
        .await?;
        if !status.success() {
            if capture_transport_diagnostic
                && let Some(error) = classify_transport_diagnostic(&stderr)
            {
                return Err(error);
            }
            return Err(GitSyncError::CommandFailed {
                phase,
                status: status.code(),
            });
        }
        Ok(GitCommandOutput { stdout })
    }

    async fn prepare_git_command(
        &self,
        phase: GitCommandPhase,
        current_directory: &Path,
        arguments: Vec<OsString>,
        transport: Option<&SshTransport<'_>>,
    ) -> Result<Command, GitSyncError> {
        let mirror_root = self.mirror_root.clone();
        tokio::task::spawn_blocking(move || validate_mirror_configuration(&mirror_root, phase))
            .await
            .map_err(|_| GitSyncError::FilesystemWorkerFailed)??;
        self.git_command(phase, current_directory, arguments, transport)
    }

    fn git_command(
        &self,
        phase: GitCommandPhase,
        current_directory: &Path,
        arguments: Vec<OsString>,
        transport: Option<&SshTransport<'_>>,
    ) -> Result<Command, GitSyncError> {
        let restricted_ssh = matches!(
            phase,
            GitCommandPhase::DiscoverBranch | GitCommandPhase::FetchBranch
        );
        if restricted_ssh != transport.is_some() {
            return Err(GitSyncError::CommandPolicyViolation(phase));
        }
        let mut command = Command::new(&self.git_executable);
        command
            .env_clear()
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env(
                "GIT_ALLOW_PROTOCOL",
                if restricted_ssh { "ssh" } else { "" },
            )
            .env("GIT_PROTOCOL_FROM_USER", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", &self.mirror_root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "")
            .current_dir(current_directory)
            .args([OsStr::new("-c"), OsStr::new("core.hooksPath=/dev/null")])
            .args(arguments)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(transport) = transport {
            verify_credential(&transport.credential)?;
            command
                .env("GIT_SSH", transport.helper)
                .env("GIT_SSH_VARIANT", "ssh")
                .env(SSH_EXECUTABLE_ENV, transport.ssh)
                .env(PRIVATE_KEY_ENV, &transport.credential.private_key.path)
                .env(KNOWN_HOSTS_ENV, &transport.credential.known_hosts.path)
                .env(EXPECTED_TARGET_ENV, &transport.target)
                .env(EXPECTED_PORT_ENV, &transport.port)
                .env(EXPECTED_REPOSITORY_ENV, transport.repository);
        }
        apply_process_policy(&mut command, self.process_limits)?;
        Ok(command)
    }

    async fn require_mirror_within_limit(&self) -> Result<(), GitSyncError> {
        let root = self.mirror_root.clone();
        let byte_limit = self.process_limits.mirror_bytes.get();
        tokio::task::spawn_blocking(move || {
            bounded_tree_bytes(&root, byte_limit, MAX_MIRROR_FILESYSTEM_ENTRIES)
        })
        .await
        .map_err(|_| GitSyncError::FilesystemWorkerFailed)?
    }

    fn mirror_path(&self) -> PathBuf {
        self.mirror_root.join(MIRROR_DIRECTORY)
    }
}

struct SshTransport<'configuration> {
    remote_argument: OsString,
    target: String,
    port: String,
    repository: &'configuration str,
    credential: ResolvedCredential,
    ssh: &'configuration Path,
    helper: &'configuration Path,
}

impl<'configuration> SshTransport<'configuration> {
    fn new(
        remote: &'configuration SshRemote,
        credential: ResolvedCredential,
        ssh: &'configuration Path,
        helper: &'configuration Path,
    ) -> Self {
        let target = format!("{}@{}", remote.user, remote.host);
        Self {
            remote_argument: OsString::from(format!("{}:{}", target, remote.repository_path)),
            target,
            port: remote.port.get().to_string(),
            repository: remote.repository_path.as_str(),
            credential,
            ssh,
            helper,
        }
    }
}

struct ResolvedCredential {
    private_key: ValidatedCredentialFile,
    known_hosts: ValidatedCredentialFile,
}

struct ValidatedCredentialFile {
    path: PathBuf,
    identity: CredentialFileIdentity,
}

#[cfg(unix)]
#[derive(Clone, Copy, Eq, PartialEq)]
struct CredentialFileIdentity {
    device: u64,
    inode: u64,
    bytes: u64,
    status_changed_seconds: i64,
    status_changed_nanoseconds: i64,
}

#[cfg(not(unix))]
#[derive(Clone, Copy, Eq, PartialEq)]
struct CredentialFileIdentity {
    bytes: u64,
    modified: Option<std::time::SystemTime>,
}

#[derive(Clone, Copy)]
enum CredentialFileKind {
    PrivateKey,
    KnownHosts,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TreeEntry {
    object: String,
    size: u64,
    relative_path: String,
}

struct AdvertisedCommit(SourceCommit);

impl AdvertisedCommit {
    fn algorithm_name(&self) -> &'static str {
        match self.0.algorithm() {
            SourceCommitAlgorithm::Sha1 => "sha1",
            SourceCommitAlgorithm::Sha256 => "sha256",
        }
    }
}

struct GitCommandOutput {
    stdout: Vec<u8>,
}

/// Safe failure classes that never contain remote output or credential paths.
#[derive(Debug, Error)]
pub(crate) enum GitSyncError {
    #[error("a managed Git executable or mirror path is not absolute")]
    ExecutableOrMirrorPathNotAbsolute,
    #[error("a required managed Git executable could not be discovered")]
    ExecutableUnavailable,
    #[error("the current executable location could not be discovered")]
    CurrentExecutableUnavailable,
    #[error("the current executable has no containing directory")]
    CurrentExecutableHasNoParent,
    #[error("managed Git synchronization is unavailable on this platform")]
    UnsupportedPlatform,
    #[error("the selected SSH credential is unavailable or unsafe")]
    CredentialUnavailable,
    #[error("the SSH credential validation worker failed")]
    CredentialValidationWorkerFailed,
    #[error("the managed Git filesystem worker failed")]
    FilesystemWorkerFailed,
    #[error("the managed Git mirror is unavailable")]
    MirrorUnavailable,
    #[error("the managed Git mirror contains an unsafe filesystem entry")]
    MirrorUnsafe,
    #[error("the managed Git mirror configuration is not the closed Maincopy configuration")]
    MirrorConfigurationUnsafe,
    #[error("the managed Git mirror exceeds its configured byte limit")]
    MirrorByteLimitExceeded,
    #[error("the managed Git mirror exceeds its filesystem entry limit")]
    MirrorEntryLimitExceeded,
    #[error("the managed Git mirror uses a different object format")]
    MirrorObjectFormatMismatch,
    #[error("the configured branch is unavailable")]
    BranchUnavailable,
    #[error("the SSH host key is unknown or did not match")]
    UnknownHost,
    #[error("the SSH remote rejected the configured public key")]
    AuthenticationFailed,
    #[error("the SSH remote could not be reached")]
    RemoteUnavailable,
    #[error("the remote branch advertisement is invalid")]
    RemoteAdvertisementInvalid,
    #[error("the fetched Git commit is invalid")]
    CommitInvalid,
    #[error("managed Git {0:?} could not be started")]
    CommandSpawnFailed(GitCommandPhase),
    #[error("managed Git {0:?} violated the fixed command policy")]
    CommandPolicyViolation(GitCommandPhase),
    #[error("managed Git command pipes could not be acquired")]
    CommandPipeUnavailable,
    #[error("managed Git command input failed")]
    CommandInputFailed,
    #[error("managed Git command output could not be read")]
    CommandOutputFailed,
    #[error("managed Git command output exceeded its configured limit")]
    CommandOutputLimitExceeded,
    #[error("managed Git command could not be reaped")]
    CommandWaitFailed,
    #[error("managed Git {phase:?} failed with a redacted status")]
    CommandFailed {
        phase: GitCommandPhase,
        status: Option<i32>,
    },
    #[error("managed Git {0:?} exceeded its wall-time limit")]
    TimedOut(GitCommandPhase),
    #[error("managed Git synchronization was cancelled")]
    Cancelled,
    #[error("the candidate tree listing is invalid")]
    CandidateListingInvalid,
    #[error("the candidate contains a symlink, gitlink, or unsupported object")]
    CandidateObjectUnsupported,
    #[error("the candidate path is unsafe or exceeds its configured limit")]
    CandidatePathInvalid,
    #[error("the candidate has too many entries or too much content")]
    CandidateLimitExceeded,
    #[error("the candidate workspace is unavailable")]
    CandidateWorkspaceUnavailable,
    #[error("the candidate blob stream is invalid")]
    CandidateBlobStreamInvalid,
    #[error("the candidate blob could not be written")]
    CandidateWriteFailed,
    #[error("the candidate discovery worker failed")]
    CandidateDiscoveryWorkerFailed,
    #[error("the candidate failed content validation")]
    CandidateValidation(#[source] Box<ContentValidationErrors>),
}

impl GitSyncError {
    pub(crate) const fn failure_code(&self) -> SourceSyncFailureCode {
        match self {
            Self::CredentialUnavailable | Self::CredentialValidationWorkerFailed => {
                SourceSyncFailureCode::CredentialUnavailable
            }
            Self::BranchUnavailable => SourceSyncFailureCode::BranchUnavailable,
            Self::UnknownHost => SourceSyncFailureCode::UnknownHost,
            Self::AuthenticationFailed => SourceSyncFailureCode::AuthenticationFailed,
            Self::RemoteUnavailable | Self::RemoteAdvertisementInvalid => {
                SourceSyncFailureCode::RemoteUnavailable
            }
            Self::CommitInvalid | Self::MirrorObjectFormatMismatch => {
                SourceSyncFailureCode::CommitInvalid
            }
            Self::TimedOut(_) => SourceSyncFailureCode::TimedOut,
            Self::Cancelled => SourceSyncFailureCode::Interrupted,
            Self::CandidateValidation(_) => SourceSyncFailureCode::ValidationFailed,
            Self::CandidateListingInvalid
            | Self::CandidateObjectUnsupported
            | Self::CandidatePathInvalid
            | Self::CandidateLimitExceeded
            | Self::CandidateWorkspaceUnavailable
            | Self::CandidateBlobStreamInvalid
            | Self::CandidateWriteFailed
            | Self::CandidateDiscoveryWorkerFailed => SourceSyncFailureCode::CandidateFailed,
            Self::CommandFailed {
                phase: GitCommandPhase::DiscoverBranch | GitCommandPhase::FetchBranch,
                ..
            } => SourceSyncFailureCode::FetchFailed,
            Self::ExecutableOrMirrorPathNotAbsolute
            | Self::ExecutableUnavailable
            | Self::CurrentExecutableUnavailable
            | Self::CurrentExecutableHasNoParent
            | Self::UnsupportedPlatform
            | Self::FilesystemWorkerFailed
            | Self::MirrorUnavailable
            | Self::MirrorUnsafe
            | Self::MirrorConfigurationUnsafe
            | Self::MirrorByteLimitExceeded
            | Self::MirrorEntryLimitExceeded
            | Self::CommandSpawnFailed(_)
            | Self::CommandPolicyViolation(_)
            | Self::CommandPipeUnavailable
            | Self::CommandInputFailed
            | Self::CommandOutputFailed
            | Self::CommandOutputLimitExceeded
            | Self::CommandWaitFailed
            | Self::CommandFailed { .. } => SourceSyncFailureCode::Internal,
        }
    }
}

/// The fixed Git operation whose safe status is reported on failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GitCommandPhase {
    DiscoverBranch,
    InitializeMirror,
    InspectMirror,
    FetchBranch,
    PruneMirror,
    ResolveCommit,
    ListCandidate,
    ReadCandidateBlobs,
}

fn discover_executable(
    environment_name: &str,
    fallback_name: &str,
) -> Result<PathBuf, GitSyncError> {
    if let Some(configured) = env::var_os(environment_name) {
        return canonical_executable(PathBuf::from(configured));
    }
    let search = env::var_os("PATH").ok_or(GitSyncError::ExecutableUnavailable)?;
    env::split_paths(&search)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(fallback_name))
        .find_map(|candidate| canonical_executable(candidate).ok())
        .ok_or(GitSyncError::ExecutableUnavailable)
}

fn canonical_executable(path: PathBuf) -> Result<PathBuf, GitSyncError> {
    if !path.is_absolute() {
        return Err(GitSyncError::ExecutableOrMirrorPathNotAbsolute);
    }
    let canonical = fs::canonicalize(path).map_err(|_| GitSyncError::ExecutableUnavailable)?;
    let metadata = fs::metadata(&canonical).map_err(|_| GitSyncError::ExecutableUnavailable)?;
    if !metadata.is_file() {
        return Err(GitSyncError::ExecutableUnavailable);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(GitSyncError::ExecutableUnavailable);
        }
    }
    Ok(canonical)
}

fn discover_sibling_helper() -> Result<PathBuf, GitSyncError> {
    let current = env::current_exe().map_err(|_| GitSyncError::CurrentExecutableUnavailable)?;
    let mut directory = current
        .parent()
        .ok_or(GitSyncError::CurrentExecutableHasNoParent)?;
    if directory.file_name() == Some(OsStr::new("deps")) {
        directory = directory
            .parent()
            .ok_or(GitSyncError::CurrentExecutableHasNoParent)?;
    }
    canonical_executable(directory.join(format!("maincopy-ssh{}", env::consts::EXE_SUFFIX)))
}

fn validate_credential(
    reference: SshCredentialReference,
) -> Result<ResolvedCredential, GitSyncError> {
    let private_key =
        validate_credential_file(reference.private_key.path(), CredentialFileKind::PrivateKey)?;
    let known_hosts =
        validate_credential_file(reference.known_hosts.path(), CredentialFileKind::KnownHosts)?;
    Ok(ResolvedCredential {
        private_key,
        known_hosts,
    })
}

fn verify_credential(credential: &ResolvedCredential) -> Result<(), GitSyncError> {
    verify_credential_file(&credential.private_key, CredentialFileKind::PrivateKey)?;
    verify_credential_file(&credential.known_hosts, CredentialFileKind::KnownHosts)
}

fn validate_credential_file(
    path: &Path,
    kind: CredentialFileKind,
) -> Result<ValidatedCredentialFile, GitSyncError> {
    if !path.is_absolute() {
        return Err(GitSyncError::CredentialUnavailable);
    }
    reject_symlink_components(path).map_err(|_| GitSyncError::CredentialUnavailable)?;
    validate_credential_ancestor_directories(path)?;
    let initial = inspect_credential_file(path, kind)?;
    let canonical = fs::canonicalize(path).map_err(|_| GitSyncError::CredentialUnavailable)?;
    reject_symlink_components(&canonical).map_err(|_| GitSyncError::CredentialUnavailable)?;
    validate_credential_ancestor_directories(&canonical)?;
    let canonical_identity = inspect_credential_file(&canonical, kind)?;
    let opened = fs::File::open(&canonical).map_err(|_| GitSyncError::CredentialUnavailable)?;
    validate_credential_ancestor_directories(&canonical)?;
    let opened_identity = credential_file_identity(
        &opened
            .metadata()
            .map_err(|_| GitSyncError::CredentialUnavailable)?,
        kind,
    )?;
    if canonical_identity != initial || opened_identity != initial {
        return Err(GitSyncError::CredentialUnavailable);
    }
    Ok(ValidatedCredentialFile {
        path: canonical,
        identity: initial,
    })
}

#[cfg(unix)]
fn validate_credential_ancestor_directories(path: &Path) -> Result<(), GitSyncError> {
    use std::os::unix::fs::MetadataExt as _;

    let effective_user = rustix::process::geteuid().as_raw();
    for ancestor in path.ancestors().skip(1) {
        let metadata =
            fs::symlink_metadata(ancestor).map_err(|_| GitSyncError::CredentialUnavailable)?;
        let mode = metadata.mode();
        let owner_is_trusted = metadata.uid() == effective_user || metadata.uid() == 0;
        let writable_by_others = mode & 0o022 != 0;
        let sticky = mode & 0o1000 != 0;
        if !metadata.file_type().is_dir()
            || metadata.file_type().is_symlink()
            || !owner_is_trusted
            || (writable_by_others && !sticky)
        {
            return Err(GitSyncError::CredentialUnavailable);
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_credential_ancestor_directories(path: &Path) -> Result<(), GitSyncError> {
    reject_symlink_components(path).map_err(|_| GitSyncError::CredentialUnavailable)
}

fn verify_credential_file(
    expected: &ValidatedCredentialFile,
    kind: CredentialFileKind,
) -> Result<(), GitSyncError> {
    let current = validate_credential_file(&expected.path, kind)?;
    if current.path != expected.path || current.identity != expected.identity {
        return Err(GitSyncError::CredentialUnavailable);
    }
    Ok(())
}

fn inspect_credential_file(
    path: &Path,
    kind: CredentialFileKind,
) -> Result<CredentialFileIdentity, GitSyncError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| GitSyncError::CredentialUnavailable)?;
    credential_file_identity(&metadata, kind)
}

#[cfg(unix)]
fn credential_file_identity(
    metadata: &fs::Metadata,
    kind: CredentialFileKind,
) -> Result<CredentialFileIdentity, GitSyncError> {
    use std::os::unix::fs::MetadataExt as _;

    let unsafe_permissions = match kind {
        CredentialFileKind::PrivateKey => metadata.mode() & 0o077 != 0,
        CredentialFileKind::KnownHosts => metadata.mode() & 0o022 != 0,
    };
    if !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_CREDENTIAL_FILE_BYTES
        || metadata.nlink() != 1
        || !credential_owner_is_trusted(kind, metadata.uid(), rustix::process::geteuid().as_raw())
        || unsafe_permissions
    {
        return Err(GitSyncError::CredentialUnavailable);
    }
    Ok(CredentialFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        bytes: metadata.len(),
        status_changed_seconds: metadata.ctime(),
        status_changed_nanoseconds: metadata.ctime_nsec(),
    })
}

#[cfg(unix)]
const fn credential_owner_is_trusted(
    kind: CredentialFileKind,
    owner: u32,
    effective_user: u32,
) -> bool {
    match kind {
        CredentialFileKind::PrivateKey => owner == effective_user,
        CredentialFileKind::KnownHosts => owner == effective_user || owner == 0,
    }
}

#[cfg(not(unix))]
fn credential_file_identity(
    metadata: &fs::Metadata,
    _kind: CredentialFileKind,
) -> Result<CredentialFileIdentity, GitSyncError> {
    if !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_CREDENTIAL_FILE_BYTES
    {
        return Err(GitSyncError::CredentialUnavailable);
    }
    Ok(CredentialFileIdentity {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MirrorConfiguration {
    Sha1,
    Sha256,
}

fn validate_mirror_configuration(
    mirror_root: &Path,
    phase: GitCommandPhase,
) -> Result<(), GitSyncError> {
    match fs::symlink_metadata(mirror_root.join(".git")) {
        Ok(_) => return Err(GitSyncError::MirrorConfigurationUnsafe),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(GitSyncError::MirrorConfigurationUnsafe),
    }

    let mirror = mirror_root.join(MIRROR_DIRECTORY);
    let configuration =
        match inspect_mirror_path(&mirror).map_err(|_| GitSyncError::MirrorConfigurationUnsafe)? {
            MirrorState::Absent => None,
            MirrorState::Directory => Some(read_mirror_configuration(&mirror)?),
        };
    let allowed = match phase {
        GitCommandPhase::InitializeMirror => configuration.is_none(),
        GitCommandPhase::DiscoverBranch => true,
        GitCommandPhase::InspectMirror
        | GitCommandPhase::FetchBranch
        | GitCommandPhase::PruneMirror
        | GitCommandPhase::ResolveCommit
        | GitCommandPhase::ListCandidate
        | GitCommandPhase::ReadCandidateBlobs => configuration.is_some(),
    };
    if allowed {
        Ok(())
    } else {
        Err(GitSyncError::MirrorConfigurationUnsafe)
    }
}

fn read_mirror_configuration(mirror: &Path) -> Result<MirrorConfiguration, GitSyncError> {
    let path = mirror.join("config");
    reject_symlink_components(&path).map_err(|_| GitSyncError::MirrorConfigurationUnsafe)?;
    let file = fs::File::open(path).map_err(|_| GitSyncError::MirrorConfigurationUnsafe)?;
    let metadata = file
        .metadata()
        .map_err(|_| GitSyncError::MirrorConfigurationUnsafe)?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_MIRROR_CONFIGURATION_BYTES {
        return Err(GitSyncError::MirrorConfigurationUnsafe);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        if metadata.nlink() != 1
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o022 != 0
        {
            return Err(GitSyncError::MirrorConfigurationUnsafe);
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_MIRROR_CONFIGURATION_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| GitSyncError::MirrorConfigurationUnsafe)?;
    if bytes.len() as u64 > MAX_MIRROR_CONFIGURATION_BYTES {
        return Err(GitSyncError::MirrorConfigurationUnsafe);
    }
    parse_mirror_configuration(&bytes)
}

fn parse_mirror_configuration(bytes: &[u8]) -> Result<MirrorConfiguration, GitSyncError> {
    #[derive(Clone, Copy)]
    enum Section {
        Core,
        Extensions,
    }

    let text = std::str::from_utf8(bytes).map_err(|_| GitSyncError::MirrorConfigurationUnsafe)?;
    let mut section = None;
    let mut repository_format = None;
    let mut file_mode = None;
    let mut bare = None;
    let mut object_format = None;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        section = match line {
            "[core]" => Some(Section::Core),
            "[extensions]" => Some(Section::Extensions),
            _ if line.starts_with('[') => {
                return Err(GitSyncError::MirrorConfigurationUnsafe);
            }
            _ => {
                let (key, value) = line
                    .split_once('=')
                    .ok_or(GitSyncError::MirrorConfigurationUnsafe)?;
                let key = key.trim();
                let value = value.trim();
                let slot = match (section, key) {
                    (Some(Section::Core), "repositoryformatversion") => &mut repository_format,
                    (Some(Section::Core), "filemode") => &mut file_mode,
                    (Some(Section::Core), "bare") => &mut bare,
                    (Some(Section::Extensions), "objectformat") => &mut object_format,
                    _ => return Err(GitSyncError::MirrorConfigurationUnsafe),
                };
                if slot.replace(value).is_some() {
                    return Err(GitSyncError::MirrorConfigurationUnsafe);
                }
                continue;
            }
        };
    }
    match (repository_format, file_mode, bare, object_format) {
        (Some("0"), Some("true"), Some("true"), None) => Ok(MirrorConfiguration::Sha1),
        (Some("1"), Some("true"), Some("true"), Some("sha256")) => Ok(MirrorConfiguration::Sha256),
        _ => Err(GitSyncError::MirrorConfigurationUnsafe),
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MirrorState {
    Absent,
    Directory,
}

fn inspect_mirror_path(path: &Path) -> io::Result<MirrorState> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(MirrorState::Directory),
        Ok(_) => Err(io::Error::other("managed mirror path is unsafe")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(MirrorState::Absent),
        Err(error) => Err(error),
    }
}

fn bounded_tree_bytes(
    root: &Path,
    byte_limit: u64,
    entry_limit: usize,
) -> Result<(), GitSyncError> {
    if entry_limit == 0 {
        return Err(GitSyncError::MirrorEntryLimitExceeded);
    }
    let mut pending = vec![root.to_path_buf()];
    let mut discovered = 1_usize;
    let mut total = 0_u64;
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).map_err(|_| GitSyncError::MirrorUnsafe)?;
        if metadata.file_type().is_symlink() {
            return Err(GitSyncError::MirrorUnsafe);
        }
        total = total
            .checked_add(metadata.len())
            .ok_or(GitSyncError::MirrorByteLimitExceeded)?;
        if total > byte_limit {
            return Err(GitSyncError::MirrorByteLimitExceeded);
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&path).map_err(|_| GitSyncError::MirrorUnsafe)? {
                discovered = discovered
                    .checked_add(1)
                    .ok_or(GitSyncError::MirrorEntryLimitExceeded)?;
                if discovered > entry_limit {
                    return Err(GitSyncError::MirrorEntryLimitExceeded);
                }
                pending.push(entry.map_err(|_| GitSyncError::MirrorUnsafe)?.path());
            }
        } else if !metadata.is_file() {
            return Err(GitSyncError::MirrorUnsafe);
        }
    }
    Ok(())
}

fn parse_advertised_commit(
    output: &[u8],
    expected_reference: &str,
) -> Result<AdvertisedCommit, GitSyncError> {
    let text = std::str::from_utf8(output).map_err(|_| GitSyncError::RemoteAdvertisementInvalid)?;
    let line = text
        .strip_suffix('\n')
        .ok_or(GitSyncError::RemoteAdvertisementInvalid)?;
    if line.contains('\n') || line.contains('\r') {
        return Err(GitSyncError::RemoteAdvertisementInvalid);
    }
    let (hex, reference) = line
        .split_once('\t')
        .ok_or(GitSyncError::RemoteAdvertisementInvalid)?;
    if reference != expected_reference {
        return Err(GitSyncError::RemoteAdvertisementInvalid);
    }
    SourceCommit::from_git_hex(hex)
        .map(AdvertisedCommit)
        .map_err(|_| GitSyncError::RemoteAdvertisementInvalid)
}

fn classify_transport_diagnostic(bytes: &[u8]) -> Option<GitSyncError> {
    if contains_bytes(bytes, b"Host key verification failed")
        || contains_bytes(bytes, b"REMOTE HOST IDENTIFICATION HAS CHANGED")
        || contains_bytes(bytes, b"host key is known")
    {
        return Some(GitSyncError::UnknownHost);
    }
    if contains_bytes(bytes, b"Permission denied (publickey") {
        return Some(GitSyncError::AuthenticationFailed);
    }
    if [
        b"Could not resolve hostname".as_slice(),
        b"Connection timed out".as_slice(),
        b"Connection refused".as_slice(),
        b"No route to host".as_slice(),
        b"Connection closed by".as_slice(),
        b"kex_exchange_identification".as_slice(),
    ]
    .into_iter()
    .any(|needle| contains_bytes(bytes, needle))
    {
        return Some(GitSyncError::RemoteUnavailable);
    }
    None
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn parse_tree_entries(
    output: &[u8],
    commit: &SourceCommit,
    content_subdirectory: &RepositoryContentSubdirectory,
    limits: ContentTreeLimits,
    process_file_limit: u64,
) -> Result<Vec<TreeEntry>, GitSyncError> {
    let mut entries = Vec::new();
    let mut total_bytes = 0_u64;
    let mut directories = BTreeSet::new();
    let listing = if output.is_empty() {
        output
    } else {
        output
            .strip_suffix(&[0])
            .ok_or(GitSyncError::CandidateListingInvalid)?
    };
    for raw_entry in listing.split(|byte| *byte == 0) {
        let Some(entry) = parse_tree_entry(
            raw_entry,
            commit,
            content_subdirectory,
            limits,
            process_file_limit,
        )?
        else {
            continue;
        };
        total_bytes = total_bytes
            .checked_add(entry.size)
            .ok_or(GitSyncError::CandidateLimitExceeded)?;
        if total_bytes > limits.total_tree_bytes.get() {
            return Err(GitSyncError::CandidateLimitExceeded);
        }
        register_parent_directories(&entry.relative_path, &mut directories, limits.depth.get())?;
        entries.push(entry);
        if entries.len().saturating_add(directories.len()) > limits.entries.get() {
            return Err(GitSyncError::CandidateLimitExceeded);
        }
    }
    Ok(entries)
}

fn parse_tree_entry(
    raw_entry: &[u8],
    commit: &SourceCommit,
    content_subdirectory: &RepositoryContentSubdirectory,
    limits: ContentTreeLimits,
    process_file_limit: u64,
) -> Result<Option<TreeEntry>, GitSyncError> {
    let tab = raw_entry
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or(GitSyncError::CandidateListingInvalid)?;
    let (metadata, raw_path) = raw_entry.split_at(tab);
    let raw_path = raw_path
        .get(1..)
        .ok_or(GitSyncError::CandidateListingInvalid)?;
    let metadata =
        std::str::from_utf8(metadata).map_err(|_| GitSyncError::CandidateListingInvalid)?;
    let mut fields = metadata.split_ascii_whitespace();
    let mode = fields.next().ok_or(GitSyncError::CandidateListingInvalid)?;
    let object_type = fields.next().ok_or(GitSyncError::CandidateListingInvalid)?;
    let object = fields.next().ok_or(GitSyncError::CandidateListingInvalid)?;
    let size = fields.next().ok_or(GitSyncError::CandidateListingInvalid)?;
    if fields.next().is_some() {
        return Err(GitSyncError::CandidateListingInvalid);
    }
    let relative_bytes = strip_content_subdirectory_bytes(raw_path, content_subdirectory)?;
    if !is_managed_content_path_bytes(relative_bytes) {
        return Ok(None);
    }
    let relative =
        std::str::from_utf8(relative_bytes).map_err(|_| GitSyncError::CandidatePathInvalid)?;
    validate_portable_path(relative, limits.path_bytes.get())?;
    if !matches!(mode, "100644" | "100755") || object_type != "blob" {
        return Err(GitSyncError::CandidateObjectUnsupported);
    }
    let size = size
        .parse::<u64>()
        .map_err(|_| GitSyncError::CandidateListingInvalid)?;
    validate_object_identifier(object, commit)?;
    let file_limit = content_file_limit(relative, limits).min(process_file_limit);
    if size > file_limit {
        return Err(GitSyncError::CandidateLimitExceeded);
    }
    Ok(Some(TreeEntry {
        object: object.to_owned(),
        size,
        relative_path: relative.to_owned(),
    }))
}

fn validate_object_identifier(object: &str, commit: &SourceCommit) -> Result<(), GitSyncError> {
    let expected = commit.as_bytes().len() * 2;
    if object.len() != expected
        || !object
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(GitSyncError::CandidateListingInvalid);
    }
    Ok(())
}

fn strip_content_subdirectory_bytes<'path>(
    path: &'path [u8],
    subdirectory: &RepositoryContentSubdirectory,
) -> Result<&'path [u8], GitSyncError> {
    if subdirectory.as_str() == "." {
        return Ok(path);
    }
    let prefix = format!("{}/", subdirectory.as_str()).into_bytes();
    path.strip_prefix(prefix.as_slice())
        .filter(|relative| !relative.is_empty())
        .ok_or(GitSyncError::CandidatePathInvalid)
}

fn validate_portable_path(path: &str, maximum_bytes: usize) -> Result<(), GitSyncError> {
    if path.is_empty()
        || path.len() > maximum_bytes
        || path.starts_with(['/', '\\'])
        || path.contains(['%', '\\'])
        || !path.split('/').all(|component| {
            !component.is_empty()
                && !matches!(component, "." | "..")
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return Err(GitSyncError::CandidatePathInvalid);
    }
    Ok(())
}

fn is_managed_content_path_bytes(path: &[u8]) -> bool {
    let first = path.split(|byte| *byte == b'/').next().unwrap_or_default();
    first.eq_ignore_ascii_case(b"publication.toml")
        || first.eq_ignore_ascii_case(b"posts")
        || first.eq_ignore_ascii_case(b"drafts")
        || first.eq_ignore_ascii_case(b"assets")
}

fn content_file_limit(path: &str, limits: ContentTreeLimits) -> u64 {
    if path.eq_ignore_ascii_case("publication.toml") {
        limits.publication_file_bytes.get()
    } else if path
        .split('/')
        .next()
        .is_some_and(|root| root.eq_ignore_ascii_case("assets"))
    {
        limits.asset_file_bytes.get()
    } else {
        limits.post_file_bytes.get()
    }
}

fn register_parent_directories(
    path: &str,
    directories: &mut BTreeSet<String>,
    maximum_depth: usize,
) -> Result<(), GitSyncError> {
    let components = path.split('/').collect::<Vec<_>>();
    let parent_depth = components.len().saturating_sub(1);
    if parent_depth > maximum_depth {
        return Err(GitSyncError::CandidateLimitExceeded);
    }
    for depth in 1..=parent_depth {
        directories.insert(components[..depth].join("/"));
    }
    Ok(())
}

fn prepare_candidate_directories(root: &Path, entries: &[TreeEntry]) -> Result<(), GitSyncError> {
    for entry in entries {
        let path = root.join(&entry.relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|_| GitSyncError::CandidateWorkspaceUnavailable)?;
        }
    }
    Ok(())
}

async fn materialize_batch_output(
    stdout: impl AsyncRead + Unpin,
    root: PathBuf,
    entries: Vec<TreeEntry>,
) -> Result<(), GitSyncError> {
    let mut reader = BufReader::new(stdout);
    let mut header = Vec::new();
    for entry in entries {
        header.clear();
        (&mut reader)
            .take((MAX_BATCH_HEADER_BYTES + 1) as u64)
            .read_until(b'\n', &mut header)
            .await
            .map_err(|_| GitSyncError::CommandOutputFailed)?;
        if header.len() > MAX_BATCH_HEADER_BYTES || header.last() != Some(&b'\n') {
            return Err(GitSyncError::CandidateBlobStreamInvalid);
        }
        let header_text = std::str::from_utf8(&header[..header.len() - 1])
            .map_err(|_| GitSyncError::CandidateBlobStreamInvalid)?;
        let expected = format!("{} blob {}", entry.object, entry.size);
        if header_text != expected {
            return Err(GitSyncError::CandidateBlobStreamInvalid);
        }
        let path = root.join(&entry.relative_path);
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .await
            .map_err(|_| GitSyncError::CandidateWriteFailed)?;
        let copied = tokio::io::copy(&mut (&mut reader).take(entry.size), &mut file)
            .await
            .map_err(|_| GitSyncError::CandidateWriteFailed)?;
        if copied != entry.size {
            return Err(GitSyncError::CandidateBlobStreamInvalid);
        }
        file.flush()
            .await
            .map_err(|_| GitSyncError::CandidateWriteFailed)?;
        let mut delimiter = [0_u8; 1];
        reader
            .read_exact(&mut delimiter)
            .await
            .map_err(|_| GitSyncError::CandidateBlobStreamInvalid)?;
        if delimiter != *b"\n" {
            return Err(GitSyncError::CandidateBlobStreamInvalid);
        }
    }
    let mut trailing = [0_u8; 1];
    if reader
        .read(&mut trailing)
        .await
        .map_err(|_| GitSyncError::CommandOutputFailed)?
        != 0
    {
        return Err(GitSyncError::CandidateBlobStreamInvalid);
    }
    Ok(())
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    remaining: Arc<AtomicU64>,
) -> Result<Vec<u8>, GitSyncError> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|_| GitSyncError::CommandOutputFailed)?;
        if read == 0 {
            return Ok(bytes);
        }
        let read = read as u64;
        let reserved = remaining.fetch_update(Ordering::AcqRel, Ordering::Acquire, |available| {
            available.checked_sub(read)
        });
        if reserved.is_err() {
            return Err(GitSyncError::CommandOutputLimitExceeded);
        }
        bytes.extend_from_slice(&buffer[..read as usize]);
    }
}

#[derive(Clone, Copy)]
struct ChildProcessGroup(u32);

impl ChildProcessGroup {
    fn capture(child: &Child) -> Option<Self> {
        // `process_group(0)` makes the spawned child's PID its process-group ID.
        // Capture it before `wait`, because Tokio clears `Child::id` after reap.
        child.id().map(Self)
    }

    fn kill(self) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(pid) = i32::try_from(self.0).ok().and_then(Pid::from_raw) {
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }
}

async fn supervise_child_output<Output>(
    child: &mut Child,
    process_group: ChildProcessGroup,
    output: impl Future<Output = Result<Output, GitSyncError>>,
    wall_time: Duration,
    cancellation: &CancellationToken,
    phase: GitCommandPhase,
) -> Result<(ExitStatus, Output), GitSyncError> {
    enum ProcessEvent<Output> {
        Output(Result<Output, GitSyncError>),
        Status(io::Result<ExitStatus>),
        Cancelled,
        TimedOut,
    }

    tokio::pin!(output);
    let timeout = tokio::time::sleep(wall_time);
    tokio::pin!(timeout);
    let mut completed_output = None;
    let mut completed_status = None;

    loop {
        let event = {
            let wait = child.wait();
            tokio::pin!(wait);
            tokio::select! {
                biased;
                () = cancellation.cancelled() => ProcessEvent::Cancelled,
                () = &mut timeout => ProcessEvent::TimedOut,
                result = &mut output, if completed_output.is_none() => {
                    ProcessEvent::Output(result)
                }
                status = &mut wait, if completed_status.is_none() => {
                    ProcessEvent::Status(status)
                }
            }
        };

        match event {
            ProcessEvent::Output(Ok(output)) => {
                if let Some(status) = completed_status.take() {
                    return Ok((status, output));
                }
                completed_output = Some(output);
            }
            ProcessEvent::Status(Ok(status)) => {
                if let Some(output) = completed_output.take() {
                    return Ok((status, output));
                }
                completed_status = Some(status);
            }
            ProcessEvent::Output(Err(error)) => {
                terminate_and_reap(child, process_group).await;
                return Err(error);
            }
            ProcessEvent::Status(Err(_)) => {
                terminate_and_reap(child, process_group).await;
                return Err(GitSyncError::CommandWaitFailed);
            }
            ProcessEvent::Cancelled => {
                terminate_and_reap(child, process_group).await;
                return Err(GitSyncError::Cancelled);
            }
            ProcessEvent::TimedOut => {
                terminate_and_reap(child, process_group).await;
                return Err(GitSyncError::TimedOut(phase));
            }
        }
    }
}

async fn terminate_and_reap(child: &mut Child, process_group: ChildProcessGroup) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    process_group.kill();
    let _ = child.start_kill();
    let _ = child.wait().await;
}

fn apply_process_policy(
    command: &mut Command,
    limits: GitProcessLimits,
) -> Result<(), GitSyncError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::process::CommandExt as _;

        command.as_std_mut().process_group(0);
        // SAFETY: `Command` owns this closure until `spawn`; it captures only
        // `Copy` limit values, so it has no borrowed state or release duty. After
        // `fork` and before `exec`, it calls only async-signal-safe `setrlimit`
        // syscalls. A syscall error aborts the child before the Git image starts,
        // and successful limits belong to that child and its descendants only.
        unsafe {
            command.as_std_mut().pre_exec(move || {
                install_limit(Resource::Core, 0)?;
                install_limit(
                    Resource::Fsize,
                    limits.file_bytes.get().min(limits.mirror_bytes.get()),
                )?;
                install_limit(Resource::As, limits.address_space_bytes.get())?;
                install_limit(Resource::Cpu, limits.cpu_seconds.get())?;
                install_limit(Resource::Nofile, limits.open_files.get())?;
                Ok(())
            });
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (command, limits);
        Err(GitSyncError::UnsupportedPlatform)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn install_limit(resource: Resource, requested: u64) -> io::Result<()> {
    let current = getrlimit(resource);
    let effective = current
        .maximum
        .map_or(requested, |hard| hard.min(requested));
    setrlimit(
        resource,
        Rlimit {
            current: Some(effective),
            maximum: Some(effective),
        },
    )
    .map_err(io::Error::from)
}

fn require_supported_platform() -> Result<(), GitSyncError> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        Err(GitSyncError::UnsupportedPlatform)
    }
}

fn os_arguments<const LENGTH: usize>(values: [&str; LENGTH]) -> Vec<OsString> {
    values.into_iter().map(OsString::from).collect()
}

fn parse_ascii_line_output(bytes: &[u8]) -> Result<&str, GitSyncError> {
    let text = std::str::from_utf8(bytes).map_err(|_| GitSyncError::CommitInvalid)?;
    text.strip_suffix('\n').ok_or(GitSyncError::CommitInvalid)
}

fn parse_one_ascii_token(bytes: &[u8]) -> Result<&str, GitSyncError> {
    let value = parse_ascii_line_output(bytes)?;
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(GitSyncError::CommitInvalid);
    }
    Ok(value)
}

fn commit_hex(commit: &SourceCommit) -> &str {
    commit
        .as_str()
        .split_once(':')
        .map(|(_, hex)| hex)
        .expect("validated source commits contain one algorithm prefix")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use maincopy_shared::{
        auth::{AdminAuditEventId, InstanceId, UserId},
        source::{
            SourcePollInterval, SourceSyncAdmission, SourceSyncFailureCode, SourceSyncOutcome,
            SourceSyncRequestOrigin, SourceSyncStage, SshRemoteHost, SshRemotePort, SshRemoteUser,
            SshRepositoryPath,
        },
    };

    #[cfg(target_os = "linux")]
    use crate::{
        config::{
            DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
            DatabaseWriterQueueCapacity, SecretFileReference,
        },
        content_sync::PreparedContentCandidate,
        database::{self, store::DatabaseStore},
        domain::{
            auth::{
                NostrPublicKey,
                store::{
                    AdminMutationKey, AuditPrincipalReference, BootstrapIdentity,
                    ConfiguredLoginProviders, MutationAuditContext, NewHumanCredential,
                },
            },
            publication::{
                PublicLedgerProjection,
                activation::{
                    PublicationCoordinator, PublicationCoordinatorActor,
                    PublicationCoordinatorHandle, PublishNow, observed_post_revisions,
                },
                store::InstallStartupSnapshot,
            },
            source::{
                ManagedSourceConfigurationInput,
                store::{
                    PutSourceConfiguration, SourceStore, StoredSourceConfiguration,
                    StoredSourceSync,
                },
            },
        },
        frontend_assets::embedded_manifest,
        render::{
            ContentCompiler, SiteSnapshotReader, build_site_snapshot, render_bound_post_preview,
            render_site_shell, snapshot_store,
        },
        source_sync::{ManagedSourceEngine, ManagedSourceSyncError},
        web::Readiness,
    };

    #[cfg(target_os = "linux")]
    use std::{os::unix::fs::PermissionsExt as _, process::Command as StdCommand};

    #[cfg(target_os = "linux")]
    use markdown_compiler::{ContentCandidateStore, PostId};
    #[cfg(target_os = "linux")]
    use time::OffsetDateTime;
    #[cfg(target_os = "linux")]
    use tokio::{sync::Notify, task::JoinHandle};
    #[cfg(target_os = "linux")]
    use uuid::Uuid;

    #[cfg(target_os = "linux")]
    const OWNER_NOSTR_KEY: &str =
        "63fe6318dc58583cfe16810f86dd09e18bfd76aabc24a0081ce2856f330504ed";

    #[cfg(target_os = "linux")]
    struct ManagedSourceFixture {
        _root: tempfile::TempDir,
        work: PathBuf,
        owner: UserId,
        store: DatabaseStore,
        configuration: StoredSourceConfiguration,
        git: GitSync,
        candidates: ContentCandidateStore,
        compiler: ContentCompiler,
        cancellation: CancellationToken,
        writer_shutdown: CancellationToken,
        writer_task: JoinHandle<()>,
    }

    #[cfg(target_os = "linux")]
    impl ManagedSourceFixture {
        async fn start() -> Self {
            let root = tempfile::tempdir().unwrap();
            let work = root.path().join("work");
            run_fixture_git(root.path(), ["init", "--initial-branch=main", "work"]);
            run_fixture_git(&work, ["config", "user.name", "Maincopy test"]);
            run_fixture_git(&work, ["config", "user.email", "test@example.test"]);
            write_valid_site(&work, "Initial managed post", "Initial managed body.");
            commit_fixture(&work, "initial managed content");

            let remote_path = root.path().join("remote.git");
            run_fixture_git(
                root.path(),
                [
                    "clone",
                    "--bare",
                    work.to_str().unwrap(),
                    remote_path.to_str().unwrap(),
                ],
            );
            run_fixture_git(
                &work,
                ["remote", "add", "origin", remote_path.to_str().unwrap()],
            );

            let git_executable = discover_executable(GIT_EXECUTABLE_ENV, "git").unwrap();
            let upload_pack = PathBuf::from(fixture_git_output(root.path(), ["--exec-path"]))
                .join("git-upload-pack");
            let helper = root.path().join("fixture-ssh");
            fs::write(
                &helper,
                format!(
                    "#!/bin/sh\nexec \"{}\" \"$MAINCOPY_SSH_EXPECTED_REPOSITORY\"\n",
                    upload_pack.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
            let private_key = root.path().join("key");
            let known_hosts = root.path().join("known-hosts");
            fs::write(&private_key, "fixture key\n").unwrap();
            fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600)).unwrap();
            fs::write(&known_hosts, "fixture host\n").unwrap();
            let credential_name = SshCredentialName::parse("deploy").unwrap();
            let credentials = BTreeMap::from([(
                credential_name.clone(),
                SshCredentialReference {
                    private_key: SecretFileReference::new(private_key).unwrap(),
                    known_hosts: SecretFileReference::new(known_hosts).unwrap(),
                },
            )]);
            let git = GitSync::from_executables(
                root.path().join("mirror"),
                credentials,
                GitProcessLimits::default(),
                ContentTreeLimits::default(),
                git_executable.clone(),
                git_executable,
                fs::canonicalize(helper).unwrap(),
            )
            .unwrap();

            let state = root.path().join("state");
            let database_path = state.join("maincopy.db");
            let database = database::bootstrap(database_configuration(&database_path))
                .await
                .unwrap();
            let (store, writer) = database.into_store(16);
            let writer_shutdown = CancellationToken::new();
            let task_shutdown = writer_shutdown.clone();
            let writer_task = tokio::spawn(async move {
                writer.run(task_shutdown).await.unwrap();
            });
            let owner = UserId::from_uuid(Uuid::from_u128(2));
            store
                .auth
                .bootstrap_identity(BootstrapIdentity {
                    instance_id: InstanceId::from_uuid(Uuid::from_u128(1)),
                    owner_user_id: owner,
                    credential: NewHumanCredential::Nostr {
                        public_key: NostrPublicKey::parse(OWNER_NOSTR_KEY).unwrap(),
                    },
                    configured_providers: ConfiguredLoginProviders::new(false, true).unwrap(),
                    occurred_at: fixture_time(1),
                    audit_event_id: AdminAuditEventId::from_uuid(Uuid::from_u128(3)),
                })
                .await
                .unwrap();
            let configuration = store
                .source
                .put_configuration(PutSourceConfiguration {
                    request: ManagedSourceConfigurationInput {
                        remote: SshRemote {
                            user: SshRemoteUser::parse("git").unwrap(),
                            host: SshRemoteHost::parse("fixture.test").unwrap(),
                            port: SshRemotePort::new(22).unwrap(),
                            repository_path: SshRepositoryPath::parse(
                                remote_path.to_str().unwrap(),
                            )
                            .unwrap(),
                        },
                        branch: GitBranchName::parse("main").unwrap(),
                        content_subdirectory: subdirectory("site"),
                        credential_name,
                        poll_interval_seconds: SourcePollInterval::from_seconds(60).unwrap(),
                        expected_version: None,
                    },
                    occurred_at: fixture_time(2),
                    audit: MutationAuditContext {
                        audit_event_id: AdminAuditEventId::from_uuid(Uuid::from_u128(4)),
                        principal: AuditPrincipalReference::Offline {
                            user_id: Some(owner),
                        },
                        request_id: Some(Uuid::from_u128(5)),
                        idempotency_key: AdminMutationKey(Uuid::from_u128(6)),
                    },
                })
                .await
                .unwrap();
            let candidates =
                ContentCandidateStore::open(&state, ContentTreeLimits::default()).unwrap();

            Self {
                _root: root,
                work,
                owner,
                store,
                configuration,
                git,
                candidates,
                compiler: ContentCompiler::discover().unwrap(),
                cancellation: CancellationToken::new(),
                writer_shutdown,
                writer_task,
            }
        }

        async fn prepare(&self) -> Result<PreparedContentCandidate, ManagedSourceSyncError> {
            let (engine, _handle) = ManagedSourceEngine::new(
                self.store.source.clone(),
                self.configuration.clone(),
                self.git.clone(),
                self.candidates.clone(),
                self.compiler.clone(),
                self.cancellation.clone(),
            );
            engine.prepare_startup().await
        }

        fn commit_valid_revision(&self) {
            write_valid_site(&self.work, "Updated managed post", "Updated managed body.");
            commit_fixture(&self.work, "update managed content");
            run_fixture_git(&self.work, ["push", "origin", "main"]);
        }

        fn commit_invalid_revision(&self) {
            fs::write(
                self.work.join("site/posts/managed.md"),
                "invalid post without required metadata\n",
            )
            .unwrap();
            commit_fixture(&self.work, "break managed content");
            run_fixture_git(&self.work, ["push", "origin", "main"]);
        }

        async fn stop(self) {
            self.cancellation.cancel();
            drop(self.store);
            self.writer_shutdown.cancel();
            self.writer_task.await.unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    struct PublicationFixture {
        handle: PublicationCoordinatorHandle,
        actor: PublicationCoordinatorActor,
        snapshots: SiteSnapshotReader,
        cancellation: CancellationToken,
    }

    #[cfg(target_os = "linux")]
    impl PublicationFixture {
        async fn start(
            fixture: &ManagedSourceFixture,
            candidate: &PreparedContentCandidate,
        ) -> Self {
            let source_commit = candidate.source_commit.clone().unwrap();
            let ledger = PublicLedgerProjection::empty();
            let shell =
                render_site_shell(Arc::clone(&candidate.catalog), embedded_manifest(), &ledger)
                    .unwrap();
            let snapshot = build_site_snapshot(shell, &ledger).unwrap();
            let site = fixture
                .store
                .publications
                .install_startup_snapshot(InstallStartupSnapshot {
                    expected: None,
                    candidate_digest: snapshot.digest.clone(),
                    activated_at: OffsetDateTime::now_utc(),
                    source_commit: Some(source_commit.clone()),
                    posts: observed_post_revisions(&candidate.catalog),
                })
                .await
                .unwrap();
            let (snapshots, activator) = snapshot_store(snapshot);
            let cancellation = CancellationToken::new();
            let coordinator = PublicationCoordinator {
                catalog: Arc::clone(&candidate.catalog),
                content_digest: candidate.content_digest.clone(),
                candidates: Arc::new(BTreeMap::from([(
                    candidate.content_digest.clone(),
                    Arc::clone(&candidate.catalog),
                )])),
                ledger,
                site,
                activator,
                store: fixture.store.publications.clone(),
                profiles: fixture.store.profiles.clone(),
                tip_recipient: None,
                frontend: embedded_manifest(),
                source_commit: Some(source_commit),
                scheduled: BTreeMap::new(),
                scheduler_wakeup: Arc::new(Notify::new()),
                readiness: Readiness::new(true),
                cancellation: cancellation.clone(),
            };
            let (handle, actor) = coordinator.into_actor(4);
            Self {
                handle,
                actor,
                snapshots,
                cancellation,
            }
        }
    }

    fn commit() -> SourceCommit {
        SourceCommit::parse(&format!("git-sha1:{}", "ab".repeat(20))).unwrap()
    }

    fn subdirectory(value: &str) -> RepositoryContentSubdirectory {
        RepositoryContentSubdirectory::parse(value).unwrap()
    }

    #[test]
    fn advertisement_requires_one_exact_branch_and_typed_hash() {
        let expected = "refs/heads/main";
        let hash = "ab".repeat(20);
        let parsed =
            parse_advertised_commit(format!("{hash}\t{expected}\n").as_bytes(), expected).unwrap();
        assert_eq!(parsed.0, commit());

        for invalid in [
            format!("{hash}\trefs/heads/other\n"),
            format!("{hash}\t{expected}"),
            format!("{hash}\t{expected}\n{hash}\t{expected}\n"),
            format!("{}\t{expected}\n", "zz".repeat(20)),
        ] {
            assert!(parse_advertised_commit(invalid.as_bytes(), expected).is_err());
        }
    }

    #[test]
    fn tree_listing_strips_only_the_exact_content_root_and_enforces_objects() {
        let hash = "cd".repeat(20);
        let listing = format!(
            "100644 blob {hash} 7\tsite/publication.toml\0\
             100644 blob {hash} 11\tsite/posts/hello.md\0\
             100644 blob {hash} 5\tsite/README.md\0"
        );
        let entries = parse_tree_entries(
            listing.as_bytes(),
            &commit(),
            &subdirectory("site"),
            ContentTreeLimits::default(),
            u64::MAX,
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].relative_path, "publication.toml");
        assert_eq!(entries[1].relative_path, "posts/hello.md");

        let symlink = format!("120000 blob {hash} 4\tsite/assets/link\0");
        assert!(matches!(
            parse_tree_entries(
                symlink.as_bytes(),
                &commit(),
                &subdirectory("site"),
                ContentTreeLimits::default(),
                u64::MAX,
            ),
            Err(GitSyncError::CandidateObjectUnsupported)
        ));
        let gitlink = format!("160000 commit {hash} -\tsite/assets/vendor\0");
        assert!(matches!(
            parse_tree_entries(
                gitlink.as_bytes(),
                &commit(),
                &subdirectory("site"),
                ContentTreeLimits::default(),
                u64::MAX,
            ),
            Err(GitSyncError::CandidateListingInvalid)
                | Err(GitSyncError::CandidateObjectUnsupported)
        ));
    }

    #[test]
    fn candidate_paths_share_the_compiler_portable_grammar() {
        for accepted in [
            "publication.toml",
            "posts/hello-world.md",
            "assets/images/cover_2.png",
        ] {
            assert!(validate_portable_path(accepted, 1_024).is_ok());
        }
        for rejected in [
            "../publication.toml",
            "/publication.toml",
            "posts//hello.md",
            "posts/%2e%2e/secret",
            "posts\\hello.md",
            "posts/café.md",
        ] {
            assert!(validate_portable_path(rejected, 1_024).is_err());
        }
    }

    #[test]
    fn ssh_diagnostics_collapse_to_safe_failure_classes() {
        for (diagnostic, expected) in [
            (
                b"Host key verification failed".as_slice(),
                SourceSyncFailureCode::UnknownHost,
            ),
            (
                b"Permission denied (publickey).".as_slice(),
                SourceSyncFailureCode::AuthenticationFailed,
            ),
            (
                b"ssh: Could not resolve hostname example.test".as_slice(),
                SourceSyncFailureCode::RemoteUnavailable,
            ),
        ] {
            let error = classify_transport_diagnostic(diagnostic).unwrap();
            assert_eq!(error.failure_code(), expected);
            assert!(!format!("{error:?} {error}").contains("example.test"));
        }
        assert!(classify_transport_diagnostic(b"arbitrary remote text").is_none());
    }

    #[test]
    #[cfg(unix)]
    fn credential_validation_rejects_writable_known_hosts() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let known_hosts = root.path().join("known-hosts");
        fs::write(&known_hosts, "fixture host\n").unwrap();
        fs::set_permissions(&known_hosts, fs::Permissions::from_mode(0o622)).unwrap();
        assert!(matches!(
            validate_credential_file(&known_hosts, CredentialFileKind::KnownHosts),
            Err(GitSyncError::CredentialUnavailable)
        ));
    }

    #[test]
    #[cfg(unix)]
    fn credential_ownership_accepts_root_only_for_the_public_trust_anchor() {
        assert!(credential_owner_is_trusted(
            CredentialFileKind::KnownHosts,
            0,
            1_000,
        ));
        assert!(!credential_owner_is_trusted(
            CredentialFileKind::PrivateKey,
            0,
            1_000,
        ));
        assert!(credential_owner_is_trusted(
            CredentialFileKind::PrivateKey,
            1_000,
            1_000,
        ));
    }

    #[test]
    #[cfg(unix)]
    fn credential_validation_rejects_hardlinks() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let private_key = root.path().join("key");
        fs::write(&private_key, "fixture key\n").unwrap();
        fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600)).unwrap();
        let hardlink = root.path().join("key-hardlink");
        fs::hard_link(&private_key, &hardlink).unwrap();
        assert!(matches!(
            validate_credential_file(&private_key, CredentialFileKind::PrivateKey),
            Err(GitSyncError::CredentialUnavailable)
        ));
    }

    #[test]
    #[cfg(unix)]
    fn credential_validation_rejects_a_final_symlink() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = tempfile::tempdir().unwrap();
        let private_key = root.path().join("key");
        fs::write(&private_key, "fixture key\n").unwrap();
        fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600)).unwrap();
        let final_symlink = root.path().join("key-symlink");
        symlink(&private_key, &final_symlink).unwrap();
        assert!(matches!(
            validate_credential_file(&final_symlink, CredentialFileKind::PrivateKey),
            Err(GitSyncError::CredentialUnavailable)
        ));
    }

    #[test]
    #[cfg(unix)]
    fn credential_validation_rejects_a_symlinked_parent_component() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = tempfile::tempdir().unwrap();
        let real_parent = root.path().join("real");
        fs::create_dir(&real_parent).unwrap();
        let private_key = real_parent.join("key");
        fs::write(&private_key, "fixture key\n").unwrap();
        fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600)).unwrap();
        let linked_parent = root.path().join("linked");
        symlink(&real_parent, &linked_parent).unwrap();

        assert!(matches!(
            validate_credential_file(&linked_parent.join("key"), CredentialFileKind::PrivateKey),
            Err(GitSyncError::CredentialUnavailable)
        ));
    }

    #[test]
    #[cfg(unix)]
    fn credential_validation_rejects_a_writable_non_sticky_parent() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let writable_parent = root.path().join("writable");
        fs::create_dir(&writable_parent).unwrap();
        fs::set_permissions(&writable_parent, fs::Permissions::from_mode(0o770)).unwrap();
        let private_key = writable_parent.join("key");
        fs::write(&private_key, "fixture key\n").unwrap();
        fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(matches!(
            validate_credential_file(&private_key, CredentialFileKind::PrivateKey),
            Err(GitSyncError::CredentialUnavailable)
        ));
    }

    #[test]
    #[cfg(unix)]
    fn credential_identity_change_is_rejected_before_the_next_transport() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let private_key = root.path().join("key");
        fs::write(&private_key, "first value\n").unwrap();
        fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600)).unwrap();
        let validated =
            validate_credential_file(&private_key, CredentialFileKind::PrivateKey).unwrap();

        let replacement = root.path().join("replacement");
        fs::write(&replacement, "other value\n").unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();
        fs::rename(replacement, private_key).unwrap();

        assert!(matches!(
            verify_credential_file(&validated, CredentialFileKind::PrivateKey),
            Err(GitSyncError::CredentialUnavailable)
        ));
    }

    #[test]
    fn mirror_scanner_rejects_links_byte_overflow_and_entry_overflow() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("one"), [0_u8; 16]).unwrap();
        fs::write(root.path().join("two"), []).unwrap();
        assert!(bounded_tree_bytes(root.path(), 1024 * 1024, 3).is_ok());
        assert!(matches!(
            bounded_tree_bytes(root.path(), 1, 3),
            Err(GitSyncError::MirrorByteLimitExceeded)
        ));
        assert!(matches!(
            bounded_tree_bytes(root.path(), 1024 * 1024, 2),
            Err(GitSyncError::MirrorEntryLimitExceeded)
        ));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("one", root.path().join("link")).unwrap();
            assert!(matches!(
                bounded_tree_bytes(root.path(), 1024 * 1024, 4),
                Err(GitSyncError::MirrorUnsafe)
            ));
        }
    }

    #[test]
    fn mirror_configuration_accepts_only_the_git_init_contract() {
        let sha1 = b"[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n";
        let sha256 = b"[extensions]\n\tobjectformat = sha256\n[core]\n\trepositoryformatversion = 1\n\tfilemode = true\n\tbare = true\n";
        assert!(matches!(
            parse_mirror_configuration(sha1),
            Ok(MirrorConfiguration::Sha1)
        ));
        assert!(matches!(
            parse_mirror_configuration(sha256),
            Ok(MirrorConfiguration::Sha256)
        ));

        for poisoned in [
            "[include]\npath = /tmp/poisoned\n",
            "[url \"file:///tmp/bypass\"]\ninsteadOf = git@example.test:\n",
            "[core]\nsshCommand = /tmp/bypass-ssh\n",
            "[core]\nhooksPath = /tmp/bypass-hooks\n",
            "[protocol]\nallow = always\n",
            "[core]\nrepositoryformatversion = 0\nfilemode = true\nbare = true\nbare = true\n",
        ] {
            assert!(matches!(
                parse_mirror_configuration(poisoned.as_bytes()),
                Err(GitSyncError::MirrorConfigurationUnsafe)
            ));
        }
    }

    #[tokio::test]
    async fn batch_stream_writes_only_declared_blob_bytes() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("posts")).unwrap();
        let object = "cd".repeat(20);
        let entries = vec![TreeEntry {
            object: object.clone(),
            size: 5,
            relative_path: "posts/test.md".to_owned(),
        }];
        let stream = format!("{object} blob 5\nhello\n");
        materialize_batch_output(stream.as_bytes(), root.path().to_path_buf(), entries)
            .await
            .unwrap();
        assert_eq!(
            fs::read(root.path().join("posts/test.md")).unwrap(),
            b"hello"
        );
    }

    #[tokio::test]
    async fn batch_stream_rejects_an_oversized_header_without_buffering_for_a_newline() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("posts")).unwrap();
        let entries = vec![TreeEntry {
            object: "cd".repeat(20),
            size: 1,
            relative_path: "posts/test.md".to_owned(),
        }];
        let stream = vec![b'x'; MAX_BATCH_HEADER_BYTES * 4];

        assert!(matches!(
            materialize_batch_output(stream.as_slice(), root.path().to_path_buf(), entries).await,
            Err(GitSyncError::CandidateBlobStreamInvalid)
        ));
        assert!(!root.path().join("posts/test.md").exists());
    }

    #[tokio::test]
    async fn cancellation_wins_before_admission_or_credential_resolution() {
        let root = tempfile::tempdir().unwrap();
        let sync = GitSync::from_executables(
            root.path().join("mirror"),
            BTreeMap::new(),
            GitProcessLimits::default(),
            ContentTreeLimits::default(),
            root.path().join("git"),
            root.path().join("ssh"),
            root.path().join("helper"),
        )
        .unwrap();
        let remote = SshRemote {
            user: SshRemoteUser::parse("git").unwrap(),
            host: SshRemoteHost::parse("fixture.test").unwrap(),
            port: SshRemotePort::new(22).unwrap(),
            repository_path: SshRepositoryPath::parse("publisher/site.git").unwrap(),
        };
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let result = sync
            .synchronize(
                &remote,
                &GitBranchName::parse("main").unwrap(),
                &subdirectory("site"),
                &SshCredentialName::parse("missing").unwrap(),
                None,
                &cancellation,
            )
            .await;

        assert!(matches!(result, Err(GitSyncError::Cancelled)));
    }

    #[test]
    fn a_managed_mirror_has_one_process_owner() {
        let root = tempfile::tempdir().unwrap();
        let mirror = root.path().join("mirror");
        let executable = root.path().join("executable");
        let first = GitSync::from_executables(
            mirror.clone(),
            BTreeMap::new(),
            GitProcessLimits::default(),
            ContentTreeLimits::default(),
            executable.clone(),
            executable.clone(),
            executable.clone(),
        )
        .unwrap();

        assert!(matches!(
            GitSync::from_executables(
                mirror.clone(),
                BTreeMap::new(),
                GitProcessLimits::default(),
                ContentTreeLimits::default(),
                executable.clone(),
                executable.clone(),
                executable,
            ),
            Err(GitSyncError::MirrorUnavailable)
        ));

        drop(first);
        assert!(
            GitSync::from_executables(
                mirror,
                BTreeMap::new(),
                GitProcessLimits::default(),
                ContentTreeLimits::default(),
                root.path().join("git"),
                root.path().join("ssh"),
                root.path().join("helper"),
            )
            .is_ok()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn managed_git_disables_hooks_and_rejects_transport_configuration_bypasses() {
        let fixture = ManagedSourceFixture::start().await;
        fixture.prepare().await.unwrap();
        let mirror = fixture.git.mirror_path();
        let hook_marker = fixture._root.path().join("hook-ran");
        let hook = mirror.join("hooks/reference-transaction");
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::write(
            &hook,
            format!("#!/bin/sh\nprintf invoked > '{}'\n", hook_marker.display()),
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();

        fixture.commit_valid_revision();
        fixture.prepare().await.unwrap();
        assert!(!hook_marker.exists(), "the repository hook executed");

        let bypass_marker = fixture._root.path().join("bypass-ssh-ran");
        let bypass = fixture._root.path().join("bypass-ssh");
        fs::write(
            &bypass,
            format!(
                "#!/bin/sh\nprintf invoked > '{}'\nexit 1\n",
                bypass_marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&bypass, fs::Permissions::from_mode(0o700)).unwrap();
        let included = fixture._root.path().join("included-git-config");
        fs::write(&included, "[protocol]\nallow = always\n").unwrap();
        let configuration_path = mirror.join("config");
        let canonical = fs::read_to_string(&configuration_path).unwrap();
        let configured = &fixture.configuration.configuration;
        let poisons = [
            format!("[include]\npath = {}\n", included.display()),
            format!("[core]\nsshCommand = {}\n", bypass.display()),
            format!(
                "[url \"file://{}\"]\ninsteadOf = git@fixture.test:\n",
                fixture._root.path().display()
            ),
            format!("[core]\nhooksPath = {}\n", mirror.join("hooks").display()),
            "[protocol]\nallow = always\n".to_owned(),
        ];
        for poison in poisons {
            fs::write(&configuration_path, format!("{canonical}{poison}")).unwrap();
            let result = fixture
                .git
                .synchronize(
                    &configured.remote,
                    &configured.branch,
                    &configured.content_subdirectory,
                    &configured.credential_name,
                    None,
                    &fixture.cancellation,
                )
                .await;
            assert!(matches!(
                result,
                Err(GitSyncError::MirrorConfigurationUnsafe)
            ));
            assert!(
                !bypass_marker.exists(),
                "the configured SSH command executed"
            );
            assert!(!hook_marker.exists(), "the configured hook executed");
        }
        fs::write(configuration_path, canonical).unwrap();
        fixture.stop().await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn managed_source_engine_applies_changes_handles_no_change_and_preserves_last_good() {
        let fixture = ManagedSourceFixture::start().await;

        let initial_commit = fixture_commit(&fixture.work);
        let initial = fixture.prepare().await.unwrap();
        assert_eq!(initial.source_commit.as_ref(), Some(&initial_commit));
        assert_only_candidate_title(&initial, "Initial managed post");
        let initial_installation = fixture.store.source.installation().await.unwrap().unwrap();
        assert_eq!(initial_installation.source_commit, initial_commit);
        assert_eq!(initial_installation.content_digest, initial.content_digest);
        assert_eq!(
            fixture
                .store
                .source
                .status()
                .await
                .unwrap()
                .latest_sync
                .unwrap()
                .outcome,
            Some(SourceSyncOutcome::Applied)
        );

        let unchanged = fixture.prepare().await.unwrap();
        assert_eq!(unchanged.source_commit, initial.source_commit);
        assert_eq!(unchanged.content_digest, initial.content_digest);
        assert_only_candidate_title(&unchanged, "Initial managed post");
        let unchanged_installation = fixture.store.source.installation().await.unwrap().unwrap();
        assert_eq!(
            unchanged_installation.source_commit,
            initial_installation.source_commit
        );
        assert_eq!(
            unchanged_installation.content_digest,
            initial_installation.content_digest
        );
        assert_ne!(
            unchanged_installation.source_sync_id,
            initial_installation.source_sync_id
        );
        let unchanged_sync = fixture
            .store
            .source
            .status()
            .await
            .unwrap()
            .latest_sync
            .unwrap();
        assert_eq!(unchanged_sync.outcome, Some(SourceSyncOutcome::NoChange));
        assert_eq!(
            unchanged_sync.source_sync_id,
            unchanged_installation.source_sync_id
        );

        fixture.commit_valid_revision();
        let updated_commit = fixture_commit(&fixture.work);
        let updated = fixture.prepare().await.unwrap();
        assert_eq!(updated.source_commit.as_ref(), Some(&updated_commit));
        assert_only_candidate_title(&updated, "Updated managed post");
        assert_ne!(updated.content_digest, initial.content_digest);
        let last_good = fixture.store.source.installation().await.unwrap().unwrap();
        assert_eq!(last_good.source_commit, updated_commit);
        assert_eq!(last_good.content_digest, updated.content_digest);
        assert_eq!(fixture.candidates.load_all().unwrap().len(), 2);

        fixture.commit_invalid_revision();
        let rejected_commit = fixture_commit(&fixture.work);
        assert!(matches!(
            fixture.prepare().await,
            Err(ManagedSourceSyncError::OperationFailed {
                code: SourceSyncFailureCode::ValidationFailed,
                ..
            })
        ));
        let after_rejection = fixture.store.source.installation().await.unwrap().unwrap();
        assert_eq!(after_rejection, last_good);
        assert_eq!(fixture.candidates.load_all().unwrap().len(), 2);
        let rejected = fixture
            .store
            .source
            .status()
            .await
            .unwrap()
            .latest_sync
            .unwrap();
        assert_eq!(rejected.outcome, Some(SourceSyncOutcome::Failed));
        assert_eq!(
            rejected.failure_code,
            Some(SourceSyncFailureCode::ValidationFailed)
        );
        assert_eq!(rejected.source_commit, Some(rejected_commit));
        assert!(fixture.store.source.active_sync().await.unwrap().is_none());

        fixture.stop().await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn live_poll_updates_only_private_preview_until_existing_publish_flow_promotes_it() {
        let fixture = ManagedSourceFixture::start().await;
        let startup_candidate = fixture.prepare().await.unwrap();
        let PublicationFixture {
            handle: publications,
            actor: publication_actor,
            snapshots,
            cancellation: publication_cancellation,
        } = PublicationFixture::start(&fixture, &startup_candidate).await;
        let publication_task =
            tokio::spawn(publication_actor.run(publication_cancellation.clone()));
        let post_id = PostId::parse("11111111-1111-4111-8111-111111111111").unwrap();
        let initial_projection = publications.read();
        let initial_post = initial_projection.catalog.current_post(&post_id).unwrap();
        let initial_revision = initial_post.revision.clone();
        let slug = initial_post.document.metadata.slug.clone();
        let initial_preview = render_bound_post_preview(
            &initial_projection.catalog,
            embedded_manifest(),
            &post_id,
            None,
            "/api/admin/v1/preview-assets/reproduce",
            None,
        )
        .unwrap()
        .unwrap();
        assert!(initial_preview.html.contains("Initial managed body."));
        drop(initial_projection);

        let initially_published = publications
            .publish_now(PublishNow {
                creation_key: Uuid::from_u128(1_000),
                publication_id: Uuid::from_u128(1_001),
                stable_post_id: post_id.clone(),
                expected_revision: None,
                accepted_preview_digest: initial_preview.digest,
            })
            .await
            .unwrap();
        assert_eq!(initially_published.revision, initial_revision);
        let initial_public_html = snapshots.load_full().post_page(&slug).unwrap();
        assert!(initial_public_html.contains("Initial managed body."));
        assert!(!initial_public_html.contains("Updated managed body."));

        let (engine, source) = ManagedSourceEngine::new(
            fixture.store.source.clone(),
            fixture.configuration.clone(),
            fixture.git.clone(),
            fixture.candidates.clone(),
            fixture.compiler.clone(),
            fixture.cancellation.clone(),
        );
        let source_task = tokio::spawn(engine.into_live(publications.clone()).run());
        let armed = source
            .begin_manual(mutation_audit(fixture.owner, 30))
            .await
            .unwrap();
        let armed_sync_id = armed.sync.source_sync_id;
        let armed = wait_for_terminal_source_sync(&fixture.store.source, |sync| {
            sync.source_sync_id == armed_sync_id
        })
        .await;
        assert_eq!(armed.outcome, Some(SourceSyncOutcome::NoChange));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        fixture.commit_valid_revision();
        let pushed_commit = fixture_commit(&fixture.work);
        let next_poll_at = fixture
            .store
            .source
            .configuration()
            .await
            .unwrap()
            .unwrap()
            .next_poll_at
            .unwrap();
        let wall_delay = (next_poll_at - OffsetDateTime::now_utc())
            .try_into()
            .unwrap_or(std::time::Duration::ZERO);
        tokio::time::pause();
        tokio::time::advance(wall_delay + std::time::Duration::from_secs(1)).await;
        tokio::time::resume();

        let polled = wait_for_terminal_source_sync(&fixture.store.source, |sync| {
            sync.source_sync_id != armed_sync_id
        })
        .await;
        assert_eq!(polled.request_origin, SourceSyncRequestOrigin::Poll);
        assert_eq!(polled.outcome, Some(SourceSyncOutcome::Applied));
        assert_eq!(polled.source_commit.as_ref(), Some(&pushed_commit));
        let polled_digest = polled.content_digest.clone().unwrap();
        let installation = fixture.store.source.installation().await.unwrap().unwrap();
        assert_eq!(installation.source_commit, pushed_commit);
        assert_eq!(installation.content_digest, polled_digest);
        let private_projection = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let projection = publications.read();
                if projection.content_digest == polled_digest {
                    break projection;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let updated_post = private_projection.catalog.current_post(&post_id).unwrap();
        let updated_revision = updated_post.revision.clone();
        assert_ne!(updated_revision, initial_revision);
        assert_eq!(
            updated_post.document.metadata.title.as_str(),
            "Updated managed post"
        );
        let published = private_projection.ledger.published_post(&post_id).unwrap();
        assert_eq!(published.revision, initial_revision);
        let updated_preview = render_bound_post_preview(
            &private_projection.catalog,
            embedded_manifest(),
            &post_id,
            None,
            "/api/admin/v1/preview-assets/reproduce",
            Some(published.published_at),
        )
        .unwrap()
        .unwrap();
        assert!(updated_preview.html.contains("Updated managed body."));
        assert_eq!(
            snapshots.load_full().post_page(&slug).unwrap(),
            initial_public_html
        );
        drop(private_projection);

        let promoted = publications
            .publish_now(PublishNow {
                creation_key: Uuid::from_u128(1_002),
                publication_id: Uuid::from_u128(1_003),
                stable_post_id: post_id,
                expected_revision: Some(updated_revision.clone()),
                accepted_preview_digest: updated_preview.digest,
            })
            .await
            .unwrap();
        assert_eq!(promoted.revision, updated_revision);
        let promoted_html = snapshots.load_full().post_page(&slug).unwrap();
        assert!(promoted_html.contains("Updated managed body."));
        assert!(!promoted_html.contains("Initial managed body."));

        fixture.cancellation.cancel();
        source_task.await.unwrap().unwrap();
        drop(source);
        publication_cancellation.cancel();
        drop(publications);
        publication_task.await.unwrap().unwrap();
        fixture.stop().await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn managed_source_shutdown_terminalizes_an_admitted_queued_operation() {
        let fixture = ManagedSourceFixture::start().await;
        let candidate = fixture.prepare().await.unwrap();
        let publication = PublicationFixture::start(&fixture, &candidate).await;
        let (engine, source) = ManagedSourceEngine::new(
            fixture.store.source.clone(),
            fixture.configuration.clone(),
            fixture.git.clone(),
            fixture.candidates.clone(),
            fixture.compiler.clone(),
            fixture.cancellation.clone(),
        );
        let admitted = source
            .begin_manual(mutation_audit(fixture.owner, 20))
            .await
            .unwrap();
        assert_eq!(admitted.admission, SourceSyncAdmission::Created);
        assert_eq!(admitted.sync.stage, SourceSyncStage::Queued);
        assert_eq!(admitted.sync.outcome, None);

        fixture.cancellation.cancel();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            engine.into_live(publication.handle).run(),
        )
        .await
        .unwrap()
        .unwrap();

        let cancelled = fixture
            .store
            .source
            .sync(admitted.sync.source_sync_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cancelled.stage, SourceSyncStage::Queued);
        assert_eq!(cancelled.outcome, Some(SourceSyncOutcome::Cancelled));
        assert!(fixture.store.source.active_sync().await.unwrap().is_none());

        drop(source);
        drop(publication.actor);
        fixture.stop().await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn managed_sync_fetches_only_one_branch_and_materializes_one_subdirectory() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        run_fixture_git(root.path(), ["init", "--initial-branch=main", "work"]);
        run_fixture_git(&work, ["config", "user.name", "Maincopy test"]);
        run_fixture_git(&work, ["config", "user.email", "test@example.test"]);
        fs::create_dir_all(work.join("site/posts")).unwrap();
        fs::write(
            work.join("site/publication.toml"),
            "[site]\ntitle = \"Managed\"\nbase_url = \"https://example.test\"\n\
             [author]\nname = \"Test\"\n",
        )
        .unwrap();
        fs::write(work.join("site/posts/hello.md"), "# Managed source\n").unwrap();
        fs::write(work.join("outside.txt"), "not content\n").unwrap();
        run_fixture_git(&work, ["add", "."]);
        run_fixture_git(&work, ["commit", "-m", "main content"]);
        let excluded_ancestor = fixture_git_output(&work, ["rev-parse", "HEAD"]);
        fs::write(work.join("outside.txt"), "new non-content revision\n").unwrap();
        commit_fixture(&work, "main tip");

        run_fixture_git(&work, ["checkout", "-b", "ignored"]);
        fs::write(work.join("ignored-only.txt"), "must not be fetched\n").unwrap();
        run_fixture_git(&work, ["add", "ignored-only.txt"]);
        run_fixture_git(&work, ["commit", "-m", "ignored content"]);
        let ignored_commit = fixture_git_output(&work, ["rev-parse", "HEAD"]);
        run_fixture_git(&work, ["checkout", "main"]);

        let remote_path = root.path().join("remote.git");
        run_fixture_git(
            root.path(),
            [
                "clone",
                "--bare",
                work.to_str().unwrap(),
                remote_path.to_str().unwrap(),
            ],
        );
        run_fixture_git(
            &work,
            ["remote", "add", "origin", remote_path.to_str().unwrap()],
        );
        let git = discover_executable(GIT_EXECUTABLE_ENV, "git").unwrap();
        let git_exec_path = fixture_git_output(root.path(), ["--exec-path"]);
        let upload_pack = PathBuf::from(git_exec_path).join("git-upload-pack");
        let helper = root.path().join("fixture-ssh");
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\nexec \"{}\" \"$MAINCOPY_SSH_EXPECTED_REPOSITORY\"\n",
                upload_pack.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let private_key = root.path().join("key");
        let known_hosts = root.path().join("known-hosts");
        fs::write(&private_key, "fixture key\n").unwrap();
        fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&known_hosts, "fixture host\n").unwrap();
        let credential_name = SshCredentialName::parse("deploy").unwrap();
        let credentials = BTreeMap::from([(
            credential_name.clone(),
            SshCredentialReference {
                private_key: SecretFileReference::new(private_key).unwrap(),
                known_hosts: SecretFileReference::new(known_hosts).unwrap(),
            },
        )]);
        let mirror_root = root.path().join("mirror");
        let sync = GitSync::from_executables(
            mirror_root.clone(),
            credentials,
            GitProcessLimits::default(),
            ContentTreeLimits::default(),
            git.clone(),
            git,
            fs::canonicalize(helper).unwrap(),
        )
        .unwrap();
        let remote = SshRemote {
            user: SshRemoteUser::parse("git").unwrap(),
            host: SshRemoteHost::parse("fixture.test").unwrap(),
            port: SshRemotePort::new(22).unwrap(),
            repository_path: SshRepositoryPath::parse(remote_path.to_str().unwrap()).unwrap(),
        };
        let branch = GitBranchName::parse("main").unwrap();
        let cancellation = CancellationToken::new();

        let candidate = match sync
            .synchronize(
                &remote,
                &branch,
                &subdirectory("site"),
                &credential_name,
                None,
                &cancellation,
            )
            .await
            .unwrap()
        {
            GitSyncOutcome::Candidate(candidate) => candidate,
            GitSyncOutcome::NoChange { .. } => panic!("first fetch must produce a candidate"),
        };
        assert!(candidate.tree.publication.source.contains("Managed"));
        assert_eq!(candidate.tree.posts.len(), 1);
        let ignored_present = StdCommand::new("git")
            .args([
                "--git-dir",
                sync.mirror_path().to_str().unwrap(),
                "cat-file",
                "-e",
                ignored_commit.as_str(),
            ])
            .status()
            .unwrap()
            .success();
        assert!(
            !ignored_present,
            "the unconfigured branch entered the mirror"
        );
        let ancestor_present = StdCommand::new("git")
            .args([
                "--git-dir",
                sync.mirror_path().to_str().unwrap(),
                "cat-file",
                "-e",
                excluded_ancestor.as_str(),
            ])
            .status()
            .unwrap()
            .success();
        assert!(
            !ancestor_present,
            "history beyond depth one entered the mirror"
        );
        assert_eq!(
            fixture_git_output(
                &sync.mirror_path(),
                ["rev-list", "--count", STAGING_REFERENCE]
            ),
            "1"
        );
        assert_eq!(
            fixture_git_output(
                &sync.mirror_path(),
                ["for-each-ref", "--format=%(refname)", "refs/maincopy"]
            ),
            STAGING_REFERENCE
        );

        let second = sync
            .synchronize(
                &remote,
                &branch,
                &subdirectory("site"),
                &credential_name,
                Some(&candidate.source_commit),
                &cancellation,
            )
            .await
            .unwrap();
        assert!(matches!(second, GitSyncOutcome::NoChange { .. }));

        let previous_head = commit_hex(&candidate.source_commit).to_owned();
        let tree = fixture_git_output(&work, ["write-tree"]);
        let replacement = fixture_git_output(
            &work,
            ["commit-tree", tree.as_str(), "-m", "force-moved main"],
        );
        run_fixture_git(&work, ["reset", "--hard", replacement.as_str()]);
        run_fixture_git(&work, ["push", "--force", "origin", "main"]);
        let replacement_candidate = match sync
            .synchronize(
                &remote,
                &branch,
                &subdirectory("site"),
                &credential_name,
                Some(&candidate.source_commit),
                &cancellation,
            )
            .await
            .unwrap()
        {
            GitSyncOutcome::Candidate(candidate) => candidate,
            GitSyncOutcome::NoChange { .. } => panic!("the force-moved head must be a candidate"),
        };
        assert_eq!(
            commit_hex(&replacement_candidate.source_commit),
            replacement
        );
        let previous_head_present = StdCommand::new("git")
            .args([
                "--git-dir",
                sync.mirror_path().to_str().unwrap(),
                "cat-file",
                "-e",
                previous_head.as_str(),
            ])
            .status()
            .unwrap()
            .success();
        assert!(
            !previous_head_present,
            "the unreachable prior head survived mirror maintenance"
        );
        assert_eq!(
            fixture_git_output(
                &sync.mirror_path(),
                ["for-each-ref", "--format=%(refname)", "refs/maincopy"]
            ),
            STAGING_REFERENCE
        );
    }

    #[cfg(target_os = "linux")]
    fn write_valid_site(work: &Path, title: &str, body: &str) {
        fs::create_dir_all(work.join("site/posts")).unwrap();
        fs::write(
            work.join("site/publication.toml"),
            "[site]\n\
             title = \"Managed source tests\"\n\
             base_url = \"https://example.test/\"\n\
             description = \"Managed source coordinator tests.\"\n\
             [author]\n\
             name = \"Example Author\"\n\
             [assets]\n\
             allowed_https_origins = []\n",
        )
        .unwrap();
        fs::write(
            work.join("site/posts/managed.md"),
            format!(
                "+++\n\
                 id = \"11111111-1111-4111-8111-111111111111\"\n\
                 title = {title:?}\n\
                 slug = \"managed\"\n\
                 authored_at = 2026-09-04T12:00:00Z\n\
                 description = \"Managed source coordinator fixture.\"\n\
                 draft = false\n\
                 +++\n\
                 {body}\n"
            ),
        )
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    fn commit_fixture(work: &Path, message: &str) {
        run_fixture_git(work, ["add", "."]);
        run_fixture_git(work, ["commit", "-m", message]);
    }

    #[cfg(target_os = "linux")]
    fn fixture_commit(work: &Path) -> SourceCommit {
        let hex = fixture_git_output(work, ["rev-parse", "HEAD"]);
        SourceCommit::parse(&format!("git-sha1:{hex}")).unwrap()
    }

    #[cfg(target_os = "linux")]
    fn assert_only_candidate_title(candidate: &PreparedContentCandidate, expected: &str) {
        let mut posts = candidate.catalog.rendered_posts();
        assert_eq!(
            posts.next().unwrap().document.metadata.title.as_str(),
            expected
        );
        assert!(posts.next().is_none());
    }

    #[cfg(target_os = "linux")]
    fn database_configuration(path: &Path) -> DatabaseConfigurationView<'_> {
        DatabaseConfigurationView {
            path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1_000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(16).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        }
    }

    #[cfg(target_os = "linux")]
    fn fixture_time(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    #[cfg(target_os = "linux")]
    fn mutation_audit(owner: UserId, discriminator: u128) -> MutationAuditContext {
        MutationAuditContext {
            audit_event_id: AdminAuditEventId::from_uuid(Uuid::from_u128(100 + discriminator)),
            principal: AuditPrincipalReference::Offline {
                user_id: Some(owner),
            },
            request_id: Some(Uuid::from_u128(200 + discriminator)),
            idempotency_key: AdminMutationKey(Uuid::from_u128(300 + discriminator)),
        }
    }

    #[cfg(target_os = "linux")]
    async fn wait_for_terminal_source_sync(
        source: &SourceStore,
        matches_operation: impl Fn(&StoredSourceSync) -> bool,
    ) -> StoredSourceSync {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(sync) = source.status().await.unwrap().latest_sync
                    && sync.outcome.is_some()
                    && matches_operation(&sync)
                {
                    break sync;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[cfg(target_os = "linux")]
    fn run_fixture_git<const LENGTH: usize>(directory: &Path, arguments: [&str; LENGTH]) {
        let status = StdCommand::new("git")
            .current_dir(directory)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(target_os = "linux")]
    fn fixture_git_output<const LENGTH: usize>(
        directory: &Path,
        arguments: [&str; LENGTH],
    ) -> String {
        let output = StdCommand::new("git")
            .current_dir(directory)
            .args(arguments)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}
