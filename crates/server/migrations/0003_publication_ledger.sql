-- Canonical publication owns public visibility. Target work remains waiting
-- until the matching canonical revision is committed as Published.

CREATE TABLE canonical_publications (
    canonical_publication_id TEXT PRIMARY KEY
        CHECK (
            length(CAST(canonical_publication_id AS BLOB)) = 36
            AND instr(canonical_publication_id, char(0)) = 0
            AND canonical_publication_id = lower(canonical_publication_id)
            AND length(replace(canonical_publication_id, '-', '')) = 32
            AND replace(canonical_publication_id, '-', '') NOT GLOB '*[^0-9a-f]*'
            AND substr(canonical_publication_id, 9, 1) = '-'
            AND substr(canonical_publication_id, 14, 1) = '-'
            AND substr(canonical_publication_id, 19, 1) = '-'
            AND substr(canonical_publication_id, 24, 1) = '-'
        ),
    stable_post_id TEXT NOT NULL,
    pinned_post_digest TEXT NOT NULL,
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
    state TEXT NOT NULL
        CHECK (state IN ('scheduled', 'activating', 'blocked', 'published', 'cancelled')),
    scheduled_at_ns INTEGER NOT NULL,
    activation_at_ns INTEGER,
    published_at_ns INTEGER,
    current_published_digest TEXT,
    block_reason TEXT CHECK (block_reason IS NULL OR block_reason = 'revision_unavailable'),
    version INTEGER NOT NULL
        CHECK (version BETWEEN 1 AND 9223372036854775807),
    UNIQUE (canonical_publication_id, stable_post_id, pinned_post_digest),
    FOREIGN KEY (stable_post_id, pinned_post_digest)
        REFERENCES post_revisions (stable_post_id, revision_digest)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (stable_post_id, current_published_digest)
        REFERENCES post_revisions (stable_post_id, revision_digest)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (
        (
            state = 'scheduled'
            AND activation_at_ns IS NULL
            AND published_at_ns IS NULL
            AND current_published_digest IS NULL
            AND block_reason IS NULL
            AND version >= 1
        )
        OR (
            state = 'activating'
            AND activation_at_ns IS NOT NULL
            AND published_at_ns IS NULL
            AND current_published_digest IS NULL
            AND block_reason IS NULL
            AND version >= 2
        )
        OR (
            state = 'blocked'
            AND activation_at_ns IS NOT NULL
            AND published_at_ns IS NULL
            AND current_published_digest IS NULL
            AND block_reason IS NOT NULL
            AND block_reason = 'revision_unavailable'
            AND version >= 3
        )
        OR (
            state = 'published'
            AND activation_at_ns IS NOT NULL
            AND published_at_ns IS NOT NULL
            AND published_at_ns = activation_at_ns
            AND current_published_digest IS NOT NULL
            AND block_reason IS NULL
            AND version >= 3
        )
        OR (
            state = 'cancelled'
            AND published_at_ns IS NULL
            AND current_published_digest IS NULL
            AND version >= 2
            AND (
                (activation_at_ns IS NULL AND block_reason IS NULL)
                OR (
                    activation_at_ns IS NOT NULL
                    AND block_reason IS NOT NULL
                    AND block_reason = 'revision_unavailable'
                )
            )
        )
    )
) STRICT;

CREATE UNIQUE INDEX one_live_canonical_publication_per_post
    ON canonical_publications (stable_post_id)
    WHERE state IN ('scheduled', 'activating', 'blocked', 'published');

CREATE INDEX scheduled_canonical_publications
    ON canonical_publications (scheduled_at_ns, canonical_publication_id)
    WHERE state = 'scheduled';

CREATE INDEX activating_canonical_publications
    ON canonical_publications (activation_at_ns, canonical_publication_id)
    WHERE state = 'activating';

CREATE TRIGGER canonical_publication_must_start_scheduled
BEFORE INSERT ON canonical_publications
WHEN NEW.state <> 'scheduled' OR NEW.version <> 1
BEGIN
    SELECT RAISE(ABORT, 'canonical publication must start scheduled');
END;

CREATE TRIGGER canonical_identity_is_immutable
BEFORE UPDATE ON canonical_publications
WHEN NEW.canonical_publication_id <> OLD.canonical_publication_id
    OR NEW.stable_post_id <> OLD.stable_post_id
    OR NEW.pinned_post_digest <> OLD.pinned_post_digest
    OR NEW.source_commit IS NOT OLD.source_commit
    OR NEW.scheduled_at_ns <> OLD.scheduled_at_ns
