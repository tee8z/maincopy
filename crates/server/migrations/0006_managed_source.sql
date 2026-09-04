-- Managed-source configuration is instance-owned database state. Host-owned
-- paths to private keys, known-hosts files, mirrors, and candidates never enter
-- this table.
CREATE TABLE source_configuration (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    ssh_user TEXT NOT NULL CHECK (length(ssh_user) BETWEEN 1 AND 64),
    ssh_host TEXT NOT NULL CHECK (length(ssh_host) BETWEEN 1 AND 253),
    ssh_port INTEGER NOT NULL CHECK (ssh_port BETWEEN 1 AND 65535),
    repository_path TEXT NOT NULL CHECK (length(repository_path) BETWEEN 1 AND 1024),
    branch TEXT NOT NULL CHECK (length(branch) BETWEEN 1 AND 255),
    content_subdirectory TEXT NOT NULL
        CHECK (length(content_subdirectory) BETWEEN 1 AND 1024),
    credential_name TEXT NOT NULL CHECK (length(credential_name) BETWEEN 1 AND 64),
    poll_interval_seconds INTEGER NOT NULL CHECK (poll_interval_seconds BETWEEN 30 AND 86400),
    version INTEGER NOT NULL CHECK (version > 0),
    updated_at_ns INTEGER NOT NULL,
    next_poll_at_ns INTEGER
) STRICT;

-- Configuration revisions are immutable so an idempotent retry can return
-- the exact result even after a later owner update advances the live head.
CREATE TABLE source_configuration_revisions (
    version INTEGER PRIMARY KEY CHECK (version > 0),
    ssh_user TEXT NOT NULL CHECK (length(ssh_user) BETWEEN 1 AND 64),
    ssh_host TEXT NOT NULL CHECK (length(ssh_host) BETWEEN 1 AND 253),
    ssh_port INTEGER NOT NULL CHECK (ssh_port BETWEEN 1 AND 65535),
    repository_path TEXT NOT NULL CHECK (length(repository_path) BETWEEN 1 AND 1024),
    branch TEXT NOT NULL CHECK (length(branch) BETWEEN 1 AND 255),
    content_subdirectory TEXT NOT NULL
        CHECK (length(content_subdirectory) BETWEEN 1 AND 1024),
    credential_name TEXT NOT NULL CHECK (length(credential_name) BETWEEN 1 AND 64),
    poll_interval_seconds INTEGER NOT NULL CHECK (poll_interval_seconds BETWEEN 30 AND 86400),
    updated_at_ns INTEGER NOT NULL
) STRICT;

CREATE TABLE source_sync_operations (
    source_sync_id BLOB PRIMARY KEY CHECK (length(source_sync_id) = 16),
    configuration_version INTEGER NOT NULL
        REFERENCES source_configuration_revisions (version)
        CHECK (configuration_version > 0),
    request_origin TEXT NOT NULL CHECK (request_origin IN ('startup', 'poll', 'manual')),
    stage TEXT NOT NULL CHECK (stage IN (
        'queued',
        'fetching',
        'resolving_commit',
        'preparing_candidate',
        'compiling',
        'reloading'
    )),
    outcome TEXT CHECK (outcome IN ('applied', 'no_change', 'failed', 'cancelled')),
    source_commit BLOB CHECK (source_commit IS NULL OR length(source_commit) IN (20, 32)),
    content_digest BLOB CHECK (content_digest IS NULL OR length(content_digest) = 32),
    failure_code TEXT CHECK (failure_code IN (
        'configuration_changed',
        'credential_unavailable',
        'unknown_host',
        'authentication_failed',
        'remote_unavailable',
        'branch_unavailable',
        'fetch_failed',
        'commit_invalid',
        'candidate_failed',
        'validation_failed',
        'compile_failed',
        'reload_failed',
        'timed_out',
        'interrupted',
        'internal'
    )),
    version INTEGER NOT NULL CHECK (version > 0),
    requested_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL,
    finished_at_ns INTEGER,
    CHECK ((outcome IS NULL) = (finished_at_ns IS NULL)),
    CHECK ((outcome IS 'failed') = (failure_code IS NOT NULL)),
    -- Successful outcomes and active stages each have one valid provenance
    -- shape. Failed and cancelled outcomes retain the state that was reached.
    CHECK (outcome IS NOT 'applied' OR (
        stage = 'reloading'
        AND source_commit IS NOT NULL
        AND content_digest IS NOT NULL
    )),
    CHECK (outcome IS NOT 'no_change' OR (
        stage = 'resolving_commit'
        AND source_commit IS NOT NULL
        AND content_digest IS NOT NULL
    )),
    CHECK (outcome IS NOT NULL OR (
        (stage IN ('queued', 'fetching', 'resolving_commit')
            AND source_commit IS NULL
            AND content_digest IS NULL)
        OR (stage IN ('preparing_candidate', 'compiling')
            AND source_commit IS NOT NULL
            AND content_digest IS NULL)
        OR (stage = 'reloading'
            AND source_commit IS NOT NULL
            AND content_digest IS NOT NULL)
    )),
    CHECK (updated_at_ns >= requested_at_ns),
    CHECK (finished_at_ns IS NULL OR finished_at_ns >= updated_at_ns)
) STRICT;

