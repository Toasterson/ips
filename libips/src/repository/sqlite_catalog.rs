//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

//! SQLite catalog shard generation and population.
//!
//! This module defines all SQLite schemas used by the IPS system and provides
//! functions to build pre-built catalog shards for distribution via the
//! catalog/2 endpoint.

use crate::actions::Manifest;
use crate::fmri::Fmri;
use crate::repository::catalog::CatalogManager;
use miette::Diagnostic;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use thiserror::Error;

/// Schema for active.db - contains non-obsolete packages and their dependencies.
/// No manifest blobs stored; manifests are fetched from repository on demand.
pub const ACTIVE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS packages (
    stem      TEXT NOT NULL,
    version   TEXT NOT NULL,
    publisher TEXT NOT NULL,
    fmri      TEXT GENERATED ALWAYS AS (
        'pkg://' || publisher || '/' || stem || '@' || version
    ) STORED,
    PRIMARY KEY (stem, version, publisher)
);
CREATE INDEX IF NOT EXISTS idx_packages_fmri ON packages(fmri);
CREATE INDEX IF NOT EXISTS idx_packages_stem ON packages(stem);

CREATE TABLE IF NOT EXISTS dependencies (
    pkg_stem      TEXT NOT NULL,
    pkg_version   TEXT NOT NULL,
    pkg_publisher TEXT NOT NULL,
    dep_type      TEXT NOT NULL,
    dep_stem      TEXT NOT NULL,
    dep_version   TEXT,
    PRIMARY KEY (pkg_stem, pkg_version, pkg_publisher, dep_type, dep_stem)
);
CREATE INDEX IF NOT EXISTS idx_deps_pkg ON dependencies(pkg_stem, pkg_version, pkg_publisher);

CREATE TABLE IF NOT EXISTS incorporate_locks (
    stem    TEXT NOT NULL PRIMARY KEY,
    release TEXT NOT NULL
);
"#;

/// Schema for obsolete.db - client-side shard for obsoleted packages.
pub const OBSOLETE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS obsolete_packages (
    publisher TEXT NOT NULL,
    stem      TEXT NOT NULL,
    version   TEXT NOT NULL,
    fmri      TEXT GENERATED ALWAYS AS (
        'pkg://' || publisher || '/' || stem || '@' || version
    ) STORED,
    PRIMARY KEY (publisher, stem, version)
);
CREATE INDEX IF NOT EXISTS idx_obsolete_fmri ON obsolete_packages(fmri);
"#;

/// Schema for fts.db - full-text search index.
pub const FTS_SCHEMA: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS package_search
    USING fts5(stem, publisher, summary, description,
               tokenize='unicode61');
"#;

/// Schema for installed.db - tracks installed packages with manifest blobs.
pub const INSTALLED_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS installed (
    fmri     TEXT NOT NULL PRIMARY KEY,
    manifest BLOB NOT NULL
);
"#;

/// Schema for index.db (repository/obsoleted.rs) - server-side obsoleted package index.
pub const OBSOLETED_INDEX_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS obsoleted_packages (
    fmri                TEXT NOT NULL PRIMARY KEY,
    publisher           TEXT NOT NULL,
    stem                TEXT NOT NULL,
    version             TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'obsolete',
    obsolescence_date   TEXT NOT NULL,
    deprecation_message TEXT,
    obsoleted_by        TEXT,
    metadata_version    INTEGER NOT NULL DEFAULT 1,
    content_hash        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_obsidx_stem ON obsoleted_packages(stem);

CREATE TABLE IF NOT EXISTS obsoleted_manifests (
    content_hash TEXT NOT NULL PRIMARY KEY,
    manifest     TEXT NOT NULL
);
"#;

#[derive(Debug, Error, Diagnostic)]
#[error("Shard building error: {message}")]
#[diagnostic(code(ips::shard_build_error))]
pub struct ShardBuildError {
    pub message: String,
}

impl ShardBuildError {
    fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

impl From<rusqlite::Error> for ShardBuildError {
    fn from(e: rusqlite::Error) -> Self {
        Self::new(format!("SQLite error: {}", e))
    }
}

impl From<std::io::Error> for ShardBuildError {
    fn from(e: std::io::Error) -> Self {
        Self::new(format!("IO error: {}", e))
    }
}

impl From<serde_json::Error> for ShardBuildError {
    fn from(e: serde_json::Error) -> Self {
        Self::new(format!("JSON error: {}", e))
    }
}

/// Shard metadata entry in catalog.attrs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardEntry {
    pub sha256: String,
    pub size: u64,
    #[serde(rename = "last-modified")]
    pub last_modified: String,
}

/// Shard index JSON structure for catalog/2/catalog.attrs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardIndex {
    pub version: u32,
    pub created: String,
    #[serde(rename = "last-modified")]
    pub last_modified: String,
    #[serde(rename = "package-count")]
    pub package_count: usize,
    #[serde(rename = "package-version-count")]
    pub package_version_count: usize,
    pub shards: BTreeMap<String, ShardEntry>,
}