BEGIN
    SELECT RAISE(ABORT, 'canonical publication identity is immutable');
END;

CREATE TRIGGER canonical_published_update_is_current_revision_only
BEFORE UPDATE ON canonical_publications
WHEN NEW.state = OLD.state
    AND (
        OLD.state <> 'published'
        OR NEW.activation_at_ns IS NOT OLD.activation_at_ns
        OR NEW.published_at_ns IS NOT OLD.published_at_ns
        OR NEW.block_reason IS NOT OLD.block_reason
        OR NEW.current_published_digest IS OLD.current_published_digest
    )
BEGIN
    SELECT RAISE(ABORT, 'published update must advance only the current revision');
END;

CREATE TRIGGER canonical_publication_cannot_be_deleted
BEFORE DELETE ON canonical_publications
BEGIN
    SELECT RAISE(ABORT, 'canonical publication cannot be deleted');
END;

CREATE TRIGGER canonical_version_must_advance
BEFORE UPDATE ON canonical_publications
WHEN NEW.version <> OLD.version + 1
BEGIN
    SELECT RAISE(ABORT, 'canonical publication version must advance by one');
END;

CREATE TRIGGER canonical_state_transition_is_valid
BEFORE UPDATE OF state ON canonical_publications
WHEN NEW.state <> OLD.state
    AND NOT (
        (OLD.state = 'scheduled' AND NEW.state = 'activating')
        OR (
            OLD.state = 'scheduled'
            AND NEW.state = 'cancelled'
            AND NEW.activation_at_ns IS OLD.activation_at_ns
            AND NEW.block_reason IS OLD.block_reason
        )
        OR (
            OLD.state = 'activating'
            AND NEW.state IN ('blocked', 'published')
            AND NEW.activation_at_ns IS OLD.activation_at_ns
        )
        OR (OLD.state = 'blocked' AND NEW.state = 'activating')
        OR (
            OLD.state = 'blocked'
            AND NEW.state = 'cancelled'
            AND NEW.activation_at_ns IS OLD.activation_at_ns
            AND NEW.block_reason IS OLD.block_reason
        )
    )
BEGIN
    SELECT RAISE(ABORT, 'invalid canonical publication transition');
END;

CREATE TRIGGER initial_published_revision_must_match_the_pin
BEFORE UPDATE ON canonical_publications
WHEN OLD.state = 'activating'
    AND NEW.state = 'published'
    AND NEW.current_published_digest IS NOT NEW.pinned_post_digest
BEGIN
    SELECT RAISE(ABORT, 'initial published revision must match the immutable pin');
END;

CREATE TRIGGER cancelled_revision_cannot_be_reused
BEFORE INSERT ON canonical_publications
WHEN EXISTS (
    SELECT 1
    FROM canonical_publications
    WHERE stable_post_id = NEW.stable_post_id
        AND pinned_post_digest = NEW.pinned_post_digest
        AND state = 'cancelled'
)
BEGIN
    SELECT RAISE(ABORT, 'replacement must select a different revision');
END;

CREATE TRIGGER scheduled_revision_must_be_publishable
BEFORE INSERT ON canonical_publications
WHEN NOT EXISTS (
    SELECT 1
    FROM post_revisions
    WHERE stable_post_id = NEW.stable_post_id
        AND revision_digest = NEW.pinned_post_digest
        AND publication_status = 'publishable'
)
BEGIN
    SELECT RAISE(ABORT, 'canonical revision is not publishable');
END;

CREATE TRIGGER current_revision_must_be_publishable
BEFORE UPDATE OF current_published_digest ON canonical_publications
WHEN NEW.current_published_digest IS NOT NULL
    AND NOT EXISTS (
        SELECT 1
        FROM post_revisions
        WHERE stable_post_id = NEW.stable_post_id
            AND revision_digest = NEW.current_published_digest
            AND publication_status = 'publishable'
    )
BEGIN
    SELECT RAISE(ABORT, 'current canonical revision is not publishable');
END;

CREATE TRIGGER published_revision_update_requires_applying_reload
BEFORE UPDATE OF current_published_digest ON canonical_publications
WHEN OLD.state = 'published'
    AND NEW.state = 'published'
    AND NOT EXISTS (
        SELECT 1
        FROM reload_post_changes AS change
        JOIN reload_operations AS operation
            ON operation.reload_operation_id = change.reload_operation_id
        WHERE operation.state = 'applying'
            AND change.stable_post_id = NEW.stable_post_id
            AND change.expected_post_digest = OLD.current_published_digest
            AND change.candidate_post_digest = NEW.current_published_digest
    )
