use crate::errors::DepotError;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, knuffel::Decode, Clone)]
pub struct Config {
    #[knuffel(child)]
    pub server: ServerConfig,
    #[knuffel(child)]
    pub repository: RepositoryConfig,
    #[knuffel(child)]
    pub telemetry: Option<TelemetryConfig>,
    #[knuffel(child)]
    pub publishers: Option<PublishersConfig>,
    #[knuffel(child)]
    pub admin: Option<AdminConfig>,
    #[knuffel(child)]
    pub oauth2: Option<Oauth2Config>,
}

#[derive(Debug, knuffel::Decode, Clone)]
pub struct ServerConfig {
    #[knuffel(child, unwrap(arguments))]
    pub bind: Vec<String>,
    #[knuffel(child, unwrap(argument))]
    pub workers: Option<usize>,
    #[knuffel(child, unwrap(argument))]
    pub max_connections: Option<usize>,
    #[knuffel(child, unwrap(argument))]
    pub reuseport: Option<bool>,
    /// Default max-age for Cache-Control headers (seconds)
    #[knuffel(child, unwrap(argument))]
    pub cache_max_age: Option<u64>,
    #[knuffel(child, unwrap(argument))]
    pub tls_cert: Option<PathBuf>,
    #[knuffel(child, unwrap(argument))]
    pub tls_key: Option<PathBuf>,
}

#[derive(Debug, knuffel::Decode, Clone)]
pub struct RepositoryConfig {
    #[knuffel(child, unwrap(argument))]
    pub root: PathBuf,
    #[knuffel(child, unwrap(argument))]
    pub mode: Option<String>,
}

/// `telemetry { ... }`. Raw values; `telemetry::TelemetrySettings` validates
/// them when the configuration is loaded.
#[derive(Debug, knuffel::Decode, Clone, Default)]
pub struct TelemetryConfig {
    /// Never implemented and ambiguous about enablement. Kept in the schema
    /// only so that loading rejects it with a pointer to `otlp { endpoint }`.
    #[knuffel(child, unwrap(argument))]
    pub otlp_endpoint: Option<String>,
    #[knuffel(child, unwrap(argument))]
    pub service_name: Option<String>,
    /// `text` (default) or `json` for the local log output.
    #[knuffel(child, unwrap(argument))]
    pub log_format: Option<String>,
    #[knuffel(child)]
    pub otlp: Option<OtlpConfig>,
}

/// `telemetry { otlp { ... } }`: OTLP/HTTP export of traces, metrics and logs.
#[derive(Debug, knuffel::Decode, Clone, Default)]
pub struct OtlpConfig {
    /// Defaults to true when the block is present.
    #[knuffel(child, unwrap(argument))]
    pub enabled: Option<bool>,
    /// Collector base URL, e.g. `http://127.0.0.1:4318`. The signal paths
    /// `/v1/traces`, `/v1/metrics` and `/v1/logs` are appended.
    #[knuffel(child, unwrap(argument))]
    pub endpoint: Option<String>,
    #[knuffel(child, unwrap(argument))]
    pub traces: Option<bool>,
    #[knuffel(child, unwrap(argument))]
    pub metrics: Option<bool>,
    #[knuffel(child, unwrap(argument))]
    pub logs: Option<bool>,
    /// Minimum level of exported log records: error, warn, info, debug, trace.
    #[knuffel(child, unwrap(argument))]
    pub log_level: Option<String>,
    /// Fraction of HTTP requests traced, 0.0 to 1.0.
    #[knuffel(child, unwrap(argument))]
    pub sample_ratio: Option<f64>,
    /// Per-attempt export timeout.
    #[knuffel(child, unwrap(argument))]
    pub timeout_ms: Option<u64>,
    /// Retries after a failed attempt; each batch gets at most 1 + this.
    #[knuffel(child, unwrap(argument))]
    pub max_retries: Option<u32>,
    /// Records buffered per signal; the oldest is dropped when full.
    #[knuffel(child, unwrap(argument))]
    pub queue_size: Option<usize>,
    /// Records sent per export request.
    #[knuffel(child, unwrap(argument))]
    pub batch_size: Option<usize>,
    /// Longest time a trace or log record waits before it is sent.
    #[knuffel(child, unwrap(argument))]
    pub flush_interval_ms: Option<u64>,
    #[knuffel(child, unwrap(argument))]
    pub metrics_interval_ms: Option<u64>,
    /// Upper bound on flushing telemetry at shutdown.
    #[knuffel(child, unwrap(argument))]
    pub shutdown_timeout_ms: Option<u64>,
}

#[derive(Debug, knuffel::Decode, Clone)]
pub struct PublishersConfig {
    #[knuffel(child, unwrap(arguments))]
    pub list: Vec<String>,
}

#[derive(Debug, knuffel::Decode, Clone)]
pub struct AdminConfig {
    #[knuffel(child, unwrap(argument))]
    pub unix_socket: Option<PathBuf>,
    /// If true, require Authorization on /admin/health as well
    #[knuffel(child, unwrap(argument))]
    pub require_auth_for_health: Option<bool>,
}

#[derive(Debug, knuffel::Decode, Clone)]
pub struct Oauth2Config {
    #[knuffel(child, unwrap(argument))]
    pub issuer: Option<String>,
    #[knuffel(child, unwrap(argument))]
    pub jwks_uri: Option<String>,
    #[knuffel(child, unwrap(argument))]
    pub audience: Option<String>,
    #[knuffel(child, unwrap(arguments))]
    pub required_scopes: Option<Vec<String>>,
}

impl Config {
    pub fn load(path: Option<PathBuf>) -> crate::errors::Result<Self> {
        let path = path.unwrap_or_else(|| PathBuf::from("pkg6depotd.kdl"));

        let content = fs::read_to_string(&path).map_err(|e| {
            DepotError::Config(format!("Failed to read config file {:?}: {}", path, e))
        })?;

        knuffel::parse(path.to_str().unwrap_or("pkg6depotd.kdl"), &content)
            .map_err(|e| DepotError::Config(parse_error_summary(&e, &content)))
    }
}

/// Describe a KDL error by position and message only.
///
/// `knuffel::Error`'s `Debug` output embeds the whole file as miette source
/// code, which would copy credentials into the SMF log on any typo.
fn parse_error_summary(error: &knuffel::Error, content: &str) -> String {
    use miette5::Diagnostic;

    let mut problems = Vec::new();
    if let Some(related) = error.related() {
        for diag in related {
            let at = diag
                .labels()
                .and_then(|mut labels| labels.next())
                .map(|label| {
                    let (line, column) = line_column(content, label.offset());
                    format!("line {line}, column {column}: ")
                })
                .unwrap_or_default();
            problems.push(format!("{at}{diag}"));
        }
    }
    if problems.is_empty() {
        format!("Failed to parse config: {error}")
    } else {
        format!("Failed to parse config: {}", problems.join("; "))
    }
}

fn line_column(content: &str, offset: usize) -> (usize, usize) {
    let before = content.get(..offset).unwrap_or(content);
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}
