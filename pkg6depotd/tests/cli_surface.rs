//! The public command line offers only what works: `start` and
//! `config-test`. Control subcommands that used to print "not yet
//! implemented" and exit 0 are gone and must be rejected before any
//! configuration is read, and the SMF start method must keep parsing.

use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

/// clap's exit status for a usage error.
const USAGE: i32 = 2;

const VALID: &str = r#"
server {
    bind "127.0.0.1:0"
}
repository {
    root "/nonexistent/pkg6depotd-test-repo"
    mode "readonly"
}
"#;

fn depotd(cwd: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    cargo_bin_cmd!("pkg6depotd")
        .current_dir(cwd)
        .env("RUST_LOG", "error")
        .args(args)
        .timeout(Duration::from_secs(30))
        .assert()
}

fn valid_config(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("pkg6depotd.kdl");
    fs::write(&path, VALID).unwrap();
    path
}

#[test]
fn help_lists_only_supported_commands_and_options() {
    let dir = TempDir::new().unwrap();
    let out = depotd(dir.path(), &["--help"]).success();
    let help = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    for present in ["start", "config-test", "--config"] {
        assert!(help.contains(present), "help lacks {present}:\n{help}");
    }
    for absent in [
        "stop",
        "status",
        "reload",
        "health",
        "admin",
        "--no-daemon",
        "--pid-file",
    ] {
        assert!(!help.contains(absent), "help lists {absent}:\n{help}");
    }
}

#[test]
fn removed_control_subcommands_fail_as_usage_errors() {
    let dir = TempDir::new().unwrap();
    let config = valid_config(&dir);
    let config = config.to_str().unwrap();
    for cmd in [
        &["stop"][..],
        &["status"],
        &["reload"],
        &["health"],
        &["admin", "auth-check"],
    ] {
        let mut args = vec!["--config", config];
        args.extend_from_slice(cmd);
        depotd(dir.path(), &args)
            .code(USAGE)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("unrecognized subcommand"))
            .stderr(predicate::str::contains("not yet implemented").not());
    }
}

#[test]
fn pid_file_option_is_rejected() {
    let dir = TempDir::new().unwrap();
    let config = valid_config(&dir);
    depotd(
        dir.path(),
        &[
            "--config",
            config.to_str().unwrap(),
            "--pid-file",
            "/tmp/pkg6depotd.pid",
            "config-test",
        ],
    )
    .code(USAGE)
    .stderr(predicate::str::contains("--pid-file"));
}

#[test]
fn legacy_no_daemon_flag_is_still_accepted() {
    let dir = TempDir::new().unwrap();
    let config = valid_config(&dir);
    depotd(
        dir.path(),
        &[
            "--no-daemon",
            "--config",
            config.to_str().unwrap(),
            "config-test",
        ],
    )
    .success()
    .stdout(predicate::str::contains(
        "Configuration loaded successfully",
    ));
}

/// Run the exact argument list of the packaged SMF start method, with
/// `config-test` in place of `start` so the test does not serve.
#[test]
fn smf_start_method_arguments_parse() {
    let method = Path::new(env!("CARGO_MANIFEST_DIR")).join("../packaging/ips/svc-pkg6depotd");
    let script = fs::read_to_string(&method).unwrap();
    let exec: Vec<&str> = script
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("exec $DEPOTD"))
        .collect();
    assert_eq!(exec.len(), 1, "expected one exec line in {method:?}");
    let words: Vec<&str> = exec[0].split_whitespace().skip(2).collect();
    assert_eq!(words.last(), Some(&"start"), "SMF must run `start`");

    let dir = TempDir::new().unwrap();
    let config = valid_config(&dir);
    let mut args: Vec<&str> = words
        .iter()
        .map(|w| {
            if *w == "\"$config\"" {
                config.to_str().unwrap()
            } else {
                w
            }
        })
        .collect();
    *args.last_mut().unwrap() = "config-test";
    depotd(dir.path(), &args)
        .success()
        .stdout(predicate::str::contains(
            "Configuration loaded successfully",
        ));
}
