//! Atomic pkg5 import (#38): one `.p5p` per CI run becomes visible in a single
//! generation switch, with crash injection before and after the switch.

#[path = "support/pkg5_fixture.rs"]
mod pkg5_fixture;

use libips::repository::generation::{CURRENT_LINK, GENERATIONS_DIR};
use libips::repository::{
    FailPoint, FileBackend, GenerationError, GenerationStore, Pkg5ImportError, Pkg5ImportOptions,
    Pkg5ImportOutcome, ReadableRepository, RepositoryVersion, WritableRepository, import_pkg5,
};
use pkg5_fixture::{Pkg, gzip, make_p5p, quote, sha1_hex, write_pkg5_repo};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn import(repo: &Path, source: &Path) -> Result<Pkg5ImportOutcome, Pkg5ImportError> {
    import_pkg5(repo, source, &Pkg5ImportOptions::default())
}

fn import_failing(repo: &Path, source: &Path, point: FailPoint) {
    let options = Pkg5ImportOptions {
        fail_point: Some(point),
        ..Default::default()
    };
    match import_pkg5(repo, source, &options) {
        Err(Pkg5ImportError::Generation(GenerationError::InjectedFailure(p))) => {
            assert_eq!(p, point)
        }
        other => panic!("expected injected failure at {point}, got {other:?}"),
    }
}

fn current_generation(repo: &Path) -> String {
    fs::read_link(repo.join(GENERATIONS_DIR).join(CURRENT_LINK))
        .unwrap()
        .to_string_lossy()
        .to_string()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// `stem@version` entries of a publisher's legacy base catalog part, read
/// through the stable path pkg5 clients use.
fn catalog_entries(repo: &Path, publisher: &str) -> Vec<String> {
    let part = repo
        .join("publisher")
        .join(publisher)
        .join("catalog")
        .join("catalog.base.C");
    if !part.exists() {
        return vec![];
    }
    let json = read_json(&part);
    let mut out = vec![];
    if let Some(stems) = json.get(publisher).and_then(Value::as_object) {
        for (stem, versions) in stems {
            for v in versions.as_array().unwrap() {
                out.push(format!("{}@{}", stem, v["version"].as_str().unwrap()));
            }
        }
    }
    out.sort();
    out
}

fn entry(pkg: &Pkg) -> String {
    format!("{}@{}", pkg.stem, pkg.version)
}

fn manifest_path(repo: &Path, pkg: &Pkg) -> PathBuf {
    FileBackend::construct_manifest_path(repo, &pkg.publisher, &pkg.stem, &pkg.version)
}

/// Emulates a pkg5 client catalog refresh: fetch catalog.attrs, then each
/// part it lists, and validate every part's SHA-1 against catalog.attrs (as
/// pkg5's `Catalog.validate()` does). `between` runs after attrs were read.
fn pkg5_refresh(
    repo: &Path,
    publisher: &str,
    between: impl FnOnce(),
) -> Result<BTreeMap<String, Value>, String> {
    let dir = repo.join("publisher").join(publisher).join("catalog");
    let attrs = read_json(&dir.join("catalog.attrs"));
    between();
    let mut parts = BTreeMap::new();
    for (name, info) in attrs["parts"].as_object().unwrap() {
        let part = read_json(&dir.join(name));
        let expected = info["signature-sha-1"].as_str().unwrap();
        let actual = part["_SIGNATURE"]["sha-1"].as_str().unwrap();
        if expected != actual {
            return Err(format!(
                "{name}: signature {actual} != catalog.attrs {expected}"
            ));
        }
        parts.insert(name.clone(), part);
    }
    Ok(parts)
}

/// Everything a reader can observe for these publishers.
fn visible_state(repo: &Path, publishers: &[&str]) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    for p in publishers {
        let pdir = repo.join("publisher").join(p);
        for d in ["catalog", "catalog2", "pkg"] {
            let root = pdir.join(d);
            if !root.exists() {
                continue;
            }
            for e in walkdir::WalkDir::new(&root).follow_links(true) {
                let e = e.unwrap();
                if e.file_type().is_file() {
                    out.insert(e.path().to_path_buf(), fs::read(e.path()).unwrap());
                }
            }
        }
    }
    out
}

fn generation_dirs(repo: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(repo.join(GENERATIONS_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n != CURRENT_LINK)
        .collect();
    v.sort();
    v
}

fn base_pkgs() -> Vec<Pkg> {
    vec![
        Pkg::new("alpha", "web/hello"),
        Pkg::new("alpha", "legacy/gone").obsolete(),
        Pkg::new("beta", "library/world"),
    ]
}

#[test]
fn multi_publisher_archive_becomes_visible_in_one_generation() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let pkgs = base_pkgs();
    let archive = make_p5p(tmp.path(), "run1", &pkgs);

    let receipt = match import(&repo, &archive).unwrap() {
        Pkg5ImportOutcome::Committed(r) => r,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(receipt.added.len(), 3);
    assert_eq!(receipt.publishers, vec!["alpha", "beta"]);
    assert_eq!(receipt.generation, current_generation(&repo));

    // Legacy v1 catalog of both publishers, via the stable pathnames.
    assert_eq!(
        catalog_entries(&repo, "alpha"),
        vec![entry(&pkgs[1]), entry(&pkgs[0])]
    );
    assert_eq!(catalog_entries(&repo, "beta"), vec![entry(&pkgs[2])]);

    // pkg5 obsoletion stays in the main catalog, in the dependency part.
    let dep =
        fs::read_to_string(repo.join("publisher/alpha/catalog/catalog.dependency.C")).unwrap();
    assert!(dep.contains("legacy/gone") && dep.contains("set name=pkg.obsolete value=true"));
    assert!(!repo.join("obsoleted/alpha").exists());

    // Byte-exact manifests and payloads under the names pkg5 clients request.
    for pkg in &pkgs {
        assert_eq!(
            fs::read_to_string(manifest_path(&repo, pkg)).unwrap(),
            pkg.manifest()
        );
        for (_, content) in &pkg.files {
            let h = sha1_hex(content);
            let blob = FileBackend::construct_file_path_with_publisher(&repo, &pkg.publisher, &h);
            assert_eq!(fs::read(blob).unwrap(), gzip(content));
        }
    }

    // SQLite shards and search, and pkg5-consistent catalog parts.
    let backend = FileBackend::open(&repo).unwrap();
    assert!(backend.config.publishers.contains(&"beta".to_string()));
    let hits = backend
        .search_detailed("world", Some("beta"), None, false)
        .unwrap();
    assert!(hits.iter().any(|h| h.fmri.contains("library/world")));
    assert!(repo.join("publisher/alpha/catalog2/active.db").exists());
    pkg5_refresh(&repo, "alpha", || {}).unwrap();

    // Stable pathnames are links through the single switch point.
    for d in ["pkg", "catalog", "catalog2"] {
        let meta = fs::symlink_metadata(repo.join("publisher/alpha").join(d)).unwrap();
        assert!(meta.file_type().is_symlink(), "{d} is not a link");
    }
    assert!(
        repo.join(GENERATIONS_DIR)
            .join(&receipt.generation)
            .join("receipts")
            .join(format!("{}.json", receipt.archive_sha256.as_ref().unwrap()))
            .is_file()
    );
}

#[test]
fn identical_archive_retry_is_a_no_op() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let archive = make_p5p(tmp.path(), "run1", &base_pkgs());
    import(&repo, &archive).unwrap();
    let generation = current_generation(&repo);
    let before = visible_state(&repo, &["alpha", "beta"]);

    match import(&repo, &archive).unwrap() {
        Pkg5ImportOutcome::AlreadyImported(r) => assert_eq!(r.generation, generation),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(current_generation(&repo), generation);
    assert_eq!(visible_state(&repo, &["alpha", "beta"]), before);
}

#[test]
fn crash_after_switch_is_committed_and_retry_acknowledges() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let pkgs = base_pkgs();
    let archive = make_p5p(tmp.path(), "run1", &pkgs);

    // Renamed, not acknowledged: the new view and its receipt are durable.
    import_failing(&repo, &archive, FailPoint::AfterSwitch);
    assert_eq!(catalog_entries(&repo, "beta"), vec![entry(&pkgs[2])]);
    let generation = current_generation(&repo);

    match import(&repo, &archive).unwrap() {
        Pkg5ImportOutcome::AlreadyImported(r) => assert_eq!(r.generation, generation),
        other => panic!("unexpected {other:?}"),
    }
    // Recovery finished the post-switch configuration step.
    let backend = FileBackend::open(&repo).unwrap();
    assert!(backend.config.publishers.contains(&"alpha".to_string()));
    assert!(backend.config.publishers.contains(&"beta".to_string()));
    assert!(repo.join("publisher/beta/pub.p5i").is_file());
}

fn assert_crash_keeps_old_view(point: FailPoint) {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    import(&repo, &make_p5p(tmp.path(), "run1", &base_pkgs())).unwrap();
    let generation = current_generation(&repo);
    let before = visible_state(&repo, &["alpha", "beta", "gamma"]);

    let new_alpha = Pkg::new("alpha", "web/new");
    let new_gamma = Pkg::new("gamma", "tools/fresh");
    let run2 = make_p5p(tmp.path(), "run2", &[new_alpha.clone(), new_gamma.clone()]);
    import_failing(&repo, &run2, point);

    assert_eq!(current_generation(&repo), generation);
    assert_eq!(visible_state(&repo, &["alpha", "beta", "gamma"]), before);
    assert!(!manifest_path(&repo, &new_alpha).exists());
    assert!(!manifest_path(&repo, &new_gamma).exists());
    pkg5_refresh(&repo, "alpha", || {}).unwrap();

    // The retry recovers the debris and publishes the batch.
    assert!(matches!(
        import(&repo, &run2).unwrap(),
        Pkg5ImportOutcome::Committed(_)
    ));
    assert!(catalog_entries(&repo, "alpha").contains(&entry(&new_alpha)));
    assert_eq!(catalog_entries(&repo, "gamma"), vec![entry(&new_gamma)]);
    assert!(
        generation_dirs(&repo).iter().all(|n| !n.starts_with('.')),
        "leftovers: {:?}",
        generation_dirs(&repo)
    );
}

#[test]
fn crash_before_switch_keeps_old_view() {
    assert_crash_keeps_old_view(FailPoint::BeforeSwitch);
}

