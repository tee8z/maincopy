-- Canonical publication and target-job state. The application's single writer
-- owns state transitions, optimistic concurrency, and idempotent retries.

CREATE TABLE canonical_publications (
    publication_id BLOB PRIMARY KEY CHECK (length(publication_id) = 16),
    creation_key BLOB UNIQUE,
    command_kind TEXT NOT NULL
        CHECK (command_kind IN ('immediate', 'scheduled')),
    stable_post_id BLOB NOT NULL,
    requested_revision_digest BLOB,
    pinned_post_digest BLOB NOT NULL,
    content_tree_digest BLOB NOT NULL CHECK (length(content_tree_digest) = 32),
    accepted_preview_digest BLOB NOT NULL CHECK (length(accepted_preview_digest) = 32),
    state TEXT NOT NULL,
    version INTEGER NOT NULL,
    scheduled_at_ns INTEGER NOT NULL,
    activation_at_ns INTEGER,
    activation_site_digest BLOB,
    published_at_ns INTEGER,
    current_published_digest BLOB,
    source_commit BLOB,
    block_reason TEXT,
    UNIQUE (stable_post_id, pinned_post_digest),
    FOREIGN KEY (stable_post_id, pinned_post_digest)
        REFERENCES post_revisions (stable_post_id, revision_digest),
    FOREIGN KEY (stable_post_id, current_published_digest)
        REFERENCES post_revisions (stable_post_id, revision_digest)
) STRICT;

CREATE TABLE publication_jobs (
    publication_job_id BLOB PRIMARY KEY
        CHECK (length(publication_job_id) = 16),
    publication_id BLOB NOT NULL
        REFERENCES canonical_publications (publication_id),
    idempotency_key BLOB NOT NULL UNIQUE
        CHECK (length(idempotency_key) = 16),
    state TEXT NOT NULL,
    target TEXT NOT NULL,
    version INTEGER NOT NULL,
    scheduled_at_ns INTEGER NOT NULL,
    payload_version INTEGER NOT NULL,
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    payload_body TEXT NOT NULL,
    UNIQUE (publication_id, target)
) STRICT;
