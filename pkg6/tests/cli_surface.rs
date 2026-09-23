//! Subcommands this release does not implement used to log "completed
//! successfully" and exit 0. They must fail with exit status 2, say so, and
//! leave the image alone; implemented commands must keep working.

use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

/// pkg6's exit status for an unimplemented command or option.
const UNSUPPORTED: i32 = 2;

const NOT_IMPLEMENTED: [&str; 9] = [
    "exact-install",
    "uninstall",
    "update",
    "info",
    "search",
    "verify",
    "fix",
    "history",
    "contents",
];

fn pkg6(home: &Path) -> Command {
    let mut cmd = cargo_bin_cmd!("pkg6");
    // Never fall back to a real $HOME/.pkg or to /.
    cmd.env("HOME", home)
        .env("RUST_LOG", "error")
        .timeout(Duration::from_secs(30));
    cmd
}

#[test]
fn not_implemented_commands_fail_without_touching_the_image() {
    let home = TempDir::new().unwrap();
    let image = home.path().join("image");
    for sub in NOT_IMPLEMENTED {
        for extra in [&[][..], &["-n", "-v", "pkg:/editor/vim"], &["--help"]] {
            pkg6(home.path())
                .args(["-R", image.to_str().unwrap(), sub])
                .args(extra)
                .assert()
                .code(UNSUPPORTED)
                .stdout(predicate::str::is_empty())
                .stderr(predicate::str::contains(format!(
                    "pkg6 {sub} is not implemented in this release"
                )))
                .stderr(predicate::str::contains("pkg6 made no changes"))
                .stderr(predicate::str::contains("successfully").not());
        }
    }
    assert!(
        !image.exists(),
        "an unimplemented command created the image"
    );
}

#[test]
fn help_lists_only_implemented_commands() {
    let home = TempDir::new().unwrap();
    let out = pkg6(home.path()).arg("--help").assert().success();
    let help = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let listed: Vec<&str> = help
        .lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert_eq!(
        listed,
        [
            "refresh",
            "install",
            "list",
            "set-publisher",
            "unset-publisher",
            "publisher",
            "image-create",
            "help"
        ],
        "{help}"
    );
}

#[test]
fn install_rejects_options_it_would_ignore() {
    let home = TempDir::new().unwrap();
    let image = home.path().join("image");
    for (args, what) in [
        (
            &["-g", "http://pkg.example.org/", "vim"][..],
            "pkg6 install -g",
        ),
        (&["--licenses", "vim"], "pkg6 install --licenses"),
        (&["-C", "2", "vim"], "pkg6 install -C"),
    ] {
        pkg6(home.path())
            .args(["-R", image.to_str().unwrap(), "install"])
            .args(args)
            .assert()
            .code(UNSUPPORTED)
            .stderr(predicate::str::contains(format!(
                "{what} is not implemented in this release"
            )));
    }
}

#[test]
fn install_still_runs_and_fails_as_a_normal_error() {
    // Install is implemented: it gets as far as opening the image, and a
    // missing image is an ordinary error (1), not "not implemented" (2).
    let home = TempDir::new().unwrap();
    let image = home.path().join("absent");
    pkg6(home.path())
        .args(["-R", image.to_str().unwrap(), "install", "-n", "vim"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not implemented").not());
}

#[test]
fn unknown_commands_are_usage_errors() {
    let home = TempDir::new().unwrap();
    pkg6(home.path())
        .arg("frobnicate")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn list_rejects_parts_of_the_request_it_would_drop() {
    // A script asking for JSON, or for several packages, used to get a table
    // of something else and exit 0.
    let home = TempDir::new().unwrap();
    let image = home.path().join("image");
    for (args, what) in [
        (&["-o", "json"][..], "pkg6 list -o"),
        (&["-a", "-o", "json", "vim"], "pkg6 list -o"),
        (
            &["editor/vim", "web/server"],
            "pkg6 list with several patterns",
        ),
    ] {
        pkg6(home.path())
            .args(["-R", image.to_str().unwrap(), "list"])
            .args(args)
            .assert()
            .code(UNSUPPORTED)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(format!(
                "{what} is not implemented in this release"
            )));
    }
    assert!(!image.exists());
}

/// An empty image at `<home>/image`, returned as a path string.
fn empty_image(home: &Path) -> String {
    let image = home.join("image").to_str().unwrap().to_string();
    pkg6(home)
        .args(["image-create", "-F", &image])
        .assert()
        .success();
    image
}

#[test]
fn list_with_nothing_to_show_exits_1_like_pkg() {
    // pkg(1) list exits 1 when nothing matches; pkg6 used to print an empty
    // table and exit 0.
    let home = TempDir::new().unwrap();
    let image = empty_image(home.path());
    for (args, msg) in [
        (&[][..], "no packages installed"),
        (&["-v"], "no packages installed"),
        (
            &["editor/vim"],
            "no packages matching 'editor/vim' installed",
        ),
    ] {
        pkg6(home.path())
            .args(["-R", &image, "list"])
            .args(args)
            .assert()
            .code(1)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(msg));
    }
    // -q: exit status only, nothing printed at all.
    pkg6(home.path())
        .args(["-R", &image, "list", "-q", "editor/vim"])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());
}