#[test]
fn crash_after_payloads_keeps_old_view() {
    assert_crash_keeps_old_view(FailPoint::AfterPayloads);
}

#[test]
fn conflicting_exact_fmri_rejects_the_whole_batch() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    import(&repo, &make_p5p(tmp.path(), "run1", &base_pkgs())).unwrap();
    let generation = current_generation(&repo);
    let before = visible_state(&repo, &["alpha", "beta"]);

    let changed = Pkg::new("alpha", "web/hello").file("usr/share/hello.txt", b"changed");
    let fresh = Pkg::new("beta", "library/fresh");
    let run2 = make_p5p(tmp.path(), "run2", &[changed.clone(), fresh.clone()]);
    match import(&repo, &run2) {
        Err(Pkg5ImportError::Conflicts { conflicts }) => {
            assert_eq!(conflicts.len(), 1);
            assert!(conflicts[0].contains(&changed.fmri()), "{conflicts:?}");
        }
        other => panic!("expected conflict, got {other:?}"),
    }
    assert_eq!(current_generation(&repo), generation);
    assert_eq!(visible_state(&repo, &["alpha", "beta"]), before);
    assert!(!manifest_path(&repo, &fresh).exists());
}

#[test]
fn identical_existing_packages_are_skipped() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let pkgs = base_pkgs();
    import(&repo, &make_p5p(tmp.path(), "run1", &pkgs)).unwrap();

    let newer = Pkg::new("alpha", "web/hello").version("2.0,5.11-2024.0.0.0:20240201T000000Z");
    let run2 = make_p5p(tmp.path(), "run2", &[pkgs[0].clone(), newer.clone()]);
    match import(&repo, &run2).unwrap() {
        Pkg5ImportOutcome::Committed(r) => {
            assert_eq!(r.added, vec![newer.fmri()]);
            assert_eq!(r.skipped, vec![pkgs[0].fmri()]);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        catalog_entries(&repo, "alpha"),
        vec![entry(&pkgs[1]), entry(&pkgs[0]), entry(&newer)]
    );
}

#[test]
fn missing_or_corrupt_payload_rejects_the_whole_batch() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    import(&repo, &make_p5p(tmp.path(), "run1", &base_pkgs())).unwrap();
    let generation = current_generation(&repo);

    let good = Pkg::new("alpha", "web/good");
    let broken = Pkg::new("alpha", "web/broken");
    for (name, damage) in [("missing", false), ("corrupt", true)] {
        let root = write_pkg5_repo(&tmp.path().join(name), &[good.clone(), broken.clone()]);
        let h = &broken.payload_hashes()[0];
        let blob = root.join("publisher/alpha/file").join(&h[0..2]).join(h);
        if damage {
            fs::write(&blob, gzip(b"tampered")).unwrap();
        } else {
            fs::remove_file(&blob).unwrap();
        }
        let archive = pkg5_fixture::pack_p5p(&root, &tmp.path().join(format!("{name}.p5p")));
        match import(&repo, &archive) {
            Err(Pkg5ImportError::InvalidSource { problems }) => {
                assert_eq!(problems.len(), 1, "{problems:?}");
                assert!(problems[0].contains(h), "{problems:?}");
            }
            other => panic!("expected invalid source, got {other:?}"),
        }
        assert_eq!(current_generation(&repo), generation);
        assert!(!manifest_path(&repo, &good).exists());
        let good_blob = FileBackend::construct_file_path_with_publisher(
            &repo,
            "alpha",
            &good.payload_hashes()[0],
        );
        assert!(!good_blob.exists(), "payload stored for a rejected batch");
    }
}

#[test]
fn readers_see_complete_old_then_complete_new_snapshot() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let pkgs = base_pkgs();
    import(&repo, &make_p5p(tmp.path(), "run1", &pkgs)).unwrap();
    let old_parts = pkg5_refresh(&repo, "alpha", || {}).unwrap();

    // A reader that resolved the old generation (open file, or a path
    // resolved before the switch) keeps a complete, consistent snapshot.
    let old_dir = fs::canonicalize(repo.join("publisher/alpha/catalog")).unwrap();
    let held = fs::File::open(old_dir.join("catalog.base.C")).unwrap();

    let newer = Pkg::new("alpha", "web/hello").version("2.0,5.11-2024.0.0.0:20240201T000000Z");
    let run2 = make_p5p(tmp.path(), "run2", std::slice::from_ref(&newer));

    // pkg5 refresh overlapping the switch: catalog.attrs from the old
    // generation, parts from the new one. pkg5's signature validation rejects
    // the mix instead of accepting a partial batch...
    let overlapping = pkg5_refresh(&repo, "alpha", || {
        import(&repo, &run2).unwrap();
    });
    let err = overlapping.expect_err("mixed catalog generations must not validate");
    assert!(err.contains("signature"), "{err}");

    // ...and the client's retry sees the complete new catalog.
    let new_parts = pkg5_refresh(&repo, "alpha", || {}).unwrap();
    assert_ne!(new_parts["catalog.base.C"], old_parts["catalog.base.C"]);
    assert!(catalog_entries(&repo, "alpha").contains(&entry(&newer)));
    assert!(manifest_path(&repo, &newer).is_file());

    // The previous generation is retained and unchanged.
    let old_attrs = read_json(&old_dir.join("catalog.attrs"));
    for (name, info) in old_attrs["parts"].as_object().unwrap() {
        let part = read_json(&old_dir.join(name));
        assert_eq!(part["_SIGNATURE"]["sha-1"], info["signature-sha-1"]);
        assert_eq!(part, old_parts[name]);
    }
    let held_part: Value = serde_json::from_reader(held).unwrap();
    assert_eq!(held_part, old_parts["catalog.base.C"]);
}

#[test]
fn legacy_repository_is_adopted_without_losing_packages() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let legacy = Pkg::new("alpha", "web/hello");
    // A repository written by the pre-generation importer layout: real
    // directories, manifests stored byte-exact, catalog rebuilt in place.
    {
        let mut backend = FileBackend::create(&repo, RepositoryVersion::V4).unwrap();
        backend.add_publisher("alpha").unwrap();
        let path = manifest_path(&repo, &legacy);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, legacy.manifest()).unwrap();
        let h = &legacy.payload_hashes()[0];
        let blob = FileBackend::construct_file_path_with_publisher(&repo, "alpha", h);
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::write(&blob, gzip(&legacy.files[0].1)).unwrap();
        backend.rebuild(Some("alpha"), false, false).unwrap();
    }
    assert!(
        fs::symlink_metadata(repo.join("publisher/alpha/catalog"))
            .unwrap()
            .is_dir()
    );
    assert_eq!(catalog_entries(&repo, "alpha"), vec![entry(&legacy)]);

    // Same package again (identical, skipped) plus a new one.
    let fresh = Pkg::new("alpha", "web/fresh");
    let run = make_p5p(tmp.path(), "run", &[legacy.clone(), fresh.clone()]);

    // Converting a populated legacy repository needs explicit consent, and
    // refusing it changes nothing.
    match import(&repo, &run) {
        Err(Pkg5ImportError::Generation(GenerationError::AdoptionRequired { .. })) => {}
        other => panic!("expected adoption to require consent, got {other:?}"),
    }
    assert!(!GenerationStore::is_managed(&repo));
    assert!(
        fs::symlink_metadata(repo.join("publisher/alpha/catalog"))
            .unwrap()
            .is_dir()
    );

    let adopt = Pkg5ImportOptions {
        adopt_existing: true,
        ..Default::default()
    };
    match import_pkg5(&repo, &run, &adopt).unwrap() {
        Pkg5ImportOutcome::Committed(r) => {
            assert_eq!(r.added, vec![fresh.fmri()]);
            assert_eq!(r.skipped, vec![legacy.fmri()]);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        catalog_entries(&repo, "alpha"),
        vec![entry(&fresh), entry(&legacy)]
    );
    let pdir = repo.join("publisher/alpha");
    for d in ["pkg", "catalog", "catalog2"] {
        assert!(
            fs::symlink_metadata(pdir.join(d))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!pdir.join(format!(".{d}.pre-generation")).exists());
    }
    assert_eq!(
        fs::read_to_string(manifest_path(&repo, &legacy)).unwrap(),
        legacy.manifest()
    );
}

#[test]
fn rebuild_publishes_a_new_generation() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let pkgs = base_pkgs();
    import(&repo, &make_p5p(tmp.path(), "run1", &pkgs)).unwrap();
    let before = current_generation(&repo);
    let entries = catalog_entries(&repo, "alpha");

    FileBackend::open(&repo)
        .unwrap()
        .rebuild(None, false, false)
        .unwrap();
    assert_ne!(current_generation(&repo), before);
    assert_eq!(catalog_entries(&repo, "alpha"), entries);
    pkg5_refresh(&repo, "beta", || {}).unwrap();
    // Receipts survive into later generations.
    let archive = tmp.path().join("run1.p5p");
    assert!(matches!(
        import(&repo, &archive).unwrap(),
        Pkg5ImportOutcome::AlreadyImported(_)
    ));
}

#[test]
fn writers_are_serialised_by_the_repository_lock() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    import(&repo, &make_p5p(tmp.path(), "run1", &base_pkgs())).unwrap();
    let run2 = make_p5p(tmp.path(), "run2", &[Pkg::new("alpha", "web/new")]);

    let held = GenerationStore::open(&repo).unwrap();
    let options = Pkg5ImportOptions {
        wait_for_lock: false,
        ..Default::default()
    };
    match import_pkg5(&repo, &run2, &options) {
        Err(Pkg5ImportError::Generation(GenerationError::Busy { .. })) => {}
        other => panic!("expected busy, got {other:?}"),
    }

    // A waiting writer proceeds once the lock is released.
    let waiter = {
        let repo = repo.clone();
        let run2 = run2.clone();
        std::thread::spawn(move || import(&repo, &run2))
    };
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!waiter.is_finished());
    drop(held);
    assert!(matches!(
        waiter.join().unwrap().unwrap(),
        Pkg5ImportOutcome::Committed(_)
    ));
}

