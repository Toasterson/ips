//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

//! Immutable repository generations with an atomic publication switch.
//!
//! A generation-managed repository keeps every reader-visible piece of
//! per-publisher metadata (manifests, legacy v1 catalog parts, SQLite shards)
//! in an immutable generation directory:
//!
//! ```text
//! <repo>/generations/<id>/generation.json
//! <repo>/generations/<id>/receipts/<archive-sha256>.json
//! <repo>/generations/<id>/publisher/<pub>/{pkg,catalog,catalog2}/...
//! <repo>/generations/current -> <id>
//! <repo>/publisher/<pub>/{pkg,catalog,catalog2}
//!     -> ../../generations/current/publisher/<pub>/{pkg,catalog,catalog2}
//! <repo>/publisher/<pub>/file/...        shared, content-addressed payloads
//! ```
//!
//! The on-disk pathnames that pkg5 and pkg6 readers use never change. A new
//! generation is staged next to the current one (same filesystem), fsynced,
//! renamed into place and then published by a single `rename(2)` of a fresh
//! symlink over `generations/current`. That rename is the commit point for all
//! publishers at once: a crash before it leaves the previous view, a crash
//! after it leaves the new view including its import receipts.
//!
//! Manifests are shared between generations by hard links. Nothing ever
//! writes into an existing file of a committed generation: new content is
//! always a new inode, so readers holding the old generation keep a complete,
//! consistent snapshot.
//!
//! All writers serialise on an exclusive `flock(2)` of `<repo>/.pkg6-writer.lock`.

use super::{
    FileBackend, ReadableRepository, RepositoryConfig, RepositoryError, WritableRepository,
};
use miette::Diagnostic;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing::{debug, info, warn};

/// Directory below the repository root that holds all generations.
pub const GENERATIONS_DIR: &str = "generations";
/// Symlink inside [`GENERATIONS_DIR`] naming the published generation.
pub const CURRENT_LINK: &str = "current";
/// Lock file serialising every repository writer.
pub const WRITER_LOCK_FILE: &str = ".pkg6-writer.lock";
/// Per-generation metadata file.
pub const GENERATION_INFO_FILE: &str = "generation.json";
/// Per-generation directory of import receipts, keyed by archive SHA-256.
pub const RECEIPTS_DIR: &str = "receipts";
/// Per-publisher directories that live inside a generation.
pub const GENERATION_LINKED_DIRS: [&str; 3] = ["pkg", "catalog", "catalog2"];
/// Superseded generations kept after a switch, so that readers resolving
/// paths around the switch never race with deletion.
pub const RETAINED_PREVIOUS_GENERATIONS: usize = 2;

const STAGING_PREFIX: &str = ".staging-";
const INCOMING_PREFIX: &str = ".incoming-";
const CURRENT_TMP_PREFIX: &str = ".current-";

pub type Result<T> = std::result::Result<T, GenerationError>;

/// Points at which tests can simulate a crash of a publishing operation.
///
/// An injected failure returns [`GenerationError::InjectedFailure`] without
/// running any cleanup, leaving the on-disk debris a real crash would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailPoint {
    /// After new payload blobs were stored, before the generation is staged.
    AfterPayloads,
    /// After the new generation directory is complete and durable, before
    /// `generations/current` is switched.
    BeforeSwitch,
    /// After the switch, before configuration reconciliation, cleanup and
    /// acknowledgement to the caller.
    AfterSwitch,
}

impl fmt::Display for FailPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FailPoint::AfterPayloads => "after-payloads",
            FailPoint::BeforeSwitch => "before-switch",
            FailPoint::AfterSwitch => "after-switch",
        })
    }
}

#[derive(Debug, Error, Diagnostic)]
pub enum GenerationError {
    #[error("I/O error at {path}: {source}")]
    #[diagnostic(
        code(ips::generation::io),
        help("Check that the repository filesystem is writable and has free space")
    )]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("cannot take the repository writer lock {path}: {source}")]
    #[diagnostic(
        code(ips::generation::lock),
        help("The repository filesystem must support flock(2) and be writable by this user")
    )]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("repository {path} is locked by another writer")]
    #[diagnostic(
        code(ips::generation::busy),
        help("Another import, rebuild or migration is running. Wait for it to finish and retry")
    )]
    Busy { path: PathBuf },

    #[error("{operation} cannot run on {path}: it publishes through repository generations")]
    #[diagnostic(
        code(ips::generation::managed_repository),
        help(
            "This operation writes into the live catalog without the atomic publication switch. Import packages with `pkg6repo import-pkg5`, or receive into an unmanaged repository and import its .p5p"
        )
    )]
    ManagedRepository { path: PathBuf, operation: String },

    #[error(
        "{path} holds packages in the pre-generation layout; adopting it needs explicit consent"
    )]
    #[diagnostic(
        code(ips::generation::adoption_required),
        help(
            "Converting pkg/, catalog/ and catalog2/ into links makes each path briefly absent. Stop or tolerate readers, then rerun with --adopt-existing (Pkg5ImportOptions::adopt_existing)"
        )
    )]
    AdoptionRequired { path: PathBuf },

    #[error(
        "{path} is a real directory of published publisher {publisher} and differs from the published generation:\n{}",
        .diverged.iter().map(|d| format!("  - {}", d)).collect::<Vec<_>>().join("\n")
    )]
    #[diagnostic(
        code(ips::generation::unexpected_live_directory),
        help(
            "Only links into generations/current may live there; nothing was changed. Move the directory aside and retry (the next writer restores the link to the published generation), then publish any packages it holds through `pkg6repo import-pkg5`"
        )
    )]
    UnexpectedLiveDirectory {
        path: PathBuf,
        publisher: String,
        diverged: Vec<String>,
    },

    #[error(
        "real publisher metadata directories exist for publishers that are neither published nor configured:\n{}",
        .paths.iter().map(|p| format!("  - {}", p.display())).collect::<Vec<_>>().join("\n")
    )]
    #[diagnostic(
        code(ips::generation::unregistered_live_directory),
        help(
            "Nothing was changed. Readers would keep serving these directories instead of any generation, so no writer may run while they exist. Move them aside and retry; publish any packages they hold by importing a .p5p of them with `pkg6repo import-pkg5` (or, before first adoption, list the publisher in pkg6.repository and rerun with --adopt-existing)"
        )
    )]
    UnregisteredLiveDirectory { paths: Vec<PathBuf> },

    #[error("invalid publisher name {0:?}")]
    #[diagnostic(
        code(ips::generation::invalid_publisher),
        help(
            "Publisher names use letters, digits, '.', '-' and '_' and must not start with '.' or '-'"
        )
    )]
    InvalidPublisher(String),

    #[error("repository generation state is inconsistent: {0}")]
    #[diagnostic(
        code(ips::generation::corrupt),
        help(
            "Inspect <repo>/generations: `current` must be a symlink to a generation directory containing generation.json"
        )
    )]
    Corrupt(String),

    #[error("injected failure at {0}")]
    #[diagnostic(
        code(ips::generation::injected_failure),
        help("Produced only by failure-injection tests. Rerun the operation to recover")
    )]
    InjectedFailure(FailPoint),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Repository(Box<RepositoryError>),
}

