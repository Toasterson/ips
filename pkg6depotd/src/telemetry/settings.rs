//! Validated telemetry settings.
//!
//! `config::TelemetryConfig` holds whatever the KDL file said; this module
//! turns it into typed settings or refuses to. Error messages never repeat the
//! endpoint: an operator may have pasted a token into it.

use crate::config::{OtlpConfig, TelemetryConfig};
use miette::Diagnostic;
use reqwest::Url;
use std::fmt;
use std::time::Duration;
use thiserror::Error;
use tracing::level_filters::LevelFilter;

pub const DEFAULT_SERVICE_NAME: &str = "pkg6depotd";

/// Local log output format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

/// Everything telemetry needs, validated.
#[derive(Clone, Debug, PartialEq)]
pub struct TelemetrySettings {
    pub service_name: String,
    pub log_format: LogFormat,
    /// `None` when OTLP export is absent or disabled.
    pub otlp: Option<OtlpSettings>,
}

impl Default for TelemetrySettings {
    fn default() -> Self {
        Self {
            service_name: DEFAULT_SERVICE_NAME.to_string(),
            log_format: LogFormat::Text,
            otlp: None,
        }
    }
}

/// Which signals are exported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signals {
    pub traces: bool,
    pub metrics: bool,
    pub logs: bool,
}

/// OTLP/HTTP export settings.
#[derive(Clone, Debug, PartialEq)]
pub struct OtlpSettings {
    pub endpoint: OtlpEndpoint,
    pub signals: Signals,
    /// Minimum level of exported log records.
    pub log_level: LevelFilter,
    /// Fraction of root spans (HTTP requests, operations) that are sampled.
    pub sample_ratio: f64,
    pub export: ExportPolicy,
    pub queue: QueuePolicy,
    pub metrics_interval: Duration,
    pub shutdown_timeout: Duration,
}

/// Per-batch timeout and retry policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportPolicy {
    /// Upper bound on one export attempt, connect included.
    pub timeout: Duration,
    /// Retries after the first failed attempt.
    pub max_retries: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

/// Per-signal buffering for traces and logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueuePolicy {
    /// Records held per signal. When full, the oldest record is dropped.
    pub capacity: usize,
    pub batch_size: usize,
    pub flush_interval: Duration,
}

/// Collector base URL. Only `scheme://host[:port][/prefix]`.
#[derive(Clone, PartialEq, Eq)]
pub struct OtlpEndpoint(Url);

impl OtlpEndpoint {
    pub fn parse(raw: &str) -> Result<Self, TelemetryConfigError> {
        use EndpointProblem::*;
        let url = Url::parse(raw.trim()).map_err(|_| TelemetryConfigError::Endpoint(NotAUrl))?;
        match url.scheme() {
            "http" | "https" => {}
            _ => return Err(TelemetryConfigError::Endpoint(Scheme)),
        }
        if url.host_str().is_none_or(str::is_empty) {
            return Err(TelemetryConfigError::Endpoint(NoHost));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(TelemetryConfigError::Endpoint(Credentials));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(TelemetryConfigError::Endpoint(QueryOrFragment));
        }
        Ok(Self(url))
    }

    /// Full URL for one signal, e.g. `http://collector:4318/v1/traces`.
    pub fn signal_url(&self, path: &str) -> String {
        format!("{}{}", self.0.as_str().trim_end_matches('/'), path)
    }
}

/// Only scheme, host and port: safe to log even if a path prefix holds a token.
impl fmt::Display for OtlpEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let port = self.0.port_or_known_default().unwrap_or(0);
        write!(
            f,
            "{}://{}:{}",
            self.0.scheme(),
            self.0.host_str().unwrap_or(""),
            port
        )
    }
}

impl fmt::Debug for OtlpEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OtlpEndpoint({self})")
    }
}

/// Why an endpoint was rejected. The value itself is never included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointProblem {
    NotAUrl,
    Scheme,
    NoHost,
    Credentials,
    QueryOrFragment,
}

impl fmt::Display for EndpointProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotAUrl => "it is not an absolute URL",
            Self::Scheme => "only http and https are supported",
            Self::NoHost => "it has no host",
            Self::Credentials => "it contains user credentials",
            Self::QueryOrFragment => "it contains a query or fragment",
        })
    }
}

