//! SQLite schema bootstrap and single-writer ownership.

pub(crate) mod store;
mod writer;

pub(crate) use store::DatabaseStore;

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

use sqlx::{
    ConnectOptions as _, Connection as _, SqliteConnection,
    migrate::{MigrateError, Migrator},
    sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions},
};
use thiserror::Error;

use crate::{
    config::DatabaseConfigurationView,
    process_lock::{open_private_file, prepare_private_directory},
};

const APPLICATION_ID: i64 = 0x4D43_5059;
const MINIMUM_SAFE_SQLITE_VERSION: (u64, u64, u64) = (3, 51, 3);

static MIGRATOR: Migrator = sqlx::migrate!();

/// Owns the connection and database lock until startup creates the writer task.
pub(crate) struct BootstrappedDatabase {
    _writer: SqliteConnection,
    _readers: SqlitePool,
    _ownership_lock: File,
}

impl BootstrappedDatabase {
    pub(crate) async fn close(self) -> Result<(), sqlx::Error> {
        let Self {
            _writer,
            _readers,
            _ownership_lock,
        } = self;
        _readers.close().await;
        let result = _writer.close().await;
        if result.is_err() {
            tracing::error!("database writer close failed");
        }
        drop(_ownership_lock);
        result
    }
}

pub(crate) async fn bootstrap(
    configuration: DatabaseConfigurationView<'_>,
) -> Result<BootstrappedDatabase, DatabaseStartupError> {
    prepare_database_parent(configuration.path)?;
    let ownership_lock = acquire_database_lock(configuration.path)?;
    let expected_file = prepare_database_file(configuration.path)?;

    let identity = preflight_database(configuration.path, expected_file).await?;
    let options = SqliteConnectOptions::new()
        .filename(configuration.path)
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(configuration.busy_timeout.get());
    let mut writer = options
        .connect()
        .await
        .map_err(|source| DatabaseStartupError::OpenWriter { source })?;
    let initialization = async {
        verify_opened_file(configuration.path, expected_file)?;

        if matches!(identity, DatabaseIdentity::Empty) {
            mark_database_as_maincopy(&mut writer).await?;
        }
        configure_connection(&mut writer, configuration.busy_timeout.get()).await?;

        MIGRATOR
            .run(&mut writer)
            .await
            .map_err(map_migration_error)?;
        verify_foreign_keys(&mut writer).await?;
        open_read_pool(configuration).await
    }
    .await;
    let readers = match initialization {
        Ok(readers) => readers,
        Err(error) => {
            if writer.close().await.is_err() {
                tracing::error!("database writer close failed during bootstrap rollback");
            }
            return Err(error);
        }
    };

    Ok(BootstrappedDatabase {
        _writer: writer,
        _readers: readers,
        _ownership_lock: ownership_lock,
    })
}

async fn open_read_pool(
    configuration: DatabaseConfigurationView<'_>,
) -> Result<SqlitePool, DatabaseStartupError> {
    let max_connections = u32::try_from(configuration.read_pool_size.get()).map_err(|_| {
        DatabaseStartupError::ConfigurationMismatch {
            setting: "read_pool_size",
        }
    })?;
    let options = SqliteConnectOptions::new()
        .filename(configuration.path)
        .create_if_missing(false)
        .read_only(true)
        .foreign_keys(true)
        .busy_timeout(configuration.busy_timeout.get())
        .pragma("trusted_schema", "OFF")
        .pragma("ignore_check_constraints", "OFF")
        .pragma("query_only", "ON")
        .pragma("cell_size_check", "ON")
        .pragma("locking_mode", "NORMAL");
    let pool = SqlitePoolOptions::new()
        .min_connections(1)
        .max_connections(max_connections)
        .connect_with(options)
        .await
        .map_err(|source| DatabaseStartupError::OpenReaders { source })?;

    let verification = async {
        let mut reader = pool
            .acquire()
            .await
            .map_err(|source| DatabaseStartupError::OpenReaders { source })?;
        verify_integer_pragma(&mut reader, "PRAGMA foreign_keys", 1, "foreign_keys").await?;
        verify_integer_pragma(&mut reader, "PRAGMA trusted_schema", 0, "trusted_schema").await?;
        verify_integer_pragma(
            &mut reader,
            "PRAGMA ignore_check_constraints",
            0,
            "ignore_check_constraints",
        )
        .await?;
        verify_integer_pragma(&mut reader, "PRAGMA query_only", 1, "query_only").await?;
        verify_integer_pragma(&mut reader, "PRAGMA cell_size_check", 1, "cell_size_check").await?;
        verify_text_pragma(&mut reader, "PRAGMA locking_mode", "normal", "locking_mode").await?;
        let expected_timeout = i64::try_from(configuration.busy_timeout.get().as_millis())
            .map_err(|_| DatabaseStartupError::ConfigurationMismatch {
                setting: "busy_timeout",
            })?;
        verify_integer_pragma(
            &mut reader,
            "PRAGMA busy_timeout",
            expected_timeout,
            "busy_timeout",
        )
        .await
    }
    .await;

    if let Err(error) = verification {
        pool.close().await;
        return Err(error);
    }
    Ok(pool)
}

