//! ACME certificate issuance and renewal.
//!
//! State lives in the configured state directory (mode 0700, files 0600):
//!
//! - `account-<hash>.json`: account key and URL, one per ACME directory, so
//!   staging and production never share an account.
//! - `order.json`: the order in progress and its certificate key, so an
//!   order survives a restart instead of being opened again.
//! - `certificate.pem`: private key and certificate chain in one file. It is
//!   replaced with a single rename, so key and chain can never mismatch.
//! - `challenges.json`: dns-01 records that were, or may have been,
//!   published. Written before the provider is called and removed once the
//!   records are gone, so records of an issuance interrupted by a crash or
//!   a stop are removed on the next start.
//! - `backoff.json`: consecutive failed issuance attempts and the earliest
//!   time of the next one, so service restarts cannot hammer the CA.

pub mod client;
pub mod dns;
pub mod http01;
pub mod keys;

use crate::tls::cert::{LoadedCert, pem_encode, read_limited, read_private};
use crate::tls::error::{AcmeError, TlsError, truncate};
use crate::tls::plan::AcmeSettings;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use client::{AcmeClient, Authorization, Order};
use dns::{DnsProvider, Presented, TxtRecord};
use http01::Http01Tokens;
use keys::EcKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::tls::cert::CertStore;

/// Timeouts and retry bounds. Defaults suit a real CA; tests shrink them.
#[derive(Debug, Clone)]
pub struct Timing {
    /// Per HTTP request to the CA or DNS provider.
    pub request_timeout: Duration,
    /// Minimum delay between polls of an order or authorization.
    pub poll_interval: Duration,
    /// Polls before giving up on one order step.
    pub max_polls: u32,
    /// Longest sleep between renewal checks.
    pub check_interval: Duration,
    /// Delay after `startup_attempts` consecutive failures; doubles with
    /// every further failure.
    pub retry_base: Duration,
    /// Ceiling for any retry delay.
    pub retry_max: Duration,
    /// Issuance attempts per process start when no usable certificate
    /// exists, before the depot exits.
    pub startup_attempts: u32,
    /// Delay after each of the first `startup_attempts - 1` consecutive
    /// failures.
    pub startup_retry: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            request_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_secs(2),
            max_polls: 60,
            check_interval: Duration::from_secs(12 * 3600),
            retry_base: Duration::from_secs(5 * 60),
            retry_max: Duration::from_secs(6 * 3600),
            startup_attempts: 3,
            startup_retry: Duration::from_secs(10),
        }
    }
}

/// How challenges are answered.
#[derive(Debug, Clone)]
pub enum Solver {
    Http01(Http01Tokens),
    Dns01(DnsProvider),
}

impl Solver {
    fn challenge_type(&self) -> &'static str {
        match self {
            Solver::Http01(_) => "http-01",
            Solver::Dns01(_) => "dns-01",
        }
    }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// State directory

#[derive(Debug, Serialize, Deserialize)]
struct AccountFile {
    directory: String,
    /// Account URL; absent until registration succeeded.
    kid: Option<String>,
    /// PKCS#8, base64url.
    key: String,
    contact: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OrderFile {
    directory: String,
    url: String,
    domains: Vec<String>,
    /// Certificate key, PKCS#8, base64url.
    key: String,
    created: i64,
}

/// Orders expire at the CA after 7 days (Let's Encrypt); drop older state.
const ORDER_MAX_AGE: i64 = 7 * 86_400;

#[derive(Debug, Default, Serialize, Deserialize)]
struct ChallengeJournal {
    records: Vec<Presented>,
}

/// Consecutive failed issuance attempts for one directory and name set.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackoffFile {
    directory: String,
    domains: Vec<String>,
    failures: u32,
    /// Unix time before which the CA is not contacted again.
    not_before: i64,
}

#[derive(Debug, Clone)]
pub struct StateDir {
    root: PathBuf,
}

impl StateDir {
    /// Open or create the state directory. It must be a directory owned by
    /// the current user and inaccessible to anybody else.
    pub fn open(root: &Path) -> Result<Self, TlsError> {
        if !root.exists() {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(root)
                .map_err(|e| {
                    TlsError::io(
                        format!("cannot create ACME state directory {}", root.display()),
                        e,
                    )
                })?;
        }
        Self::check(root)?;
        Ok(StateDir {
            root: root.to_path_buf(),
        })
    }

