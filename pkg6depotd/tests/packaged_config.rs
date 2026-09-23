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
}
