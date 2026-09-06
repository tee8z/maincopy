-- Proposed settings remain immutable and separate from the installed head.
-- A successful catalog transaction installs them; failed operations retain only
-- their ledger history. History collection cascades this association.
CREATE TABLE source_reconfigurations (
    source_sync_id BLOB PRIMARY KEY REFERENCES source_sync_operations(source_sync_id) ON DELETE CASCADE,
    expected_configuration_version INTEGER NOT NULL REFERENCES source_configuration_revisions(version)
) STRICT;
