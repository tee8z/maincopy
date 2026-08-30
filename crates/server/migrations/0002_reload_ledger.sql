-- A reload remains Applying while the candidate snapshot is being installed.
-- The final transaction records the activated site revision and marks it
-- Applied. Startup reconciles every retained Applying row before listeners.

CREATE TABLE reload_operations (
    reload_operation_id TEXT PRIMARY KEY
        CHECK (
            length(CAST(reload_operation_id AS BLOB)) = 36
            AND instr(reload_operation_id, char(0)) = 0
            AND reload_operation_id = lower(reload_operation_id)
            AND length(replace(reload_operation_id, '-', '')) = 32
            AND replace(reload_operation_id, '-', '') NOT GLOB '*[^0-9a-f]*'
            AND substr(reload_operation_id, 9, 1) = '-'
            AND substr(reload_operation_id, 14, 1) = '-'
            AND substr(reload_operation_id, 19, 1) = '-'
            AND substr(reload_operation_id, 24, 1) = '-'
        ),
    state TEXT NOT NULL CHECK (state IN ('applying', 'applied', 'failed')),
    expected_site_digest TEXT NOT NULL
        REFERENCES site_revisions (site_revision_digest)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    candidate_site_digest TEXT NOT NULL
        CHECK (
            length(CAST(candidate_site_digest AS BLOB)) = 75
            AND instr(candidate_site_digest, char(0)) = 0
            AND substr(candidate_site_digest, 1, 11) = 'site-b3-v1-'
            AND substr(candidate_site_digest, 12) NOT GLOB '*[^0-9a-f]*'
        ),
    started_at_ns INTEGER NOT NULL,
    finished_at_ns INTEGER,
    failure_code TEXT,
    version INTEGER NOT NULL
        CHECK (version BETWEEN 1 AND 9223372036854775807),
    CHECK (expected_site_digest <> candidate_site_digest),
    CHECK (
        (state = 'applying' AND finished_at_ns IS NULL AND failure_code IS NULL AND version >= 1)
        OR (state = 'applied' AND finished_at_ns IS NOT NULL AND failure_code IS NULL AND version >= 2)
        OR (
            state = 'failed'
            AND finished_at_ns IS NOT NULL
            AND failure_code IS NOT NULL
            AND length(CAST(failure_code AS BLOB)) BETWEEN 1 AND 128
            AND instr(failure_code, char(0)) = 0
            AND failure_code NOT GLOB '*[^a-z0-9_]*'
            AND version >= 2
        )
    )
) STRICT;

CREATE UNIQUE INDEX one_applying_reload
    ON reload_operations ((1))
    WHERE state = 'applying';

CREATE INDEX applying_reloads_for_recovery
    ON reload_operations (started_at_ns, reload_operation_id)
    WHERE state = 'applying';

CREATE TRIGGER reload_operation_must_start_applying
BEFORE INSERT ON reload_operations
WHEN NEW.state <> 'applying' OR NEW.version <> 1
BEGIN
    SELECT RAISE(ABORT, 'reload operation must start applying at version one');
END;

CREATE TABLE reload_post_changes (
    reload_operation_id TEXT NOT NULL
        REFERENCES reload_operations (reload_operation_id)
        ON UPDATE RESTRICT
        ON DELETE CASCADE,
    stable_post_id TEXT NOT NULL,
    expected_post_digest TEXT NOT NULL,
    candidate_post_digest TEXT NOT NULL,
    PRIMARY KEY (reload_operation_id, stable_post_id),
    FOREIGN KEY (stable_post_id, expected_post_digest)
        REFERENCES post_revisions (stable_post_id, revision_digest)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (stable_post_id, candidate_post_digest)
        REFERENCES post_revisions (stable_post_id, revision_digest)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (expected_post_digest <> candidate_post_digest)
) STRICT;

CREATE TRIGGER reload_expected_site_must_be_current
BEFORE INSERT ON reload_operations
WHEN NOT EXISTS (
    SELECT 1
    FROM site_state
    WHERE singleton = 1
        AND current_site_digest = NEW.expected_site_digest
)
BEGIN
    SELECT RAISE(ABORT, 'reload expected site revision is not current');
END;

CREATE TRIGGER reload_operation_cannot_be_deleted
BEFORE DELETE ON reload_operations
BEGIN
    SELECT RAISE(ABORT, 'reload operation cannot be deleted');
END;

CREATE TRIGGER reload_identity_is_immutable
BEFORE UPDATE ON reload_operations
WHEN NEW.reload_operation_id <> OLD.reload_operation_id
    OR NEW.expected_site_digest <> OLD.expected_site_digest
    OR NEW.candidate_site_digest <> OLD.candidate_site_digest
    OR NEW.started_at_ns <> OLD.started_at_ns
BEGIN
    SELECT RAISE(ABORT, 'reload identity is immutable');
END;

CREATE TRIGGER reload_transition_is_valid
BEFORE UPDATE ON reload_operations
WHEN OLD.state <> 'applying'
    OR NEW.state NOT IN ('applied', 'failed')
    OR NEW.version <> OLD.version + 1
BEGIN
    SELECT RAISE(ABORT, 'invalid reload transition');
END;

CREATE TRIGGER site_head_update_requires_applying_reload
BEFORE UPDATE OF current_site_digest ON site_state
WHEN NOT EXISTS (
    SELECT 1
    FROM reload_operations
    WHERE state = 'applying'
        AND expected_site_digest = OLD.current_site_digest
        AND candidate_site_digest = NEW.current_site_digest
)
BEGIN
    SELECT RAISE(ABORT, 'site head update requires its applying reload');
END;

CREATE TRIGGER failed_reload_requires_expected_site
BEFORE UPDATE ON reload_operations
WHEN NEW.state = 'failed'
    AND NOT EXISTS (
        SELECT 1
        FROM site_state
        WHERE singleton = 1
            AND current_site_digest = NEW.expected_site_digest
    )
BEGIN
    SELECT RAISE(ABORT, 'failed reload changed the current site revision');
END;

CREATE TRIGGER applied_reload_requires_activated_site
BEFORE UPDATE ON reload_operations
WHEN NEW.state = 'applied'
    AND (
        NOT EXISTS (
        SELECT 1
        FROM site_revisions
        WHERE site_revision_digest = NEW.candidate_site_digest
        )
        OR NOT EXISTS (
            SELECT 1
            FROM site_state
            WHERE singleton = 1
                AND current_site_digest = NEW.candidate_site_digest
        )
    )
BEGIN
    SELECT RAISE(ABORT, 'applied reload site revision is not current');
END;

CREATE TRIGGER reload_post_change_requires_applying_operation
BEFORE INSERT ON reload_post_changes
WHEN NOT EXISTS (
    SELECT 1
    FROM reload_operations
    WHERE reload_operation_id = NEW.reload_operation_id
        AND state = 'applying'
)
BEGIN
    SELECT RAISE(ABORT, 'reload post change requires an applying operation');
END;

CREATE TRIGGER reload_post_change_is_immutable
BEFORE UPDATE ON reload_post_changes
BEGIN
    SELECT RAISE(ABORT, 'reload post change is immutable');
END;

CREATE TRIGGER reload_post_change_cannot_be_deleted
BEFORE DELETE ON reload_post_changes
BEGIN
    SELECT RAISE(ABORT, 'reload post change cannot be deleted');
END;
