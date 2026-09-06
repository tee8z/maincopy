# Maincopy operational metrics

Use the loopback metrics listener to inspect runtime, process, database, and backup health.
The application owns one Prometheus registry and its collector tasks.

## Scrape the local listener

The default listener is `127.0.0.1:3002`.
Set another loopback address in the host configuration:

```toml
[metrics]
bind = "127.0.0.1:3002"
```

Non-loopback addresses fail configuration validation.
Only `GET /metrics` and `HEAD /metrics` are available.
Successful responses use `text/plain; version=0.0.4` and `Cache-Control: no-store`.
The public and admin routers do not expose this endpoint.

The metrics listener accepts at most 256 TCP connections.
Each accepted connection has a 60-second lifetime, including idle time, incomplete headers, and response delivery.
Additional connections wait in the operating system backlog.
The metrics and public listeners have separate connection capacity.

Check the endpoint on the daemon host:

```console
curl --fail --noproxy '*' http://127.0.0.1:3002/metrics
```

Configure a local Prometheus scraper:

```yaml
scrape_configs:
  - job_name: maincopyd
    scrape_interval: 15s
    static_configs:
      - targets: [127.0.0.1:3002]
```

The endpoint has no admin authentication.
Host access controls protect loopback access.
Do not forward this listener through the public gateway.

## Import the dashboard

Import [the Maincopy dashboard](../deploy/grafana/maincopy.json) into Grafana.
Select the Prometheus data source and the Maincopy instance.
The dashboard uses Grafana's [classic JSON model](https://grafana.com/docs/grafana/latest/visualizations/dashboards/build-dashboards/view-dashboard-json-model/).
Its queries cover every emitted metric family.

## Read the metrics

Runtime and storage sampling occurs every five seconds.
Linux process sampling reads only the current process from `/proc/self`.
Process series remain zero on other platforms.
Writer transaction measurements update when each attempt finishes.
The sole writer runs a passive write-ahead log (WAL) checkpoint every 30 seconds.

| Metric | Type | Meaning |
| --- | --- | --- |
| `tokio_workers_count` | Gauge | Runtime worker threads |
| `tokio_worker_busy_ratio` | Gauge | Busy worker time divided by available worker time during the sample |
| `tokio_total_busy_duration_ms` | Gauge | Combined worker busy milliseconds during the sample |
| `tokio_worker_parks_total` | Counter | Combined worker park events |
| `tokio_live_tasks_count` | Gauge | Live runtime tasks |
| `tokio_global_queue_depth` | Gauge | Tasks waiting in the global runtime queue |
| `process_cpu_seconds_total` | Counter | User and system CPU seconds |
| `process_resident_memory_bytes` | Gauge | Resident memory |
| `process_virtual_memory_bytes` | Gauge | Virtual memory |
| `process_threads` | Gauge | Process threads |
| `process_open_fds` | Gauge | Open file descriptors |
| `maincopy_database_writer_up` | Gauge | One while the writer runs |
| `maincopy_database_writer_queue_depth` | Gauge | Commands waiting for the writer |
| `maincopy_database_writer_queue_capacity` | Gauge | Maximum queued writer commands |
| `maincopy_database_read_pool_connections` | Gauge | Query-only pool connections |
| `maincopy_database_read_pool_idle` | Gauge | Idle query-only pool connections |
| `maincopy_database_transaction_seconds` | Histogram | Transaction duration, including rejected attempts |
| `maincopy_database_transactions_total` | Counter | Attempts by outcome: `committed`, `rejected`, or `failed` |
| `maincopy_database_wal_bytes` | Gauge | WAL file size |
| `maincopy_database_wal_frames` | Gauge | Frames observed at the last checkpoint |
| `maincopy_database_checkpointed_frames` | Gauge | Frames copied at the last checkpoint |
| `maincopy_database_checkpoints_total` | Counter | Checkpoints by outcome: `complete`, `busy`, or `failed` |
| `maincopy_database_health` | Gauge | One active state from the fixed state set below |
| `maincopy_backup_healthy` | Gauge | One when the configured remote backup report is healthy and fresh |
| `maincopy_backup_last_success_timestamp_seconds` | Gauge | Confirmed capture time of the last complete off-site checkpoint; zero when unavailable |

Runtime series use only `service="maincopyd"` and `runtime="main"` labels.
Database outcome and state labels use fixed value sets.
The application emits no paths, URLs, user identifiers, post identifiers, secrets, or error messages as labels.
Prometheus adds its own scrape target labels, including `job` and `instance`.

## Respond to health failures

Database states are `healthy`, `corruption`, `disk_full`, `checkpoint_failed`, `io_failure`, and `stopped`.
Exactly one state is active.
SQLite corruption and disk-full errors receive typed failure classifications.
A failed checkpoint stops the writer; an incomplete passive checkpoint records `busy` and permits later retries.

When a critical task fails, the application marks readiness unavailable and begins controlled shutdown.
Accepted work drains before the writer closes its connections.
The listener and collector use the application's cancellation token.
Sampling has bounded reads and deadlines; repeated blocked samples cannot accumulate.
A terminal database failure can close the listener before another scrape records its final state.
Inspect the service journal and Prometheus target availability when a scrape disappears.

| Signal | Action |
| --- | --- |
| Writer queue approaches capacity | Inspect transaction latency and write load. Reduce producers before the queue rejects work. |
| Read pool has few idle connections | Inspect request load and long reads. |
| WAL grows while checkpoints remain busy | Inspect long-lived readers, then confirm later checkpoints make progress. |
| `disk_full` or storage errors | Restore free space and inspect filesystem health before restarting. |
| `corruption` | Stop writes and follow the offline restore procedure. |
| `checkpoint_failed` | Inspect the service journal and storage health before restarting. |
| Metrics target unavailable | Check daemon state and the configured loopback listener. |
| Backup health is zero | Inspect the backup timer, checkpoint publication result, and protected status file. Public reads remain available. |

A backup timestamp advances after all checkpoint objects and the final manifest reach remote storage.
Its value is the native synchronization confirmation time recorded before capture and upload.
Freshness includes time spent capturing, validating, encrypting, and uploading the checkpoint.
Local replica progress and individual object uploads do not advance complete checkpoint health.
The default freshness limit is 300 seconds.
The host setting `backup.stale_after_seconds` accepts 60 seconds through seven days.
Set this limit to the deployment's checkpoint interval and accepted publication delay.
See [backup and offline restore](backup-restore.md) for the complete checkpoint contract.
Missing, stale, malformed, or degraded reports produce degraded backup health.
An unconfigured backup also reports degraded health.