impl From<RepositoryError> for GenerationError {
    fn from(err: RepositoryError) -> Self {
        GenerationError::Repository(Box::new(err))
    }
}

trait IoContext<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| GenerationError::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Why a generation was created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum GenerationReason {
    /// Existing on-disk metadata was adopted into a generation (first use of
    /// a pre-generation repository, or directories created by a legacy tool).
    Adopt { publishers: Vec<String> },
    /// A pkg5 repository or `.p5p` archive was imported.
    Pkg5Import { archive_sha256: Option<String> },
    /// Catalogs were rebuilt from the generation's manifests.
    Rebuild { publishers: Vec<String> },
    /// An empty publisher was added.
    AddPublisher { publisher: String },
    /// A publisher was removed.
    RemovePublisher { publisher: String },
}

/// Contents of `generation.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationInfo {
    pub id: String,
    pub parent: Option<String>,
    pub created: String,
    pub reason: GenerationReason,
    /// Publishers present in this generation.
    pub publishers: Vec<String>,
    /// Publishers this generation introduced. Recovery re-applies their
    /// configuration if a crash hit between switch and reconciliation.
    #[serde(default)]
    pub added_publishers: Vec<String>,
    /// Latest catalog mtime (whole seconds since the epoch) assigned in this
    /// generation or any ancestor, across all publishers ever published. New
    /// catalogs are stamped later than this, so If-Modified-Since never
    /// matches a catalog it has not seen, even across publisher removal and
    /// re-addition.
    #[serde(default)]
    pub catalog_mtime: u64,
    /// Default publisher of this generation (a member of `publishers`, or
    /// `None` if there are none). Published by the same switch as the
    /// publisher list.
    #[serde(default)]
    pub default_publisher: Option<String>,
}

impl GenerationInfo {
    /// The publisher list and default publisher readers must use, derived
    /// from this generation so they switch together with its catalogs.
    ///
    /// The configured order and default are kept where they agree with the
    /// generation; configuration written after the switch never adds or
    /// removes a publisher.
    pub fn publisher_view(
        &self,
        configured: &[String],
        configured_default: Option<&str>,
    ) -> (Vec<String>, Option<String>) {
        let mut publishers: Vec<String> = configured
            .iter()
            .filter(|p| self.publishers.contains(*p))
            .cloned()
            .collect();
        for p in &self.publishers {
            if !publishers.contains(p) {
                publishers.push(p.clone());
            }
        }
        let default = configured_default
            .filter(|d| self.publishers.iter().any(|p| p == d))
            .map(str::to_string)
            .or_else(|| {
                self.default_publisher
                    .clone()
                    .filter(|d| self.publishers.contains(d))
            })
            .or_else(|| publishers.first().cloned());
        (publishers, default)
    }
}

/// Metadata of the generation `generations/current` names, read through a
/// single `readlink` so every field comes from the same generation.
pub fn published_generation(repo: &Path) -> Result<Option<GenerationInfo>> {
    read_current(repo)
}

/// Durable record of a committed import, stored in the committed generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReceipt {
    /// SHA-256 of the imported archive; `None` for directory sources, which
    /// have no stable identity and store no receipt.
    pub archive_sha256: Option<String>,
    pub archive_name: Option<String>,
    /// Generation that published this import.
    pub generation: String,
    pub committed: String,
    /// Publishers selected for this import.
    pub publishers: Vec<String>,
    /// True when every publisher in the source was selected.
    pub all_publishers: bool,
    /// FMRIs newly published by this import.
    pub added: Vec<String>,
    /// FMRIs already present with identical manifests.
    pub skipped: Vec<String>,
}

impl ImportReceipt {
    /// Whether this receipt already covers an import of the same archive with
    /// the given publisher selection.
    ///
    /// An explicit selection is checked name by name even when every source
    /// publisher was imported: a name the archive does not hold is not
    /// covered, so the import goes on to reject it as unknown.
    pub fn covers(&self, selection: Option<&[String]>) -> bool {
        match selection {
            None => self.all_publishers,
            Some(sel) => !sel.is_empty() && sel.iter().all(|p| self.publishers.contains(p)),
        }
    }
}

/// Exclusive repository writer lock; released on drop.
pub(crate) struct WriterLock {
    _file: File,
}

impl WriterLock {
    pub(crate) fn acquire(repo: &Path, wait: bool) -> Result<Self> {
        let path = repo.join(WRITER_LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| GenerationError::Lock {
                path: path.clone(),
                source,
            })?;
        if wait {
            file.lock().map_err(|source| GenerationError::Lock {
                path: path.clone(),
                source,
            })?;
        } else {
            match file.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => {
                    return Err(GenerationError::Busy {
                        path: repo.to_path_buf(),
                    });
                }
                Err(TryLockError::Error(source)) => {
                    return Err(GenerationError::Lock { path, source });
                }
            }
        }
        Ok(Self { _file: file })
    }
}

