-- PII belongs only to these bounded consent/attempt tables, never public audit.
CREATE TABLE mail_control_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    control_binding BLOB NOT NULL CHECK (length(control_binding) = 32),
    mail_epoch BLOB NOT NULL UNIQUE CHECK (length(mail_epoch)=16),
    control_version INTEGER NOT NULL CHECK (control_version>0),
    configuration_binding BLOB CHECK (configuration_binding IS NULL OR length(configuration_binding) = 32),
    mode TEXT NOT NULL CHECK (mode IN ('paused','enabled')),
    enrollment_sequence INTEGER NOT NULL CHECK (enrollment_sequence >= 0),
    max_daily_messages INTEGER NOT NULL CHECK (max_daily_messages BETWEEN 1 AND 1000000),
    max_daily_confirmations INTEGER NOT NULL CHECK (max_daily_confirmations BETWEEN 1 AND max_daily_messages),
    max_campaign_recipients INTEGER NOT NULL CHECK (max_campaign_recipients BETWEEN 1 AND 100000),
    last_feedback_ok_at INTEGER,
    feedback_gap INTEGER NOT NULL CHECK (feedback_gap IN (0,1)),
    feedback_run BLOB CHECK (feedback_run IS NULL OR length(feedback_run)=16),
    feedback_source BLOB CHECK (feedback_source IS NULL OR length(feedback_source)=32),
    feedback_retention INTEGER CHECK (feedback_retention IS NULL OR feedback_retention BETWEEN 60 AND 1209600),
    last_feedback_observed_at INTEGER,
    feedback_provider_at INTEGER,
    feedback_clock_regressed INTEGER NOT NULL DEFAULT 0 CHECK (feedback_clock_regressed IN (0,1)),
    feedback_poll BLOB CHECK (feedback_poll IS NULL OR length(feedback_poll)=16),
    feedback_poll_signed_at INTEGER,
    feedback_recover_after INTEGER,
    feedback_quiet_since INTEGER,
    CHECK ((feedback_poll IS NULL)=(feedback_poll_signed_at IS NULL)),
    CHECK (feedback_poll IS NULL OR feedback_run IS NOT NULL),
    CHECK (feedback_quiet_since IS NULL OR feedback_recover_after IS NOT NULL),
    CHECK ((feedback_source IS NULL) = (feedback_retention IS NULL)),
    CHECK (feedback_run IS NULL OR feedback_source IS NOT NULL),
    CHECK (last_feedback_observed_at IS NULL OR feedback_source IS NOT NULL)
) STRICT;

CREATE TABLE mail_enrollments (
    enrollment_id BLOB PRIMARY KEY CHECK (length(enrollment_id) = 16),
    generation BLOB NOT NULL CHECK (length(generation) = 16),
    mailbox_digest BLOB CHECK (mailbox_digest IS NULL OR length(mailbox_digest) = 32),
    address TEXT COLLATE NOCASE CHECK (address IS NULL OR length(CAST(address AS BLOB)) BETWEEN 3 AND 254),
    state TEXT NOT NULL CHECK (state IN ('pending','active','removed')),
    nonce_digest BLOB CHECK (nonce_digest IS NULL OR length(nonce_digest) = 32),
    nonce_expires_at INTEGER,
    pending_expires_at INTEGER,
    created_at INTEGER NOT NULL,
    confirmed_at INTEGER,
    confirmed_sequence INTEGER,
    confirmation_requested_at INTEGER NOT NULL,
    retire_at INTEGER,
    UNIQUE (enrollment_id, generation),
    CHECK ((nonce_digest IS NULL) = (nonce_expires_at IS NULL)),
    CHECK ((state = 'pending' AND address IS NOT NULL AND mailbox_digest IS NOT NULL
            AND pending_expires_at IS NOT NULL AND pending_expires_at > created_at
            AND confirmed_at IS NULL AND confirmed_sequence IS NULL AND retire_at IS NULL)
        OR (state = 'active' AND address IS NOT NULL AND mailbox_digest IS NOT NULL
            AND nonce_digest IS NULL AND pending_expires_at IS NULL
            AND confirmed_at IS NOT NULL AND confirmed_sequence IS NOT NULL AND confirmed_sequence > 0 AND retire_at IS NULL)
        OR (state = 'removed' AND address IS NULL AND mailbox_digest IS NULL
            AND nonce_digest IS NULL AND pending_expires_at IS NULL
            AND confirmed_at IS NULL AND confirmed_sequence IS NULL AND retire_at IS NOT NULL))
) STRICT;
CREATE UNIQUE INDEX mail_active_mailbox_idx ON mail_enrollments (mailbox_digest) WHERE mailbox_digest IS NOT NULL;
CREATE UNIQUE INDEX mail_active_address_idx ON mail_enrollments (address) WHERE address IS NOT NULL;
CREATE INDEX mail_enrollment_audience_idx ON mail_enrollments (state,confirmed_sequence,enrollment_id);

