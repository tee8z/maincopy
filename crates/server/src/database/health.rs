//! Read-only database pressure sampling; WAL checkpoint writes remain with the sole writer.
use std::{io, path::PathBuf, time::Duration};

use sqlx::SqlitePool;
use thiserror::Error;
use tokio::sync::mpsc;

use super::store::Mutation;
use crate::metrics::DatabaseMetrics;

#[derive(Clone)]
pub(crate) struct DatabaseHealth {
    pub(crate) metrics: DatabaseMetrics,
    readers: SqlitePool,
    mutations: mpsc::Sender<Mutation>,
    wal_path: PathBuf,
}

impl DatabaseHealth {
    pub(super) fn new(
        metrics: DatabaseMetrics,
        readers: SqlitePool,
        mutations: mpsc::Sender<Mutation>,
        wal_path: PathBuf,
    ) -> Self {
        Self {
            metrics,
            readers,
            mutations,
            wal_path,
        }
    }

    pub(crate) async fn sample(&self) -> Result<(), DatabaseHealthError> {
        self.metrics
            .queue_capacity
            .set(self.mutations.max_capacity() as i64);
        self.metrics
            .queue_depth
            .set((self.mutations.max_capacity() - self.mutations.capacity()) as i64);
        self.metrics
            .pool_connections
            .set(i64::from(self.readers.size()));
        self.metrics.pool_idle.set(self.readers.num_idle() as i64);
        let wal_bytes = match tokio::time::timeout(
            Duration::from_millis(500),
            tokio::fs::metadata(&self.wal_path),
        )
        .await
        .map_err(|_| DatabaseHealthError::Deadline)?
        {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(source) => return Err(DatabaseHealthError::WalMetadata { source }),
        };
        self.metrics
            .wal_bytes
            .set(i64::try_from(wal_bytes).unwrap_or(i64::MAX));
        Ok(())
    }
}

#[derive(Debug, Error)]
pub(crate) enum DatabaseHealthError {
    #[error("the database WAL metadata sampling deadline expired")]
    Deadline,
    #[error("the database WAL metadata could not be sampled")]
    WalMetadata {
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sampling_reports_pool_usage_and_wal_size_without_writing_the_database() {
        let root = tempfile::tempdir().unwrap();
        let wal = root.path().join("maincopy.db-wal");
        std::fs::write(&wal, [0; 512]).unwrap();
        let readers = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let (mutations, _receiver) = mpsc::channel(8);
        let metrics = DatabaseMetrics::new().unwrap();
        let health = DatabaseHealth::new(metrics.clone(), readers.clone(), mutations, wal.clone());
        let held = readers.acquire().await.unwrap();
        health.sample().await.unwrap();
        assert_eq!(metrics.pool_connections.get(), 1);
        assert_eq!(metrics.pool_idle.get(), 0);
        assert_eq!(metrics.queue_depth.get(), 0);
        assert_eq!(metrics.queue_capacity.get(), 8);
        assert_eq!(metrics.wal_bytes.get(), 512);
        drop(held);
        std::fs::remove_file(&wal).unwrap();
        health.sample().await.unwrap();
        assert_eq!(metrics.wal_bytes.get(), 0);
        readers.close().await;
    }

    #[tokio::test]
    async fn inaccessible_wal_metadata_returns_a_typed_failure() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("regular-file");
        std::fs::write(&parent, []).unwrap();
        let readers = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let (mutations, _receiver) = mpsc::channel(1);
        let health = DatabaseHealth::new(
            DatabaseMetrics::new().unwrap(),
            readers.clone(),
            mutations,
            parent.join("wal"),
        );
        assert!(matches!(
            health.sample().await,
            Err(DatabaseHealthError::WalMetadata { .. })
        ));
        readers.close().await;
    }
}
