//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

//! Atomic import of a pkg5 repository or `.p5p` archive.
//!
//! One archive is one publication batch: every package of every selected
//! publisher becomes visible in a single generation switch, or none does.
//!
//! 1. The archive's SHA-256 is its identity. If the current generation holds
//!    a receipt for it, the import is a no-op (this covers a crash between
//!    commit and acknowledgement).
//! 2. Every manifest is parsed and every referenced payload is located and
//!    verified against its digest before anything is written.
//! 3. A package whose exact FMRI is already published is skipped if the
//!    manifest bytes are identical, and rejects the whole batch otherwise.
//! 4. New payloads go to the shared content-addressed store (unreferenced
//!    until the switch), manifests and rebuilt catalogs to a staged
//!    generation, which is then published atomically together with the
//!    import receipt.
//!
//! Manifests are stored byte-for-byte as pkg5 published them and payloads
//! under the names the manifests reference, so pkg5 clients see exactly the
//! upstream content. `pkg.obsolete` and `pkg.renamed` packages are published
//! like any other package, as in pkg5.

use super::generation::{
    FailPoint, GenerationError, GenerationReason, GenerationStore, ImportReceipt,
};
use super::retention::universe::{Universe, check_hiding, scan_manifest, snapshot_closure};
use super::retention::{Policy, RetentionError};
use super::{FileBackend, RepositoryError, RepositoryVersion, WritableRepository};
use crate::actions::Manifest;
use crate::digest::{DigestAlgorithm, DigestSource};
use crate::fmri::Fmri;
use miette::Diagnostic;
use sha2::Digest as _;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::{debug, info, warn};

pub type Result<T> = std::result::Result<T, Pkg5ImportError>;

#[derive(Debug, Error, Diagnostic)]
pub enum Pkg5ImportError {
    #[error("{path} is not a pkg5 repository or .p5p archive")]
    #[diagnostic(
        code(ips::pkg5_import::not_pkg5),
        help("Pass a .p5p archive, or a directory containing pkg5.repository and publisher/")
    )]
    NotPkg5Source { path: PathBuf },

    #[error("failed to extract {path}: {detail}")]
    #[diagnostic(
        code(ips::pkg5_import::extract),
        help("Check that the archive is a complete .p5p (tar) file and that tar(1) is installed")
    )]
    Extract { path: PathBuf, detail: String },

    #[error("{path} exceeds the extraction limit: {detail}")]
    #[diagnostic(
        code(ips::pkg5_import::archive_limit),
        help(
            "Nothing was imported. If the archive is genuine, raise the limit (pkg6repo import-pkg5 --max-archive-entries / --max-extract-bytes)"
        )
    )]
    ArchiveLimit { path: PathBuf, detail: String },

    #[error("I/O error at {path}: {source}")]
    #[diagnostic(
        code(ips::pkg5_import::io),
        help("Check permissions and free space on the source and repository filesystems")
    )]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("publisher {publisher} is not in the source (available: {available})")]
    #[diagnostic(
        code(ips::pkg5_import::unknown_publisher),
        help("Omit --publisher to import every publisher in the archive")
    )]
    UnknownPublisher {
        publisher: String,
        available: String,
    },

    #[error("the source is invalid; nothing was imported:\n{}", format_list(.problems))]
    #[diagnostic(
        code(ips::pkg5_import::invalid_source),
        help(
            "Fix the archive (for example re-run pkgrecv -a) and retry. The repository is unchanged"
        )
    )]
    InvalidSource { problems: Vec<String> },

    #[error(
        "{} package(s) conflict with published packages; nothing was imported:\n{}",
        .conflicts.len(),
        format_list(.conflicts)
    )]
    #[diagnostic(
        code(ips::pkg5_import::conflict),
        help(
            "A published FMRI is never replaced. Publish the changed package under a new version and retry"
        )
    )]
    Conflicts { conflicts: Vec<String> },

    #[error(
        "import exclusions would leave out packages that are needed or cannot be checked ({} problem(s)); nothing was imported:\n{}\n\nSuggested keep entries (add to the policy after review):\n{}",
        .conflicts.len(),
        format_list(.conflicts),
        .suggested_keeps.iter().map(|k| format!("  keep \"{}\"", k)).collect::<Vec<_>>().join("\n")
    )]
    #[diagnostic(
        code(ips::pkg5_import::exclusion_conflict),
        help(
            "Import exclusions never silently drop a package something needs. Add the suggested exact keep entries to the policy (keep overrides exclude), or narrow the exclusion, and retry"
        )
    )]
    ExclusionConflict {
        conflicts: Vec<String>,
        suggested_keeps: Vec<String>,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Retention(#[from] RetentionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Generation(#[from] GenerationError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Repository(Box<RepositoryError>),
}

impl From<RepositoryError> for Pkg5ImportError {
    fn from(err: RepositoryError) -> Self {
        Pkg5ImportError::Repository(Box::new(err))
    }
}

fn format_list(items: &[String]) -> String {
    items
        .iter()
        .map(|i| format!("  - {}", i))
        .collect::<Vec<_>>()
        .join("\n")
}

trait IoContext<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| Pkg5ImportError::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Options for [`import_pkg5`].
#[derive(Debug, Clone)]
pub struct Pkg5ImportOptions {
    /// Publishers to import; `None` imports all publishers in the source.
    pub publishers: Option<Vec<String>>,
    /// Wait for a concurrent writer instead of failing with `Busy`.
    pub wait_for_lock: bool,
    /// Allow converting an existing, populated repository that does not use
    /// generations yet (each metadata path is briefly absent once).
    pub adopt_existing: bool,
    /// Simulate a crash at this point (failure-injection tests).
    pub fail_point: Option<FailPoint>,
    /// Most entries (files and directories) an archive may unpack.
    pub max_archive_entries: u64,
    /// Most file bytes an archive may unpack. Unpacking never writes more
    /// file data than the archive holds, so `None` bounds it by the archive
    /// size; set a limit to keep scratch space below the free space.
    pub max_extract_bytes: Option<u64>,
    /// Import policy: `import { exclude ... }` rules leave matching new
    /// packages out (unless a `keep` rule matches), after checking that no
    /// protected snapshot and no imported package needs them. `None`
    /// excludes nothing.
    pub policy: Option<Policy>,
}

/// Default for [`Pkg5ImportOptions::max_archive_entries`]: far above a full
/// OpenIndiana publication (payload files plus manifests and directories),
/// low enough to stop an archive of empty entries exhausting inodes.
pub const DEFAULT_MAX_ARCHIVE_ENTRIES: u64 = 20_000_000;

impl Default for Pkg5ImportOptions {
    fn default() -> Self {
        Self {
            publishers: None,
            wait_for_lock: true,
            adopt_existing: false,
            fail_point: None,
            max_archive_entries: DEFAULT_MAX_ARCHIVE_ENTRIES,
            max_extract_bytes: None,
            policy: None,
        }
    }
}

/// Result of a successful [`import_pkg5`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pkg5ImportOutcome {
    /// A new generation was published.
    Committed(ImportReceipt),
    /// The archive was committed before; nothing changed.
    AlreadyImported(ImportReceipt),
    /// A directory source contained only already-published packages.
    NothingToImport { skipped: Vec<String> },
}