    /// Validate an existing state directory without creating anything.
    pub fn check(root: &Path) -> Result<(), TlsError> {
        let meta = std::fs::symlink_metadata(root).map_err(|e| {
            TlsError::io(
                format!("cannot inspect ACME state directory {}", root.display()),
                e,
            )
        })?;
        if !meta.is_dir() {
            return Err(TlsError::Permissions {
                path: root.to_path_buf(),
                message: "not a directory (symbolic links are not followed)".into(),
            });
        }
        let mode = meta.permissions().mode() & 0o7777;
        if mode & 0o077 != 0 {
            return Err(TlsError::Permissions {
                path: root.to_path_buf(),
                message: format!(
                    "mode {mode:04o}; the directory holds private keys and must be 0700"
                ),
            });
        }
        let uid = nix::unistd::geteuid().as_raw();
        if meta.uid() != uid {
            return Err(TlsError::Permissions {
                path: root.to_path_buf(),
                message: format!(
                    "owned by uid {}, but the depot runs as uid {uid}",
                    meta.uid()
                ),
            });
        }
        Ok(())
    }

    pub fn bundle_path(&self) -> PathBuf {
        self.root.join("certificate.pem")
    }

    fn order_path(&self) -> PathBuf {
        self.root.join("order.json")
    }

    pub fn challenges_path(&self) -> PathBuf {
        self.root.join("challenges.json")
    }

    pub fn backoff_path(&self) -> PathBuf {
        self.root.join("backoff.json")
    }

    fn account_path(&self, directory: &str) -> PathBuf {
        let digest = Sha256::digest(directory.as_bytes());
        let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        self.root.join(format!("account-{hex}.json"))
    }

    /// Write `bytes` to `path` via a temporary file, fsync and rename, so
    /// readers see either the old or the new content, never a mix.
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), TlsError> {
        let state_err = |e: std::io::Error| AcmeError::State {
            path: path.to_path_buf(),
            message: e.to_string(),
        };
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
        let tmp = self
            .root
            .join(format!(".{name}.tmp.{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let result = (|| {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, path)?;
            std::fs::File::open(&self.root)?.sync_all()
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map_err(|e| state_err(e).into())
    }

    fn read_json<T: for<'de> Deserialize<'de>>(&self, path: &Path) -> Result<Option<T>, TlsError> {
        if !path.exists() {
            return Ok(None);
        }
        let bytes = read_limited(path)?;
        serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            AcmeError::State {
                path: path.to_path_buf(),
                message: format!("unreadable: {e}"),
            }
            .into()
        })
    }

    fn remove(&self, path: &Path) {
        if let Err(e) = std::fs::remove_file(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("cannot remove {}: {e}", path.display());
        }
    }
}

fn decode_key(b64: &str, path: &Path) -> Result<EcKey, TlsError> {
    let der = URL_SAFE_NO_PAD.decode(b64).map_err(|e| AcmeError::State {
        path: path.to_path_buf(),
        message: format!("key is not base64url: {e}"),
    })?;
    Ok(EcKey::from_pkcs8(&der)?)
}

// ---------------------------------------------------------------------------
// Manager

pub struct AcmeManager {
    settings: AcmeSettings,
    solver: Solver,
    timing: Timing,
    http: reqwest::Client,
    state: StateDir,
}

impl std::fmt::Debug for AcmeManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcmeManager")
            .field("domains", &self.settings.domains)
            .field("directory", &self.settings.directory_url)
            .field("challenge", &self.solver.challenge_type())
            .finish_non_exhaustive()
    }
}

impl AcmeManager {
    pub fn new(settings: AcmeSettings, solver: Solver, timing: Timing) -> Result<Self, TlsError> {
        let state = StateDir::open(&settings.state_dir)?;
        let http = http_client(&timing)?;
        Ok(AcmeManager {
            settings,
            solver,
            timing,
            http,
            state,
        })
    }

    pub fn timing(&self) -> &Timing {
        &self.timing
    }

