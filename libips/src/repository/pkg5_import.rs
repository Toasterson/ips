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
//!    verified against its digest before anything is written. The batch is
//!    held in memory, so a source whose selected publishers exceed the
//!    manifest count or byte budget is rejected as a whole before payloads
//!    or a catalog generation are written.
//! 3. A package whose exact FMRI is already published is skipped if the
//!    manifest bytes are identical, and rejects the whole batch otherwise.
//!    A skipped package's payload that has gone missing from the repository
//!    is verified and restored from the source, so retrying a directory
//!    import repairs the repository ([`Pkg5ImportOutcome::Repaired`], no new
//!    generation) and is a no-op once it is whole. Retrying an archive whose
//!    receipt is current returns `AlreadyImported` without looking at
//!    payloads.
//! 4. New payloads go to the shared content-addressed store (unreferenced
//!    until the switch), manifests and rebuilt catalogs to a staged
//!    generation, which is then published atomically together with the
//!    import receipt. Restored payloads go to the same shared store, but
//!    published manifests already reference them, so they are served as
//!    soon as they are written, before any metadata switch.
//!
//! Manifests are stored byte-for-byte as pkg5 published them and payloads
//! under the names the manifests reference, so pkg5 clients see exactly the
//! upstream content. `pkg.obsolete` and `pkg.renamed` packages are published
//! like any other package, as in pkg5.
//!
//! Besides version 4 repositories and archives, a directory may be a flat
//! version 3 repository (top-level `cfg_cache`, `pkg/` and `file/`) of the
//! one publisher its `cfg_cache` names. Its manifests often lack `pkg.fmri`,
//! so each is identified by its strictly decoded `pkg/<stem>/<version>`
//! path; a `pkg.fmri` it does have must agree with that path.

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
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::{debug, info, warn};

pub type Result<T> = std::result::Result<T, Pkg5ImportError>;

#[derive(Debug, Error, Diagnostic)]
pub enum Pkg5ImportError {
    #[error("{path} is not a pkg5 repository or .p5p archive")]
    #[diagnostic(
        code(ips::pkg5_import::not_pkg5),
        help(
            "Pass a .p5p archive, a directory containing pkg5.repository and publisher/, or a flat version 3 repository directory containing cfg_cache, pkg/ and file/"
        )
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

    #[error(
        "{path}: the selected publishers hold at least {manifests} manifest file(s) and {bytes} manifest byte(s), above the in-memory import budget of {max_manifests} manifest files and {max_bytes} manifest bytes; no packages were imported"
    )]
    #[diagnostic(
        code(ips::pkg5_import::manifest_budget),
        help(
            "One import holds selected manifests in memory. This batch exceeded the limit before any packages were imported. Use a bounded historical importer or a narrower --publisher selection, or raise the limits explicitly with pkg6repo import-pkg5 --max-import-manifests / --max-import-manifest-bytes after provisioning memory for the whole batch"
        )
    )]
    ManifestBudget {
        path: PathBuf,
        /// Manifest files seen when the batch was rejected.
        manifests: u64,
        /// Manifest bytes seen (including the file that crossed the limit).
        bytes: u64,
        max_manifests: u64,
        max_bytes: u64,
    },

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
    /// Most manifest files the selected publishers may hold. The whole batch
    /// is held in memory, so a larger source is rejected before publication.
    pub max_import_manifests: NonZeroU64,
    /// Most manifest bytes, summed over the selected publishers.
    pub max_import_manifest_bytes: NonZeroU64,
}

/// Default for [`Pkg5ImportOptions::max_archive_entries`]: far above a full
/// OpenIndiana publication (payload files plus manifests and directories),
/// low enough to stop an archive of empty entries exhausting inodes.
pub const DEFAULT_MAX_ARCHIVE_ENTRIES: u64 = 20_000_000;

/// Default for [`Pkg5ImportOptions::max_import_manifests`]: a CI run's
/// publication, not a historical repository.
pub const DEFAULT_MAX_IMPORT_MANIFESTS: NonZeroU64 = NonZeroU64::new(10_000).unwrap();