BEGIN
    SELECT RAISE(ABORT, 'published revision update requires its applying reload');
END;

CREATE TRIGGER reload_post_change_requires_current_publication
BEFORE INSERT ON reload_post_changes
WHEN NOT EXISTS (
    SELECT 1
    FROM canonical_publications
    WHERE stable_post_id = NEW.stable_post_id
        AND state = 'published'
        AND current_published_digest = NEW.expected_post_digest
)
BEGIN
    SELECT RAISE(ABORT, 'reload post revision is not currently published');
END;

CREATE TRIGGER applied_reload_requires_candidate_posts
BEFORE UPDATE ON reload_operations
WHEN NEW.state = 'applied'
    AND EXISTS (
        SELECT 1
        FROM reload_post_changes AS change
        LEFT JOIN canonical_publications AS publication
            ON publication.stable_post_id = change.stable_post_id
            AND publication.state = 'published'
        WHERE change.reload_operation_id = NEW.reload_operation_id
            AND (
                publication.canonical_publication_id IS NULL
                OR publication.current_published_digest <> change.candidate_post_digest
            )
    )
BEGIN
    SELECT RAISE(ABORT, 'applied reload post revision is not current');
END;

CREATE TRIGGER failed_reload_requires_expected_posts
BEFORE UPDATE ON reload_operations
WHEN NEW.state = 'failed'
    AND EXISTS (
        SELECT 1
        FROM reload_post_changes AS change
        LEFT JOIN canonical_publications AS publication
            ON publication.stable_post_id = change.stable_post_id
            AND publication.state = 'published'
        WHERE change.reload_operation_id = NEW.reload_operation_id
            AND (
                publication.canonical_publication_id IS NULL
                OR publication.current_published_digest <> change.expected_post_digest
            )
    )
BEGIN
    SELECT RAISE(ABORT, 'failed reload changed a current post revision');
END;

CREATE TRIGGER published_route_requires_current_publication
BEFORE INSERT ON published_routes
WHEN NOT EXISTS (
    SELECT 1
    FROM canonical_publications
    WHERE stable_post_id = NEW.stable_post_id
        AND state = 'published'
        AND current_published_digest = NEW.revision_digest
)
BEGIN
    SELECT RAISE(ABORT, 'published route requires the current public revision');
END;