    pub fn bundle_path(&self) -> PathBuf {
        self.state.bundle_path()
    }

    fn domains(&self) -> Vec<String> {
        self.settings
            .domains
            .iter()
            .map(|d| d.as_str().to_string())
            .collect()
    }

    /// The stored certificate, if one exists and parses. A damaged bundle is
    /// reported and treated as absent, which leads to a new issuance.
    pub fn load_current(&self) -> Option<LoadedCert> {
        let path = self.state.bundle_path();
        if !path.exists() {
            return None;
        }
        match read_private(&path).and_then(|pem| LoadedCert::from_pem(&pem, &pem, &path)) {
            Ok(cert) => Some(cert),
            Err(e) => {
                tracing::error!("ignoring stored certificate: {e}");
                None
            }
        }
    }

    /// When `cert` should be renewed: `renew-before` ahead of expiry, but at
    /// the latest when a third of its lifetime is left, so short-lived
    /// certificates are renewed in time too.
    pub fn renew_at(&self, cert: &LoadedCert) -> i64 {
        let lifetime = (cert.summary.not_after - cert.summary.not_before).max(0);
        let margin = (self.settings.renew_before.as_secs() as i64).min(lifetime / 3);
        cert.summary.not_after - margin
    }

    /// True if `cert` is due for renewal or does not cover the configured
    /// domains.
    pub fn needs_renewal(&self, cert: &LoadedCert, now: i64) -> bool {
        now >= self.renew_at(cert) || !cert.covers(self.domains().iter().map(String::as_str))
    }

    /// Usable means inside its validity window; coverage is handled by
    /// renewal.
    pub fn usable(cert: &LoadedCert, now: i64) -> bool {
        cert.check_validity(now).is_ok()
    }

    /// Obtain a certificate, making up to `timing.startup_attempts`
    /// attempts. Each attempt first waits out the persisted backoff, so a
    /// restarted depot continues the schedule instead of starting over.
    pub async fn obtain_with_retries(&self) -> Result<LoadedCert, TlsError> {
        let attempts = self.timing.startup_attempts.max(1);
        let mut last = None;
        for attempt in 1..=attempts {
            self.wait_for_backoff().await;
            match self.attempt().await {
                Ok(cert) => return Ok(cert),
                Err((e, retry_in)) => {
                    tracing::error!(
                        attempt,
                        attempts,
                        ?retry_in,
                        "certificate issuance failed: {e}"
                    );
                    last = Some(e);
                }
            }
        }
        Err(last.expect("at least one attempt"))
    }

    /// One issuance with backoff bookkeeping: success clears the persisted
    /// failures, failure records one and returns the delay before the next
    /// attempt.
    async fn attempt(&self) -> Result<LoadedCert, (TlsError, Duration)> {
        match self.obtain().await {
            Ok(cert) => {
                self.state.remove(&self.state.backoff_path());
                Ok(cert)
            }
            Err(e) => Err((e, self.record_failure())),
        }
    }

    /// Delay after `failures` consecutive failures: `startup_retry` for the
    /// first attempts of a start, then exponential from `retry_base`,
    /// never more than `retry_max`.
    pub fn backoff_delay(&self, failures: u32) -> Duration {
        let quick = self.timing.startup_attempts.max(1);
        let delay = if failures < quick {
            self.timing.startup_retry
        } else {
            let exp = (failures - quick).min(16);
            self.timing.retry_base.saturating_mul(1u32 << exp)
        };
        delay.min(self.timing.retry_max)
    }

    /// The persisted backoff, if it belongs to the current directory and
    /// names. A configuration change starts afresh.
    fn backoff(&self) -> Option<BackoffFile> {
        let path = self.state.backoff_path();
        let stored = match self.state.read_json::<BackoffFile>(&path) {
            Ok(b) => b?,
            Err(e) => {
                tracing::warn!("discarding backoff state: {e}");
                self.state.remove(&path);
                return None;
            }
        };
        if stored.directory != self.settings.directory_url || stored.domains != self.domains() {
            self.state.remove(&path);
            return None;
        }
        Some(stored)
    }