#[test]
fn directory_source_and_generation_retention() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let src = write_pkg5_repo(&tmp.path().join("src"), &base_pkgs());
    assert!(matches!(
        import(&repo, &src).unwrap(),
        Pkg5ImportOutcome::Committed(_)
    ));
    match import(&repo, &src).unwrap() {
        Pkg5ImportOutcome::NothingToImport { skipped } => assert_eq!(skipped.len(), 3),
        other => panic!("unexpected {other:?}"),
    }
    for i in 0..4 {
        let pkg = Pkg::new("beta", &format!("extra/p{i}"));
        import(&repo, &make_p5p(tmp.path(), &format!("x{i}"), &[pkg])).unwrap();
    }
    // Current plus the retained predecessors only.
    assert_eq!(
        generation_dirs(&repo).len(),
        3,
        "{:?}",
        generation_dirs(&repo)
    );
    assert_eq!(catalog_entries(&repo, "beta").len(), 5);
}

#[test]
fn directory_retry_restores_a_missing_payload_of_a_published_package() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let pkgs = base_pkgs();
    let src = write_pkg5_repo(&tmp.path().join("src"), &pkgs);
    import(&repo, &src).unwrap();
    let h = pkgs[0].payload_hashes()[0].clone();
    let blob = FileBackend::construct_file_path_with_publisher(&repo, "alpha", &h);
    let served = fs::read(&blob).unwrap();
    let generation = current_generation(&repo);
    let before = visible_state(&repo, &["alpha", "beta"]);

    let generations = generation_dirs(&repo);

    fs::remove_file(&blob).unwrap();
    match import(&repo, &src).unwrap() {
        Pkg5ImportOutcome::Repaired { repaired, skipped } => {
            assert_eq!(repaired, vec![h.clone()]);
            assert_eq!(skipped.len(), 3);
        }
        other => panic!("expected the missing payload to be restored, got {other:?}"),
    }
    assert_eq!(fs::read(&blob).unwrap(), served);
    // Restoring a payload changes no metadata: no generation is staged.
    assert_eq!(current_generation(&repo), generation);
    assert_eq!(generation_dirs(&repo), generations);
    assert_eq!(visible_state(&repo, &["alpha", "beta"]), before);

    match import(&repo, &src).unwrap() {
        Pkg5ImportOutcome::NothingToImport { skipped } => assert_eq!(skipped.len(), 3),
        other => panic!("expected a whole repository to make the retry a no-op, got {other:?}"),
    }
    assert_eq!(current_generation(&repo), generation);
    assert_eq!(generation_dirs(&repo), generations);
    assert_eq!(fs::read(&blob).unwrap(), served);
}

#[test]
fn directory_retry_fails_when_a_missing_payload_is_not_in_the_source() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let pkgs = base_pkgs();
    let src = write_pkg5_repo(&tmp.path().join("src"), &pkgs);
    import(&repo, &src).unwrap();
    let h = pkgs[0].payload_hashes()[0].clone();
    let blob = FileBackend::construct_file_path_with_publisher(&repo, "alpha", &h);
    let generation = current_generation(&repo);
    let generations = generation_dirs(&repo);
    let before = visible_state(&repo, &["alpha", "beta"]);

    fs::remove_file(&blob).unwrap();
    fs::remove_file(src.join("publisher/alpha/file").join(&h[0..2]).join(&h)).unwrap();
    match import(&repo, &src) {
        Err(Pkg5ImportError::InvalidSource { problems }) => {
            assert!(
                problems.iter().any(|p| p.contains(&h)
                    && p.contains(&pkgs[0].stem)
                    && p.contains("missing from the source and the repository")),
                "{problems:?}"
            );
        }
        other => panic!("expected the unrepairable payload to be reported, got {other:?}"),
    }
    assert!(!blob.exists());
    assert_eq!(current_generation(&repo), generation);
    assert_eq!(generation_dirs(&repo), generations);
    assert_eq!(visible_state(&repo, &["alpha", "beta"]), before);
}

#[test]
fn directory_import_with_new_packages_also_restores_missing_payloads() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let mut pkgs = base_pkgs();
    import(&repo, &write_pkg5_repo(&tmp.path().join("src1"), &pkgs)).unwrap();
    let h = pkgs[2].payload_hashes()[0].clone();
    let blob = FileBackend::construct_file_path_with_publisher(&repo, "beta", &h);
    let served = fs::read(&blob).unwrap();
    let generation = current_generation(&repo);

    fs::remove_file(&blob).unwrap();
    let fresh = Pkg::new("alpha", "web/new");
    pkgs.push(fresh.clone());
    let src = write_pkg5_repo(&tmp.path().join("src2"), &pkgs);
    match import(&repo, &src).unwrap() {
        Pkg5ImportOutcome::Committed(r) => {
            assert_eq!(r.added.len(), 1, "{r:?}");
            assert!(r.added[0].contains(&fresh.stem), "{r:?}");
            assert_eq!(r.skipped.len(), 3);
        }
        other => panic!("expected the new package to be committed, got {other:?}"),
    }
    assert_ne!(current_generation(&repo), generation);
    assert_eq!(fs::read(&blob).unwrap(), served);

    let committed = current_generation(&repo);
    match import(&repo, &src).unwrap() {
        Pkg5ImportOutcome::NothingToImport { skipped } => assert_eq!(skipped.len(), 4),
        other => panic!("expected the retry to be a no-op, got {other:?}"),
    }
    assert_eq!(current_generation(&repo), committed);
}

const V3_PREFIX: &str = "opensolaris.org";
const V3_VERSION: &str = "0.5.11,5.11-0.111:20090518T184618Z";

fn v3_pkg(stem: &str) -> Pkg {
    Pkg::new(V3_PREFIX, stem).version(V3_VERSION)
}

/// A manifest as the legacy version 3 repository holds it: a comment
/// header naming the package without its timestamp, and `pkg.fmri` only if
/// `with_fmri`.
fn v3_manifest(pkg: &Pkg, with_fmri: bool) -> String {
    let untimed = pkg.version.split(':').next().unwrap();
    let mut m = format!("# pkg:/{}@{}\n", pkg.stem, untimed);
    for line in pkg.manifest().lines() {
        if !with_fmri && line.starts_with("set name=pkg.fmri ") {
            continue;
        }
        m.push_str(line);
        m.push('\n');
    }
    m
}

/// A flat pkg5 version 3 repository: top-level cfg_cache, pkg/, file/.
fn write_v3_repo(root: &Path, prefix: &str) -> PathBuf {
    for d in ["pkg", "file", "catalog"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    fs::write(
        root.join("cfg_cache"),
        format!("[publisher]\nalias = None\nprefix = {prefix}\n\n[repository]\nname = Legacy\n"),
    )
    .unwrap();
    root.to_path_buf()
}

/// Store `manifest` under `pkg/<stem_name>/<version_name>` (already
/// encoded) and the package's payloads under `file/`.
fn add_v3(root: &Path, pkg: &Pkg, stem_name: &str, version_name: &str, manifest: &str) {
    let dir = root.join("pkg").join(stem_name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(version_name), manifest).unwrap();
    for (_, content) in &pkg.files {
        let h = sha1_hex(content);
        let fdir = root.join("file").join(&h[0..2]);
        fs::create_dir_all(&fdir).unwrap();
        fs::write(fdir.join(&h), gzip(content)).unwrap();
    }
}

fn add_v3_pkg(root: &Path, pkg: &Pkg, with_fmri: bool) {
    add_v3(
        root,
        pkg,
        &quote(&pkg.stem),
        &quote(&pkg.version),
        &v3_manifest(pkg, with_fmri),
    );
}

fn invalid_source(result: Result<Pkg5ImportOutcome, Pkg5ImportError>) -> Vec<String> {
    match result {
        Err(Pkg5ImportError::InvalidSource { problems }) => problems,
        other => panic!("expected an invalid source, got {other:?}"),
    }
}

#[test]
fn flat_v3_source_is_identified_by_path_and_config() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let src = write_v3_repo(&tmp.path().join("legacy"), V3_PREFIX);
    let cs = v3_pkg("SUNWcs");
    let explicit = v3_pkg("web/server");
    let plus = v3_pkg("library/c++");
    let empty = v3_pkg("legacy/empty").obsolete();
    add_v3_pkg(&src, &cs, false);
    add_v3_pkg(&src, &explicit, true);
    // pkg5 quotes `+`, but a literal `+` in a name is a plus, not a space.
    add_v3(
        &src,
        &plus,
        "library%2Fc++",
        &quote(V3_VERSION),
        &v3_manifest(&plus, false),
    );
    let comment_only = "# pkg:/legacy/empty@0.5.11,5.11-0.111\n";
    add_v3(
        &src,
        &empty,
        &quote(&empty.stem),
        &quote(V3_VERSION),
        comment_only,
    );

    let pkgs = [&cs, &explicit, &plus, &empty];
    match import(&repo, &src).unwrap() {
        Pkg5ImportOutcome::Committed(r) => {
            let mut expected: Vec<String> = pkgs.iter().map(|p| p.fmri()).collect();
            expected.sort();
            let mut added = r.added.clone();
            added.sort();
            assert_eq!(added, expected);
            assert_eq!(r.publishers, vec![V3_PREFIX]);
        }
        other => panic!("expected the V3 source to be committed, got {other:?}"),
    }
    // Versions keep the timestamp the path gives, not the comment's.
    let mut entries: Vec<String> = pkgs.iter().map(|p| entry(p)).collect();
    entries.sort();
    assert_eq!(catalog_entries(&repo, V3_PREFIX), entries);
    pkg5_refresh(&repo, V3_PREFIX, || {}).unwrap();

    // Byte-exact manifests, comments included, and verified payloads.
    assert_eq!(
        fs::read_to_string(manifest_path(&repo, &cs)).unwrap(),
        v3_manifest(&cs, false)
    );
    assert_eq!(
        fs::read_to_string(manifest_path(&repo, &explicit)).unwrap(),
        v3_manifest(&explicit, true)
    );
    assert_eq!(
        fs::read_to_string(manifest_path(&repo, &empty)).unwrap(),
        comment_only
    );
    for pkg in [&cs, &explicit, &plus] {
        for h in pkg.payload_hashes() {
            let blob = FileBackend::construct_file_path_with_publisher(&repo, V3_PREFIX, &h);
            assert_eq!(fs::read(blob).unwrap(), gzip(&pkg.files[0].1));
        }
    }

    // A retry is a no-op.
    let generation = current_generation(&repo);
    let before = visible_state(&repo, &[V3_PREFIX]);
    match import(&repo, &src).unwrap() {
        Pkg5ImportOutcome::NothingToImport { skipped } => assert_eq!(skipped.len(), 4),
        other => panic!("expected the retry to be a no-op, got {other:?}"),
    }
    assert_eq!(current_generation(&repo), generation);
    assert_eq!(visible_state(&repo, &[V3_PREFIX]), before);
}