#[derive(Debug, Error, Diagnostic, PartialEq)]
pub enum TelemetryConfigError {
    #[error("telemetry.otlp-endpoint is not supported")]
    #[diagnostic(
        code(ips::depot_error::telemetry::legacy_endpoint),
        help("Move the URL into the otlp block: telemetry {{ otlp {{ endpoint ... }} }}.")
    )]
    LegacyEndpoint,

    #[error("telemetry.otlp.endpoint is required when OTLP export is enabled")]
    #[diagnostic(
        code(ips::depot_error::telemetry::missing_endpoint),
        help("Set endpoint to the collector's OTLP/HTTP base URL, or set enabled false.")
    )]
    MissingEndpoint,

    #[error("telemetry.otlp.endpoint is invalid: {0}")]
    #[diagnostic(
        code(ips::depot_error::telemetry::endpoint),
        help(
            "Use the collector's base URL, e.g. http://127.0.0.1:4318. Credentials, query strings and fragments are not accepted; run a local collector that adds authentication."
        )
    )]
    Endpoint(EndpointProblem),

    #[error("telemetry.otlp enables no signal")]
    #[diagnostic(
        code(ips::depot_error::telemetry::no_signals),
        help("Enable at least one of traces, metrics and logs, or set enabled false.")
    )]
    NoSignals,

    #[error("telemetry.{key} must be between {min} and {max}")]
    #[diagnostic(code(ips::depot_error::telemetry::range))]
    OutOfRange {
        key: &'static str,
        min: String,
        max: String,
    },

    #[error("telemetry.otlp.batch-size must not exceed queue-size")]
    #[diagnostic(code(ips::depot_error::telemetry::batch_size))]
    BatchLargerThanQueue,

    #[error("telemetry.log-format must be \"text\" or \"json\"")]
    #[diagnostic(code(ips::depot_error::telemetry::log_format))]
    LogFormat,

    #[error("telemetry.otlp.log-level must be one of error, warn, info, debug, trace")]
    #[diagnostic(code(ips::depot_error::telemetry::log_level))]
    LogLevel,

    #[error("telemetry.service-name must be 1 to 255 printable characters")]
    #[diagnostic(code(ips::depot_error::telemetry::service_name))]
    ServiceName,
}

impl TelemetrySettings {
    /// Validate the `telemetry` block. A missing block means local logging only.
    pub fn from_config(config: Option<&TelemetryConfig>) -> Result<Self, TelemetryConfigError> {
        let Some(config) = config else {
            return Ok(Self::default());
        };
        if config.otlp_endpoint.is_some() {
            return Err(TelemetryConfigError::LegacyEndpoint);
        }
        let service_name = match config.service_name.as_deref() {
            None => DEFAULT_SERVICE_NAME.to_string(),
            Some(name)
                if !name.is_empty()
                    && name.len() <= 255
                    && name.chars().all(|c| !c.is_control()) =>
            {
                name.to_string()
            }
            Some(_) => return Err(TelemetryConfigError::ServiceName),
        };
        let log_format = match config.log_format.as_deref() {
            None | Some("text") => LogFormat::Text,
            Some("json") => LogFormat::Json,
            Some(_) => return Err(TelemetryConfigError::LogFormat),
        };
        let otlp = config
            .otlp
            .as_ref()
            .map(OtlpSettings::from_config)
            .transpose()?
            .flatten();
        Ok(Self {
            service_name,
            log_format,
            otlp,
        })
    }

    /// The same settings without export, for commands that do not serve.
    pub fn local_only(&self) -> Self {
        Self {
            otlp: None,
            ..self.clone()
        }
    }
}

impl OtlpSettings {
    pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
    pub const DEFAULT_MAX_RETRIES: u32 = 3;
    pub const DEFAULT_QUEUE_SIZE: usize = 2_048;
    pub const DEFAULT_BATCH_SIZE: usize = 512;
    pub const DEFAULT_FLUSH_INTERVAL_MS: u64 = 5_000;
    pub const DEFAULT_METRICS_INTERVAL_MS: u64 = 60_000;
    pub const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 5_000;

