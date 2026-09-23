//! Loading certificates and swapping them into the running TLS server.

use super::der::{self, CertificateSummary};
use super::error::TlsError;
use arc_swap::ArcSwap;
use base64::Engine;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Largest certificate or key file the depot reads.
const MAX_PEM_FILE: u64 = 1024 * 1024;

/// The crypto provider used for serving and for loading keys.
///
/// rustls is built with both ring and aws-lc-rs in this workspace, so the
/// provider is named explicitly instead of relying on a process default.
pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A certificate chain with its key, checked to belong together.
#[derive(Clone)]
pub struct LoadedCert {
    pub certified: Arc<CertifiedKey>,
    pub summary: CertificateSummary,
}

impl fmt::Debug for LoadedCert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedCert")
            .field("dns_names", &self.summary.dns_names)
            .field("not_after", &self.summary.not_after)
            .finish_non_exhaustive()
    }
}

impl LoadedCert {
    /// Build from DER parts. Fails if the chain is empty, the leaf does not
    /// parse, or the key does not match the leaf's public key.
    pub fn from_der(
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
        origin: &Path,
    ) -> Result<Self, TlsError> {
        let fail = |message: String| TlsError::Certificate {
            path: origin.to_path_buf(),
            message,
        };
        let leaf = chain
            .first()
            .ok_or_else(|| fail("no CERTIFICATE block found".into()))?;
        let summary =
            der::summarize_certificate(leaf).map_err(|e| fail(format!("leaf certificate: {e}")))?;
        let signing_key = provider()
            .key_provider
            .load_private_key(key)
            .map_err(|e| fail(format!("private key: {e}")))?;
        let certified = CertifiedKey::new(chain, signing_key);
        // Unknown consistency is an error here: never serve a key we cannot
        // tie to the certificate.
        certified
            .keys_match()
            .map_err(|e| fail(format!("private key does not match the certificate: {e}")))?;
        Ok(LoadedCert {
            certified: Arc::new(certified),
            summary,
        })
    }

    /// Parse a PEM chain and a PEM key.
    pub fn from_pem(chain_pem: &[u8], key_pem: &[u8], origin: &Path) -> Result<Self, TlsError> {
        let fail = |message: String| TlsError::Certificate {
            path: origin.to_path_buf(),
            message,
        };
        let chain = CertificateDer::pem_slice_iter(chain_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| fail(format!("certificate PEM: {e}")))?;
        let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| {
            // The pem error names only the section kind, never key material.
            fail(format!("private key PEM: {e}"))
        })?;
        Self::from_der(chain, key, origin)
    }

    /// Load operator-provided certificate and key files. The key file must
    /// not be accessible to other users.
    pub fn from_files(certificate: &Path, key: &Path) -> Result<Self, TlsError> {
        let chain = read_limited(certificate)?;
        let key_pem = read_private(key)?;
        Self::from_pem(&chain, &key_pem, certificate)
    }

    /// Load operator-provided files and require the certificate to be
    /// valid at `now`. Used at startup, by `config-test` and on reload.
    pub fn from_files_valid_at(certificate: &Path, key: &Path, now: i64) -> Result<Self, TlsError> {
        let cert = Self::from_files(certificate, key)?;
        cert.require_valid(now, certificate)?;
        Ok(cert)
    }

    /// Seconds since the Unix epoch after which the certificate is invalid.
    pub fn not_after(&self) -> i64 {
        self.summary.not_after
    }

    /// Ok if `now` lies inside the validity window, else why not.
    pub fn check_validity(&self, now: i64) -> Result<(), String> {
        let at = |t: i64| {
            chrono::DateTime::from_timestamp(t, 0)
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| t.to_string())
        };
        if now < self.summary.not_before {
            Err(format!(
                "certificate is not valid before {}",
                at(self.summary.not_before)
            ))
        } else if now >= self.summary.not_after {
            Err(format!(
                "certificate expired at {}",
                at(self.summary.not_after)
            ))
        } else {
            Ok(())
        }
    }

    pub fn require_valid(&self, now: i64, origin: &Path) -> Result<(), TlsError> {
        self.check_validity(now)
            .map_err(|message| TlsError::Certificate {
                path: origin.to_path_buf(),
                message,
            })
    }

    /// True if every name in `domains` is listed in the certificate.
    pub fn covers<'a>(&self, domains: impl IntoIterator<Item = &'a str>) -> bool {
        domains
            .into_iter()
            .all(|d| self.summary.dns_names.iter().any(|n| n == d))
    }
}

