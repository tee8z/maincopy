use std::{io, time::Duration};

use thiserror::Error;
use tokio::{
    runtime::RuntimeMetrics as TokioRuntimeMetrics,
    time::{Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

use super::Metrics;
use crate::{
    backup_health::{BackupHealth, BackupHealthState},
    database::health::{DatabaseHealth, DatabaseHealthError},
};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

pub(crate) struct MetricsCollector {
    metrics: Metrics,
    database: DatabaseHealth,
    backup: BackupHealth,
    runtime: TokioRuntimeMetrics,
}

struct RuntimeSample {
    at: Instant,
    workers: usize,
    busy: Duration,
    parks: u64,
    live_tasks: usize,
    global_queue: usize,
}

impl RuntimeSample {
    fn read(runtime: &TokioRuntimeMetrics) -> Self {
        let workers = runtime.num_workers();
        Self {
            at: Instant::now(),
            workers,
            busy: (0..workers)
                .map(|worker| runtime.worker_total_busy_duration(worker))
                .sum(),
            parks: (0..workers)
                .map(|worker| runtime.worker_park_count(worker))
                .fold(0, u64::saturating_add),
            live_tasks: runtime.num_alive_tasks(),
            global_queue: runtime.global_queue_depth(),
        }
    }
}

impl MetricsCollector {
    pub(crate) fn new(
        metrics: Metrics,
        database: DatabaseHealth,
        backup: BackupHealth,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            metrics,
            database,
            backup,
            runtime: runtime.metrics(),
        }
    }

    pub(crate) async fn run(
        self,
        cancellation: CancellationToken,
    ) -> Result<(), MetricsCollectorError> {
        let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut previous = RuntimeSample::read(&self.runtime);
        self.metrics.runtime.parks.inc_by(previous.parks);
        let mut cpu_seconds = 0.0;
        loop {
            tokio::select! { biased; () = cancellation.cancelled() => return Ok(()), _ = interval.tick() => {} }
            let current = RuntimeSample::read(&self.runtime);
            update_runtime(&self.metrics, &previous, &current);
            previous = current;
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Ok(()),
                sample = tokio::time::timeout(Duration::from_secs(2), self.sample_platform(&mut cpu_seconds)) => {
                    sample.map_err(|_| MetricsCollectorError::Deadline)??;
                }
            }
        }
    }

    async fn sample_platform(&self, cpu_seconds: &mut f64) -> Result<(), MetricsCollectorError> {
        self.database.sample().await?;
        let backup = self.backup.snapshot().await;
        self.metrics
            .backup_healthy
            .set(i64::from(backup.state == BackupHealthState::Healthy));
        self.metrics.backup_last_success.set(
            backup
                .last_success_at
                .map_or(0, |time| time.unix_timestamp()),
        );
        let sample = tokio::task::spawn_blocking(read_process_sample)
            .await
            .map_err(MetricsCollectorError::ProcessTask)??;
        if let Some(sample) = sample {
            self.metrics
                .process
                .cpu_seconds
                .inc_by((sample.cpu_seconds - *cpu_seconds).max(0.0));
            *cpu_seconds = sample.cpu_seconds;
            self.metrics
                .process
                .resident_bytes
                .set(sample.resident_bytes);
            self.metrics.process.virtual_bytes.set(sample.virtual_bytes);
            self.metrics.process.threads.set(sample.threads);
            self.metrics.process.open_fds.set(sample.open_fds);
        }
        Ok(())
    }
}

fn update_runtime(metrics: &Metrics, previous: &RuntimeSample, current: &RuntimeSample) {
    let busy = current.busy.saturating_sub(previous.busy).as_secs_f64();
    let available = current.at.duration_since(previous.at).as_secs_f64() * current.workers as f64;
    metrics.runtime.workers.set(current.workers as i64);
    metrics.runtime.live_tasks.set(current.live_tasks as i64);
    metrics
        .runtime
        .global_queue
        .set(current.global_queue as i64);
    metrics.runtime.busy_milliseconds.set(busy * 1000.0);
    metrics.runtime.busy_ratio.set(if available > 0.0 {
        (busy / available).clamp(0.0, 1.0)
    } else {
        0.0
    });
    metrics
        .runtime
        .parks
        .inc_by(current.parks.saturating_sub(previous.parks));
}

