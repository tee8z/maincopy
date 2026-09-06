//! Read-only verification and explicitly accepted offline restore mutation.

use super::health::DatabaseHealth;
use super::{
    DatabaseIdentity, DatabaseStartupError, MIGRATOR, acquire_database_lock, inspect_database_file,
    inspect_database_identity, latest_schema_version, preflight_migration_ledger,
    verify_opened_file, verify_sqlite_version,
};
use crate::{
    database::DatabaseStore,
    domain::{
        auth::store::{AuthStore, invalidate_restored_credentials},
        profile::store::ProfileStore,
        publication::store::PublicationStore,
        source::store::SourceStore,
    },
    metrics::DatabaseMetrics,
    restore::{RestoreError, RestoreSchema},
};
use sqlx::{
    ConnectOptions as _, Connection as _, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    fs::File,
    path::{Path, PathBuf},
};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use uuid::Uuid;

pub(crate) struct RestoreInspection {
    pub(crate) store: DatabaseStore,
    pub(crate) schema: RestoreSchema,
    readers: sqlx::SqlitePool,
}

impl RestoreInspection {
    pub(crate) async fn close(self) {
        self.readers.close().await;
    }
}

pub(crate) async fn inspect(path: &Path) -> Result<RestoreInspection, RestoreError> {
    let expected_file = inspect_database_file(path)?;
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .read_only(true)
        .immutable(true)
        .foreign_keys(true)
        .pragma("trusted_schema", "OFF")
        .pragma("query_only", "ON")
        .pragma("cell_size_check", "ON");
    let mut connection = options.connect().await?;
    verify_opened_file(path, expected_file)?;
    let schema_result = verify_schema(&mut connection).await;
    let close_result = connection.close().await;
    let schema = schema_result?;
    close_result?;
    let readers = SqlitePoolOptions::new()
        .min_connections(2)
        .max_connections(2)
        .connect_with(options)
        .await?;
    if let Err(error) = verify_opened_file(path, expected_file) {
        readers.close().await;
        return Err(error.into());
    }
    // The private staged file remains under the offline restore's database lock.
    // Export inspects only the private host-job snapshot, never the live database.
    // All queries use a read-only, query-only pool. A closed admission channel
    // rejects accidental mutations before any writer or SQL operation exists.
    let (mutations, receiver) = mpsc::channel(1);
    drop(receiver);
    let metrics =
        DatabaseMetrics::new().map_err(|source| DatabaseStartupError::Metrics { source })?;
    let mut wal_path = path.as_os_str().to_owned();
    wal_path.push("-wal");
    let health = DatabaseHealth::new(
        metrics,
        readers.clone(),
        mutations.clone(),
        PathBuf::from(wal_path),
    );
    let store = DatabaseStore::new(
        AuthStore::new(readers.clone(), mutations.clone()),
        ProfileStore::new(readers.clone(), mutations.clone()),
        PublicationStore::new(readers.clone(), mutations.clone()),
        SourceStore::new(readers.clone(), mutations),
        health,
    );
    Ok(RestoreInspection {
        store,
        schema,
        readers,
    })
}

async fn verify_schema(connection: &mut SqliteConnection) -> Result<RestoreSchema, RestoreError> {
    if !matches!(
        inspect_database_identity(connection).await?,
        DatabaseIdentity::Maincopy
    ) {
        return Err(RestoreError::SchemaMismatch);
    }
    verify_sqlite_version(connection).await?;
    verify_schema_objects(connection).await?;
    preflight_migration_ledger(connection).await?;
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&mut *connection)
        .await?;
    if integrity != "ok" {
        return Err(RestoreError::Integrity);
    }
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_optional(&mut *connection)
        .await?;
    if violations.is_some() {
        return Err(RestoreError::Integrity);
    }
    let actual: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations WHERE success = 1",
    )
    .fetch_one(&mut *connection)
    .await?;
    if actual != latest_schema_version() {
        return Err(RestoreError::SchemaMismatch);
    }
    Ok(binary_schema())
}