/// Take the writer lock for a write that modifies live metadata in place
/// (legacy transactions, in-place catalog rebuild, config edits of an
/// unmanaged repository). Fails before any mutation if the repository
/// publishes through generations, including when it was adopted while this
/// writer waited for the lock.
pub(crate) fn lock_unmanaged(repo: &Path, operation: &str) -> Result<WriterLock> {
    if GenerationStore::is_managed(repo) {
        return Err(GenerationError::ManagedRepository {
            path: repo.to_path_buf(),
            operation: operation.to_string(),
        });
    }
    let lock = WriterLock::acquire(repo, true)?;
    if GenerationStore::is_managed(repo) {
        return Err(GenerationError::ManagedRepository {
            path: repo.to_path_buf(),
            operation: operation.to_string(),
        });
    }
    Ok(lock)
}

/// pkg5's publisher prefix rule, which also keeps names safe as a single
/// path component.
pub fn is_valid_publisher_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('.')
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// Handle on a generation-managed repository, holding the writer lock.
///
/// Opening a store runs crash recovery: leftover staging directories are
/// removed, pre-generation directories are adopted, publisher links and
/// configuration are reconciled with the current generation.
pub struct GenerationStore {
    repo: PathBuf,
    current: GenerationInfo,
    /// Publishers whose live real directories were copied into the current
    /// generation by this store; only those directories may be replaced.
    adopted: BTreeSet<String>,
    _lock: WriterLock,
}

impl GenerationStore {
    /// Whether `repo` already publishes through generations.
    pub fn is_managed(repo: &Path) -> bool {
        fs::symlink_metadata(repo.join(GENERATIONS_DIR).join(CURRENT_LINK)).is_ok()
    }

    /// Take the writer lock (waiting for other writers) and recover.
    ///
    /// An unmanaged repository is converted only if it holds no packages;
    /// see [`GenerationStore::open_with`] for adopting a populated one.
    pub fn open(repo: &Path) -> Result<Self> {
        Self::open_with(repo, true, false)
    }

    /// Take the writer lock without waiting; fails with
    /// [`GenerationError::Busy`] if another writer holds it.
    pub fn try_open(repo: &Path) -> Result<Self> {
        Self::open_with(repo, false, false)
    }