/// A package read from the source.
struct SourcePackage {
    publisher: String,
    fmri: Fmri,
    manifest: Vec<u8>,
    payloads: Payloads,
}

/// Import `source` (a `.p5p` file or an unpacked pkg5 repository) into the
/// repository at `repo`, creating the repository if needed.
pub fn import_pkg5(
    repo: &Path,
    source: &Path,
    options: &Pkg5ImportOptions,
) -> Result<Pkg5ImportOutcome> {
    let meta = fs::metadata(source).at(source)?;
    let is_archive = meta.is_file();
    let archive_sha256 = if is_archive {
        Some(sha256_file(source)?)
    } else {
        None
    };
    ensure_repository(repo)?;

    let mut store =
        GenerationStore::open_with(repo, options.wait_for_lock, options.adopt_existing)?;

    if let Some(sha) = &archive_sha256
        && let Some(receipt) = store.find_receipt(sha)?
        && receipt.covers(options.publishers.as_deref())
        && receipt.excluded.iter().all(|f| {
            options
                .policy
                .as_ref()
                .is_some_and(|p| p.import_excludes(f))
        })
    {
        info!(archive = %sha, generation = %receipt.generation, "archive already imported");
        return Ok(Pkg5ImportOutcome::AlreadyImported(receipt));
    }

    let scratch = store.scratch_dir()?;
    let result = import_locked(
        &mut store,
        source,
        is_archive,
        archive_sha256,
        &scratch,
        options,
    );
    // A simulated crash leaves its debris for recovery to find.
    if !matches!(
        result,
        Err(Pkg5ImportError::Generation(
            GenerationError::InjectedFailure(_)
        ))
    ) && let Err(e) = fs::remove_dir_all(&scratch)
        && e.kind() != io::ErrorKind::NotFound
    {
        warn!(path = %scratch.display(), error = %e, "failed to remove import scratch directory");
    }
    result
}

