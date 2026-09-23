//! OTLP/HTTP export pipeline: SDK providers whose processors feed the bounded
//! queues, a dedicated runtime running the export workers, and the tracing
//! layers that bridge spans and events into the providers. See ADR-006.

use super::queue::{
    BatchExport, BoundedQueue, BoxFuture, Counters, DIAG_TARGET, DropReason, ShutdownRx, Signal,
    TelemetryStats, TelemetryThreadScope, export_with_retry, mark_telemetry_thread,
    on_telemetry_thread, run_worker, sdk_error,
};
use super::settings::{ExportPolicy, OtlpSettings};
use super::{OTEL_TARGET, TelemetryError};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{Context, InstrumentationScope, KeyValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{
    LogBatch, LogExporter as _, LogProcessor, SdkLogRecord, SdkLoggerProvider,
};
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};
use opentelemetry_sdk::trace::{
    Sampler, SdkTracerProvider, Span, SpanData, SpanExporter as _, SpanProcessor,
};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tokio::sync::watch;
use tracing_subscriber::filter::{EnvFilter, filter_fn};
use tracing_subscriber::{Layer, Registry};

pub(crate) type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

type LogItem = (SdkLogRecord, InstrumentationScope);

/// Crates whose events must never become exported log records: the export
/// path itself would otherwise log about every export it makes.
const NEVER_EXPORTED: &[&str] = &[
    "hyper",
    "hyper_util",
    "h2",
    "reqwest",
    "rustls",
    "tower",
    "opentelemetry",
    "opentelemetry_sdk",
    "opentelemetry_otlp",
    "opentelemetry_http",
    "tracing_opentelemetry",
    DIAG_TARGET,
];

/// Extra time for workers to report back after the flush deadline.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

pub(crate) struct Pipeline {
    runtime: Option<Runtime>,
    shutdown_tx: watch::Sender<Option<Instant>>,
    tracer_provider: Option<SdkTracerProvider>,
    logger_provider: Option<SdkLoggerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    /// Signalled once the periodic reader's final export has finished;
    /// disconnected if the reader dropped its exporter without one.
    meter_done: Option<mpsc::Receiver<()>>,
    workers: Vec<(Signal, mpsc::Receiver<()>)>,
    span_queue: Option<Arc<BoundedQueue<SpanData>>>,
    log_queue: Option<Arc<BoundedQueue<LogItem>>>,
    metric_counters: Arc<Counters>,
    settings: OtlpSettings,
}

impl Pipeline {
    pub(crate) fn start(
        service_name: &str,
        settings: &OtlpSettings,
    ) -> Result<Self, TelemetryError> {
        let (shutdown_tx, shutdown_rx) = watch::channel(None);
        let resource = Resource::builder()
            .with_service_name(service_name.to_string())
            .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
            .build();
        // Bounds connect + request + body for every attempt, independent of
        // the per-attempt timeout the worker applies on top. Export goes to
        // the configured endpoint only: proxy variables inherited from the
        // environment are ignored and redirects are not followed.
        let client = reqwest::Client::builder()
            .timeout(settings.export.timeout)
            .connect_timeout(settings.export.timeout)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(1)
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| TelemetryError::HttpClient)?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .thread_name("pkg6depotd-otel")
            .on_thread_start(mark_telemetry_thread)
            .enable_all()
            .build()
            .map_err(TelemetryError::Runtime)?;
        // Owned by the pipeline from here on, so an error below shuts it down
        // through `Drop` instead of dropping a runtime inside async context.
        let handle = runtime.handle().clone();
        let mut pipeline = Pipeline {
            runtime: Some(runtime),
            shutdown_tx,
            tracer_provider: None,
            logger_provider: None,
            meter_provider: None,
            meter_done: None,
            workers: Vec::new(),
            span_queue: None,
            log_queue: None,
            metric_counters: Arc::new(Counters::default()),
            settings: settings.clone(),
        };
        let url = |signal: Signal| settings.endpoint.signal_url(signal.path());