    /// Take the writer lock and recover. `adopt_existing` allows converting
    /// an unmanaged repository that already holds packages; its live
    /// directories are replaced by links, and each path is briefly absent
    /// between two renames.
    pub fn open_with(repo: &Path, wait: bool, adopt_existing: bool) -> Result<Self> {
        if !repo.join(super::REPOSITORY_CONFIG_FILENAME).exists() {
            return Err(RepositoryError::NotFound(repo.display().to_string()).into());
        }
        let lock = WriterLock::acquire(repo, wait)?;
        let current = read_current(repo)?;
        check_unregistered_live_dirs(repo, current.as_ref())?;
        let gens = repo.join(GENERATIONS_DIR);
        fs::create_dir_all(&gens).at(&gens)?;
        remove_leftovers(&gens)?;

        let mut store = match current {
            Some(info) => Self {
                repo: repo.to_path_buf(),
                current: info,
                adopted: BTreeSet::new(),
                _lock: lock,
            },
            None => {
                // First use: adopt the live directories into generation zero.
                if !adopt_existing && has_live_packages(repo)? {
                    return Err(GenerationError::AdoptionRequired {
                        path: repo.to_path_buf(),
                    });
                }
                let info = GenerationInfo {
                    id: String::new(),
                    parent: None,
                    created: String::new(),
                    reason: GenerationReason::Adopt { publishers: vec![] },
                    publishers: vec![],
                    added_publishers: vec![],
                    catalog_mtime: 0,
                    default_publisher: None,
                };
                Self {
                    repo: repo.to_path_buf(),
                    current: info,
                    adopted: BTreeSet::new(),
                    _lock: lock,
                }
            }
        };
        store.adopt_live_directories()?;
        store.reconcile()?;
        store.gc()?;
        Ok(store)
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// Metadata of the published generation.
    pub fn current(&self) -> &GenerationInfo {
        &self.current
    }

    /// Directory of the published generation.
    pub fn current_dir(&self) -> PathBuf {
        self.repo.join(GENERATIONS_DIR).join(&self.current.id)
    }

    /// Path of a manifest in the published generation.
    pub fn current_manifest_path(&self, publisher: &str, stem: &str, version: &str) -> PathBuf {
        FileBackend::construct_manifest_path(&self.current_dir(), publisher, stem, version)
    }

    /// Receipt of a committed import of the archive with this SHA-256.
    pub fn find_receipt(&self, archive_sha256: &str) -> Result<Option<ImportReceipt>> {
        let path = self
            .current_dir()
            .join(RECEIPTS_DIR)
            .join(format!("{}.json", archive_sha256));
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                GenerationError::Corrupt(format!("receipt {}: {}", path.display(), e))
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).at(&path),
        }
    }

    /// A scratch directory on the repository filesystem, removed by the
    /// next writer if the caller crashes before cleaning it up.
    pub fn scratch_dir(&self) -> Result<PathBuf> {
        let dir = self.repo.join(GENERATIONS_DIR).join(format!(
            "{}{}",
            INCOMING_PREFIX,
            new_generation_id()
        ));
        fs::create_dir_all(&dir).at(&dir)?;
        Ok(dir)
    }

    /// Stage a new generation as a copy of the current one. Catalogs of
    /// `rebuild` publishers start empty and are rebuilt on commit.
    pub fn stage(&self, reason: GenerationReason, rebuild: &[String]) -> Result<StagedGeneration> {
        let id = unique_generation_id(&self.repo.join(GENERATIONS_DIR));
        let dir = self
            .repo
            .join(GENERATIONS_DIR)
            .join(format!("{}{}", STAGING_PREFIX, id));
        fs::create_dir_all(dir.join("publisher")).at(&dir)?;
        fs::create_dir_all(dir.join(RECEIPTS_DIR)).at(&dir)?;

        let mut staged = StagedGeneration {
            dir,
            info: GenerationInfo {
                id: id.clone(),
                parent: (!self.current.id.is_empty()).then(|| self.current.id.clone()),
                created: now_basic(),
                reason,
                publishers: vec![],
                added_publishers: vec![],
                catalog_mtime: self.current.catalog_mtime,
                default_publisher: self.current.default_publisher.clone(),
            },
            rebuild: BTreeSet::new(),
            new_files: vec![],
            parent_dir: (!self.current.id.is_empty()).then(|| self.current_dir()),
        };

        if !self.current.id.is_empty() {
            let cur = self.current_dir();
            for publisher in &self.current.publishers {
                let src = cur.join("publisher").join(publisher);
                let dst = staged.dir.join("publisher").join(publisher);
                clone_tree(&src.join("pkg"), &dst.join("pkg"))?;
                if rebuild.contains(publisher) {
                    fs::create_dir_all(dst.join("catalog")).at(&dst)?;
                    fs::create_dir_all(dst.join("catalog2")).at(&dst)?;
                } else {
                    clone_files(&src.join("catalog"), &dst.join("catalog"))?;
                    clone_files(&src.join("catalog2"), &dst.join("catalog2"))?;
                }
                staged.info.publishers.push(publisher.clone());
            }
            clone_files(&cur.join(RECEIPTS_DIR), &staged.dir.join(RECEIPTS_DIR))?;
        }
        for publisher in rebuild {
            staged.ensure_publisher(publisher)?;
            staged.rebuild.insert(publisher.clone());
        }
        Ok(staged)
    }

    /// Build the staged generation's catalogs, make it durable and publish it
    /// with an atomic switch of `generations/current`.
    pub fn commit(
        &mut self,
        staged: StagedGeneration,
        fail_point: Option<FailPoint>,
    ) -> Result<GenerationInfo> {
        let mut staged = staged;
        let previous: HashSet<String> = self.current.publishers.iter().cloned().collect();
        staged.info.added_publishers = staged
            .info
            .publishers
            .iter()
            .filter(|p| !previous.contains(*p))
            .cloned()
            .collect();
        // The default publisher switches with the publisher list.
        let keep = staged
            .info
            .default_publisher
            .as_ref()
            .is_some_and(|d| staged.info.publishers.contains(d));
        if !keep {
            staged.info.default_publisher = staged.info.publishers.first().cloned();
        }
        staged.build_catalogs()?;
        let info = staged.finalize(&self.repo.join(GENERATIONS_DIR))?;

        // Links for publishers new in this generation dangle until the
        // switch. Real directories are replaced only after it, so their
        // content stays reachable throughout.
        for publisher in &info.publishers {
            ensure_publisher_links(&self.repo, publisher, false)?;
        }

        if fail_point == Some(FailPoint::BeforeSwitch) {
            return Err(GenerationError::InjectedFailure(FailPoint::BeforeSwitch));
        }

        switch_current(&self.repo.join(GENERATIONS_DIR), &info.id)?;
        info!(generation = %info.id, "published repository generation");
        self.current = info.clone();

        if fail_point == Some(FailPoint::AfterSwitch) {
            return Err(GenerationError::InjectedFailure(FailPoint::AfterSwitch));
        }

        self.reconcile()?;
        self.gc()?;
        Ok(info)
    }

    /// Add an empty publisher in a new generation.
    pub fn add_publisher(&mut self, publisher: &str) -> Result<()> {
        if !is_valid_publisher_name(publisher) {
            return Err(GenerationError::InvalidPublisher(publisher.to_string()));
        }
        if self.current.publishers.iter().any(|p| p == publisher) {
            return Ok(());
        }
        let staged = self.stage(
            GenerationReason::AddPublisher {
                publisher: publisher.to_string(),
            },
            &[publisher.to_string()],
        )?;
        self.commit(staged, None)?;
        Ok(())
    }

    /// Remove a publisher's packages and catalogs in a new generation. Its
    /// payloads in `publisher/<pub>/file` are kept.
    pub fn remove_publisher(&mut self, publisher: &str) -> Result<()> {
        if !self.current.publishers.iter().any(|p| p == publisher) {
            return Err(RepositoryError::PublisherNotFound(publisher.to_string()).into());
        }
        let mut staged = self.stage(
            GenerationReason::RemovePublisher {
                publisher: publisher.to_string(),
            },
            &[],
        )?;
        staged.remove_publisher(publisher)?;
        self.commit(staged, None)?;
        Ok(())
    }

    /// Rebuild catalogs (legacy v1 parts and SQLite shards) of `publishers`
    /// in a new generation and publish it atomically.
    pub fn rebuild(&mut self, publishers: &[String]) -> Result<GenerationInfo> {
        let staged = self.stage(
            GenerationReason::Rebuild {
                publishers: publishers.to_vec(),
            },
            publishers,
        )?;
        self.commit(staged, None)
    }

    /// Put live real directories (pre-generation layout, or directories a
    /// legacy tool created) into a new generation and replace them by links.
    fn adopt_live_directories(&mut self) -> Result<()> {
        let config = load_config(&self.repo)?;
        let mut candidates: BTreeSet<String> = config.publishers.iter().cloned().collect();
        candidates.extend(self.current.publishers.iter().cloned());

        let first_use = self.current.id.is_empty();
        let mut adopt: Vec<String> = Vec::new();
        for p in candidates {
            let pdir = self.repo.join("publisher").join(&p);
            let has_real = GENERATION_LINKED_DIRS
                .iter()
                .any(|d| is_real_dir(&pdir.join(d)));
            if self.current.publishers.contains(&p) {
                // The published generation is authoritative for its
                // publishers. A real directory is accepted only as the
                // leftover of the adoption that produced this generation
                // (crash before the links replaced it); anything else could
                // roll published content back, so refuse to touch it.
                if has_real {
                    check_live_dir_superseded(
                        &p,
                        &pdir,
                        &self.current_dir().join("publisher").join(&p),
                    )?;
                    self.adopted.insert(p);
                }
                continue;
            }
            if has_real || first_use {
                adopt.push(p);
            }
        }
        if adopt.is_empty() && !first_use {
            return Ok(());
        }

        info!(publishers = ?adopt, "adopting live repository metadata into a generation");
        // First use keeps the catalogs readers already see. Directories that
        // appear later (written by an older tool) get their catalogs rebuilt
        // from the adopted manifests.
        let rebuild: &[String] = if first_use { &[] } else { &adopt };
        let mut staged = self.stage(
            GenerationReason::Adopt {
                publishers: adopt.clone(),
            },
            rebuild,
        )?;
        for publisher in &adopt {
            let live = self.repo.join("publisher").join(publisher);
            let dst = staged.dir.join("publisher").join(publisher);
            for d in GENERATION_LINKED_DIRS {
                if is_real_dir(&live.join(d)) {
                    let target = dst.join(d);
                    if target.exists() {
                        fs::remove_dir_all(&target).at(&target)?;
                    }
                    if d == "pkg" {
                        clone_tree(&live.join(d), &target)?;
                    } else if first_use {
                        clone_files(&live.join(d), &target)?;
                    } else {
                        fs::create_dir_all(&target).at(&target)?;
                    }
                }
            }
            staged.ensure_publisher(publisher)?;
        }
        if first_use {
            staged.info.default_publisher = config.default_publisher.clone();
        }
        self.adopted.extend(adopt);
        self.commit(staged, None)?;
        Ok(())
    }

    /// Make links and configuration agree with the current generation, the
    /// single source of truth for which publishers exist. Idempotent; runs
    /// after every switch and during recovery.
    fn reconcile(&mut self) -> Result<()> {
        if self.current.id.is_empty() {
            return Ok(());
        }
        let published: BTreeSet<String> = self.current.publishers.iter().cloned().collect();
        for publisher in &published {
            let replace = self.adopted.contains(publisher);
            ensure_publisher_links(&self.repo, publisher, replace)?;
        }
        // Links of publishers that are no longer (or not yet) published.
        let pub_root = self.repo.join("publisher");
        if let Ok(entries) = fs::read_dir(&pub_root) {
            for entry in entries {
                let entry = entry.at(&pub_root)?;
                let name = entry.file_name().to_string_lossy().to_string();
                if !published.contains(&name) && entry.file_type().at(&pub_root)?.is_dir() {
                    remove_publisher_links(&self.repo, &name)?;
                }
            }
        }

        // Compare against the file as written, not the generation view
        // FileBackend::open derives, so legacy readers of the file converge.
        let mut backend = FileBackend::open(&self.repo)?;
        backend.config = FileBackend::read_config_file(&self.repo)?;
        let (publishers, default) = self.current.publisher_view(
            &backend.config.publishers,
            backend.config.default_publisher.as_deref(),
        );
        if publishers != backend.config.publishers || default != backend.config.default_publisher {
            debug!(
                ?publishers,
                ?default,
                "reconciling repository configuration"
            );
            backend.config.publishers = publishers;
            backend.config.default_publisher = default;
            backend.save_config()?;
        }
        for publisher in &published {
            let p5i = pub_root.join(publisher).join("pub.p5i");
            if !p5i.exists() {
                backend.write_pub_p5i(publisher)?;
            }
        }
        Ok(())
    }

    /// Delete generations that are neither current nor among the retained
    /// predecessors, plus any staging leftovers.
    fn gc(&self) -> Result<()> {
        let gens = self.repo.join(GENERATIONS_DIR);
        let mut keep: HashSet<String> = HashSet::new();
        let mut cursor = Some(self.current.id.clone()).filter(|id| !id.is_empty());
        let mut depth = 0;
        while let Some(id) = cursor {
            keep.insert(id.clone());
            if depth == RETAINED_PREVIOUS_GENERATIONS {
                break;
            }
            depth += 1;
            cursor = read_info(&gens.join(&id)).ok().and_then(|i| i.parent);
        }
        for entry in fs::read_dir(&gens).at(&gens)? {
            let entry = entry.at(&gens)?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name == CURRENT_LINK || keep.contains(&name) {
                continue;
            }
            if name.starts_with(INCOMING_PREFIX) {
                // Scratch space of a live operation holding this lock.
                continue;
            }
            debug!(generation = %name, "removing superseded generation");
            remove_path(&entry.path())?;
        }
        Ok(())
    }
}

