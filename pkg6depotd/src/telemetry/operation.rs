//! Duration/result metrics and spans for long-running repository operations.
//!
//! Call sites wrap the work in a timer and report how it ended:
//!
//! ```no_run
//! use pkg6depotd::telemetry::operation::{self, Operation};
//! # fn import() -> Result<(), std::io::Error> { Ok(()) }
//! let timer = operation::start(Operation::LocalImport);
//! let result = timer.in_span(import);
//! timer.finish_with(&result);
//! ```
//!
//! For async work, instrument the future with `timer.span().clone()`. A timer
//! dropped without `finish` records `cancelled`, so early returns and panics
//! are still counted. Nothing here blocks or allocates per record beyond the
//! attribute array; with telemetry off it only reads a lock.

use super::instruments;
use super::{OTEL_TARGET, traces_enabled};
use opentelemetry::KeyValue;
use std::time::Instant;
use tracing::field::Empty;

/// A repository operation worth timing. Attribute value `pkg6depotd.operation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Operation {
    /// Importing a pkg5 or pkg6 repository on the local host.
    LocalImport,
    /// Rebuilding catalogs and indexes.
    CatalogRebuild,
    /// Retiring (obsoleting or removing) packages.
    Retirement,
}

impl Operation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalImport => "local_import",
            Self::CatalogRebuild => "catalog_rebuild",
            Self::Retirement => "retirement",
        }
    }
}

/// How an operation ended. Attribute value `pkg6depotd.result`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationResult {
    Ok,
    Error,
    Cancelled,
}

impl OperationResult {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Times one operation. Obtain with [`start`].
#[must_use = "an unfinished timer records the operation as cancelled"]
pub struct OperationTimer {
    operation: Operation,
    start: Instant,
    span: tracing::Span,
    finished: bool,
}

/// Start timing `operation`, opening a span if traces are exported.
pub fn start(operation: Operation) -> OperationTimer {
    let span = if traces_enabled() {
        tracing::info_span!(
            target: OTEL_TARGET,
            "operation",
            otel.name = operation.as_str(),
            otel.kind = "internal",
            otel.status_code = Empty,
            pkg6depotd.operation = operation.as_str(),
            pkg6depotd.result = Empty,
        )
    } else {
        tracing::Span::none()
    };
    OperationTimer {
        operation,
        start: Instant::now(),
        span,
        finished: false,
    }
}

impl OperationTimer {
    pub fn operation(&self) -> Operation {
        self.operation
    }

    /// The operation's span, for `Instrument::instrument` on async work.
    pub fn span(&self) -> &tracing::Span {
        &self.span
    }

    /// Run synchronous work inside the operation's span.
    pub fn in_span<T>(&self, work: impl FnOnce() -> T) -> T {
        self.span.in_scope(work)
    }

    pub fn finish(mut self, result: OperationResult) {
        self.record(result);
    }

    /// `Ok` for `Ok(_)`, `Error` for `Err(_)`.
    pub fn finish_with<T, E>(self, result: &Result<T, E>) {
        self.finish(if result.is_ok() {
            OperationResult::Ok
        } else {
            OperationResult::Error
        });
    }

    fn record(&mut self, result: OperationResult) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        self.span.record("pkg6depotd.result", result.as_str());
        if result == OperationResult::Error {
            self.span.record("otel.status_code", "error");
        }
        if let Some(i) = instruments::current() {
            let attrs = [
                KeyValue::new("pkg6depotd.operation", self.operation.as_str()),
                KeyValue::new("pkg6depotd.result", result.as_str()),
            ];
            i.operations.add(1, &attrs);
            i.operation_duration
                .record(self.start.elapsed().as_secs_f64(), &attrs);
        }
    }
}

impl Drop for OperationTimer {
    fn drop(&mut self) {
        self.record(OperationResult::Cancelled);
    }
}