    /// `Ok(None)` for a block with `enabled false`. Everything in the block is
    /// validated either way, so enabling it later cannot surface old mistakes.
    fn from_config(config: &OtlpConfig) -> Result<Option<Self>, TelemetryConfigError> {
        let enabled = config.enabled.unwrap_or(true);
        let endpoint = config
            .endpoint
            .as_deref()
            .map(OtlpEndpoint::parse)
            .transpose()?;
        let signals = Signals {
            traces: config.traces.unwrap_or(true),
            metrics: config.metrics.unwrap_or(true),
            logs: config.logs.unwrap_or(true),
        };
        let log_level = match config.log_level.as_deref().unwrap_or("info") {
            "error" => LevelFilter::ERROR,
            "warn" => LevelFilter::WARN,
            "info" => LevelFilter::INFO,
            "debug" => LevelFilter::DEBUG,
            "trace" => LevelFilter::TRACE,
            _ => return Err(TelemetryConfigError::LogLevel),
        };
        let sample_ratio = config.sample_ratio.unwrap_or(1.0);
        if !(0.0..=1.0).contains(&sample_ratio) {
            return Err(out_of_range("otlp.sample-ratio", 0.0, 1.0));
        }
        let timeout = millis(
            "otlp.timeout-ms",
            config.timeout_ms,
            Self::DEFAULT_TIMEOUT_MS,
            50,
            60_000,
        )?;
        let max_retries = config.max_retries.unwrap_or(Self::DEFAULT_MAX_RETRIES);
        if max_retries > 10 {
            return Err(out_of_range("otlp.max-retries", 0, 10));
        }
        let capacity = config.queue_size.unwrap_or(Self::DEFAULT_QUEUE_SIZE);
        if !(1..=65_536).contains(&capacity) {
            return Err(out_of_range("otlp.queue-size", 1, 65_536));
        }
        let batch_size = config
            .batch_size
            .unwrap_or(Self::DEFAULT_BATCH_SIZE.min(capacity));
        if !(1..=8_192).contains(&batch_size) {
            return Err(out_of_range("otlp.batch-size", 1, 8_192));
        }
        if batch_size > capacity {
            return Err(TelemetryConfigError::BatchLargerThanQueue);
        }
        let flush_interval = millis(
            "otlp.flush-interval-ms",
            config.flush_interval_ms,
            Self::DEFAULT_FLUSH_INTERVAL_MS,
            10,
            60_000,
        )?;
        let metrics_interval = millis(
            "otlp.metrics-interval-ms",
            config.metrics_interval_ms,
            Self::DEFAULT_METRICS_INTERVAL_MS,
            100,
            3_600_000,
        )?;
        let shutdown_timeout = millis(
            "otlp.shutdown-timeout-ms",
            config.shutdown_timeout_ms,
            Self::DEFAULT_SHUTDOWN_TIMEOUT_MS,
            100,
            // With the 10 s request drain, stays inside SMF's 60 s stop timeout.
            30_000,
        )?;

        if !enabled {
            return Ok(None);
        }
        let endpoint = endpoint.ok_or(TelemetryConfigError::MissingEndpoint)?;
        if !(signals.traces || signals.metrics || signals.logs) {
            return Err(TelemetryConfigError::NoSignals);
        }
        Ok(Some(Self {
            endpoint,
            signals,
            log_level,
            sample_ratio,
            export: ExportPolicy {
                timeout,
                max_retries,
                initial_backoff: Duration::from_millis(100),
                max_backoff: Duration::from_secs(5),
            },
            queue: QueuePolicy {
                capacity,
                batch_size,
                flush_interval,
            },
            metrics_interval,
            shutdown_timeout,
        }))
    }
}

fn out_of_range(key: &'static str, min: impl ToString, max: impl ToString) -> TelemetryConfigError {
    TelemetryConfigError::OutOfRange {
        key,
        min: min.to_string(),
        max: max.to_string(),
    }
}

