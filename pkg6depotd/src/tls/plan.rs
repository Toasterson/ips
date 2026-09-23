//! Validation of the `tls` configuration block into typed settings.
//!
//! Everything here is structural and does not touch the filesystem, so
//! `config-test` and `start` reject the same mistakes. File checks (token
//! permissions, certificate contents) happen in [`super::preflight`].

use super::error::TlsError;
use crate::config::{AcmeConfig, Config, Dns01Config, TlsConfig};
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const LETSENCRYPT_PRODUCTION: &str = "https://acme-v02.api.letsencrypt.org/directory";
pub const LETSENCRYPT_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_RENEW_BEFORE_DAYS: u64 = 30;
const DEFAULT_PROPAGATION: Duration = Duration::from_secs(60);
/// Let's Encrypt accepts at most 100 names per certificate.
const MAX_DOMAINS: usize = 100;

/// How the depot terminates connections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsPlan {
    /// Certificate and key files maintained by the operator.
    Files {
        certificate: PathBuf,
        key: PathBuf,
        handshake_timeout: Duration,
    },
    /// Certificates obtained and renewed through ACME.
    Acme {
        settings: AcmeSettings,
        handshake_timeout: Duration,
    },
}

impl TlsPlan {
    pub fn handshake_timeout(&self) -> Duration {
        match self {
            TlsPlan::Files {
                handshake_timeout, ..
            }
            | TlsPlan::Acme {
                handshake_timeout, ..
            } => *handshake_timeout,
        }
    }
}

/// Validated ACME settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcmeSettings {
    pub domains: Vec<DomainName>,
    pub contact: Vec<String>,
    pub directory_url: String,
    pub state_dir: PathBuf,
    pub renew_before: Duration,
    pub challenge: ChallengePlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChallengePlan {
    Http01 { bind: SocketAddr },
    Dns01(DnsPlan),
}