        if settings.signals.traces {
            let mut exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_http_client(client.clone())
                .with_protocol(Protocol::HttpBinary)
                .with_endpoint(url(Signal::Traces))
                .with_timeout(settings.export.timeout)
                .build()
                .map_err(|_| TelemetryError::Exporter(Signal::Traces))?;
            exporter.set_resource(&resource);
            let queue = Arc::new(BoundedQueue::new(
                &settings.queue,
                Arc::new(Counters::default()),
            ));
            pipeline.spawn_worker(
                &handle,
                Signal::Traces,
                queue.clone(),
                SpanSink(Arc::new(exporter)),
                shutdown_rx.clone(),
            );
            let sampler =
                Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(settings.sample_ratio)));
            pipeline.tracer_provider = Some(
                SdkTracerProvider::builder()
                    .with_span_processor(QueueSpanProcessor(queue.clone()))
                    .with_sampler(sampler)
                    .with_resource(resource.clone())
                    .build(),
            );
            pipeline.span_queue = Some(queue);
        }

        if settings.signals.logs {
            let mut exporter = opentelemetry_otlp::LogExporter::builder()
                .with_http()
                .with_http_client(client.clone())
                .with_protocol(Protocol::HttpBinary)
                .with_endpoint(url(Signal::Logs))
                .with_timeout(settings.export.timeout)
                .build()
                .map_err(|_| TelemetryError::Exporter(Signal::Logs))?;
            exporter.set_resource(&resource);
            let queue = Arc::new(BoundedQueue::new(
                &settings.queue,
                Arc::new(Counters::default()),
            ));
            pipeline.spawn_worker(
                &handle,
                Signal::Logs,
                queue.clone(),
                LogSink(Arc::new(exporter)),
                shutdown_rx.clone(),
            );
            pipeline.logger_provider = Some(
                SdkLoggerProvider::builder()
                    .with_log_processor(QueueLogProcessor(queue.clone()))
                    .with_resource(resource.clone())
                    .build(),
            );
            pipeline.log_queue = Some(queue);
        }

        if settings.signals.metrics {
            let exporter = opentelemetry_otlp::MetricExporter::builder()
                .with_http()
                .with_http_client(client)
                .with_protocol(Protocol::HttpBinary)
                .with_endpoint(url(Signal::Metrics))
                .with_timeout(settings.export.timeout)
                .with_temporality(Temporality::Cumulative)
                .build()
                .map_err(|_| TelemetryError::Exporter(Signal::Metrics))?;
            let (done_tx, done_rx) = mpsc::sync_channel(1);
            let reader = PeriodicReader::builder(MetricSink {
                inner: exporter,
                runtime: handle,
                policy: settings.export,
                counters: pipeline.metric_counters.clone(),
                shutdown: shutdown_rx,
                done: done_tx,
            })
            .with_interval(settings.metrics_interval)
            .build();
            pipeline.meter_provider = Some(
                SdkMeterProvider::builder()
                    .with_reader(reader)
                    .with_resource(resource)
                    .build(),
            );
            pipeline.meter_done = Some(done_rx);
        }

        Ok(pipeline)
    }

    fn spawn_worker<T, E>(
        &mut self,
        runtime: &tokio::runtime::Handle,
        signal: Signal,
        queue: Arc<BoundedQueue<T>>,
        exporter: E,
        shutdown: ShutdownRx,
    ) where
        T: Send + Sync + 'static,
        E: BatchExport<T>,
    {
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let policy = self.settings.export;
        let flush = self.settings.queue.flush_interval;
        runtime.spawn(async move {
            run_worker(signal, queue, exporter, policy, flush, shutdown).await;
            let _ = done_tx.send(());
        });
        self.workers.push((signal, done_rx));
    }

    /// The meter for instruments; `None` when metrics export is off.
    pub(crate) fn meter_provider(&self) -> Option<&SdkMeterProvider> {
        self.meter_provider.as_ref()
    }

    /// Layers that feed tracing spans and events into the providers.
    pub(crate) fn layers(&self) -> Vec<BoxedLayer> {
        let mut layers: Vec<BoxedLayer> = Vec::new();
        if let Some(provider) = &self.tracer_provider {
            // Only spans we create with bounded attributes are exported;
            // handler and library spans may carry FMRIs and paths.
            let layer = tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("pkg6depotd"))
                .with_location(false)
                .with_threads(false)
                .with_tracked_inactivity(false)
                .with_level(false)
                .with_filter(filter_fn(|meta| meta.target() == OTEL_TARGET));
            layers.push(Box::new(layer));
        }
        if let Some(provider) = &self.logger_provider {
            let mut directives = vec![self.settings.log_level.to_string().to_lowercase()];
            directives.extend(NEVER_EXPORTED.iter().map(|t| format!("{t}=off")));
            let filter = EnvFilter::builder()
                .parse(directives.join(","))
                .expect("static log filter directives are valid");
            let layer = OpenTelemetryTracingBridge::new(provider).with_filter(filter);
            layers.push(Box::new(layer));
        }
        layers
    }

    pub(crate) fn stats(&self) -> TelemetryStats {
        let empty = || Counters::default().snapshot(0);
        TelemetryStats {
            traces: self.span_queue.as_ref().map_or_else(empty, |q| q.stats()),
            logs: self.log_queue.as_ref().map_or_else(empty, |q| q.stats()),
            metrics: self.metric_counters.snapshot(0),
        }
    }

    /// Counters for the exporter's self-observation metrics.
    pub(crate) fn counters(&self) -> Vec<(Signal, Arc<Counters>)> {
        let mut all = vec![(Signal::Metrics, self.metric_counters.clone())];
        if let Some(q) = &self.span_queue {
            all.push((Signal::Traces, q.counters()));
        }
        if let Some(q) = &self.log_queue {
            all.push((Signal::Logs, q.counters()));
        }
        all
    }

    /// Flush all signals in parallel against one deadline, then stop.
    /// Returns within `shutdown-timeout` plus a small grace period, whatever
    /// the collector does.
    pub(crate) fn shutdown(&mut self) -> TelemetryStats {
        let Some(runtime) = self.runtime.take() else {
            return self.stats();
        };
        let deadline = Instant::now() + self.settings.shutdown_timeout;
        self.shutdown_tx.send_replace(Some(deadline));

        // The periodic reader does a final collect + export on its own thread;
        // our exporter bounds that by the same deadline. The SDK stops
        // waiting for that thread after a fixed 5 s, whatever timeout it is
        // passed, so completion is reported by `MetricSink` instead.
        let meter = self.meter_provider.take().map(|provider| {
            let rx = self.meter_done.take().expect("set with the meter provider");
            std::thread::Builder::new()
                .name("pkg6depotd-otel-shutdown".into())
                .spawn(move || {
                    let _ = provider.shutdown();
                })
                .map(|_| rx)
        });

        let mut clean = true;
        let wait = |rx: &mpsc::Receiver<()>| {
            let left = deadline.saturating_duration_since(Instant::now()) + SHUTDOWN_GRACE;
            rx.recv_timeout(left).is_ok()
        };
        for (signal, rx) in &self.workers {
            if !wait(rx) {
                clean = false;
                tracing::warn!(target: DIAG_TARGET, signal = signal.as_str(), "telemetry worker did not stop in time");
            }
        }
        match meter {
            Some(Ok(rx)) => clean &= wait(&rx),
            Some(Err(_)) => clean = false,
            None => {}
        }
        if let Some(provider) = self.tracer_provider.take() {
            let _ = provider.shutdown();
        }
        if let Some(provider) = self.logger_provider.take() {
            let _ = provider.shutdown();
        }

        let stats = self.stats();
        if clean {
            runtime.shutdown_background();
        } else {
            // A thread we do not own may still be inside an export on this
            // runtime; shutting it down would make that thread panic. The
            // process is about to exit, so leave it be.
            std::mem::forget(runtime);
        }
        stats
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        if self.runtime.is_some() {
            self.settings.shutdown_timeout = Duration::ZERO;
            self.shutdown();
        }
    }
}

