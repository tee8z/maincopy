use std::sync::Arc;

use markdown_compiler::{PostId, PostRevisionDigest};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};

/// The exact post revision and committed publication time authorized for public use.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedPostRevision {
    pub(crate) post_id: PostId,
    pub(crate) revision: PostRevisionDigest,
    pub(crate) published_at: OffsetDateTime,
}

impl PublishedPostRevision {
    pub fn new(
        post_id: PostId,
        revision: PostRevisionDigest,
        published_at: OffsetDateTime,
    ) -> Self {
        Self {
            post_id,
            revision,
            published_at: published_at.to_offset(UtcOffset::UTC),
        }
    }
}

/// An exact, storage-neutral view of revisions authorized for one public snapshot.
///
/// Persistence adapters and the snapshot-transition coordinator construct the
/// entries inside the crate. Public callers can explicitly request an empty
/// projection, but cannot infer publication from the content catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicLedgerProjection {
    entries: Arc<[PublishedPostRevision]>,
}

impl PublicLedgerProjection {
    pub fn empty() -> Self {
        Self {
            entries: Arc::from([]),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Finds the exact public-ledger entry for one stable post ID.
    pub(crate) fn published_post(&self, post_id: &PostId) -> Option<&PublishedPostRevision> {
        self.entries
            .binary_search_by(|entry| entry.post_id.cmp(post_id))
            .ok()
            .map(|index| &self.entries[index])
    }

    /// Iterates the exact durable posts represented by this public projection.
    pub(crate) fn published_posts(&self) -> impl ExactSizeIterator<Item = &PublishedPostRevision> {
        self.entries.iter()
    }

    /// Returns the exact catalog keys that durable public visibility retains.
    pub(crate) fn revision_keys(
        &self,
    ) -> impl ExactSizeIterator<Item = (PostId, PostRevisionDigest)> + '_ {
        self.entries
            .iter()
            .map(|entry| (entry.post_id.clone(), entry.revision.clone()))
    }

    pub(crate) fn try_from_exact_entries(
        entries: impl IntoIterator<Item = PublishedPostRevision>,
    ) -> Result<Self, PublicLedgerProjectionError> {
        let mut entries: Vec<_> = entries.into_iter().collect();
        entries.sort_by(|left, right| left.post_id.cmp(&right.post_id));
        for pair in entries.windows(2) {
            if pair[0].post_id == pair[1].post_id {
                return Err(PublicLedgerProjectionError {
                    post_id: pair[0].post_id.clone(),
                });
            }
        }
        Ok(Self {
            entries: entries.into(),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_published(
        &self,
        published: PublishedPostRevision,
    ) -> Result<Self, PublicLedgerProjectionError> {
        let insert_at = match self
            .entries
            .binary_search_by(|entry| entry.post_id.cmp(&published.post_id))
        {
            Ok(_) => {
                return Err(PublicLedgerProjectionError {
                    post_id: published.post_id,
                });
            }
            Err(insert_at) => insert_at,
        };

        let mut entries = Vec::with_capacity(self.entries.len() + 1);
        entries.extend_from_slice(&self.entries[..insert_at]);
        entries.push(published);
        entries.extend_from_slice(&self.entries[insert_at..]);
        Ok(Self {
            entries: entries.into(),
        })
    }

    /// Inserts a first publication or replaces its revision at the original publication time.
    pub(crate) fn with_approved(&self, mut approved: PublishedPostRevision) -> Self {
        let mut entries = self.entries.to_vec();
        match entries.binary_search_by(|entry| entry.post_id.cmp(&approved.post_id)) {
            Ok(index) => {
                approved.published_at = entries[index].published_at;
                entries[index] = approved;
            }
            Err(index) => entries.insert(index, approved),
        }
        Self {
            entries: entries.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("public ledger contains duplicate post {post_id}")]
pub(crate) struct PublicLedgerProjectionError {
    post_id: PostId,
}

impl PublicLedgerProjectionError {
    #[cfg(test)]
    pub(crate) fn post_id(&self) -> &PostId {
        &self.post_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post_id() -> PostId {
        PostId::parse("11111111-1111-4111-8111-111111111111").unwrap()
    }

    fn revision(byte: u8) -> PostRevisionDigest {
        let encoded = format!("{byte:02x}").repeat(32);
        PostRevisionDigest::parse(&format!("post-b3-v1-{encoded}")).unwrap()
    }

    #[test]
    fn approving_an_update_preserves_the_original_publication_time() {
        let initial_time = OffsetDateTime::from_unix_timestamp(1_000).unwrap();
        let update_time = OffsetDateTime::from_unix_timestamp(2_000).unwrap();
        let initial = PublicLedgerProjection::empty().with_approved(PublishedPostRevision::new(
            post_id(),
            revision(0x11),
            initial_time,
        ));
        let updated = initial.with_approved(PublishedPostRevision::new(
            post_id(),
            revision(0x22),
            update_time,
        ));
        let published = updated.published_post(&post_id()).unwrap();

        assert_eq!(published.revision, revision(0x22));
        assert_eq!(published.published_at, initial_time);
    }
}
