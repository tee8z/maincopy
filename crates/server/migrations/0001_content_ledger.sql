-- Durable content identities. SQLite stores identities and route ownership,
-- never Markdown source or rendered HTML.

CREATE TABLE site_revisions (
    site_revision_digest TEXT PRIMARY KEY
        CHECK (
            length(CAST(site_revision_digest AS BLOB)) = 75
            AND instr(site_revision_digest, char(0)) = 0
            AND substr(site_revision_digest, 1, 11) = 'site-b3-v1-'
            AND substr(site_revision_digest, 12) NOT GLOB '*[^0-9a-f]*'
        ),
    source_commit TEXT
        CHECK (
            source_commit IS NULL
            OR (
                instr(source_commit, char(0)) = 0
                AND (
                    (
                        length(CAST(source_commit AS BLOB)) = 49
                        AND substr(source_commit, 1, 9) = 'git-sha1:'
                        AND substr(source_commit, 10) NOT GLOB '*[^0-9a-f]*'
                    )
                    OR (
                        length(CAST(source_commit AS BLOB)) = 75
                        AND substr(source_commit, 1, 11) = 'git-sha256:'
                        AND substr(source_commit, 12) NOT GLOB '*[^0-9a-f]*'
                    )
                )
            )
        ),
    activated_at_ns INTEGER NOT NULL,
    version INTEGER NOT NULL UNIQUE
        CHECK (version BETWEEN 1 AND 9223372036854775807)
) STRICT;

CREATE TRIGGER site_revision_is_immutable
BEFORE UPDATE ON site_revisions
BEGIN
    SELECT RAISE(ABORT, 'site revision is immutable');
END;

CREATE TRIGGER site_revision_cannot_be_deleted
BEFORE DELETE ON site_revisions
BEGIN
    SELECT RAISE(ABORT, 'site revision cannot be deleted');
END;

CREATE TABLE site_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    current_site_digest TEXT NOT NULL
        REFERENCES site_revisions (site_revision_digest)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    version INTEGER NOT NULL
        CHECK (version BETWEEN 1 AND 9223372036854775807)
) STRICT;

CREATE TRIGGER site_state_must_start_at_version_one
BEFORE INSERT ON site_state
WHEN NEW.version <> 1
BEGIN
    SELECT RAISE(ABORT, 'site state must start at version one');
END;

CREATE TRIGGER site_state_version_must_advance
BEFORE UPDATE ON site_state
WHEN NEW.version <> OLD.version + 1
    OR NEW.current_site_digest IS OLD.current_site_digest
BEGIN
    SELECT RAISE(ABORT, 'site state head and version must advance');
END;

CREATE TRIGGER site_state_cannot_be_deleted
BEFORE DELETE ON site_state
BEGIN
    SELECT RAISE(ABORT, 'site state cannot be deleted');
END;

CREATE TABLE post_revisions (
    stable_post_id TEXT NOT NULL
        CHECK (
            length(CAST(stable_post_id AS BLOB)) = 36
            AND instr(stable_post_id, char(0)) = 0
            AND stable_post_id = lower(stable_post_id)
            AND length(replace(stable_post_id, '-', '')) = 32
            AND replace(stable_post_id, '-', '') NOT GLOB '*[^0-9a-f]*'
            AND substr(stable_post_id, 9, 1) = '-'
            AND substr(stable_post_id, 14, 1) = '-'
            AND substr(stable_post_id, 19, 1) = '-'
            AND substr(stable_post_id, 24, 1) = '-'
        ),
    revision_digest TEXT NOT NULL
        CHECK (
            length(CAST(revision_digest AS BLOB)) = 75
            AND instr(revision_digest, char(0)) = 0
            AND substr(revision_digest, 1, 11) = 'post-b3-v1-'
            AND substr(revision_digest, 12) NOT GLOB '*[^0-9a-f]*'
        ),
    slug TEXT NOT NULL
        CHECK (
            length(CAST(slug AS BLOB)) BETWEEN 1 AND 1024
            AND instr(slug, char(0)) = 0
            AND slug NOT GLOB '*[^a-z0-9-]*'
            AND substr(slug, 1, 1) <> '-'
            AND substr(slug, -1, 1) <> '-'
            AND instr(slug, '--') = 0
        ),
    publication_status TEXT NOT NULL
        CHECK (publication_status IN ('publishable', 'draft')),
    source_commit TEXT
        CHECK (
            source_commit IS NULL
            OR (
                instr(source_commit, char(0)) = 0
                AND (
                    (
                        length(CAST(source_commit AS BLOB)) = 49
                        AND substr(source_commit, 1, 9) = 'git-sha1:'
                        AND substr(source_commit, 10) NOT GLOB '*[^0-9a-f]*'
                    )
                    OR (
                        length(CAST(source_commit AS BLOB)) = 75
                        AND substr(source_commit, 1, 11) = 'git-sha256:'
                        AND substr(source_commit, 12) NOT GLOB '*[^0-9a-f]*'
                    )
                )
            )
        ),
    first_observed_at_ns INTEGER NOT NULL,
    PRIMARY KEY (stable_post_id, revision_digest)
) STRICT;

CREATE TRIGGER post_revision_is_immutable
BEFORE UPDATE ON post_revisions
BEGIN
    SELECT RAISE(ABORT, 'post revision is immutable');
END;

CREATE TRIGGER post_revision_cannot_be_deleted
BEFORE DELETE ON post_revisions
BEGIN
    SELECT RAISE(ABORT, 'post revision cannot be deleted');
END;

CREATE INDEX post_revisions_by_slug
    ON post_revisions (slug, stable_post_id, revision_digest);

CREATE TABLE published_routes (
    route TEXT PRIMARY KEY
        CHECK (
            length(CAST(route AS BLOB)) BETWEEN 8 AND 1031
            AND instr(route, char(0)) = 0
            AND substr(route, 1, 7) = '/posts/'
            AND substr(route, 8) NOT GLOB '*[^a-z0-9-]*'
            AND substr(route, 8, 1) <> '-'
            AND substr(route, -1, 1) <> '-'
            AND instr(substr(route, 8), '--') = 0
        ),
    kind TEXT NOT NULL CHECK (kind IN ('slug', 'alias')),
    stable_post_id TEXT NOT NULL,
    revision_digest TEXT NOT NULL,
    claimed_at_ns INTEGER NOT NULL,
    FOREIGN KEY (stable_post_id, revision_digest)
        REFERENCES post_revisions (stable_post_id, revision_digest)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
) STRICT;

CREATE TRIGGER published_route_is_immutable
BEFORE UPDATE ON published_routes
BEGIN
    SELECT RAISE(ABORT, 'published route is immutable');
END;

CREATE TRIGGER published_route_cannot_be_deleted
BEFORE DELETE ON published_routes
BEGIN
    SELECT RAISE(ABORT, 'published route cannot be deleted');
END;

CREATE INDEX published_routes_by_post
    ON published_routes (stable_post_id, claimed_at_ns, route);
