//! pkg5 read requests across an atomic pkg5 import (#38): the depot serves
//! the complete previous generation while an import stages or crashes before
//! its switch, and the complete new generation afterwards, from the same
//! catalog pathnames.

#[path = "../../libips/tests/support/pkg5_fixture.rs"]
mod pkg5_fixture;

use libips::repository::{
    FailPoint, GenerationError, Pkg5ImportError, Pkg5ImportOptions, import_pkg5,
};
use pkg5_fixture::{Pkg, gzip, make_p5p, quote, sha1_hex};
use pkg6depotd::config::{Config, RepositoryConfig, ServerConfig};
use pkg6depotd::http;
use pkg6depotd::repo::DepotRepo;
use reqwest::StatusCode;
use serde_json::Value;
use std::collections::BTreeMap;
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

async fn get(url: String) -> (StatusCode, Vec<u8>) {
    let resp = reqwest::get(url).await.unwrap();
    let status = resp.status();
    (status, resp.bytes().await.unwrap().to_vec())
}

async fn get_json(url: String) -> Value {
    let (status, body) = get(url.clone()).await;
    assert_eq!(status, StatusCode::OK, "{url}");
    serde_json::from_slice(&body).unwrap()
}

/// pkg5 client catalog refresh over HTTP: catalog.attrs, then every part,
/// each validated against the SHA-1 in catalog.attrs. `between` runs after
/// catalog.attrs was fetched.
async fn refresh(
    base: &str,
    publisher: &str,
    between: impl FnOnce(),
) -> Result<BTreeMap<String, Value>, String> {
    let attrs = get_json(format!("{base}/{publisher}/catalog/1/catalog.attrs")).await;
    between();
    let mut parts = BTreeMap::new();
    for (name, info) in attrs["parts"].as_object().unwrap() {
        let part = get_json(format!("{base}/{publisher}/catalog/1/{name}")).await;
        if part["_SIGNATURE"]["sha-1"] != info["signature-sha-1"] {
            return Err(format!("{name} does not match catalog.attrs"));
        }
        parts.insert(name.clone(), part);
    }
    Ok(parts)
}

fn has_version(parts: &BTreeMap<String, Value>, publisher: &str, pkg: &Pkg) -> bool {
    parts["catalog.base.C"][publisher][&pkg.stem]
        .as_array()
        .is_some_and(|vs| vs.iter().any(|v| v["version"] == pkg.version.as_str()))
}

fn manifest_url(base: &str, pkg: &Pkg) -> String {
    format!(
        "{base}/{}/manifest/0/{}",
        pkg.publisher,
        quote(&format!("{}@{}", pkg.stem, pkg.version))
    )
}