/// Default for [`Pkg5ImportOptions::max_import_manifest_bytes`]: 64 MiB.
pub const DEFAULT_MAX_IMPORT_MANIFEST_BYTES: NonZeroU64 = NonZeroU64::new(64 << 20).unwrap();

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
            max_import_manifests: DEFAULT_MAX_IMPORT_MANIFESTS,
            max_import_manifest_bytes: DEFAULT_MAX_IMPORT_MANIFEST_BYTES,
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
    /// A directory source contained only already-published packages, some
    /// of whose payloads were missing from the repository and have been
    /// restored from the source. No generation was published.
    Repaired {
        /// Digests of the restored payloads.
        repaired: Vec<String>,
        skipped: Vec<String>,
    },
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
    let layout = detect_layout(&root, source, is_archive)?;
    let available = match &layout {
        SourceLayout::V4 => list_publishers(&root)?,
        SourceLayout::V3 { publisher } => vec![publisher.clone()],
    };
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

    // Phase 1: read and validate every manifest, within one memory budget
    // shared by all selected publishers.
    let mut problems = Vec::new();
    let mut packages = Vec::new();
    let mut budget = ManifestBudget::new(
        source,
        options.max_import_manifests,
        options.max_import_manifest_bytes,
    );
    for publisher in &selected {
        scan_publisher(
            &root,
            &layout,
            publisher,
            &mut budget,
            &mut packages,
            &mut problems,
        )?;
    }

    // Phase 2: classify against the published generation and within the batch.
    // An already-published package keeps only the payloads it references
    // that have gone missing from the repository, not its manifest.
    let repo = store.repo().to_path_buf();
    let mut conflicts = Vec::new();
    let mut skipped = Vec::new();
    let mut wanted: BTreeMap<(String, String), WantedPayload> = BTreeMap::new();
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
            Ok(existing) if existing == pkg.manifest => {
                for (hash, pinned) in pkg.payloads {
                    if stored_payload(&repo, &pkg.publisher, &hash).is_some() {
                        continue;
                    }
                    let w = wanted.entry((pkg.publisher.clone(), hash)).or_default();
                    w.published.push(key.clone());
                    w.pinned.extend(pinned);
                }
                skipped.push(key);
            }
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
    // the source. A payload of a published package that has gone missing
    // from the repository is restored from the source, so retrying an
    // import repairs it. Copies only published packages reference were
    // verified when they were published and are not re-hashed.
    for (idx, pkg) in to_add.iter().enumerate() {
        for (hash, pinned) in &pkg.payloads {
            let w = wanted
                .entry((pkg.publisher.clone(), hash.clone()))
                .or_default();
            w.new.push(idx);
            w.pinned.extend(pinned.iter().cloned());
        }
    }
    let mut moves: Vec<PayloadMove> = Vec::new();
    let mut repaired: Vec<String> = Vec::new();
    for ((publisher, hash), w) in &wanted {
        let who = || {
            w.new
                .iter()
                .map(|&i| to_add[i].fmri.to_string())
                .chain(w.published.iter().cloned())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let pinned = &w.pinned;
        // An existing copy is reused only if pkg5 clients can install it,
        // and is never replaced: pkg6 packages may reference the same digest
        // in their own encoding (LZ4), and a publisher copy added next to a
        // global one would change what they are served.
        if let Some(stored) = stored_payload(&repo, publisher, hash) {
            if !w.new.is_empty()
                && let Err(why) = check_wire_payload(&stored, hash, pinned)
            {
                problems.push(format!(
                    "{}: payload {}: the repository already stores it at {} in a form pkg5 clients cannot install ({}); it is never replaced",
                    who(),
                    hash,
                    stored.display(),
                    why
                ));
            }
            continue;
        }
        let dest = FileBackend::construct_file_path_with_publisher(&repo, publisher, hash);
        match find_source_payload(&layout.file_dir(&root, publisher), hash) {
            Some(src) => match verify_source_payload(&src, hash, pinned) {
                Ok(encoding) => {
                    if !w.published.is_empty() {
                        repaired.push(hash.clone());
                    }
                    moves.push(PayloadMove {
                        src,
                        dest,
                        hash: hash.clone(),
                        pinned: pinned.clone(),
                        encoding,
                    })
                }
                Err(why) => problems.push(format!("{}: payload {}: {}", who(), hash, why)),
            },
            None => problems.push(format!(
                "{}: payload {} is missing from the source and the repository",
                who(),
                hash
            )),
        }
    }
    drop(wanted);
    if !problems.is_empty() {
        return Err(Pkg5ImportError::InvalidSource { problems });
    }
    if !repaired.is_empty() {
        warn!(
            count = repaired.len(),
            "restoring payloads of published packages missing from the repository"
        );
    }

    if to_add.is_empty() && archive_sha256.is_none() {
        if repaired.is_empty() {
            return Ok(Pkg5ImportOutcome::NothingToImport { skipped });
        }
        // Only published packages were missing payloads: restoring them
        // changes no metadata, so no generation is staged.
        store_payloads(&moves, is_archive)?;
        info!(count = repaired.len(), "restored missing payloads");
        return Ok(Pkg5ImportOutcome::Repaired { repaired, skipped });
    }

    // Phase 4: store payloads. New ones stay unreferenced until the switch;
    // restored ones are served as soon as they are written.
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
        repaired = repaired.len(),
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

/// On-disk layout of a pkg5 source.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceLayout {
    /// pkg5 repository version 4 (and `.p5p`): `pkg5.repository` or
    /// `pkg5.index.0.gz`, and `publisher/<prefix>/{pkg,file}`.
    V4,
    /// Flat pkg5 repository version 3: top-level `cfg_cache`, `pkg/` and
    /// `file/` of the single publisher `cfg_cache` configures. Its
    /// manifests may lack `pkg.fmri`; their identity is their path.
    V3 { publisher: String },
}

