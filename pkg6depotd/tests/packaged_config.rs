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