pub(crate) fn binary_schema() -> RestoreSchema {
    let mut hasher = blake3::Hasher::new_derive_key("maincopy.restore.schema.v1");
    for migration in MIGRATOR.iter() {
        hasher.update(&migration.version.to_be_bytes());
        hasher.update(migration.checksum.as_ref());
    }
    RestoreSchema {
        version: latest_schema_version(),
        digest: *hasher.finalize().as_bytes(),
    }
}

pub(crate) async fn accept(path: &Path, restore_id: Uuid) -> Result<(), RestoreError> {
    let mut connection = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true)
        .pragma("trusted_schema", "OFF")
        .connect()
        .await?;
    let result = async {
        let mut transaction = connection.begin().await?;
        invalidate_restored_credentials(&mut transaction, restore_id, OffsetDateTime::now_utc())
            .await?;
        transaction.commit().await?;
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&mut connection)
            .await?;
        Ok::<(), RestoreError>(())
    }
    .await;
    let closed = connection.close().await;
    result?;
    closed?;
    Ok(())
}

/// Compare the actual schema, including triggers, against this exact migration set.
/// Ledger checksums alone cannot detect objects changed outside migrations.
async fn verify_schema_objects(connection: &mut SqliteConnection) -> Result<(), RestoreError> {
    let mut reference = SqliteConnectOptions::new()
        .in_memory(true)
        .connect()
        .await?;
    MIGRATOR
        .run(&mut reference)
        .await
        .map_err(super::map_migration_error)?;
    let replicated = prepare_replication_reference(connection, &mut reference).await?;
    let expected = schema_objects(&mut reference).await;
    reference.close().await?;
    if schema_objects(connection).await? != expected? {
        return Err(RestoreError::SchemaMismatch);
    }
    if replicated {
        verify_replication_rows(connection).await?;
    }
    Ok(())
}

/// Litestream 0.5.17 owns exactly these two bookkeeping tables.
/// Establish their reference shape before querying any restored table contents.
async fn prepare_replication_reference(
    connection: &mut SqliteConnection,
    reference: &mut SqliteConnection,
) -> Result<bool, RestoreError> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name IN ('_litestream_seq', '_litestream_lock')").fetch_one(&mut *connection).await?;
    match count {
        0 => return Ok(false),
        2 => {}
        _ => return Err(RestoreError::SchemaMismatch),
    }
    sqlx::query("CREATE TABLE _litestream_seq (id INTEGER PRIMARY KEY, seq INTEGER)")
        .execute(&mut *reference)
        .await?;
    sqlx::query("CREATE TABLE _litestream_lock (id INTEGER)")
        .execute(reference)
        .await?;
    Ok(true)
}

async fn verify_replication_rows(connection: &mut SqliteConnection) -> Result<(), RestoreError> {
    let valid: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM _litestream_lock) AND NOT EXISTS(SELECT 1 FROM _litestream_seq WHERE id != 1 OR typeof(seq) != 'integer' OR seq < 1)").fetch_one(connection).await?;
    if !valid {
        return Err(RestoreError::Integrity);
    }
    Ok(())
}

async fn schema_objects(
    connection: &mut SqliteConnection,
) -> Result<Vec<(String, String, String, Option<String>)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name LIMIT 1025",
    )
    .fetch_all(connection)
    .await
}

/// Owns offline database access without letting inherited descriptors extend it.
pub(crate) struct RestoreOwnership {
    file: File,
}

impl Drop for RestoreOwnership {
    fn drop(&mut self) {
        // A concurrent fork duplicates the descriptor before close-on-exec runs.
        // Closing our descriptor alone would leave its shared flock in the child.
        if let Err(error) = self.file.unlock() {
            tracing::error!(%error, "restore database ownership release failed");
        }
    }
}