fn import_locked(
    store: &mut GenerationStore,
    source: &Path,
    is_archive: bool,
    archive_sha256: Option<String>,
    scratch: &Path,
    options: &Pkg5ImportOptions,
) -> Result<Pkg5ImportOutcome> {
    let root = if is_archive {
        let root = scratch.join("archive");
        fs::create_dir_all(&root).at(&root)?;
        extract_archive(
            source,
            &root,
            options.max_archive_entries,
            options.max_extract_bytes,
        )?;
        root
    } else {
        source.to_path_buf()
    };
    validate_source_tree(&root)?;
    if !root.join("pkg5.repository").is_file() && !root.join("pkg5.index.0.gz").is_file() {
        return Err(Pkg5ImportError::NotPkg5Source {
            path: source.to_path_buf(),
        });
    }

    let available = list_publishers(&root)?;
    let selected: Vec<String> = match &options.publishers {
        Some(sel) => {
            for p in sel {
                if !available.contains(p) {
                    return Err(Pkg5ImportError::UnknownPublisher {
                        publisher: p.clone(),
                        available: available.join(", "),
                    });
                }
            }
            sel.iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        }
        None => available.clone(),
    };
    if selected.is_empty() {
        return Err(Pkg5ImportError::NotPkg5Source {
            path: source.to_path_buf(),
        });
    }

    // Phase 1: read and validate every manifest.
    let mut problems = Vec::new();
    let mut packages = Vec::new();
    for publisher in &selected {
        scan_publisher(&root, publisher, &mut packages, &mut problems)?;
    }

    // Phase 2: classify against the published generation and within the batch.
    let mut conflicts = Vec::new();
    let mut skipped = Vec::new();
    let mut to_add: Vec<SourcePackage> = Vec::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for pkg in packages {
        let key = pkg.fmri.to_string();
        if let Some(&idx) = seen.get(&key) {
            if to_add[idx].manifest != pkg.manifest {
                conflicts.push(format!(
                    "{}: appears twice in the source with different manifests",
                    key
                ));
            }
            continue;
        }
        let published =
            store.current_manifest_path(&pkg.publisher, pkg.fmri.stem(), &pkg.fmri.version());
        match fs::read(&published) {
            Ok(existing) if existing == pkg.manifest => skipped.push(key),
            Ok(_) => conflicts.push(format!(
                "{}: a different manifest is already published",
                key
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                seen.insert(key, to_add.len());
                to_add.push(pkg);
            }
            Err(e) => return Err(e).at(&published),
        }
    }
    if !problems.is_empty() {
        return Err(Pkg5ImportError::InvalidSource { problems });
    }
    if !conflicts.is_empty() {
        return Err(Pkg5ImportError::Conflicts { conflicts });
    }

    // Import exclusions: leave out new packages the policy excludes, but
    // only if nothing protected or imported needs them.
    let mut excluded: Vec<SourcePackage> = Vec::new();
    if let Some(policy) = &options.policy
        && !policy.import_exclude.is_empty()
    {
        let (out, keep): (Vec<SourcePackage>, Vec<SourcePackage>) = to_add
            .into_iter()
            .partition(|p| policy.import_excludes(&p.fmri.to_string()));
        to_add = keep;
        excluded = out;
        if !excluded.is_empty() {
            check_exclusions(store, policy, &to_add, &excluded)?;
        }
    }

    // Phase 3: every payload a new manifest references must be servable to
    // pkg5 clients, from a verified copy already in the repository or from
    // the source.
    let repo = store.repo().to_path_buf();
    let mut wanted: BTreeMap<(String, String), (Vec<String>, BTreeSet<CompressedDigest>)> =
        BTreeMap::new();
    for pkg in &to_add {
        for (hash, pinned) in &pkg.payloads {
            let (fmris, all_pinned) = wanted
                .entry((pkg.publisher.clone(), hash.clone()))
                .or_default();
            fmris.push(pkg.fmri.to_string());
            all_pinned.extend(pinned.iter().cloned());
        }
    }
    let mut moves: Vec<PayloadMove> = Vec::new();
    for ((publisher, hash), (fmris, pinned)) in &wanted {
        let who = fmris.join(", ");
        // The copy the depot serves this publisher: its own, else the
        // global one. An existing copy is reused only if pkg5 clients can
        // install it, and is never replaced: pkg6 packages may reference
        // the same digest in their own encoding (LZ4), and a publisher copy
        // added next to a global one would change what they are served.
        let dest = FileBackend::construct_file_path_with_publisher(&repo, publisher, hash);
        let global = FileBackend::construct_file_path(&repo, hash);
        if let Some(stored) = [&dest, &global].into_iter().find(|p| p.exists()) {
            if let Err(why) = check_wire_payload(stored, hash, pinned) {
                problems.push(format!(
                    "{}: payload {}: the repository already stores it at {} in a form pkg5 clients cannot install ({}); it is never replaced",
                    who,
                    hash,
                    stored.display(),
                    why
                ));
            }
            continue;
        }
        match find_source_payload(&root, publisher, hash) {
            Some(src) => match verify_source_payload(&src, hash, pinned) {
                Ok(encoding) => moves.push(PayloadMove {
                    src,
                    dest,
                    hash: hash.clone(),
                    pinned: pinned.clone(),
                    encoding,
                }),
                Err(why) => problems.push(format!("{}: payload {}: {}", who, hash, why)),
            },
            None => problems.push(format!(
                "{}: payload {} is missing from the source and the repository",
                who, hash
            )),
        }
    }
    if !problems.is_empty() {
        return Err(Pkg5ImportError::InvalidSource { problems });
    }

    if to_add.is_empty() && archive_sha256.is_none() {
        return Ok(Pkg5ImportOutcome::NothingToImport { skipped });
    }

    // Phase 4: store payloads. They stay unreferenced until the switch.
    store_payloads(&moves, is_archive)?;
    if options.fail_point == Some(FailPoint::AfterPayloads) {
        return Err(GenerationError::InjectedFailure(FailPoint::AfterPayloads).into());
    }

    // Phase 5: stage the generation and publish it.
    let touched: Vec<String> = to_add
        .iter()
        .map(|p| p.publisher.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut staged = store.stage(
        GenerationReason::Pkg5Import {
            archive_sha256: archive_sha256.clone(),
        },
        &touched,
    )?;
    let mut added = Vec::with_capacity(to_add.len());
    for pkg in &to_add {
        staged.add_manifest(
            &pkg.publisher,
            pkg.fmri.stem(),
            &pkg.fmri.version(),
            &pkg.manifest,
        )?;
        added.push(pkg.fmri.to_string());
    }
    // An earlier import of the same archive with a narrower publisher
    // selection: the new receipt covers both.
    let previous = match &archive_sha256 {
        Some(sha) => store.find_receipt(sha)?,
        None => None,
    };
    let union = |a: &[String], b: Vec<String>| -> Vec<String> {
        a.iter()
            .cloned()
            .chain(b)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    let mut excluded: Vec<String> = excluded.iter().map(|p| p.fmri.to_string()).collect();
    if let Some(prev) = &previous {
        excluded.extend(
            prev.excluded
                .iter()
                .filter(|f| !added.contains(f) && !skipped.contains(f))
                .cloned(),
        );
    }
    excluded.sort();
    excluded.dedup();
    let (publishers, added, skipped) = match &previous {
        Some(prev) => (
            union(&prev.publishers, selected.clone()),
            union(&prev.added, added),
            union(&prev.skipped, skipped),
        ),
        None => (selected.clone(), added, skipped),
    };
    let receipt = ImportReceipt {
        archive_sha256,
        archive_name: source.file_name().map(|n| n.to_string_lossy().to_string()),
        generation: staged.id().to_string(),
        committed: chrono::Utc::now().to_rfc3339(),
        all_publishers: available.iter().all(|p| publishers.contains(p)),
        publishers,
        added,
        skipped,
        excluded,
    };
    staged.put_receipt(&receipt)?;
    store.commit(staged, options.fail_point)?;
    info!(
        generation = %receipt.generation,
        added = receipt.added.len(),
        skipped = receipt.skipped.len(),
        "pkg5 import committed"
    );
    Ok(Pkg5ImportOutcome::Committed(receipt))
}

/// Fail if an excluded package is needed: by the closure of a protected
/// snapshot, or as the only satisfier of a hard dependency of a visible
/// package, whether this import publishes it or it is already published.
/// The check runs over the published generation plus every package of the
/// batch. Content whose dependencies cannot be interpreted (in the batch or
/// the repository) blocks excluding what it may need, and an excluded
/// package whose own version cannot be interpreted is never excluded
/// silently.
fn check_exclusions(
    store: &GenerationStore,
    policy: &Policy,
    imported: &[SourcePackage],
    excluded: &[SourcePackage],
) -> Result<()> {
    let info = store.current();
    let root = store.current_dir();
    let (mut universe, _) = if info.id.is_empty() {
        (Universe::default(), Default::default())
    } else {
        Universe::load(&root, &info.publishers, &Default::default(), &|_, _| false)?
    };
    let hidden_fmris = if info.id.is_empty() {
        Default::default()
    } else {
        store.retirement_state()?.hidden
    };
    let mut add = |pkg: &SourcePackage| -> Option<usize> {
        let scan = scan_manifest(&String::from_utf8_lossy(&pkg.manifest));
        universe.add_scanned(&pkg.fmri, &pkg.publisher, scan, None)
    };
    // Imported packages whose version cannot be read are kept as
    // unverifiable content by the universe.
    for pkg in imported {
        add(pkg);
    }
    let mut conflicts = Vec::new();
    let mut unreadable = Vec::new();
    let mut hide = std::collections::HashSet::new();
    for pkg in excluded {
        match add(pkg) {
            Some(i) => {
                hide.insert(i);
            }
            None => {
                conflicts.push(format!(
                    "{}: excluded, but its version is not a pkg5 version, so whether anything needs it cannot be checked",
                    pkg.fmri
                ));
                unreadable.push(pkg.fmri.to_string());
            }
        }
    }
    universe.finish();
    let hidden: std::collections::HashSet<usize> = hidden_fmris
        .keys()
        .filter_map(|f| universe.get(f))
        .collect();
    let closures: Vec<_> = policy
        .snapshots
        .iter()
        .map(|s| snapshot_closure(&universe, s))
        .collect();
    let b = check_hiding(&universe, &hidden, &hide, &closures, &|_| true, false);
    conflicts.extend(b.blockers);
    if conflicts.is_empty() {
        return Ok(());
    }
    let mut suggested_keeps: Vec<String> = b.keeps.into_iter().chain(unreadable).collect();
    suggested_keeps.sort();
    suggested_keeps.dedup();
    Err(Pkg5ImportError::ExclusionConflict {
        conflicts,
        suggested_keeps,
    })
}

fn ensure_repository(repo: &Path) -> Result<()> {
    if repo.join(super::REPOSITORY_CONFIG_FILENAME).is_file() {
        return Ok(());
    }
    info!(repo = %repo.display(), "creating repository");
    FileBackend::create(repo, RepositoryVersion::V4)?;
    Ok(())
}

/// Unpack a `.p5p` (pax tar) into `dest`, which must be a fresh directory.
///
/// Only regular files and directories with plain relative paths are
/// accepted. Links, devices, absolute paths and `..` reject the whole
/// archive, and files are created exclusively, so nothing an archive holds
/// can make the importer write or read outside `dest`.
fn extract_archive(
    archive: &Path,
    dest: &Path,
    max_entries: u64,
    max_bytes: Option<u64>,
) -> Result<()> {
    let bad = |detail: String| Pkg5ImportError::Extract {
        path: archive.to_path_buf(),
        detail,
    };
    let limit = |detail: String| Pkg5ImportError::ArchiveLimit {
        path: archive.to_path_buf(),
        detail,
    };
    let file = File::open(archive).at(archive)?;
    // Entry data is stored raw, so honest archives never unpack more file
    // bytes than their own size.
    let archive_len = file.metadata().at(archive)?.len();
    let byte_limit = max_bytes.map_or(archive_len, |m| m.min(archive_len));
    let mut entry_count: u64 = 0;
    let mut byte_count: u64 = 0;
    let mut tar = tar::Archive::new(io::BufReader::with_capacity(1 << 20, file));
    let entries = tar.entries().map_err(|e| bad(e.to_string()))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| bad(e.to_string()))?;
        let raw = entry.path().map_err(|e| bad(e.to_string()))?.into_owned();
        let mut rel = PathBuf::new();
        for component in raw.components() {
            match component {
                std::path::Component::Normal(c) => rel.push(c),
                std::path::Component::CurDir => {}
                _ => return Err(bad(format!("unsafe path {:?} in archive", raw))),
            }
        }
        let kind = entry.header().entry_type();
        entry_count += 1;
        if entry_count > max_entries {
            return Err(limit(format!("more than {} entries", max_entries)));
        }
        byte_count = byte_count.saturating_add(entry.size());
        if byte_count > byte_limit {
            return Err(limit(format!(
                "more than {} bytes of file data{}",
                byte_limit,
                if max_bytes.is_some_and(|m| m < archive_len) {
                    ""
                } else {
                    " (more than the archive holds)"
                }
            )));
        }
        match kind {
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader => continue,
            tar::EntryType::Directory => {
                if !rel.as_os_str().is_empty() {
                    let dir = dest.join(&rel);
                    fs::create_dir_all(&dir).at(&dir)?;
                }
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if rel.as_os_str().is_empty() {
                    return Err(bad(format!("unsafe path {:?} in archive", raw)));
                }
                let target = dest.join(&rel);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).at(parent)?;
                }
                let mut out = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                    .map_err(|e| bad(format!("{}: {}", rel.display(), e)))?;
                // Copy at most the declared size.
                let size = entry.size();
                let copied = io::copy(&mut (&mut entry).take(size), &mut out).at(&target)?;
                if copied != size {
                    return Err(bad(format!("{:?} is truncated", raw)));
                }
            }
            other => {
                return Err(bad(format!(
                    "{:?} has unsupported entry type {:?}; only files and directories are allowed",
                    raw, other
                )));
            }
        }
    }
    Ok(())
}