#[test]
fn flat_v3_explicit_fmri_must_agree_with_its_path() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let src = write_v3_repo(&tmp.path().join("legacy"), V3_PREFIX);
    let pkg = v3_pkg("web/server");
    let other = v3_pkg("web/other");
    // The manifest names web/other but is stored as web/server.
    add_v3(
        &src,
        &pkg,
        &quote(&pkg.stem),
        &quote(&pkg.version),
        &v3_manifest(&other, true),
    );
    let problems = invalid_source(import(&repo, &src));
    assert!(
        problems
            .iter()
            .any(|p| p.contains("does not match") && p.contains(&other.fmri())),
        "{problems:?}"
    );
    assert!(catalog_entries(&repo, V3_PREFIX).is_empty());
}

#[test]
fn flat_v3_and_v4_publishers_stay_independent() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let oi = Pkg::new("openindiana.org", "web/server");
    import(
        &repo,
        &write_pkg5_repo(&tmp.path().join("hipster"), &[oi.clone()]),
    )
    .unwrap();
    let oi_before = visible_state(&repo, &["openindiana.org"]);

    // Same stem, other publisher.
    let legacy = v3_pkg("web/server");
    let src = write_v3_repo(&tmp.path().join("legacy"), V3_PREFIX);
    add_v3_pkg(&src, &legacy, false);
    match import(&repo, &src).unwrap() {
        Pkg5ImportOutcome::Committed(r) => {
            assert_eq!(r.added, vec![legacy.fmri()]);
            assert_eq!(r.publishers, vec![V3_PREFIX]);
        }
        other => panic!("expected the V3 source to be committed, got {other:?}"),
    }
    assert_eq!(catalog_entries(&repo, V3_PREFIX), vec![entry(&legacy)]);
    assert_eq!(catalog_entries(&repo, "openindiana.org"), vec![entry(&oi)]);
    assert_eq!(visible_state(&repo, &["openindiana.org"]), oi_before);
    let backend = FileBackend::open(&repo).unwrap();
    for p in ["openindiana.org", V3_PREFIX] {
        assert!(backend.config.publishers.contains(&p.to_string()), "{p}");
    }
}

#[test]
fn flat_v3_bad_config_is_rejected() {
    let bad = [
        "[repository]\nname = x\n",
        "[publisher]\nalias = x\n",
        "[publisher]\nprefix =\n",
        "[publisher]\nprefix = None\n",
        "[publisher]\nprefix = a.org\nprefix = b.org\n",
        "[publisher]\nprefix = a.org\n[publisher]\nprefix = a.org\n",
        "[publisher]\nprefix = ../escape\n",
    ];
    for (i, cfg) in bad.iter().enumerate() {
        let tmp = TempDir::new().unwrap();
        let src = write_v3_repo(&tmp.path().join("legacy"), V3_PREFIX);
        add_v3_pkg(&src, &v3_pkg("SUNWcs"), false);
        fs::write(src.join("cfg_cache"), cfg).unwrap();
        let problems = invalid_source(import(&tmp.path().join("repo"), &src));
        assert!(
            problems.iter().any(|p| p.starts_with("cfg_cache:")),
            "case {i}: {problems:?}"
        );
    }
}

#[test]
fn flat_v3_bad_paths_are_rejected() {
    let pkg = v3_pkg("SUNWcs");
    let version = quote(V3_VERSION);
    let version = version.as_str();
    let cases = [
        ("bad%2", version),
        ("bad%zz", version),
        ("%FF", version),
        ("a%2F..%2Fb", version),
        ("%2Fabs", version),
        ("ctl%0A", version),
        ("back%5Cslash", version),
        ("with%20space", version),
        ("per%25cent", version),
        ("caf%C3%A9", version),
        ("a%2F.%2Fb", version),
        ("SUNWcs", "0.5.11%2C5.11-0.111%3"),
        ("SUNWcs", "0.5.11%2F..%2Fx"),
        ("SUNWcs", "not%20a%20version"),
    ];
    for (stem_name, version_name) in cases {
        let tmp = TempDir::new().unwrap();
        let src = write_v3_repo(&tmp.path().join("legacy"), V3_PREFIX);
        add_v3(
            &src,
            &pkg,
            stem_name,
            version_name,
            &v3_manifest(&pkg, false),
        );
        let repo = tmp.path().join("repo");
        let problems = invalid_source(import(&repo, &src));
        assert!(
            problems.iter().any(|p| p.contains(stem_name)),
            "{stem_name}/{version_name}: {problems:?}"
        );
        assert!(catalog_entries(&repo, V3_PREFIX).is_empty());
    }
}

#[test]
fn flat_v3_empty_manifests_are_rejected() {
    let pkg = v3_pkg("SUNWcs");
    for manifest in ["", " \n\t\n"] {
        let tmp = TempDir::new().unwrap();
        let src = write_v3_repo(&tmp.path().join("legacy"), V3_PREFIX);
        add_v3(
            &src,
            &pkg,
            &quote(&pkg.stem),
            &quote(&pkg.version),
            manifest,
        );
        let repo = tmp.path().join("repo");
        let problems = invalid_source(import(&repo, &src));
        assert!(
            problems.iter().any(|p| p.contains("manifest is empty")),
            "{manifest:?}: {problems:?}"
        );
        assert!(catalog_entries(&repo, V3_PREFIX).is_empty());
    }
}

#[test]
fn flat_v3_two_level_payload_layout_is_read() {
    let tmp = TempDir::new().unwrap();
    let src = write_v3_repo(&tmp.path().join("legacy"), V3_PREFIX);
    let pkg = v3_pkg("SUNWcs");
    add_v3_pkg(&src, &pkg, false);
    // Older pkg5 stored payloads as file/<first2>/<next6>/<hash>.
    for h in pkg.payload_hashes() {
        let flat = src.join("file").join(&h[0..2]).join(&h);
        let nested = src.join("file").join(&h[0..2]).join(&h[2..8]);
        fs::create_dir_all(&nested).unwrap();
        fs::rename(&flat, nested.join(&h)).unwrap();
    }
    let repo = tmp.path().join("repo");
    match import(&repo, &src).unwrap() {
        Pkg5ImportOutcome::Committed(r) => assert_eq!(r.added, vec![pkg.fmri()]),
        other => panic!("expected the V3 source to be committed, got {other:?}"),
    }
    assert_eq!(catalog_entries(&repo, V3_PREFIX), vec![entry(&pkg)]);
    for h in pkg.payload_hashes() {
        let blob = FileBackend::construct_file_path_with_publisher(&repo, V3_PREFIX, &h);
        assert_eq!(fs::read(blob).unwrap(), gzip(&pkg.files[0].1));
    }
}

#[test]
fn flat_v3_incomplete_or_mixed_trees_are_rejected() {
    let tmp = TempDir::new().unwrap();
    let pkg = v3_pkg("SUNWcs");
    let fresh = || {
        let dir = TempDir::new_in(tmp.path()).unwrap().keep();
        let src = write_v3_repo(&dir, V3_PREFIX);
        add_v3_pkg(&src, &pkg, false);
        src
    };

    // Mixed with version 4 entries.
    let src = fresh();
    fs::write(src.join("pkg5.repository"), "[repository]\nversion = 4\n").unwrap();
    let problems = invalid_source(import(&tmp.path().join("r1"), &src));
    assert!(problems.iter().any(|p| p.contains("mixed")), "{problems:?}");
    let src = fresh();
    fs::create_dir_all(src.join("publisher").join(V3_PREFIX).join("pkg")).unwrap();
    let problems = invalid_source(import(&tmp.path().join("r2"), &src));
    assert!(problems.iter().any(|p| p.contains("mixed")), "{problems:?}");

    // Incomplete: no file/ directory, cfg_cache not a regular file.
    let src = fresh();
    fs::remove_dir_all(src.join("file")).unwrap();
    let problems = invalid_source(import(&tmp.path().join("r3"), &src));
    assert!(
        problems.iter().any(|p| p == "file: missing"),
        "{problems:?}"
    );
    let src = fresh();
    fs::remove_file(src.join("cfg_cache")).unwrap();
    fs::create_dir(src.join("cfg_cache")).unwrap();
    let problems = invalid_source(import(&tmp.path().join("r4"), &src));
    assert!(
        problems
            .iter()
            .any(|p| p == "cfg_cache: not a regular file"),
        "{problems:?}"
    );
}

#[cfg(unix)]
#[test]
fn flat_v3_links_and_missing_payloads_are_rejected() {
    use std::os::unix::fs::symlink;
    let tmp = TempDir::new().unwrap();
    let pkg = v3_pkg("SUNWcs");
    let fresh = || {
        let dir = TempDir::new_in(tmp.path()).unwrap().keep();
        let src = write_v3_repo(&dir, V3_PREFIX);
        add_v3_pkg(&src, &pkg, false);
        src
    };
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("cfg"), "[publisher]\nprefix = other.org\n").unwrap();

    let cases: Vec<(&str, Box<dyn Fn(&Path) + '_>)> = vec![
        (
            "cfg_cache",
            Box::new(|s: &Path| {
                fs::remove_file(s.join("cfg_cache")).unwrap();
                symlink(outside.join("cfg"), s.join("cfg_cache")).unwrap();
            }),
        ),
        (
            "file",
            Box::new(|s: &Path| {
                fs::rename(s.join("file"), outside.join("file")).unwrap();
                symlink(outside.join("file"), s.join("file")).unwrap();
            }),
        ),
        (
            "pkg/linked",
            Box::new(|s: &Path| symlink(&outside, s.join("pkg/linked")).unwrap()),
        ),
        (
            "file/",
            Box::new(|s: &Path| {
                let h = pkg.payload_hashes()[0].clone();
                let blob = s.join("file").join(&h[0..2]).join(&h);
                let moved = outside.join(&h);
                fs::rename(&blob, &moved).unwrap();
                symlink(&moved, &blob).unwrap();
            }),
        ),
    ];
    for (i, (what, break_it)) in cases.iter().enumerate() {
        let src = fresh();
        break_it(src.as_path());
        let repo = tmp.path().join(format!("repo{i}"));
        let problems = invalid_source(import(&repo, &src));
        assert!(
            problems.iter().any(|p| p.starts_with(what)),
            "{what}: {problems:?}"
        );
        assert!(catalog_entries(&repo, V3_PREFIX).is_empty());
        assert!(catalog_entries(&repo, "other.org").is_empty());
    }

    // A payload the manifest references but the source lacks.
    let src = fresh();
    let h = pkg.payload_hashes()[0].clone();
    fs::remove_file(src.join("file").join(&h[0..2]).join(&h)).unwrap();
    let repo = tmp.path().join("missing");
    let problems = invalid_source(import(&repo, &src));
    assert!(
        problems
            .iter()
            .any(|p| p.contains(&h) && p.contains("missing from the source and the repository")),
        "{problems:?}"
    );
    assert!(catalog_entries(&repo, V3_PREFIX).is_empty());
}

