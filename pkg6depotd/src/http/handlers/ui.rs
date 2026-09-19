use crate::errors::DepotError;
use crate::http::handlers::info::{compute_sizes, find_attr, format_packaging_date, human_bytes};
use crate::repo::DepotRepo;
use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use libips::actions::Manifest;
use libips::fmri::Fmri;
use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helper: render an Askama template into an HTML response
// ---------------------------------------------------------------------------
fn render_template(tmpl: &impl Template) -> Result<Response, DepotError> {
    let html = tmpl
        .render()
        .map_err(|e| DepotError::Server(e.to_string()))?;
    Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response())
}

// ---------------------------------------------------------------------------
// Helper: URL-encode an FMRI for use in URL path segments
// ---------------------------------------------------------------------------
fn encode_fmri(fmri_str: &str) -> String {
    // Encode characters that are problematic in URLs but preserve / for readability
    let mut out = String::with_capacity(fmri_str.len());
    for ch in fmri_str.chars() {
        match ch {
            '@' => out.push_str("%40"),
            ',' => out.push_str("%2C"),
            ':' => out.push_str("%3A"),
            _ => out.push(ch),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Helper: parse a manifest from text (JSON or legacy)
// ---------------------------------------------------------------------------
fn parse_manifest(content: &str) -> Result<Manifest, DepotError> {
    match serde_json::from_str::<Manifest>(content) {
        Ok(m) => Ok(m),
        Err(_) => Manifest::parse_string(content.to_string()).map_err(|e| {
            DepotError::Repo(libips::repository::RepositoryError::Other(e.to_string()))
        }),
    }
}

// ---------------------------------------------------------------------------
// Helper: parse version components from an FMRI version string
// ---------------------------------------------------------------------------
struct VersionParts {
    version: String,
    build_release: Option<String>,
    branch: Option<String>,
    timestamp: Option<String>,
}

fn parse_version_parts(version_full: &str) -> VersionParts {
    let mut version = version_full.to_string();
    let mut build_release = None;
    let mut branch = None;
    let mut timestamp = None;

    if let Some((core, rest)) = version_full.split_once(',') {
        version = core.to_string();
        if let Some((rel_branch, ts)) = rest.split_once(':') {
            timestamp = Some(ts.to_string());
            if let Some((rel, br)) = rel_branch.split_once('-') {
                if !rel.is_empty() {
                    build_release = Some(rel.to_string());
                }
                if !br.is_empty() {
                    branch = Some(br.to_string());
                }
            } else if !rel_branch.is_empty() {
                build_release = Some(rel_branch.to_string());
            }
        } else if let Some((rel, br)) = rest.split_once('-') {
            if !rel.is_empty() {
                build_release = Some(rel.to_string());
            }
            if !br.is_empty() {
                branch = Some(br.to_string());
            }
        } else if !rest.is_empty() {
            build_release = Some(rest.to_string());
        }
    }

    VersionParts {
        version,
        build_release,
        branch,
        timestamp,
    }
}

// ---------------------------------------------------------------------------
// Helper: read last-modified from catalog.attrs for a publisher
// ---------------------------------------------------------------------------
fn read_catalog_last_modified(repo: &DepotRepo, publisher: &str) -> Option<String> {
    let attrs_path = repo.shard_dir(publisher).join("catalog.attrs");
    let content = std::fs::read_to_string(&attrs_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let last_modified = json.get("last-modified")?.as_str()?;
    format_packaging_date(last_modified)
}

// ===========================================================================
// Templates
// ===========================================================================

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    publishers: Vec<PublisherDisplay>,
}

struct PublisherDisplay {
    name: String,
    package_count: usize,
    updated: String,
}

#[derive(Template)]
#[template(path = "packages.html")]
struct PackagesTemplate {
    publisher: String,
    packages: Vec<PackageRow>,
    page: usize,
    per_page: usize,
    total_pages: usize,
    total_packages: usize,
}

struct PackageRow {
    name: String,
    version: String,
    fmri_str: String,
    fmri_encoded: String,
}

#[derive(Template)]
#[template(path = "search.html")]
struct SearchTemplate {
    publishers: Vec<String>,
}

#[derive(Template)]
#[template(path = "partials/search_results.html")]
struct SearchResultsTemplate {
    results: Vec<SearchResultRow>,
}

struct SearchResultRow {
    name: String,
    publisher: String,
    fmri: String,
    fmri_encoded: String,
}

#[derive(Template)]
#[template(path = "package_detail.html")]
struct PackageDetailTemplate {
    publisher: String,
    name: String,
    summary: Option<String>,
    version: String,
    build_release: Option<String>,
    branch: Option<String>,
    packaging_date: Option<String>,
    size: String,
    csize: String,
    fmri_str: String,
    fmri_encoded: String,
    dependencies: Vec<DepDisplay>,
}

struct DepDisplay {
    dep_type: String,
    fmri: String,
    link: Option<String>,
}

#[derive(Template)]
#[template(path = "partials/manifest.html")]
struct ManifestTemplate {
    manifest_text: String,
}

// ===========================================================================
// Query parameter structs
// ===========================================================================

#[derive(serde::Deserialize)]
pub struct PaginationParams {
    page: Option<usize>,
    per_page: Option<usize>,
}

#[derive(serde::Deserialize)]
pub struct SearchParams {
    q: Option<String>,
    publisher: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct P5iParams {
    publisher: String,
    #[serde(default)]
    pkg: Vec<String>,
}

// ===========================================================================
// Handlers
// ===========================================================================

/// GET /ui/ - Home page: publisher list with package counts
pub async fn ui_index(State(repo): State<Arc<DepotRepo>>) -> Result<Response, DepotError> {
    let info = repo.get_info()?;
    let publishers = info
        .publishers
        .into_iter()
        .map(|p| {
            // If backend reports 0 packages, try counting via list_packages
            let package_count = if p.package_count == 0 {
                repo.list_packages(Some(&p.name), None)
                    .map(|pkgs| pkgs.len())
                    .unwrap_or(0)
            } else {
                p.package_count
            };
            // Try to get last-modified from catalog.attrs (more accurate than get_info)
            let updated = read_catalog_last_modified(&repo, &p.name)
                .or_else(|| format_packaging_date(&p.updated))
                .unwrap_or(p.updated);
            PublisherDisplay {
                name: p.name,
                package_count,
                updated,
            }
        })
        .collect();
    render_template(&IndexTemplate { publishers })
}

/// GET /ui/packages/:publisher - Paginated package list
pub async fn ui_packages(
    State(repo): State<Arc<DepotRepo>>,
    Path(publisher): Path<String>,
    Query(params): Query<PaginationParams>,
) -> Result<Response, DepotError> {
    let page = params.page.unwrap_or(1).max(1);
    let per_page = params.per_page.unwrap_or(50).clamp(10, 200);

    let mut pkg_list = repo.list_packages(Some(&publisher), None)?;
    pkg_list.sort_by(|a, b| a.fmri.name.cmp(&b.fmri.name));

    let total_packages = pkg_list.len();
    let total_pages = (total_packages + per_page - 1) / per_page.max(1);

    let start = (page - 1) * per_page;
    let packages: Vec<PackageRow> = pkg_list
        .into_iter()
        .skip(start)
        .take(per_page)
        .map(|pi| {
            let name = pi.fmri.name.clone();
            let version = pi.fmri.version();
            let fmri_str = if version.is_empty() {
                format!("pkg://{}/{}", publisher, name)
            } else {
                format!("pkg://{}/{}@{}", publisher, name, version)
            };
            // URL path uses just name@version (publisher is a separate path segment)
            let path_fmri = if version.is_empty() {
                name.clone()
            } else {
                format!("{}@{}", name, version)
            };
            let fmri_encoded = encode_fmri(&path_fmri);
            PackageRow {
                name,
                version,
                fmri_str,
                fmri_encoded,
            }
        })
        .collect();

    render_template(&PackagesTemplate {
        publisher,
        packages,
        page,
        per_page,
        total_pages,
        total_packages,
    })
}

/// GET /ui/search - Search page (full page)
pub async fn ui_search(State(repo): State<Arc<DepotRepo>>) -> Result<Response, DepotError> {
    let info = repo.get_info()?;
    let publishers = info.publishers.into_iter().map(|p| p.name).collect();
    render_template(&SearchTemplate { publishers })
}

/// GET /ui/search/results - HTMX partial: search result rows
pub async fn ui_search_results(
    State(repo): State<Arc<DepotRepo>>,
    Query(params): Query<SearchParams>,
) -> Result<Response, DepotError> {
    let query = match params.q {
        Some(ref q) if !q.trim().is_empty() => q.trim().to_string(),
        _ => {
            return render_template(&SearchResultsTemplate {
                results: Vec::new(),
            });
        }
    };

    let publisher = params.publisher.as_deref().filter(|p| !p.is_empty());

    let entries = repo.search(publisher, &query, false)?;

    // Deduplicate by FMRI
    let mut seen = BTreeSet::new();
    let mut results = Vec::new();
    for entry in entries {
        if seen.insert(entry.fmri.clone()) {
            // Extract publisher, name, and version from the FMRI string
            let (pub_name, pkg_name, path_fmri) = if let Ok(fmri) = Fmri::from_str(&entry.fmri) {
                let pub_name = fmri.publisher.clone().unwrap_or_default();
                let name = fmri.name.clone();
                let version = fmri.version();
                let path = if version.is_empty() {
                    name.clone()
                } else {
                    format!("{}@{}", name, version)
                };
                (pub_name, name, path)
            } else {
                (String::new(), entry.fmri.clone(), entry.fmri.clone())
            };
            let fmri_encoded = encode_fmri(&path_fmri);
            results.push(SearchResultRow {
                name: pkg_name,
                publisher: pub_name,
                fmri: entry.fmri,
                fmri_encoded,
            });
        }
    }

    render_template(&SearchResultsTemplate { results })
}

/// GET /ui/package/:publisher/*fmri - Package detail or manifest partial
pub async fn ui_package_detail(
    State(repo): State<Arc<DepotRepo>>,
    Path((publisher, fmri_str)): Path<(String, String)>,
) -> Result<Response, DepotError> {
    // If the path ends with /manifest, serve the manifest partial instead
    if fmri_str.ends_with("/manifest") {
        return ui_package_manifest_inner(&repo, &publisher, &fmri_str).await;
    }

    let decoded_fmri = urlencoding::decode(&fmri_str)
        .map(|c| c.into_owned())
        .unwrap_or(fmri_str);
    let fmri = Fmri::from_str(&decoded_fmri)
        .map_err(|e| DepotError::Repo(libips::repository::RepositoryError::Other(e.to_string())))?;

    // If get_manifest_text fails (e.g. name-only FMRI from a dependency link),
    // try to find the latest version of this package and redirect
    let content = match repo.get_manifest_text(&publisher, &fmri) {
        Ok(c) => c,
        Err(_) if fmri.version().is_empty() || !fmri.version().contains(':') => {
            // Likely a partial/missing version — find latest from package list
            let pkg_name = fmri.stem();
            if let Ok(packages) = repo.list_packages(Some(&publisher), None) {
                if let Some(latest) = packages
                    .into_iter()
                    .filter(|p| p.fmri.name == pkg_name)
                    .max_by(|a, b| a.fmri.version().cmp(&b.fmri.version()))
                {
                    let version = latest.fmri.version();
                    let path = format!("{}@{}", pkg_name, version);
                    let redirect_url = format!("/ui/package/{}/{}", publisher, encode_fmri(&path));
                    return Ok(axum::response::Redirect::to(&redirect_url).into_response());
                }
            }
            return Err(DepotError::Repo(
                libips::repository::RepositoryError::NotFound(format!(
                    "Package {} not found in {}",
                    pkg_name, publisher
                )),
            ));
        }
        Err(e) => return Err(e),
    };
    let manifest = parse_manifest(&content)?;

    let name = fmri.stem().to_string();
    let version_full = fmri.version();
    let vp = parse_version_parts(&version_full);

    let packaging_date = vp.timestamp.as_deref().and_then(format_packaging_date);

    let (total_size, total_csize) = compute_sizes(&manifest);

    let fmri_display = if version_full.is_empty() {
        format!("pkg://{}/{}", publisher, name)
    } else {
        format!("pkg://{}/{}@{}", publisher, name, version_full)
    };
    // URL path uses just name@version (publisher is a separate path segment)
    let path_fmri = if version_full.is_empty() {
        name.clone()
    } else {
        format!("{}@{}", name, version_full)
    };
    let fmri_encoded = encode_fmri(&path_fmri);

    let summary = find_attr(&manifest, "pkg.summary");

    let dependencies: Vec<DepDisplay> = manifest
        .dependencies
        .iter()
        .filter_map(|dep| {
            let dep_fmri = dep.fmri.as_ref()?;
            let dep_fmri_str = format!("{}", dep_fmri);
            // Try to link to detail page if same publisher
            let link = if dep_fmri.publisher.is_none()
                || dep_fmri.publisher.as_deref() == Some(&publisher)
            {
                // Link to package name only — dependency versions are constraints,
                // not exact versions, so they won't match a specific manifest
                let dep_path = dep_fmri.stem().to_string();
                Some(format!(
                    "/ui/package/{}/{}",
                    publisher,
                    encode_fmri(&dep_path)
                ))
            } else {
                None
            };
            Some(DepDisplay {
                dep_type: dep.dependency_type.clone(),
                fmri: dep_fmri_str,
                link,
            })
        })
        .collect();

    render_template(&PackageDetailTemplate {
        publisher,
        name,
        summary,
        version: vp.version,
        build_release: vp.build_release,
        branch: vp.branch,
        packaging_date,
        size: human_bytes(total_size),
        csize: human_bytes(total_csize),
        fmri_str: fmri_display,
        fmri_encoded,
        dependencies,
    })
}

/// Serve manifest partial for HTMX lazy loading
async fn ui_package_manifest_inner(
    repo: &DepotRepo,
    publisher: &str,
    fmri_str: &str,
) -> Result<Response, DepotError> {
    // Strip trailing "/manifest" from the catch-all path
    let fmri_path = fmri_str
        .strip_suffix("/manifest")
        .unwrap_or(fmri_str)
        .to_string();
    let decoded = urlencoding::decode(&fmri_path)
        .map(|c| c.into_owned())
        .unwrap_or(fmri_path);
    let fmri = Fmri::from_str(&decoded)
        .map_err(|e| DepotError::Repo(libips::repository::RepositoryError::Other(e.to_string())))?;

    let manifest_text = repo.get_manifest_text(publisher, &fmri)?;
    render_template(&ManifestTemplate { manifest_text })
}

/// GET /ui/p5i - Generate P5I file from query params
pub async fn ui_p5i_generate(
    headers: HeaderMap,
    Query(params): Query<P5iParams>,
) -> Result<Response, DepotError> {
    if params.pkg.is_empty() {
        return Err(DepotError::Server("No packages selected".to_string()));
    }

    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let origin = format!("http://{}", host);

    #[derive(serde::Serialize)]
    struct P5iPublisherInfo {
        alias: Option<String>,
        name: String,
        packages: Vec<String>,
        repositories: Vec<String>,
    }

    #[derive(serde::Serialize)]
    struct P5iFile {
        packages: Vec<String>,
        publishers: Vec<P5iPublisherInfo>,
        version: u32,
    }

    let p5i = P5iFile {
        packages: Vec::new(),
        publishers: vec![P5iPublisherInfo {
            alias: None,
            name: params.publisher,
            packages: params.pkg,
            repositories: vec![origin],
        }],
        version: 1,
    };

    let json = serde_json::to_string_pretty(&p5i).map_err(|e| DepotError::Server(e.to_string()))?;

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/vnd.pkg5.info"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"packages.p5i\"",
            ),
        ],
        json,
    )
        .into_response())
}

// ===========================================================================
// Embedded static assets
// ===========================================================================

static HTMX_JS_CONTENT: &str = include_str!("../../../static/js/htmx.min.js");

/// GET /ui/static/js/htmx.min.js
pub async fn static_htmx_js() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        HTMX_JS_CONTENT,
    )
        .into_response()
}
