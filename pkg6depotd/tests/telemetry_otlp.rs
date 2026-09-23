//! OTLP/HTTP export against a mock collector.
//!
//! Each test installs the global subscriber, so they rely on nextest's
//! process-per-test isolation (`cargo nextest run`), like the rest of the
//! workspace.

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use libips::actions::{Attr, File as FileAction, Manifest};
use libips::repository::{FileBackend, RepositoryVersion, WritableRepository};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::metrics::v1::metric;
use pkg6depotd::config::{Config, OtlpConfig, RepositoryConfig, ServerConfig, TelemetryConfig};
use pkg6depotd::http;
use pkg6depotd::repo::DepotRepo;
use pkg6depotd::telemetry::operation::{self, Operation, OperationResult};
use pkg6depotd::telemetry::{self, Telemetry, TelemetrySettings, TelemetryStats};
use prost::Message;
use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Mock collector

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Answers 200 at once.
    Healthy,
    /// Accepts and reads requests, never answers.
    Hang,
    /// Holds the first request until `release`, then answers everything.
    GateFirst,
    /// Answers 200 after a delay.
    Slow(Duration),
    /// Answers every request with this status.
    Reject(StatusCode),
}

#[derive(Clone, Debug)]
struct Received {
    path: String,
    content_type: String,
    body: Vec<u8>,
}

struct CollectorState {
    mode: Mode,
    started: AtomicUsize,
    released: AtomicBool,
    received: Mutex<Vec<Received>>,
}

struct Collector {
    addr: SocketAddr,
    state: Arc<CollectorState>,
}

impl Collector {
    fn start(mode: Mode) -> Self {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let state = Arc::new(CollectorState {
            mode,
            started: AtomicUsize::new(0),
            released: AtomicBool::new(false),
            received: Mutex::new(Vec::new()),
        });
        let app = Router::new().fallback(collect).with_state(state.clone());
        // Its own runtime: nothing the depot or telemetry does can stall it.
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
                    axum::serve(listener, app).await.unwrap();
                });
        });
        Self { addr, state }
    }

    fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn release(&self) {
        self.state.released.store(true, Ordering::SeqCst);
    }

    fn started(&self) -> usize {
        self.state.started.load(Ordering::SeqCst)
    }

    fn received(&self, path: &str) -> Vec<Received> {
        self.state
            .received
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == path)
            .cloned()
            .collect()
    }

    fn all_bodies(&self) -> Vec<Received> {
        self.state.received.lock().unwrap().clone()
    }
}

