//! One explicitly owned Prometheus registry and bounded application metrics.
mod collector;
mod database;
mod server;

pub(crate) use collector::MetricsCollector;
pub(crate) use database::{
    CheckpointOutcome, DatabaseCondition, DatabaseMetrics, TransactionOutcome,
};
pub(crate) use server::MetricsServer;

use std::collections::HashMap;

use prometheus::{
    Encoder as _, Gauge, IntCounter, IntGauge, Opts, Registry, TextEncoder, core::Collector,
};
use thiserror::Error;

/// Clones share only this application's registry; no global registry is used.
#[derive(Clone)]
pub(crate) struct Metrics {
    registry: Registry,
    runtime: RuntimeMetrics,
    process: ProcessMetrics,
    backup_healthy: IntGauge,
    backup_last_success: IntGauge,
}

#[derive(Clone)]
struct RuntimeMetrics {
    workers: IntGauge,
    busy_ratio: Gauge,
    busy_milliseconds: Gauge,
    parks: IntCounter,
    live_tasks: IntGauge,
    global_queue: IntGauge,
}

#[derive(Clone)]
struct ProcessMetrics {
    cpu_seconds: prometheus::Counter,
    resident_bytes: IntGauge,
    virtual_bytes: IntGauge,
    threads: IntGauge,
    open_fds: IntGauge,
}

impl Metrics {
    pub(crate) fn new(database: &DatabaseMetrics) -> Result<Self, MetricsError> {
        let registry = Registry::new();
        let runtime = RuntimeMetrics::new(&registry)?;
        let process = ProcessMetrics::new(&registry)?;
        database.register(&registry)?;
        let backup_healthy = IntGauge::new(
            "maincopy_backup_healthy",
            "Whether the last replica report is healthy and fresh.",
        )?;
        let backup_last_success = IntGauge::new(
            "maincopy_backup_last_success_timestamp_seconds",
            "Last accepted replica success timestamp; zero when unavailable.",
        )?;
        register(&registry, &backup_healthy)?;
        register(&registry, &backup_last_success)?;
        Ok(Self {
            registry,
            runtime,
            process,
            backup_healthy,
            backup_last_success,
        })
    }

    fn encode(&self) -> Result<Vec<u8>, prometheus::Error> {
        let mut bytes = Vec::with_capacity(8192);
        TextEncoder::new().encode(&self.registry.gather(), &mut bytes)?;
        Ok(bytes)
    }
}

impl RuntimeMetrics {
    fn new(registry: &Registry) -> Result<Self, prometheus::Error> {
        let labels = HashMap::from([
            ("service".into(), "maincopyd".into()),
            ("runtime".into(), "main".into()),
        ]);
        let opts = |name, help| Opts::new(name, help).const_labels(labels.clone());
        let metrics = Self {
            workers: IntGauge::with_opts(opts(
                "tokio_workers_count",
                "Runtime worker thread count.",
            ))?,
            busy_ratio: Gauge::with_opts(opts(
                "tokio_worker_busy_ratio",
                "Worker busy time divided by available worker time in the last sample.",
            ))?,
            busy_milliseconds: Gauge::with_opts(opts(
                "tokio_total_busy_duration_ms",
                "Total worker busy milliseconds during the last sample.",
            ))?,
            parks: IntCounter::with_opts(opts(
                "tokio_worker_parks_total",
                "Cumulative worker park count.",
            ))?,
            live_tasks: IntGauge::with_opts(opts(
                "tokio_live_tasks_count",
                "Runtime tasks alive at the latest sample.",
            ))?,
            global_queue: IntGauge::with_opts(opts(
                "tokio_global_queue_depth",
                "Tasks waiting in the runtime global queue.",
            ))?,
        };
        for gauge in [&metrics.workers, &metrics.live_tasks, &metrics.global_queue] {
            register(registry, gauge)?;
        }
        register(registry, &metrics.busy_ratio)?;
        register(registry, &metrics.busy_milliseconds)?;
        register(registry, &metrics.parks)?;
        Ok(metrics)
    }
}

impl ProcessMetrics {
    fn new(registry: &Registry) -> Result<Self, prometheus::Error> {
        let metrics = Self {
            cpu_seconds: prometheus::Counter::new(
                "process_cpu_seconds_total",
                "Total process user and system CPU seconds.",
            )?,
            resident_bytes: IntGauge::new(
                "process_resident_memory_bytes",
                "Process resident memory bytes.",
            )?,
            virtual_bytes: IntGauge::new(
                "process_virtual_memory_bytes",
                "Process virtual memory bytes.",
            )?,
            threads: IntGauge::new("process_threads", "Current process thread count.")?,
            open_fds: IntGauge::new("process_open_fds", "Current process open file descriptors.")?,
        };
        register(registry, &metrics.cpu_seconds)?;
        for gauge in [
            &metrics.resident_bytes,
            &metrics.virtual_bytes,
            &metrics.threads,
            &metrics.open_fds,
        ] {
            register(registry, gauge)?;
        }
        Ok(metrics)
    }
}

