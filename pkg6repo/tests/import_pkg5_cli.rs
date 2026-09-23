//! `pkg6repo import-pkg5` as the CI import script runs it (#38).

#[path = "../../libips/tests/support/pkg5_fixture.rs"]
mod pkg5_fixture;

use pkg5_fixture::{Pkg, make_p5p};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn import(repo: &Path, archive: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pkg6repo"))
        .args(["import-pkg5", "-s"])
        .arg(archive)
        .arg("-d")
        .arg(repo)
        .env("RUST_LOG", "info")
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn import_retry_and_conflict_exit_codes() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let run1 = make_p5p(
        tmp.path(),
        "run1",
        &[
            Pkg::new("alpha", "web/hello"),
            Pkg::new("beta", "lib/world"),
        ],
    );

    let first = import(&repo, &run1);
    assert!(first.status.success(), "{}", text(&first));

    // CI retry after a lost acknowledgement.
    let retry = import(&repo, &run1);
    assert!(retry.status.success(), "{}", text(&retry));
    assert!(
        text(&retry).contains("already imported"),
        "{}",
        text(&retry)
    );

    let changed = Pkg::new("alpha", "web/hello").file("usr/share/hello.txt", b"changed");
    let run2 = make_p5p(tmp.path(), "run2", std::slice::from_ref(&changed));
    let conflict = import(&repo, &run2);
    assert!(!conflict.status.success());
    assert!(
        text(&conflict).contains(&changed.fmri()),
        "{}",
        text(&conflict)
    );
}

fn pkg6repo(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pkg6repo"))
        .args(args)
        .output()
        .unwrap()
}

/// A repository managed by an import, plus its config and generation link
/// for detecting any change.
fn managed_repo(tmp: &Path) -> (std::path::PathBuf, Vec<u8>, std::path::PathBuf) {
    let repo = tmp.join("repo");
    let run1 = make_p5p(tmp, "run1", &[Pkg::new("alpha", "web/hello")]);
    assert!(import(&repo, &run1).status.success());
    let config = std::fs::read(repo.join("pkg6.repository")).unwrap();
    let current = std::fs::read_link(repo.join("generations/current")).unwrap();
    (repo, config, current)
}

fn assert_repo_unchanged(repo: &Path, config: &[u8], current: &Path) {
    assert_eq!(std::fs::read(repo.join("pkg6.repository")).unwrap(), config);
    assert_eq!(
        std::fs::read_link(repo.join("generations/current")).unwrap(),
        current
    );
}

#[test]
fn remove_publisher_dry_run_leaves_managed_repository_unchanged() {
    let tmp = TempDir::new().unwrap();
    let (repo, config, current) = managed_repo(tmp.path());

    let out = pkg6repo(&[
        "remove-publisher".as_ref(),
        "-n".as_ref(),
        "-s".as_ref(),
        repo.as_os_str(),
        "alpha".as_ref(),
    ]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("Would remove publisher alpha and its 1 packages"),
        "{}",
        text(&out)
    );
    assert_repo_unchanged(&repo, &config, &current);

    // An unknown publisher fails the whole batch before any generation
    // switch.
    let out = pkg6repo(&[
        "remove-publisher".as_ref(),
        "-s".as_ref(),
        repo.as_os_str(),
        "alpha".as_ref(),
        "nosuch".as_ref(),
    ]);
    assert!(!out.status.success(), "{}", text(&out));
    assert_repo_unchanged(&repo, &config, &current);
}

#[test]
fn restore_obsoleted_on_managed_repository_keeps_the_record() {
    use libips::fmri::Fmri;
    use libips::repository::{FileBackend, ReadableRepository};
    let tmp = TempDir::new().unwrap();
    let (repo, config, current) = managed_repo(tmp.path());
    let fmri = Fmri::parse("pkg://alpha/old/thing@1.0").unwrap();
    {
        let mut backend = FileBackend::open(&repo).unwrap();
        backend
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
    }

    let out = pkg6repo(&[
        "restore-obsoleted".as_ref(),
        "-s".as_ref(),
        repo.as_os_str(),
        "-p".as_ref(),
        "alpha".as_ref(),
        "-f".as_ref(),
        "pkg://alpha/old/thing@1.0".as_ref(),
    ]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("restoring an obsoleted package"),
        "{}",
        text(&out)
    );
    assert_repo_unchanged(&repo, &config, &current);
    let mut backend = FileBackend::open(&repo).unwrap();
    assert!(
        backend
            .get_obsoleted_manager()
            .unwrap()
            .get_obsoleted_package_manifest("alpha", &fmri)
            .unwrap()
            .is_some(),
        "the side-store record was lost"
    );
}
