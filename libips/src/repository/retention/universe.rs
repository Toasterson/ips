//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

//! Package versions of a generation with their pkg5 dependencies, and the
//! checks that keep protected snapshots and the dependencies of visible
//! packages installable when versions are hidden or excluded.
//!
//! Dependency semantics follow OpenIndiana pkg5 (`pkg_solver.py`,
//! `version.py`): `require` (and `group`, `conditional`, each `require-any`
//! alternative) is satisfied by the named version or any successor that is
//! not obsolete; `incorporate` is satisfied by versions matching every
//! component it names (`CONSTRAINT_AUTO`). A dependency without a
//! publisher resolves within the dependent's publisher when that publisher
//! has the package, otherwise in any publisher.
//!
//! Content the checks cannot interpret (a package version or dependency
//! that is not a pkg5 version, a `depend` action of unknown type or
//! without `fmri`) is never skipped silently: it is kept as
//! [`Unverifiable`], and hiding or excluding versions it may need is
//! refused.

use super::policy::Snapshot;
use super::version::Pkg5Version;
use super::{IoContext, Result};
use crate::fmri::Fmri;
use crate::repository::FileBackend;
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepKind {
    Require,
    RequireAny,
    Incorporate,
    Group,
    GroupAny,
    Conditional,
    Optional,
    Other,
}

impl DepKind {
    /// `None` for a type pkg5 does not define.
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "require" => DepKind::Require,
            "require-any" => DepKind::RequireAny,
            "incorporate" => DepKind::Incorporate,
            "group" => DepKind::Group,
            "group-any" => DepKind::GroupAny,
            "conditional" => DepKind::Conditional,
            "optional" => DepKind::Optional,
            "exclude" | "parent" | "origin" => DepKind::Other,
            _ => return None,
        })
    }

    /// Whether installing the dependent fails when no version satisfies it.
    fn is_hard(self) -> bool {
        matches!(
            self,
            DepKind::Require | DepKind::RequireAny | DepKind::Incorporate
        )
    }

    /// Whether the target belongs to the installable set of a snapshot.
    fn installs_target(self) -> bool {
        matches!(
            self,
            DepKind::Require
                | DepKind::RequireAny
                | DepKind::Group
                | DepKind::GroupAny
                | DepKind::Conditional
        )
    }
}

