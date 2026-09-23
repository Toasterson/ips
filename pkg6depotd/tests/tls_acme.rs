//! Direct TLS and ACME against local mocks. Nothing here talks to a real CA
//! or DNS provider: the mock CA verifies JWS signatures and nonces, fetches
//! http-01 tokens from the depot's challenge listener, and checks dns-01
//! TXT records published through mock Cloudflare and Gandi APIs.

mod support;

use axum::Router;
use axum::routing::get;
use pkg6depotd::tls::acme::dns::{DnsProvider, Secret};
use pkg6depotd::tls::acme::http01::{self, Http01Tokens};
use pkg6depotd::tls::acme::{AcmeManager, Solver, Timing};
use pkg6depotd::tls::plan::{AcmeSettings, ChallengePlan, DnsPlan, DnsProviderKind, DomainName};
use pkg6depotd::tls::{CertSource, CertStore, LoadedCert, TlsError, TlsListener};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use support::{DnsZone, MockAcme, Tamper, TestCa, Validation, now};
use tempfile::TempDir;

const HOST: &str = "pkg.example.test";

fn fast_timing() -> Timing {
    Timing {
        request_timeout: Duration::from_secs(5),
        poll_interval: Duration::from_millis(20),
        max_polls: 50,
        check_interval: Duration::from_millis(200),
        retry_base: Duration::from_millis(100),
        retry_max: Duration::from_millis(400),
        startup_attempts: 2,
        startup_retry: Duration::from_millis(50),
    }
}

fn settings(
    directory_url: String,
    state_dir: &Path,
    domains: &[&str],
    challenge: ChallengePlan,
) -> AcmeSettings {
    AcmeSettings {
        domains: domains
            .iter()
            .map(|d| DomainName::parse(d).unwrap())
            .collect(),
        contact: vec!["mailto:hostmaster@example.test".into()],
        directory_url,
        state_dir: state_dir.to_path_buf(),
        renew_before: Duration::from_secs(30 * 86_400),
        challenge,
    }
}

async fn http01_listener(tokens: &Http01Tokens, domains: &[&str]) -> SocketAddr {
    http01_listener_for(tokens, domains, 8443).await
}

async fn http01_listener_for(
    tokens: &Http01Tokens,
    domains: &[&str],
    https_port: u16,
) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    http01::spawn(
        listener,
        tokens.clone(),
        domains.iter().map(|d| d.to_string()).collect(),
        https_port,
    );
    addr
}

