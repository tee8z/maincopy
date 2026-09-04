use std::path::Path;

use sqlx::{Executor as _, SqliteConnection};

use super::*;
use crate::config::{DatabaseBusyTimeout, DatabaseReadPoolSize, DatabaseWriterQueueCapacity};

const SITE_A: [u8; 32] = [0x11; 32];
const REVISION_A: [u8; 32] = [0x22; 32];

fn configuration(path: &Path) -> DatabaseConfigurationView<'_> {
    DatabaseConfigurationView {
        path,
        busy_timeout: DatabaseBusyTimeout::from_milliseconds(4_321).unwrap(),
        writer_queue_capacity: DatabaseWriterQueueCapacity::new(8).unwrap(),
        read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
    }
}

async fn open_database() -> (tempfile::TempDir, BootstrappedDatabase) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("state/maincopy.db");
    let database = bootstrap(configuration(&path)).await.unwrap();
    (root, database)
}

async fn bootstrap_error(path: &Path) -> DatabaseStartupError {
    match bootstrap(configuration(path)).await {
        Ok(database) => {
            database.close().await.unwrap();
            panic!("database bootstrap unexpectedly succeeded");
        }
        Err(error) => error,
    }
}

async fn insert_site_revision(connection: &mut SqliteConnection, digest: &[u8; 32], version: i64) {
    sqlx::query(
        "INSERT INTO site_revisions (site_revision_digest, activated_at_ns, version) \
         VALUES (?, ?, ?)",
    )
    .bind(digest.as_slice())
    .bind(version * 10)
    .bind(version)
    .execute(connection)
    .await
    .unwrap();
}

struct SourceSyncShape<'value> {
    stage: &'value str,
    outcome: Option<&'value str>,
    has_commit: bool,
    has_digest: bool,
    failure_code: Option<&'value str>,
    is_finished: bool,
}

async fn insert_source_sync_shape(
    connection: &mut SqliteConnection,
    identifier_byte: u8,
    shape: SourceSyncShape<'_>,
) -> Result<(), sqlx::Error> {
    let source_sync_id = [identifier_byte; 16];
    let source_commit = [0x41_u8; 20];
    let content_digest = [0x42_u8; 32];
    sqlx::query(
        "INSERT INTO source_sync_operations (\
            source_sync_id, configuration_version, request_origin, stage, outcome, \
            source_commit, content_digest, failure_code, version, requested_at_ns, \
            updated_at_ns, finished_at_ns\
         ) VALUES (?, 1, 'startup', ?, ?, ?, ?, ?, 1, 1, 2, ?)",
    )
    .bind(source_sync_id.as_slice())
    .bind(shape.stage)
    .bind(shape.outcome)
    .bind(shape.has_commit.then_some(source_commit.as_slice()))
    .bind(shape.has_digest.then_some(content_digest.as_slice()))
    .bind(shape.failure_code)
    .bind(shape.is_finished.then_some(2_i64))
    .execute(connection)
    .await?;
    Ok(())
}