impl SourceLayout {
    fn publisher_dir(&self, root: &Path, publisher: &str) -> PathBuf {
        match self {
            SourceLayout::V4 => root.join("publisher").join(publisher),
            SourceLayout::V3 { .. } => root.to_path_buf(),
        }
    }

    fn pkg_dir(&self, root: &Path, publisher: &str) -> PathBuf {
        self.publisher_dir(root, publisher).join("pkg")
    }

    fn file_dir(&self, root: &Path, publisher: &str) -> PathBuf {
        self.publisher_dir(root, publisher).join("file")
    }
}

const V4_MARKERS: [&str; 3] = ["pkg5.repository", "pkg5.index.0.gz", "publisher"];
const V3_MARKERS: [&str; 3] = ["cfg_cache", "pkg", "file"];

/// Recognize the source's layout and reject symlinks and special files in
/// the trees it will read, before reading any of them. A tree with both
/// version 3 and version 4 entries is ambiguous and rejected. Archives are
/// always version 4.
fn detect_layout(root: &Path, source: &Path, is_archive: bool) -> Result<SourceLayout> {
    let present = |names: [&'static str; 3]| -> Vec<&'static str> {
        names
            .into_iter()
            .filter(|n| fs::symlink_metadata(root.join(n)).is_ok())
            .collect()
    };
    let not_pkg5 = || Pkg5ImportError::NotPkg5Source {
        path: source.to_path_buf(),
    };
    let v4 = present(V4_MARKERS);
    let v3 = present(V3_MARKERS);
    if !v4.is_empty() && !v3.is_empty() {
        return Err(Pkg5ImportError::InvalidSource {
            problems: vec![format!(
                "mixed repository layout: version 3 entries ({}) next to version 4 entries ({}); import each layout from its own directory",
                v3.join(", "),
                v4.join(", ")
            )],
        });
    }
    if v3.is_empty() || is_archive {
        let base = root.join("publisher");
        if fs::symlink_metadata(&base).is_ok_and(|m| !m.is_dir()) {
            return Err(Pkg5ImportError::InvalidSource {
                problems: vec!["publisher: not a directory".to_string()],
            });
        }
        reject_links_below(root, &base)?;
        if !root.join("pkg5.repository").is_file() && !root.join("pkg5.index.0.gz").is_file() {
            return Err(not_pkg5());
        }
        return Ok(SourceLayout::V4);
    }

    let mut problems = Vec::new();
    let cfg = root.join("cfg_cache");
    match fs::symlink_metadata(&cfg) {
        Ok(m) if m.is_file() => {}
        Ok(_) => problems.push("cfg_cache: not a regular file".to_string()),
        Err(_) => problems.push("cfg_cache: missing".to_string()),
    }
    for dir in ["pkg", "file"] {
        match fs::symlink_metadata(root.join(dir)) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => problems.push(format!("{}: not a directory", dir)),
            Err(_) => problems.push(format!("{}: missing", dir)),
        }
    }
    if !problems.is_empty() {
        return Err(Pkg5ImportError::InvalidSource { problems });
    }
    reject_links_below(root, &root.join("pkg"))?;
    reject_links_below(root, &root.join("file"))?;
    let publisher =
        v3_publisher(&fs::read(&cfg).at(&cfg)?).map_err(|why| Pkg5ImportError::InvalidSource {
            problems: vec![format!("cfg_cache: {}", why)],
        })?;
    Ok(SourceLayout::V3 { publisher })
}

