//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

use crate::actions::Manifest;
use crate::fmri::Fmri;
use crate::repository::{
    FileBackend, NoopProgressReporter, ProgressInfo, ProgressReporter, ReadableRepository,
    RepositoryError, Result, WritableRepository,
};
use rayon::prelude::*;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tempfile::tempdir_in;
use tracing::{debug, info};

/// PackageReceiver handles downloading packages from a source repository
/// and storing them in a destination repository.
pub struct PackageReceiver<'a, S: ReadableRepository> {
    source: &'a S,
    dest: FileBackend,
    progress: Option<&'a dyn ProgressReporter>,
}

impl<'a, S: ReadableRepository + Sync> PackageReceiver<'a, S> {
    /// Create a new PackageReceiver
    pub fn new(source: &'a S, dest: FileBackend) -> Self {
        Self {
            source,
            dest,
            progress: None,
        }
    }

    /// Set the progress reporter
    pub fn with_progress(mut self, progress: &'a dyn ProgressReporter) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Receive packages from the source repository
    ///
    /// # Arguments
    ///
    /// * `default_publisher` - The default publisher name if not specified in FMRI
    /// * `fmris` - List of FMRIs to receive
    /// * `recursive` - Whether to receive dependencies recursively
    pub fn receive(
        &mut self,
        default_publisher: Option<&str>,
        fmris: &[Fmri],
        recursive: bool,
    ) -> Result<()> {
        let mut processed = HashSet::new();
        let mut queue: Vec<Fmri> = fmris.to_vec();
        let mut updated_publishers = HashSet::new();
        let mut queued: HashSet<Fmri> = fmris.iter().cloned().collect();

        let progress = self.progress.unwrap_or(&NoopProgressReporter);
        let mut overall_progress = ProgressInfo::new("Receiving packages");
        progress.start(&overall_progress);

        let mut total_packages = queue.len() as u64;
        let mut packages_done = 0u64;

        while let Some(fmri) = queue.pop() {
            // If the FMRI doesn't have a version, we need to find the newest one
            let fmris_to_fetch = if fmri.version.is_none() {
                let publisher =
                    fmri.publisher
                        .as_deref()
                        .or(default_publisher)
                        .ok_or_else(|| {
                            RepositoryError::Other(format!(
                                "No publisher specified for package {}",
                                fmri.name
                            ))
                        })?;

                overall_progress = overall_progress
                    .with_context(format!("Looking up newest version for {}", fmri.name));
                progress.update(&overall_progress);

                debug!("No version specified for {}, looking up newest", fmri.name);
                let pkgs = self
                    .source
                    .list_packages(Some(publisher), Some(&fmri.name))?;

                // Group by package name to find the newest version for each
                let mut by_name: std::collections::HashMap<
                    String,
                    Vec<crate::repository::PackageInfo>,
                > = std::collections::HashMap::new();
                for pi in pkgs {
                    by_name.entry(pi.fmri.name.clone()).or_default().push(pi);
                }

                let mut results = Vec::new();
                for (name, versions) in by_name {
                    let newest = versions
                        .into_iter()
                        .max_by(|a, b| a.fmri.to_string().cmp(&b.fmri.to_string()));
                    if let Some(pi) = newest {
                        results.push(pi.fmri);
                    } else {
                        info!(
                            "Package {} not found in source for publisher {}",
                            name, publisher
                        );
                    }
                }

                if results.is_empty() {
                    info!(
                        "Package {} not found in source for publisher {}",
                        fmri.name, publisher
                    );
                    continue;
                }
                // Update total_packages: remove the wildcard FMRI we just popped, and add actual results
                total_packages = total_packages.saturating_sub(1) + results.len() as u64;
                results
            } else {
                vec![fmri]
            };

            for fmri_to_fetch in fmris_to_fetch {
                let publisher_name = fmri_to_fetch
                    .publisher
                    .as_deref()
                    .or(default_publisher)
                    .ok_or_else(|| {
                        RepositoryError::Other(format!(
                            "No publisher specified for package {}",
                            fmri_to_fetch.name
                        ))
                    })?
                    .to_string();

                if !processed.insert(fmri_to_fetch.clone()) {
                    // If we already processed it (possibly as a dependency), don't count it again
                    // and decrement total if we just added it from wildcard expansion
                    continue;
                }

                packages_done += 1;
                overall_progress = overall_progress
                    .with_total(total_packages)
                    .with_current(packages_done)
                    .with_context(format!("Receiving {}", fmri_to_fetch));
                progress.update(&overall_progress);

                info!(
                    "Receiving package {} from publisher {}",
                    fmri_to_fetch, publisher_name
                );
                let manifest = self.receive_one(&publisher_name, &fmri_to_fetch)?;
                updated_publishers.insert(publisher_name.clone());

                if recursive {
                    for dep in manifest.dependencies {
                        if let Some(mut dep_fmri) = dep.fmri {
                            // Ensure it has the publisher if not specified
                            if dep_fmri.publisher.is_none() {
                                dep_fmri.publisher = Some(publisher_name.clone());
                            }

                            if !processed.contains(&dep_fmri) && queued.insert(dep_fmri.clone()) {
                                total_packages += 1;
                                queue.push(dep_fmri);
                            }
                        }
                    }
                }
            }
        }

        for pub_name in updated_publishers {
            info!("Rebuilding metadata for publisher {}", pub_name);
            overall_progress =
                overall_progress.with_context(format!("Rebuilding metadata for {}", pub_name));
            progress.update(&overall_progress);
            self.dest.rebuild(Some(&pub_name), false, false)?;
        }

        progress.finish(&overall_progress);

        Ok(())
    }

    /// Receive a single package
    pub fn receive_one(&mut self, publisher: &str, fmri: &Fmri) -> Result<Manifest> {
        let progress = self.progress.unwrap_or(&NoopProgressReporter);

        let manifest_text = self.source.fetch_manifest_text(publisher, fmri)?;
        // Use fetch_manifest which prefers JSON (has correct SHA-256 hashes
        // from add_file) over legacy text (which may have SHA1 hashes)
        let manifest = self.source.fetch_manifest(publisher, fmri)?;

        // Ensure publisher exists in destination
        let dest_info = self.dest.get_info()?;
        if !dest_info.publishers.iter().any(|p| p.name == publisher) {
            info!("Adding publisher {} to destination repository", publisher);
            self.dest.add_publisher(publisher)?;
        }

        let mut txn = self.dest.begin_transaction()?;
        txn.set_publisher(publisher);
        txn.set_legacy_manifest(manifest_text);

        let temp_dir = tempdir_in(&self.dest.path).map_err(RepositoryError::IoError)?;

        let payload_files: Vec<_> = manifest
            .files
            .iter()
            .filter(|f| f.payload.is_some())
            .collect();
        // Deduplicate payloads by digest — multiple file actions may reference the
        // same payload, and parallel downloads to the same temp path cause races
        let mut unique_digests: Vec<String> = payload_files
            .iter()
            .map(|f| f.payload.as_ref().unwrap().primary_identifier.hash.clone())
            .collect();
        unique_digests.sort();
        unique_digests.dedup();

        // Download unique payloads in parallel
        let files_done = Arc::new(Mutex::new(0u64));
        let publisher_str = publisher.to_string();
        let fmri_name = fmri.name.clone();
        let temp_dir_path = temp_dir.path().to_path_buf();
        let unique_total = unique_digests.len() as u64;

        let download_results: std::result::Result<Vec<_>, RepositoryError> = unique_digests
            .par_iter()
            .map(|digest| {
                let temp_file_path = temp_dir_path.join(digest);

                debug!(
                    "Fetching payload {} to {}",
                    digest,
                    temp_file_path.display()
                );

                self.source
                    .fetch_payload(&publisher_str, digest, &temp_file_path)?;

                let current_count = {
                    let mut count = files_done.lock()
                        .map_err(|e| RepositoryError::Other(format!("Failed to lock progress counter: {}", e)))?;
                    *count += 1;
                    *count
                };

                progress.update(
                    &ProgressInfo::new(format!("Receiving payloads for {}", fmri_name))
                        .with_total(unique_total)
                        .with_current(current_count)
                        .with_context(format!("Payload: {}", digest)),
                );

                Ok(())
            })
            .collect();

        download_results?;

        // Add all files to the transaction, referencing the downloaded payloads
        for file in &payload_files {
            let digest = &file.payload.as_ref().unwrap().primary_identifier.hash;
            let temp_file_path = temp_dir_path.join(digest);
            txn.add_file((*file).clone(), &temp_file_path)?;
        }

        // Fetch signature payloads
        for sig in &manifest.signatures {
            // In IPS, signature actions have the digest of the actual signature payload in 'value'.
            // The 'chash' field usually contains the digest of the manifest itself or is empty.
            let digest = if !sig.value.is_empty() {
                &sig.value
            } else if !sig.chash.is_empty() {
                &sig.chash
            } else {
                continue;
            };

            let temp_file_path = temp_dir.path().join(format!("sig-{}", digest));
            debug!(
                "Fetching signature payload {} to {}",
                digest,
                temp_file_path.display()
            );

            self.source
                .fetch_payload(publisher, digest, &temp_file_path)?;

            info!("Successfully fetched signature payload {}", digest);

            // Store the signature payload in the destination repository's file store.
            // Signature payloads are identified by their digest (from 'value' or 'chash').
            // We store them in the same way as other files in the FileBackend.
            let dest_path = crate::repository::FileBackend::construct_file_path_with_publisher(
                &self.dest.path,
                publisher,
                digest,
            );

            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent).map_err(RepositoryError::IoError)?;
            }

            // Also check the global location as a fallback
            let global_dest_path = crate::repository::FileBackend::construct_file_path(
                &self.dest.path,
                digest,
            );

            if !dest_path.exists() && !global_dest_path.exists() {
                debug!(
                    "Storing signature payload {} to {}",
                    digest,
                    dest_path.display()
                );
                std::fs::rename(&temp_file_path, &dest_path).map_err(|e| {
                    RepositoryError::FileRenameError {
                        from: temp_file_path.clone(),
                        to: dest_path,
                        source: e,
                    }
                })?;
            }
        }