/// Reject symlinks and special files anywhere below the source's
/// `publisher/` tree. A linked directory would make the importer read
/// manifests or move payloads belonging to another tree.
fn validate_source_tree(root: &Path) -> Result<()> {
    let base = root.join("publisher");
    if fs::symlink_metadata(&base).is_ok_and(|m| !m.is_dir()) {
        return Err(Pkg5ImportError::InvalidSource {
            problems: vec!["publisher: not a directory".to_string()],
        });
    }
    let mut problems = Vec::new();
    for entry in walkdir::WalkDir::new(&base).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) if e.io_error().map(|io| io.kind()) == Some(io::ErrorKind::NotFound) => {
                continue;
            }
            Err(e) => {
                return Err(Pkg5ImportError::Io {
                    path: base.clone(),
                    source: e.into(),
                });
            }
        };
        let ft = entry.file_type();
        if !ft.is_dir() && !ft.is_file() {
            let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
            problems.push(format!(
                "{}: symbolic links and special files are not allowed",
                rel.display()
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Pkg5ImportError::InvalidSource { problems })
    }
}

/// Publisher directories of the source. A name that is not a valid pkg5
/// publisher prefix (and so could escape the repository as a path
/// component) rejects the source.
fn list_publishers(root: &Path) -> Result<Vec<String>> {
    let dir = root.join("publisher");
    let mut publishers = Vec::new();
    let mut invalid = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(publishers),
        Err(e) => return Err(e).at(&dir),
    };
    for entry in entries {
        let entry = entry.at(&dir)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !entry.file_type().at(&dir)?.is_dir() {
            continue;
        }
        if !super::generation::is_valid_publisher_name(&name) {
            invalid.push(format!("publisher/{}: not a valid publisher name", name));
            continue;
        }
        publishers.push(name);
    }
    if !invalid.is_empty() {
        return Err(Pkg5ImportError::InvalidSource { problems: invalid });
    }
    publishers.sort();
    Ok(publishers)
}