/// Feeds finished, sampled spans into the trace queue.
#[derive(Debug)]
struct QueueSpanProcessor(Arc<BoundedQueue<SpanData>>);

impl SpanProcessor for QueueSpanProcessor {
    fn on_start(&self, _span: &mut Span, _cx: &Context) {}

    fn on_end(&self, span: SpanData) {
        if span.span_context.is_sampled() && !on_telemetry_thread() {
            self.0.push(span);
        }
    }

    /// Export is driven by the worker; see `Pipeline::shutdown`.
    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

/// Feeds log records into the log queue.
#[derive(Debug)]
struct QueueLogProcessor(Arc<BoundedQueue<LogItem>>);

impl LogProcessor for QueueLogProcessor {
    fn emit(&self, record: &mut SdkLogRecord, scope: &InstrumentationScope) {
        if !on_telemetry_thread() {
            self.0.push((record.clone(), scope.clone()));
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

struct SpanSink(Arc<opentelemetry_otlp::SpanExporter>);

impl BatchExport<SpanData> for SpanSink {
    fn export(&self, batch: Arc<Vec<SpanData>>) -> BoxFuture<'static, OTelSdkResult> {
        use opentelemetry_sdk::trace::SpanExporter;
        let exporter = self.0.clone();
        Box::pin(async move { exporter.export(batch.to_vec()).await })
    }
}

struct LogSink(Arc<opentelemetry_otlp::LogExporter>);

impl BatchExport<LogItem> for LogSink {
    fn export(&self, batch: Arc<Vec<LogItem>>) -> BoxFuture<'static, OTelSdkResult> {
        use opentelemetry_sdk::logs::LogExporter;
        let exporter = self.0.clone();
        Box::pin(async move {
            let refs: Vec<(&SdkLogRecord, &InstrumentationScope)> = batch
                .iter()
                .map(|(record, scope)| (record, scope))
                .collect();
            exporter.export(LogBatch::new(&refs)).await
        })
    }
}

/// Runs the OTLP metric export for the SDK's periodic reader on the telemetry
/// runtime, with the shared timeout/retry policy. The reader's thread waits;
/// instruments never do.
struct MetricSink {
    inner: opentelemetry_otlp::MetricExporter,
    runtime: tokio::runtime::Handle,
    policy: ExportPolicy,
    counters: Arc<Counters>,
    shutdown: ShutdownRx,
    /// The reader calls `shutdown_with_timeout` right after its final export.
    done: mpsc::SyncSender<()>,
}

impl PushMetricExporter for MetricSink {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let _scope = TelemetryThreadScope::enter();
        self.counters.enqueued(1);
        let exported = self.runtime.block_on(export_with_retry(
            Signal::Metrics,
            &self.policy,
            &self.counters,
            &self.shutdown,
            || -> BoxFuture<'_, OTelSdkResult> { Box::pin(self.inner.export(metrics)) },
        ));
        if exported {
            self.counters.exported(1);
            Ok(())
        } else {
            // Cumulative temporality: the next snapshot supersedes this one.
            self.counters.dropped(DropReason::ExportFailed, 1);
            Err(sdk_error("metrics export failed"))
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        let result = self.inner.shutdown_with_timeout(timeout);
        let _ = self.done.try_send(());
        result
    }

    fn temporality(&self) -> Temporality {
        Temporality::Cumulative
    }
}