#[tokio::test]
async fn empty_directory_bootstraps_the_complete_core_schema() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("new-state/maincopy.db");
    let mut database = bootstrap(configuration(&path)).await.unwrap();

    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    )
    .fetch_all(&mut database._writer)
    .await
    .unwrap();
    assert_eq!(
        tables,
        [
            "_sqlx_migrations",
            "admin_audit_events",
            "admin_identity_mutation_receipts",
            "admin_profile_mutation_receipts",
            "agent_credential_scopes",
            "agent_credentials",
            "browser_sessions",
            "canonical_publications",
            "instance_identity",
            "login_challenges",
            "nip98_replay_events",
            "post_revisions",
            "publication_routes",
            "reload_operations",
            "reload_post_changes",
            "site_revisions",
            "site_state",
            "site_tip_recipient",
            "source_configuration",
            "source_configuration_mutation_receipts",
            "source_configuration_revisions",
            "source_installation",
            "source_sync_idempotency_aliases",
            "source_sync_operations",
            "user_nostr_credentials",
            "user_password_credentials",
            "user_profiles",
            "user_roles",
            "users",
        ]
    );
    let migrations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success = TRUE")
            .fetch_one(&mut database._writer)
            .await
            .unwrap();
    assert_eq!(migrations, latest_schema_version());

    database.close().await.unwrap();
    assert!(path.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn bootstrap_is_idempotent_and_completes_each_empty_initial_version() {
    for retained_version in 1..latest_schema_version() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(format!("v{retained_version}/maincopy.db"));
        prepare_database_file(&path).unwrap();
        let mut connection = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(false)
            .connect()
            .await
            .unwrap();
        sqlx::query("PRAGMA application_id = 1296257113")
            .execute(&mut connection)
            .await
            .unwrap();
        MIGRATOR
            .run_to(retained_version, &mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();

        let mut upgraded = bootstrap(configuration(&path)).await.unwrap();
        let version: i64 =
            sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success = TRUE")
                .fetch_one(&mut upgraded._writer)
                .await
                .unwrap();
        assert_eq!(version, latest_schema_version());
        upgraded.close().await.unwrap();
    }

    let (root, database) = open_database().await;
    let path = root.path().join("state/maincopy.db");
    database.close().await.unwrap();
    let reopened = bootstrap(configuration(&path)).await.unwrap();
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn managed_source_migration_preserves_existing_agent_scopes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("v5/maincopy.db");
    prepare_database_file(&path).unwrap();
    let mut connection = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(false)
        .connect()
        .await
        .unwrap();
    sqlx::query("PRAGMA application_id = 1296257113")
        .execute(&mut connection)
        .await
        .unwrap();
    MIGRATOR.run_to(5, &mut connection).await.unwrap();

    let user_id = [0x31_u8; 16];
    let credential_id = [0x32_u8; 16];
    sqlx::query(
        "INSERT INTO users (user_id, status, version, created_at_ns, updated_at_ns) \
         VALUES (?, 'enabled', 1, 10, 10)",
    )
    .bind(user_id.as_slice())
    .execute(&mut connection)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_credentials (\
            agent_credential_id, owner_user_id, issuer_user_id, public_key, \
            label, version, created_at_ns\
         ) VALUES (?, ?, ?, ?, 'pre-source agent', 1, 10)",
    )
    .bind(credential_id.as_slice())
    .bind(user_id.as_slice())
    .bind(user_id.as_slice())
    .bind([0x33_u8; 32].as_slice())
    .execute(&mut connection)
    .await
    .unwrap();
    for scope in ["content_read", "status_read"] {
        sqlx::query(
            "INSERT INTO agent_credential_scopes (agent_credential_id, scope) \
             VALUES (?, ?)",
        )
        .bind(credential_id.as_slice())
        .bind(scope)
        .execute(&mut connection)
        .await
        .unwrap();
    }
    connection.close().await.unwrap();

    let mut upgraded = bootstrap(configuration(&path)).await.unwrap();
    for scope in ["source_sync", "source_manage"] {
        sqlx::query(
            "INSERT INTO agent_credential_scopes (agent_credential_id, scope) \
             VALUES (?, ?)",
        )
        .bind(credential_id.as_slice())
        .bind(scope)
        .execute(&mut upgraded._writer)
        .await
        .unwrap();
    }
    let scopes: Vec<String> = sqlx::query_scalar(
        "SELECT scope FROM agent_credential_scopes \
         WHERE agent_credential_id = ? ORDER BY scope",
    )
    .bind(credential_id.as_slice())
    .fetch_all(&mut upgraded._writer)
    .await
    .unwrap();
    assert_eq!(
        scopes,
        [
            "content_read",
            "source_manage",
            "source_sync",
            "status_read",
        ]
    );
    let source_tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema \
         WHERE type = 'table' AND name GLOB 'source_*' ORDER BY name",
    )
    .fetch_all(&mut upgraded._writer)
    .await
    .unwrap();
    assert_eq!(
        source_tables,
        [
            "source_configuration",
            "source_configuration_mutation_receipts",
            "source_configuration_revisions",
            "source_installation",
            "source_sync_idempotency_aliases",
            "source_sync_operations",
        ]
    );

    upgraded.close().await.unwrap();
}