fn scan_publisher(
    root: &Path,
    publisher: &str,
    packages: &mut Vec<SourcePackage>,
    problems: &mut Vec<String>,
) -> Result<()> {
    let pkg_dir = root.join("publisher").join(publisher).join("pkg");
    let stems = match fs::read_dir(&pkg_dir) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).at(&pkg_dir),
    };
    for stem in stems {
        let stem = stem.at(&pkg_dir)?;
        if !stem.file_type().at(&pkg_dir)?.is_dir() {
            continue;
        }
        let stem_path = stem.path();
        for ver in fs::read_dir(&stem_path).at(&stem_path)? {
            let ver = ver.at(&stem_path)?;
            let path = ver.path();
            let ft = ver.file_type().at(&stem_path)?;
            if !ft.is_file() {
                problems.push(format!(
                    "{}: not a regular file",
                    path.strip_prefix(root).unwrap_or(&path).display()
                ));
                continue;
            }
            let bytes = fs::read(&path).at(&path)?;
            match read_package(publisher, &bytes) {
                Ok((fmri, payloads)) => packages.push(SourcePackage {
                    publisher: publisher.to_string(),
                    fmri,
                    manifest: bytes,
                    payloads,
                }),
                Err(why) => problems.push(format!(
                    "{}: {}",
                    path.strip_prefix(root).unwrap_or(&path).display(),
                    why
                )),
            }
        }
    }
    Ok(())
}

