use miette::Diagnostic;
use std::path::PathBuf;
use thiserror::Error;

/// Errors from TLS setup and ACME certificate management.
///
/// Messages never include secrets: API tokens and private keys are not part
/// of any variant, and provider or CA responses are truncated.
#[derive(Debug, Error, Diagnostic)]
pub enum TlsError {
    #[error("invalid TLS configuration: {message}")]
    #[diagnostic(code(ips::depot_error::tls::config))]
    Config {
        message: String,
        #[help]
        help: String,
    },

    #[error("{context}: {source}")]
    #[diagnostic(
        code(ips::depot_error::tls::io),
        help("Check that the path exists and that the service user (pkg6srv) may access it.")
    )]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("certificate {path}: {message}")]
    #[diagnostic(
        code(ips::depot_error::tls::certificate),
        help(
            "The file must hold a PEM certificate chain (leaf first) and the matching PEM private key. The depot keeps serving its previous certificate when a reload fails."
        )
    )]
    Certificate { path: PathBuf, message: String },

    #[error("insecure permissions on {path}: {message}")]
    #[diagnostic(
        code(ips::depot_error::tls::permissions),
        help(
            "Secrets must not be accessible to other users. For example: chown root:pkg6srv FILE && chmod 0640 FILE, or chmod 0700 for the ACME state directory."
        )
    )]
    Permissions { path: PathBuf, message: String },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Acme(#[from] AcmeError),
}

impl TlsError {
    pub fn config(message: impl Into<String>, help: impl Into<String>) -> Self {
        TlsError::Config {
            message: message.into(),
            help: help.into(),
        }
    }

    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        TlsError::Io {
            context: context.into(),
            source,
        }
    }
}

/// Errors talking to an ACME CA or a DNS provider.
#[derive(Debug, Error, Diagnostic)]
pub enum AcmeError {
    #[error("ACME request failed ({context}): {message}")]
    #[diagnostic(
        code(ips::depot_error::acme::http),
        help("Check network access to the ACME directory. The request is retried with backoff.")
    )]
    Http { context: String, message: String },

    #[error("ACME server rejected {context}: {kind}: {detail} (HTTP {status})")]
    #[diagnostic(
        code(ips::depot_error::acme::problem),
        help(
            "See the ACME problem type. rateLimited means wait; unauthorized or dns usually means the challenge was not reachable for this domain."
        )
    )]
    Problem {
        context: String,
        status: u16,
        kind: String,
        detail: String,
    },

    #[error("ACME protocol error: {0}")]
    #[diagnostic(
        code(ips::depot_error::acme::protocol),
        help("The CA sent a response pkg6depotd does not understand. Check directory-url.")
    )]
    Protocol(String),

    #[error("challenge for {domain} failed: {detail}")]
    #[diagnostic(
        code(ips::depot_error::acme::challenge),
        help(
            "http-01 needs port 80 of every domain to reach http01-bind; dns-01 needs the provider token to be allowed to edit TXT records in the zone."
        )
    )]
    Challenge { domain: String, detail: String },

    #[error("gave up waiting for {what} after {attempts} attempts")]
    #[diagnostic(
        code(ips::depot_error::acme::timeout),
        help("The CA did not finish in time. The next attempt starts a fresh order.")
    )]
    Timeout { what: String, attempts: u32 },

    #[error("DNS provider {provider}: {message}")]
    #[diagnostic(
        code(ips::depot_error::acme::dns),
        help(
            "Check the API token (Cloudflare: Zone.DNS edit; Gandi: personal access token with LiveDNS rights) and the zone name."
        )
    )]
    Dns {
        provider: &'static str,
        message: String,
    },

    #[error("ACME state {path}: {message}")]
    #[diagnostic(
        code(ips::depot_error::acme::state),
        help(
            "The state directory holds the account key and certificates. Fix or remove the named file; a removed account is registered again."
        )
    )]
    State { path: PathBuf, message: String },

    #[error("dns-01 cleanup journal {path} is unreadable: {message}")]
    #[diagnostic(
        code(ips::depot_error::acme::journal),
        help(
            "The file lists _acme-challenge TXT records that may still be published. pkg6depotd will not overwrite it. Remove the listed records at the DNS provider (the file may be partly readable), then delete the file and restart."
        )
    )]
    Journal { path: PathBuf, message: String },

    #[error("cryptographic operation failed: {0}")]
    #[diagnostic(code(ips::depot_error::acme::crypto))]
    Crypto(String),
}

/// Keep at most `max` characters of an untrusted remote message.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}