fn prepare_database_parent(path: &Path) -> Result<(), DatabaseStartupError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(DatabaseStartupError::InvalidPath)?;
    prepare_private_directory(parent)
        .map_err(|source| DatabaseStartupError::PathPreparation { source })
}

fn prepare_database_file(path: &Path) -> Result<ExpectedFile, DatabaseStartupError> {
    prepare_database_parent(path)?;

    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);

    match options.open(path) {
        Ok(file) => {
            file.sync_all()
                .map_err(|source| DatabaseStartupError::PathPreparation { source })?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => return Err(DatabaseStartupError::PathPreparation { source }),
    }

    inspect_database_file(path)
}

fn acquire_database_lock(database_path: &Path) -> Result<File, DatabaseStartupError> {
    let file = open_private_file(&database_lock_path(database_path))
        .map_err(|source| DatabaseStartupError::OwnershipLock { source })?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(DatabaseStartupError::AlreadyOwned),
        Err(TryLockError::Error(source)) => Err(DatabaseStartupError::OwnershipLock { source }),
    }
}

fn database_lock_path(database_path: &Path) -> PathBuf {
    let mut lock_path = database_path.as_os_str().to_owned();
    lock_path.push(".lock");
    lock_path.into()
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct ExpectedFile {
    device: u64,
    inode: u64,
}

#[cfg(not(unix))]
#[derive(Clone, Copy)]
struct ExpectedFile;

#[cfg(unix)]
fn inspect_database_file(path: &Path) -> Result<ExpectedFile, DatabaseStartupError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| DatabaseStartupError::PathPreparation { source })?;
    if !metadata.file_type().is_file()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(DatabaseStartupError::UnsafeDatabaseFile);
    }
    Ok(ExpectedFile {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn inspect_database_file(path: &Path) -> Result<ExpectedFile, DatabaseStartupError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| DatabaseStartupError::PathPreparation { source })?;
    if !metadata.file_type().is_file() {
        return Err(DatabaseStartupError::UnsafeDatabaseFile);
    }
    Ok(ExpectedFile)
}