/// Parse a pkg5 manifest: its FMRI (qualified with `publisher`) and the
/// digests of every payload it references.
fn read_package(publisher: &str, bytes: &[u8]) -> std::result::Result<(Fmri, Payloads), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "manifest is not UTF-8".to_string())?;
    if text.trim_start().starts_with('{') {
        return Err("manifest is JSON, expected a pkg5 text manifest".to_string());
    }
    let owned = text.to_string();
    // The action parser panics on some malformed payload fields; contain it.
    let manifest = std::panic::catch_unwind(move || Manifest::parse_string(owned))
        .map_err(|_| "manifest could not be parsed".to_string())?
        .map_err(|e| format!("manifest could not be parsed: {}", e))?;

    let fmri_str = manifest
        .attributes
        .iter()
        .find(|a| a.key == "pkg.fmri")
        .and_then(|a| a.values.first())
        .ok_or_else(|| "manifest has no pkg.fmri".to_string())?;
    let mut fmri =
        Fmri::parse(fmri_str).map_err(|e| format!("invalid pkg.fmri {}: {}", fmri_str, e))?;
    if fmri.version.is_none() {
        return Err(format!("pkg.fmri {} has no version", fmri_str));
    }
    // The stem and version each become one path component in the repository.
    if matches!(fmri.stem(), "" | "." | "..") {
        return Err(format!("pkg.fmri {} has an invalid package name", fmri_str));
    }
    match &fmri.publisher {
        Some(p) if p != publisher => {
            return Err(format!(
                "pkg.fmri {} names publisher {} but is stored under {}",
                fmri_str, p, publisher
            ));
        }
        _ => fmri.publisher = Some(publisher.to_string()),
    }

    let mut payloads: Payloads = BTreeMap::new();
    let mut add = |h: &str, pinned: Vec<CompressedDigest>| -> std::result::Result<(), String> {
        if is_payload_hash(h) {
            payloads.entry(h.to_string()).or_default().extend(pinned);
            Ok(())
        } else {
            Err(format!("unsupported payload reference {:?}", h))
        }
    };
    for file in &manifest.files {
        let mut pinned: Vec<CompressedDigest> = file
            .properties
            .iter()
            .filter_map(|p| CompressedDigest::from_attr(&p.key, &p.value))
            .collect();
        if let Some(payload) = &file.payload {
            // The action parser files `chash` and `pkg.content-hash` values
            // as additional identifiers of the payload.
            pinned.extend(payload.additional_identifiers.iter().filter_map(|d| {
                match (&d.source, &d.algorithm) {
                    (DigestSource::PrimaryPayloadHash, DigestAlgorithm::SHA1) => {
                        CompressedDigest::from_attr("chash", &d.hash)
                    }
                    (DigestSource::GzipCompressed, DigestAlgorithm::SHA512Half) => {
                        CompressedDigest::from_attr(
                            "pkg.content-hash",
                            &format!("gzip:sha512t_256:{}", d.hash),
                        )
                    }
                    _ => None,
                }
            }));
            add(&payload.primary_identifier.hash, pinned)?;
        } else if let Some(h) = file.properties.iter().find(|p| p.key == "hash") {
            add(&h.value, pinned)?;
        }
    }
    for license in &manifest.licenses {
        let pinned = license
            .properties
            .values()
            .filter_map(|p| CompressedDigest::from_attr(&p.key, &p.value))
            .collect();
        if !license.payload.is_empty() {
            add(&license.payload, pinned)?;
        } else if let Some(h) = license.properties.get("hash") {
            add(&h.value, pinned)?;
        }
    }
    for signature in &manifest.signatures {
        if !signature.value.is_empty() {
            add(&signature.value, vec![])?;
        }
        for h in signature.chain.split_whitespace() {
            add(h, vec![])?;
        }
    }
    Ok((fmri, payloads))
}

/// Payloads a manifest references, by name, with the digests of their
/// compressed form it pins.
type Payloads = BTreeMap<String, BTreeSet<CompressedDigest>>;

/// A digest of a payload's compressed (gzip) bytes pinned by a manifest
/// action. pkg5 clients check it on download, so the copy a depot serves
/// must be byte-identical to the one the package was published with.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CompressedDigest {
    /// `chash=<sha1>`
    Sha1(String),
    /// `pkg.chash.sha256=<sha256>`
    Sha256(String),
    /// `pkg.content-hash=gzip:sha512t_256:<hex>`
    Sha512t256(String),
}

impl CompressedDigest {
    fn from_attr(key: &str, value: &str) -> Option<Self> {
        let hex = |v: &str, len: usize| {
            (v.len() == len && v.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| v.to_ascii_lowercase())
        };
        match key {
            "chash" => hex(value, 40).map(CompressedDigest::Sha1),
            "pkg.chash.sha256" => hex(value, 64).map(CompressedDigest::Sha256),
            "pkg.content-hash" => value
                .strip_prefix("gzip:sha512t_256:")
                .and_then(|v| hex(v, 64))
                .map(CompressedDigest::Sha512t256),
            _ => None,
        }
    }

    fn describe(&self) -> String {
        match self {
            CompressedDigest::Sha1(h) => format!("chash {}", h),
            CompressedDigest::Sha256(h) => format!("pkg.chash.sha256 {}", h),
            CompressedDigest::Sha512t256(h) => format!("gzip:sha512t_256 {}", h),
        }
    }
}