-- A single durable operation owns managed-source coordination across startup,
-- scheduled polls, and manual requests. Concurrent requests coalesce onto it.
CREATE UNIQUE INDEX source_sync_one_nonterminal_idx
    ON source_sync_operations ((1))
    WHERE outcome IS NULL;

CREATE INDEX source_sync_history_idx
    ON source_sync_operations (requested_at_ns DESC, source_sync_id DESC);

CREATE TABLE source_installation (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    configuration_version INTEGER NOT NULL
        REFERENCES source_configuration_revisions (version)
        CHECK (configuration_version > 0),
    source_commit BLOB NOT NULL CHECK (length(source_commit) IN (20, 32)),
    content_digest BLOB NOT NULL CHECK (length(content_digest) = 32),
    source_sync_id BLOB NOT NULL REFERENCES source_sync_operations (source_sync_id),
    installed_at_ns INTEGER NOT NULL
) STRICT;

-- Configuration receipts and manual-sync aliases make accepted admin writes
-- replayable after a process restart without rerunning a mutation.
CREATE TABLE source_configuration_mutation_receipts (
    idempotency_key BLOB PRIMARY KEY CHECK (length(idempotency_key) = 16),
    audit_event_id BLOB NOT NULL UNIQUE
        REFERENCES admin_audit_events (audit_event_id),
    command_fingerprint BLOB NOT NULL CHECK (length(command_fingerprint) = 32),
    result_version INTEGER NOT NULL
        REFERENCES source_configuration_revisions (version),
    completed_at_ns INTEGER NOT NULL
) STRICT;

CREATE TABLE source_sync_idempotency_aliases (
    idempotency_key BLOB PRIMARY KEY CHECK (length(idempotency_key) = 16),
    audit_event_id BLOB NOT NULL UNIQUE
        REFERENCES admin_audit_events (audit_event_id),
    command_fingerprint BLOB NOT NULL CHECK (length(command_fingerprint) = 32),
    source_sync_id BLOB NOT NULL REFERENCES source_sync_operations (source_sync_id),
    requested_at_ns INTEGER NOT NULL
) STRICT;

CREATE INDEX source_sync_idempotency_alias_history_idx
    ON source_sync_idempotency_aliases (requested_at_ns DESC, idempotency_key DESC);

-- SQLite cannot widen a CHECK constraint in place. Rebuild the leaf table so
-- existing credential assignments remain valid while new source operations
-- can be authorized by the same closed scope vocabulary as the application.
CREATE TABLE agent_credential_scopes_with_source (
    agent_credential_id BLOB NOT NULL
        REFERENCES agent_credentials (agent_credential_id),
    scope TEXT NOT NULL CHECK (scope IN (
        'content_read',
        'status_read',
        'source_sync',
        'preview_read',
        'release_manage',
        'source_manage',
        'profile_manage',
        'lightning_manage',
        'user_manage',
        'credential_manage',
        'role_assign',
        'audit_read'
    )),
    PRIMARY KEY (agent_credential_id, scope)
) STRICT;

INSERT INTO agent_credential_scopes_with_source (
    agent_credential_id,
    scope
)
SELECT
    agent_credential_id,
    scope
FROM agent_credential_scopes;

DROP TABLE agent_credential_scopes;

ALTER TABLE agent_credential_scopes_with_source
    RENAME TO agent_credential_scopes;
