//! Metric instruments and the process-wide handle to them.
//!
//! Every attribute value comes from a closed set: normalised HTTP methods,
//! axum route templates, status codes, and the enums in this module. Nothing
//! derived from a request path, FMRI or digest is ever recorded.

use super::queue::{Counters, DropReason, Signal};
use opentelemetry::KeyValue;
use opentelemetry::metrics::{
    Counter, Histogram, Meter, MeterProvider as _, ObservableCounter, UpDownCounter,
};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use std::sync::{Arc, RwLock};

/// Seconds; the OpenTelemetry HTTP semantic-convention boundaries.
const DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// Operation durations: imports and rebuilds take minutes, not milliseconds.
const OPERATION_BUCKETS: &[f64] = &[
    0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
];

pub(crate) struct Instruments {
    pub(crate) http_requests: Counter<u64>,
    pub(crate) http_duration: Histogram<f64>,
    pub(crate) http_errors: Counter<u64>,
    pub(crate) http_active: UpDownCounter<i64>,
    pub(crate) operations: Counter<u64>,
    pub(crate) operation_duration: Histogram<f64>,
    _telemetry_dropped: ObservableCounter<u64>,
    _telemetry_exported: ObservableCounter<u64>,
}

impl Instruments {
    fn new(meter: &Meter, counters: Vec<(Signal, Arc<Counters>)>) -> Self {
        let counters = Arc::new(counters);
        let dropped_counters = counters.clone();
        Self {
            http_requests: meter
                .u64_counter("pkg6depotd.http.server.requests")
                .with_description("HTTP requests handled, by route, status and result.")
                .with_unit("{request}")
                .build(),
            http_duration: meter
                .f64_histogram("http.server.request.duration")
                .with_description("Duration of HTTP server requests.")
                .with_unit("s")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            http_errors: meter
                .u64_counter("pkg6depotd.http.server.errors")
                .with_description("HTTP requests that ended in a server error (5xx).")
                .with_unit("{request}")
                .build(),
            http_active: meter
                .i64_up_down_counter("http.server.active_requests")
                .with_description("HTTP requests currently being handled.")
                .with_unit("{request}")
                .build(),
            operations: meter
                .u64_counter("pkg6depotd.operations")
                .with_description("Repository operations (import, rebuild, retirement) by result.")
                .with_unit("{operation}")
                .build(),
            operation_duration: meter
                .f64_histogram("pkg6depotd.operation.duration")
                .with_description("Duration of repository operations.")
                .with_unit("s")
                .with_boundaries(OPERATION_BUCKETS.to_vec())
                .build(),
            _telemetry_dropped: meter
                .u64_observable_counter("pkg6depotd.telemetry.dropped")
                .with_description("Telemetry records dropped before export, by signal and reason.")
                .with_unit("{record}")
                .with_callback(move |observer| {
                    for (signal, c) in dropped_counters.iter() {
                        for reason in DropReason::ALL {
                            observer.observe(
                                c.dropped_by(reason),
                                &[
                                    KeyValue::new("signal", signal.as_str()),
                                    KeyValue::new("reason", reason.as_str()),
                                ],
                            );
                        }
                    }
                })
                .build(),
            _telemetry_exported: meter
                .u64_observable_counter("pkg6depotd.telemetry.exported")
                .with_description("Telemetry records acknowledged by the collector, by signal.")
                .with_unit("{record}")
                .with_callback(move |observer| {
                    for (signal, c) in counters.iter() {
                        observer.observe(
                            c.exported_total(),
                            &[KeyValue::new("signal", signal.as_str())],
                        );
                    }
                })
                .build(),
        }
    }
}

static CURRENT: RwLock<Option<Arc<Instruments>>> = RwLock::new(None);

/// Install instruments from `provider`, replacing any earlier ones.
pub(crate) fn install(provider: &SdkMeterProvider, counters: Vec<(Signal, Arc<Counters>)>) {
    let meter = provider.meter("pkg6depotd");
    let instruments = Arc::new(Instruments::new(&meter, counters));
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some(instruments);
}

pub(crate) fn uninstall() {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = None;
}

/// `None` when metrics export is off: recording is then skipped entirely.
pub(crate) fn current() -> Option<Arc<Instruments>> {
    CURRENT.read().unwrap_or_else(|e| e.into_inner()).clone()
}