#[derive(Debug, Clone)]
pub struct DepTarget {
    pub publisher: Option<String>,
    pub stem: String,
    pub version: Option<Pkg5Version>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Dep {
    pub kind: DepKind,
    pub targets: Vec<DepTarget>,
}

/// Dependency-relevant content of a manifest.
#[derive(Debug, Default, Clone)]
pub struct ManifestScan {
    pub deps: Vec<Dep>,
    pub obsolete: bool,
    pub renamed: bool,
    /// Payload hashes (file, license, signature actions).
    pub payloads: Vec<String>,
    /// Dependencies that cannot be interpreted, with the stem they name
    /// (`None`: unknown).
    pub invalid: Vec<(String, Option<String>)>,
}

/// Scan a pkg5 text manifest for dependencies, obsoletion and payloads
/// without building the full action tree.
pub fn scan_manifest(text: &str) -> ManifestScan {
    let mut scan = ManifestScan::default();
    let mut logical = String::new();
    for line in text.lines() {
        if let Some(stripped) = line.strip_suffix('\\') {
            logical.push_str(stripped);
            logical.push(' ');
            continue;
        }
        logical.push_str(line);
        let trimmed = logical.trim_start();
        if let Some((head, rest)) = trimmed.split_once(char::is_whitespace) {
            match head {
                "depend" => {
                    let props = crate::actions::parse_action_properties(rest);
                    let mut kind_text: Option<&str> = None;
                    let mut targets = Vec::new();
                    let mut fmris = 0usize;
                    for (k, v) in &props {
                        match k.as_str() {
                            "type" => kind_text = Some(v),
                            "fmri" => {
                                fmris += 1;
                                match dep_target(v) {
                                    Ok(t) => targets.push(t),
                                    Err(stem) => scan.invalid.push((
                                        format!(
                                            "dependency {:?} is not a pkg5 FMRI and version",
                                            v
                                        ),
                                        stem,
                                    )),
                                }
                            }
                            _ => {}
                        }
                    }
                    match kind_text.and_then(DepKind::parse) {
                        Some(_) if fmris == 0 => scan
                            .invalid
                            .push(("depend action without fmri".to_string(), None)),
                        Some(kind) => {
                            if !targets.is_empty() {
                                scan.deps.push(Dep { kind, targets });
                            }
                        }
                        None => {
                            let what = match kind_text {
                                Some(t) => format!("depend action of unknown type {:?}", t),
                                None => "depend action without type".to_string(),
                            };
                            if targets.is_empty() {
                                scan.invalid.push((what.clone(), None));
                            }
                            for t in targets {
                                scan.invalid.push((what.clone(), Some(t.stem)));
                            }
                        }
                    }
                }
                "set" => {
                    let props = crate::actions::parse_action_properties(rest);
                    let name = props.iter().find(|(k, _)| k == "name").map(|(_, v)| v);
                    let truthy = props
                        .iter()
                        .any(|(k, v)| k == "value" && v.eq_ignore_ascii_case("true"));
                    match name.map(String::as_str) {
                        Some("pkg.obsolete") if truthy => scan.obsolete = true,
                        Some("pkg.renamed") if truthy => scan.renamed = true,
                        _ => {}
                    }
                }
                "file" | "license" | "signature" => {
                    let first = rest.split_whitespace().next().unwrap_or("");
                    if !first.is_empty() && !first.contains('=') {
                        scan.payloads.push(first.to_string());
                    } else if let Some((_, h)) = crate::actions::parse_action_properties(rest)
                        .into_iter()
                        .find(|(k, _)| k == "hash")
                    {
                        scan.payloads.push(h);
                    }
                }
                _ => {}
            }
        }
        logical.clear();
    }
    scan
}

/// The target, or the stem it names (if any) when it cannot be read.
fn dep_target(s: &str) -> std::result::Result<DepTarget, Option<String>> {
    let f = Fmri::parse(s).map_err(|_| {
        // The name alone may still be readable, which narrows the block.
        let name = s.split('@').next().unwrap_or(s);
        Fmri::parse(name).ok().map(|f| f.name)
    })?;
    let version = match &f.version {
        Some(v) => Some(Pkg5Version::from_version(v).ok_or_else(|| Some(f.name.clone()))?),
        None => None,
    };
    Ok(DepTarget {
        publisher: f.publisher.clone(),
        stem: f.name.clone(),
        version,
        text: s.to_string(),
    })
}

/// A package version.
#[derive(Debug, Clone)]
pub struct PackageRecord {
    /// Canonical full FMRI.
    pub fmri: String,
    pub publisher: String,
    pub stem: String,
    pub version: Pkg5Version,
    pub obsolete: bool,
    pub renamed: bool,
    pub deps: Vec<Dep>,
    /// When the repository first published it: the later of the FMRI
    /// timestamp and the first import receipt naming it.
    pub published_at: Option<DateTime<Utc>>,
}

/// Content whose dependencies cannot be checked.
#[derive(Debug, Clone)]
pub struct Unverifiable {
    pub what: String,
    /// Stems whose versions may be needed by it; `None`: any stem.
    pub stems: Option<BTreeSet<String>>,
}

/// Package versions indexed by FMRI and by (publisher, stem).
#[derive(Debug, Default)]
pub struct Universe {
    pub packages: Vec<PackageRecord>,
    /// Never skipped: hiding or excluding what it may need is refused.
    pub unverifiable: Vec<Unverifiable>,
    index: HashMap<String, usize>,
    /// (publisher, stem) -> indices ascending by version.
    stems: HashMap<(String, String), Vec<usize>>,
    /// stem -> publishers holding it.
    stem_publishers: HashMap<String, BTreeSet<String>>,
}

/// Payload references collected while loading: (publisher, hash) -> package
/// versions referencing it.
pub type PayloadRefs = HashMap<(String, String), Vec<usize>>;

impl Universe {
    /// Load every manifest of `publishers` in the generation at `root`.
    /// Payload references are collected only for payloads `wanted` accepts.
    pub fn load(
        root: &Path,
        publishers: &[String],
        first_seen: &HashMap<String, DateTime<Utc>>,
        wanted: &dyn Fn(&str, &str) -> bool,
    ) -> Result<(Self, PayloadRefs)> {
        let backend = FileBackend::for_generation_root(root, publishers.to_vec());
        let mut u = Universe::default();
        let mut refs = PayloadRefs::new();
        for publisher in publishers {
            for info in backend.list_packages_from_disk(Some(publisher), None)? {
                let fmri = info.fmri;
                let path: PathBuf = FileBackend::construct_manifest_path(
                    root,
                    publisher,
                    fmri.stem(),
                    &fmri.version(),
                );
                let bytes = std::fs::read(&path).at(&path)?;
                let mut scan = scan_manifest(&String::from_utf8_lossy(&bytes));
                let payloads = std::mem::take(&mut scan.payloads);
                let seen = first_seen.get(&fmri.to_string()).copied();
                let Some(idx) = u.add_scanned(&fmri, publisher, scan, seen) else {
                    continue;
                };
                for h in payloads {
                    if wanted(publisher, &h) {
                        let e = refs.entry((publisher.clone(), h)).or_default();
                        if !e.contains(&idx) {
                            e.push(idx);
                        }
                    }
                }
            }
        }
        u.finish();
        Ok((u, refs))
    }

