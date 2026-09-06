use std::time::Duration;

use prometheus::{Histogram, HistogramOpts, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry};

use super::register;

/// Metric handles are created with the database and registered only in its application's registry.
#[derive(Clone)]
pub(crate) struct DatabaseMetrics {
    pub(crate) writer_up: IntGauge,
    pub(crate) queue_depth: IntGauge,
    pub(crate) queue_capacity: IntGauge,
    pub(crate) pool_connections: IntGauge,
    pub(crate) pool_idle: IntGauge,
    pub(crate) wal_bytes: IntGauge,
    pub(crate) wal_frames: IntGauge,
    pub(crate) checkpointed_frames: IntGauge,
    transaction_seconds: Histogram,
    transactions: IntCounterVec,
    checkpoints: IntCounterVec,
    condition: IntGaugeVec,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DatabaseCondition {
    Healthy,
    Corruption,
    DiskFull,
    CheckpointFailed,
    IoFailure,
    Stopped,
}

impl DatabaseCondition {
    const ALL: [Self; 6] = [
        Self::Healthy,
        Self::Corruption,
        Self::DiskFull,
        Self::CheckpointFailed,
        Self::IoFailure,
        Self::Stopped,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Corruption => "corruption",
            Self::DiskFull => "disk_full",
            Self::CheckpointFailed => "checkpoint_failed",
            Self::IoFailure => "io_failure",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransactionOutcome {
    Committed,
    Rejected,
    Failed,
}

impl TransactionOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CheckpointOutcome {
    Complete,
    Busy,
    Failed,
}

impl CheckpointOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Busy => "busy",
            Self::Failed => "failed",
        }
    }
}

impl DatabaseMetrics {
    pub(crate) fn new() -> Result<Self, prometheus::Error> {
        let metrics = Self {
            writer_up: IntGauge::new(
                "maincopy_database_writer_up",
                "Whether the sole database writer is running.",
            )?,
            queue_depth: IntGauge::new(
                "maincopy_database_writer_queue_depth",
                "Queued commands waiting for the sole writer.",
            )?,
            queue_capacity: IntGauge::new(
                "maincopy_database_writer_queue_capacity",
                "Maximum queued writer commands.",
            )?,
            pool_connections: IntGauge::new(
                "maincopy_database_read_pool_connections",
                "Current query-only pool connection count.",
            )?,
            pool_idle: IntGauge::new(
                "maincopy_database_read_pool_idle",
                "Idle query-only pool connections.",
            )?,
            wal_bytes: IntGauge::new(
                "maincopy_database_wal_bytes",
                "Current database WAL file size in bytes.",
            )?,
            wal_frames: IntGauge::new(
                "maincopy_database_wal_frames",
                "WAL frames observed by the last passive checkpoint.",
            )?,
            checkpointed_frames: IntGauge::new(
                "maincopy_database_checkpointed_frames",
                "Frames copied by the last passive checkpoint.",
            )?,
            transaction_seconds: Histogram::with_opts(
                HistogramOpts::new(
                    "maincopy_database_transaction_seconds",
                    "Sole-writer transaction duration, including rejected attempts.",
                )
                .buckets(vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 30.0]),
            )?,
            transactions: IntCounterVec::new(
                Opts::new(
                    "maincopy_database_transactions_total",
                    "Sole-writer transaction attempts by outcome.",
                ),
                &["outcome"],
            )?,
            checkpoints: IntCounterVec::new(
                Opts::new(
                    "maincopy_database_checkpoints_total",
                    "Passive WAL checkpoint attempts by outcome.",
                ),
                &["outcome"],
            )?,
            condition: IntGaugeVec::new(
                Opts::new(
                    "maincopy_database_health",
                    "Database condition; exactly one bounded state is active.",
                ),
                &["state"],
            )?,
        };
        metrics.set_condition(DatabaseCondition::Healthy);
        for outcome in [
            TransactionOutcome::Committed,
            TransactionOutcome::Rejected,
            TransactionOutcome::Failed,
        ] {
            metrics.transactions.with_label_values(&[outcome.as_str()]);
        }
        for outcome in [
            CheckpointOutcome::Complete,
            CheckpointOutcome::Busy,
            CheckpointOutcome::Failed,
        ] {
            metrics.checkpoints.with_label_values(&[outcome.as_str()]);
        }
        Ok(metrics)
    }

    pub(super) fn register(&self, registry: &Registry) -> Result<(), prometheus::Error> {
        for gauge in [
            &self.writer_up,
            &self.queue_depth,
            &self.queue_capacity,
            &self.pool_connections,
            &self.pool_idle,
            &self.wal_bytes,
            &self.wal_frames,
            &self.checkpointed_frames,
        ] {
            register(registry, gauge)?;
        }
        register(registry, &self.transaction_seconds)?;
        register(registry, &self.transactions)?;
        register(registry, &self.checkpoints)?;
        register(registry, &self.condition)
    }

    pub(crate) fn set_condition(&self, condition: DatabaseCondition) {
        for candidate in DatabaseCondition::ALL {
            self.condition
                .with_label_values(&[candidate.as_str()])
                .set(i64::from(candidate == condition));
        }
    }

    pub(crate) fn observe_transaction(&self, elapsed: Duration, outcome: TransactionOutcome) {
        self.transaction_seconds.observe(elapsed.as_secs_f64());
        self.transactions
            .with_label_values(&[outcome.as_str()])
            .inc();
    }

    pub(crate) fn observe_checkpoint(&self, outcome: CheckpointOutcome) {
        self.checkpoints
            .with_label_values(&[outcome.as_str()])
            .inc();
    }
}
