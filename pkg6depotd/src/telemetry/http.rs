//! Per-request metrics and server spans.
//!
//! Attributes are bounded: normalised method, axum route template (the raw
//! path never), status code, operation and result.

use super::instruments::{self, Instruments};
use super::{OTEL_TARGET, traces_enabled};
use axum::extract::{MatchedPath, Request};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::KeyValue;
use std::sync::Arc;
use std::time::Instant;
use tracing::Instrument;
use tracing::field::Empty;

/// Route label for requests no route matched (404s, probes, scanners).
pub const UNMATCHED_ROUTE: &str = "unmatched";

/// What a route does, as a low-cardinality label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpOperation {
    Versions,
    Catalog,
    Manifest,
    File,
    Info,
    Publisher,
    Search,
    Index,
    Admin,
    Ui,
    Root,
    Unmatched,
}

impl HttpOperation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Versions => "versions",
            Self::Catalog => "catalog",
            Self::Manifest => "manifest",
            Self::File => "file",
            Self::Info => "info",
            Self::Publisher => "publisher",
            Self::Search => "search",
            Self::Index => "index",
            Self::Admin => "admin",
            Self::Ui => "ui",
            Self::Root => "root",
            Self::Unmatched => "unmatched",
        }
    }

    /// Classify a route template from `routes.rs`, e.g.
    /// `/{publisher}/file/1/{digest}` is `File`.
    pub fn from_route(route: &str) -> Self {
        if route == UNMATCHED_ROUTE {
            return Self::Unmatched;
        }
        if route == "/" {
            return Self::Root;
        }
        let mut segments = route.trim_start_matches('/').split('/');
        let first = segments.next().unwrap_or("");
        let service = if first.starts_with('{') {
            segments.next().unwrap_or("")
        } else {
            first
        };
        match service {
            "versions" => Self::Versions,
            "catalog" => Self::Catalog,
            "manifest" => Self::Manifest,
            "file" => Self::File,
            "info" => Self::Info,
            "publisher" => Self::Publisher,
            "search" => Self::Search,
            "index" => Self::Index,
            "admin" => Self::Admin,
            "ui" => Self::Ui,
            _ => Self::Unmatched,
        }
    }
}

/// Outcome label shared by requests and operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpResult {
    Ok,
    ClientError,
    ServerError,
    /// The response future was dropped: client went away or a handler panicked.
    Cancelled,
}

impl HttpResult {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::ClientError => "client_error",
            Self::ServerError => "server_error",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_status(status: StatusCode) -> Self {
        if status.is_server_error() {
            Self::ServerError
        } else if status.is_client_error() {
            Self::ClientError
        } else {
            Self::Ok
        }
    }
}

/// Known methods as-is, everything else `_OTHER` (semantic conventions).
pub fn normalize_method(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::OPTIONS => "OPTIONS",
        Method::PATCH => "PATCH",
        Method::CONNECT => "CONNECT",
        Method::TRACE => "TRACE",
        _ => "_OTHER",
    }
}

/// Records one request when dropped, so cancelled requests are counted too.
struct RequestRecord {
    instruments: Option<Arc<Instruments>>,
    start: Instant,
    method: &'static str,
    route: String,
    operation: HttpOperation,
    status: Option<StatusCode>,
}