/// A generation being assembled in `generations/.staging-<id>`.
///
/// Its directory mirrors the repository layout, so [`FileBackend`] catalog
/// builders run against it unchanged.
pub struct StagedGeneration {
    dir: PathBuf,
    info: GenerationInfo,
    rebuild: BTreeSet<String>,
    new_files: Vec<PathBuf>,
    /// The generation this one replaces.
    parent_dir: Option<PathBuf>,
}

impl StagedGeneration {
    pub fn id(&self) -> &str {
        &self.info.id
    }

    /// Root of the staged tree (mirrors the repository layout).
    pub fn root(&self) -> &Path {
        &self.dir
    }

    pub fn has_publisher(&self, publisher: &str) -> bool {
        self.info.publishers.iter().any(|p| p == publisher)
    }

    /// Add `publisher` (empty) if the generation lacks it.
    pub fn ensure_publisher(&mut self, publisher: &str) -> Result<()> {
        let pdir = self.dir.join("publisher").join(publisher);
        for d in GENERATION_LINKED_DIRS {
            fs::create_dir_all(pdir.join(d)).at(&pdir)?;
        }
        if !self.has_publisher(publisher) {
            self.info.publishers.push(publisher.to_string());
            self.info.publishers.sort();
        }
        Ok(())
    }

    /// Drop `publisher` from this generation.
    pub fn remove_publisher(&mut self, publisher: &str) -> Result<()> {
        let pdir = self.dir.join("publisher").join(publisher);
        if pdir.exists() {
            fs::remove_dir_all(&pdir).at(&pdir)?;
        }
        self.info.publishers.retain(|p| p != publisher);
        self.rebuild.remove(publisher);
        self.drop_publisher_from_receipts(publisher)
    }

