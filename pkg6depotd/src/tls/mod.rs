//! Direct HTTPS for pkg6depotd.
//!
//! Without a `tls` block the depot serves plain HTTP and expects a reverse
//! proxy in front of it. With one, it terminates TLS itself, using either
//! operator-provided files or certificates obtained and renewed through
//! ACME (http-01 or dns-01 with Cloudflare or Gandi).
//!
//! The rules are fail-closed: an invalid `tls` block, an unreadable or
//! mismatched certificate, or an ACME setup that cannot produce a
//! certificate at startup stops the depot. It never falls back to plain
//! HTTP. Once running, a failed reload or renewal keeps the current
//! certificate.

pub mod acme;
pub mod cert;
pub mod der;
pub mod error;
pub mod listener;
pub mod plan;

pub use cert::{CertSource, CertStore, LoadedCert};
pub use error::{AcmeError, TlsError};
pub use listener::TlsListener;
pub use plan::{TlsPlan, plan};

use acme::{AcmeManager, Solver, Timing};
use plan::ChallengePlan;
use std::sync::Arc;

/// Start the certificate machinery for `plan` and return the store the TLS
/// listener serves from. `https_port` is where http-01 redirects plain HTTP
/// requests to. Background tasks (http-01 listener, renewal) are spawned on
/// the current runtime.
pub async fn start(plan: &TlsPlan, https_port: u16) -> Result<Arc<CertStore>, TlsError> {
    let store = match plan {
        TlsPlan::Files {
            certificate, key, ..
        } => {
            let cert = LoadedCert::from_files_valid_at(certificate, key, acme::unix_now())?;
            CertStore::new(
                cert,
                CertSource::Files {
                    certificate: certificate.clone(),
                    key: key.clone(),
                },
            )
        }
        TlsPlan::Acme { settings, .. } => {
            let timing = Timing::default();
            let solver = match &settings.challenge {
                ChallengePlan::Http01 { bind } => {
                    let listener = tokio::net::TcpListener::bind(bind).await.map_err(|e| {
                        TlsError::io(format!("cannot bind http-01 listener {bind}"), e)
                    })?;
                    let tokens = acme::http01::Http01Tokens::default();
                    let domains = settings.domains.iter().map(|d| d.to_string()).collect();
                    acme::http01::spawn(listener, tokens.clone(), domains, https_port);
                    tracing::info!("http-01 challenge listener on {bind}");
                    Solver::Http01(tokens)
                }
                ChallengePlan::Dns01(dns) => Solver::Dns01(acme::dns::DnsProvider::from_plan(
                    dns,
                    acme::default_http_client(&timing)?,
                )?),
            };
            let manager = Arc::new(AcmeManager::new(settings.clone(), solver, timing)?);
            // A damaged cleanup journal is fatal: it may list live records.
            manager.pending_challenges()?;
            // dns-01 records of an issuance interrupted by a stop or crash.
            // Not fatal: they stay journaled and are retried.
            if let Err(e) = manager.recover_challenges().await {
                tracing::warn!("{e}");
            }
            let cert = match manager.load_current() {
                Some(cert) if AcmeManager::usable(&cert, acme::unix_now()) => cert,
                _ => manager.obtain_with_retries().await?,
            };
            let store = CertStore::new(cert, CertSource::Bundle(manager.bundle_path()));
            tokio::spawn(manager.renewal_loop(store.clone()));
            store
        }
    };
    Ok(store)
}

/// The certificate store SIGHUP reloads, once there is one.
#[derive(Debug, Clone, Default)]
pub struct HangupTarget(Arc<std::sync::OnceLock<Arc<CertStore>>>);

impl HangupTarget {
    /// Reload `store` on later signals. Only the first call has an effect.
    pub fn set(&self, store: Arc<CertStore>) {
        let _ = self.0.set(store);
    }
}

/// Handle SIGHUP (SMF `refresh`): re-read the certificate once TLS is
/// serving. A bad file is logged and the current certificate stays in
/// service. Installed before ACME issuance, which can wait for backoff for
/// hours, so a refresh during startup is only logged; without a handler it
/// would terminate the depot. In plain-HTTP mode the signal is only logged.
pub fn spawn_hangup_handler() -> Result<HangupTarget, TlsError> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut hup = signal(SignalKind::hangup())
        .map_err(|e| TlsError::io("cannot install SIGHUP handler", e))?;
    let target = HangupTarget::default();
    let store = target.0.clone();
    tokio::spawn(async move {
        while hup.recv().await.is_some() {
            match store.get() {
                Some(store) => match store.reload() {
                    Ok(()) => tracing::info!("TLS certificate reloaded on SIGHUP"),
                    Err(e) => {
                        tracing::error!("TLS reload failed, keeping the current certificate: {e}")
                    }
                },
                None => tracing::info!(
                    "SIGHUP: no certificate is being served; configuration changes need a restart"
                ),
            }
        }
    });
    Ok(target)
}

/// Checks `config-test` runs beyond structural validation: certificate
/// files parse and match, secrets are private, the state directory is
/// private. Returns summary lines without secrets.
pub fn preflight(plan: Option<&TlsPlan>) -> Result<Vec<String>, TlsError> {
    let Some(plan) = plan else {
        return Ok(vec![
            "tls: off (plain HTTP; terminate TLS in a reverse proxy)".into(),
        ]);
    };
    match plan {
        TlsPlan::Files {
            certificate, key, ..
        } => {
            let cert = LoadedCert::from_files_valid_at(certificate, key, acme::unix_now())?;
            let expiry = chrono::DateTime::from_timestamp(cert.not_after(), 0)
                .map(|t| t.to_rfc3339())
                .unwrap_or_default();
            Ok(vec![
                format!("tls: files {}", certificate.display()),
                format!(
                    "  names: {}, valid until {expiry}",
                    cert.summary.dns_names.join(", ")
                ),
            ])
        }
        TlsPlan::Acme { settings, .. } => {
            if let ChallengePlan::Dns01(dns) = &settings.challenge {
                acme::dns::read_secret_file(&dns.api_token_file)?;
            }
            if settings.state_dir.exists() {
                acme::StateDir::check(&settings.state_dir)?;
            }
            let challenge = match &settings.challenge {
                ChallengePlan::Http01 { bind } => format!("http-01 on {bind}"),
                ChallengePlan::Dns01(dns) => {
                    format!("dns-01 via {} in zone {}", dns.provider.name(), dns.zone)
                }
            };
            let domains: Vec<&str> = settings.domains.iter().map(|d| d.as_str()).collect();
            Ok(vec![
                format!("tls: acme {}", domains.join(", ")),
                format!("  challenge: {challenge}"),
                format!("  directory: {}", settings.directory_url),
                format!("  state: {}", settings.state_dir.display()),
            ])
        }
    }
}