impl ChallengePlan {
    pub fn name(&self) -> &'static str {
        match self {
            ChallengePlan::Http01 { .. } => "http-01",
            ChallengePlan::Dns01(_) => "dns-01",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsProviderKind {
    Cloudflare,
    Gandi,
}

impl DnsProviderKind {
    pub fn name(self) -> &'static str {
        match self {
            DnsProviderKind::Cloudflare => "cloudflare",
            DnsProviderKind::Gandi => "gandi",
        }
    }

    /// Allowed TTL range and default for TXT records.
    fn ttl_bounds(self) -> (u32, u32, u32) {
        match self {
            // Cloudflare: 60..86400 (1 means "auto", which we do not use).
            DnsProviderKind::Cloudflare => (60, 86400, 120),
            // Gandi LiveDNS: 300..2592000.
            DnsProviderKind::Gandi => (300, 2_592_000, 300),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsPlan {
    pub provider: DnsProviderKind,
    pub api_token_file: PathBuf,
    pub zone: DomainName,
    pub zone_id: Option<String>,
    pub propagation: Duration,
    pub ttl: u32,
}

/// A lower-case DNS name, optionally a wildcard (`*.example.org`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DomainName(String);

impl DomainName {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let name = raw.trim_end_matches('.').to_ascii_lowercase();
        if name.is_empty() || name.len() > 253 {
            return Err(format!("{raw:?} is not a valid DNS name (length)"));
        }
        let base = name.strip_prefix("*.").unwrap_or(&name);
        let labels: Vec<&str> = base.split('.').collect();
        if labels.len() < 2 {
            return Err(format!(
                "{raw:?} is not a fully qualified DNS name; use for example \"pkg.example.org\""
            ));
        }
        for label in &labels {
            let ok = !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-');
            if !ok {
                return Err(format!(
                    "{raw:?} is not a valid DNS name (label {label:?}); only letters, digits and inner hyphens are allowed, IDNs must be given in punycode"
                ));
            }
        }
        if labels
            .last()
            .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(format!(
                "{raw:?} looks like an IP address; ACME certificates here are for DNS names only"
            ));
        }
        Ok(DomainName(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_wildcard(&self) -> bool {
        self.0.starts_with("*.")
    }

    /// The name without a leading `*.`.
    pub fn base(&self) -> &str {
        self.0.strip_prefix("*.").unwrap_or(&self.0)
    }

    /// True if this name is `zone` or below it.
    pub fn is_within(&self, zone: &DomainName) -> bool {
        let base = self.base();
        base == zone.as_str() || base.ends_with(&format!(".{}", zone.as_str()))
    }
}

impl fmt::Display for DomainName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn bad(message: impl Into<String>, help: impl Into<String>) -> TlsError {
    TlsError::config(message, help)
}

/// Validate the TLS part of the configuration.
///
/// Returns `None` for plain HTTP (reverse-proxy mode). Any inconsistency is
/// an error: the depot never falls back to plain HTTP when TLS was asked for.
pub fn plan(config: &Config) -> Result<Option<TlsPlan>, TlsError> {
    if config.server.tls_cert.is_some() || config.server.tls_key.is_some() {
        return Err(bad(
            "server.tls-cert and server.tls-key are not supported",
            "Move them into a top-level block: tls { certificate \"/path/fullchain.pem\"; key \"/path/key.pem\" }",
        ));
    }
    let Some(tls) = &config.tls else {
        return Ok(None);
    };
    plan_tls(tls, config).map(Some)
}

fn plan_tls(tls: &TlsConfig, config: &Config) -> Result<TlsPlan, TlsError> {
    let handshake_timeout = match tls.handshake_timeout {
        None => DEFAULT_HANDSHAKE_TIMEOUT,
        Some(s @ 1..=120) => Duration::from_secs(s),
        Some(s) => {
            return Err(bad(
                format!("tls.handshake-timeout {s} is out of range"),
                "Use 1 to 120 seconds.",
            ));
        }
    };
    match (&tls.certificate, &tls.key, &tls.acme) {
        (Some(certificate), Some(key), None) => {
            absolute("tls.certificate", certificate)?;
            absolute("tls.key", key)?;
            Ok(TlsPlan::Files {
                certificate: certificate.clone(),
                key: key.clone(),
                handshake_timeout,
            })
        }
        (None, None, Some(acme)) => Ok(TlsPlan::Acme {
            settings: plan_acme(acme, config)?,
            handshake_timeout,
        }),
        (None, None, None) => Err(bad(
            "the tls block names no certificate source",
            "Give either certificate and key files, or an acme block. Remove the tls block to serve plain HTTP behind a reverse proxy.",
        )),
        (_, _, Some(_)) => Err(bad(
            "tls.certificate/tls.key and tls.acme are mutually exclusive",
            "Use static files or ACME, not both.",
        )),
        _ => Err(bad(
            "tls.certificate and tls.key must be given together",
            "Set both paths: the PEM certificate chain and its PEM private key.",
        )),
    }
}

fn absolute(what: &str, path: &Path) -> Result<(), TlsError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(bad(
            format!("{what} {} is not an absolute path", path.display()),
            "SMF starts the depot with an unrelated working directory; use absolute paths.",
        ))
    }
}

fn plan_acme(acme: &AcmeConfig, config: &Config) -> Result<AcmeSettings, TlsError> {
    // Domains.
    let raw_domains = acme.domains.as_deref().unwrap_or_default();
    if raw_domains.is_empty() {
        return Err(bad(
            "tls.acme.domains is missing or empty",
            "List the names clients use, for example: domains \"pkg.example.org\"",
        ));
    }
    if raw_domains.len() > MAX_DOMAINS {
        return Err(bad(
            format!("tls.acme.domains lists {} names", raw_domains.len()),
            format!("At most {MAX_DOMAINS} names fit in one certificate."),
        ));
    }
    let mut domains = Vec::with_capacity(raw_domains.len());
    for raw in raw_domains {
        let d = DomainName::parse(raw).map_err(|m| {
            bad(
                format!("tls.acme.domains: {m}"),
                "Use fully qualified DNS names.",
            )
        })?;
        if domains.contains(&d) {
            return Err(bad(
                format!("tls.acme.domains lists {d} twice"),
                "Remove the duplicate.",
            ));
        }
        domains.push(d);
    }

    // Contact.
    let contact = acme.contact.clone().unwrap_or_default();
    for c in &contact {
        let addr = c.strip_prefix("mailto:").unwrap_or("");
        if addr.is_empty() || !addr.contains('@') || addr.contains([',', ' ', '\n', '?']) {
            return Err(bad(
                format!("tls.acme.contact {c:?} is not a mailto: address"),
                "Use for example: contact \"mailto:hostmaster@example.org\"",
            ));
        }
    }

    // Terms of service.
    if acme.terms_agreed != Some(true) {
        return Err(bad(
            "tls.acme.terms-agreed is not true",
            "Read the CA's subscriber agreement and set terms-agreed true to accept it.",
        ));
    }

    // Directory.
    let directory_url = match (&acme.directory_url, acme.staging) {
        (Some(_), Some(true)) => {
            return Err(bad(
                "tls.acme.staging and tls.acme.directory-url are both set",
                "staging true selects the Let's Encrypt staging directory; for another CA set directory-url only.",
            ));
        }
        (Some(url), _) => {
            check_directory_url(url)?;
            url.clone()
        }
        (None, Some(true)) => LETSENCRYPT_STAGING.to_string(),
        (None, _) => LETSENCRYPT_PRODUCTION.to_string(),
    };

    // State directory.
    let Some(state_dir) = acme.state_dir.clone() else {
        return Err(bad(
            "tls.acme.state-dir is missing",
            "Name a directory owned by pkg6srv with mode 0700, for example state-dir \"/var/pkg6depotd/acme\".",
        ));
    };
    absolute("tls.acme.state-dir", &state_dir)?;

    let renew_before_days = acme.renew_before_days.unwrap_or(DEFAULT_RENEW_BEFORE_DAYS);
    if !(1..=60).contains(&renew_before_days) {
        return Err(bad(
            format!("tls.acme.renew-before-days {renew_before_days} is out of range"),
            "Use 1 to 60 days.",
        ));
    }

    // Challenge.
    let challenge = match acme.challenge.as_deref() {
        Some("http-01") => {
            if acme.dns01.is_some() {
                return Err(bad(
                    "tls.acme.dns01 is set but challenge is http-01",
                    "Remove the dns01 block or set challenge \"dns-01\".",
                ));
            }
            if let Some(w) = domains.iter().find(|d| d.is_wildcard()) {
                return Err(bad(
                    format!("wildcard {w} cannot be validated with http-01"),
                    "Use challenge \"dns-01\" for wildcard names.",
                ));
            }
            let Some(bind) = &acme.http01_bind else {
                return Err(bad(
                    "tls.acme.http01-bind is missing",
                    "http-01 needs a plain HTTP listener reachable on port 80 of every domain, for example http01-bind \"0.0.0.0:80\".",
                ));
            };
            let bind: SocketAddr = bind.parse().map_err(|e| {
                bad(
                    format!("tls.acme.http01-bind {bind:?}: {e}"),
                    "Use an IP address and port, for example \"0.0.0.0:80\".",
                )
            })?;
            if config.server.bind.iter().any(|b| b.parse() == Ok(bind)) {
                return Err(bad(
                    "tls.acme.http01-bind equals server.bind",
                    "The HTTPS listener and the http-01 listener need different ports (usually 443 and 80).",
                ));
            }
            ChallengePlan::Http01 { bind }
        }
        Some("dns-01") => {
            if acme.http01_bind.is_some() {
                return Err(bad(
                    "tls.acme.http01-bind is set but challenge is dns-01",
                    "Remove http01-bind or set challenge \"http-01\".",
                ));
            }
            let Some(dns) = &acme.dns01 else {
                return Err(bad(
                    "tls.acme.dns01 block is missing",
                    "dns-01 needs a dns01 { provider ...; api-token-file ...; zone ... } block.",
                ));
            };
            let plan = plan_dns(dns)?;
            if let Some(d) = domains.iter().find(|d| !d.is_within(&plan.zone)) {
                return Err(bad(
                    format!("{d} is not inside the DNS zone {}", plan.zone),
                    "Every ACME domain must live in tls.acme.dns01.zone.",
                ));
            }
            ChallengePlan::Dns01(plan)
        }
        Some(other) => {
            return Err(bad(
                format!("unknown tls.acme.challenge {other:?}"),
                "Use \"http-01\" or \"dns-01\".",
            ));
        }
        None => {
            return Err(bad(
                "tls.acme.challenge is missing",
                "Set challenge \"http-01\" or challenge \"dns-01\".",
            ));
        }
    };

    Ok(AcmeSettings {
        domains,
        contact,
        directory_url,
        state_dir,
        renew_before: Duration::from_secs(renew_before_days * 86_400),
        challenge,
    })
}

/// ACME directories must be HTTPS. Plain HTTP is accepted only for a
/// loopback host, which the test suite uses for its mock CA.
fn check_directory_url(url: &str) -> Result<(), TlsError> {
    let parsed = reqwest::Url::parse(url).map_err(|e| {
        bad(
            format!("tls.acme.directory-url {url:?}: {e}"),
            "Use the CA's https:// directory URL.",
        )
    })?;
    let loopback = match parsed.host_str() {
        Some("localhost") => true,
        Some(h) => h
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    };
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        _ => Err(bad(
            format!("tls.acme.directory-url {url:?} is not an https URL"),
            "ACME account keys and orders must only travel over HTTPS.",
        )),
    }
}

fn plan_dns(dns: &Dns01Config) -> Result<DnsPlan, TlsError> {
    let provider = match dns.provider.as_deref() {
        Some("cloudflare") => DnsProviderKind::Cloudflare,
        Some("gandi") => DnsProviderKind::Gandi,
        Some(other) => {
            return Err(bad(
                format!("unknown tls.acme.dns01.provider {other:?}"),
                "Supported providers: \"cloudflare\", \"gandi\".",
            ));
        }
        None => {
            return Err(bad(
                "tls.acme.dns01.provider is missing",
                "Set provider \"cloudflare\" or provider \"gandi\".",
            ));
        }
    };
    let Some(api_token_file) = dns.api_token_file.clone() else {
        return Err(bad(
            "tls.acme.dns01.api-token-file is missing",
            "Put the API token in a file readable by pkg6srv only (mode 0600 or 0640) and name it here. The token itself never goes into this configuration.",
        ));
    };
    absolute("tls.acme.dns01.api-token-file", &api_token_file)?;
    let Some(zone) = &dns.zone else {
        return Err(bad(
            "tls.acme.dns01.zone is missing",
            "Name the DNS zone that holds the domains, for example zone \"example.org\".",
        ));
    };
    let zone = DomainName::parse(zone)
        .ok()
        .filter(|z| !z.is_wildcard())
        .ok_or_else(|| {
            bad(
                format!("tls.acme.dns01.zone {zone:?} is not a DNS zone name"),
                "Use the registered domain, for example \"example.org\".",
            )
        })?;
    if dns.zone_id.is_some() && provider != DnsProviderKind::Cloudflare {
        return Err(bad(
            "tls.acme.dns01.zone-id is only used with cloudflare",
            "Remove zone-id.",
        ));
    }
    if let Some(id) = &dns.zone_id
        && (id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()))
    {
        return Err(bad(
            "tls.acme.dns01.zone-id is not a Cloudflare zone identifier",
            "Copy the zone ID from the Cloudflare dashboard, or remove zone-id to look it up by name.",
        ));
    }
    let propagation = match dns.propagation_seconds {
        None => DEFAULT_PROPAGATION,
        Some(s @ 0..=3600) => Duration::from_secs(s),
        Some(s) => {
            return Err(bad(
                format!("tls.acme.dns01.propagation-seconds {s} is out of range"),
                "Use 0 to 3600 seconds.",
            ));
        }
    };
    let (min, max, default) = provider.ttl_bounds();
    let ttl = dns.ttl.unwrap_or(default);
    if !(min..=max).contains(&ttl) {
        return Err(bad(
            format!(
                "tls.acme.dns01.ttl {ttl} is out of range for {}",
                provider.name()
            ),
            format!("Use {min} to {max} seconds."),
        ));
    }
    Ok(DnsPlan {
        provider,
        api_token_file,
        zone,
        zone_id: dns.zone_id.clone(),
        propagation,
        ttl,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_names() {
        assert_eq!(
            DomainName::parse("Pkg.Example.ORG.").unwrap().as_str(),
            "pkg.example.org"
        );
        assert!(DomainName::parse("*.example.org").unwrap().is_wildcard());
        for bad in [
            "localhost",
            "-a.example.org",
            "a..org",
            "a_b.example.org",
            "10.0.0.1",
            "*.*.example.org",
            "",
        ] {
            assert!(DomainName::parse(bad).is_err(), "{bad} accepted");
        }
        let zone = DomainName::parse("example.org").unwrap();
        assert!(DomainName::parse("example.org").unwrap().is_within(&zone));
        assert!(
            DomainName::parse("*.pkg.example.org")
                .unwrap()
                .is_within(&zone)
        );
        assert!(
            !DomainName::parse("badexample.org")
                .unwrap()
                .is_within(&zone)
        );
    }

    #[test]
    fn directory_urls() {
        assert!(check_directory_url("https://acme.example.org/dir").is_ok());
        assert!(check_directory_url("http://127.0.0.1:1234/dir").is_ok());
        assert!(check_directory_url("http://localhost:1234/dir").is_ok());
        assert!(check_directory_url("http://acme.example.org/dir").is_err());
        assert!(check_directory_url("ftp://127.0.0.1/dir").is_err());
    }
}
