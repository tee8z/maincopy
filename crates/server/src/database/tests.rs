use std::path::Path;

use sqlx::{ConnectOptions as _, Connection as _, Executor as _, SqliteConnection};

use super::*;
use crate::{
    config::{DatabaseBusyTimeout, DatabaseReadPoolSize, DatabaseWriterQueueCapacity},
    distribution::TargetPayload,
};

const POST_A: &str = "11111111-1111-4111-8111-111111111111";
const POST_B: &str = "22222222-2222-4222-8222-222222222222";
const REVISION_A: &str =
    "post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111";
const REVISION_B: &str =
    "post-b3-v1-2222222222222222222222222222222222222222222222222222222222222222";
const REVISION_OTHER_POST: &str =
    "post-b3-v1-3333333333333333333333333333333333333333333333333333333333333333";
const SITE_A: &str = "site-b3-v1-1111111111111111111111111111111111111111111111111111111111111111";
const SITE_B: &str = "site-b3-v1-2222222222222222222222222222222222222222222222222222222222222222";
const CANONICAL_A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const CANONICAL_B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const JOB_A: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const RELOAD_A: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";

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

async fn insert_post(connection: &mut SqliteConnection, post_id: &str, digest: &str, slug: &str) {
    insert_post_with_status(connection, post_id, digest, slug, "publishable").await;
}

async fn insert_post_with_status(
    connection: &mut SqliteConnection,
    post_id: &str,
    digest: &str,
    slug: &str,
    publication_status: &str,
) {
    sqlx::query(
        "INSERT INTO post_revisions (\
            stable_post_id, revision_digest, slug, publication_status, first_observed_at_ns\
        ) VALUES (?, ?, ?, ?, 1)",
    )
    .bind(post_id)
    .bind(digest)
    .bind(slug)
    .bind(publication_status)
    .execute(connection)
    .await
    .unwrap();
}

async fn insert_site_revision(connection: &mut SqliteConnection, digest: &str, version: i64) {
    sqlx::query(
        "INSERT INTO site_revisions (site_revision_digest, activated_at_ns, version) \
         VALUES (?, ?, ?)",
    )
    .bind(digest)
    .bind(version * 10)
    .bind(version)
    .execute(connection)
    .await
    .unwrap();
}

async fn insert_initial_site(connection: &mut SqliteConnection, digest: &str) {
    insert_site_revision(connection, digest, 1).await;
    sqlx::query(
        "INSERT INTO site_state (singleton, current_site_digest, version) \
         VALUES (1, ?, 1)",
    )
    .bind(digest)
    .execute(connection)
    .await
    .unwrap();
}

async fn insert_scheduled_canonical(
    connection: &mut SqliteConnection,
    id: &str,
    post_id: &str,
    digest: &str,
) -> Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error> {
    sqlx::query(
        "INSERT INTO canonical_publications (\
            canonical_publication_id, stable_post_id, pinned_post_digest, state, \
            scheduled_at_ns, version\
        ) VALUES (?, ?, ?, 'scheduled', 10, 1)",
    )
    .bind(id)
    .bind(post_id)
    .bind(digest)
    .execute(connection)
    .await
}

async fn publish_canonical(connection: &mut SqliteConnection, id: &str, digest: &str) {
    sqlx::query(
        "UPDATE canonical_publications \
         SET state = 'activating', activation_at_ns = 11, version = 2 \
         WHERE canonical_publication_id = ?",
    )
    .bind(id)
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE canonical_publications \
         SET state = 'published', published_at_ns = activation_at_ns, \
             current_published_digest = ?, version = 3 \
         WHERE canonical_publication_id = ?",
    )
    .bind(digest)
    .bind(id)
    .execute(connection)
    .await
    .unwrap();
}