fn file_url(base: &str, pkg: &Pkg) -> String {
    format!(
        "{base}/{}/file/1/{}",
        pkg.publisher,
        sha1_hex(&pkg.files[0].1)
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn pkg5_reads_see_old_then_new_generation() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let hello = Pkg::new("alpha", "web/hello");
    let world = Pkg::new("beta", "library/world");
    let run1 = make_p5p(tmp.path(), "run1", &[hello.clone(), world.clone()]);
    import_pkg5(&repo, &run1, &Pkg5ImportOptions::default()).unwrap();
    let base = serve(&repo).await;

    let old = refresh(&base, "alpha", || {}).await.unwrap();
    assert!(has_version(&old, "alpha", &hello));

    // Crash after staging, before the switch: readers keep the old view.
    let fresh = Pkg::new("alpha", "web/fresh");
    let run2 = make_p5p(tmp.path(), "run2", &[fresh.clone()]);
    let crash = Pkg5ImportOptions {
        fail_point: Some(FailPoint::BeforeSwitch),
        ..Default::default()
    };
    assert!(matches!(
        import_pkg5(&repo, &run2, &crash),
        Err(Pkg5ImportError::Generation(
            GenerationError::InjectedFailure(FailPoint::BeforeSwitch)
        ))
    ));
    assert_eq!(refresh(&base, "alpha", || {}).await.unwrap(), old);
    assert_eq!(
        get(manifest_url(&base, &fresh)).await.0,
        StatusCode::NOT_FOUND
    );

    // Retry commits; the new view is complete: catalog, manifest, payload.
    import_pkg5(&repo, &run2, &Pkg5ImportOptions::default()).unwrap();
    let new = refresh(&base, "alpha", || {}).await.unwrap();
    assert!(has_version(&new, "alpha", &fresh) && has_version(&new, "alpha", &hello));
    let (status, body) = get(manifest_url(&base, &fresh)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(String::from_utf8(body).unwrap(), fresh.manifest());
    let (status, body) = get(file_url(&base, &fresh)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, gzip(&fresh.files[0].1));
    assert_eq!(get(manifest_url(&base, &hello)).await.0, StatusCode::OK);
    assert_eq!(get(file_url(&base, &world)).await.0, StatusCode::OK);
    let (status, body) = get(format!("{base}/alpha/search/0/fresh")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8(body).unwrap().contains("web/fresh"));

    // A refresh overlapping the next switch fails pkg5 validation rather than
    // mixing generations, and the retry sees the complete newer catalog.
    let newer = Pkg::new("alpha", "web/newer");
    let run3 = make_p5p(tmp.path(), "run3", &[newer.clone()]);
    let overlapping = refresh(&base, "alpha", || {
        import_pkg5(&repo, &run3, &Pkg5ImportOptions::default()).unwrap();
    })
    .await;
    assert!(overlapping.is_err());
    let latest = refresh(&base, "alpha", || {}).await.unwrap();
    assert!(has_version(&latest, "alpha", &newer) && has_version(&latest, "alpha", &fresh));
}

fn publisher_names(p5i: &Value) -> Vec<String> {
    p5i["publishers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn live_depot_follows_publishers_and_catalog_changes() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let hello = Pkg::new("alpha", "web/hello");
    import_pkg5(
        &repo,
        &make_p5p(tmp.path(), "run1", std::slice::from_ref(&hello)),
        &Pkg5ImportOptions::default(),
    )
    .unwrap();
    let base = serve(&repo).await;
    assert_eq!(
        get(format!("{base}/gamma/publisher/0")).await.0,
        StatusCode::NOT_FOUND
    );
    let client = reqwest::Client::new();
    let attrs_url = format!("{base}/alpha/catalog/1/catalog.attrs");
    let first = client.get(&attrs_url).send().await.unwrap();
    let last_modified = first.headers()["last-modified"].clone();

    // One CI archive adds a package to alpha and a whole new publisher,
    // within the same second as the previous publication.
    let fresh = Pkg::new("alpha", "web/fresh");
    let tool = Pkg::new("gamma", "tools/new");
    import_pkg5(
        &repo,
        &make_p5p(tmp.path(), "run2", &[fresh.clone(), tool.clone()]),
        &Pkg5ImportOptions::default(),
    )
    .unwrap();

    // pkg5 revalidates catalog.attrs with If-Modified-Since: no false 304.
    let revalidated = client
        .get(&attrs_url)
        .header("If-Modified-Since", last_modified)
        .send()
        .await
        .unwrap();
    assert_eq!(revalidated.status(), StatusCode::OK);

    // The new publisher is served everywhere without a depot restart.
    for v in ["0", "1"] {
        let p5i = get_json(format!("{base}/publisher/{v}")).await;
        assert_eq!(
            publisher_names(&p5i),
            vec!["alpha", "gamma"],
            "/publisher/{v}"
        );
        let p5i = get_json(format!("{base}/gamma/publisher/{v}")).await;
        assert_eq!(publisher_names(&p5i), vec!["gamma"]);
    }
    let catalog = refresh(&base, "gamma", || {}).await.unwrap();
    assert!(has_version(&catalog, "gamma", &tool));
    let (status, body) = get(format!("{base}/gamma/search/0/new")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8(body).unwrap().contains("tools/new"));
    assert_eq!(get(file_url(&base, &tool)).await.0, StatusCode::OK);

    // Path segments cannot leave the repository.
    for path in [
        "/..%2F..%2F..%2Fetc/file/1/abcd",
        "/alpha/file/1/..%2F..%2F..%2Fpkg6.repository",
        "/..%2Fgenerations/catalog/1/catalog.attrs",
        "/alpha/catalog/1/..",
        "/alpha/catalog/1/.catalog.attrs.tmp",
        "/..%2F..%2Fpublisher%2Falpha/catalog/2/catalog.attrs",
    ] {
        let (status, _) = get(format!("{base}{path}")).await;
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
            "{path}: {status}"
        );
    }
}

/// Every publisher-discovery answer of a live depot.
#[derive(Debug, PartialEq)]
struct PublisherView {
    publisher_0: Vec<String>,
    publisher_1: Vec<String>,
    /// Publishers answering `/{pub}/publisher/0` and serving catalog.attrs.
    reachable: Vec<String>,
    /// Package names an unscoped search (default publisher) returns.
    unscoped_search: Vec<String>,
}

async fn publisher_view(base: &str, candidates: &[&str], token: &str) -> PublisherView {
    let mut reachable = vec![];
    for p in candidates {
        let discovered = get(format!("{base}/{p}/publisher/0")).await.0 == StatusCode::OK;
        let catalog = get(format!("{base}/{p}/catalog/1/catalog.attrs")).await.0 == StatusCode::OK;
        assert_eq!(discovered, catalog, "{p}: discovery and catalog disagree");
        if discovered {
            reachable.push(p.to_string());
        }
    }
    let (status, body) = get(format!("{base}/search/0/{token}")).await;
    let unscoped_search = if status == StatusCode::OK {
        String::from_utf8(body)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    } else {
        vec![]
    };
    PublisherView {
        publisher_0: publisher_names(&get_json(format!("{base}/publisher/0")).await),
        publisher_1: publisher_names(&get_json(format!("{base}/publisher/1")).await),
        reachable,
        unscoped_search,
    }
}

fn configured_publishers(repo: &Path) -> Vec<String> {
    let config: Value =
        serde_json::from_slice(&std::fs::read(repo.join("pkg6.repository")).unwrap()).unwrap();
    config["publishers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn publisher_discovery_switches_with_the_generation() {
    use libips::repository::{GenerationReason, GenerationStore};
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    // Both publishers carry a "shared" package, so unscoped search shows
    // which publisher is the default.
    let alpha = Pkg::new("alpha", "shared/alpha-tool");
    let beta = Pkg::new("beta", "shared/beta-tool");
    import_pkg5(
        &repo,
        &make_p5p(tmp.path(), "run1", &[alpha.clone(), beta.clone()]),
        &Pkg5ImportOptions::default(),
    )
    .unwrap();
    let base = serve(&repo).await;
    let all = ["alpha", "beta", "gamma"];
    let old = publisher_view(&base, &all, "shared").await;
    assert_eq!(old.publisher_0, vec!["alpha", "beta"]);
    assert_eq!(old.reachable, vec!["alpha", "beta"]);
    assert!(
        old.unscoped_search
            .iter()
            .any(|l| l.contains("shared/alpha-tool"))
    );

    // An import adding publisher gamma, interrupted before its switch:
    // the complete old view.
    let tool = Pkg::new("gamma", "shared/gamma-tool");
    let run2 = make_p5p(tmp.path(), "run2", std::slice::from_ref(&tool));
    let before_switch = Pkg5ImportOptions {
        fail_point: Some(FailPoint::BeforeSwitch),
        ..Default::default()
    };
    assert!(import_pkg5(&repo, &run2, &before_switch).is_err());
    assert_eq!(publisher_view(&base, &all, "shared").await, old);

    // Switched, but the configuration write after it has not happened: the
    // complete new view, not a mix.
    let after_switch = Pkg5ImportOptions {
        fail_point: Some(FailPoint::AfterSwitch),
        ..Default::default()
    };
    assert!(import_pkg5(&repo, &run2, &after_switch).is_err());
    assert_eq!(configured_publishers(&repo), vec!["alpha", "beta"]);
    let added = publisher_view(&base, &all, "shared").await;
    assert_eq!(added.publisher_0, vec!["alpha", "beta", "gamma"]);
    assert_eq!(added.publisher_1, added.publisher_0);
    assert_eq!(added.reachable, added.publisher_0);
    assert_eq!(added.unscoped_search, old.unscoped_search);
    assert!(has_version(
        &refresh(&base, "gamma", || {}).await.unwrap(),
        "gamma",
        &tool
    ));

    // Removing the default publisher, interrupted after its switch: alpha
    // disappears everywhere at once and the default moves to beta, although
    // the configuration still names alpha.
    {
        let mut store = GenerationStore::open(&repo).unwrap();
        let mut staged = store
            .stage(
                GenerationReason::RemovePublisher {
                    publisher: "alpha".to_string(),
                },
                &[],
            )
            .unwrap();
        staged.remove_publisher("alpha").unwrap();
        assert!(matches!(
            store.commit(staged, Some(FailPoint::AfterSwitch)),
            Err(GenerationError::InjectedFailure(FailPoint::AfterSwitch))
        ));
    }
    assert!(configured_publishers(&repo).contains(&"alpha".to_string()));
    let removed = publisher_view(&base, &all, "shared").await;
    assert_eq!(removed.publisher_0, vec!["beta", "gamma"]);
    assert_eq!(removed.publisher_1, removed.publisher_0);
    assert_eq!(removed.reachable, removed.publisher_0);
    assert!(
        removed
            .unscoped_search
            .iter()
            .all(|l| l.contains("shared/beta-tool")),
        "{:?}",
        removed.unscoped_search
    );
    assert!(!removed.unscoped_search.is_empty());
    assert_eq!(
        get(manifest_url(&base, &alpha)).await.0,
        StatusCode::NOT_FOUND
    );

    // Recovery converges the configuration; readers see no change.
    drop(GenerationStore::open(&repo).unwrap());
    assert_eq!(configured_publishers(&repo), vec!["beta", "gamma"]);
    assert_eq!(publisher_view(&base, &all, "shared").await, removed);
}