#[tokio::test]
async fn a_second_live_bootstrap_of_the_same_database_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("state/maincopy.db");
    let database = bootstrap(configuration(&path)).await.unwrap();

    assert!(matches!(
        bootstrap_error(&path).await,
        DatabaseStartupError::AlreadyOwned
    ));

    database.close().await.unwrap();
    let reopened = bootstrap(configuration(&path)).await.unwrap();
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn connection_pragmas_and_foreign_keys_are_verified() {
    let (_root, mut database) = open_database().await;
    let connection = &mut database._writer;

    let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    assert_eq!(journal, "wal");
    let locking_mode: String = sqlx::query_scalar("PRAGMA locking_mode")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    assert_eq!(locking_mode, "normal");
    for (query, expected) in [
        ("PRAGMA synchronous", 1_i64),
        ("PRAGMA foreign_keys", 1),
        ("PRAGMA busy_timeout", 4_321),
        ("PRAGMA trusted_schema", 0),
        ("PRAGMA ignore_check_constraints", 0),
        ("PRAGMA query_only", 0),
        ("PRAGMA cell_size_check", 1),
        ("PRAGMA application_id", APPLICATION_ID),
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(query)
                .fetch_one(&mut *connection)
                .await
                .unwrap(),
            expected
        );
    }
    let version: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    assert!(parse_version(&version).unwrap() >= MINIMUM_SAFE_SQLITE_VERSION);

    let error = sqlx::query(
        "INSERT INTO site_state (singleton, current_site_digest, version) \
         VALUES (1, ?, 1)",
    )
    .bind(SITE_A.as_slice())
    .execute(&mut *connection)
    .await
    .unwrap_err();
    assert!(
        error
            .as_database_error()
            .is_some_and(|error| error.is_foreign_key_violation())
    );

    insert_site_revision(connection, &SITE_A, 1).await;
    sqlx::query(
        "INSERT INTO site_state (singleton, current_site_digest, version) \
         VALUES (1, ?, 1)",
    )
    .bind(SITE_A.as_slice())
    .execute(&mut *connection)
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT current_site_digest FROM site_state WHERE singleton = 1",
        )
        .fetch_one(&mut *connection)
        .await
        .unwrap(),
        SITE_A
    );

    let mut reader = database._readers.acquire().await.unwrap();
    for (query, expected) in [
        ("PRAGMA foreign_keys", 1_i64),
        ("PRAGMA busy_timeout", 4_321),
        ("PRAGMA trusted_schema", 0),
        ("PRAGMA ignore_check_constraints", 0),
        ("PRAGMA query_only", 1),
        ("PRAGMA cell_size_check", 1),
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(query)
                .fetch_one(&mut *reader)
                .await
                .unwrap(),
            expected
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT current_site_digest FROM site_state WHERE singleton = 1",
        )
        .fetch_one(&mut *reader)
        .await
        .unwrap(),
        SITE_A
    );
    assert!(
        sqlx::query(
            "INSERT INTO site_revisions (site_revision_digest, activated_at_ns, version) \
             VALUES (?, 20, 2)",
        )
        .bind(REVISION_A.as_slice())
        .execute(&mut *reader)
        .await
        .is_err()
    );
    drop(reader);
    database.close().await.unwrap();
}

#[tokio::test]
async fn managed_source_configuration_versions_reference_immutable_revisions() {
    let (_root, mut database) = open_database().await;
    let references: Vec<String> = sqlx::query_scalar(
        "SELECT 'source_installation.' || \"from\" || '->' || \"table\" || '.' || \"to\" \
         FROM pragma_foreign_key_list('source_installation') \
         UNION ALL \
         SELECT 'source_sync_operations.' || \"from\" || '->' || \"table\" || '.' || \"to\" \
         FROM pragma_foreign_key_list('source_sync_operations') \
         ORDER BY 1",
    )
    .fetch_all(&mut database._writer)
    .await
    .unwrap();
    assert_eq!(
        references,
        [
            "source_installation.configuration_version->source_configuration_revisions.version",
            "source_installation.source_sync_id->source_sync_operations.source_sync_id",
            "source_sync_operations.configuration_version->source_configuration_revisions.version",
        ]
    );
}

