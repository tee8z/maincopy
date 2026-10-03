-- Owner-entered channel credentials stay in the application database. The
-- admin portal accepts them and never displays them again.
CREATE TABLE sharing_substack (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    version INTEGER NOT NULL CHECK (version > 0),
    mode TEXT NOT NULL CHECK (mode IN ('paused','enabled')),
    credentials_rejected INTEGER NOT NULL CHECK (credentials_rejected IN (0,1)),
    subdomain TEXT NOT NULL CHECK (length(CAST(subdomain AS BLOB)) BETWEEN 1 AND 63),
    session_cookie TEXT NOT NULL CHECK (length(CAST(session_cookie AS BLOB)) BETWEEN 1 AND 1024)
) STRICT;

CREATE TABLE sharing_x (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    version INTEGER NOT NULL CHECK (version > 0),
    mode TEXT NOT NULL CHECK (mode IN ('paused','enabled')),
    credentials_rejected INTEGER NOT NULL CHECK (credentials_rejected IN (0,1)),
    api_key TEXT NOT NULL CHECK (length(CAST(api_key AS BLOB)) BETWEEN 1 AND 256),
    api_secret TEXT NOT NULL CHECK (length(CAST(api_secret AS BLOB)) BETWEEN 1 AND 256),
    access_token TEXT NOT NULL CHECK (length(CAST(access_token AS BLOB)) BETWEEN 1 AND 256),
    access_token_secret TEXT NOT NULL CHECK (length(CAST(access_token_secret AS BLOB)) BETWEEN 1 AND 256)
) STRICT;

CREATE TABLE sharing_receipts (
    idempotency_key BLOB PRIMARY KEY CHECK (length(idempotency_key) = 16),
    audit_event_id BLOB NOT NULL UNIQUE CHECK (length(audit_event_id) = 16) REFERENCES admin_audit_events(audit_event_id),
    command_fingerprint BLOB NOT NULL CHECK (length(command_fingerprint) = 32)
) STRICT;

-- One teaser per article, written with its first publication. Articles
-- published before this migration have none, so an upgrade shares no archive.
CREATE TABLE sharing_teasers (
    post_id BLOB PRIMARY KEY CHECK (length(post_id) = 16),
    publication_id BLOB NOT NULL CHECK (length(publication_id) = 16),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    title TEXT NOT NULL CHECK (length(CAST(title AS BLOB)) BETWEEN 1 AND 1024),
    summary TEXT NOT NULL CHECK (length(CAST(summary AS BLOB)) <= 1024),
    url TEXT NOT NULL CHECK (length(CAST(url AS BLOB)) BETWEEN 1 AND 2048)
) STRICT;

-- A delivery leaves 'sending' only with a recorded provider outcome; an
-- interrupted one fails closed instead of posting twice.
CREATE TABLE sharing_deliveries (
    post_id BLOB NOT NULL CHECK (length(post_id) = 16) REFERENCES sharing_teasers(post_id),
    channel TEXT NOT NULL CHECK (channel IN ('substack','x')),
    state TEXT NOT NULL CHECK (state IN ('queued','sending','posted','failed')),
    attempts INTEGER NOT NULL CHECK (attempts BETWEEN 0 AND 1000),
    retry_after INTEGER NOT NULL CHECK (retry_after >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= 0),
    draft INTEGER CHECK (draft IS NULL OR draft > 0),
    posted_url TEXT CHECK (posted_url IS NULL OR length(CAST(posted_url AS BLOB)) BETWEEN 1 AND 2048),
    failure TEXT CHECK (failure IS NULL OR failure IN ('interrupted','refused','unexpected_response','retries_exhausted')),
    PRIMARY KEY (post_id, channel),
    CHECK ((state = 'posted') = (posted_url IS NOT NULL)),
    CHECK ((state = 'failed') = (failure IS NOT NULL))
) STRICT;
