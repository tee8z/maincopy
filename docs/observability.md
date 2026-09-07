# Monitor Maincopy

Scrape Prometheus metrics locally and use the service journal when a target disappears.
Public and admin routers do not expose metrics. The metrics listener has no authentication; do not forward it through the public gateway.

## Scrape the local listener

The default address is `127.0.0.1:3002`. Direct host configuration accepts another loopback address:

```toml
[metrics]
bind = "127.0.0.1:3002"
```

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

## Import the dashboard

Import [the Maincopy dashboard](../deploy/grafana/maincopy.json) into Grafana.
Select the Prometheus data source and Maincopy instance.
The endpoint's `HELP` entries describe individual metrics; the dashboard covers runtime, process, database, and backup health.

Runtime and storage samples update every five seconds. Process metrics require Linux and remain zero elsewhere.
Metric labels contain fixed categories, without addresses, private paths, tokens, or user identifiers.

## Respond to health failures

| Signal | Action |
| --- | --- |
| Writer queue approaches capacity | Inspect transaction latency and write load. Reduce producers before the queue rejects work. |
| Read pool has few idle connections | Inspect request load and long reads. |
| WAL grows while checkpoints remain busy | Inspect long-lived readers, then confirm later checkpoints make progress. |
| `disk_full` or `io_failure` | Restore free space and inspect filesystem health before restarting. |
| `corruption` | Stop writes and follow [offline recovery](backup-restore.md#recover-a-complete-checkpoint). |
| `checkpoint_failed` | Inspect the service journal and storage health before restarting. |
| Metrics target unavailable | Check daemon state, journal, and configured loopback listener. A critical failure can close metrics before the final scrape. |
| `maincopy_backup_healthy` is zero | Follow [backup health diagnosis](backup-restore.md#secrets-and-health). Public reads remain available. |

```console
sudo systemctl status maincopy.service
sudo journalctl -u maincopy.service --since '-15 minutes'
```

A busy passive checkpoint permits later retries. A failed checkpoint stops the writer and starts controlled application shutdown.
Do not run external WAL truncation while Litestream replicates; see [backup retention limits](deployment.md#finite-epochs-and-expiration).