#[cfg(unix)]
fn verify_opened_file(path: &Path, expected: ExpectedFile) -> Result<(), DatabaseStartupError> {
    let actual = inspect_database_file(path)?;
    if actual.device != expected.device || actual.inode != expected.inode {
        return Err(DatabaseStartupError::DatabaseFileChanged);
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_opened_file(path: &Path, _expected: ExpectedFile) -> Result<(), DatabaseStartupError> {
    inspect_database_file(path).map(|_| ())
}

enum DatabaseIdentity {
    Empty,
    Maincopy,
}

async fn preflight_database(
    path: &Path,
    expected_file: ExpectedFile,
) -> Result<DatabaseIdentity, DatabaseStartupError> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .read_only(true);
    let mut inspector = options
        .connect()
        .await
        .map_err(|source| DatabaseStartupError::OpenInspector { source })?;
    let inspection = async {
        verify_opened_file(path, expected_file)?;
        let identity = inspect_database_identity(&mut inspector).await?;
        verify_sqlite_version(&mut inspector).await?;
        preflight_migration_ledger(&mut inspector).await?;
        Ok(identity)
    }
    .await;
    let close = inspector
        .close()
        .await
        .map_err(|source| DatabaseStartupError::Inspect { source });
    match (inspection, close) {
        (Err(error), close) => {
            if close.is_err() {
                tracing::error!("database inspector close failed after preflight failure");
            }
            Err(error)
        }
        (Ok(identity), Ok(())) => Ok(identity),
        (Ok(_), Err(error)) => Err(error),
    }
}

async fn inspect_database_identity(
    connection: &mut SqliteConnection,
) -> Result<DatabaseIdentity, DatabaseStartupError> {
    let application_id: i64 = sqlx::query_scalar("PRAGMA application_id")
        .fetch_one(&mut *connection)
        .await
        .map_err(|source| DatabaseStartupError::Inspect { source })?;
    let has_schema: bool = sqlx::query_scalar(
        "SELECT EXISTS(\
            SELECT 1 FROM sqlite_schema \
            WHERE name NOT GLOB 'sqlite_*'\
        )",
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(|source| DatabaseStartupError::Inspect { source })?;

    match (application_id, has_schema) {
        (0, false) => Ok(DatabaseIdentity::Empty),
        (APPLICATION_ID, _) => Ok(DatabaseIdentity::Maincopy),
        (0, true) => Err(DatabaseStartupError::UnmarkedDatabase),
        (found, _) => Err(DatabaseStartupError::ForeignDatabase { found }),
    }
}

async fn mark_database_as_maincopy(
    connection: &mut SqliteConnection,
) -> Result<(), DatabaseStartupError> {
    sqlx::query("PRAGMA application_id = 1296257113")
        .execute(&mut *connection)
        .await
        .map_err(|source| DatabaseStartupError::Initialize { source })?;
    verify_integer_pragma(
        connection,
        "PRAGMA application_id",
        APPLICATION_ID,
        "application_id",
    )
    .await
}

async fn preflight_migration_ledger(
    connection: &mut SqliteConnection,
) -> Result<(), DatabaseStartupError> {
    let has_migrations: bool = sqlx::query_scalar(
        "SELECT EXISTS(\
            SELECT 1 FROM sqlite_schema \
            WHERE type = 'table' AND name = '_sqlx_migrations'\
        )",
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(|source| DatabaseStartupError::Inspect { source })?;
    if !has_migrations {
        let has_untracked_schema: bool = sqlx::query_scalar(
            "SELECT EXISTS(\
                SELECT 1 FROM sqlite_schema \
                WHERE name NOT GLOB 'sqlite_*'\
            )",
        )
        .fetch_one(&mut *connection)
        .await
        .map_err(|source| DatabaseStartupError::Inspect { source })?;
        if has_untracked_schema {
            return Err(DatabaseStartupError::MigrationLedgerMissing);
        }
        return Ok(());
    }

    let applied: Vec<(i64, bool, Vec<u8>)> = sqlx::query_as(
        "SELECT version, success, checksum \
         FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(|source| DatabaseStartupError::Inspect { source })?;

    if let Some((version, _, _)) = applied.iter().find(|(_, success, _)| !success) {
        return Err(DatabaseStartupError::MigrationDirty { version: *version });
    }

    let mut expected_prefix = MIGRATOR.iter();
    for (version, _, checksum) in applied {
        let binary = latest_schema_version();
        if version > binary {
            return Err(DatabaseStartupError::SchemaTooNew {
                database: version,
                binary,
            });
        }
        let Some(migration) = MIGRATOR
            .iter()
            .find(|migration| migration.version == version)
        else {
            return Err(DatabaseStartupError::MigrationMissing { version });
        };
        let Some(expected) = expected_prefix.next() else {
            return Err(DatabaseStartupError::MigrationMissing { version });
        };
        if expected.version != version {
            return Err(DatabaseStartupError::MigrationLedgerGap {
                missing: expected.version,
                found: version,
            });
        }
        if migration.checksum.as_ref() != checksum {
            return Err(DatabaseStartupError::MigrationModified { version });
        }
    }
    Ok(())
}

fn latest_schema_version() -> i64 {
    MIGRATOR
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0)
}

async fn verify_sqlite_version(
    connection: &mut SqliteConnection,
) -> Result<(), DatabaseStartupError> {
    let encoded: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(connection)
        .await
        .map_err(|source| DatabaseStartupError::Inspect { source })?;
    let version = parse_version(&encoded)
        .filter(|version| *version >= MINIMUM_SAFE_SQLITE_VERSION)
        .ok_or_else(|| DatabaseStartupError::UnsafeSqliteVersion {
            found: encoded.into_boxed_str(),
        })?;
    debug_assert!(version >= MINIMUM_SAFE_SQLITE_VERSION);
    Ok(())
}

fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let mut components = value.split('.');
    let version = (
        components.next()?.parse().ok()?,
        components.next()?.parse().ok()?,
        components.next()?.parse().ok()?,
    );
    components.next().is_none().then_some(version)
}

async fn configure_connection(
    connection: &mut SqliteConnection,
    busy_timeout: Duration,
) -> Result<(), DatabaseStartupError> {
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode = WAL")
        .fetch_one(&mut *connection)
        .await
        .map_err(|source| DatabaseStartupError::Configure { source })?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(DatabaseStartupError::ConfigurationMismatch {
            setting: "journal_mode",
        });
    }

    for statement in [
        "PRAGMA synchronous = NORMAL",
        "PRAGMA foreign_keys = ON",
        "PRAGMA trusted_schema = OFF",
        "PRAGMA ignore_check_constraints = OFF",
        "PRAGMA query_only = OFF",
        "PRAGMA cell_size_check = ON",
    ] {
        sqlx::query(statement)
            .execute(&mut *connection)
            .await
            .map_err(|source| DatabaseStartupError::Configure { source })?;
    }

    verify_integer_pragma(connection, "PRAGMA synchronous", 1, "synchronous").await?;
    verify_integer_pragma(connection, "PRAGMA foreign_keys", 1, "foreign_keys").await?;
    verify_integer_pragma(connection, "PRAGMA trusted_schema", 0, "trusted_schema").await?;
    verify_integer_pragma(
        connection,
        "PRAGMA ignore_check_constraints",
        0,
        "ignore_check_constraints",
    )
    .await?;
    verify_integer_pragma(connection, "PRAGMA query_only", 0, "query_only").await?;
    verify_integer_pragma(connection, "PRAGMA cell_size_check", 1, "cell_size_check").await?;
    verify_text_pragma(
        connection,
        "PRAGMA locking_mode = NORMAL",
        "normal",
        "locking_mode",
    )
    .await?;

    let expected_timeout = i64::try_from(busy_timeout.as_millis()).map_err(|_| {
        DatabaseStartupError::ConfigurationMismatch {
            setting: "busy_timeout",
        }
    })?;
    verify_integer_pragma(
        connection,
        "PRAGMA busy_timeout",
        expected_timeout,
        "busy_timeout",
    )
    .await
}

async fn verify_integer_pragma(
    connection: &mut SqliteConnection,
    query: &'static str,
    expected: i64,
    setting: &'static str,
) -> Result<(), DatabaseStartupError> {
    let actual: i64 = sqlx::query_scalar(query)
        .fetch_one(connection)
        .await
        .map_err(|source| DatabaseStartupError::Configure { source })?;
    if actual != expected {
        return Err(DatabaseStartupError::ConfigurationMismatch { setting });
    }
    Ok(())
}

async fn verify_text_pragma(
    connection: &mut SqliteConnection,
    query: &'static str,
    expected: &'static str,
    setting: &'static str,
) -> Result<(), DatabaseStartupError> {
    let actual: String = sqlx::query_scalar(query)
        .fetch_one(connection)
        .await
        .map_err(|source| DatabaseStartupError::Configure { source })?;
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(DatabaseStartupError::ConfigurationMismatch { setting });
    }
    Ok(())
}