/// pkg5 payload names: lowercase hex SHA-1, SHA-256 or SHA-512 digests.
fn is_payload_hash(h: &str) -> bool {
    matches!(h.len(), 40 | 64 | 128)
        && h.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn find_source_payload(root: &Path, publisher: &str, hash: &str) -> Option<PathBuf> {
    let file_dir = root.join("publisher").join(publisher).join("file");
    [
        file_dir.join(&hash[0..2]).join(hash),
        // pkg5's original V0 layout (file_layout/layout.py).
        file_dir.join(&hash[0..2]).join(&hash[2..8]).join(hash),
    ]
    .into_iter()
    .find(|p| {
        fs::symlink_metadata(p)
            .map(|m| m.is_file())
            .unwrap_or(false)
    })
}

enum PayloadHasher {
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
}

impl PayloadHasher {
    fn for_hash(hash: &str) -> Self {
        match hash.len() {
            40 => PayloadHasher::Sha1(sha1::Sha1::new()),
            64 => PayloadHasher::Sha256(sha2::Sha256::new()),
            _ => PayloadHasher::Sha512(sha2::Sha512::new()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            PayloadHasher::Sha1(h) => h.update(data),
            PayloadHasher::Sha256(h) => h.update(data),
            PayloadHasher::Sha512(h) => h.update(data),
        }
    }

    fn hex(self) -> String {
        match self {
            PayloadHasher::Sha1(h) => format!("{:x}", h.finalize()),
            PayloadHasher::Sha256(h) => format!("{:x}", h.finalize()),
            PayloadHasher::Sha512(h) => format!("{:x}", h.finalize()),
        }
    }
}

fn hash_stream<R: Read>(reader: &mut R, hash: &str) -> io::Result<String> {
    let mut hasher = PayloadHasher::for_hash(hash);
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(hasher.hex());
        }
        hasher.update(&buf[..n]);
    }
}

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const LZ4_FRAME_MAGIC: [u8; 4] = [0x04, 0x22, 0x4d, 0x18];

fn read_magic(path: &Path) -> io::Result<[u8; 4]> {
    let mut magic = [0u8; 4];
    let mut f = File::open(path)?;
    let mut filled = 0;
    while filled < magic.len() {
        match f.read(&mut magic[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(magic)
}

/// Check that the file at `path` is what pkg5 clients expect from
/// `/file/0/<hash>`: a single gzip member (pkg5 decompresses only the first)
/// whose content has digest `hash`, and whose compressed bytes match every
/// digest the manifests `pinned`.
fn check_wire_payload(
    path: &Path,
    hash: &str,
    pinned: &BTreeSet<CompressedDigest>,
) -> std::result::Result<(), String> {
    let magic = read_magic(path).map_err(|e| e.to_string())?;
    if magic == LZ4_FRAME_MAGIC {
        return Err("it is LZ4-compressed, not gzip".to_string());
    }
    if magic[..2] != GZIP_MAGIC {
        return Err("it is not gzip-compressed".to_string());
    }
    let f = File::open(path).map_err(|e| e.to_string())?;
    let mut decoder = flate2::bufread::GzDecoder::new(io::BufReader::new(f));
    let got = hash_stream(&mut decoder, hash).map_err(|e| format!("damaged gzip data: {}", e))?;
    if got != hash {
        return Err(format!("its content digest is {}", got));
    }
    let mut rest = decoder.into_inner();
    if rest.read(&mut [0u8; 1]).map_err(|e| e.to_string())? != 0 {
        return Err("it has data after the first gzip member".to_string());
    }
    if pinned.is_empty() {
        return Ok(());
    }

    let mut sha1 = sha1::Sha1::new();
    let mut sha256 = sha2::Sha256::new();
    let mut sha512t256 = sha2::Sha512_256::new();
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        sha1.update(&buf[..n]);
        sha256.update(&buf[..n]);
        sha512t256.update(&buf[..n]);
    }
    let sha1 = format!("{:x}", sha1.finalize());
    let sha256 = format!("{:x}", sha256.finalize());
    let sha512t256 = format!("{:x}", sha512t256.finalize());
    let mismatched: Vec<String> = pinned
        .iter()
        .filter(|d| match d {
            CompressedDigest::Sha1(h) => *h != sha1,
            CompressedDigest::Sha256(h) => *h != sha256,
            CompressedDigest::Sha512t256(h) => *h != sha512t256,
        })
        .map(CompressedDigest::describe)
        .collect();
    if mismatched.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "its compressed bytes do not match the manifest's {}",
            mismatched.join(", ")
        ))
    }
}

/// How a verified source payload is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceEncoding {
    /// Already what pkg5 clients expect; stored byte for byte.
    Wire,
    /// Correct content, but uncompressed or in several gzip members; stored
    /// as a fresh single gzip member. Only allowed when no manifest pins the
    /// compressed bytes.
    Recompress,
}

/// pkg5 names a payload by the digest of its uncompressed content and stores
/// it gzip-compressed.
fn verify_source_payload(
    path: &Path,
    hash: &str,
    pinned: &BTreeSet<CompressedDigest>,
) -> std::result::Result<SourceEncoding, String> {
    let why = match check_wire_payload(path, hash, pinned) {
        Ok(()) => return Ok(SourceEncoding::Wire),
        Err(why) => why,
    };
    if !pinned.is_empty() {
        // Recompressing would break the pinned digests.
        return Err(why);
    }
    let is_gzip = read_magic(path).map_err(|e| e.to_string())?[..2] == GZIP_MAGIC;
    let f = File::open(path).map_err(|e| e.to_string())?;
    let got = if is_gzip {
        hash_stream(&mut flate2::read::MultiGzDecoder::new(f), hash)
    } else {
        hash_stream(&mut io::BufReader::new(f), hash)
    };
    match got {
        Ok(got) if got == hash => Ok(SourceEncoding::Recompress),
        _ => Err(why),
    }
}