CREATE TABLE publication_jobs (
    publication_job_id TEXT PRIMARY KEY
        CHECK (
            length(CAST(publication_job_id AS BLOB)) = 36
            AND instr(publication_job_id, char(0)) = 0
            AND publication_job_id = lower(publication_job_id)
            AND length(replace(publication_job_id, '-', '')) = 32
            AND replace(publication_job_id, '-', '') NOT GLOB '*[^0-9a-f]*'
            AND substr(publication_job_id, 9, 1) = '-'
            AND substr(publication_job_id, 14, 1) = '-'
            AND substr(publication_job_id, 19, 1) = '-'
            AND substr(publication_job_id, 24, 1) = '-'
        ),
    canonical_publication_id TEXT NOT NULL,
    state TEXT NOT NULL
        CHECK (
            state IN (
                'waiting_for_canonical', 'scheduled', 'ready', 'running',
                'succeeded', 'failed', 'outcome_unknown', 'cancelled'
            )
        ),
    target TEXT NOT NULL CHECK (target = 'x'),
    stable_post_id TEXT NOT NULL,
    pinned_post_digest TEXT NOT NULL,
    scheduled_at_ns INTEGER NOT NULL,
    payload_version INTEGER NOT NULL CHECK (payload_version BETWEEN 1 AND 65535),
    payload_body TEXT NOT NULL
        CHECK (length(CAST(payload_body AS BLOB)) <= 65536),
    payload_digest TEXT NOT NULL
        CHECK (
            length(CAST(payload_digest AS BLOB)) = 85
            AND instr(payload_digest, char(0)) = 0
            AND substr(payload_digest, 1, 21) = 'target-payload-b3-v1-'
            AND substr(payload_digest, 22) NOT GLOB '*[^0-9a-f]*'
        ),
    version INTEGER NOT NULL
        CHECK (version BETWEEN 1 AND 9223372036854775807),
    UNIQUE (canonical_publication_id, target),
    FOREIGN KEY (canonical_publication_id, stable_post_id, pinned_post_digest)
        REFERENCES canonical_publications (
            canonical_publication_id, stable_post_id, pinned_post_digest
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    FOREIGN KEY (stable_post_id, pinned_post_digest)
        REFERENCES post_revisions (stable_post_id, revision_digest)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (
        (state = 'waiting_for_canonical' AND version >= 1)
        OR (state IN ('scheduled', 'ready', 'cancelled') AND version >= 2)
        OR (state IN ('running', 'succeeded') AND version >= 3)
        OR (state IN ('failed', 'outcome_unknown') AND version >= 4)
    )
) STRICT;

CREATE INDEX scheduled_publication_jobs
    ON publication_jobs (scheduled_at_ns, publication_job_id)
    WHERE state = 'scheduled';

CREATE INDEX ready_publication_jobs
    ON publication_jobs (scheduled_at_ns, publication_job_id)
    WHERE state = 'ready';

CREATE INDEX running_publication_jobs
    ON publication_jobs (publication_job_id)
    WHERE state = 'running';

CREATE TRIGGER publication_job_insert_state_is_valid
BEFORE INSERT ON publication_jobs
WHEN (NEW.state = 'waiting_for_canonical' AND NEW.version <> 1)
    OR (NEW.state IN ('scheduled', 'ready') AND NEW.version <> 2)
    OR NEW.state NOT IN ('waiting_for_canonical', 'scheduled', 'ready')
BEGIN
    SELECT RAISE(ABORT, 'publication job has an invalid initial state');
END;

CREATE TRIGGER publication_job_identity_is_immutable
BEFORE UPDATE ON publication_jobs
WHEN NEW.publication_job_id <> OLD.publication_job_id
    OR NEW.canonical_publication_id <> OLD.canonical_publication_id
    OR NEW.target <> OLD.target
    OR NEW.stable_post_id <> OLD.stable_post_id
    OR NEW.pinned_post_digest <> OLD.pinned_post_digest
    OR NEW.scheduled_at_ns <> OLD.scheduled_at_ns
    OR NEW.payload_version <> OLD.payload_version
    OR NEW.payload_body <> OLD.payload_body
    OR NEW.payload_digest <> OLD.payload_digest
BEGIN
    SELECT RAISE(ABORT, 'publication job identity is immutable');
END;

CREATE TRIGGER publication_job_version_must_advance
BEFORE UPDATE ON publication_jobs
WHEN NEW.version <> OLD.version + 1
BEGIN
    SELECT RAISE(ABORT, 'publication job version must advance by one');
END;

CREATE TRIGGER publication_job_state_transition_is_valid
BEFORE UPDATE OF state ON publication_jobs
WHEN NEW.state <> OLD.state
    AND NOT (
        (OLD.state = 'waiting_for_canonical' AND NEW.state IN ('scheduled', 'ready', 'cancelled'))
        OR (OLD.state = 'scheduled' AND NEW.state IN ('ready', 'cancelled'))
        OR (OLD.state = 'ready' AND NEW.state IN ('running', 'succeeded', 'cancelled'))
        OR (OLD.state = 'running' AND NEW.state IN ('succeeded', 'failed', 'outcome_unknown'))
        OR (OLD.state IN ('failed', 'outcome_unknown') AND NEW.state IN ('ready', 'cancelled'))
    )
BEGIN
    SELECT RAISE(ABORT, 'invalid publication job transition');
END;

CREATE TRIGGER publication_job_state_must_change
BEFORE UPDATE ON publication_jobs
WHEN NEW.state = OLD.state
BEGIN
    SELECT RAISE(ABORT, 'publication job update must change state');
END;

CREATE TRIGGER publication_job_cannot_be_deleted
BEFORE DELETE ON publication_jobs
BEGIN
    SELECT RAISE(ABORT, 'publication job cannot be deleted');
END;

CREATE TRIGGER inserted_released_job_requires_published_canonical
BEFORE INSERT ON publication_jobs
WHEN NEW.state <> 'waiting_for_canonical'
    AND NOT EXISTS (
        SELECT 1
        FROM canonical_publications
        WHERE canonical_publication_id = NEW.canonical_publication_id
            AND state = 'published'
            AND current_published_digest = NEW.pinned_post_digest
    )
BEGIN
    SELECT RAISE(ABORT, 'publication job canonical revision is not published');
END;

CREATE TRIGGER released_job_requires_published_canonical
BEFORE UPDATE OF state ON publication_jobs
WHEN OLD.state = 'waiting_for_canonical'
    AND NEW.state IN ('scheduled', 'ready')
    AND NOT EXISTS (
        SELECT 1
        FROM canonical_publications
        WHERE canonical_publication_id = NEW.canonical_publication_id
            AND state = 'published'
            AND current_published_digest = NEW.pinned_post_digest
    )
BEGIN
    SELECT RAISE(ABORT, 'publication job canonical revision is not published');
END;
