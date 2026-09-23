//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

//! The v1 `pkg6repo` command surface (#40): commands either do what they
//! report or fail without changing the repository. Every test drives the
//! built binary against its own temporary repository.

use libips::actions::{Attr, File as FileAction, Manifest};
use libips::repository::{FileBackend, ReadableRepository, WritableRepository};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn pkg6repo(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pkg6repo"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

#[track_caller]
fn ok(args: &[&str]) -> String {
    let out = pkg6repo(args);
    assert!(out.status.success(), "{args:?}: {}", stderr(&out));
    stdout(&out)
}

#[track_caller]
fn fails(args: &[&str]) -> Output {
    let out = pkg6repo(args);
    assert!(
        !out.status.success(),
        "{args:?} succeeded: {}",
        stdout(&out)
    );
    out
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// A repository with publishers `alpha` and `beta`, no packages.
fn repo(dir: &TempDir) -> PathBuf {
    let repo = dir.path().join("repo");
    ok(&["create", s(&repo)]);
    ok(&["add-publisher", "-s", s(&repo), "alpha", "beta"]);
    repo
}

/// Publish `fmri` into its publisher with one file `usr/share/<stem>`.
fn publish(repo: &Path, publisher: &str, fmri: &str) {
    let proto = TempDir::new().unwrap();
    let backend = FileBackend::open(repo).unwrap();
    let mut tx = backend.begin_transaction().unwrap();
    tx.set_publisher(publisher);
    let src = proto.path().join("payload");
    fs::write(&src, fmri).unwrap();
    let mut fa = FileAction::read_from_path(&src).unwrap();
    let stem = fmri.rsplit_once('/').unwrap().1.split('@').next().unwrap();
    fa.path = format!("usr/share/{stem}");
    fa.mode = "0644".to_string();
    fa.owner = "root".to_string();
    fa.group = "bin".to_string();
    tx.add_file(fa, &src).unwrap();
    let mut manifest = Manifest::new();
    manifest.attributes.push(Attr {
        key: "pkg.fmri".to_string(),
        values: vec![fmri.to_string()],
        properties: HashMap::new(),
    });
    tx.update_manifest(manifest);
    tx.commit().unwrap();
    backend.rebuild(Some(publisher), false, false).unwrap();
}

fn config(repo: &Path) -> String {
    fs::read_to_string(repo.join("pkg6.repository")).unwrap()
}

#[test]
fn create_refuses_to_overwrite_an_existing_repository() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let before = config(&repo);

    let out = fails(&["create", s(&repo)]);
    assert!(stderr(&out).contains("already exists"), "{}", stderr(&out));
    assert_eq!(config(&repo), before, "the publisher list was overwritten");
}

#[test]
fn publisher_changes_are_all_or_nothing() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    publish(&repo, "alpha", "pkg://alpha/web/a@1.0");
    let before = config(&repo);

    // An existing or repeated publisher fails before anything is added.
    fails(&["add-publisher", "-s", s(&repo), "gamma", "alpha"]);
    fails(&["add-publisher", "-s", s(&repo), "gamma", "gamma"]);
    // An unknown publisher fails before anything is removed.
    let out = fails(&["remove-publisher", "-s", s(&repo), "alpha", "nosuch"]);
    assert!(stderr(&out).contains("nosuch"), "{}", stderr(&out));
    assert_eq!(config(&repo), before);
    assert!(ok(&["list", "-s", s(&repo)]).contains("web/a"));

    // A dry run reports what would go and removes nothing.
    let plan = ok(&["remove-publisher", "-n", "-s", s(&repo), "alpha"]);
    assert!(
        plan.contains("alpha") && plan.contains("1 packages"),
        "{plan}"
    );
    assert_eq!(config(&repo), before);
}

#[test]
fn get_and_set_cover_only_effective_properties() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);

    let json = ok(&["get", "-s", s(&repo), "-F", "json", "publisher/prefix"]);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value, serde_json::json!({"publisher/prefix": "alpha"}));

    ok(&["set", "-s", s(&repo), "publisher/prefix=beta"]);
    let tsv = ok(&["get", "-s", s(&repo), "-F", "tsv", "-H", "publisher"]);
    assert_eq!(tsv, "publisher\tprefix\tbeta\n");

    let before = config(&repo);
    // An inert property, an unknown default publisher, or a bad pair in
    // the batch: nothing changes.
    fails(&["set", "-s", s(&repo), "repository/origins=http://x"]);
    fails(&["set", "-s", s(&repo), "publisher/prefix=nosuch"]);
    fails(&["set", "-s", s(&repo), "publisher/prefix=alpha", "junk"]);
    fails(&["get", "-s", s(&repo), "repository/origins"]);
    assert_eq!(config(&repo), before);
}

#[test]
fn list_honours_every_filter_and_reports_unmatched_patterns() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    publish(&repo, "alpha", "pkg://alpha/web/a@1.0");
    publish(&repo, "beta", "pkg://beta/web/b@1.0");
    publish(&repo, "beta", "pkg://beta/lib/c@1.0");

    let both = ok(&[
        "list",
        "-H",
        "-s",
        s(&repo),
        "-p",
        "alpha",
        "-p",
        "beta",
        "web/*",
    ]);
    assert!(both.contains("web/a") && both.contains("web/b"), "{both}");
    assert!(!both.contains("lib/c"), "{both}");

    let two = ok(&["list", "-H", "-s", s(&repo), "web/a", "lib/c"]);
    assert_eq!(two.lines().count(), 2, "{two}");

    // Matches are printed, then the command fails naming the pattern that
    // matched nothing.
    let out = fails(&["list", "-H", "-s", s(&repo), "web/a", "nosuch"]);
    assert!(stdout(&out).contains("web/a"), "{}", stdout(&out));
    assert!(stderr(&out).contains("nosuch"), "{}", stderr(&out));

    fails(&["list", "-s", s(&repo), "-p", "nosuch"]);
    fails(&["info", "-s", s(&repo), "-p", "nosuch"]);
    fails(&["search", "-s", s(&repo), "-p", "nosuch", "web"]);
}