/// A verified source payload and where it goes.
struct PayloadMove {
    src: PathBuf,
    dest: PathBuf,
    hash: String,
    pinned: BTreeSet<CompressedDigest>,
    encoding: SourceEncoding,
}

/// Write `src` recompressed as a single gzip member to `tmp`.
fn recompress(src: &Path, tmp: &Path) -> io::Result<()> {
    let is_gzip = read_magic(src)?[..2] == GZIP_MAGIC;
    let f = File::open(src)?;
    let mut reader: Box<dyn Read> = if is_gzip {
        Box::new(flate2::read::MultiGzDecoder::new(f))
    } else {
        Box::new(io::BufReader::new(f))
    };
    let out = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(tmp)?;
    let mut encoder = flate2::write::GzEncoder::new(out, flate2::Compression::default());
    io::copy(&mut reader, &mut encoder)?;
    encoder.finish()?;
    Ok(())
}

/// Move (archive scratch) or copy (directory source) verified payloads into
/// the shared store and make them durable. Copies of a directory source and
/// recompressed payloads are verified again once written, so a source that
/// changes after validation cannot slip in unverified content.
fn store_payloads(moves: &[PayloadMove], from_scratch: bool) -> Result<()> {
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    for m in moves {
        let dest = &m.dest;
        let parent = dest.parent().expect("payload path has a parent");
        fs::create_dir_all(parent).at(parent)?;
        let name = dest
            .file_name()
            .expect("payload path has a name")
            .to_string_lossy();
        let tmp = parent.join(format!(".{}.import", name));
        match m.encoding {
            SourceEncoding::Wire if from_scratch => fs::rename(&m.src, &tmp).at(&tmp)?,
            SourceEncoding::Wire => {
                fs::copy(&m.src, &tmp).at(&tmp)?;
            }
            SourceEncoding::Recompress => recompress(&m.src, &tmp).at(&tmp)?,
        }
        if (!from_scratch || m.encoding == SourceEncoding::Recompress)
            && let Err(why) = check_wire_payload(&tmp, &m.hash, &m.pinned)
        {
            let _ = fs::remove_file(&tmp);
            return Err(Pkg5ImportError::InvalidSource {
                problems: vec![format!(
                    "payload {}: {} changed while importing: {}",
                    m.hash,
                    m.src.display(),
                    why
                )],
            });
        }
        File::open(&tmp).and_then(|f| f.sync_all()).at(&tmp)?;
        // link(2) rather than rename(2): a copy that appeared meanwhile is
        // never replaced.
        fs::hard_link(&tmp, dest).at(dest)?;
        fs::remove_file(&tmp).at(&tmp)?;
        dirs.insert(parent.to_path_buf());
    }
    // Every directory store_payloads may have created, up to `publisher/`
    // (file/<xx>, file, publisher/<pub>, publisher), so the entries survive a
    // power loss before the generation that references them is published.
    let mut to_sync: BTreeSet<PathBuf> = BTreeSet::new();
    for dir in &dirs {
        to_sync.extend(dir.ancestors().take(4).map(Path::to_path_buf));
    }
    for dir in &to_sync {
        File::open(dir).and_then(|f| f.sync_all()).at(dir)?;
    }
    debug!(count = moves.len(), "stored payloads");
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let f = File::open(path).at(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut reader = io::BufReader::with_capacity(1 << 20, f);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf).at(path)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0acf3a5d1d1f9d0b8f2a1c6c1a3b1f0e8d7c6b5a";

    #[test]
    fn reads_fmri_and_payloads_from_pkg5_manifest() {
        let manifest = format!(
            "set name=pkg.fmri value=pkg://test/web/hello@1.0,5.11-2024.0.0.0:20240101T000000Z\n\
             set name=pkg.summary value=\"Hello\"\n\
             file {h} chash={h} group=bin mode=0644 owner=root path=usr/share/hello pkg.csize=10 pkg.size=5\n\
             license {h2} license=MIT\n",
            h = HASH,
            h2 = "1".repeat(40)
        );
        let (fmri, payloads) = read_package("test", manifest.as_bytes()).unwrap();
        assert_eq!(fmri.stem(), "web/hello");
        assert_eq!(fmri.publisher.as_deref(), Some("test"));
        assert_eq!(fmri.version(), "1.0,5.11-2024.0.0.0:20240101T000000Z");
        assert!(payloads.contains_key(HASH));
        assert!(payloads.contains_key(&"1".repeat(40)));
    }

    #[test]
    fn rejects_foreign_publisher_and_missing_version() {
        let m = "set name=pkg.fmri value=pkg://other/hello@1.0\n";
        assert!(
            read_package("test", m.as_bytes())
                .unwrap_err()
                .contains("names publisher")
        );
        let m = "set name=pkg.fmri value=pkg://test/hello\n";
        assert!(
            read_package("test", m.as_bytes())
                .unwrap_err()
                .contains("no version")
        );
    }

    #[test]
    fn payload_names_must_be_digests() {
        assert!(is_payload_hash(HASH));
        assert!(!is_payload_hash("../../etc/passwd"));
        assert!(!is_payload_hash(&HASH.to_uppercase()));
    }
}
