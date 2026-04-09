//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

use crate::fmri::Fmri;
use crate::repository::{ReadableRepository, RepositoryError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use tracing::{debug, info};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use super::file_backend::FileBackend;

/// Metadata stored in `pkg6.archive.json` at the root of a `.p6p` archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveMetadata {
    pub version: u32,
    pub format: String,
    pub created: String,
    pub publishers: Vec<String>,
    pub default_publisher: Option<String>,
    pub packages: Vec<ArchivePackageEntry>,
    pub total_packages: usize,
    pub total_files: usize,
}

/// An entry in the archive's package index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivePackageEntry {
    pub fmri: String,
    pub publisher: String,
    pub manifest_path: String,
    pub file_hashes: Vec<String>,
    #[serde(default)]
    pub obsolete: bool,
    #[serde(default)]
    pub renamed: bool,
    pub summary: Option<String>,
}

/// Builder for creating `.p6p` archives.
pub struct ArchiveWriter {
    zip: ZipWriter<fs::File>,
    packages: Vec<ArchivePackageEntry>,
    publishers: HashSet<String>,
    default_publisher: Option<String>,
    written_files: HashSet<String>,
    total_files: usize,
}

impl ArchiveWriter {
    /// Create a new archive at the given path.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = fs::File::create(path.as_ref()).map_err(|e| RepositoryError::FileCreateError {
            path: path.as_ref().to_path_buf(),
            source: e,
        })?;

        let zip = ZipWriter::new(file);

        Ok(ArchiveWriter {
            zip,
            packages: Vec::new(),
            publishers: HashSet::new(),
            default_publisher: None,
            written_files: HashSet::new(),
            total_files: 0,
        })
    }

    /// Add all packages from a readable repository for the given publisher.
    /// If `fmri_filter` is `Some`, only packages matching those FMRIs are added.
    pub fn add_from_repository<R: ReadableRepository + Sync>(
        &mut self,
        source: &R,
        publisher: &str,
        fmri_filter: Option<&[Fmri]>,
    ) -> Result<()> {
        self.publishers.insert(publisher.to_string());
        if self.default_publisher.is_none() {
            self.default_publisher = Some(publisher.to_string());
        }

        let packages = source.list_packages(Some(publisher), None)?;

        for pkg_info in &packages {
            let fmri = &pkg_info.fmri;

            // Apply filter if provided
            if let Some(filter) = fmri_filter {
                let matches = filter.iter().any(|f| {
                    f.name == fmri.name
                        && (f.version.is_none() || f.version() == fmri.version())
                });
                if !matches {
                    continue;
                }
            }

            self.add_package_from_repo(source, publisher, fmri)?;
        }

        Ok(())
    }

    /// Add a single package from a repository source.
    fn add_package_from_repo<R: ReadableRepository + Sync>(
        &mut self,
        source: &R,
        publisher: &str,
        fmri: &Fmri,
    ) -> Result<()> {
        info!("Adding package {} from publisher {}", fmri, publisher);

        // Fetch the manifest text (legacy IPS format) for archival
        let manifest_text = source.fetch_manifest_text(publisher, fmri)?;

        // Fetch the parsed manifest (prefers JSON which has correct SHA-256 hashes
        // from add_file, falling back to text parsing). pkg5-imported repos store
        // files under SHA-256 hashes but legacy manifests reference SHA1 hashes.
        let manifest = source.fetch_manifest(publisher, fmri)?;

        let version_str = fmri.version();
        let encoded_stem = FileBackend::url_encode(&fmri.name);
        let encoded_version = FileBackend::url_encode(&version_str);

        let manifest_archive_path = format!(
            "publisher/{}/pkg/{}/{}",
            publisher, encoded_stem, encoded_version
        );

        // Write the legacy manifest
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Zstd)
            .compression_level(Some(3));

        self.zip
            .start_file(&manifest_archive_path, options)
            .map_err(|e| RepositoryError::ArchiveError(format!("failed to start manifest entry: {}", e)))?;
        self.zip
            .write_all(manifest_text.as_bytes())
            .map_err(|e| RepositoryError::ArchiveError(format!("failed to write manifest: {}", e)))?;

        // Write the JSON manifest
        let json_manifest_path = format!("{}.json", manifest_archive_path);
        let json_bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| RepositoryError::JsonSerializeError(e.to_string()))?;

        self.zip
            .start_file(&json_manifest_path, options)
            .map_err(|e| RepositoryError::ArchiveError(format!("failed to start json manifest entry: {}", e)))?;
        self.zip
            .write_all(&json_bytes)
            .map_err(|e| RepositoryError::ArchiveError(format!("failed to write json manifest: {}", e)))?;

        // Collect file hashes and write payloads
        let mut file_hashes = Vec::new();

        for file_action in &manifest.files {
            if let Some(ref payload) = file_action.payload {
                let hash = &payload.primary_identifier.hash;
                file_hashes.push(hash.clone());

                // Deduplicate: skip if already written
                if self.written_files.contains(hash) {
                    continue;
                }

                let file_archive_path = Self::file_archive_path(publisher, hash);

                // Fetch payload to a temporary file, then read and compress into ZIP
                let temp_dir = tempfile::tempdir()
                    .map_err(|e| RepositoryError::ArchiveError(format!("failed to create temp dir: {}", e)))?;
                let temp_path = temp_dir.path().join(hash);

                source.fetch_payload(publisher, hash, &temp_path)?;

                // Read the payload (which is compressed from the source repo)
                // and decompress it before writing to the archive.
                // The ZIP zstd compression will re-compress it.
                let payload_bytes = Self::read_and_decompress(&temp_path)?;

                self.zip
                    .start_file(&file_archive_path, options)
                    .map_err(|e| RepositoryError::ArchiveError(format!("failed to start file entry: {}", e)))?;
                self.zip
                    .write_all(&payload_bytes)
                    .map_err(|e| RepositoryError::ArchiveError(format!("failed to write file: {}", e)))?;

                self.written_files.insert(hash.clone());
                self.total_files += 1;

                debug!("Wrote payload {} ({} bytes)", hash, payload_bytes.len());
            }
        }

        // Also handle signature payloads
        for sig in &manifest.signatures {
            let digest = if !sig.value.is_empty() {
                &sig.value
            } else if !sig.chash.is_empty() {
                &sig.chash
            } else {
                continue;
            };

            if self.written_files.contains(digest) {
                continue;
            }

            let file_archive_path = Self::file_archive_path(publisher, digest);
            let temp_dir = tempfile::tempdir()
                .map_err(|e| RepositoryError::ArchiveError(format!("failed to create temp dir: {}", e)))?;
            let temp_path = temp_dir.path().join(digest);

            match source.fetch_payload(publisher, digest, &temp_path) {
                Ok(()) => {
                    let payload_bytes = fs::read(&temp_path).map_err(|e| {
                        RepositoryError::FileReadError {
                            path: temp_path.clone(),
                            source: e,
                        }
                    })?;

                    self.zip
                        .start_file(&file_archive_path, options)
                        .map_err(|e| RepositoryError::ArchiveError(format!("failed to start sig entry: {}", e)))?;
                    self.zip
                        .write_all(&payload_bytes)
                        .map_err(|e| RepositoryError::ArchiveError(format!("failed to write sig: {}", e)))?;

                    self.written_files.insert(digest.clone());
                    self.total_files += 1;
                }
                Err(e) => {
                    debug!("Could not fetch signature payload {}: {}", digest, e);
                }
            }
        }

        // Extract summary from manifest attributes
        let summary = manifest
            .attributes
            .iter()
            .find(|a| a.key == "pkg.summary")
            .and_then(|a| a.values.first().cloned());

        let obsolete = manifest
            .attributes
            .iter()
            .any(|a| a.key == "pkg.obsolete" && a.values.first().map(|v| v == "true").unwrap_or(false));

        let renamed = manifest
            .attributes
            .iter()
            .any(|a| a.key == "pkg.renamed" && a.values.first().map(|v| v == "true").unwrap_or(false));

        self.packages.push(ArchivePackageEntry {
            fmri: fmri.to_string(),
            publisher: publisher.to_string(),
            manifest_path: manifest_archive_path,
            file_hashes,
            obsolete,
            renamed,
            summary,
        });

        Ok(())
    }

    /// Construct the archive-internal path for a file payload.
    fn file_archive_path(publisher: &str, hash: &str) -> String {
        if hash.len() < 4 {
            format!("publisher/{}/file/{}", publisher, hash)
        } else {
            let first_two = &hash[0..2];
            let next_two = &hash[2..4];
            format!(
                "publisher/{}/file/{}/{}/{}",
                publisher, first_two, next_two, hash
            )
        }
    }

    /// Read a file and attempt to decompress it (gzip or lz4).
    /// If decompression fails, return the raw bytes (already uncompressed).
    fn read_and_decompress(path: &Path) -> Result<Vec<u8>> {
        let raw = fs::read(path).map_err(|e| RepositoryError::FileReadError {
            path: path.to_path_buf(),
            source: e,
        })?;

        // Try gzip decompression
        if raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
            let mut decoder = flate2::read::GzDecoder::new(&raw[..]);
            let mut decompressed = Vec::new();
            if std::io::Read::read_to_end(&mut decoder, &mut decompressed).is_ok() {
                return Ok(decompressed);
            }
        }

        // Try LZ4 decompression (LZ4 frame magic: 04 22 4D 18)
        if raw.len() >= 4 && raw[0] == 0x04 && raw[1] == 0x22 && raw[2] == 0x4D && raw[3] == 0x18
        {
            let decoder = lz4::Decoder::new(&raw[..]);
            if let Ok(mut decoder) = decoder {
                let mut decompressed = Vec::new();
                if std::io::Read::read_to_end(&mut decoder, &mut decompressed).is_ok() {
                    return Ok(decompressed);
                }
            }
        }

        // Not compressed or unknown format — return raw bytes
        Ok(raw)
    }

    /// Finalize the archive: write the metadata index and close the ZIP file.
    pub fn finish(mut self) -> Result<()> {
        let metadata = ArchiveMetadata {
            version: 1,
            format: "pkg6p".to_string(),
            created: chrono::Utc::now().to_rfc3339(),
            publishers: self.publishers.into_iter().collect(),
            default_publisher: self.default_publisher,
            packages: self.packages.clone(),
            total_packages: self.packages.len(),
            total_files: self.total_files,
        };

        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Zstd)
            .compression_level(Some(3));

        let json = serde_json::to_vec_pretty(&metadata)
            .map_err(|e| RepositoryError::JsonSerializeError(e.to_string()))?;

        self.zip
            .start_file("pkg6.archive.json", options)
            .map_err(|e| RepositoryError::ArchiveError(format!("failed to write index: {}", e)))?;
        self.zip
            .write_all(&json)
            .map_err(|e| RepositoryError::ArchiveError(format!("failed to write index: {}", e)))?;

        self.zip
            .finish()
            .map_err(|e| RepositoryError::ArchiveError(format!("failed to finalize archive: {}", e)))?;

        info!(
            "Archive complete: {} packages, {} files",
            metadata.total_packages, metadata.total_files
        );

        Ok(())
    }
}