CREATE TABLE mail_attempts (
    feedback_source BLOB NOT NULL CHECK (length(feedback_source)=32),
    mail_epoch BLOB NOT NULL CHECK (length(mail_epoch)=16),
    attempt_id BLOB PRIMARY KEY CHECK (length(attempt_id) = 16),
    attempt_fence BLOB NOT NULL CHECK (length(attempt_fence) = 16),
    enrollment_id BLOB NOT NULL,
    generation BLOB NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('confirmation','campaign')),
    campaign_id BLOB REFERENCES mail_campaigns(campaign_id),
    campaign_fence BLOB,
    recipient_binding BLOB NOT NULL CHECK (length(recipient_binding) = 32),
    configuration_binding BLOB NOT NULL CHECK (length(configuration_binding) = 32),
    instance_version INTEGER NOT NULL CHECK (instance_version > 0),
    outcome TEXT NOT NULL CHECK (outcome IN ('queued','admitted','accepted','rejected','unknown','cancelled')),
    provider_message_id TEXT CHECK (provider_message_id IS NULL OR length(provider_message_id) BETWEEN 1 AND 256),
    budget_day INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    admitted_at INTEGER,
    finished_at INTEGER,
    retire_at INTEGER NOT NULL,
    FOREIGN KEY (enrollment_id,generation) REFERENCES mail_enrollments(enrollment_id,generation),
    CHECK ((kind = 'confirmation' AND campaign_id IS NULL AND campaign_fence IS NULL)
        OR (kind = 'campaign' AND campaign_id IS NOT NULL AND campaign_fence IS NOT NULL AND length(campaign_id) = 16 AND length(campaign_fence) = 16)),
    CHECK ((outcome = 'queued' AND admitted_at IS NULL AND finished_at IS NULL)
        OR (outcome = 'admitted' AND admitted_at IS NOT NULL AND finished_at IS NULL)
        OR (outcome = 'cancelled' AND admitted_at IS NULL AND finished_at IS NOT NULL)
        OR (outcome IN ('accepted','rejected','unknown') AND admitted_at IS NOT NULL AND finished_at IS NOT NULL)),
    CHECK ((outcome = 'accepted') = (provider_message_id IS NOT NULL))
) STRICT;
CREATE UNIQUE INDEX mail_campaign_recipient_idx ON mail_attempts (campaign_id,enrollment_id,generation) WHERE kind = 'campaign';
CREATE INDEX mail_confirmation_queue_idx ON mail_attempts (kind,outcome,created_at);

CREATE TABLE mail_daily_budget (
    day INTEGER PRIMARY KEY,
    total INTEGER NOT NULL CHECK (total >= 0),
    confirmations INTEGER NOT NULL CHECK (confirmations BETWEEN 0 AND total)
) STRICT;

CREATE TABLE mail_suppressions (
    mailbox_digest BLOB PRIMARY KEY CHECK (length(mailbox_digest) = 32),
    reason TEXT NOT NULL CHECK (reason IN ('hard_bounce','complaint')),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL CHECK (expires_at > created_at)
) STRICT;

-- Reset receipts retain only global epoch identities and aggregate history.
CREATE TABLE mail_consent_resets (
    idempotency_key BLOB PRIMARY KEY CHECK (length(idempotency_key)=16),
    audit_event_id BLOB NOT NULL UNIQUE REFERENCES admin_audit_events(audit_event_id),
    command_fingerprint BLOB NOT NULL CHECK (length(command_fingerprint)=32),
    version INTEGER NOT NULL CHECK (version>0),
    retired_epoch BLOB NOT NULL UNIQUE CHECK (length(retired_epoch)=16),
    new_epoch BLOB NOT NULL UNIQUE CHECK (length(new_epoch)=16),
    discarded_enrollments INTEGER NOT NULL CHECK (discarded_enrollments BETWEEN 0 AND 100000),
    discarded_attempts INTEGER NOT NULL CHECK (discarded_attempts BETWEEN 0 AND 1000000),
    quarantined_campaigns INTEGER NOT NULL CHECK (quarantined_campaigns BETWEEN 0 AND 10000)
) STRICT;