fn shard_rows(repo: &Path, publisher: &str, db: &str, sql: &str) -> Vec<String> {
    let path = repo
        .join("publisher")
        .join(publisher)
        .join("catalog2")
        .join(db);
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn obsolete_packages_are_not_installable_or_searchable() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    import(&repo, &make_p5p(tmp.path(), "run1", &base_pkgs())).unwrap();

    let active = shard_rows(&repo, "alpha", "active.db", "SELECT stem FROM packages");
    assert_eq!(active, vec!["web/hello"]);
    let obsolete = shard_rows(
        &repo,
        "alpha",
        "obsolete.db",
        "SELECT stem FROM obsolete_packages",
    );
    assert_eq!(obsolete, vec!["legacy/gone"]);
    let fts = shard_rows(&repo, "alpha", "fts.db", "SELECT stem FROM package_search");
    assert!(!fts.contains(&"legacy/gone".to_string()), "{fts:?}");
    let backend = FileBackend::open(&repo).unwrap();
    assert!(
        backend
            .search_detailed("gone", Some("alpha"), None, false)
            .unwrap()
            .is_empty()
    );

    // Still published: listed (like `pkgrepo list`) and in the v1 catalog.
    let listed: Vec<String> = backend
        .list_packages(Some("alpha"), None)
        .unwrap()
        .into_iter()
        .map(|p| p.fmri.stem().to_string())
        .collect();
    assert!(listed.contains(&"legacy/gone".to_string()), "{listed:?}");
}

#[test]
fn legacy_writers_are_rejected_on_managed_repositories_before_mutation() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let mut backend = FileBackend::create(&repo, RepositoryVersion::V4).unwrap();
    backend.add_publisher("alpha").unwrap();

    // A transaction that began before the repository became managed...
    let content = tmp.path().join("payload.txt");
    fs::write(&content, b"legacy").unwrap();
    let mut tx = backend.begin_transaction().unwrap();
    tx.set_publisher("alpha");
    let mut fa = libips::actions::File::read_from_path(&content).unwrap();
    fa.path = "legacy.txt".to_string();
    tx.add_file(fa, &content).unwrap();
    let mut manifest = libips::actions::Manifest::new();
    manifest.attributes.push(libips::actions::Attr {
        key: "pkg.fmri".to_string(),
        values: vec!["pkg://alpha/legacy/pkg@1.0".to_string()],
        properties: Default::default(),
    });
    tx.update_manifest(manifest);

    // ...an import converts the (still empty) repository meanwhile...
    let pkgs = base_pkgs();
    import(&repo, &make_p5p(tmp.path(), "run1", &pkgs)).unwrap();
    let generation = current_generation(&repo);
    let before = visible_state(&repo, &["alpha", "beta"]);

    // ...so its commit fails under the lock without touching the generation.
    match tx.commit() {
        Err(libips::repository::RepositoryError::Generation(e)) => {
            assert!(
                matches!(*e, GenerationError::ManagedRepository { .. }),
                "{e:?}"
            )
        }
        other => panic!("expected managed-repository rejection, got {other:?}"),
    }
    assert!(!FileBackend::construct_package_dir(&repo, "alpha", "legacy/pkg").exists());
    assert_eq!(current_generation(&repo), generation);
    assert_eq!(visible_state(&repo, &["alpha", "beta"]), before);

    // New transactions and in-place catalog writes are refused up front.
    let backend = FileBackend::open(&repo).unwrap();
    assert!(backend.begin_transaction().is_err());
    let mut attrs = libips::repository::catalog::CatalogAttrs::new();
    assert!(backend.save_catalog_attrs("alpha", &mut attrs).is_err());
    assert_eq!(visible_state(&repo, &["alpha", "beta"]), before);
}

#[test]
fn legacy_writer_lock_serialises_with_first_adoption() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let mut backend = FileBackend::create(&repo, RepositoryVersion::V4).unwrap();
    backend.add_publisher("alpha").unwrap();

    // An in-place writer (transaction commit, legacy rebuild) holds the
    // repository writer lock while it mutates the live directories.
    let lock = fs::OpenOptions::new()
        .write(true)
        .open(repo.join(libips::repository::generation::WRITER_LOCK_FILE))
        .unwrap();
    lock.lock().unwrap();

    let run1 = make_p5p(tmp.path(), "run1", &base_pkgs());
    let importer = {
        let repo = repo.clone();
        std::thread::spawn(move || import(&repo, &run1))
    };
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        !importer.is_finished(),
        "adoption did not wait for the writer"
    );
    assert!(!GenerationStore::is_managed(&repo));
    lock.unlock().unwrap();
    assert!(matches!(
        importer.join().unwrap().unwrap(),
        Pkg5ImportOutcome::Committed(_)
    ));

    // Once managed, rebuild goes through a generation instead of in place.
    let before = current_generation(&repo);
    FileBackend::open(&repo)
        .unwrap()
        .rebuild(None, false, false)
        .unwrap();
    assert_ne!(current_generation(&repo), before);
}