    /// A receipt says its publishers' packages are published. After removing
    /// a publisher that is no longer true, so narrow every receipt naming it
    /// (dropping receipts left without publishers); re-importing the same
    /// archive then publishes the publisher again instead of being a no-op.
    fn drop_publisher_from_receipts(&mut self, publisher: &str) -> Result<()> {
        let dir = self.dir.join(RECEIPTS_DIR);
        let prefix = format!("pkg://{}/", publisher);
        for entry in fs::read_dir(&dir).at(&dir)? {
            let path = entry.at(&dir)?.path();
            let bytes = fs::read(&path).at(&path)?;
            let mut receipt: ImportReceipt = serde_json::from_slice(&bytes).map_err(|e| {
                GenerationError::Corrupt(format!("receipt {}: {}", path.display(), e))
            })?;
            if !receipt.publishers.iter().any(|p| p == publisher) {
                continue;
            }
            // Hard-linked from the parent: replace, never rewrite in place.
            fs::remove_file(&path).at(&path)?;
            self.new_files.retain(|f| f != &path);
            receipt.publishers.retain(|p| p != publisher);
            if receipt.publishers.is_empty() {
                continue;
            }
            receipt.all_publishers = false;
            receipt.added.retain(|f| !f.starts_with(&prefix));
            receipt.skipped.retain(|f| !f.starts_with(&prefix));
            self.put_receipt(&receipt)?;
        }
        Ok(())
    }

    /// Write a new manifest. Existing manifests are never overwritten: they
    /// are hard links shared with committed generations.
    pub fn add_manifest(
        &mut self,
        publisher: &str,
        stem: &str,
        version: &str,
        content: &[u8],
    ) -> Result<()> {
        self.ensure_publisher(publisher)?;
        self.rebuild.insert(publisher.to_string());
        let path = FileBackend::construct_manifest_path(&self.dir, publisher, stem, version);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).at(parent)?;
        }
        write_new_file(&path, content)?;
        self.new_files.push(path);
        Ok(())
    }

    /// Record an import receipt in this generation.
    pub fn put_receipt(&mut self, receipt: &ImportReceipt) -> Result<()> {
        let Some(sha) = &receipt.archive_sha256 else {
            return Ok(());
        };
        let path = self.dir.join(RECEIPTS_DIR).join(format!("{}.json", sha));
        // A receipt for a narrower publisher selection may be linked in from
        // the parent; replace the link, never the shared inode.
        if fs::symlink_metadata(&path).is_ok() {
            fs::remove_file(&path).at(&path)?;
        }
        let json = serde_json::to_vec_pretty(receipt)
            .map_err(|e| GenerationError::Corrupt(format!("serialising receipt: {}", e)))?;
        write_new_file(&path, &json)?;
        self.new_files.push(path);
        Ok(())
    }

    fn build_catalogs(&mut self) -> Result<()> {
        if self.rebuild.is_empty() {
            return Ok(());
        }
        let backend = FileBackend::for_generation_root(&self.dir, self.info.publishers.clone());
        for publisher in &self.rebuild {
            info!(publisher, generation = %self.info.id, "building catalogs");
            let pdir = self.dir.join("publisher").join(publisher);
            for d in ["catalog", "catalog2"] {
                let path = pdir.join(d);
                // Never rewrite files hard-linked from a committed generation.
                if path.exists() {
                    fs::remove_dir_all(&path).at(&path)?;
                }
                fs::create_dir_all(&path).at(&path)?;
            }
            backend.rebuild_catalog(publisher, false)?;
            super::sqlite_catalog::build_shards(
                &pdir.join("catalog"),
                publisher,
                &pdir.join("catalog2"),
            )
            .map_err(|e| {
                RepositoryError::Other(format!(
                    "failed to build catalog shards for {}: {}",
                    publisher, e.message
                ))
            })?;
            // HTTP Last-Modified has one-second resolution and pkg5 sends
            // If-Modified-Since for catalog.attrs. Make the new catalog
            // strictly newer, in whole seconds, than the one it replaces, so
            // two publications within a second never yield a false 304.
            let mtime = self.catalog_mtime(publisher);
            self.info.catalog_mtime = self.info.catalog_mtime.max(secs(mtime));
            for d in ["catalog", "catalog2"] {
                let path = pdir.join(d);
                for entry in fs::read_dir(&path).at(&path)? {
                    let entry = entry.at(&path)?;
                    if entry.file_type().at(&path)?.is_file() {
                        let file = OpenOptions::new()
                            .write(true)
                            .open(entry.path())
                            .at(&entry.path())?;
                        file.set_modified(mtime).at(&entry.path())?;
                        self.new_files.push(entry.path());
                    }
                }
            }
        }
        Ok(())
    }

    /// max(now, previous catalog mtime + 1 s), truncated to whole seconds.
    fn catalog_mtime(&self, publisher: &str) -> SystemTime {
        let now_secs = secs(SystemTime::now());
        let prev_secs = self
            .parent_dir
            .as_ref()
            .map(|d| {
                ["catalog/catalog.attrs", "catalog2/catalog.attrs"]
                    .iter()
                    .filter_map(|f| {
                        fs::metadata(d.join("publisher").join(publisher).join(f))
                            .and_then(|m| m.modified())
                            .ok()
                    })
                    .map(secs)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
            // Generations before the high-water mark existed record 0.
            .max(self.info.catalog_mtime);
        UNIX_EPOCH + Duration::from_secs(now_secs.max(prev_secs + 1))
    }

    /// Write generation.json, make everything durable and move the staged
    /// tree to its final name.
    fn finalize(mut self, gens: &Path) -> Result<GenerationInfo> {
        let info_path = self.dir.join(GENERATION_INFO_FILE);
        let json = serde_json::to_vec_pretty(&self.info)
            .map_err(|e| GenerationError::Corrupt(format!("serialising generation: {}", e)))?;
        write_new_file(&info_path, &json)?;
        self.new_files.push(info_path);

        for file in &self.new_files {
            fsync_path(file)?;
        }
        fsync_dirs_recursive(&self.dir)?;

        let final_dir = gens.join(&self.info.id);
        fs::rename(&self.dir, &final_dir).at(&final_dir)?;
        fsync_path(gens)?;
        Ok(self.info)
    }
}

/// Read the published generation, if the repository is managed.
fn read_current(repo: &Path) -> Result<Option<GenerationInfo>> {
    let link = repo.join(GENERATIONS_DIR).join(CURRENT_LINK);
    let target = match fs::read_link(&link) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).at(&link),
    };
    let id = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| GenerationError::Corrupt(format!("{} has no target", link.display())))?;
    let info = read_info(&repo.join(GENERATIONS_DIR).join(&id))?;
    if info.id != id {
        return Err(GenerationError::Corrupt(format!(
            "generation {} records id {}",
            id, info.id
        )));
    }
    Ok(Some(info))
}