/// The publisher prefix of a version 3 `cfg_cache` (a Python ConfigParser
/// file): exactly one `prefix` in exactly one `[publisher]` section, a valid
/// publisher name.
fn v3_publisher(cfg: &[u8]) -> std::result::Result<String, String> {
    let text = std::str::from_utf8(cfg).map_err(|_| "not UTF-8".to_string())?;
    let mut section: Option<&str> = None;
    let mut publisher_sections = 0;
    let mut prefixes = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[') {
            let name = name
                .strip_suffix(']')
                .ok_or_else(|| format!("line {}: malformed section header", n + 1))?;
            if name == "publisher" {
                publisher_sections += 1;
            }
            section = Some(name);
            continue;
        }
        if section != Some("publisher") {
            continue;
        }
        // ConfigParser reads an indented line as the continuation of the
        // previous value.
        if raw.starts_with(char::is_whitespace) {
            return Err(format!(
                "line {}: continuation lines are not supported in [publisher]",
                n + 1
            ));
        }
        let (key, value) = line
            .find([':', '='])
            .map(|i| (&line[..i], &line[i + 1..]))
            .ok_or_else(|| format!("line {}: expected key = value", n + 1))?;
        if key.trim().eq_ignore_ascii_case("prefix") {
            prefixes.push(value.trim());
        }
    }
    if publisher_sections != 1 {
        return Err(format!(
            "expected exactly one [publisher] section, found {}",
            publisher_sections
        ));
    }
    match prefixes.as_slice() {
        [] => Err("[publisher] has no prefix".to_string()),
        [p] if p.is_empty() || *p == "None" => Err("[publisher] prefix is not set".to_string()),
        [p] if !super::generation::is_valid_publisher_name(p) => Err(format!(
            "[publisher] prefix {:?} is not a valid publisher name",
            p
        )),
        [p] => Ok(p.to_string()),
        _ => Err("[publisher] sets prefix more than once".to_string()),
    }
}

/// Decode one percent-encoded path component of a version 3 manifest path,
/// as pkg5's `urllib.quote(s, "")` wrote it: every `%` starts a two-digit
/// hex escape, `+` is a literal plus, and the result must be UTF-8 without
/// control characters.
fn decode_path_component(name: &std::ffi::OsStr) -> std::result::Result<String, String> {
    let s = name
        .to_str()
        .ok_or_else(|| "name is not UTF-8".to_string())?;
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
                .ok_or_else(|| "malformed percent escape".to_string())?;
            out.push(u8::from_str_radix(hex, 16).expect("two hex digits"));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(out).map_err(|_| "decodes to invalid UTF-8".to_string())?;
    if decoded.chars().any(char::is_control) {
        return Err("decodes to a name with control characters".to_string());
    }
    Ok(decoded)
}

