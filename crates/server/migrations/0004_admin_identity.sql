-- Private admin identities and proof-of-possession credentials. The sole
-- database writer preserves cross-table account and authorization invariants.

CREATE TABLE instance_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    instance_id BLOB NOT NULL UNIQUE CHECK (length(instance_id) = 16),
    version INTEGER NOT NULL CHECK (version > 0),
    created_at_ns INTEGER NOT NULL
) STRICT;

CREATE TABLE users (
    user_id BLOB PRIMARY KEY CHECK (length(user_id) = 16),
    status TEXT NOT NULL CHECK (status IN ('enabled', 'disabled')),
    version INTEGER NOT NULL CHECK (version > 0),
    created_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL CHECK (updated_at_ns >= created_at_ns)
) STRICT;

CREATE TABLE user_roles (
    user_id BLOB NOT NULL REFERENCES users (user_id),
    role TEXT NOT NULL CHECK (role IN ('owner', 'administrator', 'publisher')),
    assigned_by_user_id BLOB REFERENCES users (user_id),
    assigned_at_ns INTEGER NOT NULL,
    PRIMARY KEY (user_id, role)
) STRICT;

CREATE TABLE user_password_credentials (
    user_id BLOB PRIMARY KEY REFERENCES users (user_id),
    canonical_username TEXT NOT NULL UNIQUE
        CHECK (length(canonical_username) BETWEEN 1 AND 64),
    password_phc TEXT NOT NULL CHECK (length(password_phc) BETWEEN 1 AND 256),
    policy_version INTEGER NOT NULL CHECK (policy_version > 0),
    version INTEGER NOT NULL CHECK (version > 0),
    created_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL CHECK (updated_at_ns >= created_at_ns)
) STRICT;

CREATE TABLE user_nostr_credentials (
    user_id BLOB PRIMARY KEY REFERENCES users (user_id),
    public_key BLOB NOT NULL UNIQUE CHECK (length(public_key) = 32),
    version INTEGER NOT NULL CHECK (version > 0),
    created_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL CHECK (updated_at_ns >= created_at_ns)
) STRICT;

CREATE TABLE browser_sessions (
    session_id BLOB PRIMARY KEY CHECK (length(session_id) = 16),
    user_id BLOB NOT NULL REFERENCES users (user_id),
    provider TEXT NOT NULL CHECK (provider IN ('password', 'nostr')),
    session_token_digest BLOB NOT NULL UNIQUE
        CHECK (length(session_token_digest) = 32),
    csrf_token_digest BLOB NOT NULL UNIQUE CHECK (length(csrf_token_digest) = 32),
    instance_version INTEGER NOT NULL CHECK (instance_version > 0),
    version INTEGER NOT NULL CHECK (version > 0),
    authenticated_at_ns INTEGER NOT NULL,
    fresh_until_ns INTEGER NOT NULL CHECK (fresh_until_ns >= authenticated_at_ns),
    expires_at_ns INTEGER NOT NULL CHECK (expires_at_ns >= fresh_until_ns),
    revoked_at_ns INTEGER,
    last_seen_at_ns INTEGER NOT NULL CHECK (last_seen_at_ns >= authenticated_at_ns),
    CHECK (session_token_digest <> csrf_token_digest),
    CHECK (revoked_at_ns IS NULL OR revoked_at_ns >= authenticated_at_ns)
) STRICT;

CREATE TABLE login_challenges (
    challenge_id BLOB PRIMARY KEY CHECK (length(challenge_id) = 16),
    provider TEXT NOT NULL CHECK (provider IN ('password', 'nostr')),
    challenge_digest BLOB NOT NULL UNIQUE CHECK (length(challenge_digest) = 32),
    created_at_ns INTEGER NOT NULL,
    expires_at_ns INTEGER NOT NULL CHECK (expires_at_ns > created_at_ns),
    consumed_at_ns INTEGER,
    CHECK (
        consumed_at_ns IS NULL
        OR consumed_at_ns BETWEEN created_at_ns AND expires_at_ns
    )
) STRICT;

CREATE TABLE agent_credentials (
    agent_credential_id BLOB PRIMARY KEY
        CHECK (length(agent_credential_id) = 16),
    owner_user_id BLOB NOT NULL REFERENCES users (user_id),
    issuer_user_id BLOB NOT NULL REFERENCES users (user_id),
    public_key BLOB NOT NULL UNIQUE CHECK (length(public_key) = 32),
    label TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 96),
    version INTEGER NOT NULL CHECK (version > 0),
    created_at_ns INTEGER NOT NULL,
    expires_at_ns INTEGER CHECK (expires_at_ns > created_at_ns),
    last_used_at_ns INTEGER,
    revoked_at_ns INTEGER,
    CHECK (last_used_at_ns IS NULL OR last_used_at_ns >= created_at_ns),
    CHECK (revoked_at_ns IS NULL OR revoked_at_ns >= created_at_ns),
    CHECK (
        last_used_at_ns IS NULL
        OR revoked_at_ns IS NULL
        OR last_used_at_ns <= revoked_at_ns
    )
) STRICT;

