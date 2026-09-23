//! `pkg6repo retire` and `import-pkg5 --policy` as an operator runs them
//! (#39): plan, review, apply, restore, status, policy validation, blocked
//! plans with suggested keep entries, and explicit import exclusions.

#[path = "../../libips/tests/support/pkg5_fixture.rs"]
mod pkg5_fixture;

use libips::repository::retention::{AccessKind, AccessRecorder, Recorded};
use pkg5_fixture::{Pkg, make_p5p};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn pkg6repo(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pkg6repo"))
        .args(args)
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

fn ok(out: Output) -> String {
    assert!(out.status.success(), "{}", text(&out));
    text(&out)
}

fn fails(out: Output) -> String {
    assert!(!out.status.success(), "unexpected success: {}", text(&out));
    text(&out)
}

fn pkg(stem: &str, version: &str) -> Pkg {
    Pkg::new("oi", stem).version(version).file(
        &format!("usr/share/{}-{}", stem.replace('/', "_"), version),
        format!("{stem} {version}").as_bytes(),
    )
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn operator_workflow() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let foo1 = pkg("library/foo", "1.0:20200101T000000Z");
    let foo2 = pkg("library/foo", "2.0:20210101T000000Z");
    let dev = pkg("developer/gcc", "10:20200101T000000Z");

    // The repository policy excludes developer packages from imports.
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("policy.kdl"),
        "import {\n    exclude regex=\"^pkg://oi/developer/\"\n}\n",
    )
    .unwrap();
    let archive = make_p5p(
        tmp.path(),
        "run1",
        &[foo1.clone(), foo2.clone(), dev.clone()],
    );
    let out = ok(pkg6repo(&[
        "import-pkg5",
        "-s",
        s(&archive),
        "-d",
        s(&repo),
    ]));
    assert!(out.contains("Excluded by the import policy: 1"), "{out}");
    assert!(out.contains(&dev.fmri()), "{out}");

    ok(pkg6repo(&["retire", "check-policy", "-s", s(&repo)]));

    // Plan, review, apply.
    let plan = tmp.path().join("plan.json");
    let out = ok(pkg6repo(&[
        "retire",
        "plan",
        "-s",
        s(&repo),
        "--fmri",
        &foo1.fmri(),
        "--fmri",
        &foo2.fmri(),
        "-o",
        s(&plan),
    ]));
    assert!(out.contains(&foo1.fmri()), "{out}");
    assert!(out.contains("newest version"), "{out}");
    let out = ok(pkg6repo(&["retire", "apply", "-s", s(&repo), s(&plan)]));
    assert!(out.contains("Hid 1 version(s)"), "{out}");
    let out = ok(pkg6repo(&["retire", "apply", "-s", s(&repo), s(&plan)]));
    assert!(out.contains("already applied"), "{out}");

    let out = ok(pkg6repo(&["retire", "status", "-s", s(&repo), "-v"]));
    assert!(out.contains("Hidden versions:   1"), "{out}");
    assert!(out.contains(&foo1.fmri()), "{out}");
    assert!(out.contains("disabled"), "{out}");

    // Restore, with a dry run first.
    let out = ok(pkg6repo(&[
        "retire",
        "restore",
        "-s",
        s(&repo),
        "-n",
        &foo1.fmri(),
    ]));
    assert!(out.contains("Would restore 1"), "{out}");
    let out = ok(pkg6repo(&[
        "retire",
        "restore",
        "-s",
        s(&repo),
        &foo1.fmri(),
    ]));
    assert!(out.contains("Restored 1"), "{out}");
    let out = fails(pkg6repo(&[
        "retire",
        "restore",
        "-s",
        s(&repo),
        "regex:^pkg://oi/none",
    ]));
    assert!(out.contains("nothing_matched"), "{out}");
}

#[test]
fn blocked_plans_print_exact_keep_entries_and_invalid_policies_fail() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let foo1 = pkg("library/foo", "1.0:20200101T000000Z");
    let foo2 = pkg("library/foo", "2.0:20210101T000000Z");
    let archive = make_p5p(tmp.path(), "run1", &[foo1.clone(), foo2.clone()]);
    ok(pkg6repo(&[
        "import-pkg5",
        "-s",
        s(&archive),
        "-d",
        s(&repo),
    ]));

    fs::write(
        repo.join("policy.kdl"),
        format!(
            "snapshot \"hipster-2020.10\" {{\n    fmri \"{}\"\n}}\n",
            foo1.fmri()
        ),
    )
    .unwrap();
    let out = fails(pkg6repo(&[
        "retire",
        "plan",
        "-s",
        s(&repo),
        "--fmri",
        &foo1.fmri(),
    ]));
    assert!(out.contains("hipster-2020.10"), "{out}");
    assert!(out.contains(&format!("keep \"{}\"", foo1.fmri())), "{out}");

    // Keep files in text form resolve it.
    let keep = tmp.path().join("keep.txt");
    fs::write(&keep, format!("# snapshot pins\n{}\n", foo1.fmri())).unwrap();
    let out = ok(pkg6repo(&[
        "retire",
        "plan",
        "-s",
        s(&repo),
        "--keep-file",
        s(&keep),
        "--fmri",
        &foo1.fmri(),
    ]));
    assert!(out.contains("Nothing to hide"), "{out}");

    fs::write(repo.join("policy.kdl"), "keep regex=\"(unclosed\"\n").unwrap();
    let out = fails(pkg6repo(&["retire", "check-policy", "-s", s(&repo)]));
    assert!(out.contains("ips::retention::policy"), "{out}");
    fails(pkg6repo(&["retire", "plan", "-s", s(&repo)]));
}

