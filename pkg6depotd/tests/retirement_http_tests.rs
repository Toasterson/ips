//! Retirement over HTTP (#39): a hidden version leaves the pkg5 catalog and
//! search, its exact manifest and payload keep being served, serving them
//! records a qualifying access (which leaves the version hidden), readers
//! switch from the old to the new catalog without a restart, a running
//! depot confirms apply's access fence, and a depot that cannot record
//! keeps serving while retirement fails closed.

#[path = "../../libips/tests/support/pkg5_fixture.rs"]
mod pkg5_fixture;

use chrono::Utc;
use libips::repository::retention::{
    AccessFence, AccessLog, ApplyOptions, FmriRule, PlanRequest, Policy, apply_retirement,
    plan_retirement, retirement_status,
};
use libips::repository::{FailPoint, Pkg5ImportOptions, import_pkg5};
use pkg5_fixture::{Pkg, make_p5p, quote, sha1_hex};
use pkg6depotd::config::{Config, RepositoryConfig, ServerConfig};
use pkg6depotd::http;
use pkg6depotd::repo::DepotRepo;
use reqwest::StatusCode;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::net::TcpListener;

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
    state.start_access_heartbeat().unwrap();
    let router = http::routes::app_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        http::server::run(router, listener).await.unwrap();
    });
    format!("http://{}", addr)
}

async fn get(url: String) -> (StatusCode, Vec<u8>) {
    let resp = reqwest::get(url).await.unwrap();
    let status = resp.status();
    (status, resp.bytes().await.unwrap().to_vec())
}