/// Serve a trivial router over TLS from `store`; returns the address.
async fn serve_tls(store: Arc<CertStore>) -> SocketAddr {
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let listener =
        TlsListener::new(tcp, store.server_config().unwrap(), Duration::from_secs(5)).unwrap();
    let app = Router::new().route("/", get(|| async { "depot ok" }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// GET https://HOST/ via `addr`, trusting only the test CA. Returns the body
/// and the DER of the certificate the server presented.
async fn https_get(ca: &TestCa, addr: SocketAddr) -> (String, Vec<u8>) {
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_der(&ca.cert_der).unwrap())
        .resolve(HOST, addr)
        .tls_info(true)
        .build()
        .unwrap();
    let resp = client
        .get(format!("https://{HOST}:{}/", addr.port()))
        .send()
        .await
        .expect("HTTPS request with the issued certificate");
    let peer = resp
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .and_then(|i| i.peer_certificate())
        .expect("peer certificate")
        .to_vec();
    (resp.text().await.unwrap(), peer)
}

fn leaf_der(cert: &LoadedCert) -> Vec<u8> {
    cert.certified.cert[0].as_ref().to_vec()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn http01_issues_persists_and_serves_https() {
    let ca = Arc::new(TestCa::new());
    let tokens = Http01Tokens::default();
    let http01_addr = http01_listener(&tokens, &[HOST]).await;
    let mock = MockAcme::start(ca.clone(), Validation::Http01(http01_addr)).await;
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("acme");
    let s = settings(
        mock.directory_url(),
        &state,
        &[HOST],
        ChallengePlan::Http01 { bind: http01_addr },
    );

    let manager =
        AcmeManager::new(s.clone(), Solver::Http01(tokens.clone()), fast_timing()).unwrap();
    let cert = manager.obtain().await.expect("issuance through http-01");

    assert!(cert.covers([HOST]));
    assert!(!manager.needs_renewal(&cert, now()));
    let c = mock.counters();
    assert_eq!(c.bad_nonce_sent, 1, "the stale-nonce retry was exercised");
    assert_eq!((c.accounts, c.orders_created, c.certs_issued), (1, 1, 1));
    assert!(tokens.is_empty(), "http-01 tokens are withdrawn afterwards");

    // Persistent state: private directory and files, no leftover order.
    assert_eq!(mode(&state), 0o700);
    let bundle = state.join("certificate.pem");
    assert_eq!(mode(&bundle), 0o600);
    assert!(!state.join("order.json").exists());
    let accounts: Vec<_> = std::fs::read_dir(&state)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("account-"))
        .collect();
    assert_eq!(accounts.len(), 1);
    assert_eq!(mode(&state.join(&accounts[0])), 0o600);

    // Serve it and connect with a client that trusts only the test CA.
    let store = CertStore::new(cert.clone(), CertSource::Bundle(bundle.clone()));
    let addr = serve_tls(store.clone()).await;
    let (body, peer) = https_get(&ca, addr).await;
    assert_eq!(body, "depot ok");
    assert_eq!(peer, leaf_der(&cert));

    // A restarted depot finds the certificate and reuses the account.
    let again = AcmeManager::new(s, Solver::Http01(tokens.clone()), fast_timing()).unwrap();
    let stored = again.load_current().expect("stored certificate");
    assert_eq!(leaf_der(&stored), leaf_der(&cert));
    assert!(!again.needs_renewal(&stored, now()));
    again.obtain().await.unwrap();
    let c = mock.counters();
    assert_eq!(c.accounts, 1, "same account key after restart");
    assert_eq!(c.new_account, 2);
}

#[tokio::test]
async fn renewal_swaps_the_certificate_in_a_running_server() {
    let ca = Arc::new(TestCa::new());
    let tokens = Http01Tokens::default();
    let http01_addr = http01_listener(&tokens, &[HOST]).await;
    let mock = MockAcme::start(ca.clone(), Validation::Http01(http01_addr)).await;
    let tmp = TempDir::new().unwrap();
    let s = settings(
        mock.directory_url(),
        &tmp.path().join("acme"),
        &[HOST],
        ChallengePlan::Http01 { bind: http01_addr },
    );
    let manager = Arc::new(AcmeManager::new(s, Solver::Http01(tokens), fast_timing()).unwrap());

    // A certificate with one hour lifetime, 40 minutes of it used: less
    // than a third left, so it is due although renew-before is 30 days.
    mock.set_lifetime(-2400, 1200);
    let short = manager.obtain().await.unwrap();
    assert!(manager.needs_renewal(&short, now()));

    let store = CertStore::new(short.clone(), CertSource::Bundle(manager.bundle_path()));
    let addr = serve_tls(store.clone()).await;
    assert_eq!(https_get(&ca, addr).await.1, leaf_der(&short));

    mock.set_lifetime(-60, 90 * 86_400);
    tokio::spawn(manager.clone().renewal_loop(store.clone()));
    let renewed = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let current = store.current();
            if leaf_der(&current) != leaf_der(&short) {
                return current;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("renewal within 20s");

    assert!(!manager.needs_renewal(&renewed, now()));
    // New handshakes get the new certificate without a restart.
    assert_eq!(https_get(&ca, addr).await.1, leaf_der(&renewed));
    // And it is what a restart would load.
    assert_eq!(
        leaf_der(&manager.load_current().unwrap()),
        leaf_der(&renewed)
    );
}

fn dns_plan(provider: DnsProviderKind, token_file: &Path, zone_id: Option<&str>) -> DnsPlan {
    dns_plan_waiting(provider, token_file, zone_id, Duration::from_millis(10))
}

fn dns_plan_waiting(
    provider: DnsProviderKind,
    token_file: &Path,
    zone_id: Option<&str>,
    propagation: Duration,
) -> DnsPlan {
    DnsPlan {
        provider,
        api_token_file: token_file.to_path_buf(),
        zone: DomainName::parse("example.test").unwrap(),
        zone_id: zone_id.map(str::to_string),
        propagation,
        ttl: match provider {
            DnsProviderKind::Cloudflare => 120,
            DnsProviderKind::Gandi => 300,
        },
    }
}

fn write_token(dir: &Path, token: &str) -> std::path::PathBuf {
    let path = dir.join("token");
    std::fs::write(&path, format!("{token}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

/// Shared TXT name of the apex and the wildcard challenge.
const APEX_CHALLENGE: &str = "_acme-challenge.example.test";

/// Records that belong to somebody else and must survive every issuance
/// and cleanup: one at the very name our challenges use, with our comment
/// but another value, and one elsewhere.
fn seed_unrelated(zone: &DnsZone) -> Vec<(String, String)> {
    zone.seed(APEX_CHALLENGE, "unrelated-verification", None);
    zone.seed(
        APEX_CHALLENGE,
        "other-value-same-comment",
        Some(pkg6depotd::tls::acme::dns::CLOUDFLARE_COMMENT),
    );
    zone.seed("example.test", "v=spf1 -all", None);
    zone.all()
}

struct DnsSetup {
    ca: Arc<TestCa>,
    zone: DnsZone,
    mock: MockAcme,
    tmp: TempDir,
    settings: AcmeSettings,
    provider: DnsProvider,
    seeded: Vec<(String, String)>,
}

/// A wildcard and its apex share the TXT name _acme-challenge.example.test.
const DNS_DOMAINS: [&str; 3] = [HOST, "*.example.test", "example.test"];

async fn dns_setup(kind: DnsProviderKind, propagation: Duration) -> DnsSetup {
    let ca = Arc::new(TestCa::new());
    let zone = DnsZone::default();
    let seeded = seed_unrelated(&zone);
    let token = "test-token-s3cr3t";
    let api = match kind {
        DnsProviderKind::Cloudflare => {
            support::cloudflare(zone.clone(), "example.test", token).await
        }
        DnsProviderKind::Gandi => support::gandi(zone.clone(), "example.test", token).await,
    };
    let mock = MockAcme::start(ca.clone(), Validation::Dns01(zone.lookup())).await;
    let tmp = TempDir::new().unwrap();
    let token_file = write_token(tmp.path(), token);
    let plan = dns_plan_waiting(kind, &token_file, None, propagation);
    let http = pkg6depotd::tls::acme::default_http_client(&fast_timing()).unwrap();
    let provider = DnsProvider::from_plan(&plan, http)
        .unwrap()
        .with_api_base(api);
    let settings = settings(
        mock.directory_url(),
        &tmp.path().join("acme"),
        &DNS_DOMAINS,
        ChallengePlan::Dns01(plan),
    );
    DnsSetup {
        ca,
        zone,
        mock,
        tmp,
        settings,
        provider,
        seeded,
    }
}

impl DnsSetup {
    fn manager(&self) -> AcmeManager {
        AcmeManager::new(
            self.settings.clone(),
            Solver::Dns01(self.provider.clone()),
            fast_timing(),
        )
        .unwrap()
    }

    fn journal(&self) -> std::path::PathBuf {
        self.tmp.path().join("acme").join("challenges.json")
    }

    /// Only the unrelated records are left, and nothing clobbered them.
    fn assert_only_unrelated_left(&self) {
        assert_eq!(self.zone.all(), self.seeded, "{}", self.zone.log_text());
        let log = self.zone.log_text();
        assert!(!log.contains("VIOLATION"), "{log}");
        assert!(!self.journal().exists(), "cleanup journal is removed");
    }
}

async fn dns01_round_trip(kind: DnsProviderKind) {
    let t = dns_setup(kind, Duration::from_millis(10)).await;
    let _ = &t.ca;
    let manager = t.manager();
    let cert = manager.obtain().await.expect("issuance through dns-01");

    assert!(cert.covers(DNS_DOMAINS));
    assert_eq!(t.mock.counters().challenges_validated, 3);
    t.assert_only_unrelated_left();
    let log = t.zone.log_text();
    match kind {
        DnsProviderKind::Cloudflare => assert!(log.contains("delete"), "{log}"),
        DnsProviderKind::Gandi => {
            // Added to the existing apex RRset, created for the absent one,
            // removed value by value.
            assert!(
                log.contains("patch _acme-challenge add=2 remove=0"),
                "{log}"
            );
            assert!(log.contains("post _acme-challenge.pkg"), "{log}");
            assert!(
                log.contains("patch _acme-challenge add=0 remove=2"),
                "{log}"
            );
        }
    }
}

#[tokio::test]
async fn dns01_cloudflare_publishes_and_cleans_up() {
    dns01_round_trip(DnsProviderKind::Cloudflare).await;
}

#[tokio::test]
async fn dns01_gandi_publishes_and_cleans_up() {
    dns01_round_trip(DnsProviderKind::Gandi).await;
}

#[tokio::test]
async fn dns_provider_rejection_fails_without_leaking_the_token() {
    let ca = Arc::new(TestCa::new());
    let zone = DnsZone::default();
    let api = support::cloudflare(zone.clone(), "example.test", "the-right-token").await;
    let mock = MockAcme::start(ca, Validation::Dns01(zone.lookup())).await;
    let tmp = TempDir::new().unwrap();
    let plan = dns_plan(
        DnsProviderKind::Cloudflare,
        &tmp.path().join("unused"),
        None,
    );
    let http = pkg6depotd::tls::acme::default_http_client(&fast_timing()).unwrap();
    let provider =
        DnsProvider::new(&plan, http, Secret::new("wrong-s3cr3t-token")).with_api_base(api);
    let s = settings(
        mock.directory_url(),
        &tmp.path().join("acme"),
        &[HOST],
        ChallengePlan::Dns01(plan),
    );
    let manager = AcmeManager::new(s, Solver::Dns01(provider), fast_timing()).unwrap();

    let err = manager.obtain().await.expect_err("provider refused");
    let text = format!("{err} {err:?}");
    assert!(text.contains("Authentication error"), "{text}");
    assert!(!text.contains("s3cr3t"), "token leaked: {text}");
    assert!(manager.load_current().is_none());
    assert_eq!(mock.counters().certs_issued, 0);
}

#[tokio::test]
async fn failed_challenge_is_reported_and_cleaned_up() {
    let ca = Arc::new(TestCa::new());
    let tokens = Http01Tokens::default();
    let http01_addr = http01_listener(&tokens, &[HOST]).await;
    let mock = MockAcme::start(ca, Validation::Reject).await;
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("acme");
    let s = settings(
        mock.directory_url(),
        &state,
        &[HOST],
        ChallengePlan::Http01 { bind: http01_addr },
    );
    let manager = AcmeManager::new(s, Solver::Http01(tokens.clone()), fast_timing()).unwrap();

    let started = std::time::Instant::now();
    let err = manager
        .obtain_with_retries()
        .await
        .expect_err("every challenge is rejected");
    assert!(started.elapsed() < Duration::from_secs(15), "bounded");
    match err {
        TlsError::Acme(pkg6depotd::tls::AcmeError::Challenge { domain, detail }) => {
            assert_eq!(domain, HOST);
            assert!(detail.contains("rejects every challenge"), "{detail}");
        }
        other => panic!("unexpected error {other:?}"),
    }
    // Two startup attempts, one order each; nothing left behind.
    assert_eq!(mock.counters().orders_created, 2);
    assert!(tokens.is_empty());
    assert!(!state.join("order.json").exists());
    assert!(!state.join("certificate.pem").exists());
}

#[tokio::test]
async fn interrupted_order_is_resumed_after_restart() {
    let ca = Arc::new(TestCa::new());
    let tokens = Http01Tokens::default();
    let http01_addr = http01_listener(&tokens, &[HOST]).await;
    let mock = MockAcme::start(ca, Validation::Http01(http01_addr)).await;
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("acme");
    let s = settings(
        mock.directory_url(),
        &state,
        &[HOST],
        ChallengePlan::Http01 { bind: http01_addr },
    );
    let timing = Timing {
        request_timeout: Duration::from_secs(1),
        ..fast_timing()
    };

    // Finalize hangs past the request timeout: a network failure, so the
    // order is kept for the next attempt.
    mock.hang_next_finalize(Duration::from_secs(3));
    let first =
        AcmeManager::new(s.clone(), Solver::Http01(tokens.clone()), timing.clone()).unwrap();
    let err = first.obtain().await.expect_err("finalize timed out");
    assert!(
        matches!(err, TlsError::Acme(pkg6depotd::tls::AcmeError::Http { .. })),
        "{err:?}"
    );
    assert!(state.join("order.json").exists());
    assert_eq!(mode(&state.join("order.json")), 0o600);

    let second = AcmeManager::new(s, Solver::Http01(tokens), timing).unwrap();
    second.obtain().await.expect("resumed order completes");
    let c = mock.counters();
    assert_eq!(c.orders_created, 1, "no second order was opened");
    assert_eq!(c.certs_issued, 1);
    assert!(!state.join("order.json").exists());
}

#[tokio::test]
async fn http01_listener_answers_only_known_tokens_and_redirects_own_names() {
    let tokens = Http01Tokens::default();
    let addr = http01_listener(&tokens, &[HOST]).await;
    tokens.insert("tok_1", "tok_1.thumb");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let get = |path: &str, host: &str| {
        client
            .get(format!("http://{addr}{path}"))
            .header("Host", host)
            .send()
    };

    let r = get("/.well-known/acme-challenge/tok_1", HOST)
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "tok_1.thumb");
    assert_eq!(
        get("/.well-known/acme-challenge/other", HOST)
            .await
            .unwrap()
            .status(),
        404
    );
    let r = get("/en/catalog/0/?x=1", &format!("{HOST}:80"))
        .await
        .unwrap();
    assert_eq!(r.status(), 308);
    // The redirect names the depot's HTTPS port.
    assert_eq!(
        r.headers()["location"],
        format!("https://{HOST}:8443/en/catalog/0/?x=1").as_str()
    );
    assert_eq!(get("/", "evil.example").await.unwrap().status(), 404);

    // On the default port the port is left out.
    let default_port = http01_listener_for(&tokens, &[HOST], 443).await;
    let r = client
        .get(format!("http://{default_port}/x"))
        .header("Host", HOST)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.headers()["location"],
        format!("https://{HOST}/x").as_str()
    );
}

/// Idle or half-sent connections are closed after their lifetime and the
/// cap never locks the CA out of the challenge route for longer than that.
#[tokio::test]
async fn http01_listener_bounds_connections_and_keeps_the_challenge_reachable() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let tokens = Http01Tokens::default();
    tokens.insert("tok_1", "tok_1.thumb");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    http01::spawn_with_limits(
        listener,
        tokens.clone(),
        vec![HOST.to_string()],
        8443,
        http01::Limits {
            max_connections: 2,
            connection_lifetime: Duration::from_millis(300),
        },
    );

    // Fill the cap: one silent client, one that never finishes its headers.
    let mut silent = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut slow = tokio::net::TcpStream::connect(addr).await.unwrap();
    slow.write_all(b"GET /.well-known/acme-challenge/tok_1 HTTP/1.1\r\nHost: ")
        .await
        .unwrap();

    let started = std::time::Instant::now();
    let r = tokio::time::timeout(
        Duration::from_secs(5),
        reqwest::Client::new()
            .get(format!("http://{addr}/.well-known/acme-challenge/tok_1"))
            .header("Host", HOST)
            .send(),
    )
    .await
    .expect("challenge answered once the stalled connections expire")
    .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "tok_1.thumb");
    assert!(started.elapsed() < Duration::from_secs(3));

    // Both stalled connections were closed by the server.
    for stream in [&mut silent, &mut slow] {
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
            .await
            .expect("server closed the connection")
            .unwrap_or(0);
        assert_eq!(n, 0, "no response to an incomplete request");
    }
}

/// SIGHUP (svcadm refresh) must not end a depot that is still waiting for
/// its first certificate.
#[tokio::test]
async fn sighup_before_a_certificate_is_served_is_harmless() {
    let _target = pkg6depotd::tls::spawn_hangup_handler().unwrap();
    nix::sys::signal::raise(nix::sys::signal::Signal::SIGHUP).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Still alive: the default action would have terminated the process.
}

#[tokio::test]
async fn static_files_reload_keeps_the_old_certificate_on_error() {
    let ca = TestCa::new();
    let tmp = TempDir::new().unwrap();
    let cert_path = tmp.path().join("fullchain.pem");
    let key_path = tmp.path().join("key.pem");
    let (chain, key) = ca.issue_pem(&[HOST.to_string()], now() + 86_400, 11);
    std::fs::write(&cert_path, &chain).unwrap();
    std::fs::write(&key_path, &key).unwrap();
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o640)).unwrap();

    let first = LoadedCert::from_files(&cert_path, &key_path).unwrap();
    let store = CertStore::new(
        first.clone(),
        CertSource::Files {
            certificate: cert_path.clone(),
            key: key_path.clone(),
        },
    );
    let addr = serve_tls(store.clone()).await;
    assert_eq!(https_get(&ca, addr).await.1, leaf_der(&first));

    // A key that does not belong to the certificate is refused.
    let (_, other_key) = ca.issue_pem(&[HOST.to_string()], now() + 86_400, 12);
    std::fs::write(&key_path, &other_key).unwrap();
    let err = store.reload().expect_err("mismatched key");
    assert!(err.to_string().contains("does not match"), "{err}");
    assert!(!err.to_string().contains("PRIVATE KEY"));
    assert_eq!(https_get(&ca, addr).await.1, leaf_der(&first));

    // Garbage is refused too.
    std::fs::write(&cert_path, "not a certificate").unwrap();
    assert!(store.reload().is_err());
    assert_eq!(leaf_der(&store.current()), leaf_der(&first));

    // A consistent pair that is not valid yet is refused.
    let (early, early_key) =
        ca.issue_pem_window(&[HOST.to_string()], now() + 3600, now() + 86_400, 14);
    std::fs::write(&cert_path, &early).unwrap();
    std::fs::write(&key_path, &early_key).unwrap();
    let err = store.reload().expect_err("not yet valid");
    assert!(err.to_string().contains("not valid before"), "{err}");
    assert_eq!(https_get(&ca, addr).await.1, leaf_der(&first));

    // So is one that has expired by the time of the reload.
    let (short, short_key) = ca.issue_pem(&[HOST.to_string()], now() + 600, 15);
    std::fs::write(&cert_path, &short).unwrap();
    std::fs::write(&key_path, &short_key).unwrap();
    let err = store.reload_at(now() + 3600).expect_err("expired");
    assert!(err.to_string().contains("expired"), "{err}");
    assert_eq!(leaf_der(&store.current()), leaf_der(&first));

    // A key file other users can read is refused, whatever it holds.
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = store.reload().expect_err("world-readable key");
    assert!(matches!(err, TlsError::Permissions { .. }), "{err:?}");
    assert_eq!(leaf_der(&store.current()), leaf_der(&first));
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    // A consistent new pair is swapped in.
    let (chain2, key2) = ca.issue_pem(&[HOST.to_string()], now() + 2 * 86_400, 13);
    std::fs::write(&cert_path, &chain2).unwrap();
    std::fs::write(&key_path, &key2).unwrap();
    store.reload().unwrap();
    let second = store.current();
    assert_ne!(leaf_der(&second), leaf_der(&first));
    assert_eq!(https_get(&ca, addr).await.1, leaf_der(&second));
}