async fn install_reload_candidate(connection: &mut SqliteConnection) {
    insert_site_revision(connection, SITE_B, 2).await;
    sqlx::query(
        "UPDATE canonical_publications \
         SET current_published_digest = ?, version = 4 \
         WHERE canonical_publication_id = ?",
    )
    .bind(REVISION_B)
    .bind(CANONICAL_A)
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE site_state SET current_site_digest = ?, version = 2 \
         WHERE singleton = 1",
    )
    .bind(SITE_B)
    .execute(connection)
    .await
    .unwrap();
}

async fn insert_waiting_job(
    connection: &mut SqliteConnection,
    canonical_id: &str,
) -> Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error> {
    let payload = TargetPayload::new("copy").unwrap();
    sqlx::query(
        "INSERT INTO publication_jobs (\
            publication_job_id, canonical_publication_id, state, target, stable_post_id, \
            pinned_post_digest, scheduled_at_ns, payload_version, payload_body, \
            payload_digest, version\
        ) VALUES (?, ?, 'waiting_for_canonical', 'x', ?, ?, 10, ?, ?, ?, 1)",
    )
    .bind(JOB_A)
    .bind(canonical_id)
    .bind(POST_A)
    .bind(REVISION_A)
    .bind(i64::from(payload.version()))
    .bind(payload.body())
    .bind(payload.digest().as_str())
    .execute(connection)
    .await
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
    assert_eq!(migrations, 3);

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
        ("PRAGMA recursive_triggers", 1),
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
    .bind(SITE_A)
    .execute(&mut *connection)
    .await
    .unwrap_err();
    assert!(
        error
            .as_database_error()
            .is_some_and(|error| error.is_foreign_key_violation())
    );

    insert_post(connection, POST_A, REVISION_A, "first").await;
    insert_scheduled_canonical(connection, CANONICAL_A, POST_A, REVISION_A)
        .await
        .unwrap();
    publish_canonical(connection, CANONICAL_A, REVISION_A).await;
    sqlx::query(
        "INSERT INTO published_routes (\
            route, kind, stable_post_id, revision_digest, claimed_at_ns\
        ) VALUES ('/posts/first', 'slug', ?, ?, 1)",
    )
    .bind(POST_A)
    .bind(REVISION_A)
    .execute(&mut *connection)
    .await
    .unwrap();
    let maximum_route = format!("/posts/{}", "a".repeat(1024));
    sqlx::query(
        "INSERT INTO published_routes (\
            route, kind, stable_post_id, revision_digest, claimed_at_ns\
        ) VALUES (?, 'alias', ?, ?, 1)",
    )
    .bind(maximum_route)
    .bind(POST_A)
    .bind(REVISION_A)
    .execute(&mut *connection)
    .await
    .unwrap();
    let oversized_route = format!("/posts/{}", "a".repeat(1025));
    assert!(
        sqlx::query(
            "INSERT INTO published_routes (\
                route, kind, stable_post_id, revision_digest, claimed_at_ns\
            ) VALUES (?, 'alias', ?, ?, 1)",
        )
        .bind(oversized_route)
        .bind(POST_A)
        .bind(REVISION_A)
        .execute(&mut *connection)
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO published_routes (\
                route, kind, stable_post_id, revision_digest, claimed_at_ns\
            ) VALUES (?, 'alias', ?, ?, 1)",
        )
        .bind("/posts/first\0hidden")
        .bind(POST_A)
        .bind(REVISION_A)
        .execute(&mut *connection)
        .await
        .is_err()
    );
    for invalid_route in ["/", "/admin", "/posts/../admin", "/posts/bad--alias"] {
        assert!(
            sqlx::query(
                "INSERT INTO published_routes (\
                    route, kind, stable_post_id, revision_digest, claimed_at_ns\
                ) VALUES (?, 'alias', ?, ?, 1)",
            )
            .bind(invalid_route)
            .bind(POST_A)
            .bind(REVISION_A)
            .execute(&mut *connection)
            .await
            .is_err()
        );
    }
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
    let mut connection = SqliteConnectOptions::new()
        .filename(&path)
        .connect()
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (\
            version, description, success, checksum, execution_time\
        ) VALUES (4, 'future', TRUE, x'00', 0)",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
    assert!(matches!(
        bootstrap_error(&path).await,
        DatabaseStartupError::SchemaTooNew {
            database: 4,
            binary: 3
        }
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
async fn canonical_shapes_uniqueness_retry_and_replacement_are_enforced() {
    let (_root, mut database) = open_database().await;
    let connection = &mut database._writer;
    insert_post(connection, POST_A, REVISION_A, "first").await;
    insert_post(connection, POST_A, REVISION_B, "second").await;

    insert_scheduled_canonical(connection, CANONICAL_A, POST_A, REVISION_A)
        .await
        .unwrap();
    assert!(
        insert_scheduled_canonical(connection, CANONICAL_B, POST_A, REVISION_B)
            .await
            .is_err()
    );
    assert!(
        sqlx::query(
            "UPDATE canonical_publications \
             SET published_at_ns = 10, version = 2 \
             WHERE canonical_publication_id = ?",
        )
        .bind(CANONICAL_A)
        .execute(&mut *connection)
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "UPDATE canonical_publications \
             SET state = 'cancelled', activation_at_ns = 10, \
                 block_reason = 'revision_unavailable', version = 2 \
             WHERE canonical_publication_id = ?",
        )
        .bind(CANONICAL_A)
        .execute(&mut *connection)
        .await
        .is_err()
    );

    sqlx::query(
        "UPDATE canonical_publications \
         SET state = 'activating', activation_at_ns = 10, version = 2 \
         WHERE canonical_publication_id = ?",
    )
    .bind(CANONICAL_A)
    .execute(&mut *connection)
    .await
    .unwrap();
    for statement in [
        "UPDATE canonical_publications \
         SET state = 'blocked', block_reason = NULL, version = 3 \
         WHERE canonical_publication_id = ?",
        "UPDATE canonical_publications \
         SET state = 'blocked', activation_at_ns = 11, \
             block_reason = 'revision_unavailable', version = 3 \
         WHERE canonical_publication_id = ?",
        "UPDATE canonical_publications \
         SET state = 'published', published_at_ns = NULL, \
             current_published_digest = 'post-b3-v1-1111111111111111111111111111111111111111111111111111111111111111', \
             version = 3 \
         WHERE canonical_publication_id = ?",
        "UPDATE canonical_publications \
         SET state = 'published', published_at_ns = activation_at_ns, \
             current_published_digest = 'post-b3-v1-2222222222222222222222222222222222222222222222222222222222222222', \
             version = 3 \
         WHERE canonical_publication_id = ?",
    ] {
        assert!(
            sqlx::query(statement)
                .bind(CANONICAL_A)
                .execute(&mut *connection)
                .await
                .is_err()
        );
    }
    sqlx::query(
        "UPDATE canonical_publications \
         SET state = 'blocked', block_reason = 'revision_unavailable', version = 3 \
         WHERE canonical_publication_id = ?",
    )
    .bind(CANONICAL_A)
    .execute(&mut *connection)
    .await
    .unwrap();
    assert!(
        sqlx::query(
            "UPDATE canonical_publications \
             SET state = 'cancelled', activation_at_ns = NULL, block_reason = NULL, \
                 version = 4 \
             WHERE canonical_publication_id = ?",
        )
        .bind(CANONICAL_A)
        .execute(&mut *connection)
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "UPDATE canonical_publications \
             SET state = 'activating', pinned_post_digest = ?, block_reason = NULL, \
                 activation_at_ns = 12, version = 4 \
             WHERE canonical_publication_id = ?",
        )
        .bind(REVISION_B)
        .bind(CANONICAL_A)
        .execute(&mut *connection)
        .await
        .is_err()
    );
    sqlx::query(
        "UPDATE canonical_publications \
         SET state = 'activating', block_reason = NULL, activation_at_ns = 12, version = 4 \
         WHERE canonical_publication_id = ?",
    )
    .bind(CANONICAL_A)
    .execute(&mut *connection)
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT pinned_post_digest FROM canonical_publications \
             WHERE canonical_publication_id = ?",
        )
        .bind(CANONICAL_A)
        .fetch_one(&mut *connection)
        .await
        .unwrap(),
        REVISION_A
    );

    sqlx::query(
        "UPDATE canonical_publications \
         SET state = 'blocked', block_reason = 'revision_unavailable', version = 5 \
         WHERE canonical_publication_id = ?",
    )
    .bind(CANONICAL_A)
    .execute(&mut *connection)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE canonical_publications SET state = 'cancelled', version = 6 \
         WHERE canonical_publication_id = ?",
    )
    .bind(CANONICAL_A)
    .execute(&mut *connection)
    .await
    .unwrap();
    assert!(
        insert_scheduled_canonical(connection, CANONICAL_B, POST_A, REVISION_A)
            .await
            .is_err()
    );
    insert_scheduled_canonical(connection, CANONICAL_B, POST_A, REVISION_B)
        .await
        .unwrap();
}

