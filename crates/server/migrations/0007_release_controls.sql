-- Cancelled approvals do not prevent a new, explicitly reviewed release.
CREATE TABLE canonical_publications_next (
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
    approved_scheduled_at_ns INTEGER,
    FOREIGN KEY (stable_post_id, pinned_post_digest)
        REFERENCES post_revisions (stable_post_id, revision_digest),
    FOREIGN KEY (stable_post_id, current_published_digest)
        REFERENCES post_revisions (stable_post_id, revision_digest)
) STRICT;

INSERT INTO canonical_publications_next (publication_id, creation_key, command_kind, stable_post_id, requested_revision_digest, pinned_post_digest, content_tree_digest, accepted_preview_digest, state, version, scheduled_at_ns, activation_at_ns, activation_site_digest, published_at_ns, current_published_digest, source_commit, block_reason, approved_scheduled_at_ns)
SELECT publication_id, creation_key, command_kind, stable_post_id, requested_revision_digest, pinned_post_digest, content_tree_digest, accepted_preview_digest, state, version, scheduled_at_ns, activation_at_ns, activation_site_digest, published_at_ns, current_published_digest, source_commit, block_reason, scheduled_at_ns FROM canonical_publications;
DROP TABLE canonical_publications;
ALTER TABLE canonical_publications_next RENAME TO canonical_publications;
CREATE UNIQUE INDEX canonical_active_revision_idx ON canonical_publications (stable_post_id, pinned_post_digest) WHERE state != 'cancelled';

-- Receipts bind each accepted operation to its exact release and version.
CREATE TABLE release_operations (
    operation_id BLOB PRIMARY KEY CHECK (length(operation_id) = 16),
    publication_id BLOB NOT NULL REFERENCES canonical_publications(publication_id)
        CHECK (length(publication_id) = 16),
    expected_version INTEGER NOT NULL CHECK (expected_version > 0),
    kind TEXT NOT NULL CHECK (kind IN ('reschedule', 'cancel', 'retry')),
    scheduled_at_ns INTEGER,
    result_version INTEGER NOT NULL CHECK (result_version = expected_version + 1),
    created_at_ns INTEGER NOT NULL,
    CHECK ((kind = 'reschedule' AND scheduled_at_ns IS NOT NULL)
        OR (kind IN ('cancel', 'retry') AND scheduled_at_ns IS NULL))
) STRICT;