#[test]
fn contents_filters_by_action_type_and_reports_unmatched_patterns() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    publish(&repo, "alpha", "pkg://alpha/web/a@1.0");

    let files = ok(&["contents", "-s", s(&repo), "-t", "file", "web/a"]);
    assert!(files.contains("usr/share/a"), "{files}");
    assert!(
        files.lines().all(|l| l.trim_end().ends_with("file")),
        "{files}"
    );
    assert!(ok(&["contents", "-s", s(&repo), "-t", "dir", "web/a"]).is_empty());

    let out = fails(&["contents", "-s", s(&repo), "nosuch"]);
    assert!(stderr(&out).contains("nosuch"), "{}", stderr(&out));
}

#[test]
fn rebuild_no_index_still_rebuilds_the_catalog() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    publish(&repo, "alpha", "pkg://alpha/web/a@1.0");
    fs::remove_dir_all(repo.join("publisher/alpha/catalog")).unwrap();
    fs::create_dir_all(repo.join("publisher/alpha/catalog")).unwrap();

    let out = pkg6repo(&["rebuild", "-s", s(&repo), "--no-index"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--no-index has no effect"),
        "{}",
        stderr(&out)
    );
    assert!(repo.join("publisher/alpha/catalog/catalog.attrs").exists());
}

#[test]
fn obsoleted_store_reads_leave_the_repository_untouched() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);

    let listed = ok(&[
        "list-obsoleted",
        "-s",
        s(&repo),
        "-p",
        "alpha",
        "-F",
        "json",
    ]);
    let value: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(value["total_count"], 0);
    ok(&[
        "search-obsoleted",
        "-s",
        s(&repo),
        "-p",
        "alpha",
        "-q",
        "web",
    ]);
    fails(&[
        "show-obsoleted",
        "-s",
        s(&repo),
        "-p",
        "alpha",
        "-f",
        "pkg://alpha/web/a@1.0",
    ]);
    // `create` leaves an empty store directory; reads must not populate it.
    assert_eq!(
        fs::read_dir(repo.join("obsoleted")).unwrap().count(),
        0,
        "a read wrote into the store"
    );

    // Nothing to export: fail and write no file.
    let export = dir.path().join("export.json");
    fails(&[
        "export-obsoleted",
        "-s",
        s(&repo),
        "-p",
        "alpha",
        "-o",
        s(&export),
    ]);
    assert!(!export.exists());
}

#[test]
fn export_obsoleted_writes_entries_and_never_overwrites() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let fmri = libips::fmri::Fmri::parse("pkg://alpha/old/thing@1.0").unwrap();
    FileBackend::open(&repo)
        .unwrap()
        .get_obsoleted_manager()
        .unwrap()
        .store_obsoleted_package(
            "alpha",
            &fmri,
            "set name=pkg.fmri value=pkg://alpha/old/thing@1.0\n",
            None,
            None,
        )
        .unwrap();

    // The store is in use but nothing matches: fail and write no file.
    let export = dir.path().join("export.json");
    let none = fails(&[
        "export-obsoleted",
        "-s",
        s(&repo),
        "-p",
        "alpha",
        "-o",
        s(&export),
        "-q",
        "nomatch",
    ]);
    assert!(
        stderr(&none).contains("no obsoleted packages to export"),
        "{}",
        stderr(&none)
    );
    assert!(!export.exists());

    let out = ok(&[
        "export-obsoleted",
        "-s",
        s(&repo),
        "-p",
        "alpha",
        "-o",
        s(&export),
    ]);
    assert!(out.contains("Exported 1"), "{out}");
    let written = fs::read(&export).unwrap();

    let again = fails(&[
        "export-obsoleted",
        "-s",
        s(&repo),
        "-p",
        "alpha",
        "-o",
        s(&export),
    ]);
    assert!(
        stderr(&again).contains("already exists"),
        "{}",
        stderr(&again)
    );
    assert_eq!(fs::read(&export).unwrap(), written);
}

#[test]
fn restore_obsoleted_publishes_the_entry_into_the_catalog() {
    let dir = TempDir::new().unwrap();
    let repo = repo(&dir);
    let fmri = libips::fmri::Fmri::parse("pkg://alpha/old/thing@1.0").unwrap();
    FileBackend::open(&repo)
        .unwrap()
        .get_obsoleted_manager()
        .unwrap()
        .store_obsoleted_package(
            "alpha",
            &fmri,
            "set name=pkg.fmri value=pkg://alpha/old/thing@1.0\n\
             set name=pkg.obsolete value=true\n",
            None,
            None,
        )
        .unwrap();

    ok(&[
        "restore-obsoleted",
        "-s",
        s(&repo),
        "-p",
        "alpha",
        "-f",
        "pkg://alpha/old/thing@1.0",
    ]);

    assert!(ok(&["list", "-s", s(&repo), "old/thing"]).contains("old/thing"));
    let listed = ok(&[
        "list-obsoleted",
        "-s",
        s(&repo),
        "-p",
        "alpha",
        "-F",
        "json",
    ]);
    let value: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(value["total_count"], 0, "{listed}");
    let backend = FileBackend::open(&repo).unwrap();
    assert_eq!(
        backend
            .list_packages(Some("alpha"), Some("old/thing"))
            .unwrap()
            .len(),
        1
    );
}
