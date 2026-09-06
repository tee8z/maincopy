use std::{path::PathBuf, sync::Arc, time::Duration};

use markdown_compiler::{
    AssetResolutionErrors, ContentCandidateStore, ContentCandidateStoreError, ContentTreeDigest,
    ContentTreeLimits, ContentValidationErrors, DiscoveredContentTree, PrepareContentError,
    discover_content_tree, prepare_content,
};
use thiserror::Error;
use tokio::{task::JoinError, time::MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::{
    database::store::{DatabaseAdmissionError, DatabaseCommandError, DatabaseMutationError},
    domain::publication::{
        SourceCommit,
        activation::{
            ContentReloadError, PublicationCoordinatorHandle, PublicationCoordinatorUnavailable,
        },
    },
    render::{CatalogBuildError, ContentCatalog, ContentCompiler},
    source_provenance::{SourceCommitDiscovery, discover_source_commit},
};

const CONTENT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// One immutable content-tree candidate prepared from an exact source commit.
pub(crate) struct PreparedContentCandidate {
    pub(crate) catalog: Arc<ContentCatalog>,
    pub(crate) content_digest: ContentTreeDigest,
    pub(crate) source_commit: SourceCommit,
}

/// Validates, compiles, and retains one already stable source candidate.
///
/// Managed source synchronization calls this boundary only after it resolves
/// an exact commit and owns all candidate bytes. External checkout mode keeps
/// its repeated-observation checks below because that tree can change in place.
pub(crate) async fn prepare_immutable_candidate(
    tree: DiscoveredContentTree,
    source_commit: SourceCommit,
    candidate_store: ContentCandidateStore,
    compiler: ContentCompiler,
) -> Result<PreparedContentCandidate, ContentCandidatePreparationError> {
    tokio::task::spawn_blocking(move || {
        let content_digest = tree.digest();
        let content = prepare_content(&tree).map_err(|error| match error {
            PrepareContentError::InvalidContent(source) => {
                ContentCandidatePreparationError::Validate(source)
            }
            PrepareContentError::AssetResolution(source) => {
                ContentCandidatePreparationError::ResolveAssets(source)
            }
        })?;
        let catalog = compiler
            .compile(&content)
            .map(Arc::new)
            .map_err(ContentCandidatePreparationError::Compile)?;
        let retained = candidate_store
            .retain(&tree)
            .map_err(ContentCandidatePreparationError::Retention)?;
        if retained != content_digest {
            return Err(ContentCandidatePreparationError::RetainedDigestMismatch {
                expected: content_digest,
                retained,
            });
        }
        Ok(PreparedContentCandidate {
            catalog,
            content_digest,
            source_commit,
        })
    })
    .await
    .map_err(ContentCandidatePreparationError::Worker)?
}

#[derive(Debug, Error)]
pub(crate) enum ContentCandidatePreparationError {
    #[error("a blocking content-candidate worker failed")]
    Worker(#[source] JoinError),
    #[error("the immutable content candidate is invalid")]
    Validate(#[source] ContentValidationErrors),
    #[error("the immutable content candidate contains invalid asset references")]
    ResolveAssets(#[source] AssetResolutionErrors),
    #[error("the immutable content candidate could not be compiled")]
    Compile(#[source] CatalogBuildError),
    #[error("the immutable content candidate could not be retained durably")]
    Retention(#[source] ContentCandidateStoreError),
    #[error("retained content digest {retained} did not match candidate digest {expected}")]
    RetainedDigestMismatch {
        expected: ContentTreeDigest,
        retained: ContentTreeDigest,
    },
}

/// Polls the managed content tree and installs stable, valid changes in-process.
pub(crate) struct ContentSync {
    root: PathBuf,
    limits: ContentTreeLimits,
    candidate_store: ContentCandidateStore,
    active: ContentTreeDigest,
    publications: PublicationCoordinatorHandle,
    cancellation: CancellationToken,
    compiler: ContentCompiler,
}

impl ContentSync {
    pub(crate) fn new(
        root: PathBuf,
        limits: ContentTreeLimits,
        candidate_store: ContentCandidateStore,
        active_digest: ContentTreeDigest,
        publications: PublicationCoordinatorHandle,
        cancellation: CancellationToken,
        compiler: ContentCompiler,
    ) -> Self {
        Self {
            root,
            limits,
            candidate_store,
            active: active_digest,
            publications,
            cancellation,
            compiler,
        }
    }

    pub(crate) async fn run(mut self) -> Result<(), ContentSyncError> {
        let mut interval = tokio::time::interval(CONTENT_POLL_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut state = ContentSyncState::default();

        loop {
            tokio::select! {
                biased;
                () = self.cancellation.cancelled() => return Ok(()),
                _ = interval.tick() => {}
            }
            if self.synchronize(&mut state).await? == SyncControl::Stop {
                return Ok(());
            }
        }
    }

    async fn synchronize(
        &mut self,
        state: &mut ContentSyncState,
    ) -> Result<SyncControl, ContentSyncError> {
        let observed = match observe_tree(self.root.clone(), self.limits).await? {
            Ok(observed) => observed,
            Err(message) => {
                state.discovery_failed(message);
                return Ok(SyncControl::Continue);
            }
        };
        let Some(observed) = state.select_observed(observed, &self.active) else {
            return Ok(SyncControl::Continue);
        };
        let Some(candidate) = self.prepare_candidate(observed, state).await? else {
            return Ok(SyncControl::Continue);
        };
        let Some(candidate) = self.confirm_activation_candidate(candidate, state).await? else {
            return Ok(SyncControl::Continue);
        };
        self.activate_candidate(candidate, state).await
    }

    async fn prepare_candidate(
        &self,
        observed: ObservedCandidate,
        state: &mut ContentSyncState,
    ) -> Result<Option<CompiledCandidate>, ContentSyncError> {
        if let Some(candidate) = state
            .retained
            .take()
            .filter(|candidate| candidate.digest == observed.digest)
        {
            return Ok(Some(candidate));
        }
        self.compile_repeated_candidate(observed, state).await
    }

    async fn compile_repeated_candidate(
        &self,
        observed: ObservedCandidate,
        state: &mut ContentSyncState,
    ) -> Result<Option<CompiledCandidate>, ContentSyncError> {
        if state.pending.as_ref() != Some(&observed.digest) {
            state.pending = Some(observed.digest);
            return Ok(None);
        }
        let compiled = compile_observed(observed, self.root.clone(), self.compiler.clone()).await?;
        let Some(candidate) = state.accept_compiled(compiled) else {
            return Ok(None);
        };
        let Some(candidate) = self.confirm_compiled_candidate(candidate, state).await? else {
            return Ok(None);
        };
        retain_candidate(candidate, self.candidate_store.clone())
            .await
            .map(Some)
    }

    async fn confirm_compiled_candidate(
        &self,
        candidate: CompiledCandidate,
        state: &mut ContentSyncState,
    ) -> Result<Option<CompiledCandidate>, ContentSyncError> {
        let confirmed = match observe_tree(self.root.clone(), self.limits).await? {
            Ok(confirmed) => confirmed,
            Err(message) => {
                tracing::warn!(
                    error = %message,
                    "content changed during compilation; the last good snapshot remains active"
                );
                state.pending = None;
                return Ok(None);
            }
        };
        if confirmed.digest != candidate.digest {
            state.pending = Some(confirmed.digest);
            return Ok(None);
        }
        Ok(Some(candidate))
    }

    async fn confirm_activation_candidate(
        &self,
        candidate: CompiledCandidate,
        state: &mut ContentSyncState,
    ) -> Result<Option<CompiledCandidate>, ContentSyncError> {
        let confirmed = match observe_tree(self.root.clone(), self.limits).await? {
            Ok(confirmed) => confirmed,
            Err(message) => {
                tracing::warn!(
                    error = %message,
                    "content changed while awaiting activation; the last good snapshot remains active"
                );
                state.pending = None;
                state.retained = Some(candidate);
                return Ok(None);
            }
        };
        if confirmed.digest != candidate.digest {
            state.pending = Some(confirmed.digest);
            return Ok(None);
        }
        Ok(Some(candidate))
    }

    async fn activate_candidate(
        &mut self,
        candidate: CompiledCandidate,
        state: &mut ContentSyncState,
    ) -> Result<SyncControl, ContentSyncError> {
        // Once the bounded actor accepts this command, wait for its durable
        // outcome even if process cancellation arrives concurrently.
        let result = self
            .publications
            .apply_content_catalog(
                Arc::clone(&candidate.catalog),
                candidate.digest.clone(),
                candidate.source_commit.clone(),
            )
            .await;
        match result {
            Ok(site) => {
                tracing::info!(
                    content_etag = %candidate.digest,
                    site_etag = %site.digest,
                    site_version = site.version,
                    "live content snapshot synchronized"
                );
                self.active = candidate.digest;
                state.pending = None;
                state.rejected = None;
                Ok(SyncControl::Continue)
            }
            Err(error) => state.handle_reload_failure(error, candidate, &self.cancellation),
        }
    }
}

#[derive(Default)]
struct ContentSyncState {
    pending: Option<ContentTreeDigest>,
    retained: Option<CompiledCandidate>,
    rejected: Option<ContentTreeDigest>,
    last_discovery_error: Option<Box<str>>,
}

impl ContentSyncState {
    fn discovery_failed(&mut self, message: Box<str>) {
        self.pending = None;
        if self.last_discovery_error.as_deref() != Some(message.as_ref()) {
            tracing::warn!(error = %message, "content sync kept the last good snapshot");
            self.last_discovery_error = Some(message);
        }
    }

    fn select_observed(
        &mut self,
        observed: ObservedCandidate,
        active: &ContentTreeDigest,
    ) -> Option<ObservedCandidate> {
        self.last_discovery_error = None;
        if &observed.digest == active {
            self.pending = None;
            self.retained = None;
            self.rejected = None;
            return None;
        }
        if self.rejected.as_ref() == Some(&observed.digest) {
            self.pending = None;
            self.retained = None;
            return None;
        }
        Some(observed)
    }

    fn accept_compiled(
        &mut self,
        compiled: Result<CompiledCandidate, CandidateFailure>,
    ) -> Option<CompiledCandidate> {
        match compiled {
            Ok(candidate) => Some(candidate),
            Err(failure) => {
                tracing::warn!(
                    content_etag = %failure.digest,
                    error = %failure.message,
                    "content sync rejected a compiler candidate and kept the last good snapshot"
                );
                self.rejected = Some(failure.digest);
                self.pending = None;
                None
            }
        }
    }

    fn handle_reload_failure(
        &mut self,
        error: ContentReloadError,
        candidate: CompiledCandidate,
        cancellation: &CancellationToken,
    ) -> Result<SyncControl, ContentSyncError> {
        if reload_is_retryable(&error) {
            tracing::warn!(error = %error, "content sync will retry the stable candidate");
            self.pending = None;
            self.retained = Some(candidate);
            return Ok(SyncControl::Continue);
        }
        if closed_during_cancellation(&error, cancellation) {
            return Ok(SyncControl::Stop);
        }
        if reload_is_fatal(&error) {
            return Err(ContentSyncError::Reload(error));
        }
        if cancellation.is_cancelled() {
            return Ok(SyncControl::Stop);
        }
        tracing::warn!(
            content_etag = %candidate.digest,
            error = %error,
            "content sync rejected a candidate and kept the last good snapshot"
        );
        self.rejected = Some(candidate.digest);
        self.pending = None;
        Ok(SyncControl::Continue)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyncControl {
    Continue,
    Stop,
}

async fn retain_candidate(
    candidate: CompiledCandidate,
    store: ContentCandidateStore,
) -> Result<CompiledCandidate, ContentSyncError> {
    let expected = candidate.digest.clone();
    let (candidate, retained) = tokio::task::spawn_blocking(move || {
        let retained = store.retain(&candidate.tree);
        (candidate, retained)
    })
    .await
    .map_err(ContentSyncError::Worker)?;
    let retained = retained.map_err(ContentSyncError::Retention)?;
    if retained != expected {
        return Err(ContentSyncError::RetainedDigestMismatch { expected, retained });
    }
    Ok(candidate)
}

struct ObservedCandidate {
    digest: ContentTreeDigest,
    tree: DiscoveredContentTree,
}

struct CompiledCandidate {
    digest: ContentTreeDigest,
    tree: DiscoveredContentTree,
    catalog: Arc<ContentCatalog>,
    source_commit: Option<SourceCommit>,
}

struct CandidateFailure {
    digest: ContentTreeDigest,
    message: Box<str>,
}

async fn observe_tree(
    root: PathBuf,
    limits: ContentTreeLimits,
) -> Result<Result<ObservedCandidate, Box<str>>, ContentSyncError> {
    tokio::task::spawn_blocking(move || {
        let tree = discover_content_tree(&root, limits)
            .map_err(|error| Box::<str>::from(error.to_string()))?;
        Ok(ObservedCandidate {
            digest: tree.digest(),
            tree,
        })
    })
    .await
    .map_err(ContentSyncError::Worker)
}

async fn compile_observed(
    observed: ObservedCandidate,
    root: PathBuf,
    compiler: ContentCompiler,
) -> Result<Result<CompiledCandidate, CandidateFailure>, ContentSyncError> {
    tokio::task::spawn_blocking(move || {
        let digest = observed.digest.clone();
        let compiled: Result<(Arc<ContentCatalog>, Option<SourceCommit>), String> = (|| {
            let content = prepare_content(&observed.tree).map_err(|error| error.to_string())?;
            let catalog = compiler
                .compile(&content)
                .map(Arc::new)
                .map_err(|error| error.to_string())?;
            let source_commit = match discover_source_commit(&root) {
                SourceCommitDiscovery::Discovered(commit) => Some(commit),
                SourceCommitDiscovery::Unavailable(_) => None,
            };
            Ok((catalog, source_commit))
        })();
        match compiled {
            Ok((catalog, source_commit)) => Ok(CompiledCandidate {
                digest,
                tree: observed.tree,
                catalog,
                source_commit,
            }),
            Err(message) => Err(CandidateFailure {
                digest,
                message: message.into_boxed_str(),
            }),
        }
    })
    .await
    .map_err(ContentSyncError::Worker)
}

fn reload_is_retryable(error: &ContentReloadError) -> bool {
    matches!(
        error,
        ContentReloadError::Database(DatabaseMutationError::Admission(
            DatabaseAdmissionError::QueueFull
        ))
    )
}

fn closed_during_cancellation(
    error: &ContentReloadError,
    cancellation: &CancellationToken,
) -> bool {
    cancellation.is_cancelled()
        && matches!(
            error,
            ContentReloadError::Coordinator(PublicationCoordinatorUnavailable::Closed)
        )
}

fn reload_is_fatal(error: &ContentReloadError) -> bool {
    matches!(
        error,
        ContentReloadError::Coordinator(_)
            | ContentReloadError::Database(DatabaseMutationError::Admission(
                DatabaseAdmissionError::WriterClosed
            ))
            | ContentReloadError::Database(DatabaseMutationError::Command(
                DatabaseCommandError::OutcomeUnknown
            ))
    )
}

#[derive(Debug, Error)]
pub(crate) enum ContentSyncError {
    #[error("a blocking content-sync worker failed")]
    Worker(#[source] JoinError),
    #[error("a stable content candidate could not be retained durably")]
    Retention(#[source] ContentCandidateStoreError),
    #[error("retained content digest {retained} did not match observed digest {expected}")]
    RetainedDigestMismatch {
        expected: ContentTreeDigest,
        retained: ContentTreeDigest,
    },
    #[error("live content reload entered an uncertain state")]
    Reload(#[source] ContentReloadError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use markdown_compiler::{PostCollection, PostId, PostRevisionDigest};

    use crate::content_fixtures::{content_tree, post, publication};
    use crate::render::compile_content_catalog;

    fn compiled_candidate() -> CompiledCandidate {
        let publication_source = "[site]\n\
            title = \"Content sync tests\"\n\
            base_url = \"https://example.test/\"\n\
            description = \"Content sync retention tests.\"\n\
            [author]\n\
            name = \"Example Author\"\n\
            [assets]\n\
            allowed_https_origins = []\n"
            .to_owned();
        let post_source = "+++\n\
            id = \"11111111-1111-4111-8111-111111111111\"\n\
            title = \"Retained post\"\n\
            slug = \"retained-post\"\n\
            authored_at = 2026-08-30T12:00:00Z\n\
            description = \"Retained candidate fixture.\"\n\
            draft = false\n\
            +++\n\
            Exact retained Markdown body.\n"
            .to_owned();
        let total_bytes = (publication_source.len() + post_source.len()) as u64;
        let tree = content_tree(
            publication("publication.toml", publication_source),
            vec![post(
                "posts/retained-post.md",
                PostCollection::Posts,
                post_source,
            )],
            Vec::new(),
            total_bytes,
        );
        let content = prepare_content(&tree).unwrap();
        let catalog = Arc::new(compile_content_catalog(&content).unwrap());
        CompiledCandidate {
            digest: tree.digest(),
            tree,
            catalog,
            source_commit: None,
        }
    }

    fn retention_reload_error(candidate: &CompiledCandidate) -> ContentReloadError {
        let mut catalog = candidate.catalog.as_ref().clone();
        let missing_revision = (
            PostId::parse("11111111-1111-4111-8111-111111111111").unwrap(),
            PostRevisionDigest::from_bytes([0x77; 32]),
        );
        catalog
            .retain_revisions_from(&candidate.catalog, std::iter::once(missing_revision))
            .unwrap_err()
            .into()
    }

    #[tokio::test]
    async fn immutable_candidate_preparation_binds_commit_and_retains_exact_tree() {
        let state = tempfile::tempdir().unwrap();
        let store =
            ContentCandidateStore::open(state.path(), ContentTreeLimits::default()).unwrap();
        let tree = compiled_candidate().tree;
        let expected_digest = tree.digest();
        let commit = SourceCommit::parse(&format!("git-sha1:{}", "42".repeat(20))).unwrap();

        let prepared = prepare_immutable_candidate(
            tree.clone(),
            commit.clone(),
            store.clone(),
            ContentCompiler::discover().unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(prepared.content_digest, expected_digest);
        assert_eq!(prepared.source_commit, commit);
        assert_eq!(store.load(&expected_digest).unwrap(), tree);
        assert_eq!(prepared.catalog.rendered_posts().count(), 1);
    }

    #[tokio::test]
    async fn invalid_immutable_candidate_is_not_retained() {
        let state = tempfile::tempdir().unwrap();
        let store =
            ContentCandidateStore::open(state.path(), ContentTreeLimits::default()).unwrap();
        let mut tree = compiled_candidate().tree;
        tree.publication.source = "not valid publication TOML".into();

        assert!(matches!(
            prepare_immutable_candidate(
                tree,
                SourceCommit::parse(&format!("git-sha1:{}", "42".repeat(20))).unwrap(),
                store.clone(),
                ContentCompiler::discover().unwrap(),
            )
            .await,
            Err(ContentCandidatePreparationError::Validate(_))
        ));
        assert!(store.load_all().unwrap().is_empty());
    }

    #[tokio::test]
    async fn retention_worker_preserves_the_exact_observed_tree() {
        let state = tempfile::tempdir().unwrap();
        let store =
            ContentCandidateStore::open(state.path(), ContentTreeLimits::default()).unwrap();
        let candidate = compiled_candidate();
        let expected_digest = candidate.digest.clone();
        let expected_tree = candidate.tree.clone();

        let retained = retain_candidate(candidate, store.clone()).await.unwrap();

        assert_eq!(retained.digest, expected_digest);
        assert_eq!(retained.tree, expected_tree);
        assert_eq!(store.load(&expected_digest).unwrap(), expected_tree);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn unsafe_retention_target_is_a_fatal_sync_error() {
        use std::os::unix::fs::symlink;

        let state = tempfile::tempdir().unwrap();
        let store =
            ContentCandidateStore::open(state.path(), ContentTreeLimits::default()).unwrap();
        let candidate = compiled_candidate();
        let target = state.path().join("outside");
        std::fs::write(&target, b"not a candidate").unwrap();
        let occupied = state
            .path()
            .join("content-candidates")
            .join(format!("{}.candidate", candidate.digest));
        symlink(target, occupied).unwrap();

        assert!(matches!(
            retain_candidate(candidate, store).await,
            Err(ContentSyncError::Retention(_))
        ));
    }

    #[test]
    fn reload_failures_retry_only_safe_outcomes_and_preserve_rejection_state() {
        let candidate = compiled_candidate();
        let candidate_digest = candidate.digest.clone();
        let mut retry = ContentSyncState {
            pending: Some(candidate.digest.clone()),
            ..ContentSyncState::default()
        };
        assert_eq!(
            retry
                .handle_reload_failure(
                    ContentReloadError::Database(DatabaseAdmissionError::QueueFull.into()),
                    candidate,
                    &CancellationToken::new(),
                )
                .unwrap(),
            SyncControl::Continue
        );
        assert!(retry.pending.is_none());
        assert_eq!(
            retry.retained.as_ref().map(|candidate| &candidate.digest),
            Some(&candidate_digest)
        );

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let candidate = compiled_candidate();
        let mut stopped = ContentSyncState::default();
        assert_eq!(
            stopped
                .handle_reload_failure(
                    ContentReloadError::Coordinator(PublicationCoordinatorUnavailable::Closed),
                    candidate,
                    &cancellation,
                )
                .unwrap(),
            SyncControl::Stop
        );

        let candidate = compiled_candidate();
        let mut fatal = ContentSyncState::default();
        assert!(matches!(
            fatal.handle_reload_failure(
                ContentReloadError::Database(DatabaseAdmissionError::WriterClosed.into()),
                candidate,
                &CancellationToken::new(),
            ),
            Err(ContentSyncError::Reload(ContentReloadError::Database(_)))
        ));

        let candidate = compiled_candidate();
        let rejection = retention_reload_error(&candidate);
        let mut rejected = ContentSyncState::default();
        assert_eq!(
            rejected
                .handle_reload_failure(rejection, candidate, &CancellationToken::new(),)
                .unwrap(),
            SyncControl::Continue
        );
        assert_eq!(rejected.rejected, Some(candidate_digest));
        assert!(rejected.pending.is_none());
        assert!(rejected.retained.is_none());
    }
}