    fn record_failure(&self) -> Duration {
        let failures = self.backoff().map_or(0, |b| b.failures).saturating_add(1);
        let delay = self.backoff_delay(failures);
        let file = BackoffFile {
            directory: self.settings.directory_url.clone(),
            domains: self.domains(),
            failures,
            not_before: unix_now().saturating_add(delay.as_secs().max(1) as i64),
        };
        if let Err(e) = self.state.write_atomic(
            &self.state.backoff_path(),
            &serde_json::to_vec_pretty(&file).expect("serializable"),
        ) {
            tracing::warn!("cannot persist issuance backoff: {e}");
        }
        delay
    }

    /// Sleep until the persisted backoff allows the next attempt. The wait
    /// is capped at `retry_max`, so a clock step or a damaged file cannot
    /// stall issuance for longer.
    pub async fn wait_for_backoff(&self) {
        let Some(b) = self.backoff() else { return };
        let wait = b.not_before - unix_now();
        if wait <= 0 {
            return;
        }
        let wait = Duration::from_secs(wait as u64).min(self.timing.retry_max);
        tracing::warn!(
            failures = b.failures,
            ?wait,
            "previous certificate issuance attempts failed; waiting before contacting the CA again (remove {} to retry now)",
            self.state.backoff_path().display()
        );
        tokio::time::sleep(wait).await;
    }

    /// Run one complete issuance: account, order, challenges, finalize,
    /// download. The result is stored before it is returned.
    pub async fn obtain(&self) -> Result<LoadedCert, TlsError> {
        let domains = self.domains();
        tracing::info!(
            ?domains,
            directory = %self.settings.directory_url,
            challenge = self.solver.challenge_type(),
            "requesting certificate"
        );
        let account_key = self.account_key()?;
        let mut client = AcmeClient::connect(
            self.http.clone(),
            &self.settings.directory_url,
            account_key,
            self.timing.clone(),
        )
        .await?;
        self.register(&mut client).await?;
        // Records left by an interrupted run could otherwise pile up.
        if let Err(e) = self.recover_challenges().await {
            tracing::warn!("{e}");
        }

        let (order_url, cert_key, order) = match self.resume_order(&mut client, &domains).await {
            Some(resumed) => resumed,
            None => {
                let cert_key = EcKey::generate()?;
                let (url, order) = client.new_order(&domains).await?;
                check_order(&order, &domains)?;
                self.state.write_atomic(
                    &self.state.order_path(),
                    &serde_json::to_vec_pretty(&OrderFile {
                        directory: self.settings.directory_url.clone(),
                        url: url.clone(),
                        domains: domains.clone(),
                        key: URL_SAFE_NO_PAD.encode(cert_key.pkcs8()),
                        created: unix_now(),
                    })
                    .expect("serializable"),
                )?;
                (url, cert_key, order)
            }
        };

        let result = self
            .complete_order(&mut client, &order_url, order, &cert_key, &domains)
            .await;
        match &result {
            // Network trouble: keep the order to resume it next time.
            Err(TlsError::Acme(AcmeError::Http { .. })) => {}
            _ => self.state.remove(&self.state.order_path()),
        }
        result
    }

    fn account_key(&self) -> Result<EcKey, TlsError> {
        let path = self.state.account_path(&self.settings.directory_url);
        if let Some(account) = self.state.read_json::<AccountFile>(&path)? {
            if account.directory != self.settings.directory_url {
                return Err(AcmeError::State {
                    path,
                    message: "belongs to a different ACME directory".into(),
                }
                .into());
            }
            return decode_key(&account.key, &path);
        }
        // Persist the key before registering: a crash after registration
        // then still finds the same account.
        let key = EcKey::generate()?;
        self.state.write_atomic(
            &path,
            &serde_json::to_vec_pretty(&AccountFile {
                directory: self.settings.directory_url.clone(),
                kid: None,
                key: URL_SAFE_NO_PAD.encode(key.pkcs8()),
                contact: Vec::new(),
            })
            .expect("serializable"),
        )?;
        tracing::info!("generated new ACME account key");
        Ok(key)
    }

