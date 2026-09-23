//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests of the `pkg6repo` binary.
//!
//! Every test drives the real `pkg6repo` executable that Cargo built for this
//! test profile (`CARGO_BIN_EXE_pkg6repo`; with `--release` that is the same
//! binary that ships). Each test works in its own temporary directory, so the
//! tests are safe to run in parallel processes under nextest.
//!
//! There is no standalone publishing CLI any more (the former `pkg6dev`
//! tool is gone), so fixture packages are published through the libips
//! transaction API, the same path the importers use. Everything under test
//! (create, add-publisher, list, contents) goes through the binary. The
//! former `obsolete-package` command was removed (ADR-004); `tests.rs`
//! asserts that it stays gone.

use libips::actions::{Attr, File as FileAction, Manifest};
use libips::repository::{FileBackend, ReadableRepository, WritableRepository};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const PKG6REPO: &str = env!("CARGO_BIN_EXE_pkg6repo");

/// Run pkg6repo; Ok(stdout) on success, Err(stderr) otherwise.
fn run_pkg6repo(args: &[&str]) -> Result<String, String> {
    let output = Command::new(PKG6REPO)
        .args(args)
        .output()
        .expect("failed to execute pkg6repo");
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

fn path_str(p: &Path) -> &str {
    p.to_str().expect("test paths are UTF-8")
}

/// Create a repository with publisher `test` through the binary.
fn create_repo_with_publisher(dir: &TempDir) -> PathBuf {
    let repo = dir.path().join("repo");
    run_pkg6repo(&["create", "--repo-version", "4", path_str(&repo)])
        .expect("pkg6repo create failed");
    run_pkg6repo(&["add-publisher", "-s", path_str(&repo), "test"])
        .expect("pkg6repo add-publisher failed");
    repo
}

/// A file delivered by a fixture package: (path in package, content, mode).
type FixtureFile<'a> = (&'a str, &'a str, &'a str);

/// The files of the historical `example` fixture package.
const EXAMPLE_FILES: &[FixtureFile] = &[
    ("usr/bin/hello", "#!/bin/sh\necho 'Hello, World!'\n", "0755"),
    (
        "usr/share/doc/example/README.txt",
        "This is an example document.",
        "0644",
    ),
    (
        "etc/config/example.conf",
        "# Example configuration file\nvalue=42\n",
        "0644",
    ),
];

/// Publish one package into publisher `test` of `repo` via libips.
fn publish(repo: &Path, fmri: &str, summary: &str, files: &[FixtureFile]) {
    let proto = TempDir::new().unwrap();
    let backend = FileBackend::open(repo).expect("open repository");
    let mut tx = backend.begin_transaction().expect("begin transaction");
    tx.set_publisher("test");

    for (path, content, mode) in files {
        let src = proto.path().join(path);
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::write(&src, content).unwrap();
        let mut fa = FileAction::read_from_path(&src).expect("read fixture file");
        fa.path = path.to_string();
        fa.mode = mode.to_string();
        fa.owner = "root".to_string();
        fa.group = "bin".to_string();
        tx.add_file(fa, &src).expect("add file to transaction");
    }

    let mut manifest = Manifest::new();
    for (key, value) in [("pkg.fmri", fmri), ("pkg.summary", summary)] {
        manifest.attributes.push(Attr {
            key: key.to_string(),
            values: vec![value.to_string()],
            properties: HashMap::new(),
        });
    }
    tx.update_manifest(manifest);
    tx.commit().expect("commit transaction");
    backend
        .rebuild(Some("test"), false, false)
        .expect("rebuild catalog");
}

#[test]
fn test_e2e_create_repository() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");

    let result = run_pkg6repo(&["create", "--repo-version", "4", path_str(&repo)]);
    assert!(
        result.is_ok(),
        "Failed to create repository: {:?}",
        result.err()
    );

    assert!(repo.exists());
    assert!(repo.join("publisher").exists());
    assert!(repo.join("index").exists());
    assert!(repo.join("trans").exists());
    assert!(repo.join("pkg6.repository").exists());
}

#[test]
fn test_e2e_add_publisher() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");

    let result = run_pkg6repo(&["create", "--repo-version", "4", path_str(&repo)]);
    assert!(
        result.is_ok(),
        "Failed to create repository: {:?}",
        result.err()
    );

    let result = run_pkg6repo(&["add-publisher", "-s", path_str(&repo), "example.com"]);
    assert!(
        result.is_ok(),
        "Failed to add publisher: {:?}",
        result.err()
    );

    let publisher = repo.join("publisher").join("example.com");
    assert!(publisher.exists());
    assert!(publisher.join("catalog").exists());
    assert!(publisher.join("pkg").exists());
}

#[test]
fn test_e2e_publish_package() {
    let dir = TempDir::new().unwrap();
    let repo = create_repo_with_publisher(&dir);
    publish(
        &repo,
        "pkg://test/example@1.0.0",
        "Example package for testing",
        EXAMPLE_FILES,
    );

    let result = run_pkg6repo(&["list", "-s", path_str(&repo)]);
    assert!(
        result.is_ok(),
        "Failed to list packages: {:?}",
        result.err()
    );
    assert!(
        result.unwrap().contains("example"),
        "Package not found in repository"
    );
}

#[test]
fn test_e2e_show_contents() {
    let dir = TempDir::new().unwrap();
    let repo = create_repo_with_publisher(&dir);
    publish(
        &repo,
        "pkg://test/example@1.0.0",
        "Example package for testing",
        EXAMPLE_FILES,
    );

    let result = run_pkg6repo(&["contents", "-s", path_str(&repo), "example"]);
    assert!(
        result.is_ok(),
        "Failed to show package contents: {:?}",
        result.err()
    );
    let output = result.unwrap();
    for (path, _, _) in EXAMPLE_FILES {
        assert!(
            output.contains(path),
            "{path} not found in package contents:\n{output}"
        );
    }
}

#[test]
fn test_e2e_multiple_packages() {
    let dir = TempDir::new().unwrap();
    let repo = create_repo_with_publisher(&dir);
    publish(
        &repo,
        "pkg://test/example@1.0.0",
        "Example package for testing",
        EXAMPLE_FILES,
    );
    publish(
        &repo,
        "pkg://test/example2@1.0.0",
        "Second example package for testing",
        &EXAMPLE_FILES[..2],
    );

    let result = run_pkg6repo(&["list", "-s", path_str(&repo)]);
    assert!(
        result.is_ok(),
        "Failed to list packages: {:?}",
        result.err()
    );
    let output = result.unwrap();
    assert!(
        output.contains("example"),
        "First package not found in repository"
    );
    assert!(
        output.contains("example2"),
        "Second package not found in repository"
    );
}