#[tokio::test]
async fn managed_source_sync_constraints_match_the_validated_lifecycle_shapes() {
    let (_root, mut database) = open_database().await;
    sqlx::query(
        "INSERT INTO source_configuration_revisions (\
            version, ssh_user, ssh_host, ssh_port, repository_path, branch, \
            content_subdirectory, credential_name, poll_interval_seconds, updated_at_ns\
         ) VALUES (1, 'git', 'git.example.test', 22, 'publisher/site.git', 'main', \
            'publication', 'deploy', 300, 1)",
    )
    .execute(&mut database._writer)
    .await
    .unwrap();

    let valid_shapes = [
        SourceSyncShape {
            stage: "queued",
            outcome: None,
            has_commit: false,
            has_digest: false,
            failure_code: None,
            is_finished: false,
        },
        SourceSyncShape {
            stage: "fetching",
            outcome: None,
            has_commit: false,
            has_digest: false,
            failure_code: None,
            is_finished: false,
        },
        SourceSyncShape {
            stage: "resolving_commit",
            outcome: None,
            has_commit: false,
            has_digest: false,
            failure_code: None,
            is_finished: false,
        },
        SourceSyncShape {
            stage: "preparing_candidate",
            outcome: None,
            has_commit: true,
            has_digest: false,
            failure_code: None,
            is_finished: false,
        },
        SourceSyncShape {
            stage: "compiling",
            outcome: None,
            has_commit: true,
            has_digest: false,
            failure_code: None,
            is_finished: false,
        },
        SourceSyncShape {
            stage: "reloading",
            outcome: None,
            has_commit: true,
            has_digest: true,
            failure_code: None,
            is_finished: false,
        },
        SourceSyncShape {
            stage: "reloading",
            outcome: Some("applied"),
            has_commit: true,
            has_digest: true,
            failure_code: None,
            is_finished: true,
        },
        SourceSyncShape {
            stage: "resolving_commit",
            outcome: Some("no_change"),
            has_commit: true,
            has_digest: true,
            failure_code: None,
            is_finished: true,
        },
        SourceSyncShape {
            stage: "fetching",
            outcome: Some("failed"),
            has_commit: false,
            has_digest: false,
            failure_code: Some("fetch_failed"),
            is_finished: true,
        },
        SourceSyncShape {
            stage: "compiling",
            outcome: Some("cancelled"),
            has_commit: true,
            has_digest: false,
            failure_code: None,
            is_finished: true,
        },
    ];
    for (index, shape) in valid_shapes.into_iter().enumerate() {
        let identifier_byte = u8::try_from(index + 1).unwrap();
        insert_source_sync_shape(&mut database._writer, identifier_byte, shape)
            .await
            .unwrap();
        sqlx::query("DELETE FROM source_sync_operations WHERE source_sync_id = ?")
            .bind([identifier_byte; 16].as_slice())
            .execute(&mut database._writer)
            .await
            .unwrap();
    }

    let invalid_shapes = [
        SourceSyncShape {
            stage: "queued",
            outcome: None,
            has_commit: false,
            has_digest: false,
            failure_code: Some("fetch_failed"),
            is_finished: false,
        },
        SourceSyncShape {
            stage: "queued",
            outcome: Some("applied"),
            has_commit: true,
            has_digest: true,
            failure_code: None,
            is_finished: true,
        },
        SourceSyncShape {
            stage: "reloading",
            outcome: Some("applied"),
            has_commit: true,
            has_digest: false,
            failure_code: None,
            is_finished: true,
        },
        SourceSyncShape {
            stage: "fetching",
            outcome: Some("no_change"),
            has_commit: true,
            has_digest: true,
            failure_code: None,
            is_finished: true,
        },
        SourceSyncShape {
            stage: "resolving_commit",
            outcome: Some("no_change"),
            has_commit: true,
            has_digest: false,
            failure_code: None,
            is_finished: true,
        },
        SourceSyncShape {
            stage: "queued",
            outcome: None,
            has_commit: true,
            has_digest: false,
            failure_code: None,
            is_finished: false,
        },
        SourceSyncShape {
            stage: "resolving_commit",
            outcome: None,
            has_commit: false,
            has_digest: true,
            failure_code: None,
            is_finished: false,
        },
        SourceSyncShape {
            stage: "preparing_candidate",
            outcome: None,
            has_commit: false,
            has_digest: false,
            failure_code: None,
            is_finished: false,
        },
        SourceSyncShape {
            stage: "compiling",
            outcome: None,
            has_commit: true,
            has_digest: true,
            failure_code: None,
            is_finished: false,
        },
        SourceSyncShape {
            stage: "reloading",
            outcome: None,
            has_commit: true,
            has_digest: false,
            failure_code: None,
            is_finished: false,
        },
    ];
    for (index, shape) in invalid_shapes.into_iter().enumerate() {
        let identifier_byte = u8::try_from(index + 0x41).unwrap();
        assert!(
            insert_source_sync_shape(&mut database._writer, identifier_byte, shape)
                .await
                .is_err()
        );
    }

    database.close().await.unwrap();
}