#[test]
fn publisher_removal_and_addition_are_atomic_and_stick() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let pkgs = base_pkgs();
    let run1 = make_p5p(tmp.path(), "run1", &pkgs);
    import(&repo, &run1).unwrap();

    let mut backend = FileBackend::open(&repo).unwrap();
    backend.remove_publisher("beta", false).unwrap();
    assert!(!backend.config.publishers.contains(&"beta".to_string()));
    let check_removed = |repo: &Path| {
        let config = FileBackend::open(repo).unwrap().config;
        assert_eq!(config.publishers, vec!["alpha"]);
        assert_eq!(config.default_publisher.as_deref(), Some("alpha"));
        for d in ["pkg", "catalog", "catalog2"] {
            assert!(fs::symlink_metadata(repo.join("publisher/beta").join(d)).is_err());
        }
        let store = GenerationStore::open(repo).unwrap();
        assert_eq!(store.current().publishers, vec!["alpha"]);
    };
    check_removed(&repo);

    // Neither a rebuild nor recovery resurrects it.
    FileBackend::open(&repo)
        .unwrap()
        .rebuild(None, false, false)
        .unwrap();
    check_removed(&repo);
    assert_eq!(catalog_entries(&repo, "alpha").len(), 2);

    // Adding a publisher publishes it (empty) atomically.
    let mut backend = FileBackend::open(&repo).unwrap();
    backend.add_publisher("gamma").unwrap();
    let store = GenerationStore::open(&repo).unwrap();
    assert_eq!(store.current().publishers, vec!["alpha", "gamma"]);
    drop(store);
    assert!(
        fs::symlink_metadata(repo.join("publisher/gamma/catalog"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(repo.join("publisher/gamma/catalog/catalog.attrs").is_file());

    // The removed publisher's packages can be imported again.
    let run2 = make_p5p(tmp.path(), "run2", &[pkgs[2].clone()]);
    import(&repo, &run2).unwrap();
    assert_eq!(catalog_entries(&repo, "beta"), vec![entry(&pkgs[2])]);
}

#[test]
fn receipts_merge_narrower_publisher_selections() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let pkgs = base_pkgs();
    let archive = make_p5p(tmp.path(), "run1", &pkgs);
    let only = |sel: &[&str]| Pkg5ImportOptions {
        publishers: Some(sel.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    };

    // A duplicated selection is still only "alpha".
    match import_pkg5(&repo, &archive, &only(&["alpha", "alpha"])).unwrap() {
        Pkg5ImportOutcome::Committed(r) => {
            assert_eq!(r.publishers, vec!["alpha"]);
            assert!(!r.all_publishers);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(
        import_pkg5(&repo, &archive, &only(&["alpha"])).unwrap(),
        Pkg5ImportOutcome::AlreadyImported(_)
    ));
    // The full import still has work to do, and its receipt covers both.
    match import(&repo, &archive).unwrap() {
        Pkg5ImportOutcome::Committed(r) => {
            assert_eq!(r.publishers, vec!["alpha", "beta"]);
            assert!(r.all_publishers);
            assert!(r.added.contains(&pkgs[0].fmri()) && r.added.contains(&pkgs[2].fmri()));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(catalog_entries(&repo, "beta"), vec![entry(&pkgs[2])]);
    for sel in [None, Some(only(&["beta"]))] {
        let options = sel.unwrap_or_default();
        assert!(matches!(
            import_pkg5(&repo, &archive, &options).unwrap(),
            Pkg5ImportOutcome::AlreadyImported(_)
        ));
    }
}

#[test]
fn source_layouts_and_names_are_validated() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");

    // pkg5's original V0 payload layout: file/<h[0:2]>/<h[2:8]>/<h>.
    let pkg = Pkg::new("alpha", "web/v0");
    let root = write_pkg5_repo(&tmp.path().join("v0"), std::slice::from_ref(&pkg));
    let h = &pkg.payload_hashes()[0];
    let file_dir = root.join("publisher/alpha/file");
    fs::create_dir_all(file_dir.join(&h[0..2]).join(&h[2..8])).unwrap();
    fs::rename(
        file_dir.join(&h[0..2]).join(h),
        file_dir.join(&h[0..2]).join(&h[2..8]).join(h),
    )
    .unwrap();
    import(&repo, &root).unwrap();
    assert_eq!(catalog_entries(&repo, "alpha"), vec![entry(&pkg)]);

    // A publisher directory that is not a publisher prefix rejects the batch.
    let bad = write_pkg5_repo(&tmp.path().join("bad"), &[Pkg::new("alpha", "web/other")]);
    fs::create_dir_all(bad.join("publisher/-evil/pkg")).unwrap();
    match import(&repo, &bad) {
        Err(Pkg5ImportError::InvalidSource { problems }) => {
            assert!(problems[0].contains("-evil"), "{problems:?}")
        }
        other => panic!("expected invalid source, got {other:?}"),
    }
    assert_eq!(catalog_entries(&repo, "alpha"), vec![entry(&pkg)]);
}

#[test]
fn catalog_mtime_increases_across_quick_publications() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let attrs = repo.join("publisher/alpha/catalog/catalog.attrs");
    let mut last = 0;
    for i in 0..3 {
        let pkg = Pkg::new("alpha", &format!("quick/p{i}"));
        import(&repo, &make_p5p(tmp.path(), &format!("q{i}"), &[pkg])).unwrap();
        let secs = fs::metadata(&attrs)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            secs > last,
            "catalog.attrs mtime did not advance: {secs} <= {last}"
        );
        last = secs;
    }
}

fn catalog_attrs_mtime(repo: &Path, publisher: &str) -> std::time::SystemTime {
    fs::metadata(
        repo.join("publisher")
            .join(publisher)
            .join("catalog/catalog.attrs"),
    )
    .unwrap()
    .modified()
    .unwrap()
}

#[test]
fn removed_publisher_is_reimported_from_the_same_archive() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let pkgs = base_pkgs();
    let run1 = make_p5p(tmp.path(), "run1", &pkgs);
    import(&repo, &run1).unwrap();
    let beta_mtime = catalog_attrs_mtime(&repo, "beta");

    FileBackend::open(&repo)
        .unwrap()
        .remove_publisher("beta", false)
        .unwrap();
    assert!(catalog_entries(&repo, "beta").is_empty());

    // The receipt no longer claims beta, so the same archive publishes it
    // again (alpha's identical packages are skipped).
    match import(&repo, &run1).unwrap() {
        Pkg5ImportOutcome::Committed(r) => {
            // Merged with the narrowed receipt, it covers both runs again.
            assert!(r.added.contains(&pkgs[2].fmri()), "{r:?}");
            assert!(r.skipped.contains(&pkgs[0].fmri()), "{r:?}");
            assert!(r.all_publishers);
        }
        other => panic!("expected beta to be re-imported, got {other:?}"),
    }
    assert_eq!(catalog_entries(&repo, "beta"), vec![entry(&pkgs[2])]);
    assert!(matches!(
        import(&repo, &run1).unwrap(),
        Pkg5ImportOutcome::AlreadyImported(_)
    ));
    // The re-added publisher's catalog is newer than its removed one, so a
    // client revalidating with If-Modified-Since refetches it.
    assert!(catalog_attrs_mtime(&repo, "beta") > beta_mtime);

    // Removing the only publisher of a receipt drops that receipt.
    let solo = make_p5p(tmp.path(), "solo", &[Pkg::new("gamma", "tools/solo")]);
    import(&repo, &solo).unwrap();
    FileBackend::open(&repo)
        .unwrap()
        .remove_publisher("gamma", false)
        .unwrap();
    assert!(matches!(
        import(&repo, &solo).unwrap(),
        Pkg5ImportOutcome::Committed(_)
    ));
}

/// A repository that must stay untouched by hostile archives.
fn victim(tmp: &Path) -> (PathBuf, PathBuf, Vec<u8>) {
    let content = b"victim payload".to_vec();
    let h = sha1_hex(&content);
    let file_dir = tmp.join("victim/publisher/x/file");
    let blob = file_dir.join(&h[0..2]).join(&h);
    fs::create_dir_all(blob.parent().unwrap()).unwrap();
    fs::write(&blob, gzip(&content)).unwrap();
    (file_dir, blob, content)
}

fn tar_header(kind: tar::EntryType, size: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_size(size);
    header.set_mode(0o644);
    header
}

/// Write an archive whose entry names bypass the tar crate's builder checks.
fn hostile_p5p(out: &Path, pkg: &Pkg, extra: impl FnOnce(&mut tar::Builder<fs::File>)) {
    let mut builder = tar::Builder::new(fs::File::create(out).unwrap());
    let repo_conf = b"[publisher]\nprefix = alpha\n";
    let mut h = tar_header(tar::EntryType::Regular, repo_conf.len() as u64);
    builder
        .append_data(&mut h, "pkg5.repository", &repo_conf[..])
        .unwrap();
    let manifest = pkg.manifest();
    let name = format!(
        "publisher/alpha/pkg/{}/{}",
        pkg5_fixture::quote(&pkg.stem),
        pkg5_fixture::quote(&pkg.version)
    );
    let mut h = tar_header(tar::EntryType::Regular, manifest.len() as u64);
    builder
        .append_data(&mut h, &name, manifest.as_bytes())
        .unwrap();
    extra(&mut builder);
    builder.finish().unwrap();
}

/// Set a raw entry name, bypassing `set_path`'s refusal of `..`/absolute.
fn raw_name(header: &mut tar::Header, name: &str) {
    let bytes = &mut header.as_old_mut().name;
    bytes.fill(0);
    bytes[..name.len()].copy_from_slice(name.as_bytes());
    header.set_cksum();
}

#[test]
fn hostile_archives_cannot_reach_outside_the_scratch_tree() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    import(&repo, &make_p5p(tmp.path(), "run1", &base_pkgs())).unwrap();
    let generation = current_generation(&repo);
    let before = visible_state(&repo, &["alpha", "beta"]);
    let (victim_files, victim_blob, victim_content) = victim(tmp.path());
    // The hostile package references the victim's payload by digest.
    let pkg = Pkg::new("alpha", "web/hostile").file("usr/share/v", &victim_content);

    let escape = tmp.path().join("escape-marker");
    let absolute = tmp.path().join("absolute-marker");
    let cases: Vec<(&str, Box<dyn FnOnce(&mut tar::Builder<fs::File>)>)> = vec![
        (
            "symlinked payload directory",
            Box::new(|b| {
                let mut h = tar_header(tar::EntryType::Symlink, 0);
                b.append_link(&mut h, "publisher/alpha/file", &victim_files)
                    .unwrap();
            }),
        ),
        (
            "symlinked package directory",
            Box::new(|b| {
                let mut h = tar_header(tar::EntryType::Symlink, 0);
                b.append_link(&mut h, "publisher/beta", "../../victim/publisher/x")
                    .unwrap();
            }),
        ),
        (
            "hard link",
            Box::new(|b| {
                let mut h = tar_header(tar::EntryType::Link, 0);
                b.append_link(&mut h, "publisher/alpha/file/aa/link", &victim_blob)
                    .unwrap();
            }),
        ),
        (
            "parent traversal",
            Box::new(|b| {
                let mut h = tar_header(tar::EntryType::Regular, 1);
                raw_name(&mut h, "../../../../escape-marker");
                b.append(&h, &b"x"[..]).unwrap();
            }),
        ),
        (
            "absolute path",
            Box::new(|b| {
                let mut h = tar_header(tar::EntryType::Regular, 1);
                raw_name(&mut h, absolute.to_str().unwrap());
                b.append(&h, &b"x"[..]).unwrap();
            }),
        ),
    ];
    for (name, extra) in cases {
        let archive = tmp.path().join(format!("{}.p5p", name.replace(' ', "-")));
        hostile_p5p(&archive, &pkg, extra);
        match import(&repo, &archive) {
            Err(Pkg5ImportError::Extract { .. }) => {}
            other => panic!("{name}: expected the archive to be refused, got {other:?}"),
        }
        assert_eq!(
            fs::read(&victim_blob).unwrap(),
            gzip(&victim_content),
            "{name}"
        );
        assert!(
            !escape.exists() && !absolute.exists(),
            "{name}: wrote outside"
        );
        assert_eq!(current_generation(&repo), generation, "{name}");
        assert_eq!(visible_state(&repo, &["alpha", "beta"]), before, "{name}");
    }

    // A directory source with a linked directory is refused the same way.
    let src = write_pkg5_repo(&tmp.path().join("linked-src"), std::slice::from_ref(&pkg));
    let file_dir = src.join("publisher/alpha/file");
    fs::remove_dir_all(&file_dir).unwrap();
    std::os::unix::fs::symlink(&victim_files, &file_dir).unwrap();
    match import(&repo, &src) {
        Err(Pkg5ImportError::InvalidSource { problems }) => {
            assert!(problems[0].contains("publisher/alpha/file"), "{problems:?}")
        }
        other => panic!("expected a linked source to be refused, got {other:?}"),
    }
    assert_eq!(fs::read(&victim_blob).unwrap(), gzip(&victim_content));
    assert_eq!(current_generation(&repo), generation);
}

#[test]
fn dot_package_names_are_rejected() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let good = Pkg::new("alpha", "web/good");
    let src = write_pkg5_repo(&tmp.path().join("src"), std::slice::from_ref(&good));
    for stem in [".", ".."] {
        let dir = src
            .join("publisher/alpha/pkg")
            .join(pkg5_fixture::quote(stem).replace('.', "%2E"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("1.0"),
            format!("set name=pkg.fmri value=pkg://alpha/{stem}@1.0\n"),
        )
        .unwrap();
    }
    match import(&repo, &src) {
        Err(Pkg5ImportError::InvalidSource { problems }) => {
            assert_eq!(problems.len(), 2, "{problems:?}");
            assert!(problems.iter().all(|p| p.contains("invalid package name")));
        }
        other => panic!("expected invalid source, got {other:?}"),
    }
}

#[test]
fn receiving_into_a_managed_repository_fails_before_any_change() {
    let tmp = TempDir::new().unwrap();
    // Unmanaged source with a package of a publisher the target lacks.
    let source_path = tmp.path().join("source");
    let mut source = FileBackend::create(&source_path, RepositoryVersion::V4).unwrap();
    source.add_publisher("delta").unwrap();
    let mut tx = source.begin_transaction().unwrap();
    tx.set_publisher("delta");
    let mut manifest = libips::actions::Manifest::new();
    manifest.attributes.push(libips::actions::Attr {
        key: "pkg.fmri".to_string(),
        values: vec!["pkg://delta/tools/d@1.0".to_string()],
        properties: Default::default(),
    });
    tx.update_manifest(manifest);
    tx.commit().unwrap();
    source.rebuild(Some("delta"), false, false).unwrap();

    let repo = tmp.path().join("repo");
    import(&repo, &make_p5p(tmp.path(), "run1", &base_pkgs())).unwrap();
    let generation = current_generation(&repo);
    let config = fs::read(repo.join("pkg6.repository")).unwrap();

    let fmris: Vec<_> = source
        .list_packages(Some("delta"), None)
        .unwrap()
        .into_iter()
        .map(|p| p.fmri)
        .collect();
    let mut receiver =
        libips::recv::PackageReceiver::new(&source, FileBackend::open(&repo).unwrap());
    let err = receiver
        .receive(Some("delta"), &fmris, false)
        .expect_err("receive into a managed repository must fail");
    assert!(err.to_string().contains("receiving packages"), "{err}");
    assert!(receiver.receive_one("delta", &fmris[0]).is_err());

    assert_eq!(current_generation(&repo), generation);
    assert_eq!(fs::read(repo.join("pkg6.repository")).unwrap(), config);
    assert!(!repo.join("publisher/delta").exists());
}

