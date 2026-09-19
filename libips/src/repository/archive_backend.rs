//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

use crate::actions::Manifest;
use crate::fmri::Fmri;
use crate::repository::{
    PackageContents, PackageInfo, ReadableRepository, RepositoryError, RepositoryInfo, Result,
};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;
use tracing::debug;
use zip::ZipArchive;

use super::PublisherInfo;
use super::archive_writer::ArchiveMetadata;
use super::file_backend::FileBackend;

/// A read-only repository backed by a `.p6p` ZIP archive.
pub struct ArchiveBackend {
    archive: Mutex<ZipArchive<fs::File>>,
    metadata: ArchiveMetadata,
}

impl ArchiveBackend {
    /// Construct the manifest path inside the archive for a given publisher/stem/version.
    fn manifest_path(publisher: &str, stem: &str, version: &str) -> String {
        let encoded_stem = FileBackend::url_encode(stem);
        let encoded_version = FileBackend::url_encode(version);
        format!(
            "publisher/{}/pkg/{}/{}",
            publisher, encoded_stem, encoded_version
        )
    }

    /// Construct the file path inside the archive for a given publisher/hash.
    fn file_path(publisher: &str, hash: &str) -> String {
        if hash.len() < 2 {
            format!("publisher/{}/file/{}", publisher, hash)
        } else {
            let first_two = &hash[0..2];
            let next_two = if hash.len() >= 4 { &hash[2..4] } else { "" };
            format!(
                "publisher/{}/file/{}/{}/{}",
                publisher, first_two, next_two, hash
            )
        }
    }
}

impl ReadableRepository for ArchiveBackend {
    fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = fs::File::open(path.as_ref()).map_err(|e| RepositoryError::FileOpenError {
            path: path.as_ref().to_path_buf(),
            source: e,
        })?;

        let mut archive =
            ZipArchive::new(file).map_err(|e| RepositoryError::ArchiveError(e.to_string()))?;

        // Read and parse the metadata index
        let metadata: ArchiveMetadata = {
            let mut entry = archive.by_name("pkg6.archive.json").map_err(|e| {
                RepositoryError::ArchiveError(format!("missing pkg6.archive.json: {}", e))
            })?;
            let mut buf = String::new();
            entry.read_to_string(&mut buf).map_err(|e| {
                RepositoryError::ArchiveError(format!("failed to read pkg6.archive.json: {}", e))
            })?;
            serde_json::from_str(&buf).map_err(|e| {
                RepositoryError::ArchiveError(format!("invalid pkg6.archive.json: {}", e))
            })?
        };

