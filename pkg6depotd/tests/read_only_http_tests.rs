//! v1 administration boundary: the public HTTP interface is repository-read-only.
//!
//! These tests serve a real repository, snapshot its on-disk state, issue
//! every pkg5 mutating/administrative request shape plus the full set of
//! read routes, and assert that nothing on disk changed.

use libips::actions::{Attr, File as FileAction, Manifest};
use libips::repository::{FileBackend, ReadableRepository, RepositoryVersion, WritableRepository};
use pkg6depotd::config::{Config, RepositoryConfig, ServerConfig};
use pkg6depotd::http;
use pkg6depotd::repo::DepotRepo;
use reqwest::{Method, StatusCode};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tempfile::TempDir;
use tokio::net::TcpListener;

const PUBLISHER: &str = "test";

fn setup_repo(dir: &TempDir) -> PathBuf {
    let repo_path = dir.path().join("repo");
    let mut backend = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();
    backend.add_publisher(PUBLISHER).unwrap();

    let mut tx = backend.begin_transaction().unwrap();
    tx.set_publisher(PUBLISHER);

    let content_dir = dir.path().join("content");
    fs::create_dir_all(&content_dir).unwrap();
    let file_path = content_dir.join("hello.txt");
    fs::write(&file_path, "Hello IPS").unwrap();
    let mut fa = FileAction::read_from_path(&file_path).unwrap();
    fa.path = "hello.txt".to_string();
    tx.add_file(fa, &file_path).unwrap();

    let mut manifest = Manifest::new();
    for (key, value) in [
        ("pkg.fmri", "pkg://test/example@1.0.0"),
        ("pkg.summary", "Test Package"),
    ] {
        manifest.attributes.push(Attr {
            key: key.to_string(),
            values: vec![value.to_string()],
            properties: HashMap::new(),
        });
    }
    tx.update_manifest(manifest);
    tx.commit().unwrap();

    backend.rebuild(Some(PUBLISHER), false, false).unwrap();
    repo_path
}

/// Content and mtime of every entry below `root`, so both new/removed files
/// and in-place rewrites are detected.
type Snapshot = BTreeMap<PathBuf, (Option<Vec<u8>>, SystemTime)>;

fn snapshot(root: &Path) -> Snapshot {
    fn walk(root: &Path, dir: &Path, out: &mut Snapshot) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            if meta.is_dir() {
                out.insert(rel, (None, meta.modified().unwrap()));
                walk(root, &path, out);
            } else {
                out.insert(
                    rel,
                    (Some(fs::read(&path).unwrap()), meta.modified().unwrap()),
                );
            }
        }
    }
    let mut out = Snapshot::new();
    walk(root, root, &mut out);
    out
}

fn assert_unchanged(before: &Snapshot, after: &Snapshot) {
    let changed: Vec<_> = before
        .keys()
        .chain(after.keys())
        .filter(|k| before.get(*k) != after.get(*k))
        .collect();
    assert!(
        changed.is_empty(),
        "public HTTP requests mutated repository state: {:?}",
        changed
    );
}

async fn serve(repo_path: &Path) -> String {
    let config = Config {
        server: ServerConfig {
            bind: vec!["127.0.0.1:0".to_string()],
            workers: None,
            max_connections: None,
            reuseport: None,
            cache_max_age: Some(3600),
            tls_cert: None,
            tls_key: None,
        },
        repository: RepositoryConfig {
            root: repo_path.to_path_buf(),
            mode: Some("readonly".to_string()),
        },
        telemetry: None,
        publishers: None,
        admin: None,
        oauth2: None,
        tls: None,
    };
    let state = Arc::new(DepotRepo::new(&config).unwrap());
    let router = http::routes::app_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        http::server::run(router, listener).await.unwrap();
    });
    format!("http://{}", addr)
}

/// Guards the snapshot itself: a catalog/shard rebuild (what pkg5's
/// `admin/0?cmd=rebuild` requests) must be visible to it, otherwise the
/// read-only assertions below would pass vacuously.
#[test]
fn snapshot_detects_repository_rebuild() {
    let temp_dir = TempDir::new().unwrap();
    let repo_path = setup_repo(&temp_dir);
    let before = snapshot(&repo_path);

    std::thread::sleep(std::time::Duration::from_millis(1100));
    FileBackend::open(&repo_path)
        .unwrap()
        .rebuild(Some(PUBLISHER), false, false)
        .unwrap();

    assert_ne!(before, snapshot(&repo_path));
}