pub(crate) fn acquire_ownership(path: &Path) -> Result<RestoreOwnership, RestoreError> {
    Ok(RestoreOwnership {
        file: acquire_database_lock(path)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
        DatabaseWriterQueueCapacity,
    };

    #[test]
    #[cfg(unix)]
    fn restore_ownership_drop_unlocks_a_descriptor_inherited_before_exec() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("maincopy.db");
        let ownership = acquire_ownership(&path).unwrap();
        let inherited_descriptor = ownership.file.try_clone().unwrap();

        drop(ownership);

        let _reopened = acquire_ownership(&path).unwrap();
        drop(inherited_descriptor);
    }

    #[tokio::test]
    async fn only_exact_litestream_bookkeeping_tables_and_valid_rows_extend_the_schema() {
        let mut connection = SqliteConnectOptions::new()
            .in_memory(true)
            .connect()
            .await
            .unwrap();
        MIGRATOR.run(&mut connection).await.unwrap();
        sqlx::query("CREATE TABLE _litestream_seq (id INTEGER PRIMARY KEY, seq INTEGER)")
            .execute(&mut connection)
            .await
            .unwrap();
        assert!(matches!(
            verify_schema_objects(&mut connection).await,
            Err(RestoreError::SchemaMismatch)
        ));
        sqlx::query("CREATE TABLE _litestream_lock (id INTEGER)")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("INSERT INTO _litestream_seq VALUES (1, 7)")
            .execute(&mut connection)
            .await
            .unwrap();
        verify_schema_objects(&mut connection).await.unwrap();
        sqlx::query("INSERT INTO _litestream_lock VALUES (1)")
            .execute(&mut connection)
            .await
            .unwrap();
        assert!(matches!(
            verify_schema_objects(&mut connection).await,
            Err(RestoreError::Integrity)
        ));
        sqlx::query("DELETE FROM _litestream_lock")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("UPDATE _litestream_seq SET seq = 'corrupt'")
            .execute(&mut connection)
            .await
            .unwrap();
        assert!(matches!(
            verify_schema_objects(&mut connection).await,
            Err(RestoreError::Integrity)
        ));
        sqlx::query("UPDATE _litestream_seq SET seq = 7")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("CREATE INDEX unexpected_replica_index ON _litestream_seq(seq)")
            .execute(&mut connection)
            .await
            .unwrap();
        assert!(matches!(
            verify_schema_objects(&mut connection).await,
            Err(RestoreError::SchemaMismatch)
        ));
        connection.close().await.unwrap();
    }

    #[tokio::test]
    async fn read_only_restore_inspection_rejects_schema_changes_with_an_unchanged_migration_ledger()
     {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/maincopy.db");
        let configuration = DatabaseConfigurationView {
            path: &path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(8).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        };
        super::super::bootstrap(configuration)
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        inspect(&path).await.unwrap().close().await;
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let mut connection = SqliteConnectOptions::new()
            .filename(&path)
            .connect()
            .await
            .unwrap();
        for (create, remove) in [
            (
                "CREATE TRIGGER altered_restore AFTER UPDATE ON instance_identity BEGIN DELETE FROM browser_sessions; END",
                "DROP TRIGGER altered_restore",
            ),
            (
                "CREATE TRIGGER sqliteXrestore AFTER UPDATE ON browser_sessions BEGIN UPDATE browser_sessions SET revoked_at_ns = NULL, instance_version = (SELECT version FROM instance_identity WHERE singleton = 1); END",
                "DROP TRIGGER sqliteXrestore",
            ),
        ] {
            sqlx::query(create).execute(&mut connection).await.unwrap();
            connection.close().await.unwrap();
            let changed = std::fs::read(&path).unwrap();
            assert!(matches!(
                inspect(&path).await,
                Err(RestoreError::SchemaMismatch)
            ));
            assert_eq!(std::fs::read(&path).unwrap(), changed);
            connection = SqliteConnectOptions::new()
                .filename(&path)
                .connect()
                .await
                .unwrap();
            sqlx::query(remove).execute(&mut connection).await.unwrap();
        }
        sqlx::query("CREATE TRIGGER sqliteXhidden AFTER UPDATE ON browser_sessions BEGIN UPDATE browser_sessions SET revoked_at_ns = NULL; END").execute(&mut connection).await.unwrap();
        sqlx::query("PRAGMA writable_schema = ON")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("UPDATE sqlite_schema SET name = 'sqlite_hidden', sql = replace(sql, 'sqliteXhidden', 'sqlite_hidden') WHERE name = 'sqliteXhidden'").execute(&mut connection).await.unwrap();
        sqlx::query("PRAGMA writable_schema = OFF")
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
        let changed = std::fs::read(&path).unwrap();
        assert!(matches!(
            inspect(&path).await,
            Err(RestoreError::SchemaMismatch)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), changed);
    }
}