/// The FMRI a version 3 manifest's path names: `pkg/<stem>/<version>`,
/// both percent-encoded.
fn v3_path_fmri(
    publisher: &str,
    stem_name: &std::ffi::OsStr,
    version_name: &std::ffi::OsStr,
) -> std::result::Result<Fmri, String> {
    let stem = decode_path_component(stem_name).map_err(|e| format!("package directory: {}", e))?;
    let version =
        decode_path_component(version_name).map_err(|e| format!("version file name: {}", e))?;
    // Only the ASCII characters pkg5 allows in package names, so the stem
    // round-trips through FileBackend's path encoding unchanged.
    let safe_segment = |c: &str| {
        !matches!(c, "" | "." | "..")
            && c.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
    };
    if !stem.split('/').all(safe_segment) {
        return Err(format!(
            "package directory decodes to invalid name {:?}",
            stem
        ));
    }
    if version.is_empty() || version.contains(['/', '@']) {
        return Err(format!(
            "version file name decodes to invalid version {:?}",
            version
        ));
    }
    let parsed = crate::fmri::Version::parse(&version)
        .map_err(|e| format!("invalid version {:?}: {}", version, e))?;
    let fmri = Fmri::parse(&format!("pkg://{}/{}@{}", publisher, stem, version))
        .map_err(|e| format!("invalid FMRI for {}@{}: {}", stem, version, e))?;
    if fmri.stem() != stem || fmri.version.as_ref() != Some(&parsed) {
        return Err(format!("{}@{} is not a valid FMRI", stem, version));
    }
    Ok(fmri)
}

/// Reject symlinks and special files anywhere below `base`. A linked
/// directory would make the importer read manifests or move payloads
/// belonging to another tree.
fn reject_links_below(root: &Path, base: &Path) -> Result<()> {
    let mut problems = Vec::new();
    for entry in walkdir::WalkDir::new(base).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) if e.io_error().map(|io| io.kind()) == Some(io::ErrorKind::NotFound) => {
                continue;
            }
            Err(e) => {
                return Err(Pkg5ImportError::Io {
                    path: base.to_path_buf(),
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

/// Bounds how many manifest files and bytes one import holds in memory.
/// Every candidate manifest counts, before it is parsed, excluded or
/// deduplicated, so the whole batch is rejected before its size matters.
struct ManifestBudget<'a> {
    source: &'a Path,
    max_manifests: u64,
    max_bytes: u64,
    manifests: u64,
    /// Always at most `max_bytes`.
    bytes: u64,
}

impl<'a> ManifestBudget<'a> {
    fn new(source: &'a Path, max_manifests: NonZeroU64, max_bytes: NonZeroU64) -> Self {
        Self {
            source,
            max_manifests: max_manifests.get(),
            max_bytes: max_bytes.get(),
            manifests: 0,
            bytes: 0,
        }
    }

    fn exceeded(&self, bytes: u64) -> Pkg5ImportError {
        Pkg5ImportError::ManifestBudget {
            path: self.source.to_path_buf(),
            manifests: self.manifests,
            bytes,
            max_manifests: self.max_manifests,
            max_bytes: self.max_bytes,
        }
    }

    /// Count one more manifest file.
    fn count(&mut self) -> Result<()> {
        self.manifests = self.manifests.checked_add(1).unwrap_or(u64::MAX);
        if self.manifests > self.max_manifests {
            return Err(self.exceeded(self.bytes));
        }
        Ok(())
    }

    /// Read a counted manifest if it fits the remaining bytes: its size is
    /// checked before allocating, and the read stops one byte past the
    /// remaining budget, so a file that grows after the size check is caught
    /// without reading it whole.
    fn read(&mut self, path: &Path) -> Result<Vec<u8>> {
        let file = File::open(path).at(path)?;
        let len = file.metadata().at(path)?.len();
        self.read_from(file, len).at(path)?
    }

    /// Read `reader`, whose metadata claims `len` bytes. The outer result is
    /// the I/O outcome, the inner one the budget's verdict.
    fn read_from(&mut self, reader: impl Read, len: u64) -> io::Result<Result<Vec<u8>>> {
        let remaining = self.max_bytes.saturating_sub(self.bytes);
        if len > remaining {
            return Ok(Err(self.exceeded(self.bytes.saturating_add(len))));
        }
        // len <= remaining <= max_bytes, which the caller chose to allow.
        let mut buf = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
        reader
            .take(remaining.saturating_add(1))
            .read_to_end(&mut buf)?;
        let n = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        match self.bytes.checked_add(n).filter(|&b| b <= self.max_bytes) {
            Some(total) => {
                self.bytes = total;
                Ok(Ok(buf))
            }
            None => Ok(Err(self.exceeded(self.bytes.saturating_add(n)))),
        }
    }
}

fn scan_publisher(
    root: &Path,
    layout: &SourceLayout,
    publisher: &str,
    budget: &mut ManifestBudget,
    packages: &mut Vec<SourcePackage>,
    problems: &mut Vec<String>,
) -> Result<()> {
    let pkg_dir = layout.pkg_dir(root, publisher);
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
            // Every entry counts, even one that is then reported as a
            // problem: problems are held in memory too.
            budget.count()?;
            let path = ver.path();
            let ft = ver.file_type().at(&stem_path)?;
            if !ft.is_file() {
                problems.push(format!(
                    "{}: not a regular file",
                    path.strip_prefix(root).unwrap_or(&path).display()
                ));
                continue;
            }
            let rel = || {
                path.strip_prefix(root)
                    .unwrap_or(&path)
                    .display()
                    .to_string()
            };
            // A version 3 manifest is identified by its path.
            let path_fmri = match layout {
                SourceLayout::V4 => None,
                SourceLayout::V3 { .. } => {
                    match v3_path_fmri(publisher, &stem.file_name(), &ver.file_name()) {
                        Ok(fmri) => Some(fmri),
                        Err(why) => {
                            problems.push(format!("{}: {}", rel(), why));
                            continue;
                        }
                    }
                }
            };
            let bytes = budget.read(&path)?;
            match read_package(publisher, &bytes, path_fmri.as_ref()) {
                Ok((fmri, payloads)) => packages.push(SourcePackage {
                    publisher: publisher.to_string(),
                    fmri,
                    manifest: bytes,
                    payloads,
                }),
                Err(why) => problems.push(format!("{}: {}", rel(), why)),
            }
        }
    }
    Ok(())
}