#[tokio::test]
async fn mutating_and_admin_operations_are_not_routed() {
    let temp_dir = TempDir::new().unwrap();
    let repo_path = setup_repo(&temp_dir);
    let base = serve(&repo_path).await;
    let before = snapshot(&repo_path);
    // Make any rewrite observable through mtime even on coarse filesystems.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    // pkg5 depot operations that are absent from REPO_OPS_READONLY
    // (src/modules/server/depot.py), under both publisher-prefixed and
    // default-publisher paths.
    let mut paths = vec![
        "/admin/auth/check".to_string(),
        "/admin/0".to_string(),
        "/index/0/refresh".to_string(),
    ];
    for cmd in [
        "rebuild",
        "rebuild-indexes",
        "rebuild-packages",
        "refresh",
        "refresh-indexes",
        "refresh-packages",
    ] {
        paths.push(format!("/admin/0?cmd={cmd}"));
        paths.push(format!("/{PUBLISHER}/admin/0?cmd={cmd}"));
    }
    paths.push(format!("/{PUBLISHER}/index/0/refresh"));
    for op in ["open", "append", "close", "abandon", "add"] {
        paths.push(format!("/{op}/0/example@2.0.0"));
        paths.push(format!("/{PUBLISHER}/{op}/0/example@2.0.0"));
        paths.push(format!("/{op}/1/example@2.0.0"));
        paths.push(format!("/{PUBLISHER}/{op}/1/example@2.0.0"));
    }
    paths.push("/add/0/transaction/file".to_string());
    paths.push(format!("/{PUBLISHER}/add/0/transaction/file"));

    let client = reqwest::Client::new();
    for path in &paths {
        for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE] {
            let resp = client
                .request(method.clone(), format!("{base}{path}"))
                .header("Authorization", "Bearer any-token")
                .body("")
                .send()
                .await
                .unwrap();
            assert!(
                matches!(
                    resp.status(),
                    StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                ),
                "{method} {path} must not be served publicly, got {}",
                resp.status()
            );
        }
    }

    assert_unchanged(&before, &snapshot(&repo_path));
}

#[tokio::test]
async fn read_routes_leave_repository_untouched() {
    let temp_dir = TempDir::new().unwrap();
    let repo_path = setup_repo(&temp_dir);
    let base = serve(&repo_path).await;
    let before = snapshot(&repo_path);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let client = reqwest::Client::new();
    let get = [
        "/versions/0/".to_string(),
        format!("/{PUBLISHER}/catalog/1/catalog.attrs"),
        format!("/{PUBLISHER}/catalog/2/catalog.attrs"),
        format!("/{PUBLISHER}/manifest/0/example%401.0.0"),
        format!("/{PUBLISHER}/manifest/1/example%401.0.0"),
        format!("/{PUBLISHER}/info/0/example%401.0.0"),
        format!("/{PUBLISHER}/publisher/0"),
        format!("/{PUBLISHER}/publisher/1"),
        "/publisher/0".to_string(),
        "/publisher/1".to_string(),
        format!("/{PUBLISHER}/search/0/example"),
        format!("/{PUBLISHER}/search/1/False_2_None_None_example"),
        "/search/0/example".to_string(),
        "/admin/health".to_string(),
        "/ui/".to_string(),
        format!("/ui/packages/{PUBLISHER}"),
        "/ui/search?q=example".to_string(),
        "/ui/search/results?q=example".to_string(),
        format!("/ui/package/{PUBLISHER}/example@1.0.0"),
        "/ui/p5i".to_string(),
    ];
    for path in &get {
        // Status is not asserted: some routes legitimately 4xx on this
        // minimal repository. Only the absence of side effects matters here.
        client.get(format!("{base}{path}")).send().await.unwrap();
    }
    for path in [format!("/{PUBLISHER}/search/1/"), "/search/1/".to_string()] {
        client
            .post(format!("{base}{path}"))
            .body("False_2_None_None_example")
            .send()
            .await
            .unwrap();
    }

    assert_unchanged(&before, &snapshot(&repo_path));
}