#[tokio::test]
async fn immutable_identity_text_rejects_embedded_nul_and_replace() {
    let (_root, mut database) = open_database().await;
    let connection = &mut database._writer;
    insert_post(connection, POST_A, REVISION_A, "first").await;

    assert!(
        sqlx::query(
            "INSERT OR REPLACE INTO post_revisions (\
                stable_post_id, revision_digest, slug, publication_status, \
                first_observed_at_ns\
            ) VALUES (?, ?, 'replacement', 'publishable', 2)",
        )
        .bind(POST_A)
        .bind(REVISION_A)
        .execute(&mut *connection)
        .await
        .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT slug FROM post_revisions \
             WHERE stable_post_id = ? AND revision_digest = ?",
        )
        .bind(POST_A)
        .bind(REVISION_A)
        .fetch_one(&mut *connection)
        .await
        .unwrap(),
        "first"
    );

    for (post_id, digest, slug, source_commit) in [
        (
            format!("{POST_B}\0hidden"),
            REVISION_OTHER_POST.into(),
            "safe",
            None,
        ),
        (
            POST_B.into(),
            format!("{REVISION_OTHER_POST}\0hidden"),
            "safe",
            None,
        ),
        (
            POST_B.into(),
            REVISION_OTHER_POST.into(),
            "safe\0hidden",
            None,
        ),
        (
            POST_B.into(),
            REVISION_OTHER_POST.into(),
            "safe",
            Some(format!("git-sha1:{}\0hidden", "a".repeat(40))),
        ),
    ] {
        assert!(
            sqlx::query(
                "INSERT INTO post_revisions (\
                    stable_post_id, revision_digest, slug, publication_status, source_commit, \
                    first_observed_at_ns\
                ) VALUES (?, ?, ?, 'publishable', ?, 1)",
            )
            .bind(post_id)
            .bind(digest)
            .bind(slug)
            .bind(source_commit)
            .execute(&mut *connection)
            .await
            .is_err()
        );
    }
}