impl Drop for RequestRecord {
    fn drop(&mut self) {
        let Some(i) = &self.instruments else {
            return;
        };
        let result = self
            .status
            .map_or(HttpResult::Cancelled, HttpResult::from_status);
        let mut attrs = vec![
            KeyValue::new("http.request.method", self.method),
            KeyValue::new("http.route", self.route.clone()),
            KeyValue::new("pkg6depotd.operation", self.operation.as_str()),
            KeyValue::new("pkg6depotd.result", result.as_str()),
        ];
        if let Some(status) = self.status {
            attrs.push(KeyValue::new(
                "http.response.status_code",
                i64::from(status.as_u16()),
            ));
        }
        i.http_requests.add(1, &attrs);
        i.http_duration
            .record(self.start.elapsed().as_secs_f64(), &attrs);
        if result == HttpResult::ServerError {
            let status = self.status.map_or(0, |s| s.as_u16());
            i.http_errors.add(
                1,
                &[
                    KeyValue::new("http.request.method", self.method),
                    KeyValue::new("http.route", self.route.clone()),
                    KeyValue::new("pkg6depotd.operation", self.operation.as_str()),
                    KeyValue::new("error.type", status.to_string()),
                ],
            );
        }
        i.http_active.add(-1, &self.active_attrs());
    }
}

impl RequestRecord {
    fn active_attrs(&self) -> [KeyValue; 2] {
        [
            KeyValue::new("http.request.method", self.method),
            KeyValue::new("pkg6depotd.operation", self.operation.as_str()),
        ]
    }
}

/// axum middleware; install with `Router::layer(from_fn(track))` so that
/// `MatchedPath` is set by the time it runs.
pub async fn track(request: Request, next: Next) -> Response {
    let instruments = instruments::current();
    let tracing = traces_enabled();
    if instruments.is_none() && !tracing {
        return next.run(request).await;
    }

    let method = normalize_method(request.method());
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| UNMATCHED_ROUTE.to_string(), |p| p.as_str().to_string());
    let operation = HttpOperation::from_route(&route);
    let mut record = RequestRecord {
        instruments,
        start: Instant::now(),
        method,
        route,
        operation,
        status: None,
    };
    if let Some(i) = &record.instruments {
        i.http_active.add(1, &record.active_attrs());
    }

    let response = if tracing {
        let span = tracing::info_span!(
            target: OTEL_TARGET,
            "http.request",
            otel.name = %format!("{method} {}", record.route),
            otel.kind = "server",
            otel.status_code = Empty,
            http.request.method = method,
            http.route = record.route.as_str(),
            http.response.status_code = Empty,
            pkg6depotd.operation = operation.as_str(),
            pkg6depotd.result = Empty,
        );
        let response = next.run(request).instrument(span.clone()).await;
        let status = response.status();
        let result = HttpResult::from_status(status);
        span.record("http.response.status_code", status.as_u16());
        span.record("pkg6depotd.result", result.as_str());
        if result == HttpResult::ServerError {
            span.record("otel.status_code", "error");
        }
        response
    } else {
        next.run(request).await
    };
    record.status = Some(response.status());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_map_to_operations() {
        let cases = [
            ("/versions/0/", HttpOperation::Versions),
            ("/{publisher}/catalog/1/{filename}", HttpOperation::Catalog),
            ("/{publisher}/catalog/2/{sha256}", HttpOperation::Catalog),
            ("/{publisher}/manifest/0/{fmri}", HttpOperation::Manifest),
            ("/{publisher}/file/1/{digest}", HttpOperation::File),
            ("/{publisher}/info/0/{fmri}", HttpOperation::Info),
            ("/publisher/0", HttpOperation::Publisher),
            ("/{publisher}/publisher/1/", HttpOperation::Publisher),
            ("/search/1/", HttpOperation::Search),
            ("/{publisher}/index/0/{command}", HttpOperation::Index),
            ("/admin/health", HttpOperation::Admin),
            ("/ui/package/{publisher}/{*fmri}", HttpOperation::Ui),
            ("/", HttpOperation::Root),
            (UNMATCHED_ROUTE, HttpOperation::Unmatched),
        ];
        for (route, op) in cases {
            assert_eq!(HttpOperation::from_route(route), op, "{route}");
        }
    }

    #[test]
    fn methods_are_normalised() {
        assert_eq!(normalize_method(&Method::GET), "GET");
        let odd = Method::from_bytes(b"PROPFIND").unwrap();
        assert_eq!(normalize_method(&odd), "_OTHER");
    }
}
