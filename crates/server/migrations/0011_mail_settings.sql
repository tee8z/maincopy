-- Public newsletter settings are independent of host credentials and endpoints.
CREATE TABLE mail_settings (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    version INTEGER NOT NULL CHECK (version > 0),
    mode TEXT NOT NULL CHECK (mode IN ('paused','enabled')),
    operator_name TEXT NOT NULL CHECK (length(CAST(operator_name AS BLOB)) BETWEEN 1 AND 200),
    postal_address TEXT CHECK (postal_address IS NULL OR length(CAST(postal_address AS BLOB)) BETWEEN 1 AND 500),
    purpose TEXT NOT NULL CHECK (length(CAST(purpose AS BLOB)) BETWEEN 1 AND 2000),
    privacy_url TEXT NOT NULL CHECK (length(CAST(privacy_url AS BLOB)) BETWEEN 1 AND 2048),
    contact_address TEXT NOT NULL CHECK (length(CAST(contact_address AS BLOB)) BETWEEN 3 AND 254),
    max_campaign_recipients INTEGER NOT NULL CHECK (max_campaign_recipients BETWEEN 1 AND 100000),
    max_daily_messages INTEGER NOT NULL CHECK (max_daily_messages BETWEEN 1 AND 1000000),
    max_daily_confirmation_messages INTEGER NOT NULL CHECK (max_daily_confirmation_messages BETWEEN 1 AND max_daily_messages),
    send_interval_milliseconds INTEGER NOT NULL CHECK (send_interval_milliseconds BETWEEN 100 AND 60000)
) STRICT;

CREATE TABLE mail_settings_receipts (
    idempotency_key BLOB PRIMARY KEY CHECK (length(idempotency_key) = 16),
    audit_event_id BLOB NOT NULL UNIQUE CHECK (length(audit_event_id) = 16) REFERENCES admin_audit_events(audit_event_id),
    command_fingerprint BLOB NOT NULL CHECK (length(command_fingerprint) = 32),
    version INTEGER NOT NULL CHECK (version > 0)
) STRICT;
