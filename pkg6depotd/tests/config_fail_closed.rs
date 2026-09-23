//! pkg6depotd must refuse to run without a valid configuration. It used to
//! fall back to 0.0.0.0:8080 serving /tmp/pkg_repo, which would turn a typo in
//! a packaged service's config into a public listener.

use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use std::fs;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

const VALID: &str = r#"
server {
    bind "127.0.0.1:0"
}
repository {
    root "/nonexistent/pkg6depotd-test-repo"
    mode "readonly"
}
"#;

fn write(dir: &TempDir, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    fs::write(&path, body).unwrap();
    path
}

/// Run pkg6depotd in `cwd` with a hard timeout, so a regression that starts
/// serving fails the test instead of hanging it.
fn depotd(cwd: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    cargo_bin_cmd!("pkg6depotd")
        .current_dir(cwd)
        .env("RUST_LOG", "error")
        .args(args)
        .timeout(Duration::from_secs(30))
        .assert()
}

fn no_fallback() -> impl Predicate<str> {
    predicate::str::contains("Using default").not()
}

#[test]
fn missing_config_fails_config_test_and_start() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("absent.kdl");
    let missing = missing.to_str().unwrap();

    for cmd in ["config-test", "start"] {
        depotd(dir.path(), &["--no-daemon", "--config", missing, cmd])
            .failure()
            .stderr(predicate::str::contains("cannot load configuration"))
            .stderr(predicate::str::contains("absent.kdl"))
            .stderr(no_fallback());
    }
}

#[test]
fn no_config_flag_and_no_default_file_fails() {
    let dir = TempDir::new().unwrap();
    for cmd in ["config-test", "start"] {
        depotd(dir.path(), &["--no-daemon", cmd])
            .failure()
            .stderr(predicate::str::contains("pkg6depotd.kdl"))
            .stderr(no_fallback());
    }
}

#[test]
fn unparsable_config_fails() {
    let dir = TempDir::new().unwrap();
    let bad = write(&dir, "bad.kdl", "server { bind \"127.0.0.1:0\"\n");
    for cmd in ["config-test", "start"] {
        depotd(
            dir.path(),
            &["--no-daemon", "--config", bad.to_str().unwrap(), cmd],
        )
        .failure()
        .stderr(predicate::str::contains("cannot load configuration"))
        .stderr(no_fallback());
    }
}

#[test]
fn config_without_repository_block_fails() {
    let dir = TempDir::new().unwrap();
    let bad = write(
        &dir,
        "norepo.kdl",
        "server {\n    bind \"127.0.0.1:0\"\n}\n",
    );
    depotd(
        dir.path(),
        &["--config", bad.to_str().unwrap(), "config-test"],
    )
    .failure()
    .stderr(predicate::str::contains("cannot load configuration"));
}

#[test]
fn bad_bind_addresses_fail() {
    let cases = [
        (
            "empty.kdl",
            "server {\n    bind\n}\n",
            "bind address is missing",
        ),
        (
            "garbage.kdl",
            "server {\n    bind \"not-an-address\"\n}\n",
            "invalid server bind address",
        ),
        (
            "two.kdl",
            "server {\n    bind \"127.0.0.1:0\" \"127.0.0.1:1\"\n}\n",
            "only one server bind address",
        ),
    ];
    let dir = TempDir::new().unwrap();
    for (name, server, message) in cases {
        let body = format!("{server}repository {{\n    root \"/nonexistent\"\n}}\n");
        let path = write(&dir, name, &body);
        for cmd in ["config-test", "start"] {
            depotd(
                dir.path(),
                &["--no-daemon", "--config", path.to_str().unwrap(), cmd],
            )
            .failure()
            .stderr(predicate::str::contains(message))
            .stderr(no_fallback());
        }
    }
}

#[test]
fn valid_config_passes_config_test_without_a_repository() {
    let dir = TempDir::new().unwrap();
    let good = write(&dir, "good.kdl", VALID);
    depotd(
        dir.path(),
        &["--config", good.to_str().unwrap(), "config-test"],
    )
    .success()
    .stdout(predicate::str::contains(
        "Configuration loaded successfully",
    ));
}

#[test]
fn config_test_prints_a_summary_without_secrets() {
    let dir = TempDir::new().unwrap();
    let with_secrets = write(
        &dir,
        "secrets.kdl",
        r#"
server {
    bind "127.0.0.1:0"
}
repository {
    root "/nonexistent/pkg6depotd-test-repo"
}
telemetry {
    otlp-endpoint "https://collector:s3cr3t-otlp@otel.invalid:4318"
}
oauth2 {
    issuer "https://issuer.invalid/s3cr3t-issuer"
    audience "s3cr3t-audience"
}
"#,
    );
    depotd(
        dir.path(),
        &["--config", with_secrets.to_str().unwrap(), "config-test"],
    )
    .success()
    .stdout(predicate::str::contains("bind: 127.0.0.1:0"))
    .stdout(predicate::str::contains(
        "repository: /nonexistent/pkg6depotd-test-repo",
    ))
    .stdout(predicate::str::contains("s3cr3t").not())
    .stderr(predicate::str::contains("s3cr3t").not());
}

#[test]
fn start_with_valid_config_but_missing_repository_fails() {
    let dir = TempDir::new().unwrap();
    let good = write(&dir, "good.kdl", VALID);
    depotd(
        dir.path(),
        &["--no-daemon", "--config", good.to_str().unwrap(), "start"],
    )
    .failure();
}

#[test]
fn load_config_api_rejects_invalid_and_accepts_valid() {
    let dir = TempDir::new().unwrap();
    assert!(pkg6depotd::load_config(Some(dir.path().join("absent.kdl"))).is_err());

    let good = write(&dir, "good.kdl", VALID);
    let config = pkg6depotd::load_config(Some(good)).expect("valid config");
    assert_eq!(
        pkg6depotd::bind_addr(&config).unwrap(),
        "127.0.0.1:0".parse().unwrap()
    );
}