/// Parse a pkg5 manifest: its FMRI (qualified with `publisher`) and the
/// digests of every payload it references. `path_fmri` is the identity a
/// version 3 manifest's path gives it: used when the manifest has no
/// `pkg.fmri`, which must otherwise agree with it. Comments never identify
/// a package.
fn read_package(
    publisher: &str,
    bytes: &[u8],
    path_fmri: Option<&Fmri>,
) -> std::result::Result<(Fmri, Payloads), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "manifest is not UTF-8".to_string())?;
    if text.trim_start().starts_with('{') {
        return Err("manifest is JSON, expected a pkg5 text manifest".to_string());
    }
    // The grammar needs at least one action; a version 3 manifest may hold
    // only its comment header.
    let actionless = text.lines().all(|l| {
        let l = l.trim_start();
        l.is_empty() || l.starts_with('#')
    });
    // Its header is what makes it a manifest: an empty file is not one.
    if path_fmri.is_some() && text.trim().is_empty() {
        return Err("manifest is empty".to_string());
    }
    let manifest = if actionless {
        Manifest::new()
    } else {
        let owned = text.to_string();
        // The action parser panics on some malformed payload fields; contain it.
        std::panic::catch_unwind(move || Manifest::parse_string(owned))
            .map_err(|_| "manifest could not be parsed".to_string())?
            .map_err(|e| format!("manifest could not be parsed: {}", e))?
    };

    let explicit = manifest
        .attributes
        .iter()
        .find(|a| a.key == "pkg.fmri")
        .and_then(|a| a.values.first());
    let fmri = match (explicit, path_fmri) {
        (Some(fmri_str), _) => {
            let mut fmri = Fmri::parse(fmri_str)
                .map_err(|e| format!("invalid pkg.fmri {}: {}", fmri_str, e))?;
            if fmri.version.is_none() {
                return Err(format!("pkg.fmri {} has no version", fmri_str));
            }
            // The stem and version each become one path component in the
            // repository.
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
            if let Some(by_path) = path_fmri
                && (fmri.stem() != by_path.stem() || fmri.version != by_path.version)
            {
                return Err(format!(
                    "pkg.fmri {} does not match the FMRI its path names, {}",
                    fmri_str, by_path
                ));
            }
            fmri
        }
        (None, Some(by_path)) => by_path.clone(),
        (None, None) => return Err("manifest has no pkg.fmri".to_string()),
    };

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

