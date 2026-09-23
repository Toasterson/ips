# OpenTelemetry

## Local logs

Logs go to standard output, which SMF writes to the file that
`svcs -L pkg6depotd` names. `telemetry { log-format "json" }` selects JSON;
the default is `text`. The SMF property `config/log_filter` sets `RUST_LOG`.

## OTLP export

The depot can export traces, metrics and logs to an OpenTelemetry
collector. It uses OTLP/HTTP with protobuf; gRPC is not built in.

```kdl
telemetry {
    service-name "pkg6depotd"
    log-format "json"
    otlp {
        enabled true
        endpoint "http://127.0.0.1:4318"
    }
}
```

The endpoint is the collector's base URL. `/v1/traces`, `/v1/metrics` and
`/v1/logs` are appended. The following are rejected at load time, and the
error message never echoes the URL:

- user credentials;
- a query string;
- a fragment.

To authenticate to a remote collector, run a local collector that adds the
credentials. Export ignores the `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY`
variables and does not follow redirects.

| Key | Default | Range |
|-----|---------|-------|
| `enabled` | `true` if the block is present | |
| `traces`, `metrics`, `logs` | `true` | at least one enabled |
| `log-level` | `info` | `error` to `trace` |
| `sample-ratio` | 1.0 | 0.0 to 1.0 |
| `timeout-ms` | 5000 | 50 to 60000 |
| `max-retries` | 3 | 0 to 10 |
| `queue-size` | 2048 | 1 to 65536 |
| `batch-size` | 512 | 1 to 8192, at most `queue-size` |
| `flush-interval-ms` | 5000 | 10 to 60000 |
| `metrics-interval-ms` | 60000 | 100 to 3600000 |
| `shutdown-timeout-ms` | 5000 | 100 to 30000 |

## Export never slows the depot

Serving packages is the depot's job; telemetry must not get in its way.

- Spans and log records go into a fixed-size queue per signal. **When the
  queue is full, the oldest record is dropped**, so the records that
  describe the incident survive.
- The worst case for memory is `queue-size` queued records plus one batch
  of `batch-size` being sent, per signal.
- Export runs on its own thread with timeouts and bounded retries: capped
  exponential backoff from 100 ms to 5 s. A 4xx answer other than 408 or
  429 is dropped without retrying.
- On stop, the depot drains requests, then flushes all three signals
  within `shutdown-timeout-ms`. Anything still queued then is dropped and
  counted.

Drops are counted by signal and reason (`overflow`, `export_failed`,
`shutdown`) in `pkg6depotd.telemetry.dropped`. Watch this metric.

## What is exported

Metrics:

| Metric | Notes |
|--------|-------|
| `pkg6depotd.http.server.requests` | |
| `http.server.request.duration` | |
| `http.server.active_requests` | |
| `pkg6depotd.http.server.errors` | 5xx responses |
| `pkg6depotd.telemetry.dropped`, `pkg6depotd.telemetry.exported` | |
| `pkg6depotd.operations`, `pkg6depotd.operation.duration` | **Pending**: defined for imports, rebuilds and retirement, but nothing records them yet. Imports run in `pkg6repo`, not in the depot. |

Each request produces one server span. Span and metric attributes are
limited to:

- the HTTP method;
- the route template, for example `/{publisher}/file/1/{digest}`, or
  `unmatched`;
- the status code;
- the operation;
- the result.

Request paths, package names and digests are never attributes. Exported
log records do carry message text, which can name packages.