fn read_info(dir: &Path) -> Result<GenerationInfo> {
    let path = dir.join(GENERATION_INFO_FILE);
    let bytes = fs::read(&path).at(&path)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| GenerationError::Corrupt(format!("{}: {}", path.display(), e)))
}

fn load_config(repo: &Path) -> Result<RepositoryConfig> {
    Ok(FileBackend::open(repo)?.config)
}

/// Remove staging directories and temporary links left by a crashed writer.
fn remove_leftovers(gens: &Path) -> Result<()> {
    for entry in fs::read_dir(gens).at(gens)? {
        let entry = entry.at(gens)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(STAGING_PREFIX)
            || name.starts_with(INCOMING_PREFIX)
            || name.starts_with(CURRENT_TMP_PREFIX)
        {
            warn!(path = %entry.path().display(), "removing leftover of an interrupted repository update");
            remove_path(&entry.path())?;
        }
    }
    Ok(())
}

fn switch_current(gens: &Path, id: &str) -> Result<()> {
    let tmp = gens.join(format!("{}{}", CURRENT_TMP_PREFIX, id));
    if fs::symlink_metadata(&tmp).is_ok() {
        fs::remove_file(&tmp).at(&tmp)?;
    }
    symlink(id, &tmp).at(&tmp)?;
    let link = gens.join(CURRENT_LINK);
    fs::rename(&tmp, &link).at(&link)?;
    fsync_path(gens)
}

/// Make `publisher/<pub>/{pkg,catalog,catalog2}` links through
/// `generations/current`. Real directories are replaced only when
/// `replace_real_dirs` is set, i.e. their content was adopted into the
/// published generation; otherwise they are left for the next recovery,
/// which replaces them only if the generation already holds their content.
fn ensure_publisher_links(repo: &Path, publisher: &str, replace_real_dirs: bool) -> Result<()> {
    if !is_valid_publisher_name(publisher) {
        return Err(GenerationError::InvalidPublisher(publisher.to_string()));
    }
    let pdir = repo.join("publisher").join(publisher);
    let created = !pdir.exists();
    fs::create_dir_all(pdir.join("file")).at(&pdir)?;
    if created {
        fsync_path(&repo.join("publisher"))?;
    }
    for d in GENERATION_LINKED_DIRS {
        let link = pdir.join(d);
        let target = Path::new("../..")
            .join(GENERATIONS_DIR)
            .join(CURRENT_LINK)
            .join("publisher")
            .join(publisher)
            .join(d);
        let parked = pdir.join(format!(".{}.pre-generation", d));
        match fs::symlink_metadata(&link) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if fs::read_link(&link).at(&link)? != target {
                    replace_symlink(&link, &target)?;
                }
            }
            Ok(meta) if meta.is_dir() && !replace_real_dirs => {
                warn!(path = %link.display(), "leaving unadopted directory in place");
                continue;
            }
            Ok(meta) if meta.is_dir() => {
                // rename(2) cannot replace a directory by a symlink, so park
                // the directory first; the path is absent only between the
                // two renames.
                let tmp = pdir.join(format!(".{}.gen-link", d));
                if fs::symlink_metadata(&tmp).is_ok() {
                    fs::remove_file(&tmp).at(&tmp)?;
                }
                symlink(&target, &tmp).at(&tmp)?;
                if parked.exists() {
                    fs::remove_dir_all(&parked).at(&parked)?;
                }
                fs::rename(&link, &parked).at(&link)?;
                fs::rename(&tmp, &link).at(&link)?;
            }
            Ok(_) => {
                return Err(GenerationError::Corrupt(format!(
                    "{} is neither a directory nor a symlink",
                    link.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => replace_symlink(&link, &target)?,
            Err(e) => return Err(e).at(&link),
        }
        if fs::symlink_metadata(&parked).is_ok() {
            remove_path(&parked)?;
        }
    }
    fsync_path(&pdir)
}

/// Remove the generation links (and pub.p5i) of a publisher that is not
/// published. Real directories and the payload store are left alone.
fn remove_publisher_links(repo: &Path, publisher: &str) -> Result<()> {
    let pdir = repo.join("publisher").join(publisher);
    let mut changed = false;
    for d in GENERATION_LINKED_DIRS {
        let link = pdir.join(d);
        if fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()) {
            fs::remove_file(&link).at(&link)?;
            changed = true;
        }
    }
    if changed {
        let p5i = pdir.join("pub.p5i");
        if p5i.exists() {
            fs::remove_file(&p5i).at(&p5i)?;
        }
        fsync_path(&pdir)?;
    }
    Ok(())
}

/// Whether any publisher directory of an unmanaged repository holds a
/// manifest.
fn has_live_packages(repo: &Path) -> Result<bool> {
    let pub_root = repo.join("publisher");
    let entries = match fs::read_dir(&pub_root) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).at(&pub_root),
    };
    for entry in entries {
        let pkg = entry.at(&pub_root)?.path().join("pkg");
        for e in walkdir::WalkDir::new(&pkg).follow_links(false) {
            match e {
                Ok(e) if e.file_type().is_file() => return Ok(true),
                _ => {}
            }
        }
    }
    Ok(false)
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn replace_symlink(link: &Path, target: &Path) -> Result<()> {
    let name = link
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let tmp = link.with_file_name(format!(".{}.gen-link", name));
    if fs::symlink_metadata(&tmp).is_ok() {
        fs::remove_file(&tmp).at(&tmp)?;
    }
    symlink(target, &tmp).at(&tmp)?;
    fs::rename(&tmp, link).at(link)
}

