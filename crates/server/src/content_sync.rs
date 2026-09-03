use std::{path::PathBuf, sync::Arc, time::Duration};

use markdown_compiler::{
    ContentCandidateStore, ContentCandidateStoreError, ContentTreeDigest, ContentTreeLimits,
    DiscoveredContentTree, discover_content_tree, resolve_content_assets,
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
    render::{ContentCatalog, compile_content_catalog},
    source_provenance::{SourceCommitDiscovery, discover_source_commit},
};

const CONTENT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Polls the managed content tree and installs stable, valid changes in-process.
pub(crate) struct ContentSync {
    root: PathBuf,
    limits: ContentTreeLimits,
    candidate_store: ContentCandidateStore,
    active: CandidateKey,
    publications: PublicationCoordinatorHandle,
    cancellation: CancellationToken,
}

impl ContentSync {
    pub(crate) fn new(
        root: PathBuf,
        limits: ContentTreeLimits,
        candidate_store: ContentCandidateStore,
        active_digest: ContentTreeDigest,
        publications: PublicationCoordinatorHandle,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            root,
            limits,
            candidate_store,
            active: CandidateKey {
                digest: active_digest,
            },
            publications,
            cancellation,
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
            .filter(|candidate| candidate.key == observed.key)
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
        if !state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.key == observed.key)
        {
            state.pending = Some(observed);
            return Ok(None);
        }
        let compiled = compile_observed(observed, self.root.clone()).await?;
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
        if confirmed.key != candidate.key {
            state.pending = Some(confirmed);
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
        if confirmed.key != candidate.key {
            state.pending = Some(confirmed);
            return Ok(None);
        }
        Ok(Some(candidate))
    }

    async fn activate_candidate(
        &mut self,
        candidate: CompiledCandidate,
        state: &mut ContentSyncState,
    ) -> Result<SyncControl, ContentSyncError> {
        let candidate_key = candidate.key.clone();
        // Once the bounded actor accepts this command, wait for its durable
        // outcome even if process cancellation arrives concurrently.
        let result = self
            .publications
            .apply_content_catalog(
                Arc::clone(&candidate.catalog),
                candidate.key.digest.clone(),
                candidate.source_commit.clone(),
            )
            .await;
        match result {
            Ok(site) => {
                tracing::info!(
                    content_etag = %candidate_key.digest,
                    site_etag = %site.digest,
                    site_version = site.version,
                    "live content snapshot synchronized"
                );
                self.active = candidate_key;
                state.pending = None;
                state.rejected = None;
                Ok(SyncControl::Continue)
            }
            Err(error) => {
                state.handle_reload_failure(error, candidate, candidate_key, &self.cancellation)
            }
        }
    }
}

#[derive(Default)]
struct ContentSyncState {
    pending: Option<ObservedCandidate>,
    retained: Option<CompiledCandidate>,
    rejected: Option<CandidateKey>,
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
        active: &CandidateKey,
    ) -> Option<ObservedCandidate> {
        self.last_discovery_error = None;
        if &observed.key == active {
            self.pending = None;
            self.retained = None;
            self.rejected = None;
            return None;
        }
        if self.rejected.as_ref() == Some(&observed.key) {
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
                    content_etag = %failure.key.digest,
                    error = %failure.message,
                    "content sync rejected a compiler candidate and kept the last good snapshot"
                );
                self.rejected = Some(failure.key);
                self.pending = None;
                None
            }
        }
    }

    fn handle_reload_failure(
        &mut self,
        error: ContentReloadError,
        candidate: CompiledCandidate,
        candidate_key: CandidateKey,
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
            content_etag = %candidate_key.digest,
            error = %error,
            "content sync rejected a candidate and kept the last good snapshot"
        );
        self.rejected = Some(candidate_key);
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
    let expected = candidate.key.digest.clone();
    let (candidate, retained) = tokio::task::spawn_blocking(move || {
        let retained = store.retain(&candidate.observed.tree);
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct CandidateKey {
    digest: ContentTreeDigest,
}

struct ObservedCandidate {
    key: CandidateKey,
    tree: DiscoveredContentTree,
}

struct CompiledCandidate {
    key: CandidateKey,
    observed: ObservedCandidate,
    catalog: Arc<ContentCatalog>,
    source_commit: Option<SourceCommit>,
}

struct CandidateFailure {
    key: CandidateKey,
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
            key: CandidateKey {
                digest: tree.digest(),
            },
            tree,
        })
    })
    .await
    .map_err(ContentSyncError::Worker)
}