#[tokio::test]
async fn slow_handshakes_do_not_block_other_clients() {
    let ca = TestCa::new();
    let (chain, key) = ca.issue_pem(&[HOST.to_string()], now() + 86_400, 21);
    let cert = LoadedCert::from_pem(chain.as_bytes(), key.as_bytes(), Path::new("mem")).unwrap();
    let store = CertStore::new(cert, CertSource::Bundle("/nonexistent".into()));
    let addr = serve_tls(store).await;

    // Open idle TCP connections that never start a handshake.
    let mut idle = Vec::new();
    for _ in 0..8 {
        idle.push(tokio::net::TcpStream::connect(addr).await.unwrap());
    }
    let res = tokio::time::timeout(Duration::from_secs(5), https_get(&ca, addr)).await;
    assert_eq!(res.expect("served despite idle connections").0, "depot ok");
}

#[tokio::test]
async fn dns01_cloudflare_recovers_a_lost_create_response() {
    let t = dns_setup(DnsProviderKind::Cloudflare, Duration::from_millis(10)).await;
    t.zone
        .lose_next_create_response
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let manager = t.manager();
    let cert = manager
        .obtain()
        .await
        .expect("the record exists although the answer was lost");
    assert!(cert.covers(DNS_DOMAINS));
    let log = t.zone.log_text();
    assert!(log.contains("create response lost"), "{log}");
    // Found again by name, value and comment, then deleted; the record
    // with our comment but another value stays.
    t.assert_only_unrelated_left();
}