async fn collect(
    State(state): State<Arc<CollectorState>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let n = state.started.fetch_add(1, Ordering::SeqCst);
    match state.mode {
        Mode::Healthy => {}
        Mode::Hang => std::future::pending::<()>().await,
        Mode::GateFirst => {
            if n == 0 {
                while !state.released.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
        Mode::Slow(delay) => tokio::time::sleep(delay).await,
        Mode::Reject(status) => return status,
    }
    state.received.lock().unwrap().push(Received {
        path: uri.path().to_string(),
        content_type: headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string(),
        body: body.to_vec(),
    });
    StatusCode::OK
}

/// Port that nothing listens on.
fn closed_port_endpoint() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

// ---------------------------------------------------------------------------
// Decoding

fn string_value(v: &Option<AnyValue>) -> Option<String> {
    match v.as_ref()?.value.as_ref()? {
        any_value::Value::StringValue(s) => Some(s.clone()),
        any_value::Value::IntValue(i) => Some(i.to_string()),
        any_value::Value::BoolValue(b) => Some(b.to_string()),
        _ => None,
    }
}

fn attrs(kvs: &[KeyValue]) -> HashMap<String, String> {
    kvs.iter()
        .filter_map(|kv| Some((kv.key.clone(), string_value(&kv.value)?)))
        .collect()
}

#[derive(Debug)]
struct SpanInfo {
    name: String,
    attrs: HashMap<String, String>,
    trace_id: Vec<u8>,
    service: Option<String>,
}

fn spans(c: &Collector) -> Vec<SpanInfo> {
    let mut out = Vec::new();
    for r in c.received("/v1/traces") {
        let req = ExportTraceServiceRequest::decode(r.body.as_slice()).unwrap();
        for rs in req.resource_spans {
            let service = rs
                .resource
                .as_ref()
                .and_then(|res| attrs(&res.attributes).get("service.name").cloned());
            for ss in rs.scope_spans {
                for s in ss.spans {
                    out.push(SpanInfo {
                        name: s.name,
                        attrs: attrs(&s.attributes),
                        trace_id: s.trace_id,
                        service: service.clone(),
                    });
                }
            }
        }
    }
    out
}

#[derive(Debug)]
struct LogInfo {
    body: String,
    trace_id: Vec<u8>,
}

fn logs(c: &Collector) -> Vec<LogInfo> {
    let mut out = Vec::new();
    for r in c.received("/v1/logs") {
        let req = ExportLogsServiceRequest::decode(r.body.as_slice()).unwrap();
        for rl in req.resource_logs {
            for sl in rl.scope_logs {
                for l in sl.log_records {
                    out.push(LogInfo {
                        body: string_value(&l.body).unwrap_or_default(),
                        trace_id: l.trace_id,
                    });
                }
            }
        }
    }
    out
}

/// Metric name -> attribute sets of its data points, from the latest export.
fn metrics(c: &Collector) -> HashMap<String, Vec<HashMap<String, String>>> {
    let mut out: HashMap<String, Vec<HashMap<String, String>>> = HashMap::new();
    let Some(last) = c.received("/v1/metrics").pop() else {
        return out;
    };
    let req = ExportMetricsServiceRequest::decode(last.body.as_slice()).unwrap();
    for rm in req.resource_metrics {
        for sm in rm.scope_metrics {
            for m in sm.metrics {
                let points: Vec<HashMap<String, String>> = match m.data {
                    Some(metric::Data::Sum(s)) => {
                        s.data_points.iter().map(|p| attrs(&p.attributes)).collect()
                    }
                    Some(metric::Data::Histogram(h)) => {
                        h.data_points.iter().map(|p| attrs(&p.attributes)).collect()
                    }
                    Some(metric::Data::Gauge(g)) => {
                        g.data_points.iter().map(|p| attrs(&p.attributes)).collect()
                    }
                    _ => Vec::new(),
                };
                out.entry(m.name).or_default().extend(points);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Depot under test

fn setup_repo(dir: &Path) -> PathBuf {
    let repo_path = dir.join("repo");
    let mut backend = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();
    backend.add_publisher("test").unwrap();
    let mut tx = backend.begin_transaction().unwrap();
    tx.set_publisher("test");
    let content = dir.join("content");
    std::fs::create_dir_all(&content).unwrap();
    let file_path = content.join("hello.txt");
    std::fs::write(&file_path, "Hello IPS").unwrap();
    let mut fa = FileAction::read_from_path(&file_path).unwrap();
    fa.path = "hello.txt".to_string();
    tx.add_file(fa, &file_path).unwrap();
    let mut manifest = Manifest::new();
    manifest.attributes.push(Attr {
        key: "pkg.fmri".to_string(),
        values: vec!["pkg://test/s3cretpkg@1.0.0".to_string()],
        properties: HashMap::new(),
    });
    tx.update_manifest(manifest);
    tx.commit().unwrap();
    backend.rebuild(Some("test"), false, false).unwrap();
    repo_path
}

fn depot_config(repo: &Path) -> Config {
    Config {
        server: ServerConfig {
            bind: vec!["127.0.0.1:0".to_string()],
            workers: None,
            max_connections: None,
            reuseport: None,
            cache_max_age: Some(3600),
            tls_cert: None,
            tls_key: None,
        },
        repository: RepositoryConfig {
            root: repo.to_path_buf(),
            mode: Some("readonly".to_string()),
        },
        telemetry: None,
        publishers: None,
        admin: None,
        oauth2: None,
    }
}

async fn start_depot(dir: &Path) -> String {
    let repo = setup_repo(dir);
    let state = Arc::new(DepotRepo::new(&depot_config(&repo)).unwrap());
    let router = http::routes::app_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(http::server::run(router, listener));
    format!("http://{addr}")
}

fn settings(endpoint: &str, tune: impl FnOnce(&mut OtlpConfig)) -> TelemetrySettings {
    let mut otlp = OtlpConfig {
        endpoint: Some(endpoint.to_string()),
        flush_interval_ms: Some(50),
        metrics_interval_ms: Some(200),
        timeout_ms: Some(2_000),
        shutdown_timeout_ms: Some(3_000),
        ..Default::default()
    };
    tune(&mut otlp);
    TelemetrySettings::from_config(Some(&TelemetryConfig {
        service_name: Some("depot-under-test".into()),
        otlp: Some(otlp),
        ..Default::default()
    }))
    .unwrap()
}

async fn shutdown(t: Telemetry) -> (TelemetryStats, Duration) {
    let started = Instant::now();
    let stats = tokio::task::spawn_blocking(move || t.shutdown())
        .await
        .unwrap();
    (stats, started.elapsed())
}

async fn wait_until(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Raw-path fragments that must never appear in exported spans or metrics.
const FORBIDDEN: &[&str] = &["s3cretpkg", "deadbeefcafe0123", "route-marker-xyz"];

// ---------------------------------------------------------------------------
// Tests

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exports_traces_metrics_and_logs_with_bounded_attributes() {
    let collector = Collector::start(Mode::Healthy);
    let t = telemetry::init(&settings(&collector.endpoint(), |_| {})).unwrap();
    assert!(t.exporting());

    let dir = TempDir::new().unwrap();
    let base = start_depot(dir.path()).await;
    let client = reqwest::Client::new();
    let get = |path: &str| client.get(format!("{base}{path}")).send();
    assert_eq!(get("/versions/0/").await.unwrap().status(), 200);
    get("/test/manifest/0/pkg%3A%2F%2Ftest%2Fs3cretpkg%401.0.0")
        .await
        .unwrap();
    get("/test/file/1/deadbeefcafe0123").await.unwrap();
    assert_eq!(
        get("/no/such/route-marker-xyz").await.unwrap().status(),
        404
    );
    tracing::info!(target: "depot_test", "hello from the test");
    let timer = operation::start(Operation::CatalogRebuild);
    timer.in_span(|| tracing::info!(target: "depot_test", "rebuilding"));
    timer.finish(OperationResult::Ok);

    // Let at least one periodic metrics export happen before shutdown.
    wait_until("metrics export", Duration::from_secs(5), || {
        !collector.received("/v1/metrics").is_empty()
    })
    .await;
    let (stats, _) = shutdown(t).await;

    for path in ["/v1/traces", "/v1/metrics", "/v1/logs"] {
        let got = collector.received(path);
        assert!(!got.is_empty(), "nothing on {path}");
        for r in &got {
            assert_eq!(r.content_type, "application/x-protobuf", "{path}");
        }
    }
    for signal in telemetry::Signal::ALL {
        let s = stats.signal(signal);
        assert!(s.exported > 0, "{signal:?}: {s:?}");
        assert_eq!(s.dropped(), 0, "{signal:?}: {s:?}");
    }

    // Traces: one server span per request, named by method and route template.
    let spans = spans(&collector);
    let names: BTreeSet<&str> = spans.iter().map(|s| s.name.as_str()).collect();
    for expected in [
        "GET /versions/0/",
        "GET /{publisher}/manifest/0/{fmri}",
        "GET /{publisher}/file/1/{digest}",
        "GET unmatched",
        "catalog_rebuild",
    ] {
        assert!(
            names.contains(expected),
            "{expected} missing from {names:?}"
        );
    }
    let versions = spans.iter().find(|s| s.name == "GET /versions/0/").unwrap();
    assert_eq!(versions.attrs["http.route"], "/versions/0/");
    assert_eq!(versions.attrs["http.request.method"], "GET");
    assert_eq!(versions.attrs["http.response.status_code"], "200");
    assert_eq!(versions.attrs["pkg6depotd.operation"], "versions");
    assert_eq!(versions.attrs["pkg6depotd.result"], "ok");
    assert_eq!(versions.service.as_deref(), Some("depot-under-test"));
    let unmatched = spans.iter().find(|s| s.name == "GET unmatched").unwrap();
    assert_eq!(unmatched.attrs["pkg6depotd.result"], "client_error");

    // Metrics: request count/duration/status by route template, operations,
    // and the exporter's own counters.
    let metrics = metrics(&collector);
    let requests = &metrics["pkg6depotd.http.server.requests"];
    assert!(requests.iter().any(|a| a["http.route"] == "/versions/0/"
        && a["http.response.status_code"] == "200"
        && a["pkg6depotd.result"] == "ok"));
    assert!(
        requests
            .iter()
            .any(|a| a["http.route"] == "unmatched" && a["http.response.status_code"] == "404")
    );
    assert!(metrics.contains_key("http.server.request.duration"));
    assert!(metrics.contains_key("http.server.active_requests"));
    assert!(
        metrics["pkg6depotd.operations"]
            .iter()
            .any(|a| a["pkg6depotd.operation"] == "catalog_rebuild"
                && a["pkg6depotd.result"] == "ok")
    );
    let dropped = &metrics["pkg6depotd.telemetry.dropped"];
    assert!(
        dropped
            .iter()
            .any(|a| a["signal"] == "logs" && a["reason"] == "overflow")
    );

    // No attribute anywhere in traces or metrics carries a raw path, FMRI or
    // digest. Protobuf stores strings verbatim, so a byte search is exhaustive.
    for r in collector.all_bodies() {
        if r.path == "/v1/logs" {
            continue;
        }
        for needle in FORBIDDEN {
            assert!(
                !r.body.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "{needle} leaked into {}",
                r.path
            );
        }
    }

    // Logs: structured records, correlated with the operation's trace.
    let logs = logs(&collector);
    assert!(logs.iter().any(|l| l.body == "hello from the test"));
    let op_trace = &spans
        .iter()
        .find(|s| s.name == "catalog_rebuild")
        .unwrap()
        .trace_id;
    let rebuilding = logs.iter().find(|l| l.body == "rebuilding").unwrap();
    assert_eq!(&rebuilding.trace_id, op_trace);
    // The exporter's own diagnostics and HTTP client never become records.
    assert!(!logs.iter().any(|l| l.body.contains("OTLP")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hung_collector_never_slows_depot_reads_and_shutdown_is_bounded() {
    let collector = Collector::start(Mode::Hang);
    let t = telemetry::init(&settings(&collector.endpoint(), |o| {
        // A request that waited on export would take at least this long.
        o.timeout_ms = Some(2_000);
        o.max_retries = Some(2);
        o.queue_size = Some(32);
        o.batch_size = Some(8);
        o.flush_interval_ms = Some(20);
        o.metrics_interval_ms = Some(100);
        o.shutdown_timeout_ms = Some(500);
    }))
    .unwrap();

    let dir = TempDir::new().unwrap();
    let base = start_depot(dir.path()).await;
    let client = reqwest::Client::new();
    let started = Instant::now();
    let mut slowest = Duration::ZERO;
    for i in 0..300 {
        let t0 = Instant::now();
        let resp = client
            .get(format!("{base}/versions/0/"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        resp.bytes().await.unwrap();
        slowest = slowest.max(t0.elapsed());
        tracing::info!(target: "depot_test", "request {i}");
    }
    let total = started.elapsed();
    // Export attempts hang until their 2 s timeout and retry; none of that
    // may show up on the request path.
    assert!(
        slowest < Duration::from_secs(1),
        "slowest request {slowest:?}"
    );
    assert!(
        total < Duration::from_secs(20),
        "300 requests took {total:?}"
    );
    assert!(collector.started() > 0, "collector was never contacted");

    let (stats, took) = shutdown(t).await;
    assert!(
        took < Duration::from_millis(500) + Duration::from_millis(1_500),
        "shutdown took {took:?}"
    );
    for signal in telemetry::Signal::ALL {
        let s = stats.signal(signal);
        assert_eq!(s.exported, 0, "{signal:?}: {s:?}");
        assert!(s.failed_attempts > 0, "{signal:?}: {s:?}");
        assert!(s.dropped() > 0, "{signal:?}: {s:?}");
        assert_eq!(s.queued, 0, "{signal:?}: {s:?}");
    }
    assert!(stats.traces.dropped_overflow > 0, "{:?}", stats.traces);
    assert!(stats.logs.dropped_overflow > 0, "{:?}", stats.logs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_collector_is_harmless() {
    let t = telemetry::init(&settings(&closed_port_endpoint(), |o| {
        o.timeout_ms = Some(2_000);
        o.max_retries = Some(1);
        o.shutdown_timeout_ms = Some(300);
    }))
    .unwrap();
    let dir = TempDir::new().unwrap();
    let base = start_depot(dir.path()).await;
    let client = reqwest::Client::new();
    for _ in 0..50 {
        let t0 = Instant::now();
        let resp = client
            .get(format!("{base}/versions/0/"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(t0.elapsed() < Duration::from_secs(1));
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (stats, took) = shutdown(t).await;
    assert!(took < Duration::from_secs(2), "shutdown took {took:?}");
    assert!(stats.traces.failed_attempts > 0, "{:?}", stats.traces);
    assert_eq!(stats.traces.exported, 0);
    assert_eq!(stats.traces.enqueued, stats.traces.dropped());
}

/// While the collector holds the first batch, later records overflow the
/// queue; the oldest ones are evicted and the newest are delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overflow_evicts_oldest_records_first() {
    let collector = Collector::start(Mode::GateFirst);
    let t = telemetry::init(&settings(&collector.endpoint(), |o| {
        o.traces = Some(false);
        o.metrics = Some(false);
        o.queue_size = Some(8);
        o.batch_size = Some(8);
        // Only a full batch wakes the worker, so the first batch is exactly
        // seq 0..8.
        o.flush_interval_ms = Some(60_000);
        o.timeout_ms = Some(10_000);
    }))
    .unwrap();

    for i in 0..8 {
        tracing::info!(target: "depot_test", "seq {i}");
    }
    // The first batch is now stuck in the collector.
    wait_until("first export", Duration::from_secs(5), || {
        collector.started() == 1
    })
    .await;
    for i in 8..50 {
        tracing::info!(target: "depot_test", "seq {i}");
    }
    let before_release = t.stats().logs;
    assert_eq!(before_release.queued, 8);
    assert_eq!(before_release.dropped_overflow, 34, "{before_release:?}");

    collector.release();
    let (stats, _) = shutdown(t).await;
    let seqs: BTreeSet<u32> = logs(&collector)
        .iter()
        .filter_map(|l| l.body.strip_prefix("seq ")?.parse().ok())
        .collect();
    let expected: BTreeSet<u32> = (0..8).chain(42..50).collect();
    assert_eq!(seqs, expected);
    assert_eq!(stats.logs.dropped_overflow, 34);
    assert_eq!(stats.logs.exported, 16);
    assert_eq!(stats.logs.dropped(), 34);
}

/// Records still queued at shutdown are flushed, not lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_flushes_queued_records() {
    let collector = Collector::start(Mode::Healthy);
    let t = telemetry::init(&settings(&collector.endpoint(), |o| {
        // Nothing is sent before shutdown.
        o.flush_interval_ms = Some(60_000);
        o.metrics_interval_ms = Some(3_600_000);
    }))
    .unwrap();
    for i in 0..5 {
        tracing::info!(target: "depot_test", "pending {i}");
    }
    let timer = operation::start(Operation::LocalImport);
    let result: Result<(), ()> = Err(());
    timer.finish_with(&result);
    drop(operation::start(Operation::Retirement)); // counted as cancelled
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(collector.all_bodies().is_empty());

    let (stats, took) = shutdown(t).await;
    assert!(took < Duration::from_secs(3), "shutdown took {took:?}");
    assert_eq!(stats.logs.exported, 5, "{:?}", stats.logs);
    assert_eq!(stats.traces.exported, 2, "{:?}", stats.traces);
    assert_eq!(stats.metrics.exported, 1, "{:?}", stats.metrics);
    let bodies: Vec<String> = logs(&collector).into_iter().map(|l| l.body).collect();
    assert_eq!(bodies.len(), 5);
    let ops = &metrics(&collector)["pkg6depotd.operations"];
    assert!(
        ops.iter()
            .any(|a| a["pkg6depotd.operation"] == "local_import"
                && a["pkg6depotd.result"] == "error")
    );
    assert!(ops.iter().any(
        |a| a["pkg6depotd.operation"] == "retirement" && a["pkg6depotd.result"] == "cancelled"
    ));
}

/// The final metrics export may take the whole configured shutdown budget,
/// beyond the 5 s the SDK's periodic reader waits for itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_metrics_export_gets_the_configured_shutdown_timeout() {
    let collector = Collector::start(Mode::Slow(Duration::from_millis(6_500)));
    let t = telemetry::init(&settings(&collector.endpoint(), |o| {
        o.traces = Some(false);
        o.logs = Some(false);
        o.metrics_interval_ms = Some(3_600_000);
        o.timeout_ms = Some(10_000);
        o.shutdown_timeout_ms = Some(9_000);
    }))
    .unwrap();
    operation::start(Operation::CatalogRebuild).finish_with(&Ok::<(), ()>(()));

    let (stats, took) = shutdown(t).await;
    assert_eq!(stats.metrics.exported, 1, "{:?}", stats.metrics);
    assert_eq!(stats.metrics.dropped(), 0, "{:?}", stats.metrics);
    assert!(
        took >= Duration::from_millis(6_500),
        "shutdown took {took:?}"
    );
    assert!(
        took < Duration::from_millis(9_500),
        "shutdown took {took:?}"
    );
    assert!(metrics(&collector).contains_key("pkg6depotd.operations"));
}

/// A collector that rejects the payload (400) gets it once, not max-retries
/// more times.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permanent_rejection_is_not_retried() {
    let collector = Collector::start(Mode::Reject(StatusCode::BAD_REQUEST));
    let t = telemetry::init(&settings(&collector.endpoint(), |o| {
        o.traces = Some(false);
        o.metrics = Some(false);
        o.max_retries = Some(5);
        o.batch_size = Some(4);
        o.flush_interval_ms = Some(60_000);
    }))
    .unwrap();
    for i in 0..4 {
        tracing::info!(target: "depot_test", "rejected {i}");
    }
    wait_until("first export", Duration::from_secs(5), || {
        collector.started() >= 1
    })
    .await;
    // Retries would come after 100 ms, 200 ms, ... of backoff.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(collector.started(), 1);

    let (stats, _) = shutdown(t).await;
    assert_eq!(stats.logs.failed_attempts, 1, "{:?}", stats.logs);
    assert_eq!(stats.logs.dropped_export_failed, 4, "{:?}", stats.logs);
}

/// Proxy variables in the service environment do not divert export.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_environment_is_ignored() {
    let proxy = closed_port_endpoint();
    for var in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        // SAFETY: set before telemetry starts any thread; nextest runs each
        // test in its own process.
        unsafe { std::env::set_var(var, &proxy) };
    }
    for var in ["NO_PROXY", "no_proxy"] {
        unsafe { std::env::remove_var(var) };
    }
    let collector = Collector::start(Mode::Healthy);
    let t = telemetry::init(&settings(&collector.endpoint(), |o| {
        o.traces = Some(false);
        o.metrics = Some(false);
        o.max_retries = Some(0);
    }))
    .unwrap();
    tracing::info!(target: "depot_test", "direct");
    let (stats, _) = shutdown(t).await;
    assert_eq!(stats.logs.exported, 1, "{:?}", stats.logs);
    assert_eq!(stats.logs.failed_attempts, 0, "{:?}", stats.logs);
}

/// Telemetry off: the middleware is a pass-through and nothing is exported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_export_serves_normally() {
    let t = telemetry::init(&TelemetrySettings::default()).unwrap();
    assert!(!t.exporting());
    let dir = TempDir::new().unwrap();
    let base = start_depot(dir.path()).await;
    let resp = reqwest::get(format!("{base}/versions/0/")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let (stats, _) = shutdown(t).await;
    assert_eq!(stats, TelemetryStats::default());
}

/// The daemon flushes all three signals when SMF stops it with SIGTERM.
#[cfg(unix)]
#[test]
fn sigterm_drains_and_flushes_before_exit() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let collector = Collector::start(Mode::Healthy);
    let dir = TempDir::new().unwrap();
    let repo = setup_repo(dir.path());
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let config = dir.path().join("depot.kdl");
    std::fs::write(
        &config,
        format!(
            "server {{\n    bind \"127.0.0.1:{port}\"\n}}\nrepository {{\n    root \"{}\"\n}}\ntelemetry {{\n    otlp {{\n        endpoint \"{}\"\n        metrics-interval-ms 3600000\n    }}\n}}\n",
            repo.display(),
            collector.endpoint()
        ),
    )
    .unwrap();

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin!("pkg6depotd"))
        .args(["--no-daemon", "--config", config.to_str().unwrap(), "start"])
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let url = format!("http://127.0.0.1:{port}/versions/0/");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(resp) = reqwest::blocking::get(&url) {
            assert_eq!(resp.status(), 200);
            break;
        }
        assert!(Instant::now() < deadline, "depot did not come up");
        std::thread::sleep(Duration::from_millis(50));
    }

    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("depot did not exit after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status:?}");

    assert!(
        spans(&collector)
            .iter()
            .any(|s| s.name == "GET /versions/0/"),
        "request span not flushed"
    );
    assert!(
        metrics(&collector).contains_key("pkg6depotd.http.server.requests"),
        "final metrics not flushed"
    );
    let logs = logs(&collector);
    assert!(
        logs.iter().any(|l| l.body == "pkg6depotd stopped"),
        "final log not flushed: {:?}",
        logs.iter().map(|l| &l.body).collect::<Vec<_>>()
    );
}