async fn verify_foreign_keys(
    connection: &mut SqliteConnection,
) -> Result<(), DatabaseStartupError> {
    let violation = sqlx::query("PRAGMA foreign_key_check")
        .fetch_optional(connection)
        .await
        .map_err(|source| DatabaseStartupError::ForeignKeyCheck { source })?;
    if violation.is_some() {
        return Err(DatabaseStartupError::ForeignKeyViolation);
    }
    Ok(())
}

fn map_migration_error(error: MigrateError) -> DatabaseStartupError {
    match error {
        MigrateError::VersionMissing(version) if version > latest_schema_version() => {
            DatabaseStartupError::SchemaTooNew {
                database: version,
                binary: latest_schema_version(),
            }
        }
        MigrateError::VersionMissing(version) => DatabaseStartupError::MigrationMissing { version },
        MigrateError::VersionMismatch(version) => {
            DatabaseStartupError::MigrationModified { version }
        }
        MigrateError::Dirty(version) => DatabaseStartupError::MigrationDirty { version },
        source => DatabaseStartupError::Migrate { source },
    }
}

#[derive(Debug, Error)]
pub(crate) enum DatabaseStartupError {
    #[error("database path must include a parent directory")]
    InvalidPath,
    #[error("database path preparation failed")]
    PathPreparation {
        #[source]
        source: io::Error,
    },
    #[error("database path is not a private owner-controlled regular file")]
    UnsafeDatabaseFile,
    #[error("database file identity changed while it was opened")]
    DatabaseFileChanged,
    #[error("another Maincopy server owns this database")]
    AlreadyOwned,
    #[error("database ownership lock is unavailable")]
    OwnershipLock {
        #[source]
        source: io::Error,
    },
    #[error("database inspection connection could not be opened")]
    OpenInspector {
        #[source]
        source: sqlx::Error,
    },
    #[error("database writer connection could not be opened")]
    OpenWriter {
        #[source]
        source: sqlx::Error,
    },
    #[error("database read pool could not be opened")]
    OpenReaders {
        #[source]
        source: sqlx::Error,
    },
    #[error("new database initialization failed")]
    Initialize {
        #[source]
        source: sqlx::Error,
    },
    #[error("database identity could not be inspected")]
    Inspect {
        #[source]
        source: sqlx::Error,
    },
    #[error("an existing nonempty SQLite database is not marked as Maincopy data")]
    UnmarkedDatabase,
    #[error("database has a foreign application ID {found}")]
    ForeignDatabase { found: i64 },
    #[error("database schema {database} is newer than supported schema {binary}")]
    SchemaTooNew { database: i64, binary: i64 },
    #[error("database migration {version} is missing from this binary")]
    MigrationMissing { version: i64 },
    #[error("database migration {version} differs from the embedded history")]
    MigrationModified { version: i64 },
    #[error("database migration {version} was not completed")]
    MigrationDirty { version: i64 },
    #[error("database schema exists without a migration ledger")]
    MigrationLedgerMissing,
    #[error("database migration ledger skips version {missing} before version {found}")]
    MigrationLedgerGap { missing: i64, found: i64 },
    #[error("bundled SQLite version {found} is below the safe WAL version")]
    UnsafeSqliteVersion { found: Box<str> },
    #[error("database connection configuration failed")]
    Configure {
        #[source]
        source: sqlx::Error,
    },
    #[error("database connection setting {setting} did not take effect")]
    ConfigurationMismatch { setting: &'static str },
    #[error("database migration failed")]
    Migrate {
        #[source]
        source: MigrateError,
    },
    #[error("database foreign-key verification failed")]
    ForeignKeyCheck {
        #[source]
        source: sqlx::Error,
    },
    #[error("database contains a foreign-key violation")]
    ForeignKeyViolation,
}

#[cfg(test)]
mod tests;