#[tokio::test]
async fn draft_post_revisions_cannot_be_scheduled_for_publication() {
    let (_root, mut database) = open_database().await;
    let connection = &mut database._writer;
    insert_post_with_status(connection, POST_A, REVISION_A, "draft", "draft").await;

    assert!(
        insert_scheduled_canonical(connection, CANONICAL_A, POST_A, REVISION_A)
            .await
            .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO published_routes (\
                route, kind, stable_post_id, revision_digest, claimed_at_ns\
            ) VALUES ('/posts/draft', 'slug', ?, ?, 1)",
        )
        .bind(POST_A)
        .bind(REVISION_A)
        .execute(&mut *connection)
        .await
        .is_err()
    );
}

#[tokio::test]
async fn target_release_requires_the_matching_published_canonical_revision() {
    let (_root, mut database) = open_database().await;
    let connection = &mut database._writer;
    insert_post(connection, POST_A, REVISION_A, "first").await;
    insert_scheduled_canonical(connection, CANONICAL_A, POST_A, REVISION_A)
        .await
        .unwrap();
    insert_waiting_job(connection, CANONICAL_A).await.unwrap();

    assert!(
        sqlx::query(
            "UPDATE publication_jobs SET state = 'ready', version = 2 \
             WHERE publication_job_id = ?",
        )
        .bind(JOB_A)
        .execute(&mut *connection)
        .await
        .is_err()
    );
    sqlx::query(
        "UPDATE canonical_publications \
         SET state = 'activating', activation_at_ns = 11, version = 2 \
         WHERE canonical_publication_id = ?",
    )
    .bind(CANONICAL_A)
    .execute(&mut *connection)
    .await
    .unwrap();
    let mut transaction = connection.begin().await.unwrap();
    sqlx::query(
        "UPDATE canonical_publications \
         SET state = 'published', published_at_ns = activation_at_ns, \
             current_published_digest = ?, version = 3 \
         WHERE canonical_publication_id = ?",
    )
    .bind(REVISION_A)
    .bind(CANONICAL_A)
    .execute(&mut *transaction)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE publication_jobs SET state = 'ready', version = 2 \
         WHERE publication_job_id = ?",
    )
    .bind(JOB_A)
    .execute(&mut *transaction)
    .await
    .unwrap();
    transaction.commit().await.unwrap();
}