    /// newAccount is idempotent for a known key; it returns the existing
    /// account. Contact changes are pushed with an account update.
    async fn register(&self, client: &mut AcmeClient) -> Result<(), TlsError> {
        let path = self.state.account_path(&self.settings.directory_url);
        let mut account =
            self.state
                .read_json::<AccountFile>(&path)?
                .ok_or_else(|| AcmeError::State {
                    path: path.clone(),
                    message: "account file vanished".into(),
                })?;
        let kid = client.register(&self.settings.contact).await?;
        if account.kid.as_deref() != Some(kid.as_str()) {
            tracing::info!("ACME account {kid}");
        }
        if account.kid.is_some() && account.contact != self.settings.contact {
            client.update_contact(&self.settings.contact).await?;
        }
        if account.kid.as_deref() != Some(kid.as_str()) || account.contact != self.settings.contact
        {
            account.kid = Some(kid);
            account.contact = self.settings.contact.clone();
            self.state.write_atomic(
                &path,
                &serde_json::to_vec_pretty(&account).expect("serializable"),
            )?;
        }
        Ok(())
    }

    /// Pick up a stored order for the same directory and domains, if the CA
    /// still considers it live.
    async fn resume_order(
        &self,
        client: &mut AcmeClient,
        domains: &[String],
    ) -> Option<(String, EcKey, Order)> {
        let path = self.state.order_path();
        let stored = match self.state.read_json::<OrderFile>(&path) {
            Ok(Some(o)) => o,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!("discarding order state: {e}");
                self.state.remove(&path);
                return None;
            }
        };
        let fresh = unix_now() - stored.created < ORDER_MAX_AGE;
        if stored.directory != self.settings.directory_url || stored.domains != domains || !fresh {
            self.state.remove(&path);
            return None;
        }
        let key = match decode_key(&stored.key, &path) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!("discarding order state: {e}");
                self.state.remove(&path);
                return None;
            }
        };
        match client.fetch::<Order>(&stored.url, "order").await {
            Ok((order, _)) if order.status != "invalid" && check_order(&order, domains).is_ok() => {
                tracing::info!(status = %order.status, "resuming stored ACME order");
                Some((stored.url, key, order))
            }
            Ok(_) | Err(_) => {
                self.state.remove(&path);
                None
            }
        }
    }

    async fn complete_order(
        &self,
        client: &mut AcmeClient,
        order_url: &str,
        mut order: Order,
        cert_key: &EcKey,
        domains: &[String],
    ) -> Result<LoadedCert, TlsError> {
        // pending -> ready -> processing -> valid; a few steps at most.
        for _ in 0..8 {
            check_order(&order, domains)?;
            match order.status.as_str() {
                "pending" => {
                    self.authorize(client, &order).await?;
                    order = client
                        .poll(order_url, "order", |o: &Order| o.status != "pending")
                        .await?;
                }
                "ready" => {
                    let csr = cert_key.csr(domains)?;
                    order = client.finalize(&order.finalize, &csr).await?;
                }
                "processing" => {
                    order = client
                        .poll(order_url, "order", |o: &Order| o.status != "processing")
                        .await?;
                }
                "valid" => {
                    let url = order.certificate.clone().ok_or_else(|| {
                        AcmeError::Protocol("valid order without certificate URL".into())
                    })?;
                    let chain = client.download_certificate(&url).await?;
                    return self.install(&chain, cert_key);
                }
                "invalid" => {
                    let detail = order
                        .error
                        .map(|p| format!("{}: {}", p.kind, p.detail))
                        .unwrap_or_else(|| "order is invalid".into());
                    return Err(AcmeError::Protocol(detail).into());
                }
                other => {
                    return Err(
                        AcmeError::Protocol(format!("unknown order status {other:?}")).into(),
                    );
                }
            }
        }
        Err(AcmeError::Timeout {
            what: "order state changes".into(),
            attempts: 8,
        }
        .into())
    }

    /// Validate the downloaded chain against our key and store it.
    fn install(&self, chain_pem: &[u8], cert_key: &EcKey) -> Result<LoadedCert, TlsError> {
        let path = self.state.bundle_path();
        let mut bundle = cert_key.to_pem();
        bundle.push_str(
            std::str::from_utf8(chain_pem)
                .map_err(|_| AcmeError::Protocol("certificate chain is not PEM text".into()))?,
        );
        let cert = LoadedCert::from_pem(bundle.as_bytes(), bundle.as_bytes(), &path)?;
        let missing: Vec<String> = self
            .domains()
            .into_iter()
            .filter(|d| !cert.summary.dns_names.contains(d))
            .collect();
        if !missing.is_empty() {
            return Err(AcmeError::Protocol(format!(
                "issued certificate lacks {}",
                missing.join(", ")
            ))
            .into());
        }
        if !Self::usable(&cert, unix_now()) {
            return Err(AcmeError::Protocol("issued certificate is already expired".into()).into());
        }
        self.state.write_atomic(&path, bundle.as_bytes())?;
        tracing::info!(
            names = ?cert.summary.dns_names,
            not_after = cert.not_after(),
            "stored new certificate in {}",
            path.display()
        );
        Ok(cert)
    }

    async fn authorize(&self, client: &mut AcmeClient, order: &Order) -> Result<(), TlsError> {
        let mut http_tokens = Vec::new();
        let mut dns_presented = Vec::new();
        // Entries of earlier runs that could not be removed yet stay in the
        // journal next to ours. Read before anything is published, so a
        // damaged journal stops the attempt instead of being overwritten.
        let earlier = match &self.solver {
            Solver::Dns01(_) => self.read_journal()?,
            Solver::Http01(_) => Vec::new(),
        };
        let result = self
            .authorize_inner(
                client,
                order,
                &earlier,
                &mut http_tokens,
                &mut dns_presented,
            )
            .await;
        // Always clean up, also after failures.
        match &self.solver {
            Solver::Http01(tokens) => {
                for t in &http_tokens {
                    tokens.remove(t);
                }
            }
            Solver::Dns01(provider) => {
                let failed = provider.cleanup(&dns_presented).await;
                for (p, e) in &failed {
                    tracing::warn!("could not remove dns-01 record {p:?}, retrying later: {e}");
                }
                let mut left = earlier;
                left.extend(failed.into_iter().map(|(p, _)| p));
                if let Err(e) = self.write_journal(&left) {
                    tracing::warn!("{e}");
                }
            }
        }
        result
    }

    /// The journal of an earlier run. A damaged journal is an error, never
    /// empty: treating it as empty would overwrite it and forget records
    /// that may still be live at the provider.
    fn read_journal(&self) -> Result<Vec<Presented>, TlsError> {
        let path = self.state.challenges_path();
        match self.state.read_json::<ChallengeJournal>(&path) {
            Ok(j) => Ok(j.map(|j| j.records).unwrap_or_default()),
            Err(e) => Err(AcmeError::Journal {
                path,
                message: e.to_string(),
            }
            .into()),
        }
    }

    fn write_journal(&self, records: &[Presented]) -> Result<(), TlsError> {
        let path = self.state.challenges_path();
        if records.is_empty() {
            self.state.remove(&path);
            return Ok(());
        }
        let journal = ChallengeJournal {
            records: records.to_vec(),
        };
        self.state.write_atomic(
            &path,
            &serde_json::to_vec_pretty(&journal).expect("serializable"),
        )
    }

    /// dns-01 records still listed in the cleanup journal, e.g. after the
    /// depot was stopped during an issuance.
    pub fn pending_challenges(&self) -> Result<Vec<Presented>, TlsError> {
        self.read_journal()
    }

    /// Remove dns-01 records that an interrupted run left in the journal.
    /// Records that cannot be removed stay in the journal for next time.
    pub async fn recover_challenges(&self) -> Result<(), TlsError> {
        let pending = self.read_journal()?;
        if pending.is_empty() {
            return Ok(());
        }
        let Solver::Dns01(provider) = &self.solver else {
            return Err(AcmeError::Dns {
                provider: "journal",
                message: format!(
                    "{} dns-01 challenge record(s) from an earlier run are listed in {}, but dns-01 is not configured; remove them at the provider and delete the file",
                    pending.len(),
                    self.state.challenges_path().display()
                ),
            }
            .into());
        };
        tracing::info!(
            records = pending.len(),
            "removing dns-01 records left by an interrupted issuance"
        );
        let failed = provider.cleanup(&pending).await;
        let first = failed.first().map(|(_, e)| e.to_string());
        self.write_journal(&failed.into_iter().map(|(p, _)| p).collect::<Vec<_>>())?;
        match first {
            None => Ok(()),
            Some(e) => Err(AcmeError::Dns {
                provider: "journal",
                message: format!("left dns-01 records not removed yet: {e}"),
            }
            .into()),
        }
    }

    async fn authorize_inner(
        &self,
        client: &mut AcmeClient,
        order: &Order,
        earlier: &[Presented],
        http_tokens: &mut Vec<String>,
        dns_presented: &mut Vec<Presented>,
    ) -> Result<(), TlsError> {
        let domains = self.domains();
        let thumbprint = client.key().thumbprint();
        let kind = self.solver.challenge_type();
        let mut pending = Vec::new();
        let mut records = Vec::new();

        for authz_url in &order.authorizations {
            let (authz, _): (Authorization, _) = client.fetch(authz_url, "authorization").await?;
            let domain = authz_domain(&authz, &domains)?;
            match authz.status.as_str() {
                "valid" => continue,
                "pending" => {}
                other => {
                    return Err(AcmeError::Challenge {
                        domain,
                        detail: format!("authorization is {other}"),
                    }
                    .into());
                }
            }
            let challenge = authz
                .challenges
                .iter()
                .find(|c| c.kind == kind)
                .ok_or_else(|| AcmeError::Challenge {
                    domain: domain.clone(),
                    detail: format!("the CA offered no {kind} challenge"),
                })?;
            let token = challenge
                .token
                .as_deref()
                .filter(|t| {
                    !t.is_empty()
                        && t.len() <= 256
                        && t.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                })
                .ok_or_else(|| AcmeError::Protocol(format!("bad {kind} token for {domain}")))?;
            let key_authorization = format!("{token}.{thumbprint}");
            match &self.solver {
                Solver::Http01(tokens) => {
                    tokens.insert(token, &key_authorization);
                    http_tokens.push(token.to_string());
                }
                Solver::Dns01(_) => records.push(TxtRecord {
                    name: format!("_acme-challenge.{}", domain.trim_start_matches("*.")),
                    value: keys::dns01_value(&key_authorization),
                }),
            }
            pending.push((authz_url.clone(), domain, challenge.url.clone()));
        }

        if let Solver::Dns01(provider) = &self.solver
            && !records.is_empty()
        {
            // Journal first: whatever happens from here on, the records can
            // be found and removed again.
            *dns_presented = provider.prepare(&records).await?;
            let journal = |presented: &[Presented]| {
                let mut all = earlier.to_vec();
                all.extend_from_slice(presented);
                self.write_journal(&all)
            };
            if let Err(e) = journal(dns_presented) {
                dns_presented.clear();
                return Err(e);
            }
            let presented = provider.present(dns_presented).await;
            // Record ids learnt so far, also after a partial failure.
            if let Err(e) = journal(dns_presented) {
                tracing::warn!("{e}");
            }
            presented?;
            tracing::info!(
                records = records.len(),
                wait = ?provider.propagation(),
                "published dns-01 records, waiting for propagation"
            );
            tokio::time::sleep(provider.propagation()).await;
        }

        for (_, _, challenge_url) in &pending {
            client.respond(challenge_url).await?;
        }
        for (authz_url, domain, _) in &pending {
            let authz: Authorization = client
                .poll(authz_url, "authorization", |a: &Authorization| {
                    a.status != "pending" && a.status != "processing"
                })
                .await?;
            if authz.status != "valid" {
                let detail = authz
                    .challenges
                    .iter()
                    .find(|c| c.kind == kind)
                    .and_then(|c| c.error.clone())
                    .map(|p| format!("{}: {}", p.kind, p.detail))
                    .unwrap_or_else(|| format!("authorization is {}", authz.status));
                return Err(AcmeError::Challenge {
                    domain: domain.clone(),
                    detail: crate::tls::error::truncate(&detail, 400),
                }
                .into());
            }
        }
        Ok(())
    }

    /// Keep `store` renewed. Never returns. Failures share the persisted
    /// backoff with startup issuance.
    pub async fn renewal_loop(self: Arc<Self>, store: Arc<CertStore>) {
        loop {
            let cert = store.current();
            let now = unix_now();
            if !self.needs_renewal(&cert, now) {
                let until = (self.renew_at(&cert) - now).max(1) as u64;
                let wait = Duration::from_secs(until).min(self.timing.check_interval);
                tokio::time::sleep(wait).await;
                continue;
            }
            self.wait_for_backoff().await;
            match self.attempt().await {
                Ok(next) => store.swap(next),
                Err((e, delay)) => {
                    if Self::usable(&cert, unix_now()) {
                        tracing::error!(
                            retry_in = ?delay,
                            not_after = cert.not_after(),
                            "certificate renewal failed, still serving the current certificate: {e}"
                        );
                    } else {
                        tracing::error!(
                            retry_in = ?delay,
                            "certificate renewal failed and the served certificate has EXPIRED; clients will reject connections: {e}"
                        );
                    }
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

/// The order must be for exactly the requested names; a CA answering with
/// other identifiers or more authorizations is refused.
fn check_order(order: &Order, domains: &[String]) -> Result<(), AcmeError> {
    let mut got = Vec::with_capacity(order.identifiers.len());
    for id in &order.identifiers {
        if id.kind != "dns" {
            return Err(AcmeError::Protocol(format!(
                "order contains a {:?} identifier",
                truncate(&id.kind, 40)
            )));
        }
        got.push(id.value.to_ascii_lowercase());
    }
    got.sort();
    got.dedup();
    let mut want = domains.to_vec();
    want.sort();
    want.dedup();
    if got != want {
        return Err(AcmeError::Protocol(format!(
            "order identifiers {} differ from the requested {}",
            truncate(&got.join(", "), 400),
            want.join(", ")
        )));
    }
    if order.authorizations.len() > want.len() {
        return Err(AcmeError::Protocol(format!(
            "order has {} authorizations for {} names",
            order.authorizations.len(),
            want.len()
        )));
    }
    Ok(())
}

/// The requested name an authorization is for; anything else is refused
/// before a challenge is answered.
fn authz_domain(authz: &Authorization, domains: &[String]) -> Result<String, AcmeError> {
    let value = authz.identifier.value.to_ascii_lowercase();
    let domain = if authz.wildcard {
        format!("*.{value}")
    } else {
        value.clone()
    };
    if authz.identifier.kind != "dns" || value.starts_with("*.") || !domains.contains(&domain) {
        return Err(AcmeError::Protocol(format!(
            "authorization for {} was not requested",
            truncate(&domain, 260)
        )));
    }
    Ok(domain)
}

fn http_client(timing: &Timing) -> Result<reqwest::Client, TlsError> {
    reqwest::Client::builder()
        .use_rustls_tls()
        .https_only(false)
        .timeout(timing.request_timeout)
        .connect_timeout(timing.request_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("pkg6depotd/", env!("CARGO_PKG_VERSION"), " (ACME)"))
        .build()
        .map_err(|e| TlsError::config(format!("HTTP client: {e}"), "This is a bug."))
}

/// A reqwest client with the same settings the manager uses, for building a
/// [`DnsProvider`] outside the manager.
pub fn default_http_client(timing: &Timing) -> Result<reqwest::Client, TlsError> {
    http_client(timing)
}

/// Encode a DER certificate as PEM (used by tests building chains).
pub fn certificate_pem(der_bytes: &[u8]) -> String {
    pem_encode("CERTIFICATE", der_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn state_dir_is_created_private_and_checked() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("acme");
        let state = StateDir::open(&root).unwrap();
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);

        let file = root.join("x.json");
        state.write_atomic(&file, b"{}").unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        state.write_atomic(&file, b"[1]").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"[1]");
        // No temporary files left behind.
        let names: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1);

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(matches!(
            StateDir::open(&root),
            Err(TlsError::Permissions { .. })
        ));
    }

    #[test]
    fn account_files_are_per_directory() {
        let tmp = TempDir::new().unwrap();
        let state = StateDir::open(&tmp.path().join("acme")).unwrap();
        assert_ne!(
            state.account_path(crate::tls::plan::LETSENCRYPT_PRODUCTION),
            state.account_path(crate::tls::plan::LETSENCRYPT_STAGING)
        );
    }
}