/// Real audits use the wall clock: there is no way to evaluate the policy
/// at another time from the command line.
#[test]
fn policy_driven_plans_use_the_wall_clock() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let foo1 = pkg("library/foo", "1.0:20200101T000000Z");
    let foo2 = pkg("library/foo", "2.0:20210101T000000Z");
    let archive = make_p5p(tmp.path(), "run1", &[foo1, foo2]);
    ok(pkg6repo(&[
        "import-pkg5",
        "-s",
        s(&archive),
        "-d",
        s(&repo),
    ]));
    fs::write(
        repo.join("policy.kdl"),
        "retirement {\n    enabled true\n    inactivity \"12 months\"\n}\n",
    )
    .unwrap();
    for sub in ["plan", "apply"] {
        let out = fails(pkg6repo(&[
            "retire",
            sub,
            "-s",
            s(&repo),
            "--now",
            "2099-01-01T00:00:00Z",
        ]));
        assert!(out.contains("--now"), "{out}");
    }

    let out = ok(pkg6repo(&["retire", "plan", "-s", s(&repo)]));
    assert!(out.contains("no qualifying access history"), "{out}");
    // A depot started recording now (and stopped cleanly).
    assert_eq!(
        AccessRecorder::new(&repo, "test-depot").record(
            AccessKind::Manifest,
            "oi",
            "pkg://oi/unrelated@1:20200101T000000Z",
        ),
        Recorded::Written
    );
    let out = ok(pkg6repo(&["retire", "plan", "-s", s(&repo)]));
    assert!(out.contains("Nothing to hide"), "{out}");
    let out = ok(pkg6repo(&["retire", "status", "-s", s(&repo)]));
    assert!(out.contains("Access complete:   since"), "{out}");
    assert!(out.contains("Access gaps:       none"), "{out}");
}

/// A depot that stopped without a clean shutdown and never came back can
/// only be closed once it has stopped reporting.
#[test]
fn close_depot_refuses_a_depot_that_still_reports() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let archive = make_p5p(
        tmp.path(),
        "run1",
        &[pkg("library/foo", "1.0:20200101T000000Z")],
    );
    ok(pkg6repo(&[
        "import-pkg5",
        "-s",
        s(&archive),
        "-d",
        s(&repo),
    ]));
    let rec = AccessRecorder::new(&repo, "host/127.0.0.1:80");
    rec.record(AccessKind::Manifest, "oi", "pkg://oi/x@1:20200101T000000Z");
    // A crash: no clean end.
    std::mem::forget(rec);
    let out = ok(pkg6repo(&["retire", "status", "-s", s(&repo)]));
    assert!(out.contains("depot host/127.0.0.1:80"), "{out}");
    let out = fails(pkg6repo(&[
        "retire",
        "close-depot",
        "-s",
        s(&repo),
        "host/127.0.0.1:80",
    ]));
    assert!(out.contains("depot_active"), "{out}");
    let out = fails(pkg6repo(&[
        "retire",
        "close-depot",
        "-s",
        s(&repo),
        "other",
    ]));
    assert!(out.contains("unknown_depot"), "{out}");
}

#[test]
fn retire_output_follows_the_pkg6repo_command_rules() {
    // ADR-009: an unknown -F is a usage error, and -o never replaces a file.
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let archive = make_p5p(
        tmp.path(),
        "run1",
        &[
            pkg("library/foo", "1.0:20200101T000000Z"),
            pkg("library/foo", "2.0:20210101T000000Z"),
        ],
    );
    ok(pkg6repo(&[
        "import-pkg5",
        "-s",
        s(&archive),
        "-d",
        s(&repo),
    ]));

    for sub in ["plan", "status"] {
        let out = pkg6repo(&["retire", sub, "-s", s(&repo), "-F", "yaml"]);
        assert_eq!(out.status.code(), Some(2), "{sub}: {}", text(&out));
    }

    let plan = tmp.path().join("plan.json");
    fs::write(&plan, "operator notes").unwrap();
    let out = fails(pkg6repo(&[
        "retire",
        "plan",
        "-s",
        s(&repo),
        "--fmri",
        "regex:^pkg://oi/library/foo@1",
        "-o",
        s(&plan),
    ]));
    assert!(out.contains("already exists"), "{out}");
    assert_eq!(fs::read_to_string(&plan).unwrap(), "operator notes");
}

#[test]
fn retire_rejects_a_repository_uri_as_a_usage_error() {
    // ADR-009: -s takes a local path; a URI is a usage error (exit 2) for
    // every retire subcommand, not a later repository error.
    let tmp = TempDir::new().unwrap();
    let uri = format!("file://{}", tmp.path().join("repo").display());
    let plan = tmp.path().join("plan.json");
    let cases: [&[&str]; 6] = [
        &["plan"],
        &["apply", s(&plan)],
        &["restore", "pkg://oi/library/foo@1.0"],
        &["status"],
        &["close-depot", "depot"],
        &["check-policy"],
    ];
    for args in cases {
        let (sub, rest) = args.split_first().unwrap();
        let mut argv = vec!["retire", sub, "-s", uri.as_str()];
        argv.extend_from_slice(rest);
        let out = pkg6repo(&argv);
        assert_eq!(out.status.code(), Some(2), "{sub}: {}", text(&out));
        assert!(
            text(&out).contains("without the file:// prefix"),
            "{sub}: {}",
            text(&out)
        );
    }
}