/// Build catalog shards from JSON catalog parts.
///
/// Reads catalog parts from `catalog_parts_dir`, generates active.db, obsolete.db,
/// and fts.db, writes them to `output_dir`, and creates catalog.attrs index.
pub fn build_shards(
    catalog_parts_dir: &Path,
    publisher: &str,
    output_dir: &Path,
) -> Result<(), ShardBuildError> {
    // Create temp directory for shard generation
    fs::create_dir_all(output_dir)?;
    let temp_dir = output_dir.join(".tmp");
    fs::create_dir_all(&temp_dir)?;

    // Create shard databases
    let active_path = temp_dir.join("active.db");
    let obsolete_path = temp_dir.join("obsolete.db");
    let fts_path = temp_dir.join("fts.db");

    let mut active_conn = Connection::open(&active_path)?;
    let mut obsolete_conn = Connection::open(&obsolete_path)?;
    let mut fts_conn = Connection::open(&fts_path)?;

    // Execute schemas
    active_conn.execute_batch(ACTIVE_SCHEMA)?;
    obsolete_conn.execute_batch(OBSOLETE_SCHEMA)?;
    fts_conn.execute_batch(FTS_SCHEMA)?;

    // Read catalog parts
    let catalog_manager = CatalogManager::new(catalog_parts_dir, publisher)
        .map_err(|e| ShardBuildError::new(format!("Failed to create catalog manager: {}", e)))?;
    let mut package_count = 0usize;
    let mut package_version_count = 0usize;

    // Begin transactions for batch inserts
    let active_tx = active_conn.transaction()?;
    let obsolete_tx = obsolete_conn.transaction()?;
    let fts_tx = fts_conn.transaction()?;

    {
        let mut insert_pkg = active_tx.prepare(
            "INSERT OR REPLACE INTO packages (stem, version, publisher) VALUES (?1, ?2, ?3)",
        )?;
        let mut insert_dep = active_tx.prepare(
            "INSERT OR REPLACE INTO dependencies (pkg_stem, pkg_version, pkg_publisher, dep_type, dep_stem, dep_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut insert_obs = obsolete_tx.prepare(
            "INSERT OR REPLACE INTO obsolete_packages (publisher, stem, version) VALUES (?1, ?2, ?3)",
        )?;
        let mut insert_fts = fts_tx.prepare(
            "INSERT INTO package_search (stem, publisher, summary, description) VALUES (?1, ?2, ?3, ?4)",
        )?;

        // Iterate catalog parts
        let part_names: Vec<String> = catalog_manager.attrs().parts.keys().cloned().collect();
        for part_name in part_names {
            let part_path = catalog_parts_dir.join(&part_name);

            // Load the CatalogPart
            let part = crate::repository::catalog::CatalogPart::load(&part_path)
                .map_err(|e| ShardBuildError::new(format!("Failed to load catalog part: {}", e)))?;

            // Iterate through publishers in the catalog part
            for (part_publisher, stems) in &part.packages {
                // Only process packages for the requested publisher
                if part_publisher != publisher {
                    continue;
                }

                // Iterate through package stems
                for (pkg_name, versions) in stems {
                    // Iterate through versions
                    for version_entry in versions {
                        let pkg_version = &version_entry.version;

                        // Build a minimal manifest from the actions
                        let mut manifest = Manifest::new();

                        // Parse actions if available
                        if let Some(actions) = &version_entry.actions {
                            for action_str in actions {
                                if action_str.starts_with("set ") {
                                    // Parse "set name=key value=val" format
                                    let parts: Vec<&str> = action_str.split_whitespace().collect();
                                    if parts.len() >= 3 {
                                        if let Some(name_part) = parts.get(1) {
                                            if let Some(key) = name_part.strip_prefix("name=") {
                                                if let Some(value_part) = parts.get(2) {
                                                    if let Some(mut value) =
                                                        value_part.strip_prefix("value=")
                                                    {
                                                        // Remove quotes
                                                        value = value.trim_matches('"');

                                                        let mut attr =
                                                            crate::actions::Attr::default();
                                                        attr.key = key.to_string();
                                                        attr.values = vec![value.to_string()];
                                                        manifest.attributes.push(attr);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                } else if action_str.starts_with("depend ") {
                                    // Parse "depend fmri=... type=..." format
                                    let mut dep = crate::actions::Dependency::default();
                                    for part in action_str.split_whitespace().skip(1) {
                                        if let Some((k, v)) = part.split_once('=') {
                                            match k {
                                                "fmri" => {
                                                    if let Ok(f) = crate::fmri::Fmri::parse(v) {
                                                        dep.fmri = Some(f);
                                                    }
                                                }
                                                "type" => {
                                                    dep.dependency_type = v.to_string();
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                    if dep.fmri.is_some() && !dep.dependency_type.is_empty() {
                                        manifest.dependencies.push(dep);
                                    }
                                }
                            }
                        }

                        // Determine if obsolete
                        let is_obsolete = crate::image::catalog::is_package_obsolete(&manifest);

                        // Count all package versions
                        package_version_count += 1;

                        // Obsolete packages go only to obsolete.db, non-obsolete go to active.db
                        if is_obsolete {
                            insert_obs.execute(rusqlite::params![
                                publisher,
                                pkg_name,
                                pkg_version
                            ])?;
                        } else {
                            // Insert into packages table (active.db)
                            insert_pkg.execute(rusqlite::params![
                                pkg_name,
                                pkg_version,
                                publisher
                            ])?;

                            // Extract and insert dependencies
                            for dep in &manifest.dependencies {
                                if dep.dependency_type == "require"
                                    || dep.dependency_type == "incorporate"
                                {
                                    if let Some(dep_fmri) = &dep.fmri {
                                        let dep_stem = dep_fmri.stem();
                                        let dep_version =
                                            dep_fmri.version.as_ref().map(|v| v.to_string());
                                        insert_dep.execute(rusqlite::params![
                                            pkg_name,
                                            pkg_version,
                                            publisher,
                                            &dep.dependency_type,
                                            dep_stem,
                                            dep_version
                                        ])?;
                                    }
                                }
                            }
                        }

                        // Extract summary and description for FTS
                        let summary = manifest
                            .attributes
                            .iter()
                            .find(|a| a.key == "pkg.summary")
                            .and_then(|a| a.values.first())
                            .map(|s| s.as_str())
                            .unwrap_or("");
                        let description = manifest
                            .attributes
                            .iter()
                            .find(|a| a.key == "pkg.description")
                            .and_then(|a| a.values.first())
                            .map(|s| s.as_str())
                            .unwrap_or("");

                        insert_fts.execute(rusqlite::params![
                            pkg_name,
                            publisher,
                            summary,
                            description
                        ])?;
                    }
                }
            }
        }
    }

    // Commit transactions
    active_tx.commit()?;
    obsolete_tx.commit()?;
    fts_tx.commit()?;

    // Count unique packages (stems)
    let count: i64 =
        active_conn.query_row("SELECT COUNT(DISTINCT stem) FROM packages", [], |row| {
            row.get(0)
        })?;
    package_count = count as usize;

    // Close connections
    drop(active_conn);
    drop(obsolete_conn);
    drop(fts_conn);

    // Compute SHA-256 hashes and build index
    let mut shards = BTreeMap::new();
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

    for (name, path) in [
        ("active.db", &active_path),
        ("obsolete.db", &obsolete_path),
        ("fts.db", &fts_path),
    ] {
        let bytes = fs::read(path)?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let hash = format!("{:x}", hasher.finalize());
        let size = bytes.len() as u64;

        shards.insert(
            name.to_string(),
            ShardEntry {
                sha256: hash.clone(),
                size,
                last_modified: now.clone(),
            },
        );

        // Copy shard to output directory with both original name and hash-based name
        // Keep original name for client-side use (e.g., active.db, obsolete.db)
        let named_path = output_dir.join(name);
        fs::copy(path, &named_path)?;

        // Also copy to hash-based name for content-addressed server distribution
        let hash_path = output_dir.join(&hash);
        fs::copy(path, &hash_path)?;
    }

    // Write catalog.attrs
    let index = ShardIndex {
        version: 2,
        created: now.clone(),
        last_modified: now,
        package_count,
        package_version_count,
        shards,
    };
    let index_json = serde_json::to_string_pretty(&index)?;
    fs::write(output_dir.join("catalog.attrs"), index_json)?;

    // Clean up temp directory
    fs::remove_dir_all(&temp_dir).ok();

    Ok(())
}

/// Helper function for tests: populate active.db with a single package.
/// Creates tables if absent (idempotent).
pub fn populate_active_db(
    db_path: &Path,
    fmri: &Fmri,
    manifest: &Manifest,
) -> Result<(), ShardBuildError> {
    let mut conn = Connection::open(db_path)?;
    conn.execute_batch(ACTIVE_SCHEMA)?;

    let tx = conn.transaction()?;
    {
        tx.execute(
            "INSERT OR REPLACE INTO packages (stem, version, publisher) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                fmri.stem(),
                fmri.version(),
                fmri.publisher.as_deref().unwrap_or("")
            ],
        )?;

        for dep in &manifest.dependencies {
            if dep.dependency_type == "require" || dep.dependency_type == "incorporate" {
                if let Some(dep_fmri) = &dep.fmri {
                    let dep_stem = dep_fmri.stem();
                    let dep_version = dep_fmri.version.as_ref().map(|v| v.to_string());
                    tx.execute(
                        "INSERT OR REPLACE INTO dependencies (pkg_stem, pkg_version, pkg_publisher, dep_type, dep_stem, dep_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![
                            fmri.stem(),
                            fmri.version(),
                            fmri.publisher.as_deref().unwrap_or(""),
                            &dep.dependency_type,
                            dep_stem,
                            dep_version
                        ],
                    )?;
                }
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Helper function for tests: mark a package as obsolete in obsolete.db.
/// Creates tables if absent (idempotent).
pub fn populate_obsolete_db(db_path: &Path, fmri: &Fmri) -> Result<(), ShardBuildError> {
    let conn = Connection::open(db_path)?;
    conn.execute_batch(OBSOLETE_SCHEMA)?;

    conn.execute(
        "INSERT OR REPLACE INTO obsolete_packages (publisher, stem, version) VALUES (?1, ?2, ?3)",
        rusqlite::params![
            fmri.publisher.as_deref().unwrap_or(""),
            fmri.stem(),
            fmri.version()
        ],
    )?;
    Ok(())
}

// Note: compress_json_lz4, decode_manifest_bytes, and is_package_obsolete
// are available as pub(crate) in crate::image::catalog and can be used
// within libips but not re-exported.

/// Result from an FTS5 search query.
#[derive(Debug, Clone)]
pub struct FtsSearchResult {
    pub stem: String,
    pub publisher: String,
    pub summary: String,
    pub description: String,
    pub rank: f64,
}

/// Sanitize a user query string for safe use in an FTS5 MATCH expression.
///
/// Wraps each whitespace-delimited token in double quotes to treat it as a
/// literal phrase, preventing FTS5 syntax injection. Glob-style `*` at the end
/// of a token is converted to FTS5 prefix syntax (e.g. `web*` → `"web" *`).
pub fn sanitize_fts_query(raw: &str) -> String {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if tokens.is_empty() {
        return String::new();
    }

    let mut parts = Vec::new();
    for token in tokens {
        // Strip surrounding quotes if present
        let token = token.trim_matches('"');
        if token.is_empty() {
            continue;
        }

        if token.ends_with('*') {
            let prefix = &token[..token.len() - 1];
            if !prefix.is_empty() {
                // FTS5 prefix query: "term" *  (but FTS5 prefix is just term*)
                // Actually FTS5 prefix syntax is: term*
                // We need to escape the term part but keep the *
                let escaped = prefix.replace('"', "\"\"");
                parts.push(format!("\"{}\"*", escaped));
            }
        } else {
            let escaped = token.replace('"', "\"\"");
            parts.push(format!("\"{}\"", escaped));
        }
    }
    parts.join(" ")
}

/// Search the FTS5 index in fts.db for packages matching a query.
pub fn search_fts(
    fts_db_path: &Path,
    query: &str,
    publisher_filter: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<FtsSearchResult>, ShardBuildError> {
    let conn = Connection::open_with_flags(fts_db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    let sanitized = sanitize_fts_query(query);
    if sanitized.is_empty() {
        return Ok(Vec::new());
    }

    let effective_limit = limit.unwrap_or(100) as i64;

    // FTS5 columns cannot be filtered with standard WHERE clauses,
    // so we fetch all matches and filter by publisher in Rust.
    let mut stmt = conn.prepare(
        "SELECT stem, publisher, summary, description, rank \
         FROM package_search \
         WHERE package_search MATCH ?1 \
         ORDER BY rank \
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![sanitized, effective_limit], |row| {
        Ok(FtsSearchResult {
            stem: row.get(0)?,
            publisher: row.get(1)?,
            summary: row.get(2)?,
            description: row.get(3)?,
            rank: row.get(4)?,
        })
    })?;
    let mut results: Vec<FtsSearchResult> = rows.collect::<Result<Vec<_>, _>>()?;

    if let Some(publisher) = publisher_filter {
        results.retain(|r| r.publisher == publisher);
    }

    Ok(results)
}

/// Look up the latest FMRI for each (stem, publisher) pair from active.db.
pub fn resolve_latest_fmris(
    active_db_path: &Path,
    stems: &[(String, String)],
) -> Result<HashMap<(String, String), String>, ShardBuildError> {
    let conn = Connection::open_with_flags(active_db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT fmri FROM packages WHERE stem = ?1 AND publisher = ?2 ORDER BY rowid DESC LIMIT 1",
    )?;

    let mut result = HashMap::new();
    for (stem, publisher) in stems {
        if let Ok(fmri) = stmt.query_row(rusqlite::params![stem, publisher], |row| {
            row.get::<_, String>(0)
        }) {
            result.insert((stem.clone(), publisher.clone()), fmri);
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_fts_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(FTS_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO package_search (stem, publisher, summary, description) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["web/server/nginx", "openindiana.org", "NGINX web server", "High performance HTTP server and reverse proxy"],
        ).unwrap();
        conn.execute(
            "INSERT INTO package_search (stem, publisher, summary, description) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["web/server/apache", "openindiana.org", "Apache HTTP Server", "The Apache HTTP Server Project"],
        ).unwrap();
        conn.execute(
            "INSERT INTO package_search (stem, publisher, summary, description) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["database/postgresql", "openindiana.org", "PostgreSQL database", "PostgreSQL object-relational database management"],
        ).unwrap();
        conn.execute(
            "INSERT INTO package_search (stem, publisher, summary, description) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["runtime/coreutils", "otherpub", "Core utilities", "Essential command line utilities"],
        ).unwrap();
    }

    fn create_test_active_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(ACTIVE_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO packages (stem, version, publisher) VALUES (?1, ?2, ?3)",
            rusqlite::params!["web/server/nginx", "1.24.0,5.11-2024.0.1.0", "openindiana.org"],
        ).unwrap();
        conn.execute(
            "INSERT INTO packages (stem, version, publisher) VALUES (?1, ?2, ?3)",
            rusqlite::params!["web/server/apache", "2.4.58,5.11-2024.0.1.0", "openindiana.org"],
        ).unwrap();
        conn.execute(
            "INSERT INTO packages (stem, version, publisher) VALUES (?1, ?2, ?3)",
            rusqlite::params!["database/postgresql", "16.1,5.11-2024.0.1.0", "openindiana.org"],
        ).unwrap();
    }

    #[test]
    fn test_search_fts_basic() {
        let dir = tempfile::tempdir().unwrap();
        let fts_path = dir.path().join("fts.db");
        create_test_fts_db(&fts_path);

        let results = search_fts(&fts_path, "nginx", None, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].stem, "web/server/nginx");
    }

    #[test]
    fn test_search_fts_multiple_results() {
        let dir = tempfile::tempdir().unwrap();
        let fts_path = dir.path().join("fts.db");
        create_test_fts_db(&fts_path);

        // "server" appears in both nginx and apache summaries
        let results = search_fts(&fts_path, "server", None, None).unwrap();
        assert!(results.len() >= 2);
    }

    #[test]
    fn test_search_fts_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let fts_path = dir.path().join("fts.db");
        create_test_fts_db(&fts_path);

        let results = search_fts(&fts_path, "post*", None, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].stem, "database/postgresql");
    }

    #[test]
    fn test_search_fts_no_results() {
        let dir = tempfile::tempdir().unwrap();
        let fts_path = dir.path().join("fts.db");
        create_test_fts_db(&fts_path);

        let results = search_fts(&fts_path, "nonexistent", None, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_fts_publisher_filter() {
        let dir = tempfile::tempdir().unwrap();
        let fts_path = dir.path().join("fts.db");
        create_test_fts_db(&fts_path);

        // "coreutils" matches the otherpub entry only
        let results =
            search_fts(&fts_path, "coreutils", Some("otherpub"), None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].publisher, "otherpub");

        // Same query but filtered to openindiana.org should return nothing
        let results =
            search_fts(&fts_path, "coreutils", Some("openindiana.org"), None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_fts_limit() {
        let dir = tempfile::tempdir().unwrap();
        let fts_path = dir.path().join("fts.db");
        create_test_fts_db(&fts_path);

        let results = search_fts(&fts_path, "server", None, Some(1)).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_resolve_latest_fmris() {
        let dir = tempfile::tempdir().unwrap();
        let active_path = dir.path().join("active.db");
        create_test_active_db(&active_path);

        let stems = vec![
            ("web/server/nginx".to_string(), "openindiana.org".to_string()),
            ("database/postgresql".to_string(), "openindiana.org".to_string()),
            ("nonexistent/pkg".to_string(), "openindiana.org".to_string()),
        ];

        let fmris = resolve_latest_fmris(&active_path, &stems).unwrap();
        assert_eq!(fmris.len(), 2); // nonexistent should not be in results
        assert!(fmris[&("web/server/nginx".to_string(), "openindiana.org".to_string())]
            .contains("nginx"));
        assert!(fmris[&("database/postgresql".to_string(), "openindiana.org".to_string())]
            .contains("postgresql"));
    }

    #[test]
    fn test_sanitize_fts_query() {
        assert_eq!(sanitize_fts_query("nginx"), "\"nginx\"");
        assert_eq!(sanitize_fts_query("web server"), "\"web\" \"server\"");
        assert_eq!(sanitize_fts_query("post*"), "\"post\"*");
        assert_eq!(sanitize_fts_query(""), "");
        assert_eq!(sanitize_fts_query("  "), "");
    }
}