CREATE TABLE agent_credential_scopes (
    agent_credential_id BLOB NOT NULL
        REFERENCES agent_credentials (agent_credential_id),
    scope TEXT NOT NULL CHECK (scope IN (
        'content_read',
        'status_read',
        'preview_read',
        'release_manage',
        'profile_manage',
        'lightning_manage',
        'user_manage',
        'credential_manage',
        'role_assign',
        'audit_read'
    )),
    PRIMARY KEY (agent_credential_id, scope)
) STRICT;

CREATE TABLE nip98_replay_events (
    event_id BLOB PRIMARY KEY CHECK (length(event_id) = 32),
    principal_kind TEXT NOT NULL
        CHECK (principal_kind IN ('human_nostr', 'agent_credential')),
    user_id BLOB REFERENCES users (user_id),
    agent_credential_id BLOB REFERENCES agent_credentials (agent_credential_id),
    accepted_at_ns INTEGER NOT NULL,
    expires_at_ns INTEGER NOT NULL CHECK (expires_at_ns > accepted_at_ns),
    CHECK (
        (principal_kind = 'human_nostr'
            AND user_id IS NOT NULL
            AND agent_credential_id IS NULL)
        OR
        (principal_kind = 'agent_credential'
            AND user_id IS NULL
            AND agent_credential_id IS NOT NULL)
    )
) STRICT;

CREATE TABLE admin_audit_events (
    audit_event_id BLOB PRIMARY KEY CHECK (length(audit_event_id) = 16),
    occurred_at_ns INTEGER NOT NULL,
    principal_kind TEXT NOT NULL CHECK (principal_kind IN (
        'browser_session',
        'agent_credential',
        'offline',
        'unauthenticated'
    )),
    actor_user_id BLOB REFERENCES users (user_id),
    session_id BLOB CHECK (session_id IS NULL OR length(session_id) = 16),
    agent_credential_id BLOB
        CHECK (agent_credential_id IS NULL OR length(agent_credential_id) = 16),
    request_id BLOB CHECK (request_id IS NULL OR length(request_id) = 16),
    idempotency_key BLOB CHECK (idempotency_key IS NULL OR length(idempotency_key) = 16),
    action TEXT NOT NULL CHECK (length(action) BETWEEN 1 AND 96),
    outcome TEXT NOT NULL CHECK (outcome IN ('succeeded', 'denied', 'failed')),
    reason_code TEXT CHECK (reason_code IS NULL OR length(reason_code) BETWEEN 1 AND 64),
    CHECK (
        (principal_kind = 'browser_session'
            AND actor_user_id IS NOT NULL
            AND session_id IS NOT NULL
            AND agent_credential_id IS NULL)
        OR
        (principal_kind = 'agent_credential'
            AND actor_user_id IS NOT NULL
            AND session_id IS NULL
            AND agent_credential_id IS NOT NULL)
        OR
        (principal_kind = 'offline'
            AND session_id IS NULL
            AND agent_credential_id IS NULL)
        OR
        (principal_kind = 'unauthenticated'
            AND actor_user_id IS NULL
            AND session_id IS NULL
            AND agent_credential_id IS NULL)
    ),
    CHECK (
        (outcome = 'succeeded' AND reason_code IS NULL)
        OR
        (outcome IN ('denied', 'failed') AND reason_code IS NOT NULL)
    )
) STRICT;

-- A key is globally bound to one authenticated identity command. The compact
-- result projection is enough to return the exact committed outcome after a
-- process restart without rerunning the mutation or querying mutable state.
CREATE TABLE admin_identity_mutation_receipts (
    idempotency_key BLOB PRIMARY KEY CHECK (length(idempotency_key) = 16),
    audit_event_id BLOB NOT NULL UNIQUE
        REFERENCES admin_audit_events (audit_event_id),
    command_fingerprint BLOB NOT NULL CHECK (length(command_fingerprint) = 32),
    result_kind TEXT NOT NULL CHECK (result_kind IN ('user', 'agent_credential')),
    result_id BLOB NOT NULL CHECK (length(result_id) = 16),
    result_version INTEGER NOT NULL CHECK (result_version > 0),
    completed_at_ns INTEGER NOT NULL
) STRICT;

CREATE INDEX login_challenges_cleanup_idx
    ON login_challenges (expires_at_ns, challenge_id);

CREATE INDEX nip98_replay_events_cleanup_idx
    ON nip98_replay_events (expires_at_ns, event_id);

CREATE INDEX admin_audit_events_occurred_idx
    ON admin_audit_events (occurred_at_ns, audit_event_id);

CREATE UNIQUE INDEX admin_audit_events_idempotency_idx
    ON admin_audit_events (idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE INDEX user_roles_role_idx ON user_roles (role, user_id);

CREATE INDEX browser_sessions_user_idx
    ON browser_sessions (user_id, revoked_at_ns, expires_at_ns);

CREATE INDEX agent_credentials_owner_idx
    ON agent_credentials (owner_user_id, revoked_at_ns, expires_at_ns);