    /// Add the scanned manifest of `fmri` (call [`Universe::finish`]
    /// afterwards). What cannot be interpreted is kept as [`Unverifiable`];
    /// `None` if the package's own version is not a pkg5 version (then it
    /// blocks changes to its own stem and every stem it depends on).
    pub fn add_scanned(
        &mut self,
        fmri: &Fmri,
        publisher: &str,
        scan: ManifestScan,
        first_seen: Option<DateTime<Utc>>,
    ) -> Option<usize> {
        let key = fmri.to_string();
        for (what, stem) in scan.invalid {
            self.unverifiable.push(Unverifiable {
                what: format!("{}: {}", key, what),
                stems: stem.map(|s| BTreeSet::from([s])),
            });
        }
        let Some(version) = fmri.version.as_ref().and_then(Pkg5Version::from_version) else {
            let mut stems: BTreeSet<String> = scan
                .deps
                .iter()
                .flat_map(|d| d.targets.iter().map(|t| t.stem.clone()))
                .collect();
            stems.insert(fmri.stem().to_string());
            self.unverifiable.push(Unverifiable {
                what: format!(
                    "{}: its version is not a pkg5 version (dot-separated integers)",
                    key
                ),
                stems: Some(stems),
            });
            return None;
        };
        Some(self.add(PackageRecord {
            published_at: published_at(&version, first_seen),
            fmri: key,
            publisher: publisher.to_string(),
            stem: fmri.stem().to_string(),
            version,
            obsolete: scan.obsolete,
            renamed: scan.renamed,
            deps: scan.deps,
        }))
    }

    /// Add a package version (call [`Universe::finish`] afterwards).
    /// Returns its index; a duplicate FMRI returns the existing index.
    pub fn add(&mut self, rec: PackageRecord) -> usize {
        if let Some(&i) = self.index.get(&rec.fmri) {
            return i;
        }
        let i = self.packages.len();
        self.index.insert(rec.fmri.clone(), i);
        self.stems
            .entry((rec.publisher.clone(), rec.stem.clone()))
            .or_default()
            .push(i);
        self.stem_publishers
            .entry(rec.stem.clone())
            .or_default()
            .insert(rec.publisher.clone());
        self.packages.push(rec);
        i
    }

