-- Only reviewed public content and campaign-level control state belong here.
-- Recipient addresses, identifiers, tokens and provider response bodies do not.
CREATE TABLE mail_campaigns (
    campaign_id BLOB PRIMARY KEY CHECK (length(campaign_id) = 16),
    version INTEGER NOT NULL CHECK (version > 0),
    state TEXT NOT NULL CHECK (state IN (
        'draft', 'queued', 'claimed', 'cancelling', 'completed', 'cancelled',
        'unknown', 'quarantined'
    )),
    record TEXT NOT NULL CHECK (
        length(CAST(record AS BLOB)) BETWEEN 1 AND 262144 AND json_valid(record)
        AND json_type(record, '$.version') IS 'integer'
        AND json_extract(record, '$.version') = version
        AND json_type(record, '$.state.kind') IS 'text'
        AND json_extract(record, '$.state.kind') = state
    )
) STRICT;

CREATE TABLE mail_campaign_finishes (
    campaign_id BLOB PRIMARY KEY REFERENCES mail_campaigns(campaign_id),
    fence BLOB NOT NULL UNIQUE CHECK (length(fence) = 16),
    intent TEXT NOT NULL CHECK (intent IN ('complete','cancel')),
    command_fingerprint BLOB NOT NULL CHECK (length(command_fingerprint) = 32),
    result TEXT NOT NULL CHECK (
        length(CAST(result AS BLOB)) BETWEEN 1 AND 262144 AND json_valid(result)
    )
) STRICT;

CREATE UNIQUE INDEX mail_campaign_active_idx ON mail_campaigns ((1))
    WHERE state IN ('draft', 'queued', 'claimed', 'cancelling');

-- Immutable exact results survive subsequent transitions and restore quarantine.
CREATE TABLE mail_campaign_receipts (
    idempotency_key BLOB PRIMARY KEY CHECK (length(idempotency_key) = 16),
    audit_event_id BLOB NOT NULL UNIQUE REFERENCES admin_audit_events(audit_event_id),
    command_fingerprint BLOB NOT NULL CHECK (length(command_fingerprint) = 32),
    campaign_id BLOB NOT NULL REFERENCES mail_campaigns(campaign_id),
    result TEXT NOT NULL CHECK (
        length(CAST(result AS BLOB)) BETWEEN 1 AND 262144 AND json_valid(result)
    )
) STRICT;
