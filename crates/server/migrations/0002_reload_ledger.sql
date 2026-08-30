-- Reload history is append-oriented. The application's single writer owns
-- transition legality, version checks, and the final multi-table transaction.

CREATE TABLE reload_operations (
    reload_operation_id BLOB PRIMARY KEY
        CHECK (length(reload_operation_id) = 16),
    expected_site_digest BLOB NOT NULL
        REFERENCES site_revisions (site_revision_digest),
    candidate_site_digest BLOB NOT NULL
        CHECK (length(candidate_site_digest) = 32),
    state TEXT NOT NULL,
    version INTEGER NOT NULL,
    started_at_ns INTEGER NOT NULL,
    finished_at_ns INTEGER,
    failure_code TEXT
) STRICT;

CREATE TABLE reload_post_changes (
    reload_operation_id BLOB NOT NULL
        REFERENCES reload_operations (reload_operation_id),
    stable_post_id BLOB NOT NULL,
    expected_post_digest BLOB NOT NULL,
    candidate_post_digest BLOB NOT NULL,
    PRIMARY KEY (reload_operation_id, stable_post_id),
    FOREIGN KEY (stable_post_id, expected_post_digest)
        REFERENCES post_revisions (stable_post_id, revision_digest),
    FOREIGN KEY (stable_post_id, candidate_post_digest)
        REFERENCES post_revisions (stable_post_id, revision_digest)
) STRICT;