/// A stop while records are published (the task is dropped, as a SIGTERM
/// or crash would end the process) leaves the journal; the next start
/// removes exactly our records.
async fn interrupted_dns01_is_cleaned_up_after_restart(kind: DnsProviderKind) {
    let t = dns_setup(kind, Duration::from_secs(60)).await;
    if kind == DnsProviderKind::Cloudflare {
        // Also without a record id: recovery must find it by marker.
        t.zone
            .lose_next_create_response
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    let manager = Arc::new(t.manager());
    let running = tokio::spawn({
        let m = manager.clone();
        async move { m.obtain().await }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while t.zone.all().len() < t.seeded.len() + 3 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("challenge records published");
    running.abort();
    let _ = running.await;
    drop(manager);

    assert!(t.journal().exists(), "cleanup intent survives the stop");
    assert_eq!(
        std::fs::metadata(t.journal()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(t.zone.all().len() > t.seeded.len());

    let restarted = t.manager();
    assert_eq!(
        restarted.pending_challenges().unwrap().len(),
        match kind {
            DnsProviderKind::Cloudflare => 3,
            DnsProviderKind::Gandi => 2,
        }
    );
    restarted
        .recover_challenges()
        .await
        .expect("records removed");
    t.assert_only_unrelated_left();
    assert_eq!(t.mock.counters().certs_issued, 0);
}

#[tokio::test]
async fn interrupted_dns01_cloudflare_is_cleaned_up_after_restart() {
    interrupted_dns01_is_cleaned_up_after_restart(DnsProviderKind::Cloudflare).await;
}

#[tokio::test]
async fn interrupted_dns01_gandi_is_cleaned_up_after_restart() {
    interrupted_dns01_is_cleaned_up_after_restart(DnsProviderKind::Gandi).await;
}

/// A damaged journal may list live records. It is never read as empty and
/// overwritten: issuance stops before publishing and the file stays.
#[tokio::test]
async fn corrupt_dns01_journal_fails_closed_and_is_left_untouched() {
    use std::os::unix::fs::OpenOptionsExt;

    let t = dns_setup(DnsProviderKind::Cloudflare, Duration::from_millis(50)).await;
    let manager = t.manager();
    let garbage = b"{\"records\": [{\"name\": \"_acme-challenge.example.test\", trunc";
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(t.journal())
            .unwrap();
        f.write_all(garbage).unwrap();
    }

    let err = manager.pending_challenges().unwrap_err();
    assert!(matches!(
        err,
        TlsError::Acme(pkg6depotd::tls::AcmeError::Journal { .. })
    ));
    let err = manager.recover_challenges().await.unwrap_err();
    assert!(err.to_string().contains("challenges.json"), "{err}");
    let err = manager.obtain().await.unwrap_err();
    assert!(
        matches!(
            err,
            TlsError::Acme(pkg6depotd::tls::AcmeError::Journal { .. })
        ),
        "{err}"
    );

    assert_eq!(std::fs::read(t.journal()).unwrap(), garbage);
    assert_eq!(t.zone.all(), t.seeded, "nothing was published");
    assert_eq!(t.mock.counters().certs_issued, 0);
}

#[tokio::test]
async fn startup_backoff_persists_across_restarts() {
    let ca = Arc::new(TestCa::new());
    let tokens = Http01Tokens::default();
    let http01_addr = http01_listener(&tokens, &[HOST]).await;
    let mock = MockAcme::start(ca, Validation::Reject).await;
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("acme");
    let s = settings(
        mock.directory_url(),
        &state,
        &[HOST],
        ChallengePlan::Http01 { bind: http01_addr },
    );
    let timing = Timing {
        retry_base: Duration::from_secs(2),
        retry_max: Duration::from_secs(8),
        ..fast_timing()
    };

    let first =
        AcmeManager::new(s.clone(), Solver::Http01(tokens.clone()), timing.clone()).unwrap();
    assert_eq!(first.backoff_delay(1), Duration::from_millis(50));
    assert_eq!(first.backoff_delay(2), Duration::from_secs(2));
    assert_eq!(first.backoff_delay(3), Duration::from_secs(4));
    assert_eq!(first.backoff_delay(30), Duration::from_secs(8));
    first.obtain_with_retries().await.expect_err("rejected");
    assert_eq!(mock.counters().orders_created, 2);
    let backoff: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state.join("backoff.json")).unwrap()).unwrap();
    assert_eq!(backoff["failures"], 2);

    // A restart (as SMF would do right away) waits out the stored delay
    // before it contacts the CA again.
    let restarted =
        AcmeManager::new(s.clone(), Solver::Http01(tokens.clone()), timing.clone()).unwrap();
    let started = std::time::Instant::now();
    let run = tokio::spawn(async move { restarted.obtain_with_retries().await });
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(
        mock.counters().orders_created,
        2,
        "no CA contact during backoff"
    );
    run.await.unwrap().expect_err("still rejected");
    assert!(started.elapsed() >= Duration::from_millis(1500));
    assert_eq!(mock.counters().orders_created, 4);
    let backoff: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state.join("backoff.json")).unwrap()).unwrap();
    assert_eq!(backoff["failures"], 4);

    // Success clears it.
    mock.set_validation(Validation::Http01(http01_addr));
    let s_ok = settings(
        mock.directory_url(),
        &state,
        &[HOST],
        ChallengePlan::Http01 { bind: http01_addr },
    );
    // A changed name set starts afresh instead of waiting.
    let other = AcmeSettings {
        domains: vec![DomainName::parse("other.example.test").unwrap()],
        ..s_ok
    };
    let fresh = AcmeManager::new(other, Solver::Http01(tokens.clone()), timing.clone()).unwrap();
    let started = std::time::Instant::now();
    fresh.wait_for_backoff().await;
    assert!(started.elapsed() < Duration::from_millis(500));
    let ok = AcmeManager::new(s, Solver::Http01(tokens), timing).unwrap();
    ok.obtain_with_retries()
        .await
        .expect("issued after backoff");
    assert!(!state.join("backoff.json").exists());
}

