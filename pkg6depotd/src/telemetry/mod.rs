//! Logging and OpenTelemetry export.
//!
//! Local logs always go to stdout (text or JSON). With `telemetry { otlp {} }`
//! enabled, traces, metrics and logs are also exported over OTLP/HTTP through
//! bounded, drop-oldest queues that never block request handling (ADR-006).
//!
//! - [`init`] installs the global subscriber and starts export.
//! - [`Telemetry::shutdown`] flushes within the configured deadline.
//! - [`http::track`] is the per-request middleware.
//! - [`operation::start`] times imports, rebuilds and retirements (no
//!   production call site yet).

pub mod http;
mod instruments;
pub mod operation;
mod pipeline;
mod queue;
pub mod settings;

pub use queue::{Signal, SignalStats, TelemetryStats};
pub use settings::{LogFormat, OtlpSettings, TelemetryConfigError, TelemetrySettings};

use miette::Diagnostic;
use pipeline::{BoxedLayer, Pipeline};
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

/// Target of the spans that are exported as traces. Only spans with exactly
/// this target reach the collector, so their attributes stay bounded.
pub const OTEL_TARGET: &str = "pkg6depotd::otel";

static TRACES_ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether export spans are wanted; instrumentation skips them otherwise.
pub(crate) fn traces_enabled() -> bool {
    TRACES_ENABLED.load(Ordering::Relaxed)
}

#[derive(Debug, Error, Diagnostic)]
pub enum TelemetryError {
    #[error("cannot start the telemetry runtime: {0}")]
    #[diagnostic(
        code(ips::depot_error::telemetry::runtime),
        help("The process could not create threads; check resource limits.")
    )]
    Runtime(#[source] std::io::Error),

    #[error("cannot build the HTTP client for OTLP export")]
    #[diagnostic(
        code(ips::depot_error::telemetry::http_client),
        help("The TLS backend failed to initialise; check the system certificate store.")
    )]
    HttpClient,

    // The builder's error text embeds the endpoint, so it is not shown.
    #[error("cannot build the OTLP {} exporter", .0.as_str())]
    #[diagnostic(
        code(ips::depot_error::telemetry::exporter),
        help("Check telemetry.otlp.endpoint; it must be an http(s) base URL.")
    )]
    Exporter(Signal),

    #[error("a global tracing subscriber is already installed")]
    #[diagnostic(code(ips::depot_error::telemetry::already_initialized))]
    AlreadyInitialized,
}

/// Running telemetry. Call [`Telemetry::shutdown`] before exit to flush.
pub struct Telemetry {
    pipeline: Option<Pipeline>,
}

impl Telemetry {
    /// Whether OTLP export is running.
    pub fn exporting(&self) -> bool {
        self.pipeline.is_some()
    }

    pub fn stats(&self) -> TelemetryStats {
        self.pipeline
            .as_ref()
            .map(Pipeline::stats)
            .unwrap_or_default()
    }

    /// Flush and stop export. Blocks for at most `shutdown-timeout` plus a
    /// short grace period; run it off the async executor
    /// (`tokio::task::spawn_blocking`).
    pub fn shutdown(mut self) -> TelemetryStats {
        let Some(mut pipeline) = self.pipeline.take() else {
            return TelemetryStats::default();
        };
        TRACES_ENABLED.store(false, Ordering::Relaxed);
        instruments::uninstall();
        let stats = pipeline.shutdown();
        let dropped = stats.traces.dropped() + stats.metrics.dropped() + stats.logs.dropped();
        if dropped > 0 {
            tracing::warn!(
                target: "pkg6depotd::telemetry",
                dropped_traces = stats.traces.dropped(),
                dropped_metrics = stats.metrics.dropped(),
                dropped_logs = stats.logs.dropped(),
                "telemetry records were dropped before export"
            );
        }
        stats
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if self.pipeline.is_some() {
            TRACES_ENABLED.store(false, Ordering::Relaxed);
            instruments::uninstall();
        }
        // Pipeline's own Drop stops export without waiting for a flush.
    }
}

/// Install the global tracing subscriber and, if configured, start OTLP
/// export. Call once per process, after daemonizing: export runs on threads.
pub fn init(settings: &TelemetrySettings) -> Result<Telemetry, TelemetryError> {
    let pipeline = settings
        .otlp
        .as_ref()
        .map(|otlp| Pipeline::start(&settings.service_name, otlp))
        .transpose()?;

    let mut layers: Vec<BoxedLayer> = vec![local_layer(settings.log_format)];
    if let Some(pipeline) = &pipeline {
        layers.extend(pipeline.layers());
    }
    tracing_subscriber::registry()
        .with(layers)
        .try_init()
        .map_err(|_| TelemetryError::AlreadyInitialized)?;

    if let Some(pipeline) = &pipeline {
        if let Some(provider) = pipeline.meter_provider() {
            instruments::install(provider, pipeline.counters());
        }
        let otlp = settings.otlp.as_ref().expect("pipeline implies otlp");
        TRACES_ENABLED.store(otlp.signals.traces, Ordering::Relaxed);
        tracing::info!(
            endpoint = %otlp.endpoint,
            traces = otlp.signals.traces,
            metrics = otlp.signals.metrics,
            logs = otlp.signals.logs,
            "exporting telemetry over OTLP/HTTP"
        );
    }
    Ok(Telemetry { pipeline })
}

/// Local output, filtered by `RUST_LOG` as before.
fn local_layer(format: LogFormat) -> BoxedLayer {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,pkg6depotd=debug"));
    let fmt = tracing_subscriber::fmt::layer();
    match format {
        LogFormat::Text => Box::new(fmt.with_filter(filter)),
        LogFormat::Json => Box::new(fmt.json().with_filter(filter)),
    }
}
