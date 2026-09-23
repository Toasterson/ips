//! Access recording across the real `pkg6depotd start` lifecycle on a
//! generation-managed repository: SIGTERM ends the run cleanly over HTTP
//! and HTTPS (no gap after restart), a stop whose drain times out leaves
//! the run open (a gap after restart), and a stop while ACME issuance backs
//! off starts no run at all.

#[path = "../../libips/tests/support/pkg5_fixture.rs"]
mod pkg5_fixture;
mod support;

use libips::repository::retention::AccessLog;
use libips::repository::{Pkg5ImportOptions, import_pkg5};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use pkg5_fixture::{Pkg, make_p5p, quote, sha1_hex};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
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
            .args(["--config", config.to_str().unwrap(), "start"])
            .current_dir(cwd)
            .env("RUST_LOG", "pkg6depotd=info,libips=info")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Depot(child)
    }

    /// SIGTERM (SMF stop) and wait for the process to exit.
    fn terminate(&mut self, limit: Duration) -> ExitStatus {
        kill(Pid::from_raw(self.0.id() as i32), Signal::SIGTERM).unwrap();
        let start = Instant::now();
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(start.elapsed() < limit, "depot did not stop in {limit:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn free_addr() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap()
}

/// Deterministic incompressible bytes, so a payload outgrows socket buffers.
fn noise(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// A generation-managed repository (as `import-pkg5` makes it), so the
/// depot records access.
fn managed_repo(dir: &Path, big: bool) -> (PathBuf, Pkg) {
    let content = if big {
        noise(32 << 20)
    } else {
        b"foo 1.0".to_vec()
    };
    let pkg = Pkg::new("oi", "library/foo")
        .version("1.0:20200101T000000Z")
        .file("usr/share/foo", &content);
    let archive = make_p5p(dir, "run", std::slice::from_ref(&pkg));
    let repo = dir.join("repo");
    import_pkg5(&repo, &archive, &Pkg5ImportOptions::default()).unwrap();
    (repo, pkg)
}

fn manifest_path(p: &Pkg) -> String {
    format!(
        "/oi/manifest/0/{}",
        quote(&format!("{}@{}", p.stem, p.version))
    )
}

fn write_config(dir: &Path, addr: SocketAddr, repo: &Path, tls: &str) -> PathBuf {
    let config = dir.join("depot.kdl");
    std::fs::write(
        &config,
        format!(
            "server {{\n bind \"{addr}\"\n}}\nrepository {{\n root \"{}\"\n}}\n{tls}",
            repo.display()
        ),
    )
    .unwrap();
    config
}

async fn wait_ok(client: &reqwest::Client, url: &str) {
    let start = Instant::now();
    loop {
        match client.get(url).send().await {
            Ok(r) if r.status().is_success() => return,
            Ok(r) => panic!("{url}: HTTP {}", r.status()),
            Err(e) => assert!(
                start.elapsed() < Duration::from_secs(20),
                "{url} not served: {e}"
            ),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Serve the manifest once, SIGTERM, and check the run ended cleanly: a
/// restart finds no gap.
async fn clean_stop(
    dir: &Path,
    config: &Path,
    repo: &Path,
    pkg: &Pkg,
    client: reqwest::Client,
    base: &str,
) {
    let mut depot = Depot::start(config, dir);
    wait_ok(&client, &format!("{base}/versions/0/")).await;
    wait_ok(&client, &format!("{base}{}", manifest_path(pkg))).await;
    let log = AccessLog::load(repo).unwrap();
    assert!(log.manifest_access(&pkg.fmri()).is_some());
    assert_eq!(log.open_runs().len(), 1);

    let status = depot.terminate(Duration::from_secs(20));
    assert!(status.success(), "{status:?}");
    let log = AccessLog::load(repo).unwrap();
    assert!(
        log.open_runs().is_empty(),
        "no e line: {:?}",
        log.open_runs()
    );

    // A restart after a clean stop does not restart retirement clocks.
    let mut depot = Depot::start(config, dir);
    wait_ok(&client, &format!("{base}/versions/0/")).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(depot.terminate(Duration::from_secs(20)).success());
    let log = AccessLog::load(repo).unwrap();
    assert!(log.open_runs().is_empty());
    assert!(log.gaps().is_empty(), "{:?}", log.gaps());
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_ends_the_access_run_cleanly_over_http() {
    let dir = TempDir::new().unwrap();
    let (repo, pkg) = managed_repo(dir.path(), false);
    let addr = free_addr();
    let config = write_config(dir.path(), addr, &repo, "");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    clean_stop(
        dir.path(),
        &config,
        &repo,
        &pkg,
        client,
        &format!("http://{addr}"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_ends_the_access_run_cleanly_over_https() {
    let dir = TempDir::new().unwrap();
    let (repo, pkg) = managed_repo(dir.path(), false);
    let ca = TestCa::new();
    let (chain, key) = ca.issue_pem(&[HOST.to_string()], now() + 86_400, 41);
    let cert_path = dir.path().join("fullchain.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, &chain).unwrap();
    std::fs::write(&key_path, &key).unwrap();
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let addr = free_addr();
    let tls = format!(
        "tls {{\n certificate \"{}\"\n key \"{}\"\n}}\n",
        cert_path.display(),
        key_path.display()
    );
    let config = write_config(dir.path(), addr, &repo, &tls);
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_der(&ca.cert_der).unwrap())
        .resolve(HOST, addr)
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let base = format!("https://{HOST}:{}", addr.port());
    clean_stop(dir.path(), &config, &repo, &pkg, client, &base).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_drain_timeout_leaves_the_access_run_open() {
    let dir = TempDir::new().unwrap();
    let (repo, pkg) = managed_repo(dir.path(), true);
    let addr = free_addr();
    let config = write_config(dir.path(), addr, &repo, "");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let base = format!("http://{addr}");

    let mut depot = Depot::start(&config, dir.path());
    wait_ok(&client, &format!("{base}/versions/0/")).await;

    // A client that stops reading a large payload keeps its request
    // running past the drain timeout.
    let hash = sha1_hex(&pkg.files[0].1);
    let mut slow = TcpStream::connect(addr).unwrap();
    write!(
        slow,
        "GET /oi/file/1/{hash} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut head = [0u8; 64];
    slow.read_exact(&mut head).unwrap();
    assert!(
        head.starts_with(b"HTTP/1.1 200"),
        "{:?}",
        String::from_utf8_lossy(&head)
    );
    let log = AccessLog::load(&repo).unwrap();
    assert_eq!(log.open_runs().len(), 1);

    let stopping = Instant::now();
    let status = depot.terminate(Duration::from_secs(40));
    assert!(status.success(), "{status:?}");
    assert!(
        stopping.elapsed() >= Duration::from_secs(9),
        "stopped after {:?} although a request was running",
        stopping.elapsed()
    );
    drop(slow);

    // The depot cannot vouch for what it served after giving up: no e line.
    let log = AccessLog::load(&repo).unwrap();
    assert_eq!(log.open_runs().len(), 1, "run was closed");

    // After a restart that stop is a gap in the access history.
    let mut depot = Depot::start(&config, dir.path());
    wait_ok(&client, &format!("{base}/versions/0/")).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(depot.terminate(Duration::from_secs(20)).success());
    let log = AccessLog::load(&repo).unwrap();
    assert!(log.open_runs().is_empty());
    assert_eq!(log.gaps().len(), 1, "{:?}", log.gaps());
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_during_acme_backoff_starts_no_access_run() {
    let dir = TempDir::new().unwrap();
    let (repo, _) = managed_repo(dir.path(), false);
    let ca = Arc::new(TestCa::new());
    let mock = MockAcme::start(ca, Validation::Reject).await;
    // Earlier failures on disk: startup waits for the CA instead of serving.
    let state = dir.path().join("acme");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        state.join("backoff.json"),
        serde_json::json!({
            "directory": mock.directory_url(),
            "domains": [HOST],
            "failures": 3,
            "not_before": now() + 3600,
        })
        .to_string(),
    )
    .unwrap();
    let addr = free_addr();
    let tls = format!(
        r#"tls {{
    acme {{
        domains "{HOST}"
        terms-agreed true
        directory-url "{}"
        state-dir "{}"
        challenge "http-01"
        http01-bind "{}"
    }}
}}
"#,
        mock.directory_url(),
        state.display(),
        free_addr()
    );
    let config = write_config(dir.path(), addr, &repo, &tls);

    let mut depot = Depot::start(&config, dir.path());
    // Several heartbeat ticks while backing off.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(depot.0.try_wait().unwrap().is_none(), "depot exited");
    assert!(
        TcpStream::connect(addr).is_err(),
        "port opened without a certificate"
    );
    assert_eq!(mock.counters().orders_created, 0);

    // SMF stop: SIGTERM is not handled before serving, so it kills the
    // process, which must not leave a run behind to become a gap.
    let status = depot.terminate(Duration::from_secs(10));
    assert_eq!(status.signal(), Some(Signal::SIGTERM as i32));
    let log = AccessLog::load(&repo).unwrap();
    assert!(log.open_runs().is_empty(), "{:?}", log.open_runs());
    assert!(log.gaps().is_empty(), "{:?}", log.gaps());
}