fn millis(
    key: &'static str,
    value: Option<u64>,
    default: u64,
    min: u64,
    max: u64,
) -> Result<Duration, TelemetryConfigError> {
    let value = value.unwrap_or(default);
    if (min..=max).contains(&value) {
        Ok(Duration::from_millis(value))
    } else {
        Err(out_of_range(key, min, max))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn otlp(endpoint: &str) -> OtlpConfig {
        OtlpConfig {
            endpoint: Some(endpoint.to_string()),
            ..Default::default()
        }
    }

    fn settings(otlp: OtlpConfig) -> Result<TelemetrySettings, TelemetryConfigError> {
        TelemetrySettings::from_config(Some(&TelemetryConfig {
            otlp: Some(otlp),
            ..Default::default()
        }))
    }

    #[test]
    fn absent_block_means_local_logging_only() {
        let s = TelemetrySettings::from_config(None).unwrap();
        assert_eq!(s, TelemetrySettings::default());
        assert!(s.otlp.is_none());
    }

    #[test]
    fn block_defaults_to_enabled_with_all_signals() {
        let s = settings(otlp("http://127.0.0.1:4318")).unwrap();
        let o = s.otlp.expect("enabled");
        assert_eq!(
            o.signals,
            Signals {
                traces: true,
                metrics: true,
                logs: true
            }
        );
        assert_eq!(
            o.endpoint.signal_url("/v1/traces"),
            "http://127.0.0.1:4318/v1/traces"
        );
        assert_eq!(o.queue.capacity, OtlpSettings::DEFAULT_QUEUE_SIZE);
    }

    #[test]
    fn path_prefix_is_kept_and_display_hides_it() {
        let e = OtlpEndpoint::parse("https://otel.example:443/tenant-token/").unwrap();
        assert_eq!(
            e.signal_url("/v1/logs"),
            "https://otel.example/tenant-token/v1/logs"
        );
        assert_eq!(e.to_string(), "https://otel.example:443");
        assert!(!format!("{e:?}").contains("tenant-token"));
    }

    #[test]
    fn disabled_block_needs_no_endpoint_but_is_still_validated() {
        let off = OtlpConfig {
            enabled: Some(false),
            ..Default::default()
        };
        assert!(settings(off.clone()).unwrap().otlp.is_none());

        let bad = OtlpConfig {
            queue_size: Some(0),
            ..off
        };
        assert!(matches!(
            settings(bad),
            Err(TelemetryConfigError::OutOfRange { .. })
        ));
    }

    #[test]
    fn rejected_endpoints_never_echo_the_value() {
        let cases = [
            ("not a url s3cr3t", EndpointProblem::NotAUrl),
            ("ftp://s3cr3t.example/", EndpointProblem::Scheme),
            ("http://user:s3cr3t@host:4318", EndpointProblem::Credentials),
            ("http://s3cr3t@host:4318", EndpointProblem::Credentials),
            (
                "http://host:4318/?token=s3cr3t",
                EndpointProblem::QueryOrFragment,
            ),
            ("http://host:4318/#s3cr3t", EndpointProblem::QueryOrFragment),
        ];
        for (raw, problem) in cases {
            let err = settings(otlp(raw)).unwrap_err();
            assert_eq!(err, TelemetryConfigError::Endpoint(problem), "{raw}");
            let shown = format!("{err} {err:?}");
            assert!(!shown.contains("s3cr3t"), "{shown}");
        }
    }

    #[test]
    fn enabled_without_endpoint_or_signals_fails() {
        assert_eq!(
            settings(OtlpConfig::default()).unwrap_err(),
            TelemetryConfigError::MissingEndpoint
        );
        let none = OtlpConfig {
            traces: Some(false),
            metrics: Some(false),
            logs: Some(false),
            ..otlp("http://127.0.0.1:4318")
        };
        assert_eq!(settings(none).unwrap_err(), TelemetryConfigError::NoSignals);
    }

    #[test]
    fn ranges_and_enums_are_enforced() {
        let base = otlp("http://127.0.0.1:4318");
        let bad = [
            OtlpConfig {
                timeout_ms: Some(0),
                ..base.clone()
            },
            OtlpConfig {
                max_retries: Some(11),
                ..base.clone()
            },
            OtlpConfig {
                sample_ratio: Some(1.5),
                ..base.clone()
            },
            OtlpConfig {
                metrics_interval_ms: Some(1),
                ..base.clone()
            },
            OtlpConfig {
                shutdown_timeout_ms: Some(600_000),
                ..base.clone()
            },
        ];
        for otlp in bad {
            assert!(matches!(
                settings(otlp),
                Err(TelemetryConfigError::OutOfRange { .. })
            ));
        }
        let batch = OtlpConfig {
            queue_size: Some(4),
            batch_size: Some(8),
            ..base.clone()
        };
        assert_eq!(
            settings(batch).unwrap_err(),
            TelemetryConfigError::BatchLargerThanQueue
        );
        let level = OtlpConfig {
            log_level: Some("loud".into()),
            ..base
        };
        assert_eq!(settings(level).unwrap_err(), TelemetryConfigError::LogLevel);
    }

    #[test]
    fn legacy_key_log_format_and_service_name() {
        let legacy = TelemetryConfig {
            otlp_endpoint: Some("http://host:4318".into()),
            ..Default::default()
        };
        assert_eq!(
            TelemetrySettings::from_config(Some(&legacy)).unwrap_err(),
            TelemetryConfigError::LegacyEndpoint
        );
        let json = TelemetryConfig {
            log_format: Some("json".into()),
            ..Default::default()
        };
        assert_eq!(
            TelemetrySettings::from_config(Some(&json))
                .unwrap()
                .log_format,
            LogFormat::Json
        );
        let xml = TelemetryConfig {
            log_format: Some("xml".into()),
            ..Default::default()
        };
        assert_eq!(
            TelemetrySettings::from_config(Some(&xml)).unwrap_err(),
            TelemetryConfigError::LogFormat
        );
        let empty = TelemetryConfig {
            service_name: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(
            TelemetrySettings::from_config(Some(&empty)).unwrap_err(),
            TelemetryConfigError::ServiceName
        );
    }
}