async fn compile_observed(
    observed: ObservedCandidate,
    root: PathBuf,
) -> Result<Result<CompiledCandidate, CandidateFailure>, ContentSyncError> {
    tokio::task::spawn_blocking(move || {
        let key = observed.key.clone();
        let compiled: Result<(Arc<ContentCatalog>, Option<SourceCommit>), String> = (|| {
            let content = observed
                .tree
                .validate()
                .map_err(|error| error.to_string())?;
            let assets = resolve_content_assets(&observed.tree, &content)
                .map_err(|error| error.to_string())?;
            let catalog = compile_content_catalog(&content, &assets)
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
                key,
                observed,
                catalog,
                source_commit,
            }),
            Err(message) => Err(CandidateFailure {
                key,
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
        let content = tree.validate().unwrap();
        let assets = resolve_content_assets(&tree, &content).unwrap();
        let catalog = Arc::new(compile_content_catalog(&content, &assets).unwrap());
        let key = CandidateKey {
            digest: tree.digest(),
        };
        CompiledCandidate {
            key: key.clone(),
            observed: ObservedCandidate { key, tree },
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
    async fn retention_worker_preserves_the_exact_observed_tree() {
        let state = tempfile::tempdir().unwrap();
        let store =
            ContentCandidateStore::open(state.path(), ContentTreeLimits::default()).unwrap();
        let candidate = compiled_candidate();
        let expected_digest = candidate.key.digest.clone();
        let expected_tree = candidate.observed.tree.clone();

        let retained = retain_candidate(candidate, store.clone()).await.unwrap();

        assert_eq!(retained.key.digest, expected_digest);
        assert_eq!(retained.observed.tree, expected_tree);
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
            .join(format!("{}.candidate", candidate.key.digest));
        symlink(target, occupied).unwrap();

        assert!(matches!(
            retain_candidate(candidate, store).await,
            Err(ContentSyncError::Retention(_))
        ));
    }

    #[test]
    fn reload_failures_retry_only_safe_outcomes_and_preserve_rejection_state() {
        let candidate = compiled_candidate();
        let candidate_key = candidate.key.clone();
        let mut retry = ContentSyncState {
            pending: Some(ObservedCandidate {
                key: candidate.key.clone(),
                tree: candidate.observed.tree.clone(),
            }),
            ..ContentSyncState::default()
        };
        assert_eq!(
            retry
                .handle_reload_failure(
                    ContentReloadError::Database(DatabaseAdmissionError::QueueFull.into()),
                    candidate,
                    candidate_key.clone(),
                    &CancellationToken::new(),
                )
                .unwrap(),
            SyncControl::Continue
        );
        assert!(retry.pending.is_none());
        assert_eq!(
            retry.retained.as_ref().map(|candidate| &candidate.key),
            Some(&candidate_key)
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
                    candidate_key.clone(),
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
                candidate_key.clone(),
                &CancellationToken::new(),
            ),
            Err(ContentSyncError::Reload(ContentReloadError::Database(_)))
        ));

        let candidate = compiled_candidate();
        let rejection = retention_reload_error(&candidate);
        let mut rejected = ContentSyncState::default();
        assert_eq!(
            rejected
                .handle_reload_failure(
                    rejection,
                    candidate,
                    candidate_key.clone(),
                    &CancellationToken::new(),
                )
                .unwrap(),
            SyncControl::Continue
        );
        assert_eq!(rejected.rejected, Some(candidate_key));
        assert!(rejected.pending.is_none());
        assert!(rejected.retained.is_none());
    }
}