        txn.update_manifest(manifest.clone());
        txn.commit()?;

        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::Attr;
    use crate::repository::{FileBackend, RepositoryVersion};
    use tempfile::tempdir;

    #[test]
    fn test_receive_basic() -> Result<()> {
        let source_dir = tempdir().map_err(RepositoryError::IoError)?;
        let dest_dir = tempdir().map_err(RepositoryError::IoError)?;

        // Create source repo with one package
        let mut source_repo = FileBackend::create(source_dir.path(), RepositoryVersion::V4)?;
        source_repo.add_publisher("test")?;

        let fmri = Fmri::parse("pkg://test/pkgA@1.0").unwrap();
        let mut manifest = Manifest::new();
        manifest.attributes.push(Attr {
            key: "pkg.fmri".to_string(),
            values: vec![fmri.to_string()],
            ..Default::default()
        });

        let mut txn = source_repo.begin_transaction()?;
        txn.set_publisher("test");
        txn.update_manifest(manifest);
        txn.commit()?;
        source_repo.rebuild(Some("test"), false, false)?;

        // Create dest repo
        let dest_repo = FileBackend::create(dest_dir.path(), RepositoryVersion::V4)?;

        let mut receiver = PackageReceiver::new(&source_repo, dest_repo);
        receiver.receive(Some("test"), &[Fmri::new("pkgA")], false)?;

        // Verify dest repo has the package
        let dest_repo_check = FileBackend::open(dest_dir.path())?;
        let pkgs = dest_repo_check.list_packages(Some("test"), Some("pkgA"))?;
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].fmri.name, "pkgA");
        assert_eq!(pkgs[0].fmri.version.as_ref().unwrap().release, "1.0");

        Ok(())
    }

    #[test]
    fn test_receive_preserves_manifest_format() -> Result<()> {
        let source_dir = tempdir().map_err(RepositoryError::IoError)?;
        let dest_dir = tempdir().map_err(RepositoryError::IoError)?;

        // Create source repo
        let mut source_repo = FileBackend::create(source_dir.path(), RepositoryVersion::V4)?;
        source_repo.add_publisher("test")?;

        let _fmri = Fmri::parse("pkg://test/pkgA@1.0").unwrap();
        let manifest_content =
            "set name=pkg.fmri value=pkg://test/pkgA@1.0\nset name=pkg.summary value=test\n";

        // Manually write the manifest in IPS format to the source repo
        let manifest_path =
            FileBackend::construct_manifest_path(source_dir.path(), "test", "pkgA", "1.0");
        std::fs::create_dir_all(manifest_path.parent().unwrap())
            .map_err(RepositoryError::IoError)?;
        std::fs::write(&manifest_path, manifest_content).map_err(RepositoryError::IoError)?;

        // Rebuild source repo to recognize the package
        source_repo.rebuild(Some("test"), false, false)?;

        // Create dest repo
        let dest_repo = FileBackend::create(dest_dir.path(), RepositoryVersion::V4)?;

        let mut receiver = PackageReceiver::new(&source_repo, dest_repo);
        receiver.receive(Some("test"), &[Fmri::new("pkgA")], false)?;

        // Verify dest repo has the package and the manifest is in IPS format
        let dest_manifest_path =
            FileBackend::construct_manifest_path(dest_dir.path(), "test", "pkgA", "1.0");
        let content =
            std::fs::read_to_string(&dest_manifest_path).map_err(RepositoryError::IoError)?;

        assert_eq!(content, manifest_content);
        assert!(!content.starts_with('{'), "Manifest should not be JSON");

        // Also verify the .json version exists and IS JSON
        let mut json_path = dest_manifest_path.clone();
        let mut filename = json_path.file_name().unwrap().to_os_string();
        filename.push(".json");
        json_path.set_file_name(filename);
        assert!(
            json_path.exists(),
            "JSON manifest should exist at {}",
            json_path.display()
        );
        let json_content = std::fs::read_to_string(&json_path).map_err(RepositoryError::IoError)?;
        assert!(
            json_content.starts_with('{'),
            "JSON manifest should be JSON"
        );

        Ok(())
    }

    #[test]
    fn test_receive_with_signature() -> Result<()> {
        let source_dir = tempdir().map_err(RepositoryError::IoError)?;
        let dest_dir = tempdir().map_err(RepositoryError::IoError)?;

        // Create source repo with one package having a signature
        let mut source_repo = FileBackend::create(source_dir.path(), RepositoryVersion::V4)?;
        source_repo.add_publisher("test")?;

        let fmri = Fmri::parse("pkg://test/pkgA@1.0").unwrap();
        let mut manifest = Manifest::new();
        manifest.attributes.push(Attr {
            key: "pkg.fmri".to_string(),
            values: vec![fmri.to_string()],
            ..Default::default()
        });

        // Create the signature payload in the source repo
        // FileBackend::fetch_payload expects SHA1 by default if no prefix is provided.
        use sha1::{Digest as Sha1Digest, Sha1};
        let payload_content = b"fake-signature-payload-content";
        let mut hasher = Sha1::new();
        hasher.update(payload_content);
        let payload_hash = format!("{:x}", hasher.finalize());

        let payload_dir = source_dir.path().join("publisher/test/file/27");
        std::fs::create_dir_all(&payload_dir).map_err(RepositoryError::IoError)?;
        std::fs::write(payload_dir.join(&payload_hash), payload_content)
            .map_err(RepositoryError::IoError)?;

        let mut sig = crate::actions::Signature::default();
        sig.algorithm = "rsa-sha256".to_string();
        sig.value = payload_hash.clone();
        sig.chash = "fake-manifest-hash".to_string();
        manifest.signatures.push(sig);

        let mut txn = source_repo.begin_transaction()?;
        txn.set_publisher("test");
        txn.update_manifest(manifest);
        txn.commit()?;
        source_repo.rebuild(Some("test"), false, false)?;

        // Create dest repo
        let dest_repo = FileBackend::create(dest_dir.path(), RepositoryVersion::V4)?;

        let mut receiver = PackageReceiver::new(&source_repo, dest_repo);
        receiver.receive(Some("test"), &[Fmri::new("pkgA")], false)?;

        // Verify dest repo has the signature payload in the correct location
        let expected_sig_path = FileBackend::construct_file_path_with_publisher(
            dest_dir.path(),
            "test",
            &payload_hash,
        );
        let global_sig_path = FileBackend::construct_file_path(
            dest_dir.path(),
            &payload_hash,
        );
        let final_path = if expected_sig_path.exists() { expected_sig_path } else { global_sig_path };
        let content = std::fs::read(&final_path).unwrap();
        assert_eq!(content, payload_content);

        Ok(())
    }

    #[test]
    fn test_receive_multi_file_package() -> Result<()> {
        use crate::actions::File as FileAction;

        let source_dir = tempdir().map_err(RepositoryError::IoError)?;
        let dest_dir = tempdir().map_err(RepositoryError::IoError)?;

        // Create source repo
        let mut source_repo = FileBackend::create(source_dir.path(), RepositoryVersion::V4)?;
        source_repo.add_publisher("test")?;

        // Create a prototype directory with multiple files
        let proto_dir = source_dir.path().join("proto");
        std::fs::create_dir_all(&proto_dir).map_err(RepositoryError::IoError)?;

        let file_contents: Vec<(&str, &[u8])> = vec![
            ("etc/config.txt", b"config-content-1"),
            ("usr/bin/tool", b"binary-content-2"),
            ("usr/lib/libfoo.so", b"library-content-3"),
            ("var/data/db.dat", b"data-content-4"),
            ("opt/extra/readme", b"readme-content-5"),
        ];

        let mut file_actions = Vec::new();
        for (path, content) in &file_contents {
            let full_path = proto_dir.join(path);
            std::fs::create_dir_all(full_path.parent().unwrap())
                .map_err(RepositoryError::IoError)?;
            std::fs::write(&full_path, content).map_err(RepositoryError::IoError)?;

            let mut fa = FileAction::default();
            fa.path = path.to_string();
            fa.payload = Some(crate::payload::Payload::compute_payload(&full_path).unwrap());
            file_actions.push((fa, full_path));
        }

        // Create the package with files via transaction
        let fmri = Fmri::parse("pkg://test/multifile@1.0").unwrap();
        let mut manifest = Manifest::new();
        manifest.attributes.push(Attr {
            key: "pkg.fmri".to_string(),
            values: vec![fmri.to_string()],
            ..Default::default()
        });

        let mut txn = source_repo.begin_transaction()?;
        txn.set_publisher("test");
        for (fa, path) in file_actions {
            txn.add_file(fa, &path)?;
        }
        txn.update_manifest(manifest);
        txn.commit()?;
        source_repo.rebuild(Some("test"), false, false)?;

        // Receive into destination repo
        let dest_repo = FileBackend::create(dest_dir.path(), RepositoryVersion::V4)?;
        let mut receiver = PackageReceiver::new(&source_repo, dest_repo);
        receiver.receive(Some("test"), &[Fmri::new("multifile")], false)?;

        // Verify destination has the package
        let dest_repo_check = FileBackend::open(dest_dir.path())?;
        let pkgs = dest_repo_check.list_packages(Some("test"), Some("multifile"))?;
        assert_eq!(pkgs.len(), 1, "Package should exist in destination");

        // Verify the manifest contains all files
        let dest_manifest = dest_repo_check.fetch_manifest(
            "test",
            &Fmri::parse("pkg://test/multifile@1.0").unwrap(),
        )?;
        assert_eq!(
            dest_manifest.files.len(),
            file_contents.len(),
            "All {} files should be in the manifest",
            file_contents.len()
        );

        // Verify payload files exist in the destination repo's file store.
        // Each file has both a compressed-hash entry and a primary-hash hardlink,
        // so total file count is 2x the number of payload files.
        let file_store = dest_dir.path().join("publisher/test/file");
        let mut payload_count = 0;
        if file_store.exists() {
            for bucket in std::fs::read_dir(&file_store).unwrap() {
                let bucket = bucket.unwrap();
                if bucket.path().is_dir() {
                    for entry in std::fs::read_dir(bucket.path()).unwrap() {
                        if entry.unwrap().path().is_file() {
                            payload_count += 1;
                        }
                    }
                }
            }
        }
        assert!(
            payload_count >= file_contents.len(),
            "At least {} payload files should exist in destination file store, found {}",
            file_contents.len(),
            payload_count
        );

        Ok(())
    }
}
