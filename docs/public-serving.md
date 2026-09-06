# Public request limits and shutdown

The public listener serves the immutable publication snapshot and health endpoints.
It has independent limits for connections, request input, and handler work.

| Boundary | Limit | Result at the limit |
| --- | --- | --- |
| Accepted TCP connections | 256 | Additional sockets wait in the operating system backlog. |
| TCP connection lifetime | 60 seconds | Reads and writes stop with a timeout. |
| Request target | 4096 bytes | Larger targets receive HTTP 414. |
| Request headers | 64 fields, 16 KiB of names and values | Larger headers receive HTTP 431. |
| Request body | 8 KiB | Larger bodies receive HTTP 413. |
| Concurrent request handlers | 256 | Additional requests receive HTTP 503 and `Retry-After: 1`. |
| Body read and handler deadline | 10 seconds | Unfinished requests receive HTTP 408. |

The connection lifetime also bounds stalled headers and response delivery.
Clients must reconnect after the lifetime expires, including idle keep-alive clients.
The HTTP parser can reject malformed requests before these router limits apply.

## Access events

Each dispatched public request emits a structured `maincopy::public_access` event.
The event contains a method class, matched route template, status code, and elapsed milliseconds.
The method class is `GET`, `HEAD`, or `OTHER`.
Unmatched routes use `unmatched`.

Events omit raw paths, slugs, query strings, host values, headers, and request bodies.
Elapsed time covers request input and handler work.
It excludes final socket delivery.

## Health and orderly shutdown

`GET /health/live` reports whether the public server can process a request.
It does not depend on snapshot readiness.
`GET /health/ready` returns HTTP 503 until application startup completes.

If a required supervised task exits, the application marks itself unready and starts controlled shutdown.
Accepted readiness requests then return HTTP 503.
The public server stops accepting connections and drains accepted requests.

The application waits for listeners and producers before it cancels the database writer.
It awaits writer completion before releasing database ownership and the process lock.
Public connection and request deadlines remain active during draining.

Offline restore acceptance verifies reconstructed public output and the eligible tip projection.
Follow [backup and offline restore](backup-restore.md) before starting a recovered deployment.
