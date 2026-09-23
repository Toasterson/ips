//! Real-binary tests for pkg6recv's argument resolution: requests that
//! resolve to nothing must fail, and must fail before the destination
//! repository is modified.

use libips::repository::{FileBackend, ReadableRepository, RepositoryVersion, WritableRepository};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct Repos {
    _tmp: TempDir,
    source: PathBuf,
    dest: PathBuf,
}

/// A source repo with `test/pkgA@1.0` and `test/pkgB@1.0`, and an empty
/// destination repo with no publishers.
fn repos() -> Repos {
    repos_with(&[("pkgA", "1.0"), ("pkgB", "1.0")])
}

/// A source repo holding `packages` as (name, version) under publisher
/// `test`, and an empty destination repo with no publishers.
fn repos_with(packages: &[(&str, &str)]) -> Repos {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let dest = tmp.path().join("dest");

    let mut source_repo = FileBackend::create(&source, RepositoryVersion::V4).unwrap();
    source_repo.add_publisher("test").unwrap();
    for (name, version) in packages {
        let path = FileBackend::construct_manifest_path(&source, "test", name, version);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "set name=pkg.fmri value=pkg://test/{name}@{version}\nset name=pkg.summary value=test\n"
            ),
        )
        .unwrap();
    }
    source_repo.rebuild(Some("test"), false, false).unwrap();

    FileBackend::create(&dest, RepositoryVersion::V4).unwrap();

    Repos {
        _tmp: tmp,
        source,
        dest,
    }
}

fn pkg6recv(repos: &Repos, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pkg6recv"))
        .arg("-s")
        .arg(&repos.source)
        .arg("-d")
        .arg(&repos.dest)
        .args(args)
        .env_remove("RUST_LOG")
        .output()
        .unwrap()
}

/// Every path under `root`, so a failed run can be shown to leave it untouched.
fn tree(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_failed_without_mutation(repos: &Repos, before: &[PathBuf], out: &Output) {
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(out));
    assert_eq!(tree(&repos.dest), before, "destination was modified");
}

#[test]
fn no_package_arguments_is_a_usage_error() {
    let repos = repos();
    let before = tree(&repos.dest);

    let out = pkg6recv(&repos, &["-p", "test"]);

    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("<PACKAGE>"),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(tree(&repos.dest), before);
}

#[test]
fn unmatched_wildcard_fails_before_mutation() {
    let repos = repos();
    let before = tree(&repos.dest);

    let out = pkg6recv(&repos, &["-p", "test", "nomatch*"]);

    assert_failed_without_mutation(&repos, &before, &out);
    assert!(
        stderr(&out).contains("no packages matched pattern 'nomatch*'"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn one_unmatched_wildcard_among_matches_fails_before_mutation() {
    let repos = repos();
    let before = tree(&repos.dest);

    // pkgA exists and sorts first; the later miss must still stop everything.
    let out = pkg6recv(&repos, &["-p", "test", "pkgA", "pkg*", "zzz*"]);

    assert_failed_without_mutation(&repos, &before, &out);
    assert!(stderr(&out).contains("'zzz*'"), "stderr: {}", stderr(&out));
}

#[test]
fn unknown_package_name_fails_before_mutation() {
    let repos = repos();
    let before = tree(&repos.dest);

    let out = pkg6recv(&repos, &["-p", "test", "pkgA", "missing"]);

    assert_failed_without_mutation(&repos, &before, &out);
    assert!(
        stderr(&out).contains("package 'missing' not found in source for publisher 'test'"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn package_name_without_publisher_fails_before_mutation() {
    let repos = repos();
    let before = tree(&repos.dest);

    let out = pkg6recv(&repos, &["pkg://test/pkgA", "pkgB"]);

    assert_failed_without_mutation(&repos, &before, &out);
    assert!(
        stderr(&out).contains("no publisher specified for package 'pkgB'"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn missing_exact_version_after_valid_ones_fails_before_mutation() {
    let repos = repos();
    let before = tree(&repos.dest);

    // libips receives in reverse argument order, so try the miss at both
    // ends: either way the valid packages must not have landed.
    let valid = ["pkg://test/pkgA@1.0", "pkg://test/pkgB@1.0"];
    let missing = "pkg://test/pkgB@9.9";
    for args in [[valid[0], valid[1], missing], [missing, valid[0], valid[1]]] {
        let out = pkg6recv(&repos, &args);

        assert_failed_without_mutation(&repos, &before, &out);
        assert!(
            stderr(&out).contains("package 'pkg://test/pkgB@9.9' not found in source"),
            "{args:?}: stderr: {}",
            stderr(&out)
        );
    }
}

#[test]
fn several_versions_are_refused_not_guessed() {
    // String comparison would pick 5.9 over 5.11; refuse instead of guessing.
    let repos = repos_with(&[("pkgA", "5.9"), ("pkgA", "5.11")]);
    let before = tree(&repos.dest);

    for arg in ["pkg*", "pkgA"] {
        let out = pkg6recv(&repos, &["-p", "test", arg]);

        assert_failed_without_mutation(&repos, &before, &out);
        assert!(
            stderr(&out).contains("package 'pkgA' has several versions in publisher 'test'"),
            "{arg}: stderr: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("5.11") && stderr(&out).contains("5.9"),
            "{arg}: stderr: {}",
            stderr(&out)
        );
    }
}

#[test]
fn exact_version_selects_among_several() {
    let repos = repos_with(&[("pkgA", "5.9"), ("pkgA", "5.11")]);

    let out = pkg6recv(&repos, &["pkg://test/pkgA@5.11"]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let dest = FileBackend::open(&repos.dest).unwrap();
    let versions: Vec<_> = dest
        .list_packages(Some("test"), None)
        .unwrap()
        .into_iter()
        .map(|pi| pi.fmri.version())
        .collect();
    assert_eq!(versions, ["5.11"]);
}

#[test]
fn matching_wildcard_receives_packages() {
    let repos = repos();

    let out = pkg6recv(&repos, &["-p", "test", "pkg*"]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Received 2 requested package(s)"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let dest = FileBackend::open(&repos.dest).unwrap();
    let mut names: Vec<_> = dest
        .list_packages(Some("test"), None)
        .unwrap()
        .into_iter()
        .map(|pi| pi.fmri.name)
        .collect();
    names.sort();
    assert_eq!(names, ["pkgA", "pkgB"]);
}
