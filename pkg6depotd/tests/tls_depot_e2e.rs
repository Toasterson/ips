//! The real `pkg6depotd start` binary serving HTTPS: from certificate files
//! with a SIGHUP reload, and with a certificate obtained at startup from the
//! mock ACME CA over http-01.

mod support;

use libips::repository::{FileBackend, RepositoryVersion, WritableRepository};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::{MockAcme, TestCa, Validation, now};
use tempfile::TempDir;

const HOST: &str = "pkg.example.test";

struct Depot(Child);

impl Drop for Depot {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Depot {
    fn start(config: &Path, cwd: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_pkg6depotd"))
            .args(["--no-daemon", "--config", config.to_str().unwrap(), "start"])
            .current_dir(cwd)
            .env("RUST_LOG", "pkg6depotd=info")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Depot(child)
    }

    fn hangup(&self) {
        kill(Pid::from_raw(self.0.id() as i32), Signal::SIGHUP).unwrap();
    }

    fn assert_running(&mut self) {
        assert!(self.0.try_wait().unwrap().is_none(), "depot exited");
    }
}

fn free_addr() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap()
}

fn repo(dir: &Path) -> PathBuf {
    let path = dir.join("repo");
    let mut backend = FileBackend::create(&path, RepositoryVersion::V4).unwrap();
    backend.add_publisher("test").unwrap();
    path
}

fn client(ca: &TestCa, addr: SocketAddr) -> reqwest::Client {
    reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_der(&ca.cert_der).unwrap())
        .resolve(HOST, addr)
        .tls_info(true)
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

/// GET /versions/0/ over HTTPS; returns the served leaf certificate.
async fn versions(ca: &TestCa, addr: SocketAddr) -> Result<Vec<u8>, String> {
    let resp = client(ca, addr)
        .get(format!("https://{HOST}:{}/versions/0/", addr.port()))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let peer = resp
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .and_then(|i| i.peer_certificate())
        .map(<[u8]>::to_vec)
        .ok_or("no peer certificate")?;
    let body = resp.text().await.map_err(|e| e.to_string())?;
    assert!(body.contains("versions"), "{body}");
    Ok(peer)
}

async fn wait_for<T, F, Fut>(what: &str, limit: Duration, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let start = Instant::now();
    let mut last = String::new();
    while start.elapsed() < limit {
        match f().await {
            Ok(v) => return v,
            Err(e) => last = e,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{what} not reached in {limit:?}: {last}");
}

fn leaf_of(chain_pem: &str) -> Vec<u8> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    CertificateDer::pem_slice_iter(chain_pem.as_bytes())
        .next()
        .unwrap()
        .unwrap()
        .as_ref()
        .to_vec()
}

#[tokio::test]
async fn depot_serves_https_from_files_and_reloads_on_sighup() {
    let dir = TempDir::new().unwrap();
    let ca = TestCa::new();
    let names = [HOST.to_string()];
    let (chain, key) = ca.issue_pem(&names, now() + 86_400, 31);
    let cert_path = dir.path().join("fullchain.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, &chain).unwrap();
    std::fs::write(&key_path, &key).unwrap();
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let addr = free_addr();
    let config = dir.path().join("depot.kdl");
    std::fs::write(
        &config,
        format!(
            "server {{\n bind \"{addr}\"\n}}\nrepository {{\n root \"{}\"\n}}\ntls {{\n certificate \"{}\"\n key \"{}\"\n}}\n",
            repo(dir.path()).display(),
            cert_path.display(),
            key_path.display()
        ),
    )
    .unwrap();

    let mut depot = Depot::start(&config, dir.path());
    let first = wait_for("HTTPS", Duration::from_secs(20), || versions(&ca, addr)).await;
    assert_eq!(first, leaf_of(&chain));

    // Plain HTTP is not served on the TLS port.
    let plain = reqwest::Client::new()
        .get(format!("http://{addr}/versions/0/"))
        .timeout(Duration::from_secs(5))
        .send()
        .await;
    assert!(plain.map(|r| !r.status().is_success()).unwrap_or(true));

    // A broken key is refused on SIGHUP; the depot keeps serving.
    std::fs::write(&key_path, "garbage").unwrap();
    depot.hangup();
    tokio::time::sleep(Duration::from_millis(300)).await;
    depot.assert_running();
    assert_eq!(versions(&ca, addr).await.unwrap(), first);

    // A new pair is picked up on SIGHUP without a restart.
    let (chain2, key2) = ca.issue_pem(&names, now() + 2 * 86_400, 32);
    std::fs::write(&cert_path, &chain2).unwrap();
    std::fs::write(&key_path, &key2).unwrap();
    depot.hangup();
    let second = wait_for("reloaded certificate", Duration::from_secs(10), || async {
        let peer = versions(&ca, addr).await?;
        if peer == first {
            Err("still the old certificate".to_string())
        } else {
            Ok(peer)
        }
    })
    .await;
    assert_eq!(second, leaf_of(&chain2));
    depot.assert_running();
}

#[tokio::test]
async fn depot_obtains_a_certificate_over_http01_at_startup() {
    let dir = TempDir::new().unwrap();
    let ca = Arc::new(TestCa::new());
    let http01 = free_addr();
    let mock = MockAcme::start(ca.clone(), Validation::Http01(http01)).await;
    let addr = free_addr();
    let state = dir.path().join("acme");
    let config = dir.path().join("depot.kdl");
    std::fs::write(
        &config,
        format!(
            r#"server {{
    bind "{addr}"
}}
repository {{
    root "{repo}"
}}
tls {{
    acme {{
        domains "{HOST}"
        contact "mailto:hostmaster@example.test"
        terms-agreed true
        directory-url "{dir}"
        state-dir "{state}"
        challenge "http-01"
        http01-bind "{http01}"
    }}
}}
"#,
            repo = repo(dir.path()).display(),
            dir = mock.directory_url(),
            state = state.display(),
        ),
    )
    .unwrap();

    let mut depot = Depot::start(&config, dir.path());
    let peer = wait_for(
        "HTTPS with ACME certificate",
        Duration::from_secs(60),
        || versions(&ca, addr),
    )
    .await;
    depot.assert_running();
    let c = mock.counters();
    assert_eq!((c.accounts, c.certs_issued), (1, 1));
    let bundle = std::fs::read_to_string(state.join("certificate.pem")).unwrap();
    assert_eq!(peer, leaf_of(&bundle));

    // A restart serves the stored certificate without a new order.
    drop(depot);
    let _depot = Depot::start(&config, dir.path());
    let again = wait_for("HTTPS after restart", Duration::from_secs(20), || {
        versions(&ca, addr)
    })
    .await;
    assert_eq!(again, peer);
    assert_eq!(mock.counters().orders_created, 1);
}