#[test]
fn extraction_limits_stop_oversized_archives_before_changes() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    import(&repo, &make_p5p(tmp.path(), "run1", &base_pkgs())).unwrap();
    let generation = current_generation(&repo);
    let run2 = make_p5p(tmp.path(), "run2", &[Pkg::new("alpha", "web/big")]);

    for options in [
        Pkg5ImportOptions {
            max_archive_entries: 3,
            ..Default::default()
        },
        Pkg5ImportOptions {
            max_extract_bytes: Some(16),
            ..Default::default()
        },
    ] {
        match import_pkg5(&repo, &run2, &options) {
            Err(Pkg5ImportError::ArchiveLimit { .. }) => {}
            other => panic!("expected the limit to stop the import, got {other:?}"),
        }
        assert_eq!(current_generation(&repo), generation);
        assert!(generation_dirs(&repo).iter().all(|n| !n.starts_with('.')));
    }

    // A header claiming more data than the archive holds.
    let lying = tmp.path().join("lying.p5p");
    {
        let mut raw = Vec::new();
        let mut h = tar_header(tar::EntryType::Regular, 0);
        h.set_path("pkg5.repository").unwrap();
        h.set_cksum();
        raw.extend_from_slice(h.as_bytes());
        let mut h = tar_header(tar::EntryType::Regular, 1 << 40);
        h.set_path("publisher/alpha/file/aa/huge").unwrap();
        h.set_cksum();
        raw.extend_from_slice(h.as_bytes());
        raw.extend_from_slice(&[0u8; 1024]);
        fs::write(&lying, raw).unwrap();
    }
    match import(&repo, &lying) {
        Err(Pkg5ImportError::ArchiveLimit { .. }) => {}
        other => panic!("expected the size claim to be refused, got {other:?}"),
    }
    assert!(!repo.join("publisher/alpha/file/aa/huge").exists());
    assert_eq!(current_generation(&repo), generation);

    // Defaults accept the genuine archive.
    assert!(matches!(
        import(&repo, &run2).unwrap(),
        Pkg5ImportOutcome::Committed(_)
    ));
}

/// Total size of the manifest files below `pkg_dirs`.
fn manifest_bytes(pkg_dirs: &[PathBuf]) -> u64 {
    pkg_dirs
        .iter()
        .flat_map(|d| walkdir::WalkDir::new(d).into_iter().map(Result::unwrap))
        .filter(|e| e.file_type().is_file())
        .map(|e| e.metadata().unwrap().len())
        .sum()
}

fn budget(manifests: u64, bytes: u64) -> Pkg5ImportOptions {
    Pkg5ImportOptions {
        max_import_manifests: manifests.try_into().unwrap(),
        max_import_manifest_bytes: bytes.try_into().unwrap(),
        ..Default::default()
    }
}

#[test]
fn manifest_budget_rejects_the_whole_batch_for_every_source_layout() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    import(&repo, &make_p5p(tmp.path(), "run1", &base_pkgs())).unwrap();

    // Three new packages of one publisher in each layout.
    let pkgs = |prefix: &str| -> Vec<Pkg> {
        (0..3)
            .map(|i| Pkg::new("alpha", &format!("{prefix}/pkg{i}")))
            .collect()
    };
    let v4_dir = write_pkg5_repo(&tmp.path().join("v4"), &pkgs("dir"));
    let p5p = make_p5p(tmp.path(), "big", &pkgs("archive"));
    let v3_dir = write_v3_repo(&tmp.path().join("legacy"), V3_PREFIX);
    for i in 0..3 {
        add_v3_pkg(&v3_dir, &v3_pkg(&format!("SUNWflat{i}")), false);
    }
    let v4_bytes = manifest_bytes(&[v4_dir.join("publisher/alpha/pkg")]);
    let p5p_bytes = manifest_bytes(&[tmp.path().join("big.src/publisher/alpha/pkg")]);
    let v3_bytes = manifest_bytes(&[v3_dir.join("pkg")]);
    let sources = [(v4_dir, v4_bytes), (p5p, p5p_bytes), (v3_dir, v3_bytes)];

    for (source, total) in &sources {
        let generation = current_generation(&repo);
        let before = visible_state(&repo, &["alpha", "beta", V3_PREFIX]);
        for (options, expect_manifests, expect_bytes) in [
            // One manifest file too many, whatever its size.
            (budget(2, u64::MAX), 3, None),
            // One byte too many: the last file read crosses the limit.
            (budget(3, total - 1), 3, Some(*total)),
        ] {
            let err = import_pkg5(&repo, source, &options).unwrap_err();
            let text = err.to_string();
            match err {
                Pkg5ImportError::ManifestBudget {
                    path,
                    manifests,
                    bytes,
                    max_manifests,
                    max_bytes,
                } => {
                    assert_eq!(&path, source);
                    assert_eq!(manifests, expect_manifests);
                    if let Some(b) = expect_bytes {
                        assert_eq!(bytes, b);
                    }
                    assert_eq!(max_manifests, options.max_import_manifests.get());
                    assert_eq!(max_bytes, options.max_import_manifest_bytes.get());
                }
                other => panic!(
                    "{}: expected the budget to stop it, got {other:?}",
                    source.display()
                ),
            }
            assert!(text.contains("no packages were imported"), "{text}");
            assert!(text.contains(&source.display().to_string()), "{text}");
            assert_eq!(current_generation(&repo), generation);
            assert_eq!(visible_state(&repo, &["alpha", "beta", V3_PREFIX]), before);
            assert!(generation_dirs(&repo).iter().all(|n| !n.starts_with('.')));
        }

        // Exactly at both limits, the batch is imported.
        match import_pkg5(&repo, source, &budget(3, *total)).unwrap() {
            Pkg5ImportOutcome::Committed(r) => assert_eq!(r.added.len(), 3),
            other => panic!(
                "{}: expected a commit at the limits, got {other:?}",
                source.display()
            ),
        }
    }
}

#[test]
fn manifest_budget_spans_selected_publishers_only_and_spares_receipts() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let archive = make_p5p(
        tmp.path(),
        "run1",
        &[
            Pkg::new("alpha", "web/one"),
            Pkg::new("alpha", "web/two"),
            Pkg::new("beta", "library/one"),
            Pkg::new("beta", "library/two"),
        ],
    );
    let only_alpha = Pkg5ImportOptions {
        publishers: Some(vec!["alpha".to_string()]),
        ..budget(2, u64::MAX)
    };
    assert!(matches!(
        import_pkg5(&repo, &archive, &only_alpha).unwrap(),
        Pkg5ImportOutcome::Committed(_)
    ));

    // All publishers share one budget: alpha's two fit, beta's push past it.
    let generation = current_generation(&repo);
    match import_pkg5(&repo, &archive, &budget(2, u64::MAX)) {
        Err(Pkg5ImportError::ManifestBudget { manifests: 3, .. }) => {}
        other => panic!("expected the shared budget to stop it, got {other:?}"),
    }
    assert_eq!(current_generation(&repo), generation);
    assert!(catalog_entries(&repo, "beta").is_empty());

    // Once committed, a retry takes the receipt fast path without scanning.
    assert!(matches!(
        import(&repo, &archive).unwrap(),
        Pkg5ImportOutcome::Committed(_)
    ));
    assert!(matches!(
        import_pkg5(&repo, &archive, &budget(1, 1)).unwrap(),
        Pkg5ImportOutcome::AlreadyImported(_)
    ));
}

#[test]
fn unknown_publisher_is_rejected_after_a_full_import() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let archive = make_p5p(tmp.path(), "run1", &base_pkgs());
    match import(&repo, &archive).unwrap() {
        Pkg5ImportOutcome::Committed(r) => assert!(r.all_publishers),
        other => panic!("unexpected {other:?}"),
    }
    let generation = current_generation(&repo);

    for sel in [vec!["nope"], vec!["alpha", "nope"]] {
        let options = Pkg5ImportOptions {
            publishers: Some(sel.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        };
        match import_pkg5(&repo, &archive, &options) {
            Err(Pkg5ImportError::UnknownPublisher { publisher, .. }) => {
                assert_eq!(publisher, "nope")
            }
            other => panic!("expected {sel:?} to be rejected, got {other:?}"),
        }
        assert_eq!(current_generation(&repo), generation);
    }
    // Known publishers are still acknowledged from the receipt.
    let alpha = Pkg5ImportOptions {
        publishers: Some(vec!["alpha".to_string()]),
        ..Default::default()
    };
    assert!(matches!(
        import_pkg5(&repo, &archive, &alpha).unwrap(),
        Pkg5ImportOutcome::AlreadyImported(_)
    ));
}

fn lz4(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = lz4::EncoderBuilder::new().build(Vec::new()).unwrap();
    enc.write_all(data).unwrap();
    let (out, res) = enc.finish();
    res.unwrap();
    out
}