pub(crate) fn read_limited(path: &Path) -> Result<Vec<u8>, TlsError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| TlsError::io(format!("cannot read {}", path.display()), e))?;
    if !meta.is_file() || meta.len() > MAX_PEM_FILE {
        return Err(TlsError::Certificate {
            path: path.to_path_buf(),
            message: format!("not a regular file of at most {MAX_PEM_FILE} bytes"),
        });
    }
    std::fs::read(path).map_err(|e| TlsError::io(format!("cannot read {}", path.display()), e))
}

/// Read a file holding a private key. Group access is fine (for example
/// root:pkg6srv 0640); access for other users is refused.
pub(crate) fn read_private(path: &Path) -> Result<Vec<u8>, TlsError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)
        .map_err(|e| TlsError::io(format!("cannot read {}", path.display()), e))?;
    let mode = meta.permissions().mode() & 0o7777;
    if mode & 0o007 != 0 {
        return Err(TlsError::Permissions {
            path: path.to_path_buf(),
            message: format!("mode {mode:04o} lets any user read the private key"),
        });
    }
    read_limited(path)
}

/// Encode DER as a PEM block.
pub fn pem_encode(label: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// Server certificate resolver whose certificate can be replaced while the
/// server runs. New handshakes pick up a replacement immediately; existing
/// connections keep the certificate they negotiated.
pub struct CertStore {
    current: ArcSwap<LoadedCert>,
    source: CertSource,
}

/// Where [`CertStore::reload`] reads from.
#[derive(Debug, Clone)]
pub enum CertSource {
    Files {
        certificate: PathBuf,
        key: PathBuf,
    },
    /// The ACME state bundle (key and chain in one file).
    Bundle(PathBuf),
}

impl fmt::Debug for CertStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertStore")
            .field("current", &*self.current.load())
            .field("source", &self.source)
            .finish()
    }
}

impl CertStore {
    pub fn new(initial: LoadedCert, source: CertSource) -> Arc<Self> {
        Arc::new(CertStore {
            current: ArcSwap::from_pointee(initial),
            source,
        })
    }

    pub fn current(&self) -> Arc<LoadedCert> {
        self.current.load_full()
    }

    /// Replace the served certificate.
    pub fn swap(&self, next: LoadedCert) {
        tracing::info!(
            names = ?next.summary.dns_names,
            not_after = next.not_after(),
            "installed new TLS certificate"
        );
        self.current.store(Arc::new(next));
    }

    /// Re-read the certificate from its source and swap it in. A
    /// certificate that is expired or not yet valid is refused like a
    /// broken one. On any error the current certificate stays in place.
    pub fn reload(&self) -> Result<(), TlsError> {
        self.reload_at(crate::tls::acme::unix_now())
    }

    /// [`reload`](Self::reload) with an explicit current time.
    pub fn reload_at(&self, now: i64) -> Result<(), TlsError> {
        let next = match &self.source {
            CertSource::Files { certificate, key } => {
                LoadedCert::from_files_valid_at(certificate, key, now)?
            }
            CertSource::Bundle(path) => {
                let pem = read_private(path)?;
                let cert = LoadedCert::from_pem(&pem, &pem, path)?;
                cert.require_valid(now, path)?;
                cert
            }
        };
        self.swap(next);
        Ok(())
    }

    /// A rustls server configuration serving this store's certificate.
    pub fn server_config(self: &Arc<Self>) -> Result<rustls::ServerConfig, TlsError> {
        let mut config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| TlsError::config(format!("TLS setup: {e}"), "This is a bug."))?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Resolver(self.clone())));
        // The router speaks HTTP/1.1 only (axum is built without http2).
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(config)
    }
}

#[derive(Debug)]
struct Resolver(Arc<CertStore>);

impl ResolvesServerCert for Resolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.current.load().certified.clone())
    }
}