#[tokio::test]
async fn depot_refuses_to_start_when_acme_cannot_issue() {
    let dir = TempDir::new().unwrap();
    let state = dir.path().join("acme");
    let ca = Arc::new(TestCa::new());
    let mock = MockAcme::start(ca, Validation::Reject).await;
    let addr = free_addr();
    let http01 = free_addr();
    let config = dir.path().join("depot.kdl");
    std::fs::write(
        &config,
        format!(
            r#"server {{
    bind "{addr}"
}}
repository {{
    root "{repo}"
}}
tls {{
    acme {{
        domains "{HOST}"
        terms-agreed true
        directory-url "{dir}"
        state-dir "{state}"
        challenge "http-01"
        http01-bind "{http01}"
    }}
}}
"#,
            repo = repo(dir.path()).display(),
            dir = mock.directory_url(),
            state = state.display(),
        ),
    )
    .unwrap();

    // Three attempts 10 s apart, then the process must exit non-zero on its
    // own (SMF maintenance), never listening on the HTTPS port meanwhile.
    let mut depot = Depot::start(&config, dir.path());
    let started = Instant::now();
    let mut refreshed = false;
    let status = loop {
        if let Some(status) = depot.0.try_wait().unwrap() {
            break status;
        }
        // svcadm refresh while issuance is retrying must not end startup.
        if !refreshed && mock.counters().orders_created >= 1 {
            depot.hangup();
            refreshed = true;
        }
        assert!(
            std::net::TcpStream::connect(addr).is_err(),
            "HTTPS port opened without a certificate"
        );
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "did not give up"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert!(!status.success());
    assert!(refreshed);
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), None, "SIGHUP terminated startup");
    }
    assert_eq!(
        mock.counters().orders_created,
        3,
        "bounded startup attempts"
    );

    // SMF restarts the instance right away. The failures are on disk, so
    // the new process waits (minutes by default) instead of hitting the CA.
    let backoff: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state.join("backoff.json")).unwrap()).unwrap();
    assert_eq!(backoff["failures"], 3);
    assert!(backoff["not_before"].as_i64().unwrap() >= now() + 200);
    let mut restarted = Depot::start(&config, dir.path());
    tokio::time::sleep(Duration::from_secs(3)).await;
    restarted.assert_running();
    assert!(
        std::net::TcpStream::connect(addr).is_err(),
        "HTTPS port opened without a certificate"
    );
    assert_eq!(
        mock.counters().orders_created,
        3,
        "no CA contact while backing off"
    );
}
