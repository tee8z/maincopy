use std::path::Path;

use sqlx::{ConnectOptions as _, Connection as _, Executor as _, SqliteConnection};

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
            "canonical_publications",
            "post_revisions",
            "publication_jobs",
            "published_routes",
            "reload_operations",
            "reload_post_changes",
            "site_revisions",
            "site_state",
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
        .execute("UPDATE _sqlx_migrations SET checksum = x'00' WHERE version = 1")
        .await
        .unwrap();
    connection.close().await.unwrap();
    assert!(matches!(
        bootstrap_error(&path).await,
        DatabaseStartupError::MigrationModified { version: 1 }
    ));

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
async fn application_schema_has_no_triggers_or_explicit_indexes() {
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
    assert!(indexes.is_empty());
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
            "canonical_publications.activation_site_digest:BLOB",
            "canonical_publications.creation_key:BLOB",
            "canonical_publications.current_published_digest:BLOB",
            "canonical_publications.pinned_post_digest:BLOB",
            "canonical_publications.publication_id:BLOB",
            "canonical_publications.source_commit:BLOB",
            "canonical_publications.stable_post_id:BLOB",
            "post_revisions.revision_digest:BLOB",
            "post_revisions.source_commit:BLOB",
            "post_revisions.stable_post_id:BLOB",
            "publication_jobs.idempotency_key:BLOB",
            "publication_jobs.payload_digest:BLOB",
            "publication_jobs.publication_id:BLOB",
            "publication_jobs.publication_job_id:BLOB",
            "published_routes.revision_digest:BLOB",
            "published_routes.stable_post_id:BLOB",
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
    assert_eq!(compact_definitions.matches("check(").count(), 10);
    for constraint in [
        "check(singleton=1)",
        "check(length(site_revision_digest)=32)",
        "check(length(revision_digest)=32)",
        "check(length(candidate_site_digest)=32)",
        "check(length(payload_digest)=32)",
        "check(length(publication_id)=16)",
        "check(length(stable_post_id)=16)",
        "check(length(idempotency_key)=16)",
        "check(length(publication_job_id)=16)",
        "check(length(reload_operation_id)=16)",
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