    pub fn finish(&mut self) {
        let packages = &self.packages;
        for list in self.stems.values_mut() {
            list.sort_by(|a, b| packages[*a].version.cmp(&packages[*b].version));
        }
    }

    pub fn get(&self, fmri: &str) -> Option<usize> {
        self.index.get(fmri).copied()
    }

    pub fn len(&self) -> usize {
        self.packages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    /// Versions of the same publisher and stem newer than `i`.
    pub fn successors(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        let p = &self.packages[i];
        self.stems
            .get(&(p.publisher.clone(), p.stem.clone()))
            .into_iter()
            .flatten()
            .copied()
            .filter(move |&j| {
                self.packages[j].version.pkg5_cmp(&p.version) == std::cmp::Ordering::Greater
            })
    }

    /// When `i` was superseded: the earliest publication of a newer
    /// version. `None` if it is the newest version or no newer version has
    /// a known publication time.
    pub fn superseded_at(&self, i: usize) -> Option<DateTime<Utc>> {
        self.successors(i)
            .filter_map(|j| self.packages[j].published_at)
            .min()
    }

    pub fn is_superseded(&self, i: usize) -> bool {
        self.successors(i).next().is_some()
    }

    fn candidates(&self, dependent_publisher: &str, t: &DepTarget) -> Vec<usize> {
        let publisher = match &t.publisher {
            Some(p) => p.clone(),
            None => {
                let Some(pubs) = self.stem_publishers.get(&t.stem) else {
                    return vec![];
                };
                if pubs.contains(dependent_publisher) {
                    dependent_publisher.to_string()
                } else {
                    return pubs
                        .iter()
                        .flat_map(|p| {
                            self.stems
                                .get(&(p.clone(), t.stem.clone()))
                                .cloned()
                                .unwrap_or_default()
                        })
                        .collect();
                }
            }
        };
        self.stems
            .get(&(publisher, t.stem.clone()))
            .cloned()
            .unwrap_or_default()
    }

    /// Whether any version, obsolete or not, has at least the version `t`
    /// names. A `require` that only obsolete versions match follows pkg5
    /// obsoletion and is not reported as unsatisfied.
    fn has_version_at_least(&self, from: usize, t: &DepTarget) -> bool {
        self.candidates(&self.packages[from].publisher, t)
            .into_iter()
            .any(|j| {
                t.version
                    .as_ref()
                    .is_none_or(|w| self.packages[j].version.satisfies_minimum(w))
            })
    }

    /// Versions satisfying one target of a dependency of `from`, ascending.
    pub fn satisfiers(&self, from: usize, kind: DepKind, t: &DepTarget) -> Vec<usize> {
        let publisher = &self.packages[from].publisher;
        self.candidates(publisher, t)
            .into_iter()
            .filter(|&j| {
                let p = &self.packages[j];
                match kind {
                    DepKind::Incorporate => t
                        .version
                        .as_ref()
                        .is_some_and(|w| p.version.matches_incorporation(w)),
                    _ => {
                        !p.obsolete
                            && t.version
                                .as_ref()
                                .is_none_or(|w| p.version.satisfies_minimum(w))
                    }
                }
            })
            .collect()
    }
}

fn published_at(version: &Pkg5Version, first_seen: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (version.published(), first_seen) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// The versions a protected snapshot needs visible.
#[derive(Debug, Clone)]
pub struct SnapshotClosure {
    pub name: String,
    /// Protected version -> why it is needed.
    pub protected: BTreeMap<usize, String>,
    /// Members the repository does not hold (unavailable historical
    /// content, reported, never silently dropped).
    pub missing: Vec<String>,
    /// `require` and `require-any` dependencies of protected versions that
    /// no version (under the snapshot's incorporations) satisfies: the
    /// snapshot cannot be installed as is. Reported, never dropped.
    pub unsatisfied: Vec<String>,
}

/// The set a snapshot needs for pkg5 install and repair under its own
/// constraints: its members; every version its incorporations (and those
/// of protected packages) allow; and for each installing dependency
/// (`require`, `require-any`, `group`, `conditional`) that no incorporation
/// pins, the newest non-obsolete satisfying version published at or before
/// the snapshot's `as-of` time (or the oldest satisfying one if none was),
/// recursively.
pub fn snapshot_closure(u: &Universe, snap: &Snapshot) -> SnapshotClosure {
    let mut out = SnapshotClosure {
        name: snap.name.clone(),
        protected: BTreeMap::new(),
        missing: vec![],
        unsatisfied: vec![],
    };
    let mut members = Vec::new();
    for m in &snap.members {
        match u.get(m) {
            Some(i) => members.push(i),
            None => out.missing.push(m.clone()),
        }
    }
    let as_of = snap.as_of.or_else(|| {
        members
            .iter()
            .filter_map(|&i| u.packages[i].version.published())
            .max()
    });

    // Constraints of every protected incorporation, per stem.
    let mut constraints: HashMap<String, Vec<Pkg5Version>> = HashMap::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    for &i in &members {
        if out
            .protected
            .insert(i, format!("member of snapshot {}", snap.name))
            .is_none()
        {
            queue.push_back(i);
        }
    }
    // Incorporations first, so that requirements resolve under the
    // snapshot's full set of constraints.
    let mut deferred: Vec<usize> = Vec::new();
    while let Some(i) = queue.pop_front() {
        deferred.push(i);
        for dep in &u.packages[i].deps {
            if dep.kind != DepKind::Incorporate {
                continue;
            }
            for t in &dep.targets {
                if let Some(v) = &t.version {
                    constraints
                        .entry(t.stem.clone())
                        .or_default()
                        .push(v.clone());
                }
                for j in u.satisfiers(i, dep.kind, t) {
                    if let std::collections::btree_map::Entry::Vacant(e) = out.protected.entry(j) {
                        e.insert(format!(
                            "incorporated by {} ({})",
                            u.packages[i].fmri, t.text
                        ));
                        queue.push_back(j);
                    }
                }
            }
        }
    }

    let mut queue: VecDeque<usize> = deferred.into();
    while let Some(i) = queue.pop_front() {
        for dep in &u.packages[i].deps {
            if !dep.kind.installs_target() && dep.kind != DepKind::Incorporate {
                continue;
            }
            let mut satisfied = false;
            let mut constrained = false;
            for t in &dep.targets {
                let sats = u.satisfiers(i, dep.kind, t);
                // Only obsolete versions match: pkg5 obsoletion, not a
                // missing requirement.
                if sats.is_empty() && u.has_version_at_least(i, t) {
                    satisfied = true;
                }
                let chosen: Vec<usize> = if dep.kind == DepKind::Incorporate {
                    if let Some(v) = &t.version {
                        constraints
                            .entry(t.stem.clone())
                            .or_default()
                            .push(v.clone());
                    }
                    sats
                } else if let Some(cs) = constraints.get(&t.stem) {
                    constrained = true;
                    sats.into_iter()
                        .filter(|&j| {
                            cs.iter()
                                .all(|c| u.packages[j].version.matches_incorporation(c))
                        })
                        .collect()
                } else {
                    let historical = sats
                        .iter()
                        .rev()
                        .find(|&&j| match (as_of, u.packages[j].version.published()) {
                            (Some(limit), Some(p)) => p <= limit,
                            (None, _) => true,
                            (Some(_), None) => true,
                        })
                        .or_else(|| sats.first())
                        .copied();
                    historical.into_iter().collect()
                };
                satisfied |= !chosen.is_empty();
                for j in chosen {
                    if let std::collections::btree_map::Entry::Vacant(e) = out.protected.entry(j) {
                        let how = if dep.kind == DepKind::Incorporate {
                            "incorporated"
                        } else {
                            "required"
                        };
                        e.insert(format!("{} by {} ({})", how, u.packages[i].fmri, t.text));
                        queue.push_back(j);
                    }
                }
            }
            if matches!(dep.kind, DepKind::Require | DepKind::RequireAny) && !satisfied {
                let texts: Vec<&str> = dep.targets.iter().map(|t| t.text.as_str()).collect();
                out.unsatisfied.push(format!(
                    "{} requires {}, which no version in the repository satisfies{}",
                    u.packages[i].fmri,
                    texts.join(" or "),
                    if constrained {
                        " under the snapshot's incorporations"
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    out
}

/// Problems found by [`check_hiding`], with the exact entries that resolve
/// them.
#[derive(Debug, Default, Clone)]
pub struct Blockers {
    pub blockers: Vec<String>,
    pub keeps: BTreeSet<String>,
    pub restores: BTreeSet<String>,
}

impl Blockers {
    pub fn is_empty(&self) -> bool {
        self.blockers.is_empty()
    }
}

/// Check that hiding `hide` (in addition to `hidden`) keeps every protected
/// snapshot set visible and every hard dependency of the packages
/// `dependents` selects (that stay visible) satisfiable, where it was
/// satisfiable before. With `report_hidden` a snapshot version that is
/// already hidden is a blocker too.
pub fn check_hiding(
    u: &Universe,
    hidden: &HashSet<usize>,
    hide: &HashSet<usize>,
    snapshots: &[SnapshotClosure],
    dependents: &dyn Fn(usize) -> bool,
    report_hidden: bool,
) -> Blockers {
    let mut out = Blockers::default();
    for uv in &u.unverifiable {
        let affected: Vec<usize> = hide
            .iter()
            .copied()
            .filter(|&i| {
                uv.stems
                    .as_ref()
                    .is_none_or(|s| s.contains(&u.packages[i].stem))
            })
            .collect();
        if affected.is_empty() {
            continue;
        }
        let what = match &uv.stems {
            Some(s) => format!(
                "version(s) of {}",
                s.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
            None => "any version".to_string(),
        };
        out.blockers.push(format!(
            "{}; whether it needs {} cannot be checked, so none leaves the catalog until it is fixed",
            uv.what, what
        ));
        for i in affected {
            out.keeps.insert(u.packages[i].fmri.clone());
        }
    }
    for snap in snapshots {
        for (&i, why) in &snap.protected {
            let fmri = &u.packages[i].fmri;
            if hide.contains(&i) {
                out.blockers.push(format!(
                    "{}: needed by snapshot {} ({}); hiding it breaks install and repair of that snapshot",
                    fmri, snap.name, why
                ));
                out.keeps.insert(fmri.clone());
            } else if report_hidden && hidden.contains(&i) {
                out.blockers.push(format!(
                    "{}: needed by snapshot {} ({}) but hidden; the snapshot is not installable until it is restored",
                    fmri, snap.name, why
                ));
                out.restores.insert(fmri.clone());
                out.keeps.insert(fmri.clone());
            }
        }
    }

    let touched: HashSet<&str> = hide.iter().map(|&i| u.packages[i].stem.as_str()).collect();
    if touched.is_empty() {
        return out;
    }
    let visible_before = |j: &usize| !hidden.contains(j);
    let visible_after = |j: &usize| !hidden.contains(j) && !hide.contains(j);
    for (i, p) in u.packages.iter().enumerate() {
        if hidden.contains(&i) || hide.contains(&i) || !dependents(i) {
            continue;
        }
        for dep in &p.deps {
            if !dep.kind.is_hard()
                || !dep
                    .targets
                    .iter()
                    .any(|t| touched.contains(t.stem.as_str()))
            {
                continue;
            }
            if dep.kind == DepKind::Incorporate && dep.targets.iter().all(|t| t.version.is_none()) {
                continue;
            }
            let sats: Vec<Vec<usize>> = dep
                .targets
                .iter()
                .map(|t| u.satisfiers(i, dep.kind, t))
                .collect();
            // Incorporations constrain only packages that are installed; a
            // stem the repository does not hold constrains nothing.
            let before = sats.iter().any(|s| s.iter().any(visible_before));
            let after = sats.iter().any(|s| s.iter().any(visible_after));
            if before && !after {
                let newest = sats
                    .iter()
                    .flat_map(|s| s.iter().rev().filter(|j| hide.contains(j)).take(1))
                    .next()
                    .copied();
                let texts: Vec<&str> = dep.targets.iter().map(|t| t.text.as_str()).collect();
                let what = match dep.kind {
                    DepKind::Incorporate => "incorporates",
                    DepKind::RequireAny => "requires one of",
                    _ => "requires",
                };
                out.blockers.push(format!(
                    "{} {} {}; no visible version would satisfy it",
                    p.fmri,
                    what,
                    texts.join(", ")
                ));
                if let Some(j) = newest {
                    out.keeps.insert(u.packages[j].fmri.clone());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: &str = "20200101T000000Z";

    fn rec(u: &mut Universe, fmri: &str, manifest: &str) -> usize {
        let f = Fmri::parse(fmri).unwrap();
        let publisher = f.publisher.clone().unwrap();
        u.add_scanned(&f, &publisher, scan_manifest(manifest), None)
            .unwrap()
    }

    #[test]
    fn scans_dependencies_obsoletion_and_payloads() {
        let s = scan_manifest(
            "set name=pkg.fmri value=pkg://oi/a@1\n\
             depend fmri=pkg:/b@1.0 type=require\n\
             depend fmri=c@2 fmri=d@3 \\\n    type=require-any\n\
             set name=pkg.obsolete value=true\n\
             file 0123abcd path=x owner=root group=bin mode=0644\n\
             license hash=beef license=MIT\n",
        );
        assert!(s.obsolete);
        assert_eq!(s.deps.len(), 2);
        assert_eq!(s.deps[1].kind, DepKind::RequireAny);
        assert_eq!(s.deps[1].targets.len(), 2);
        assert_eq!(s.payloads, vec!["0123abcd", "beef"]);
    }

    #[test]
    fn snapshot_closure_follows_incorporations_and_history() {
        let mut u = Universe::default();
        let inc = rec(
            &mut u,
            &format!("pkg://oi/incorp@2020-0:{TS}"),
            "depend fmri=lib@1.0-1 type=incorporate\n",
        );
        let lib1 = rec(&mut u, &format!("pkg://oi/lib@1.0-1:{TS}"), "");
        let _lib2 = rec(&mut u, "pkg://oi/lib@2.0-1:20210101T000000Z", "");
        // app is not incorporated: resolved at the snapshot's time.
        let app1 = rec(
            &mut u,
            "pkg://oi/app@1.0:20191201T000000Z",
            "depend fmri=lib@1.0 type=require\ndepend fmri=tool@1 type=require\n",
        );
        let _app2 = rec(&mut u, "pkg://oi/app@2.0:20210101T000000Z", "");
        let tool = rec(&mut u, "pkg://oi/tool@1:20190101T000000Z", "");
        let group = rec(
            &mut u,
            &format!("pkg://oi/group@1:{TS}"),
            "depend fmri=app type=group\n",
        );
        u.finish();
        let snap = Snapshot {
            name: "s".into(),
            as_of: None,
            sources: vec![],
            members: vec![
                u.packages[inc].fmri.clone(),
                u.packages[group].fmri.clone(),
                "pkg://oi/missing@1:20200101T000000Z".into(),
            ],
        };
        let c = snapshot_closure(&u, &snap);
        let got: BTreeSet<usize> = c.protected.keys().copied().collect();
        assert_eq!(got, BTreeSet::from([inc, lib1, group, app1, tool]));
        assert_eq!(c.missing, vec!["pkg://oi/missing@1:20200101T000000Z"]);
    }

    #[test]
    fn unreadable_dependencies_block_hiding_what_they_may_need() {
        let s = scan_manifest(
            "depend fmri=library/foo@1.0a type=require\n\
             depend fmri=b@1 type=requires\n\
             depend type=require\n\
             depend fmri=feature/package/dependency/self type=parent\n",
        );
        let stems: Vec<Option<&str>> = s.invalid.iter().map(|(_, st)| st.as_deref()).collect();
        assert_eq!(stems, vec![Some("library/foo"), Some("b"), None]);
        assert_eq!(s.deps.len(), 1);

        let mut u = Universe::default();
        let old = rec(&mut u, &format!("pkg://oi/library/foo@1.0:{TS}"), "");
        let _new = rec(&mut u, "pkg://oi/library/foo@2.0:20210101T000000Z", "");
        let other = rec(&mut u, &format!("pkg://oi/other@1.0:{TS}"), "");
        let _other2 = rec(&mut u, "pkg://oi/other@2.0:20210101T000000Z", "");
        let _user = rec(
            &mut u,
            &format!("pkg://oi/app@1:{TS}"),
            "depend fmri=library/foo@1.0a type=require\n",
        );
        u.finish();
        let none = HashSet::new();
        let b = check_hiding(&u, &none, &HashSet::from([old]), &[], &|_| true, true);
        assert_eq!(b.blockers.len(), 1, "{:?}", b.blockers);
        assert!(b.blockers[0].contains("pkg://oi/app@1"), "{:?}", b.blockers);
        assert!(b.keeps.contains(&u.packages[old].fmri));
        // Unrelated stems are not affected.
        assert!(check_hiding(&u, &none, &HashSet::from([other]), &[], &|_| true, true).is_empty());

        // A dependency whose stem cannot be read blocks everything.
        let _bad = rec(
            &mut u,
            &format!("pkg://oi/bad@1:{TS}"),
            "depend type=require\n",
        );
        u.finish();
        assert!(!check_hiding(&u, &none, &HashSet::from([other]), &[], &|_| true, true).is_empty());
    }

    #[test]
    fn unsatisfied_snapshot_requirements_are_reported() {
        let mut u = Universe::default();
        let app = rec(
            &mut u,
            &format!("pkg://oi/app@1:{TS}"),
            "depend fmri=lib/missing@1 type=require\n\
             depend fmri=lib/gone@1 type=require\n\
             depend fmri=lib/pinned@1 type=require\n",
        );
        // Only an obsoletion marker matches: pkg5 obsoletion, not missing.
        let _gone = rec(
            &mut u,
            &format!("pkg://oi/lib/gone@2:{TS}"),
            "set name=pkg.obsolete value=true\n",
        );
        let _p1 = rec(&mut u, &format!("pkg://oi/lib/pinned@1:{TS}"), "");
        let inc = rec(
            &mut u,
            &format!("pkg://oi/inc@1:{TS}"),
            "depend fmri=lib/pinned@2 type=incorporate\n",
        );
        u.finish();
        let snap = Snapshot {
            name: "s".into(),
            as_of: None,
            sources: vec![],
            members: vec![u.packages[app].fmri.clone(), u.packages[inc].fmri.clone()],
        };
        let c = snapshot_closure(&u, &snap);
        assert_eq!(c.unsatisfied.len(), 2, "{:?}", c.unsatisfied);
        assert!(c.unsatisfied[0].contains("lib/missing@1"));
        assert!(c.unsatisfied[1].contains("lib/pinned@1"));
        assert!(c.unsatisfied[1].contains("incorporations"));
    }

    #[test]
    fn hiding_the_only_satisfier_of_a_visible_dependency_is_blocked() {
        let mut u = Universe::default();
        let old = rec(&mut u, &format!("pkg://oi/lib@1.0:{TS}"), "");
        let _obs = rec(
            &mut u,
            "pkg://oi/lib@2.0:20210101T000000Z",
            "set name=pkg.obsolete value=true\n",
        );
        let _user = rec(
            &mut u,
            &format!("pkg://oi/user@1:{TS}"),
            "depend fmri=lib@1.0 type=require\n",
        );
        u.finish();
        let b = check_hiding(
            &u,
            &HashSet::new(),
            &HashSet::from([old]),
            &[],
            &|_| true,
            true,
        );
        assert_eq!(b.blockers.len(), 1, "{:?}", b.blockers);
        assert!(b.keeps.contains(&u.packages[old].fmri));
    }
}