#[tokio::test]
async fn published_digest_requires_its_applying_reload_and_same_post() {
    let (_root, mut database) = open_database().await;
    let connection = &mut database._writer;
    insert_post(connection, POST_A, REVISION_A, "first").await;
    insert_post(connection, POST_A, REVISION_B, "second").await;
    insert_post(connection, POST_B, REVISION_OTHER_POST, "other").await;
    insert_scheduled_canonical(connection, CANONICAL_A, POST_A, REVISION_A)
        .await
        .unwrap();
    publish_canonical(connection, CANONICAL_A, REVISION_A).await;

    assert!(
        sqlx::query(
            "UPDATE canonical_publications \
             SET current_published_digest = ?, version = 4 \
             WHERE canonical_publication_id = ?",
        )
        .bind(REVISION_B)
        .bind(CANONICAL_A)
        .execute(&mut *connection)
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "UPDATE canonical_publications \
             SET current_published_digest = ?, version = 4 \
             WHERE canonical_publication_id = ?",
        )
        .bind(REVISION_OTHER_POST)
        .bind(CANONICAL_A)
        .execute(&mut *connection)
        .await
        .is_err()
    );
}

#[tokio::test]
async fn applying_reload_is_unique_and_requires_its_activated_candidate() {
    let (_root, mut database) = open_database().await;
    let connection = &mut database._writer;
    insert_initial_site(connection, SITE_A).await;
    insert_post(connection, POST_A, REVISION_A, "first").await;
    insert_post(connection, POST_A, REVISION_B, "second").await;
    insert_scheduled_canonical(connection, CANONICAL_A, POST_A, REVISION_A)
        .await
        .unwrap();
    publish_canonical(connection, CANONICAL_A, REVISION_A).await;

    for (id, state, finished_at, failure_code, version) in [
        (
            "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
            "applied",
            Some(11_i64),
            None,
            2_i64,
        ),
        (
            "aaaaaaaa-bbbb-4ccc-8ddd-ffffffffffff",
            "failed",
            Some(11),
            Some("rejected"),
            2,
        ),
        (
            "aaaaaaaa-bbbb-4ccc-8ddd-000000000000",
            "applying",
            None,
            None,
            2,
        ),
    ] {
        assert!(
            sqlx::query(
                "INSERT INTO reload_operations (\
                    reload_operation_id, state, expected_site_digest, candidate_site_digest, \
                    started_at_ns, finished_at_ns, failure_code, version\
                ) VALUES (?, ?, ?, ?, 10, ?, ?, ?)",
            )
            .bind(id)
            .bind(state)
            .bind(SITE_A)
            .bind(SITE_B)
            .bind(finished_at)
            .bind(failure_code)
            .bind(version)
            .execute(&mut *connection)
            .await
            .is_err()
        );
    }

    sqlx::query(
        "INSERT INTO reload_operations (\
            reload_operation_id, state, expected_site_digest, candidate_site_digest, \
            started_at_ns, version\
        ) VALUES (?, 'applying', ?, ?, 10, 1)",
    )
    .bind(RELOAD_A)
    .bind(SITE_A)
    .bind(SITE_B)
    .execute(&mut *connection)
    .await
    .unwrap();
    assert!(
        sqlx::query(
            "UPDATE reload_operations \
             SET state = 'failed', finished_at_ns = 11, version = 2 \
             WHERE reload_operation_id = ?",
        )
        .bind(RELOAD_A)
        .execute(&mut *connection)
        .await
        .is_err()
    );
    sqlx::query(
        "INSERT INTO reload_post_changes (\
            reload_operation_id, stable_post_id, expected_post_digest, candidate_post_digest\
        ) VALUES (?, ?, ?, ?)",
    )
    .bind(RELOAD_A)
    .bind(POST_A)
    .bind(REVISION_A)
    .bind(REVISION_B)
    .execute(&mut *connection)
    .await
    .unwrap();
    assert!(
        sqlx::query(
            "INSERT INTO reload_operations (\
                reload_operation_id, state, expected_site_digest, candidate_site_digest, \
                started_at_ns, version\
            ) VALUES ('eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee', 'applying', ?, ?, 11, 1)",
        )
        .bind(SITE_A)
        .bind(SITE_B)
        .execute(&mut *connection)
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "UPDATE reload_operations \
             SET state = 'applied', finished_at_ns = 12, version = 2 \
             WHERE reload_operation_id = ?",
        )
        .bind(RELOAD_A)
        .execute(&mut *connection)
        .await
        .is_err()
    );
    let mut rolled_back = connection.begin().await.unwrap();
    install_reload_candidate(&mut rolled_back).await;
    rolled_back.rollback().await.unwrap();
    let unchanged: (String, String, i64) = sqlx::query_as(
        "SELECT site_state.current_site_digest, \
                canonical_publications.current_published_digest, \
                EXISTS(SELECT 1 FROM site_revisions WHERE site_revision_digest = ?) \
         FROM site_state \
         JOIN canonical_publications ON canonical_publications.stable_post_id = ?",
    )
    .bind(SITE_B)
    .bind(POST_A)
    .fetch_one(&mut *connection)
    .await
    .unwrap();
    assert_eq!(unchanged, (SITE_A.into(), REVISION_A.into(), 0));

    let mut transaction = connection.begin().await.unwrap();
    install_reload_candidate(&mut transaction).await;
    sqlx::query(
        "UPDATE reload_operations \
         SET state = 'applied', finished_at_ns = 12, version = 2 \
         WHERE reload_operation_id = ?",
    )
    .bind(RELOAD_A)
    .execute(&mut *transaction)
    .await
    .unwrap();
    transaction.commit().await.unwrap();

    let applied: (String, String, String) = sqlx::query_as(
        "SELECT reload_operations.state, site_state.current_site_digest, \
                canonical_publications.current_published_digest \
         FROM reload_operations \
         CROSS JOIN site_state \
         JOIN canonical_publications ON canonical_publications.stable_post_id = ? \
         WHERE reload_operations.reload_operation_id = ?",
    )
    .bind(POST_A)
    .bind(RELOAD_A)
    .fetch_one(&mut *connection)
    .await
    .unwrap();
    assert_eq!(
        applied,
        ("applied".into(), SITE_B.into(), REVISION_B.into())
    );
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