async fn catalog_versions(base: &str, stem: &str) -> Vec<String> {
    let (status, body) = get(format!("{base}/oi/catalog/1/catalog.base.C")).await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).unwrap();
    json["oi"][stem]
        .as_array()
        .map(|vs| {
            vs.iter()
                .map(|v| v["version"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn pkg(version: &str) -> Pkg {
    Pkg::new("oi", "library/foo").version(version).file(
        &format!("usr/share/foo-{}", version),
        format!("foo {}", version).as_bytes(),
    )
}

fn manifest_url(base: &str, p: &Pkg) -> String {
    format!(
        "{base}/oi/manifest/0/{}",
        quote(&format!("{}@{}", p.stem, p.version))
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn hidden_versions_leave_catalogs_but_exact_fetches_work_and_count() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let old = pkg("1.0:20200101T000000Z");
    let new = pkg("2.0:20210101T000000Z");
    let archive = make_p5p(tmp.path(), "run", &[old.clone(), new.clone()]);
    import_pkg5(&repo, &archive, &Pkg5ImportOptions::default()).unwrap();
    let base = serve(&repo).await;
    // Read from disk: a GET here would already count as today's access.
    let old_manifest = std::fs::read(libips::repository::FileBackend::construct_manifest_path(
        &repo,
        "oi",
        &old.stem,
        &old.version,
    ))
    .unwrap();

    // Catalog, search and HEAD requests are not qualifying accesses.
    let log = AccessLog::load(&repo).unwrap();
    assert!(log.manifest_access(&new.fmri()).is_none());
    let _ = get(format!("{base}/oi/search/0/foo")).await;
    let _ = reqwest::Client::new()
        .head(manifest_url(&base, &new))
        .send()
        .await
        .unwrap();
    assert!(
        AccessLog::load(&repo)
            .unwrap()
            .manifest_access(&new.fmri())
            .is_none()
    );

    let policy = Policy::default();
    let plan = plan_retirement(
        &repo,
        &policy,
        &PlanRequest {
            use_policy: false,
            select: vec![FmriRule::exact(&old.fmri()).unwrap()],
        },
    )
    .unwrap();

    // A crash before the switch: readers keep the complete old catalog.
    let crash = ApplyOptions {
        fail_point: Some(FailPoint::BeforeSwitch),
        ..Default::default()
    };
    assert!(apply_retirement(&repo, &plan, &policy, &crash).is_err());
    assert_eq!(catalog_versions(&base, "library/foo").await.len(), 2);

    apply_retirement(&repo, &plan, &policy, &ApplyOptions::default()).unwrap();
    // The running depot follows the switch.
    assert_eq!(
        catalog_versions(&base, "library/foo").await,
        vec![new.version.clone()]
    );
    let (status, body) = get(format!("{base}/oi/search/0/foo")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!String::from_utf8_lossy(&body).contains(&old.version));

    // Exact manifest and payload of the hidden version are still served.
    let before = Utc::now().timestamp();
    let (status, body) = get(manifest_url(&base, &old)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, old_manifest);
    let hash = sha1_hex(&old.files[0].1);
    let (status, _) = get(format!("{base}/oi/file/1/{hash}")).await;
    assert_eq!(status, StatusCode::OK);

    // Both requests were recorded; the version stays hidden.
    let log = AccessLog::load(&repo).unwrap();
    assert!(log.manifest_access(&old.fmri()).unwrap().timestamp() >= before);
    assert!(log.payloads.contains_key(&("oi".to_string(), hash)));
    let status = retirement_status(&repo, &policy).unwrap();
    assert_eq!(status.hidden[0].fmri, old.fmri());
    assert!(status.hidden[0].last_access.is_some());
    assert_eq!(
        catalog_versions(&base, "library/foo").await,
        vec![new.version.clone()]
    );
}

/// Real pkg5 versions (`release,branch-build:timestamp`): the key a manifest
/// GET records is the key retirement looks the version up by.
#[tokio::test(flavor = "multi_thread")]
async fn pkg5_versions_are_recorded_under_the_key_retirement_reads() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let old = pkg("1.0,5.11-2020.0.1.0:20200101T000000Z");
    let new = pkg("2.0,5.11-2021.0.1.0:20210101T000000Z");
    let archive = make_p5p(tmp.path(), "run", &[old.clone(), new.clone()]);
    import_pkg5(&repo, &archive, &Pkg5ImportOptions::default()).unwrap();
    let base = serve(&repo).await;
    let policy = Policy::default();
    let plan = plan_retirement(
        &repo,
        &policy,
        &PlanRequest {
            use_policy: false,
            select: vec![FmriRule::exact(&old.fmri()).unwrap()],
        },
    )
    .unwrap();
    apply_retirement(&repo, &plan, &policy, &ApplyOptions::default()).unwrap();
    assert_eq!(
        catalog_versions(&base, "library/foo").await,
        vec![new.version.clone()]
    );

    let (status, _) = get(manifest_url(&base, &old)).await;
    assert_eq!(status, StatusCode::OK);
    let log = AccessLog::load(&repo).unwrap();
    assert!(
        log.manifest_access(&old.fmri()).is_some(),
        "{:?}",
        log.manifests.keys().collect::<Vec<_>>()
    );
    let status = retirement_status(&repo, &policy).unwrap();
    assert_eq!(status.hidden[0].fmri, old.fmri());
    assert!(status.hidden[0].last_access.is_some());
}

/// Content and mtime of every entry below `root` except the access records.
fn snapshot(
    root: &Path,
) -> std::collections::BTreeMap<std::path::PathBuf, (Option<Vec<u8>>, std::time::SystemTime)> {
    let mut out = std::collections::BTreeMap::new();
    for entry in walkdir(root) {
        let rel = entry.strip_prefix(root).unwrap().to_path_buf();
        if rel.starts_with("retention") {
            continue;
        }
        let meta = std::fs::symlink_metadata(&entry).unwrap();
        let content = meta.is_file().then(|| std::fs::read(&entry).unwrap());
        out.insert(rel, (content, meta.modified().unwrap()));
    }
    out
}

fn walkdir(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let meta = std::fs::symlink_metadata(&p).unwrap();
        out.push(p.clone());
        if meta.is_dir() {
            out.extend(walkdir(&p));
        }
    }
    out
}

/// ADR-004's read-only boundary on a managed repository: serving reads
/// writes access records under `retention/access/` and nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn reads_only_append_access_records() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let p = pkg("1.0:20200101T000000Z");
    let archive = make_p5p(tmp.path(), "run", std::slice::from_ref(&p));
    import_pkg5(&repo, &archive, &Pkg5ImportOptions::default()).unwrap();
    let base = serve(&repo).await;
    let before = snapshot(&repo);
    for url in [
        manifest_url(&base, &p),
        format!("{base}/oi/file/1/{}", sha1_hex(&p.files[0].1)),
        format!("{base}/oi/catalog/1/catalog.attrs"),
        format!("{base}/oi/search/0/foo"),
        format!(
            "{base}/oi/info/0/{}",
            quote(&format!("{}@{}", p.stem, p.version))
        ),
    ] {
        assert_eq!(get(url.clone()).await.0, StatusCode::OK, "{url}");
    }
    assert_eq!(snapshot(&repo), before);
    let log = AccessLog::load(&repo).unwrap();
    assert!(log.tracking_since.is_some());
    assert!(log.manifest_access(&p.fmri()).is_some());
}

/// The depot's heartbeat answers apply's fence, which proves it has not
/// lost an access record.
#[tokio::test(flavor = "multi_thread")]
async fn a_serving_depot_confirms_the_access_fence() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let p = pkg("1.0:20200101T000000Z");
    let archive = make_p5p(tmp.path(), "run", std::slice::from_ref(&p));
    import_pkg5(&repo, &archive, &Pkg5ImportOptions::default()).unwrap();
    let base = serve(&repo).await;
    assert_eq!(get(manifest_url(&base, &p)).await.0, StatusCode::OK);
    assert_eq!(AccessLog::load(&repo).unwrap().open_runs().len(), 1);
    let repo2 = repo.clone();
    let fence = tokio::task::spawn_blocking(move || {
        AccessFence::acquire(&repo2, std::time::Duration::from_secs(20))
    })
    .await
    .unwrap();
    assert!(fence.is_ok(), "{:?}", fence.err());
    // Reads are served while the fence is held.
    assert_eq!(
        get(format!("{base}/oi/file/1/{}", sha1_hex(&p.files[0].1)))
            .await
            .0,
        StatusCode::OK
    );
    drop(fence);
}

/// A depot that cannot write access records keeps serving reads; retirement
/// for inactivity does not treat the missing records as disuse.
#[tokio::test(flavor = "multi_thread")]
async fn failed_access_recording_keeps_reads_and_fails_closed() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let old = pkg("1.0:20200101T000000Z");
    let new = pkg("2.0:20210101T000000Z");
    let archive = make_p5p(tmp.path(), "run", &[old.clone(), new]);
    import_pkg5(&repo, &archive, &Pkg5ImportOptions::default()).unwrap();
    // The access directory cannot be created (even for root).
    std::fs::create_dir_all(repo.join("retention")).unwrap();
    std::fs::write(repo.join("retention/access"), b"not a directory").unwrap();
    let base = serve(&repo).await;
    for url in [
        manifest_url(&base, &old),
        format!("{base}/oi/file/1/{}", sha1_hex(&old.files[0].1)),
        format!("{base}/oi/catalog/1/catalog.attrs"),
    ] {
        assert_eq!(get(url.clone()).await.0, StatusCode::OK, "{url}");
    }
    std::fs::write(
        repo.join("policy.kdl"),
        "retirement {\n    enabled true\n    inactivity \"1 day\"\n}\n",
    )
    .unwrap();
    let policy = Policy::load(Some(&repo), None, &[]).unwrap();
    std::fs::remove_file(repo.join("retention/access")).unwrap();
    let plan = plan_retirement(
        &repo,
        &policy,
        &PlanRequest {
            use_policy: true,
            select: vec![],
        },
    )
    .unwrap();
    assert!(plan.hide.is_empty());
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("no qualifying access history")),
        "{:?}",
        plan.warnings
    );
}