fn find_source_payload(file_dir: &Path, hash: &str) -> Option<PathBuf> {
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

/// The copy of a payload the depot serves `publisher`: its own, else the
/// global one.
fn stored_payload(repo: &Path, publisher: &str, hash: &str) -> Option<PathBuf> {
    [
        FileBackend::construct_file_path_with_publisher(repo, publisher, hash),
        FileBackend::construct_file_path(repo, hash),
    ]
    .into_iter()
    .find(|p| p.exists())
}

/// A payload of one publisher the import needs: referenced by a new
/// package, or by a published one while missing from the repository.
#[derive(Default)]
struct WantedPayload {
    /// Indices into the new packages referencing it.
    new: Vec<usize>,
    /// Published packages referencing it; only recorded while it is missing.
    published: Vec<String>,
    /// Compressed digests any of them pins.
    pinned: BTreeSet<CompressedDigest>,
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
        let (fmri, payloads) = read_package("test", manifest.as_bytes(), None).unwrap();
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
            read_package("test", m.as_bytes(), None)
                .unwrap_err()
                .contains("names publisher")
        );
        let m = "set name=pkg.fmri value=pkg://test/hello\n";
        assert!(
            read_package("test", m.as_bytes(), None)
                .unwrap_err()
                .contains("no version")
        );
    }

    #[test]
    fn v3_path_names_the_package_and_comments_do_not() {
        use std::ffi::OsStr;
        let by_path = v3_path_fmri(
            "opensolaris.org",
            OsStr::new("library%2Fc++"),
            OsStr::new("0.5.11%2C5.11-0.111%3A20090518T184618Z"),
        )
        .unwrap();
        assert_eq!(by_path.stem(), "library/c++");
        assert_eq!(by_path.version(), "0.5.11,5.11-0.111:20090518T184618Z");

        let m =
            "# pkg:/library/other@0.5.11,5.11-0.111\ndir path=usr group=sys mode=0755 owner=root\n";
        let (fmri, _) = read_package("opensolaris.org", m.as_bytes(), Some(&by_path)).unwrap();
        assert_eq!(fmri, by_path);
        let m = "# pkg:/library/c++@0.5.11,5.11-0.111\n";
        let (fmri, payloads) =
            read_package("opensolaris.org", m.as_bytes(), Some(&by_path)).unwrap();
        assert_eq!(fmri, by_path);
        assert!(payloads.is_empty());
        assert!(
            read_package("opensolaris.org", m.as_bytes(), None)
                .unwrap_err()
                .contains("no pkg.fmri")
        );
        for m in ["", " \n\t\n"] {
            assert!(
                read_package("opensolaris.org", m.as_bytes(), Some(&by_path))
                    .unwrap_err()
                    .contains("manifest is empty"),
                "{m:?}"
            );
        }

        let m = "set name=pkg.fmri value=pkg:/library/c++@0.5.11,5.11-0.111:20090518T184618Z\n";
        let (fmri, _) = read_package("opensolaris.org", m.as_bytes(), Some(&by_path)).unwrap();
        assert_eq!(fmri, by_path);
        for m in [
            "set name=pkg.fmri value=pkg:/library/c++@0.5.11,5.11-0.111\n",
            "set name=pkg.fmri value=pkg:/library/other@0.5.11,5.11-0.111:20090518T184618Z\n",
        ] {
            assert!(
                read_package("opensolaris.org", m.as_bytes(), Some(&by_path))
                    .unwrap_err()
                    .contains("does not match"),
                "{m}"
            );
        }
    }

    #[test]
    fn v3_path_components_are_decoded_strictly() {
        use std::ffi::OsStr;
        let v = OsStr::new("1.0");
        for stem in [
            "bad%2",
            "bad%zz",
            "%FF",
            "a%2F..%2Fb",
            "%2Fabs",
            "a%2F",
            "ctl%0A",
            "a%40b",
            "a%5Cb",
            "a%20b",
            "a%25b",
            "caf%C3%A9",
            "a%2F.",
            "a%2F%2Fb",
            "a%3Ab",
        ] {
            assert!(v3_path_fmri("p", OsStr::new(stem), v).is_err(), "{stem}");
        }
        for stem in ["SUNWcs", "library%2Fc++", "a_b.c-d%2Fe+f"] {
            assert!(v3_path_fmri("p", OsStr::new(stem), v).is_ok(), "{stem}");
        }
        for ver in ["", "1.0%3", "1.0%2Fx", "%00", "not a version"] {
            assert!(
                v3_path_fmri("p", OsStr::new("a"), OsStr::new(ver)).is_err(),
                "{ver}"
            );
        }
        assert_eq!(
            decode_path_component(OsStr::new("a+b%20c")).unwrap(),
            "a+b c"
        );
    }

    #[test]
    fn v3_cfg_cache_names_exactly_one_valid_publisher() {
        let ok = "[publisher]\nalias = None\nprefix = opensolaris.org\n\n[repository]\nname = x\n";
        assert_eq!(v3_publisher(ok.as_bytes()).unwrap(), "opensolaris.org");
        for bad in [
            "[repository]\nname = x\n",
            "[publisher]\nalias = x\n",
            "[publisher]\nprefix =\n",
            "[publisher]\nprefix = None\n",
            "[publisher]\nprefix = a\nprefix = b\n",
            "[publisher]\nprefix = a\n[publisher]\nprefix = a\n",
            "[publisher]\nprefix = ../x\n",
            "[publisher]\nprefix = a\n  b\n",
            "[publisher\nprefix = a\n",
        ] {
            assert!(v3_publisher(bad.as_bytes()).is_err(), "{bad}");
        }
        assert!(v3_publisher(b"[publisher]\nprefix = \xff\n").is_err());
    }

    #[test]
    fn manifest_budget_catches_claims_and_growth_past_the_limit() {
        let nz = |n| NonZeroU64::new(n).unwrap();
        let src = Path::new("src");
        let mut budget = ManifestBudget::new(src, nz(2), nz(10));
        // Exactly the byte limit fits.
        assert_eq!(
            budget.read_from(&[1u8; 6][..], 6).unwrap().unwrap().len(),
            6
        );
        assert_eq!(
            budget.read_from(&[2u8; 4][..], 4).unwrap().unwrap().len(),
            4
        );
        // Once full, even an empty claim that turns out to grow is caught
        // after reading one byte past the limit, not the whole file.
        let grown = [3u8; 1 << 20];
        let mut reader = &grown[..];
        match budget.read_from(&mut reader, 0).unwrap() {
            Err(Pkg5ImportError::ManifestBudget {
                bytes, max_bytes, ..
            }) => assert_eq!((bytes, max_bytes), (11, 10)),
            other => panic!("expected growth to be caught, got {other:?}"),
        }
        assert_eq!(reader.len(), grown.len() - 1);

        // A claim over the remaining bytes is refused before reading.
        let mut budget = ManifestBudget::new(src, nz(2), nz(10));
        let mut reader = &grown[..];
        match budget.read_from(&mut reader, u64::MAX).unwrap() {
            Err(Pkg5ImportError::ManifestBudget { bytes, .. }) => assert_eq!(bytes, u64::MAX),
            other => panic!("expected the claim to be refused, got {other:?}"),
        }
        assert_eq!(reader.len(), grown.len());

        // The count limit admits exactly max_manifests files.
        budget.count().unwrap();
        budget.count().unwrap();
        match budget.count() {
            Err(Pkg5ImportError::ManifestBudget {
                manifests,
                max_manifests,
                ..
            }) => assert_eq!((manifests, max_manifests), (3, 2)),
            other => panic!("expected the count limit, got {other:?}"),
        }
    }

    #[test]
    fn payload_names_must_be_digests() {
        assert!(is_payload_hash(HASH));
        assert!(!is_payload_hash("../../etc/passwd"));
        assert!(!is_payload_hash(&HASH.to_uppercase()));
    }
}
