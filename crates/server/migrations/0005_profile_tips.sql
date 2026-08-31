-- SQLite-owned user presentation and the single active tip-recipient setting.
-- Application code validates the complete typed display name and Lightning
-- Address before either value reaches the sole writer.

CREATE TABLE user_profiles (
    user_id BLOB PRIMARY KEY
        REFERENCES users (user_id)
        CHECK (length(user_id) = 16),
    display_name TEXT
        CHECK (
            display_name IS NULL
            OR length(CAST(display_name AS BLOB)) BETWEEN 1 AND 160
        ),
    lightning_address TEXT
        CHECK (
            lightning_address IS NULL
            OR length(CAST(lightning_address AS BLOB)) BETWEEN 1 AND 320
        ),
    tips_enabled INTEGER NOT NULL CHECK (tips_enabled IN (0, 1)),
    version INTEGER NOT NULL CHECK (version > 0),
    updated_at_ns INTEGER NOT NULL
) STRICT;

CREATE TABLE site_tip_recipient (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    recipient_user_id BLOB
        REFERENCES users (user_id)
        CHECK (
            recipient_user_id IS NULL
            OR length(recipient_user_id) = 16
        ),
    version INTEGER NOT NULL CHECK (version > 0),
    updated_at_ns INTEGER NOT NULL
) STRICT;

INSERT INTO site_tip_recipient (
    singleton,
    recipient_user_id,
    version,
    updated_at_ns
) VALUES (1, NULL, 1, 0);

-- One globally unique admin idempotency key binds to one profile-domain
-- command and its exact committed response. Optional result columns are a
-- flat storage record whose kind check excludes mixed projections.
CREATE TABLE admin_profile_mutation_receipts (
    idempotency_key BLOB PRIMARY KEY CHECK (length(idempotency_key) = 16),
    audit_event_id BLOB NOT NULL UNIQUE
        REFERENCES admin_audit_events (audit_event_id),
    command_fingerprint BLOB NOT NULL CHECK (length(command_fingerprint) = 32),
    result_kind TEXT NOT NULL
        CHECK (result_kind IN ('user_profile', 'tip_recipient')),
    profile_user_id BLOB REFERENCES users (user_id)
        CHECK (profile_user_id IS NULL OR length(profile_user_id) = 16),
    display_name TEXT
        CHECK (
            display_name IS NULL
            OR length(CAST(display_name AS BLOB)) BETWEEN 1 AND 160
        ),
    lightning_address TEXT
        CHECK (
            lightning_address IS NULL
            OR length(CAST(lightning_address AS BLOB)) BETWEEN 1 AND 320
        ),
    tips_enabled INTEGER CHECK (tips_enabled IN (0, 1)),
    recipient_user_id BLOB REFERENCES users (user_id)
        CHECK (recipient_user_id IS NULL OR length(recipient_user_id) = 16),
    result_version INTEGER NOT NULL CHECK (result_version > 0),
    result_updated_at_ns INTEGER NOT NULL,
    CHECK (
        (result_kind = 'user_profile'
            AND profile_user_id IS NOT NULL
            AND tips_enabled IS NOT NULL
            AND recipient_user_id IS NULL)
        OR
        (result_kind = 'tip_recipient'
            AND profile_user_id IS NULL
            AND display_name IS NULL
            AND lightning_address IS NULL
            AND tips_enabled IS NULL)
    )
) STRICT;
