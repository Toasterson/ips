pub mod cli;
pub mod config;
pub mod daemon;
pub mod errors;
pub mod http;
pub mod repo;
pub mod telemetry;

use clap::Parser;
use cli::{Cli, Commands};
use config::Config;
use miette::Result;
use repo::DepotRepo;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

/// Load and validate the configuration.
///
/// There is deliberately no fallback: a missing, unreadable or invalid file is
/// an error. Falling back to built-in defaults would make a misconfigured
/// service listen on every interface and serve whatever sits in a temporary
/// directory. `None` reads `pkg6depotd.kdl` from the working directory.
pub fn load_config(path: Option<PathBuf>) -> Result<Config> {
    let shown = path
        .clone()
        .unwrap_or_else(|| PathBuf::from("pkg6depotd.kdl"));
    let config = Config::load(path).map_err(|e| {
        miette::miette!(
            code = "ips::depot_error::config",
            help = "Pass a readable KDL file with --config; pkg6depotd does not start without one.",
            "cannot load configuration {}: {}",
            shown.display(),
            e
        )
    })?;
    bind_addr(&config)?;
    telemetry_settings(&config)?;
    Ok(config)
}

/// Validated telemetry settings. Invalid settings are an error, never a
/// silent fallback to "no export"; messages do not repeat the endpoint.
pub fn telemetry_settings(config: &Config) -> Result<telemetry::TelemetrySettings> {
    telemetry::TelemetrySettings::from_config(config.telemetry.as_ref()).map_err(|e| {
        miette::Report::new(e).wrap_err("cannot load configuration: invalid telemetry settings")
    })
}

/// The single socket address the depot listens on.
///
/// Exactly one `bind` address is supported. An empty list or extra addresses
/// are rejected instead of being defaulted or silently ignored.
pub fn bind_addr(config: &Config) -> Result<SocketAddr> {
    match config.server.bind.as_slice() {
        [one] => one.parse().map_err(|e| {
            miette::miette!(
                code = "ips::depot_error::config",
                help = "Use an IP address and port, for example \"127.0.0.1:8080\".",
                "invalid server bind address {:?}: {}",
                one,
                e
            )
        }),
        [] => Err(miette::miette!(
            code = "ips::depot_error::config",
            help = "Add a bind address to the server block, for example bind \"127.0.0.1:8080\".",
            "server bind address is missing"
        )),
        many => Err(miette::miette!(
            code = "ips::depot_error::config",
            help = "Keep exactly one address in server.bind.",
            "only one server bind address is supported, got {}",
            many.len()
        )),
    }
}

pub async fn run() -> Result<()> {
    let args = Cli::parse();

    let config = load_config(args.config.clone())?;
    let settings = telemetry_settings(&config)?;

    match args.command.unwrap_or(Commands::Start) {
        Commands::Start => {
            if !args.no_daemon {
                daemon::daemonize().map_err(|e| miette::miette!(e))?;
            }
            // After daemonizing: OTLP export runs on its own threads.
            let telemetry = telemetry::init(&settings)?;

            let served = serve(&config).await;
            match &served {
                Ok(()) => tracing::info!("pkg6depotd stopped"),
                Err(e) => tracing::error!("pkg6depotd failed: {e}"),
            }
            // Flush after the last request and the final log line.
            let _ = tokio::task::spawn_blocking(move || telemetry.shutdown()).await;
            served?;
        }
        Commands::ConfigTest => {
            let _telemetry = telemetry::init(&settings.local_only())?;
            // Validates the file only; the repository may not exist yet.
            // Print a fixed summary, never the whole configuration: it can
            // hold credentials (OTLP endpoints, OAuth settings, later TLS
            // and ACME secrets).
            let path = args
                .config
                .unwrap_or_else(|| PathBuf::from("pkg6depotd.kdl"));
            println!("Configuration loaded successfully: {}", path.display());
            println!("  bind: {}", bind_addr(&config)?);
            println!(
                "  repository: {} (mode {})",
                config.repository.root.display(),
                config.repository.mode.as_deref().unwrap_or("readonly")
            );
        }
        _ => {
            println!("Command not yet implemented");
        }
    }

    Ok(())
}

/// Open the repository and serve it until SIGTERM/SIGINT.
async fn serve(config: &Config) -> Result<()> {
    let repo = DepotRepo::new(config).map_err(|e| miette::miette!(e))?;
    let state = Arc::new(repo);

    // Initialize OIDC auth if configured
    let auth_state = http::middleware::AuthState::from_config(&config.oauth2)
        .await
        .map(Arc::new);

    let router = http::routes::app_router_with_auth(state, auth_state);
    let addr = bind_addr(config)?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(crate::errors::DepotError::Io)?;

    tracing::info!("Starting pkg6depotd on {}", addr);

    http::server::run_until(router, listener, http::server::shutdown_signal())
        .await
        .map_err(|e| miette::miette!(e))
}