#[tokio::test]
async fn foreign_and_unmarked_databases_are_rejected_without_migration() {
    for (application_id, expected) in [(0_i64, "unmarked"), (7_i64, "foreign")] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/maincopy.db");
        prepare_database_file(&path).unwrap();
        let mut connection = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(false)
            .connect()
            .await
            .unwrap();
        connection
            .execute("CREATE TABLE unrelated (id INTEGER)")
            .await
            .unwrap();
        if application_id != 0 {
            connection
                .execute("PRAGMA application_id = 7")
                .await
                .unwrap();
        }
        connection.close().await.unwrap();

        let error = bootstrap_error(&path).await;
        match expected {
            "unmarked" => assert!(matches!(error, DatabaseStartupError::UnmarkedDatabase)),
            "foreign" => assert!(matches!(
                error,
                DatabaseStartupError::ForeignDatabase { found: 7 }
            )),
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn marked_schema_without_a_migration_ledger_is_rejected_without_modification() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("state/maincopy.db");
    prepare_database_file(&path).unwrap();
    let mut connection = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(false)
        .connect()
        .await
        .unwrap();
    connection
        .execute("PRAGMA application_id = 1296257113")
        .await
        .unwrap();
    connection
        .execute("CREATE TABLE untracked (id INTEGER PRIMARY KEY)")
        .await
        .unwrap();
    connection.close().await.unwrap();
    let before = fs::read(&path).unwrap();

    assert!(matches!(
        bootstrap_error(&path).await,
        DatabaseStartupError::MigrationLedgerMissing
    ));
    assert_eq!(fs::read(&path).unwrap(), before);

    let mut connection = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(false)
        .connect()
        .await
        .unwrap();
    let application_id: i64 = sqlx::query_scalar("PRAGMA application_id")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(application_id, APPLICATION_ID);
    let schema_objects: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema \
         WHERE name NOT GLOB 'sqlite_*' ORDER BY name",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    assert_eq!(schema_objects, ["untracked"]);
    connection.close().await.unwrap();
}

#[tokio::test]
async fn newer_modified_missing_and_dirty_migration_history_are_distinct() {
    async fn prepared() -> (tempfile::TempDir, std::path::PathBuf) {
        let (root, database) = open_database().await;
        let path = root.path().join("state/maincopy.db");
        database.close().await.unwrap();
        (root, path)
    }

    let (_root, path) = prepared().await;
    let future_version = latest_schema_version() + 1;
    let mut connection = SqliteConnectOptions::new()
        .filename(&path)
        .connect()
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (\
            version, description, success, checksum, execution_time\
        ) VALUES (?, 'future', TRUE, x'00', 0)",
    )
    .bind(future_version)
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
    assert!(matches!(
        bootstrap_error(&path).await,
        DatabaseStartupError::SchemaTooNew { database, binary }
            if database == future_version && binary == latest_schema_version()
    ));

    let (_root, path) = prepared().await;
    let mut connection = SqliteConnectOptions::new()
        .filename(&path)
        .connect()
        .await
        .unwrap();
    connection
        .execute("UPDATE _sqlx_migrations SET checksum = x'00' WHERE version = 3")
        .await
        .unwrap();
    connection.close().await.unwrap();
    let before = fs::read(&path).unwrap();
    assert!(matches!(
        bootstrap_error(&path).await,
        DatabaseStartupError::MigrationModified { version: 3 }
    ));
    assert_eq!(fs::read(&path).unwrap(), before);

    let (_root, path) = prepared().await;
    let mut connection = SqliteConnectOptions::new()
        .filename(&path)
        .connect()
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (\
            version, description, success, checksum, execution_time\
        ) VALUES (-1, 'missing', TRUE, x'00', 0)",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
    assert!(matches!(
        bootstrap_error(&path).await,
        DatabaseStartupError::MigrationMissing { version: -1 }
    ));

    let (_root, path) = prepared().await;
    let mut connection = SqliteConnectOptions::new()
        .filename(&path)
        .connect()
        .await
        .unwrap();
    connection
        .execute("UPDATE _sqlx_migrations SET success = FALSE WHERE version = 3")
        .await
        .unwrap();
    connection.close().await.unwrap();
    assert!(matches!(
        bootstrap_error(&path).await,
        DatabaseStartupError::MigrationDirty { version: 3 }
    ));
}

