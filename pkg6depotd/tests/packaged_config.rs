//! The configuration shipped in the IPS package (packaging/ips/pkg6depotd.kdl)
//! must parse with the daemon's current schema. The SMF service refuses to
//! start without it, and a parse failure would otherwise only surface on an
//! installed system.

use pkg6depotd::config::Config;
use std::path::PathBuf;

fn packaged_config() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../packaging/ips/pkg6depotd.kdl")
}

#[test]
fn packaged_config_parses() {
    let config = Config::load(Some(packaged_config())).expect("packaged config must parse");

    assert_eq!(config.server.bind, vec!["127.0.0.1:8080".to_string()]);
    assert_eq!(
        config.repository.root,
        PathBuf::from("/var/pkg6depotd/repo")
    );
    assert_eq!(config.repository.mode.as_deref(), Some("readonly"));
    assert!(config.server.tls_cert.is_none() && config.server.tls_key.is_none());
    // Reverse-proxy mode by default; TLS examples stay commented out.
    assert!(config.tls.is_none());
    assert_eq!(pkg6depotd::tls::plan(&config).unwrap(), None);
    assert!(config.oauth2.is_none(), "v1 ships without remote auth");

    // The sample documents OTLP export but ships it disabled, and every
    // value in it must pass validation so enabling it cannot fail on them.
    let telemetry = pkg6depotd::telemetry_settings(&config).expect("valid telemetry");
    assert!(telemetry.otlp.is_none(), "OTLP export ships disabled");
    assert_eq!(telemetry.log_format, pkg6depotd::telemetry::LogFormat::Json);
    let otlp = config
        .telemetry
        .and_then(|t| t.otlp)
        .expect("sample documents the otlp block");
    assert_eq!(otlp.enabled, Some(false));
    assert_eq!(otlp.endpoint.as_deref(), Some("http://127.0.0.1:4318"));
}

#[test]
fn packaged_config_with_otlp_enabled_is_valid() {
    let text = std::fs::read_to_string(packaged_config()).unwrap();
    let enabled = text.replace("enabled false", "enabled true");
    assert_ne!(text, enabled);
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("enabled.kdl");
    std::fs::write(&path, enabled).unwrap();
    let config = Config::load(Some(path)).unwrap();
    let otlp = pkg6depotd::telemetry_settings(&config)
        .unwrap()
        .otlp
        .expect("enabled");
    assert_eq!(
        otlp.endpoint.signal_url("/v1/traces"),
        "http://127.0.0.1:4318/v1/traces"
    );
}

/// The commented-out tls examples must be valid once uncommented, so an
/// operator following them gets a working configuration. They are checked
/// together with OTLP export enabled: both blocks live in one file.
#[test]
fn packaged_tls_examples_parse_when_uncommented() {
    use pkg6depotd::tls::plan::{ChallengePlan, TlsPlan};

    let text = std::fs::read_to_string(packaged_config()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| **l == "// tls {")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(starts.len(), 2, "files and acme examples");

    let mut plans = Vec::new();
    for start in starts {
        let block: Vec<&str> = lines[start..]
            .iter()
            .take_while(|l| l.starts_with("//"))
            .map(|l| l.strip_prefix("// ").unwrap_or(""))
            .take_while(|l| *l != "}")
            .collect();
        let with_otlp = text.replace("enabled false", "enabled true");
        let example = format!("{with_otlp}\n{}\n}}\n", block.join("\n"));
        let config: Config = knuffel::parse("example.kdl", &example).expect("example parses");
        let telemetry = pkg6depotd::telemetry_settings(&config).expect("valid telemetry");
        assert!(
            telemetry.otlp.is_some(),
            "OTLP export enabled alongside TLS"
        );
        plans.push(pkg6depotd::tls::plan(&config).expect("example is valid"));
    }
    assert!(matches!(plans[0], Some(TlsPlan::Files { .. })));
    let Some(TlsPlan::Acme { settings, .. }) = &plans[1] else {
        panic!("second example is ACME");
    };
    assert!(matches!(settings.challenge, ChallengePlan::Http01 { .. }));
    assert_eq!(
        settings.directory_url,
        pkg6depotd::tls::plan::LETSENCRYPT_STAGING
    );
    // The package does not deliver the state directory, so every place an
    // operator reads must say how to create it privately for pkg6srv.
    let state = settings.state_dir.display().to_string();
    assert_eq!(state, "/var/pkg6depotd/acme");
    let docs = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../packaging/ips");
    for (file, create) in [
        ("pkg6depotd.kdl", format!("mkdir -m 0700 {state}")),
        ("README.md", format!("mkdir -m 0700 {state}")),
        ("pkg6depotd.8", format!("# mkdir -m 0700 {state}")),
    ] {
        let text = std::fs::read_to_string(docs.join(file)).unwrap();
        assert!(text.contains(&create), "{file} documents {create}");
        assert!(
            text.contains(&format!("chown pkg6srv:pkg6srv {state}")),
            "{file} documents the owner"
        );
    }
}
