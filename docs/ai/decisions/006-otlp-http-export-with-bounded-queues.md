# ADR-006: OTLP/HTTP Export Through Bounded Drop-Oldest Queues

**Date:** 2026-09-23
**Status:** Proposed
**Issues:** [#29](https://code.aopc.cloud/OpenIndiana/ips/issues/29)

> Numbered 006 because ADRs 004 and 005 are claimed by parallel v1 work.
> Renumber on merge if that changes.

## Context

pkg6depotd needs traces, metrics and logs in an OpenTelemetry collector. The
depot serves package downloads; telemetry must not add latency or failure
modes to that path, whatever state the collector is in.

The workspace already locks the OpenTelemetry 0.31 family and
tracing-opentelemetry 0.32 (reqwest 0.12). Verified against the 0.31 sources:

- `BatchSpanProcessor`/`BatchLogProcessor` drop the **newest** record when
  their channel is full. We need the newest records kept, because they describe
  the incident that made the collector fall behind.
- The OTLP HTTP exporter does not retry in 0.31 and its errors embed the
  collector URL.
- `PeriodicReader` runs exports with `futures_executor::block_on` on its own
  thread, so an async reqwest client needs a tokio runtime supplied by us.
- `SdkMeterProvider::shutdown_with_timeout` ignores its timeout, and
  `PeriodicReader` waits a hard-coded 5 s for its final export before
  returning `Timeout` while that export keeps running.
- The reqwest client turns non-2xx answers into errors that the exporter only
  reports as text (`reqwest::Error { kind: Status(400, ..) }`).
- `opentelemetry-proto` ships generated code, so no protoc is needed. In 0.31
  its `gen-tonic-messages` feature still compiles the `tonic` core crate (for
  codec types, without the gRPC channel/transport); 0.33 drops that.

## Decision

- **Transport:** OTLP/HTTP protobuf via `opentelemetry-otlp` 0.31 with
  `http-proto` and `reqwest-client`. The gRPC transport (`grpc-tonic`,
  `hyper-timeout`, tonic channel) is dropped from the build.
- **Traces and logs:** our own `SpanProcessor`/`LogProcessor` push into a
  per-signal `Mutex<VecDeque>` of fixed capacity. A full queue evicts the
  oldest record and counts it. The push is the only work on the request path.
- **Export:** a dedicated one-worker tokio runtime (`pkg6depotd-otel`) runs
  one worker per signal. Each export attempt has a timeout; failures retry a
  bounded number of times with capped exponential backoff; a batch that still
  fails is dropped and counted. A 4xx other than 408/429 (parsed from the
  error text; unrecognised errors are retried) is dropped without retrying,
  as the OTLP/HTTP spec asks. The HTTP client ignores proxy environment
  variables and does not follow redirects, so export only reaches the
  configured endpoint.
- **Memory bound:** per signal, `queue-size` queued records plus the one
  in-flight batch of up to `batch-size` records the worker has taken out of
  the queue.
- **Metrics:** the SDK `PeriodicReader` drives a wrapper exporter that runs
  the OTLP export on the telemetry runtime with the same timeout/retry policy.
  Cumulative temporality means a newer snapshot supersedes a lost one, so the
  metric "queue" is the aggregation state itself, bounded by attribute
  cardinality. A failed snapshot is counted as a metrics drop. Shutdown waits
  for the reader's final export by a signal from the wrapper's
  `shutdown_with_timeout` (called by the reader right after that export),
  bounded by our deadline, not by the SDK's fixed 5 s.
- **Attributes:** HTTP spans and metrics carry the normalised method, the
  axum route template (`unmatched` otherwise), status code, operation and
  result. Only spans with the `pkg6depotd::otel` target are exported, so
  free-form handler spans and their fields never reach the collector.
- **Logs:** tracing events at the configured level, excluding the exporter's
  own transport crates and diagnostics to avoid feedback loops. Events emitted
  on telemetry threads are dropped at the processor.
- **Shutdown:** SIGTERM/SIGINT stop accepting, drain in-flight requests, then
  flush all three signals in parallel against one deadline. Records still
  queued at the deadline are counted as dropped. If an export is still
  running at the deadline, the telemetry runtime is leaked rather than shut
  down under it; the process is exiting.
- **Config:** `telemetry { otlp { enabled; endpoint; ... } }`, validated at
  load. The endpoint must be a plain `http(s)://host[:port][/prefix]` URL; user
  info, query and fragment are rejected, and no error message echoes it.
  The earlier `telemetry { otlp-endpoint "..." }` key never enabled export;
  it is now rejected with a pointer to `otlp { endpoint }` rather than
  silently ignored.
- **Operations:** `telemetry::operation` defines the metrics and spans for
  local import, catalog rebuild and retirement. No production call site
  records them yet; they land with the atomic-import work.

## Consequences

- One small custom component (queue + worker) to maintain, instead of
  depending on SDK processor semantics we cannot configure.
- Exported logs contain message text, which can include package names; they
  are data, not attributes, and stay subject to the configured level.
- A later move to OTel 0.32+/reqwest 0.13 keeps this design; only exporter
  construction changes.
