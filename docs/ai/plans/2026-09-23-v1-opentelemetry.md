# V1 OpenTelemetry Export for pkg6depotd

**Date:** 2026-09-23
**Status:** Active
**Issues:** [#29](https://code.aopc.cloud/OpenIndiana/ips/issues/29)
**Decision:** [ADR-006](../decisions/006-otlp-http-export-with-bounded-queues.md)
**Research:** `docs/research/otlp-stack.md` on branch `research/otlp-stack`

## Goal

pkg6depotd exports traces, metrics and structured logs over OTLP/HTTP to a
configured collector. Telemetry must never slow down or stop depot reads: a
collector that is unreachable, slow or hung costs bounded memory and a counted
number of dropped records, nothing else.

## Requirements

- OTLP/HTTP (protobuf) for all three signals, endpoint and enable/disable in
  the KDL config, packaged sample and `pkg6depotd(8)`.
- Bounded per-signal buffering. On overflow the **oldest** queued record is
  evicted. Per-signal drop counters, split by reason.
- The request path only takes a short mutex to push a record. Export runs on a
  dedicated runtime with finite timeouts, a finite number of retries and
  capped exponential backoff.
- Bounded attributes: HTTP method (normalised), route template, status code,
  operation and result. Never an FMRI, digest or raw request path.
- Metrics: request count, duration and status; server errors; in-flight
  requests; the exporter's own drop counters.
- Clean shutdown: SIGTERM/SIGINT drains in-flight requests, then flushes
  telemetry within a configured deadline.
- Invalid telemetry configuration fails `config-test` and `start` without
  echoing the endpoint or other config text.
- A typed API for local import, catalog rebuild and retirement, so those
  operations get the same duration/result metrics and spans once they land.

## Steps

1. Cargo: `opentelemetry-otlp` with `http-proto` + `reqwest-client` only
   (drops the gRPC transport), `opentelemetry-appender-tracing` for logs,
   drop `rt-tokio`.
2. `config.rs`: `telemetry { otlp { ... } }` block; reject the unused legacy
   `otlp-endpoint` key. Parse errors report positions, not config text.
3. `telemetry/settings.rs`: validate into typed settings with miette errors.
4. `telemetry/queue.rs`: bounded queue with oldest eviction, stats, export
   worker with timeout/retry/backoff and deadline-bounded shutdown.
5. `telemetry/pipeline.rs`: span/log processors feeding the queues, metric
   exporter wrapper on the periodic reader, providers, subscriber layers.
6. `telemetry/http.rs` + `routes.rs`: request middleware (metrics + span).
7. `telemetry/operation.rs`: operation timer API.
8. `lib.rs`/`server.rs`: init after config, graceful shutdown, flush.
9. Tests: mock OTLP/HTTP collector for all three signals, blocked and
   unreachable collector, oldest eviction, shutdown, config fail-closed,
   binary SIGTERM flush.
10. Docs: packaged KDL sample, man page, ADR.

## Out of scope

- OTLP/gRPC, collector authentication headers and mTLS. Run a local collector
  or agent that adds credentials.
- Propagating W3C `traceparent` from clients (a public depot cannot trust it).
- Instrumenting import/rebuild/retirement call sites: those land with the
  atomic-import work and call `telemetry::operation::start`.

## Limits

Native illumos CI is not available for this slice. The added crates are
pure Rust apart from what the workspace already builds (rustls/aws-lc via
reqwest and axum-server), and no protoc is required (`opentelemetry-proto`
ships generated code).