#[tokio::test]
async fn a_gap_in_retained_migration_history_is_rejected() {
    let (root, database) = open_database().await;
    let path = root.path().join("state/maincopy.db");
    database.close().await.unwrap();
    let mut connection = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(false)
        .connect()
        .await
        .unwrap();
    connection
        .execute("DELETE FROM _sqlx_migrations WHERE version = 2")
        .await
        .unwrap();
    connection.close().await.unwrap();

    assert!(matches!(
        bootstrap_error(&path).await,
        DatabaseStartupError::MigrationLedgerGap {
            missing: 2,
            found: 3
        }
    ));
}

#[tokio::test]
async fn application_schema_has_no_triggers_and_only_expected_explicit_indexes() {
    let (_root, mut database) = open_database().await;
    let connection = &mut database._writer;

    let triggers: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_schema WHERE type = 'trigger' ORDER BY name")
            .fetch_all(&mut *connection)
            .await
            .unwrap();
    assert!(triggers.is_empty());

    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema \
         WHERE type = 'index' AND sql IS NOT NULL ORDER BY name",
    )
    .fetch_all(&mut *connection)
    .await
    .unwrap();
    assert_eq!(
        indexes,
        [
            "admin_audit_events_idempotency_idx",
            "admin_audit_events_occurred_idx",
            "agent_credentials_owner_idx",
            "browser_sessions_user_idx",
            "login_challenges_cleanup_idx",
            "nip98_replay_events_cleanup_idx",
            "source_sync_history_idx",
            "source_sync_idempotency_alias_history_idx",
            "source_sync_one_nonterminal_idx",
            "user_roles_role_idx",
        ]
    );
}

