use crate::config::Config;
use crate::errors::{DepotError, Result};
use libips::fmri::Fmri;
use libips::repository::generation::is_valid_publisher_name;
use libips::repository::retention::{AccessKind, AccessRecorder};
use libips::repository::{
    FileBackend, IndexEntry, PackageInfo, REPOSITORY_CONFIG_FILENAME, ReadableRepository,
    RepositoryError,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

pub struct DepotRepo {
    pub backend: Mutex<FileBackend>,
    pub root: PathBuf,
    pub cache_max_age: u64,
    /// Identity of the published generation and configuration file the
    /// backend was loaded from.
    config_stamp: Mutex<Option<ConfigStamp>>,
    /// Durable accounting of qualifying (manifest and payload) requests,
    /// which drives retirement. Never fails or delays a request.
    access: Arc<AccessRecorder>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigStamp {
    /// Target of `generations/current`. On a generation-managed repository
    /// the publisher list and default publisher come from that generation,
    /// so they change with the same rename as its catalogs.
    generation: Option<PathBuf>,
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    ino: u64,
}

impl ConfigStamp {
    fn read(root: &Path) -> Option<Self> {
        let generation = std::fs::read_link(
            root.join(libips::repository::generation::GENERATIONS_DIR)
                .join(libips::repository::generation::CURRENT_LINK),
        )
        .ok();
        let meta = std::fs::metadata(root.join(REPOSITORY_CONFIG_FILENAME)).ok()?;
        Some(Self {
            generation,
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            ino: std::os::unix::fs::MetadataExt::ino(&meta),
        })
    }
}

fn not_found(what: String) -> DepotError {
    DepotError::Repo(RepositoryError::NotFound(what))
}

/// Request path segments name repository paths; reject anything that is not
/// a publisher prefix so they cannot escape the repository.
fn check_publisher(publisher: &str) -> Result<()> {
    if is_valid_publisher_name(publisher) {
        Ok(())
    } else {
        Err(DepotError::Repo(RepositoryError::PublisherNotFound(
            publisher.to_string(),
        )))
    }
}

/// Name of this depot's access recorder: stable across restarts (so a
/// restart closes the previous run) and distinct between depots serving the
/// same repository (host and listen address).
fn recorder_name(config: &Config) -> String {
    let host = nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    let bind = config
        .server
        .bind
        .first()
        .cloned()
        .unwrap_or_else(|| "-".to_string());
    format!("{}/{}", host, bind)
}

fn is_hex_digest(digest: &str) -> bool {
    digest.len() >= 2 && digest.bytes().all(|b| b.is_ascii_hexdigit())
}

/// FMRI components become single path components; `.`/`..` would not.
fn check_fmri_components(fmri: &Fmri) -> Result<()> {
    let version = fmri.version();
    for c in [fmri.stem(), version.as_str()] {
        if c == "." || c == ".." {
            return Err(not_found(format!("manifest for {}", fmri)));
        }
    }
    Ok(())
}

impl DepotRepo {
    pub fn new(config: &Config) -> Result<Self> {
        let root = config.repository.root.clone();
        let backend = FileBackend::open(&root).map_err(DepotError::Repo)?;
        let cache_max_age = config.server.cache_max_age.unwrap_or(3600);
        Ok(Self {
            backend: Mutex::new(backend),
            config_stamp: Mutex::new(ConfigStamp::read(&root)),
            access: Arc::new(AccessRecorder::new(&root, &recorder_name(config))),
            root,
            cache_max_age,
        })
    }

    /// The backend, reloaded when a new generation was published or the
    /// configuration was replaced. Publisher discovery, the default
    /// publisher and unscoped search therefore follow the generation switch
    /// itself (not the configuration write that follows it) without a depot
    /// restart.
    fn backend(&self) -> Result<MutexGuard<'_, FileBackend>> {
        let mut backend = self
            .backend
            .lock()
            .map_err(|e| DepotError::Server(format!("Lock poisoned: {}", e)))?;
        let mut stamp = self
            .config_stamp
            .lock()
            .map_err(|e| DepotError::Server(format!("Lock poisoned: {}", e)))?;
        let now = ConfigStamp::read(&self.root);
        if now != *stamp {
            match FileBackend::open(&self.root) {
                Ok(fresh) => {
                    *backend = fresh;
                    *stamp = now;
                }
                // Keep serving the last good configuration.
                Err(e) => tracing::warn!("failed to reload repository configuration: {}", e),
            }
        }
        Ok(backend)
    }

    pub fn search(
        &self,
        publisher: Option<&str>,
        query: &str,
        case_sensitive: bool,
    ) -> Result<Vec<IndexEntry>> {
        if let Some(p) = publisher {
            check_publisher(p)?;
        }
        let backend = self.backend()?;
        backend
            .search_detailed(query, publisher, None, case_sensitive)
            .map_err(DepotError::Repo)
    }

    pub fn get_catalog_path(&self, publisher: &str) -> Result<PathBuf> {
        check_publisher(publisher)?;
        Ok(FileBackend::construct_catalog_path(&self.root, publisher))
    }

    pub fn get_file_path(&self, publisher: &str, hash: &str) -> Option<PathBuf> {
        if !is_valid_publisher_name(publisher) || !is_hex_digest(hash) {
            return None;
        }
        let cand_pub = FileBackend::construct_file_path_with_publisher(&self.root, publisher, hash);
        if cand_pub.exists() {
            return Some(cand_pub);
        }

        let cand_global = FileBackend::construct_file_path(&self.root, hash);
        if cand_global.exists() {
            return Some(cand_global);
        }

        None
    }

    pub fn get_manifest_text(&self, publisher: &str, fmri: &Fmri) -> Result<String> {
        check_publisher(publisher)?;
        check_fmri_components(fmri)?;
        let backend = self.backend()?;
        backend
            .fetch_manifest_text(publisher, fmri)
            .map_err(DepotError::Repo)
    }

    /// Record a served manifest of an exact package version, including a
    /// hidden (retired) one. The version stays hidden.
    pub fn record_manifest_access(&self, publisher: &str, fmri: &Fmri) {
        let exact = Fmri::with_publisher(publisher, fmri.stem(), fmri.version.clone());
        self.access
            .record(AccessKind::Manifest, publisher, &exact.to_string());
    }

    /// Record a served payload.
    pub fn record_payload_access(&self, publisher: &str, digest: &str) {
        self.access.record(AccessKind::Payload, publisher, digest);
    }

    /// Start the access heartbeat, which proves to retirement that this
    /// depot records every access and answers apply's fence. Call once the
    /// depot serves (after daemonising: threads do not survive a fork).
    pub fn start_access_heartbeat(&self) -> Result<()> {
        self.access.spawn_heartbeat().map_err(DepotError::Io)?;
        Ok(())
    }

    /// End access recording cleanly on shutdown, so the stop is not taken
    /// for a crash (which would restart every retirement clock).
    pub fn shutdown_access(&self) {
        self.access.shutdown();
    }

    /// Stop access recording without ending the run, when the depot stops
    /// while requests may still be served: the run goes stale and shows
    /// as a gap after restart instead of vouching for accesses it cannot
    /// record.
    pub fn abandon_access(&self) {
        self.access.abandon();
    }

    pub fn get_manifest_path(&self, publisher: &str, fmri: &Fmri) -> Option<PathBuf> {
        if !is_valid_publisher_name(publisher) || check_fmri_components(fmri).is_err() {
            return None;
        }
        let version = fmri.version();
        if version.is_empty() {
            return None;
        }
        let path =
            FileBackend::construct_manifest_path(&self.root, publisher, fmri.stem(), &version);
        if path.exists() {
            return Some(path);
        }
        // Fallbacks similar to lib logic
        let encoded_stem = url_encode_filename(fmri.stem());
        let encoded_version = url_encode_filename(&version);
        let alt1 = self
            .root
            .join("pkg")
            .join(&encoded_stem)
            .join(&encoded_version);
        if alt1.exists() {
            return Some(alt1);
        }
        let alt2 = self
            .root
            .join("publisher")
            .join(publisher)
            .join("pkg")
            .join(&encoded_stem)
            .join(&encoded_version);
        if alt2.exists() {
            return Some(alt2);
        }
        None
    }

    pub fn cache_max_age(&self) -> u64 {
        self.cache_max_age
    }

    pub fn shard_dir(&self, publisher: &str) -> Result<PathBuf> {
        check_publisher(publisher)?;
        Ok(self.root.join("publisher").join(publisher).join("catalog2"))
    }

    pub fn get_catalog_file_path(&self, publisher: &str, filename: &str) -> Result<PathBuf> {
        check_publisher(publisher)?;
        if filename.is_empty() || filename.starts_with('.') {
            return Err(not_found(filename.to_string()));
        }
        let backend = self.backend()?;
        backend
            .get_catalog_file_path(publisher, filename)
            .map_err(DepotError::Repo)
    }

    pub fn list_packages(
        &self,
        publisher: Option<&str>,
        pattern: Option<&str>,
    ) -> Result<Vec<PackageInfo>> {
        let backend = self.backend()?;
        backend
            .list_packages(publisher, pattern)
            .map_err(DepotError::Repo)
    }

    pub fn get_info(&self) -> Result<libips::repository::RepositoryInfo> {
        let backend = self.backend()?;
        backend.get_info().map_err(DepotError::Repo)
    }
}

// Local percent-encoding for filenames similar to lib's private helper.
fn url_encode_filename(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => result.push(c),
            ' ' => result.push('+'),
            _ => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).as_bytes() {
                    result.push('%');
                    result.push_str(&format!("{:02X}", b));
                }
            }
        }
    }
    result
}