async fn tampered_order_is_refused(tamper: Tamper, expect: &str) {
    let ca = Arc::new(TestCa::new());
    let tokens = Http01Tokens::default();
    let http01_addr = http01_listener(&tokens, &[HOST]).await;
    let mock = MockAcme::start(ca, Validation::Http01(http01_addr)).await;
    mock.set_tamper(tamper);
    let tmp = TempDir::new().unwrap();
    let s = settings(
        mock.directory_url(),
        &tmp.path().join("acme"),
        &[HOST],
        ChallengePlan::Http01 { bind: http01_addr },
    );
    let manager = AcmeManager::new(s, Solver::Http01(tokens.clone()), fast_timing()).unwrap();
    let err = manager.obtain().await.expect_err("tampered order");
    let text = err.to_string();
    assert!(
        matches!(err, TlsError::Acme(pkg6depotd::tls::AcmeError::Protocol(_))),
        "{err:?}"
    );
    assert!(text.contains(expect), "{text}");
    assert_eq!(mock.counters().certs_issued, 0);
    assert_eq!(mock.counters().finalize_calls, 0);
    assert!(tokens.is_empty());
}

#[tokio::test]
async fn order_for_other_identifiers_is_refused() {
    tampered_order_is_refused(Tamper::ExtraIdentifier, "evil.example").await;
}

#[tokio::test]
async fn plain_http_ca_url_off_the_test_origin_is_refused() {
    tampered_order_is_refused(Tamper::PlainHttpFinalize, "must use HTTPS").await;
}