#[test]
fn publisher_unknown_name_fails() {
    let home = TempDir::new().unwrap();
    let image = empty_image(home.path());
    pkg6(home.path())
        .args(["-R", &image, "publisher", "nosuch.org"])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("unknown publisher(s): nosuch.org"));
    // Without arguments an image without publishers is not an error.
    pkg6(home.path())
        .args(["-R", &image, "publisher"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No publishers configured"));
}

#[test]
fn publisher_known_and_unknown_name_fails() {
    // A known name must not hide an unknown one.
    let home = TempDir::new().unwrap();
    let image = home.path().join("image");
    let image = image.to_str().unwrap();
    pkg6(home.path())
        .args(["image-create", "-F", image, "-p", "test", "-g"])
        .arg("http://127.0.0.1:9/")
        .assert()
        .success();
    pkg6(home.path())
        .args(["-R", image, "publisher", "test"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Publisher: test"));
    pkg6(home.path())
        .args(["-R", image, "publisher", "test", "nosuch.org"])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("unknown publisher(s): nosuch.org"));
}

#[test]
fn unset_publisher_does_not_need_the_network() {
    // unset-publisher used to re-download every remaining catalog, log a
    // failure and exit 0. The catalog is now rebuilt locally.
    let home = TempDir::new().unwrap();
    let image = home.path().join("image");
    let image = image.to_str().unwrap();
    pkg6(home.path())
        .args(["image-create", "-F", image, "-p", "gone", "-g"])
        .arg("http://127.0.0.1:9/")
        .assert()
        .success();
    let catalog = home.path().join("image/var/pkg/catalog/gone");
    std::fs::create_dir_all(&catalog).unwrap();
    pkg6(home.path())
        .args(["-R", image, "unset-publisher", "gone"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    assert!(!catalog.exists(), "the removed publisher's catalog is left");
    pkg6(home.path())
        .args(["-R", image, "publisher", "gone"])
        .assert()
        .code(1);
    pkg6(home.path())
        .args(["-R", image, "unset-publisher", "gone"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Publisher not found: gone"));
}

#[test]
fn unset_publisher_reports_a_failed_catalog_rebuild() {
    // The publisher is removed but its packages may still be offered: that
    // must be an error that says so, not a success.
    let home = TempDir::new().unwrap();
    let image = home.path().join("image");
    let image = image.to_str().unwrap();
    pkg6(home.path())
        .args(["image-create", "-F", image, "-p", "gone", "-g"])
        .arg("http://127.0.0.1:9/")
        .assert()
        .success();
    // A directory where the catalog database file belongs cannot be removed.
    let active_db = home.path().join("image/var/pkg/active.db");
    let _ = std::fs::remove_file(&active_db);
    std::fs::create_dir_all(active_db.join("x")).unwrap();
    pkg6(home.path())
        .args(["-R", image, "unset-publisher", "gone"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "publisher gone was removed, but the image catalog could not be rebuilt",
        ))
        .stderr(predicate::str::contains("pkg6 refresh"));
}

#[test]
fn set_publisher_mirror_without_origin_is_rejected() {
    // Without -O the publisher was re-added with its old mirrors, silently
    // dropping the ones asked for.
    let home = TempDir::new().unwrap();
    let image = home.path().join("image");
    pkg6(home.path())
        .args(["-R", image.to_str().unwrap(), "set-publisher"])
        .args(["-p", "openindiana.org", "-M", "http://mirror.example.org/"])
        .assert()
        .code(UNSUPPORTED)
        .stderr(predicate::str::contains(
            "pkg6 set-publisher -M without -O is not implemented in this release",
        ));
    assert!(!image.exists());
}

#[test]
fn help_for_hidden_commands_says_they_are_not_implemented() {
    let home = TempDir::new().unwrap();
    for sub in NOT_IMPLEMENTED {
        pkg6(home.path())
            .args(["help", sub])
            .assert()
            .stdout(predicate::str::contains(
                "Not implemented in this release (fails with exit status 2)",
            ));
    }
}

#[test]
fn install_help_describes_the_options_it_keeps() {
    let home = TempDir::new().unwrap();
    pkg6(home.path())
        .args(["install", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("must-accept"))
        .stdout(predicate::str::contains("never updates it"))
        .stdout(predicate::str::contains("never refreshes them"))
        .stdout(predicate::str::contains("-C ").not());
}

#[test]
fn set_publisher_rejects_names_that_are_paths() {
    // `-p ..` used to be saved, so `unset-publisher ..` deleted
    // var/pkg/catalog/.., the whole image metadata.
    let home = TempDir::new().unwrap();
    let image = empty_image(home.path());
    let config = home.path().join("image/var/pkg/pkg6.image.json");
    let before = std::fs::read_to_string(&config).unwrap();
    for name in ["..", "a/b", "../x"] {
        pkg6(home.path())
            .args(["-R", &image, "set-publisher", "-p", name, "-O"])
            .arg("http://127.0.0.1:9/")
            .assert()
            .code(1)
            .stderr(predicate::str::contains("Invalid publisher name"));
    }
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
}

#[test]
fn path_publisher_names_cannot_delete_the_image_metadata() {
    // An image configured before names were checked may still hold one.
    let home = TempDir::new().unwrap();
    let image = home.path().join("image");
    let image = image.to_str().unwrap();
    pkg6(home.path())
        .args(["image-create", "-F", image, "-p", "legacy", "-g"])
        .arg("http://127.0.0.1:9/")
        .assert()
        .success();
    let meta = home.path().join("image/var/pkg");
    let config = meta.join("pkg6.image.json");
    let json = std::fs::read_to_string(&config).unwrap();
    assert!(json.contains("\"legacy\""));
    std::fs::write(&config, json.replace("\"legacy\"", "\"..\"")).unwrap();
    std::fs::write(meta.join("marker"), "image metadata").unwrap();
    let witnesses = [
        config.clone(),
        meta.join("installed.db"),
        meta.join("marker"),
    ];

    pkg6(home.path())
        .args(["-R", image, "refresh", "--full", ".."])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Invalid publisher name"));
    for w in &witnesses {
        assert!(w.exists(), "refresh --full deleted {}", w.display());
    }

    pkg6(home.path())
        .args(["-R", image, "unset-publisher", ".."])
        .assert()
        .success();
    for w in &witnesses {
        assert!(w.exists(), "unset-publisher deleted {}", w.display());
    }
    pkg6(home.path())
        .args(["-R", image, "publisher", ".."])
        .assert()
        .code(1);
}

#[test]
fn refresh_known_and_unknown_publisher_fails() {
    // A known name must not hide an unknown one; checked before any download.
    let home = TempDir::new().unwrap();
    let image = home.path().join("image");
    let image = image.to_str().unwrap();
    pkg6(home.path())
        .args(["image-create", "-F", image, "-p", "known", "-g"])
        .arg("http://127.0.0.1:9/")
        .assert()
        .success();
    pkg6(home.path())
        .args(["-R", image, "refresh", "known", "nosuch"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("Refresh completed successfully").not())
        .stderr(predicate::str::contains("Publisher not found: nosuch"));
}
