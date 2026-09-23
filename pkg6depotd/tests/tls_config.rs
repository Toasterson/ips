//! The `tls` configuration block fails closed: any inconsistency stops both
//! `config-test` and `start`, and the depot never falls back to plain HTTP
//! when TLS was asked for. `config-test` never prints secrets.

mod support;

use assert_cmd::cargo::cargo_bin_cmd;
use pkg6depotd::tls::plan::{ChallengePlan, DnsProviderKind, LETSENCRYPT_STAGING, TlsPlan};
use predicates::prelude::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

const BASE: &str = r#"
server {
    bind "127.0.0.1:0"
}
repository {
    root "/nonexistent/pkg6depotd-test-repo"
}
"#;

fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

fn config(dir: &Path, tls: &str) -> PathBuf {
    write(dir, "depot.kdl", &format!("{BASE}{tls}"))
}

fn token_file(dir: &Path, mode: u32) -> PathBuf {
    let path = write(dir, "cf.token", "cf-s3cr3t-token\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    path
}

/// miette wraps reports at 80 columns; compare with the wrapping removed.
fn flat(s: &str) -> String {
    s.replace(['│', '×'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn says(message: &'static str) -> impl Predicate<str> {
    predicate::function(move |s: &str| flat(s).contains(message))
}

fn depotd(cwd: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    cargo_bin_cmd!("pkg6depotd")
        .current_dir(cwd)
        .env("RUST_LOG", "error")
        .args(args)
        .timeout(Duration::from_secs(30))
        .assert()
}

fn acme(extra: &str) -> String {
    format!(
        r#"
tls {{
    acme {{
        domains "pkg.example.org"
        contact "mailto:hostmaster@example.org"
        terms-agreed true
        state-dir "/var/pkg6depotd/acme"
        {extra}
    }}
}}
"#
    )
}

/// (name, tls block, expected message fragment)
fn rejected_cases(token: &Path) -> Vec<(&'static str, String, &'static str)> {
    let t = token.display();
    vec![
        (
            "empty tls",
            "tls {\n}\n".into(),
            "names no certificate source",
        ),
        (
            "cert without key",
            "tls {\n certificate \"/etc/c.pem\"\n}\n".into(),
            "must be given together",
        ),
        (
            "files and acme",
            "tls {\n certificate \"/c\"\n key \"/k\"\n acme {\n }\n}\n".into(),
            "mutually exclusive",
        ),
        (
            "relative cert",
            "tls {\n certificate \"c.pem\"\n key \"/k.pem\"\n}\n".into(),
            "not an absolute path",
        ),
        (
            "no domains",
            "tls {\n acme {\n  terms-agreed true\n }\n}\n".into(),
            "domains is missing",
        ),
        (
            "terms not agreed",
            acme("challenge \"http-01\"\n http01-bind \"0.0.0.0:80\"")
                .replace("terms-agreed true", "terms-agreed false"),
            "terms-agreed is not true",
        ),
        ("no challenge", acme(""), "challenge is missing"),
        (
            "unknown challenge",
            acme("challenge \"tls-alpn-01\""),
            "unknown tls.acme.challenge",
        ),
        (
            "http-01 without bind",
            acme("challenge \"http-01\""),
            "http01-bind is missing",
        ),
        (
            "http-01 wildcard",
            acme("challenge \"http-01\"\n http01-bind \"0.0.0.0:80\"")
                .replace("\"pkg.example.org\"", "\"*.example.org\""),
            "cannot be validated with http-01",
        ),
        (
            "http-01 bind equals server bind",
            acme("challenge \"http-01\"\n http01-bind \"127.0.0.1:0\""),
            "equals server.bind",
        ),
        (
            "bad domain",
            acme("challenge \"http-01\"\n http01-bind \"0.0.0.0:80\"")
                .replace("\"pkg.example.org\"", "\"pkg_example\""),
            "not a",
        ),
        (
            "bad contact",
            acme("challenge \"http-01\"\n http01-bind \"0.0.0.0:80\"")
                .replace("mailto:hostmaster@example.org", "hostmaster@example.org"),
            "not a mailto: address",
        ),
        (
            "plain http directory",
            acme(
                "challenge \"http-01\"\n http01-bind \"0.0.0.0:80\"\n directory-url \"http://acme.example.org/dir\"",
            ),
            "is not an https URL",
        ),
        (
            "staging and directory",
            acme(
                "challenge \"http-01\"\n http01-bind \"0.0.0.0:80\"\n staging true\n directory-url \"https://acme.example.org/dir\"",
            ),
            "both set",
        ),
        (
            "dns-01 without block",
            acme("challenge \"dns-01\""),
            "dns01 block is missing",
        ),
        (
            "dns-01 unknown provider",
            acme(&format!(
                "challenge \"dns-01\"\n dns01 {{\n provider \"route53\"\n api-token-file \"{t}\"\n zone \"example.org\"\n }}"
            )),
            "unknown tls.acme.dns01.provider",
        ),
        (
            "dns-01 domain outside zone",
            acme(&format!(
                "challenge \"dns-01\"\n dns01 {{\n provider \"cloudflare\"\n api-token-file \"{t}\"\n zone \"example.net\"\n }}"
            )),
            "is not inside the DNS zone",
        ),
        (
            "gandi zone-id",
            acme(&format!(
                "challenge \"dns-01\"\n dns01 {{\n provider \"gandi\"\n api-token-file \"{t}\"\n zone \"example.org\"\n zone-id \"abc\"\n }}"
            )),
            "only used with cloudflare",
        ),
        (
            "gandi ttl too low",
            acme(&format!(
                "challenge \"dns-01\"\n dns01 {{\n provider \"gandi\"\n api-token-file \"{t}\"\n zone \"example.org\"\n ttl 60\n }}"
            )),
            "out of range for gandi",
        ),
        (
            "renew-before out of range",
            acme("challenge \"http-01\"\n http01-bind \"0.0.0.0:80\"\n renew-before-days 90"),
            "renew-before-days 90 is out of range",
        ),
    ]
}

#[test]
fn inconsistent_tls_blocks_fail_config_test_and_start() {
    let dir = TempDir::new().unwrap();
    let token = token_file(dir.path(), 0o600);
    for (name, tls, message) in rejected_cases(&token) {
        let path = config(dir.path(), &tls);
        let err = pkg6depotd::load_config(Some(path.clone()))
            .err()
            .unwrap_or_else(|| panic!("{name}: accepted"));
        assert!(
            flat(&format!("{err:?}")).contains(message),
            "{name}: expected {message:?} in {err:?}"
        );
        for cmd in ["config-test", "start"] {
            depotd(
                dir.path(),
                &["--no-daemon", "--config", path.to_str().unwrap(), cmd],
            )
            .failure()
            .stderr(says(message))
            .stderr(predicate::str::contains("s3cr3t").not());
        }
    }
}

#[test]
fn legacy_server_tls_keys_are_rejected_not_ignored() {
    let dir = TempDir::new().unwrap();
    let path = write(
        dir.path(),
        "legacy.kdl",
        r#"
server {
    bind "127.0.0.1:0"
    tls-cert "/etc/ssl/depot.pem"
    tls-key "/etc/ssl/depot.key"
}
repository {
    root "/nonexistent"
}
"#,
    );
    for cmd in ["config-test", "start"] {
        depotd(
            dir.path(),
            &["--no-daemon", "--config", path.to_str().unwrap(), cmd],
        )
        .failure()
        .stderr(says("tls-cert and server.tls-key are not supported"));
    }
}

#[test]
fn valid_acme_dns01_config_parses_into_a_typed_plan() {
    let dir = TempDir::new().unwrap();
    let token = token_file(dir.path(), 0o640);
    let path = config(
        dir.path(),
        &acme(&format!(
            "staging true\n challenge \"dns-01\"\n dns01 {{\n provider \"cloudflare\"\n api-token-file \"{}\"\n zone \"example.org\"\n propagation-seconds 30\n }}",
            token.display()
        ))
        .replace(
            "domains \"pkg.example.org\"",
            "domains \"pkg.example.org\" \"*.example.org\"",
        ),
    );
    let config = pkg6depotd::load_config(Some(path.clone())).unwrap();
    let Some(TlsPlan::Acme { settings, .. }) = pkg6depotd::tls::plan(&config).unwrap() else {
        panic!("expected an ACME plan");
    };
    assert_eq!(settings.directory_url, LETSENCRYPT_STAGING);
    assert_eq!(settings.domains.len(), 2);
    assert_eq!(settings.renew_before, Duration::from_secs(30 * 86_400));
    let ChallengePlan::Dns01(dns) = &settings.challenge else {
        panic!("expected dns-01");
    };
    assert_eq!(dns.provider, DnsProviderKind::Cloudflare);
    assert_eq!(dns.ttl, 120);
    assert_eq!(dns.propagation, Duration::from_secs(30));

    depotd(
        dir.path(),
        &["--config", path.to_str().unwrap(), "config-test"],
    )
    .success()
    .stdout(predicate::str::contains(
        "tls: acme pkg.example.org, *.example.org",
    ))
    .stdout(predicate::str::contains(
        "dns-01 via cloudflare in zone example.org",
    ))
    .stdout(predicate::str::contains("s3cr3t").not())
    .stderr(predicate::str::contains("s3cr3t").not());
}

#[test]
fn world_readable_token_file_fails_config_test() {
    let dir = TempDir::new().unwrap();
    let token = token_file(dir.path(), 0o644);
    let path = config(
        dir.path(),
        &acme(&format!(
            "challenge \"dns-01\"\n dns01 {{\n provider \"gandi\"\n api-token-file \"{}\"\n zone \"example.org\"\n }}",
            token.display()
        )),
    );
    depotd(
        dir.path(),
        &["--config", path.to_str().unwrap(), "config-test"],
    )
    .failure()
    .stderr(says("insecure permissions"))
    .stderr(predicate::str::contains("s3cr3t").not());
}

#[test]
fn group_readable_acme_state_dir_fails_config_test() {
    let dir = TempDir::new().unwrap();
    let state = dir.path().join("acme");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o750)).unwrap();
    let path = config(
        dir.path(),
        &acme("challenge \"http-01\"\n http01-bind \"127.0.0.1:1\"")
            .replace("/var/pkg6depotd/acme", state.to_str().unwrap()),
    );
    depotd(
        dir.path(),
        &["--config", path.to_str().unwrap(), "config-test"],
    )
    .failure()
    .stderr(says("must be 0700"));
}

#[test]
fn files_mode_config_test_checks_the_certificate() {
    let dir = TempDir::new().unwrap();
    let ca = support::TestCa::new();
    let names = ["pkg.example.org".to_string()];
    let (chain, key) = ca.issue_pem(&names, support::now() + 86_400, 5);
    let cert_path = write(dir.path(), "fullchain.pem", &chain);
    let key_path = write(dir.path(), "key.pem", &key);
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o640)).unwrap();
    let tls = format!(
        "tls {{\n certificate \"{}\"\n key \"{}\"\n}}\n",
        cert_path.display(),
        key_path.display()
    );
    let path = config(dir.path(), &tls);
    depotd(
        dir.path(),
        &["--config", path.to_str().unwrap(), "config-test"],
    )
    .success()
    .stdout(predicate::str::contains("tls: files"))
    .stdout(predicate::str::contains("names: pkg.example.org"))
    .stdout(predicate::str::contains("PRIVATE KEY").not());

    // A key file any user can read is refused.
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o644)).unwrap();
    depotd(
        dir.path(),
        &["--config", path.to_str().unwrap(), "config-test"],
    )
    .failure()
    .stderr(says("lets any user read the private key"));
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();

    // Not yet valid, expired, and mismatched keys are refused.
    let (early, early_key) =
        ca.issue_pem_window(&names, support::now() + 3600, support::now() + 86_400, 8);
    write(dir.path(), "fullchain.pem", &early);
    write(dir.path(), "key.pem", &early_key);
    depotd(
        dir.path(),
        &["--config", path.to_str().unwrap(), "config-test"],
    )
    .failure()
    .stderr(says("not valid before"));

    let (expired, expired_key) = ca.issue_pem(&names, support::now() - 60, 6);
    write(dir.path(), "fullchain.pem", &expired);
    write(dir.path(), "key.pem", &expired_key);
    depotd(
        dir.path(),
        &["--config", path.to_str().unwrap(), "config-test"],
    )
    .failure()
    .stderr(says("expired"));

    let (_, other_key) = ca.issue_pem(&names, support::now() + 86_400, 7);
    write(dir.path(), "fullchain.pem", &chain);
    write(dir.path(), "key.pem", &other_key);
    depotd(
        dir.path(),
        &["--config", path.to_str().unwrap(), "config-test"],
    )
    .failure()
    .stderr(says("does not match"))
    .stderr(predicate::str::contains("PRIVATE KEY").not());

    fs::remove_file(&key_path).unwrap();
    depotd(
        dir.path(),
        &["--config", path.to_str().unwrap(), "config-test"],
    )
    .failure();
}

#[test]
fn plain_http_config_test_says_so() {
    let dir = TempDir::new().unwrap();
    let path = config(dir.path(), "");
    depotd(
        dir.path(),
        &["--config", path.to_str().unwrap(), "config-test"],
    )
    .success()
    .stdout(predicate::str::contains("tls: off"));
}