fn register<Metric: Collector + Clone + 'static>(
    registry: &Registry,
    metric: &Metric,
) -> Result<(), prometheus::Error> {
    registry.register(Box::new(metric.clone()))
}

#[derive(Debug, Error)]
pub(crate) enum MetricsError {
    #[error("the application metrics registry could not be constructed")]
    Registry(#[from] prometheus::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn application_registries_are_isolated_and_expose_only_bounded_metric_labels() {
        let database = DatabaseMetrics::new().unwrap();
        let first = Metrics::new(&database).unwrap();
        let second = Metrics::new(&DatabaseMetrics::new().unwrap()).unwrap();
        database.observe_transaction(Duration::from_millis(25), TransactionOutcome::Committed);
        database.observe_checkpoint(CheckpointOutcome::Busy);
        database.set_condition(DatabaseCondition::DiskFull);
        assert_eq!(first.registry.gather().len(), 25);
        let first_text = String::from_utf8(first.encode().unwrap()).unwrap();
        let second_text = String::from_utf8(second.encode().unwrap()).unwrap();
        assert!(
            first_text.contains("maincopy_database_transactions_total{outcome=\"committed\"} 1")
        );
        assert!(
            second_text.contains("maincopy_database_transactions_total{outcome=\"committed\"} 0")
        );
        assert!(first_text.contains("# TYPE maincopy_database_transaction_seconds histogram"));
        assert!(first_text.contains("# TYPE tokio_worker_parks_total counter"));
        assert!(first_text.contains("maincopy_database_health{state=\"disk_full\"} 1"));
        for family in first.registry.gather() {
            let expected_type = match family.name() {
                "tokio_worker_parks_total"
                | "process_cpu_seconds_total"
                | "maincopy_database_transactions_total"
                | "maincopy_database_checkpoints_total" => prometheus::proto::MetricType::COUNTER,
                "maincopy_database_transaction_seconds" => prometheus::proto::MetricType::HISTOGRAM,
                _ => prometheus::proto::MetricType::GAUGE,
            };
            assert_eq!(family.get_field_type(), expected_type, "{}", family.name());
            for metric in family.get_metric() {
                for label in metric.get_label() {
                    let accepted = match label.name() {
                        "service" => label.value() == "maincopyd",
                        "runtime" => label.value() == "main",
                        "outcome" => ["committed", "rejected", "failed", "complete", "busy"]
                            .contains(&label.value()),
                        "state" => [
                            "healthy",
                            "corruption",
                            "disk_full",
                            "checkpoint_failed",
                            "io_failure",
                            "stopped",
                        ]
                        .contains(&label.value()),
                        _ => false,
                    };
                    assert!(accepted, "unbounded label in {}", family.name());
                }
            }
        }
        assert!(first_text.len() < 32 * 1024);
    }
    #[test]
    fn dashboard_queries_use_registered_metrics_and_cover_every_family() {
        let dashboard: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../deploy/grafana/maincopy.json"
        )))
        .unwrap();
        let metrics = Metrics::new(&DatabaseMetrics::new().unwrap()).unwrap();
        let families = metrics.registry.gather();
        let mut used = std::collections::BTreeSet::new();
        for panel in dashboard["panels"].as_array().unwrap() {
            for target in panel["targets"].as_array().unwrap() {
                let expression = target["expr"].as_str().unwrap();
                assert!(expression.contains("instance=\"$instance\""));
                for token in expression
                    .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                {
                    if ["maincopy_", "tokio_", "process_"]
                        .iter()
                        .any(|prefix| token.starts_with(prefix))
                    {
                        let family = families
                            .iter()
                            .find(|family| {
                                token == family.name()
                                    || (family.get_field_type()
                                        == prometheus::proto::MetricType::HISTOGRAM
                                        && token.strip_suffix("_bucket") == Some(family.name()))
                            })
                            .unwrap_or_else(|| {
                                panic!("dashboard references unregistered series {token}")
                            });
                        used.insert(family.name().to_owned());
                    }
                }
            }
        }
        assert_eq!(used.len(), families.len());
        assert_eq!(dashboard["templating"]["list"][0]["type"], "datasource");
    }
}