struct ProcessSample {
    cpu_seconds: f64,
    resident_bytes: i64,
    virtual_bytes: i64,
    threads: i64,
    open_fds: i64,
}

#[cfg(target_os = "linux")]
fn read_process_sample() -> Result<Option<ProcessSample>, ProcessSampleError> {
    use std::{fs::File, io::Read as _};
    let mut stat = String::new();
    File::open("/proc/self/stat")
        .map_err(ProcessSampleError::Read)?
        .take(8193)
        .read_to_string(&mut stat)
        .map_err(ProcessSampleError::Read)?;
    if stat.len() > 8192 {
        return Err(ProcessSampleError::Bounds);
    }
    let mut sample = parse_process_stat(
        &stat,
        rustix::param::clock_ticks_per_second(),
        rustix::param::page_size() as u64,
    )?;
    let mut descriptors = 0;
    for entry in std::fs::read_dir("/proc/self/fd")
        .map_err(ProcessSampleError::Read)?
        .take(65_537)
    {
        entry.map_err(ProcessSampleError::Read)?;
        descriptors += 1;
    }
    if descriptors > 65_536 {
        return Err(ProcessSampleError::Bounds);
    }
    sample.open_fds = descriptors;
    Ok(Some(sample))
}

#[cfg(not(target_os = "linux"))]
fn read_process_sample() -> Result<Option<ProcessSample>, ProcessSampleError> {
    Ok(None)
}

fn parse_process_stat(
    stat: &str,
    ticks_per_second: u64,
    page_bytes: u64,
) -> Result<ProcessSample, ProcessSampleError> {
    let (_, fields) = stat
        .rsplit_once(')')
        .ok_or(ProcessSampleError::InvalidStat)?;
    let fields: Vec<_> = fields.split_whitespace().collect();
    if fields.len() > 64 || ticks_per_second == 0 || page_bytes == 0 {
        return Err(ProcessSampleError::InvalidStat);
    }
    let number = |index| {
        fields
            .get(index)
            .and_then(|value: &&str| value.parse::<u64>().ok())
            .ok_or(ProcessSampleError::InvalidStat)
    };
    let ticks = number(11)?
        .checked_add(number(12)?)
        .ok_or(ProcessSampleError::InvalidStat)?;
    let resident = number(21)?
        .checked_mul(page_bytes)
        .ok_or(ProcessSampleError::InvalidStat)?;
    let signed = |value| i64::try_from(value).map_err(|_| ProcessSampleError::InvalidStat);
    Ok(ProcessSample {
        cpu_seconds: ticks as f64 / ticks_per_second as f64,
        resident_bytes: signed(resident)?,
        virtual_bytes: signed(number(20)?)?,
        threads: signed(number(17)?)?,
        open_fds: 0,
    })
}

