-- Durable content identities and route ownership. The application validates
-- domain encodings and state before its single writer persists a row.

CREATE TABLE site_revisions (
    site_revision_digest BLOB PRIMARY KEY
        CHECK (length(site_revision_digest) = 32),
    version INTEGER NOT NULL UNIQUE,
    activated_at_ns INTEGER NOT NULL,
    source_commit BLOB
) STRICT;

CREATE TABLE site_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    current_site_digest BLOB NOT NULL
        REFERENCES site_revisions (site_revision_digest),
    version INTEGER NOT NULL
) STRICT;

CREATE TABLE post_revisions (
    stable_post_id BLOB NOT NULL CHECK (length(stable_post_id) = 16),
    revision_digest BLOB NOT NULL CHECK (length(revision_digest) = 32),
    publication_status TEXT NOT NULL,
    first_observed_at_ns INTEGER NOT NULL,
    slug TEXT NOT NULL,
    source_commit BLOB,
    PRIMARY KEY (stable_post_id, revision_digest)
) STRICT;

CREATE TABLE published_routes (
    route TEXT PRIMARY KEY,
    stable_post_id BLOB NOT NULL,
    revision_digest BLOB NOT NULL,
    kind TEXT NOT NULL,
    claimed_at_ns INTEGER NOT NULL,
    FOREIGN KEY (stable_post_id, revision_digest)
        REFERENCES post_revisions (stable_post_id, revision_digest)
) STRICT;
