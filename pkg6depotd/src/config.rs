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
    /// Direct HTTPS. Without this block the depot speaks plain HTTP and is
    /// meant to sit behind a TLS-terminating reverse proxy.
    #[knuffel(child)]
    pub tls: Option<TlsConfig>,
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
    /// Never implemented; rejected at load time in favour of the `tls` block
    /// so an old configuration cannot silently serve plain HTTP.
    #[knuffel(child, unwrap(argument))]
    pub tls_cert: Option<PathBuf>,
    #[knuffel(child, unwrap(argument))]
    pub tls_key: Option<PathBuf>,
}

/// `tls { ... }`: either `certificate` and `key` files, or an `acme` block.
#[derive(Debug, knuffel::Decode, Clone)]
pub struct TlsConfig {
    #[knuffel(child, unwrap(argument))]
    pub certificate: Option<PathBuf>,
    #[knuffel(child, unwrap(argument))]
    pub key: Option<PathBuf>,
    /// Seconds a client may take to complete the TLS handshake.
    #[knuffel(child, unwrap(argument))]
    pub handshake_timeout: Option<u64>,
    #[knuffel(child)]
    pub acme: Option<AcmeConfig>,
}

#[derive(Debug, knuffel::Decode, Clone)]
pub struct AcmeConfig {
    #[knuffel(child, unwrap(arguments))]
    pub domains: Option<Vec<String>>,
    #[knuffel(child, unwrap(arguments))]
    pub contact: Option<Vec<String>>,
    /// Must be `true`: the operator agrees to the CA's terms of service.
    #[knuffel(child, unwrap(argument))]
    pub terms_agreed: Option<bool>,
    /// Use the Let's Encrypt staging directory.
    #[knuffel(child, unwrap(argument))]
    pub staging: Option<bool>,
    #[knuffel(child, unwrap(argument))]
    pub directory_url: Option<String>,
    #[knuffel(child, unwrap(argument))]
    pub state_dir: Option<PathBuf>,
    #[knuffel(child, unwrap(argument))]
    pub renew_before_days: Option<u64>,
    /// `http-01` or `dns-01`.
    #[knuffel(child, unwrap(argument))]
    pub challenge: Option<String>,
    #[knuffel(child, unwrap(argument))]
    pub http01_bind: Option<String>,
    #[knuffel(child)]
    pub dns01: Option<Dns01Config>,
}

#[derive(Debug, knuffel::Decode, Clone)]
pub struct Dns01Config {
    /// `cloudflare` or `gandi`.
    #[knuffel(child, unwrap(argument))]
    pub provider: Option<String>,
    /// File holding the provider API token. Never put the token itself in
    /// this configuration.
    #[knuffel(child, unwrap(argument))]
    pub api_token_file: Option<PathBuf>,
    #[knuffel(child, unwrap(argument))]
    pub zone: Option<String>,
    /// Cloudflare only: skips the zone lookup.
    #[knuffel(child, unwrap(argument))]
    pub zone_id: Option<String>,
    #[knuffel(child, unwrap(argument))]
    pub propagation_seconds: Option<u64>,
    #[knuffel(child, unwrap(argument))]
    pub ttl: Option<u32>,
}

#[derive(Debug, knuffel::Decode, Clone)]
pub struct RepositoryConfig {
    #[knuffel(child, unwrap(argument))]
    pub root: PathBuf,
    #[knuffel(child, unwrap(argument))]
    pub mode: Option<String>,
}

#[derive(Debug, knuffel::Decode, Clone)]
pub struct TelemetryConfig {
    #[knuffel(child, unwrap(argument))]
    pub otlp_endpoint: Option<String>,
    #[knuffel(child, unwrap(argument))]
    pub service_name: Option<String>,
    #[knuffel(child, unwrap(argument))]
    pub log_format: Option<String>,
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
            .map_err(|e| DepotError::Config(format!("Failed to parse config: {:?}", e)))
    }
}