#[tokio::test]
async fn identifiers_and_hashes_use_blob_storage() {
    let (_root, mut database) = open_database().await;
    let connection = &mut database._writer;

    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT schema.name || '.' || column_info.name || ':' || column_info.type \
         FROM sqlite_schema AS schema \
         JOIN pragma_table_info(schema.name) AS column_info \
         WHERE schema.type = 'table' \
           AND schema.name <> '_sqlx_migrations' \
           AND column_info.type = 'BLOB' \
         ORDER BY 1",
    )
    .fetch_all(&mut *connection)
    .await
    .unwrap();
    assert_eq!(
        columns,
        [
            "admin_audit_events.actor_user_id:BLOB",
            "admin_audit_events.agent_credential_id:BLOB",
            "admin_audit_events.audit_event_id:BLOB",
            "admin_audit_events.idempotency_key:BLOB",
            "admin_audit_events.request_id:BLOB",
            "admin_audit_events.session_id:BLOB",
            "admin_identity_mutation_receipts.audit_event_id:BLOB",
            "admin_identity_mutation_receipts.command_fingerprint:BLOB",
            "admin_identity_mutation_receipts.idempotency_key:BLOB",
            "admin_identity_mutation_receipts.result_id:BLOB",
            "admin_profile_mutation_receipts.audit_event_id:BLOB",
            "admin_profile_mutation_receipts.command_fingerprint:BLOB",
            "admin_profile_mutation_receipts.idempotency_key:BLOB",
            "admin_profile_mutation_receipts.profile_user_id:BLOB",
            "admin_profile_mutation_receipts.recipient_user_id:BLOB",
            "agent_credential_scopes.agent_credential_id:BLOB",
            "agent_credentials.agent_credential_id:BLOB",
            "agent_credentials.issuer_user_id:BLOB",
            "agent_credentials.owner_user_id:BLOB",
            "agent_credentials.public_key:BLOB",
            "browser_sessions.csrf_token_digest:BLOB",
            "browser_sessions.session_id:BLOB",
            "browser_sessions.session_token_digest:BLOB",
            "browser_sessions.user_id:BLOB",
            "canonical_publications.accepted_preview_digest:BLOB",
            "canonical_publications.activation_site_digest:BLOB",
            "canonical_publications.content_tree_digest:BLOB",
            "canonical_publications.creation_key:BLOB",
            "canonical_publications.current_published_digest:BLOB",
            "canonical_publications.pinned_post_digest:BLOB",
            "canonical_publications.publication_id:BLOB",
            "canonical_publications.requested_revision_digest:BLOB",
            "canonical_publications.source_commit:BLOB",
            "canonical_publications.stable_post_id:BLOB",
            "instance_identity.instance_id:BLOB",
            "login_challenges.challenge_digest:BLOB",
            "login_challenges.challenge_id:BLOB",
            "nip98_replay_events.agent_credential_id:BLOB",
            "nip98_replay_events.event_id:BLOB",
            "nip98_replay_events.user_id:BLOB",
            "post_revisions.revision_digest:BLOB",
            "post_revisions.source_commit:BLOB",
            "post_revisions.stable_post_id:BLOB",
            "publication_routes.revision_digest:BLOB",
            "publication_routes.stable_post_id:BLOB",
            "reload_operations.candidate_site_digest:BLOB",
            "reload_operations.expected_site_digest:BLOB",
            "reload_operations.reload_operation_id:BLOB",
            "reload_post_changes.candidate_post_digest:BLOB",
            "reload_post_changes.expected_post_digest:BLOB",
            "reload_post_changes.reload_operation_id:BLOB",
            "reload_post_changes.stable_post_id:BLOB",
            "site_revisions.site_revision_digest:BLOB",
            "site_revisions.source_commit:BLOB",
            "site_state.current_site_digest:BLOB",
            "site_tip_recipient.recipient_user_id:BLOB",
            "source_configuration_mutation_receipts.audit_event_id:BLOB",
            "source_configuration_mutation_receipts.command_fingerprint:BLOB",
            "source_configuration_mutation_receipts.idempotency_key:BLOB",
            "source_installation.content_digest:BLOB",
            "source_installation.source_commit:BLOB",
            "source_installation.source_sync_id:BLOB",
            "source_sync_idempotency_aliases.audit_event_id:BLOB",
            "source_sync_idempotency_aliases.command_fingerprint:BLOB",
            "source_sync_idempotency_aliases.idempotency_key:BLOB",
            "source_sync_idempotency_aliases.source_sync_id:BLOB",
            "source_sync_operations.content_digest:BLOB",
            "source_sync_operations.source_commit:BLOB",
            "source_sync_operations.source_sync_id:BLOB",
            "user_nostr_credentials.public_key:BLOB",
            "user_nostr_credentials.user_id:BLOB",
            "user_password_credentials.user_id:BLOB",
            "user_profiles.user_id:BLOB",
            "user_roles.assigned_by_user_id:BLOB",
            "user_roles.user_id:BLOB",
            "users.user_id:BLOB",
        ]
    );

    let definitions: String = sqlx::query_scalar(
        "SELECT group_concat(sql, '') FROM sqlite_schema \
         WHERE type = 'table' \
           AND name NOT LIKE 'sqlite_%' \
           AND name <> '_sqlx_migrations'",
    )
    .fetch_one(&mut *connection)
    .await
    .unwrap();
    let compact_definitions: String = definitions
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    assert_eq!(compact_definitions.matches("check(").count(), 132);
    for constraint in [
        "check(singleton=1)",
        "check(length(site_revision_digest)=32)",
        "check(length(revision_digest)=32)",
        "check(length(candidate_site_digest)=32)",
        "check(length(publication_id)=16)",
        "check(length(stable_post_id)=16)",
        "check(length(idempotency_key)=16)",
        "check(length(reload_operation_id)=16)",
        "check(length(content_tree_digest)=32)",
        "check(length(accepted_preview_digest)=32)",
        "check(length(instance_id)=16)",
        "check(length(user_id)=16)",
        "check(length(session_id)=16)",
        "check(length(session_token_digest)=32)",
        "check(length(csrf_token_digest)=32)",
        "check(length(challenge_id)=16)",
        "check(length(challenge_digest)=32)",
        "check(length(agent_credential_id)=16)",
        "check(length(public_key)=32)",
        "check(length(event_id)=32)",
        "check(length(audit_event_id)=16)",
        "check(length(source_sync_id)=16)",
    ] {
        assert!(
            compact_definitions.contains(constraint),
            "missing storage constraint: {constraint}"
        );
    }

    let valid_id = vec![7_u8; 16];
    sqlx::query(
        "INSERT INTO post_revisions (\
            stable_post_id, revision_digest, slug, publication_status, first_observed_at_ns\
        ) VALUES (?, ?, 'valid', 'publishable', 1)",
    )
    .bind(&valid_id)
    .bind(REVISION_A.as_slice())
    .execute(&mut *connection)
    .await
    .unwrap();
    let stored: (String, i64, String, i64) = sqlx::query_as(
        "SELECT typeof(stable_post_id), length(stable_post_id), \
                typeof(revision_digest), length(revision_digest) \
         FROM post_revisions WHERE stable_post_id = ?",
    )
    .bind(&valid_id)
    .fetch_one(&mut *connection)
    .await
    .unwrap();
    assert_eq!(stored, ("blob".into(), 16, "blob".into(), 32));

    let publication_id = vec![12_u8; 16];
    let content_digest = vec![13_u8; 32];
    let accepted_preview_digest = vec![14_u8; 32];
    sqlx::query(
        "INSERT INTO canonical_publications (\
            publication_id, command_kind, stable_post_id, pinned_post_digest, content_tree_digest, \
            accepted_preview_digest, state, version, scheduled_at_ns\
         ) VALUES (?, 'scheduled', ?, ?, ?, ?, 'scheduled', 1, 1)",
    )
    .bind(&publication_id)
    .bind(&valid_id)
    .bind(REVISION_A.as_slice())
    .bind(&content_digest)
    .bind(&accepted_preview_digest)
    .execute(&mut *connection)
    .await
    .unwrap();
    let stored_bindings: (String, i64, String, i64) = sqlx::query_as(
        "SELECT typeof(content_tree_digest), length(content_tree_digest), \
                typeof(accepted_preview_digest), length(accepted_preview_digest) \
         FROM canonical_publications WHERE publication_id = ?",
    )
    .bind(&publication_id)
    .fetch_one(&mut *connection)
    .await
    .unwrap();
    assert_eq!(stored_bindings, ("blob".into(), 32, "blob".into(), 32));

    for statement in [
        "UPDATE canonical_publications SET content_tree_digest = NULL WHERE publication_id = ?",
        "UPDATE canonical_publications SET accepted_preview_digest = NULL WHERE publication_id = ?",
    ] {
        assert!(
            sqlx::query(statement)
                .bind(&publication_id)
                .execute(&mut *connection)
                .await
                .is_err()
        );
    }
    for (statement, malformed) in [
        (
            "UPDATE canonical_publications SET content_tree_digest = ? \
             WHERE publication_id = ?",
            vec![15_u8; 31],
        ),
        (
            "UPDATE canonical_publications SET content_tree_digest = ? \
             WHERE publication_id = ?",
            vec![16_u8; 33],
        ),
        (
            "UPDATE canonical_publications SET accepted_preview_digest = ? \
             WHERE publication_id = ?",
            vec![17_u8; 31],
        ),
        (
            "UPDATE canonical_publications SET accepted_preview_digest = ? \
             WHERE publication_id = ?",
            vec![18_u8; 33],
        ),
    ] {
        assert!(
            sqlx::query(statement)
                .bind(malformed)
                .bind(&publication_id)
                .execute(&mut *connection)
                .await
                .is_err()
        );
    }

    for invalid_id in [vec![8_u8; 15], vec![9_u8; 17]] {
        assert!(
            sqlx::query(
                "INSERT INTO post_revisions (\
                    stable_post_id, revision_digest, slug, publication_status, \
                    first_observed_at_ns\
                ) VALUES (?, ?, 'invalid', 'publishable', 2)",
            )
            .bind(invalid_id)
            .bind(REVISION_A.as_slice())
            .execute(&mut *connection)
            .await
            .is_err()
        );
    }

    for (stable_post_id, invalid_digest) in [
        (vec![10_u8; 16], vec![3_u8; 31]),
        (vec![11_u8; 16], vec![4_u8; 33]),
    ] {
        assert!(
            sqlx::query(
                "INSERT INTO post_revisions (\
                    stable_post_id, revision_digest, slug, publication_status, \
                    first_observed_at_ns\
                ) VALUES (?, ?, 'invalid-digest', 'publishable', 3)",
            )
            .bind(stable_post_id)
            .bind(invalid_digest)
            .execute(&mut *connection)
            .await
            .is_err()
        );
    }
}

#[test]
fn retained_migrations_run_inside_transactions() {
    let migrations = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut paths: Vec<_> = std::fs::read_dir(migrations)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "sql"))
        .collect();
    paths.sort();
    assert_eq!(paths.len(), MIGRATOR.migrations.len());

    for path in paths {
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(
            !source.to_ascii_lowercase().contains("-- no-transaction"),
            "{} disables SQLx migration transactions",
            path.display()
        );
    }
}