fn gzip_best(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

#[test]
fn reused_payloads_must_be_pkg5_wire_compatible_and_are_never_replaced() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    import(&repo, &make_p5p(tmp.path(), "run1", &base_pkgs())).unwrap();
    let generation = current_generation(&repo);

    let pkg = Pkg::new("alpha", "web/shared");
    let content = pkg.files[0].1.clone();
    let h = pkg.payload_hashes()[0].clone();
    let pub_copy = FileBackend::construct_file_path_with_publisher(&repo, "alpha", &h);
    let global_copy = FileBackend::construct_file_path(&repo, &h);
    let archive = make_p5p(tmp.path(), "shared", std::slice::from_ref(&pkg));

    // A pkg6 LZ4 copy of the same content (publisher or global store), or a
    // gzip copy whose bytes differ from the manifest's chash, rejects the
    // batch and stays byte for byte as it was.
    for (copy, bytes, why) in [
        (&pub_copy, lz4(&content), "LZ4"),
        (&global_copy, lz4(&content), "LZ4"),
        (&pub_copy, gzip_best(&content), "chash"),
        (&global_copy, content.clone(), "not gzip"),
    ] {
        fs::create_dir_all(copy.parent().unwrap()).unwrap();
        fs::write(copy, &bytes).unwrap();
        match import(&repo, &archive) {
            Err(Pkg5ImportError::InvalidSource { problems }) => {
                assert_eq!(problems.len(), 1, "{problems:?}");
                assert!(
                    problems[0].contains(&h) && problems[0].contains(why),
                    "{problems:?}"
                );
            }
            other => panic!("expected {why} copy to be refused, got {other:?}"),
        }
        assert_eq!(fs::read(copy).unwrap(), bytes, "existing copy was modified");
        assert_eq!(current_generation(&repo), generation);
        assert!(!manifest_path(&repo, &pkg).exists());
        for other in [&pub_copy, &global_copy] {
            if other != copy {
                assert!(!other.exists(), "a shadowing copy was added");
            }
        }
        fs::remove_file(copy).unwrap();
    }

    // A verified gzip copy in the global store is reused as is.
    fs::create_dir_all(global_copy.parent().unwrap()).unwrap();
    fs::write(&global_copy, gzip(&content)).unwrap();
    assert!(matches!(
        import(&repo, &archive).unwrap(),
        Pkg5ImportOutcome::Committed(_)
    ));
    assert!(!pub_copy.exists());
    assert_eq!(fs::read(&global_copy).unwrap(), gzip(&content));
}

#[test]
fn source_payloads_are_stored_as_single_gzip_members() {
    use std::io::Read;
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let pkg = Pkg::new("alpha", "web/raw");
    let content = pkg.files[0].1.clone();
    let h = pkg.payload_hashes()[0].clone();

    // Uncompressed source payload: stored gzip if no manifest pins the
    // compressed bytes, rejected if one does.
    let root = write_pkg5_repo(&tmp.path().join("raw"), std::slice::from_ref(&pkg));
    let blob = root.join("publisher/alpha/file").join(&h[0..2]).join(&h);
    fs::write(&blob, &content).unwrap();
    match import(&repo, &root) {
        Err(Pkg5ImportError::InvalidSource { problems }) => {
            assert!(problems[0].contains("not gzip"), "{problems:?}")
        }
        other => panic!("expected the pinned chash to refuse recompression, got {other:?}"),
    }
    let mpath = FileBackend::construct_manifest_path(&root, "alpha", &pkg.stem, &pkg.version);
    let unpinned: String = fs::read_to_string(&mpath)
        .unwrap()
        .split(' ')
        .filter(|t| !t.starts_with("chash="))
        .collect::<Vec<_>>()
        .join(" ");
    fs::write(&mpath, unpinned).unwrap();
    assert!(matches!(
        import(&repo, &root).unwrap(),
        Pkg5ImportOutcome::Committed(_)
    ));
    let stored = fs::read(FileBackend::construct_file_path_with_publisher(
        &repo, "alpha", &h,
    ))
    .unwrap();
    assert_eq!(&stored[..2], &[0x1f, 0x8b]);
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&stored[..])
        .read_to_end(&mut out)
        .unwrap();
    assert_eq!(out, content);
}

#[test]
fn unexpected_real_publisher_directories_fail_closed() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let pkgs = base_pkgs();
    import(&repo, &make_p5p(tmp.path(), "run1", &pkgs)).unwrap();
    let generation = current_generation(&repo);
    let link = repo.join("publisher/alpha/pkg");
    let published = fs::canonicalize(&link).unwrap();
    let snapshot = visible_state(&repo, &["alpha", "beta"]);

    // A leftover byte-identical copy (crash after this generation's
    // adoption) is replaced by the link without a new generation.
    let copy_tree = |dst: &Path| {
        for e in walkdir::WalkDir::new(&published) {
            let e = e.unwrap();
            let target = dst.join(e.path().strip_prefix(&published).unwrap());
            if e.file_type().is_dir() {
                fs::create_dir_all(&target).unwrap();
            } else {
                fs::copy(e.path(), &target).unwrap();
            }
        }
    };
    fs::remove_file(&link).unwrap();
    copy_tree(&link);
    drop(GenerationStore::open(&repo).unwrap());
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(current_generation(&repo), generation);

    // An old directory that lacks, changes or adds manifests must not
    // replace the published generation: every writer refuses to open.
    let stale = Pkg::new("alpha", "web/stale");
    let changed = manifest_path(&repo, &pkgs[0])
        .strip_prefix(&link)
        .unwrap()
        .to_path_buf();
    for damage in ["extra", "changed"] {
        fs::remove_file(&link).unwrap();
        copy_tree(&link);
        match damage {
            "extra" => {
                let p = manifest_path(&repo, &stale);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(&p, stale.manifest()).unwrap();
            }
            _ => fs::write(link.join(&changed), "set name=pkg.fmri value=old\n").unwrap(),
        }
        let before = visible_state(&repo, &["alpha"]);
        match GenerationStore::open(&repo) {
            Err(GenerationError::UnexpectedLiveDirectory { publisher, .. }) => {
                assert_eq!(publisher, "alpha")
            }
            Err(other) => panic!("expected {damage} directory to be refused, got {other:?}"),
            Ok(_) => panic!("{damage} directory was accepted"),
        }
        let run2 = make_p5p(
            tmp.path(),
            &format!("run2-{damage}"),
            &[Pkg::new("beta", "x/y")],
        );
        assert!(matches!(
            import(&repo, &run2),
            Err(Pkg5ImportError::Generation(
                GenerationError::UnexpectedLiveDirectory { .. }
            ))
        ));
        assert_eq!(current_generation(&repo), generation);
        assert_eq!(
            visible_state(&repo, &["alpha"]),
            before,
            "live directory touched"
        );
        assert!(
            fs::symlink_metadata(&link).unwrap().is_dir(),
            "live directory replaced"
        );
        // The generation itself is unchanged.
        for (path, bytes) in &snapshot {
            if let Ok(rel) = path.strip_prefix(&link) {
                assert_eq!(&fs::read(published.join(rel)).unwrap(), bytes);
            }
        }
        fs::remove_dir_all(&link).unwrap();
        std::os::unix::fs::symlink(
            Path::new("../../generations/current/publisher/alpha/pkg"),
            &link,
        )
        .unwrap();
    }
    assert_eq!(visible_state(&repo, &["alpha", "beta"]), snapshot);
}

/// Real metadata directories of a publisher that neither the published
/// generation nor the configuration names would be served in place of the
/// generation that later publishes it, so every writer refuses them before
/// changing anything.
#[test]
fn unregistered_real_publisher_directories_fail_closed() {
    let tmp = TempDir::new().unwrap();
    let stray = Pkg::new("gamma", "web/stray");
    let plant = |repo: &Path| {
        let p = manifest_path(repo, &stray);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, stray.manifest()).unwrap();
        fs::create_dir_all(repo.join("publisher/gamma/catalog")).unwrap();
    };
    let assert_refused =
        |result: Result<Pkg5ImportOutcome, Pkg5ImportError>, repo: &Path| match result {
            Err(Pkg5ImportError::Generation(GenerationError::UnregisteredLiveDirectory {
                paths,
            })) => assert_eq!(
                paths,
                vec![
                    repo.join("publisher/gamma/catalog"),
                    repo.join("publisher/gamma/pkg"),
                ]
            ),
            other => panic!("expected unregistered directories to be refused, got {other:?}"),
        };
    let adopt = Pkg5ImportOptions {
        adopt_existing: true,
        ..Default::default()
    };

    // First adoption of a legacy repository.
    let repo = tmp.path().join("legacy");
    {
        let mut backend = FileBackend::create(&repo, RepositoryVersion::V4).unwrap();
        backend.add_publisher("alpha").unwrap();
    }
    plant(&repo);
    let run = make_p5p(tmp.path(), "legacy-run", &[Pkg::new("alpha", "web/hello")]);
    assert_refused(import_pkg5(&repo, &run, &adopt), &repo);
    assert!(!GenerationStore::is_managed(&repo));
    assert!(!repo.join(GENERATIONS_DIR).exists());
    assert!(
        fs::symlink_metadata(repo.join("publisher/alpha/catalog"))
            .unwrap()
            .is_dir()
    );
    assert!(manifest_path(&repo, &stray).is_file());
    fs::rename(
        repo.join("publisher/gamma"),
        tmp.path().join("legacy-gamma"),
    )
    .unwrap();
    assert!(matches!(
        import_pkg5(&repo, &run, &adopt).unwrap(),
        Pkg5ImportOutcome::Committed(_)
    ));

    // A later import on a managed repository, including one that publishes
    // that very publisher.
    let repo = tmp.path().join("managed");
    import(&repo, &make_p5p(tmp.path(), "run1", &base_pkgs())).unwrap();
    let generation = current_generation(&repo);
    let snapshot = visible_state(&repo, &["alpha", "beta"]);
    plant(&repo);
    let fresh = Pkg::new("gamma", "web/fresh");
    let run2 = make_p5p(tmp.path(), "run2", std::slice::from_ref(&fresh));
    assert_refused(import(&repo, &run2), &repo);
    assert!(matches!(
        GenerationStore::open(&repo),
        Err(GenerationError::UnregisteredLiveDirectory { .. })
    ));
    assert_eq!(current_generation(&repo), generation);
    assert_eq!(visible_state(&repo, &["alpha", "beta"]), snapshot);
    for d in ["pkg", "catalog"] {
        assert!(
            fs::symlink_metadata(repo.join("publisher/gamma").join(d))
                .unwrap()
                .is_dir()
        );
    }
    assert_eq!(
        fs::read_to_string(manifest_path(&repo, &stray)).unwrap(),
        stray.manifest()
    );

    // Moved aside, the publisher is published through the generation.
    fs::remove_dir_all(repo.join("publisher/gamma/pkg")).unwrap();
    fs::remove_dir_all(repo.join("publisher/gamma/catalog")).unwrap();
    assert!(matches!(
        import(&repo, &run2).unwrap(),
        Pkg5ImportOutcome::Committed(_)
    ));
    for d in ["pkg", "catalog", "catalog2"] {
        assert!(
            fs::symlink_metadata(repo.join("publisher/gamma").join(d))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    assert_eq!(catalog_entries(&repo, "gamma"), vec![entry(&fresh)]);
    assert!(!manifest_path(&repo, &stray).exists());
}