/// Refuse real `pkg/`, `catalog/` or `catalog2/` directories of publishers
/// that neither the published generation nor the configuration names.
/// Adoption only considers those publishers, and the links of a publisher
/// published later never replace a real directory they did not adopt, so
/// such a directory would keep being served in place of the generation.
/// Runs before recovery mutates anything.
fn check_unregistered_live_dirs(repo: &Path, current: Option<&GenerationInfo>) -> Result<()> {
    let pub_root = repo.join("publisher");
    let entries = match fs::read_dir(&pub_root) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).at(&pub_root),
    };
    let mut known: BTreeSet<String> = load_config(repo)?.publishers.into_iter().collect();
    if let Some(info) = current {
        known.extend(info.publishers.iter().cloned());
    }
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.at(&pub_root)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if known.contains(&name) {
            continue;
        }
        for d in GENERATION_LINKED_DIRS {
            let path = entry.path().join(d);
            if is_real_dir(&path) {
                paths.push(path);
            }
        }
    }
    if paths.is_empty() {
        Ok(())
    } else {
        paths.sort();
        Err(GenerationError::UnregisteredLiveDirectory { paths })
    }
}

/// Most diverging entries [`GenerationError::UnexpectedLiveDirectory`] lists.
const MAX_REPORTED_DIVERGENCES: usize = 20;

/// Succeed only if replacing the real directories of the published
/// `publisher` at `live` by generation links loses nothing: every entry of
/// a real `pkg/` must be a regular file present with identical content in
/// `generation` (the publisher's directory in the current generation).
/// `catalog/` and `catalog2/` are derived from the manifests and carry no
/// content of their own.
fn check_live_dir_superseded(publisher: &str, live: &Path, generation: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let pkg = live.join("pkg");
    if !is_real_dir(&pkg) {
        return Ok(());
    }
    let mut diverged = Vec::new();
    for entry in walkdir::WalkDir::new(&pkg).follow_links(false).min_depth(1) {
        let entry = entry.map_err(|e| GenerationError::Io {
            path: pkg.clone(),
            source: e.into(),
        })?;
        let rel = entry.path().strip_prefix(live).unwrap_or(entry.path());
        let ft = entry.file_type();
        if ft.is_dir() {
            continue;
        }
        if !ft.is_file() {
            diverged.push(format!("{}: not a regular file", rel.display()));
        } else {
            let published = generation.join(rel);
            let live_meta = entry.metadata().map_err(|e| GenerationError::Io {
                path: entry.path().to_path_buf(),
                source: e.into(),
            })?;
            match fs::symlink_metadata(&published) {
                // Adoption hard-links manifests, so the leftover of this
                // generation's own adoption shares inodes with it.
                Ok(m) if m.dev() == live_meta.dev() && m.ino() == live_meta.ino() => {}
                Ok(m) if m.is_file() => {
                    if fs::read(entry.path()).at(entry.path())?
                        != fs::read(&published).at(&published)?
                    {
                        diverged.push(format!(
                            "{}: differs from the published manifest",
                            rel.display()
                        ));
                    }
                }
                Ok(_) => diverged.push(format!(
                    "{}: not a manifest in the published generation",
                    rel.display()
                )),
                Err(e) if e.kind() == io::ErrorKind::NotFound => diverged.push(format!(
                    "{}: not in the published generation",
                    rel.display()
                )),
                Err(e) => return Err(e).at(&published),
            }
        }
        if diverged.len() >= MAX_REPORTED_DIVERGENCES {
            diverged.push("...".to_string());
            break;
        }
    }
    if diverged.is_empty() {
        Ok(())
    } else {
        Err(GenerationError::UnexpectedLiveDirectory {
            path: pkg,
            publisher: publisher.to_string(),
            diverged,
        })
    }
}

fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.is_dir())
        .unwrap_or(false)
}

/// Recreate the directory structure of `src` at `dst`, hard-linking files.
fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).at(dst)?;
    let entries = match fs::read_dir(src) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).at(src),
    };
    for entry in entries {
        let entry = entry.at(src)?;
        let ft = entry.file_type().at(src)?;
        let target = dst.join(entry.file_name());
        if ft.is_dir() {
            clone_tree(&entry.path(), &target)?;
        } else if ft.is_file() {
            link_or_copy(&entry.path(), &target)?;
        } else {
            warn!(path = %entry.path().display(), "skipping non-regular entry while cloning generation");
        }
    }
    Ok(())
}

/// Hard-link the regular files of `src` (non-recursive) into `dst`.
fn clone_files(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).at(dst)?;
    let entries = match fs::read_dir(src) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).at(src),
    };
    for entry in entries {
        let entry = entry.at(src)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.file_type().at(src)?.is_file() && !name.ends_with(".tmp") {
            link_or_copy(&entry.path(), &dst.join(&name))?;
        }
    }
    Ok(())
}

fn link_or_copy(src: &Path, dst: &Path) -> Result<()> {
    if fs::hard_link(src, dst).is_err() {
        fs::copy(src, dst).at(dst)?;
        fsync_path(dst)?;
    }
    Ok(())
}

/// Create `path` exclusively and write `content` durably.
pub(crate) fn write_new_file(path: &Path, content: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .at(path)?;
    f.write_all(content).at(path)?;
    f.sync_all().at(path)
}

pub(crate) fn fsync_path(path: &Path) -> Result<()> {
    File::open(path).at(path)?.sync_all().at(path)
}

fn fsync_dirs_recursive(root: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|e| GenerationError::Io {
            path: root.to_path_buf(),
            source: e.into(),
        })?;
        if entry.file_type().is_dir() {
            fsync_path(entry.path())?;
        }
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).at(path),
    };
    if meta.is_dir() {
        fs::remove_dir_all(path).at(path)
    } else {
        fs::remove_file(path).at(path)
    }
}

fn now_basic() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ").to_string()
}

fn new_generation_id() -> String {
    format!("{}-{}", now_basic(), std::process::id())
}

fn unique_generation_id(gens: &Path) -> String {
    loop {
        let id = new_generation_id();
        if !gens.join(&id).exists() && !gens.join(format!("{}{}", STAGING_PREFIX, id)).exists() {
            return id;
        }
        std::thread::sleep(std::time::Duration::from_micros(1));
    }
}