#[derive(Debug, Error)]
pub(crate) enum MetricsCollectorError {
    #[error("the process and storage sampling deadline expired")]
    Deadline,
    #[error("database health sampling failed")]
    Database(#[from] DatabaseHealthError),
    #[error("the Linux process sampler task failed")]
    ProcessTask(#[source] tokio::task::JoinError),
    #[error("Linux process usage could not be sampled")]
    Process(#[from] ProcessSampleError),
}

#[derive(Debug, Error)]
pub(crate) enum ProcessSampleError {
    #[error("the process statistics could not be read")]
    Read(#[source] io::Error),
    #[error("the process statistics exceeded the sampling bounds")]
    Bounds,
    #[error("the process statistics were malformed")]
    InvalidStat,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::DatabaseMetrics;

    #[test]
    fn runtime_samples_compute_interval_busy_time_without_unstable_or_worker_id_labels() {
        let metrics = Metrics::new(&DatabaseMetrics::new().unwrap()).unwrap();
        let now = Instant::now();
        let previous = RuntimeSample {
            at: now,
            workers: 4,
            busy: Duration::from_secs(10),
            parks: 15,
            live_tasks: 2,
            global_queue: 0,
        };
        let current = RuntimeSample {
            at: now + Duration::from_secs(5),
            workers: 4,
            busy: Duration::from_secs(14),
            parks: 22,
            live_tasks: 6,
            global_queue: 3,
        };
        update_runtime(&metrics, &previous, &current);
        assert_eq!(metrics.runtime.busy_ratio.get(), 0.2);
        assert_eq!(metrics.runtime.busy_milliseconds.get(), 4000.0);
        assert_eq!(metrics.runtime.parks.get(), 7);
        assert_eq!(metrics.runtime.live_tasks.get(), 6);
        assert_eq!(metrics.runtime.global_queue.get(), 3);
        update_runtime(&metrics, &current, &current);
        assert_eq!(metrics.runtime.busy_ratio.get(), 0.0);
        assert_eq!(metrics.runtime.parks.get(), 7);
    }

    #[test]
    fn process_stat_parsing_preserves_units_and_handles_parentheses_in_the_process_name() {
        let mut fields = vec!["0"; 50];
        fields[0] = "S";
        fields[11] = "200";
        fields[12] = "50";
        fields[17] = "3";
        fields[20] = "65536";
        fields[21] = "4";
        let stat = format!("123 (process ) with spaces) {}", fields.join(" "));
        let sample = parse_process_stat(&stat, 100, 4096).unwrap();
        assert_eq!(sample.cpu_seconds, 2.5);
        assert_eq!(sample.resident_bytes, 16384);
        assert_eq!(sample.virtual_bytes, 65536);
        assert_eq!(sample.threads, 3);
        for (stat, ticks, page_size) in [
            ("invalid", 100, 4096),
            (&stat, 0, 4096),
            (&stat, 100, 0),
            ("123 (short) S 1", 100, 4096),
        ] {
            assert!(parse_process_stat(stat, ticks, page_size).is_err());
        }
        fields[21] = "18446744073709551615";
        assert!(
            parse_process_stat(&format!("123 (overflow) {}", fields.join(" ")), 100, 4096).is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_process_sampler_reads_only_current_process_usage() {
        let sample = read_process_sample().unwrap().unwrap();
        assert!(sample.cpu_seconds >= 0.0);
        assert!(sample.virtual_bytes > 0);
        assert!(sample.threads > 0);
        assert!(sample.open_fds > 0 && sample.open_fds <= 65_536);
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn collector_samples_live_process_and_stops_on_cancellation() {
        use crate::{
            config::{
                DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
                DatabaseWriterQueueCapacity,
            },
            database,
        };
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/maincopy.db");
        let bootstrapped = database::bootstrap(DatabaseConfigurationView {
            path: &path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(4).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        })
        .await
        .unwrap();
        let (store, writer) = bootstrapped.into_store(4);
        let metrics = Metrics::new(&store.health.metrics).unwrap();
        let collector = MetricsCollector::new(
            metrics.clone(),
            store.health.clone(),
            BackupHealth::new(None),
            tokio::runtime::Handle::current(),
        );
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(collector.run(cancellation.clone()));
        tokio::time::timeout(Duration::from_secs(5), async {
            while metrics.process.virtual_bytes.get() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(metrics.runtime.workers.get(), 1);
        assert!(metrics.process.threads.get() > 0);
        assert!(metrics.process.open_fds.get() > 0);
        assert_eq!(metrics.backup_healthy.get(), 0);
        assert_eq!(metrics.backup_last_success.get(), 0);
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        writer.run(cancellation).await.unwrap();
    }
}