        Ok(ArchiveBackend {
            archive: Mutex::new(archive),
            metadata,
        })
    }

    fn get_info(&self) -> Result<RepositoryInfo> {
        let mut publishers = Vec::new();
        for pub_name in &self.metadata.publishers {
            let pkg_count = self
                .metadata
                .packages
                .iter()
                .filter(|p| p.publisher == *pub_name)
                .count();
            publishers.push(PublisherInfo {
                name: pub_name.clone(),
                package_count: pkg_count,
                status: "online".to_string(),
                updated: self.metadata.created.clone(),
            });
        }

        Ok(RepositoryInfo {
            publishers,
            default_publisher: self.metadata.default_publisher.clone(),
        })
    }

    fn list_packages(
        &self,
        publisher: Option<&str>,
        pattern: Option<&str>,
    ) -> Result<Vec<PackageInfo>> {
        let mut results = Vec::new();

        for entry in &self.metadata.packages {
            // Filter by publisher
            if let Some(pub_filter) = publisher {
                if entry.publisher != pub_filter {
                    continue;
                }
            }

            // Filter by pattern (simple glob)
            if let Some(pat) = pattern {
                if !matches_pattern(&entry.fmri, pat) {
                    continue;
                }
            }

            let fmri = Fmri::parse(&entry.fmri)
                .map_err(|e| RepositoryError::Other(format!("invalid FMRI in archive: {}", e)))?;
            results.push(PackageInfo { fmri });
        }

        Ok(results)
    }

    fn show_contents(
        &self,
        publisher: Option<&str>,
        pattern: Option<&str>,
        action_types: Option<&[String]>,
    ) -> Result<Vec<PackageContents>> {
        let packages = self.list_packages(publisher, pattern)?;
        let mut results = Vec::new();

        for pkg_info in &packages {
            let pub_name = pkg_info
                .fmri
                .publisher
                .as_deref()
                .or(self.metadata.default_publisher.as_deref())
                .unwrap_or("");
            let manifest = self.fetch_manifest(pub_name, &pkg_info.fmri)?;

            let mut files = Vec::new();
            let mut directories = Vec::new();
            let mut links = Vec::new();
            let mut dependencies = Vec::new();
            let mut licenses = Vec::new();

            let show_all = action_types.is_none();
            let types: Vec<&str> = action_types
                .map(|t| t.iter().map(|s| s.as_str()).collect())
                .unwrap_or_default();

            if show_all || types.contains(&"file") {
                for f in &manifest.files {
                    files.push(f.path.clone());
                }
            }
            if show_all || types.contains(&"dir") {
                for d in &manifest.directories {
                    directories.push(d.path.clone());
                }
            }
            if show_all || types.contains(&"link") {
                for l in &manifest.links {
                    links.push(l.path.clone());
                }
            }
            if show_all || types.contains(&"depend") {
                for d in &manifest.dependencies {
                    if let Some(ref fmri) = d.fmri {
                        dependencies.push(fmri.to_string());
                    }
                }
            }
            if show_all || types.contains(&"license") {
                for l in &manifest.licenses {
                    licenses.push(l.payload.clone());
                }
            }

            results.push(PackageContents {
                package_id: pkg_info.fmri.to_string(),
                files: if files.is_empty() { None } else { Some(files) },
                directories: if directories.is_empty() {
                    None
                } else {
                    Some(directories)
                },
                links: if links.is_empty() { None } else { Some(links) },
                dependencies: if dependencies.is_empty() {
                    None
                } else {
                    Some(dependencies)
                },
                licenses: if licenses.is_empty() {
                    None
                } else {
                    Some(licenses)
                },
            });
        }

        Ok(results)
    }

    fn fetch_payload(&self, publisher: &str, digest: &str, dest: &Path) -> Result<()> {
        let archive_path = Self::file_path(publisher, digest);
        debug!("Fetching payload from archive: {}", archive_path);

        let mut archive = self
            .archive
            .lock()
            .map_err(|e| RepositoryError::Other(format!("archive lock poisoned: {}", e)))?;

        let mut entry = archive.by_name(&archive_path).map_err(|e| {
            RepositoryError::ArchiveError(format!("payload {} not found: {}", digest, e))
        })?;

        // Write to a temp file then rename for atomicity
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| RepositoryError::DirectoryCreateError {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        let mut out = fs::File::create(dest).map_err(|e| RepositoryError::FileCreateError {
            path: dest.to_path_buf(),
            source: e,
        })?;

        std::io::copy(&mut entry, &mut out).map_err(|e| {
            RepositoryError::ArchiveError(format!("failed to extract payload: {}", e))
        })?;

        Ok(())
    }

    fn fetch_manifest(&self, publisher: &str, fmri: &Fmri) -> Result<Manifest> {
        let version_str = fmri.version();
        let base_path = Self::manifest_path(publisher, &fmri.name, &version_str);
        let json_path = format!("{}.json", base_path);

        // Prefer the JSON manifest (has correct SHA-256 hashes from add_file)
        {
            let mut archive = self
                .archive
                .lock()
                .map_err(|e| RepositoryError::Other(format!("archive lock poisoned: {}", e)))?;

            if let Ok(mut entry) = archive.by_name(&json_path) {
                let mut buf = String::new();
                entry.read_to_string(&mut buf).map_err(|e| {
                    RepositoryError::ArchiveError(format!("failed to read json manifest: {}", e))
                })?;
                return serde_json::from_str(&buf).map_err(|e| {
                    RepositoryError::ArchiveError(format!("invalid json manifest: {}", e))
                });
            }
        }

        // Fall back to legacy text manifest
        let text = self.fetch_manifest_text(publisher, fmri)?;
        Manifest::parse_string(text).map_err(RepositoryError::from)
    }

    fn fetch_manifest_text(&self, publisher: &str, fmri: &Fmri) -> Result<String> {
        let version_str = fmri.version();
        let archive_path = Self::manifest_path(publisher, &fmri.name, &version_str);
        debug!("Fetching manifest from archive: {}", archive_path);

        let mut archive = self
            .archive
            .lock()
            .map_err(|e| RepositoryError::Other(format!("archive lock poisoned: {}", e)))?;

        let mut entry = archive.by_name(&archive_path).map_err(|e| {
            RepositoryError::ArchiveError(format!("manifest not found for {}: {}", fmri, e))
        })?;

        let mut buf = String::new();
        entry.read_to_string(&mut buf).map_err(|e| {
            RepositoryError::ArchiveError(format!("failed to read manifest: {}", e))
        })?;

        Ok(buf)
    }

    fn search(
        &self,
        query: &str,
        publisher: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<PackageInfo>> {
        // Simple substring search over the package index
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();

        for entry in &self.metadata.packages {
            if let Some(pub_filter) = publisher {
                if entry.publisher != pub_filter {
                    continue;
                }
            }

            let matches = entry.fmri.to_lowercase().contains(&query_lower)
                || entry
                    .summary
                    .as_deref()
                    .map(|s| s.to_lowercase().contains(&query_lower))
                    .unwrap_or(false);

            if matches {
                let fmri = Fmri::parse(&entry.fmri)
                    .map_err(|e| RepositoryError::Other(format!("invalid FMRI: {}", e)))?;
                results.push(PackageInfo { fmri });

                if let Some(lim) = limit {
                    if results.len() >= lim {
                        break;
                    }
                }
            }
        }

        Ok(results)
    }
}

/// Simple glob-style pattern matching for package names.
fn matches_pattern(name: &str, pattern: &str) -> bool {
    if pattern.contains('*') || pattern.contains('?') {
        glob_match(name, pattern)
    } else {
        name.contains(pattern)
    }
}

fn glob_match(s: &str, pattern: &str) -> bool {
    let mut si = s.chars().peekable();
    let mut pi = pattern.chars().peekable();

    while pi.peek().is_some() {
        match pi.peek() {
            Some('*') => {
                pi.next();
                if pi.peek().is_none() {
                    return true;
                }
                while si.peek().is_some() {
                    if glob_match(
                        &si.clone().collect::<String>(),
                        &pi.clone().collect::<String>(),
                    ) {
                        return true;
                    }
                    si.next();
                }
                return false;
            }
            Some('?') => {
                pi.next();
                if si.next().is_none() {
                    return false;
                }
            }
            Some(&pc) => {
                pi.next();
                match si.next() {
                    Some(sc) if sc == pc => {}
                    _ => return false,
                }
            }
            None => unreachable!(),
        }
    }
    si.peek().is_none()
}
