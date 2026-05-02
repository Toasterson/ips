//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

#[cfg(test)]
mod tests {
    use crate::actions::Manifest;
    use crate::fmri::Fmri;
    use crate::repository::{
        CatalogManager, FileBackend, ProgressInfo, ProgressReporter, REPOSITORY_CONFIG_FILENAME,
        ReadableRepository, RepositoryError, RepositoryVersion, RestBackend, Result,
        WritableRepository,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    // Each test gets its own `TempDir` so tests can run in parallel under
    // `cargo nextest run` without trampling each other's scratch state.
    fn make_tempdir() -> TempDir {
        tempfile::Builder::new()
            .prefix("libips-repo-test-")
            .tempdir()
            .expect("failed to create temp dir for test")
    }

    // Build the prototype tree and manifest files used by the publish-oriented
    // tests directly inside the caller-provided base directory. This replaces
    // the previous `cargo run -p xtask -- setup-test-env` invocation, which
    // wrote to a shared `/tmp/pkg6_test/` and caused collisions when tests
    // ran in parallel.
    fn create_fixtures(base: &Path) -> (PathBuf, PathBuf) {
        let prototype_dir = base.join("prototype");
        let manifest_dir = base.join("manifests");

        fs::create_dir_all(prototype_dir.join("usr/bin")).unwrap();
        fs::create_dir_all(prototype_dir.join("usr/share/doc/example")).unwrap();
        fs::create_dir_all(prototype_dir.join("etc/config")).unwrap();
        fs::create_dir_all(&manifest_dir).unwrap();

        fs::write(
            prototype_dir.join("usr/bin/hello"),
            "#!/bin/sh\necho 'Hello, World!'",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let hello = prototype_dir.join("usr/bin/hello");
            let mut perms = fs::metadata(&hello).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&hello, perms).unwrap();
        }

        fs::write(
            prototype_dir.join("usr/share/doc/example/README.txt"),
            "This is an example document.",
        )
        .unwrap();
        fs::write(
            prototype_dir.join("etc/config/example.conf"),
            "# Example configuration file\nvalue=42",
        )
        .unwrap();

        let example_manifest = r#"set name=pkg.fmri value=pkg://test/example@1.0.0
set name=pkg.summary value="Example package for testing"
set name=pkg.description value="This is an example package used for testing the repository implementation."
set name=info.classification value="org.opensolaris.category.2008:System/Core"
set name=variant.arch value=i386 value=sparc
file path=usr/bin/hello mode=0755 owner=root group=bin
file path=usr/share/doc/example/README.txt mode=0644 owner=root group=bin
file path=etc/config/example.conf mode=0644 owner=root group=bin preserve=true
dir path=usr/bin mode=0755 owner=root group=bin
dir path=usr/share/doc/example mode=0755 owner=root group=bin
dir path=etc/config mode=0755 owner=root group=sys
"#;
        fs::write(manifest_dir.join("example.p5m"), example_manifest).unwrap();

        let example2_manifest = r#"set name=pkg.fmri value=pkg://test/example2@1.0.0
set name=pkg.summary value="Second example package for testing"
set name=pkg.description value="This is a second example package used for testing the repository implementation."
set name=info.classification value="org.opensolaris.category.2008:System/Core"
set name=variant.arch value=i386 value=sparc
file path=usr/bin/hello mode=0755 owner=root group=bin
file path=usr/share/doc/example/README.txt mode=0644 owner=root group=bin
dir path=usr/bin mode=0755 owner=root group=bin
dir path=usr/share/doc/example mode=0755 owner=root group=bin
"#;
        fs::write(manifest_dir.join("example2.p5m"), example2_manifest).unwrap();

        (prototype_dir, manifest_dir)
    }

    // Helper function to publish a package to a repository
    fn publish_package(
        repo: &mut FileBackend,
        manifest_path: &PathBuf,
        prototype_dir: &PathBuf,
        publisher: &str,
    ) -> Result<()> {
        println!(
            "Publishing package from manifest: {}",
            manifest_path.display()
        );
        println!("Prototype directory: {}", prototype_dir.display());
        println!("Publisher: {}", publisher);

        // Check if the manifest file exists
        if !manifest_path.exists() {
            println!("Error: Manifest file does not exist");
            return Err(RepositoryError::NotFound(format!(
                "Manifest file does not exist: {}",
                manifest_path.display()
            )));
        }

        // Check if the prototype directory exists
        if !prototype_dir.exists() {
            println!("Error: Prototype directory does not exist");
            return Err(RepositoryError::NotFound(format!(
                "Prototype directory does not exist: {}",
                prototype_dir.display()
            )));
        }

        // Parse the manifest file
        println!("Parsing manifest file...");
        let manifest = Manifest::parse_file(manifest_path)?;
        println!(
            "Manifest parsed successfully. Files: {}",
            manifest.files.len()
        );

        // Begin a transaction
        println!("Beginning transaction...");
        let mut transaction = repo.begin_transaction()?;

        // Add files from the prototype directory to the transaction
        println!("Adding files to transaction...");
        for file_action in manifest.files.iter() {
            // Construct the full path to the file in the prototype directory
            let file_path = prototype_dir.join(&file_action.path);

            // Check if the file exists
            if !file_path.exists() {
                println!(
                    "Warning: File does not exist in prototype directory: {}",
                    file_path.display()
                );
                continue;
            }

            // Add the file to the transaction
            println!("Adding file: {}", file_action.path);
            transaction.add_file(file_action.clone(), &file_path)?;
        }

        // Update the manifest in the transaction
        println!("Updating manifest in transaction...");
        transaction.update_manifest(manifest);

        // Set the publisher for the transaction
        println!("Setting publisher: {}", publisher);
        transaction.set_publisher(publisher);

        // Commit the transaction
        println!("Committing transaction...");
        transaction.commit()?;
        println!("Transaction committed successfully");

        // Debug: Check if the package manifest was stored in the correct location
        let publisher_pkg_dir = FileBackend::construct_package_dir(&repo.path, publisher, "");
        println!(
            "Publisher package directory: {}",
            publisher_pkg_dir.display()
        );

        if publisher_pkg_dir.exists() {
            println!("Publisher directory exists");

            // List files in the publisher directory
            if let Ok(entries) = std::fs::read_dir(&publisher_pkg_dir) {
                println!("Files in publisher directory:");
                for entry in entries.flatten() {
                    println!("  {}", entry.path().display());
                }
            } else {
                println!("Failed to read publisher directory");
            }
        } else {
            println!("Publisher directory does not exist");
        }

        // Rebuild the catalog
        println!("Rebuilding catalog...");
        repo.rebuild(Some(publisher), false, false)?;
        println!("Catalog rebuilt successfully");

        Ok(())
    }

    #[test]
    fn test_create_repository() {
        let test_dir = make_tempdir();
        let repo_path = test_dir.path().join("repo");

        let _repo = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();

        assert!(repo_path.exists());
        assert!(repo_path.join("publisher").exists());
        assert!(repo_path.join("index").exists());
        assert!(repo_path.join("trans").exists());
        assert!(repo_path.join(REPOSITORY_CONFIG_FILENAME).exists());
    }

    #[test]
    fn test_add_publisher() {
        let test_dir = make_tempdir();
        let repo_path = test_dir.path().join("repo");

        let mut repo = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();

        repo.add_publisher("example.com").unwrap();

        assert!(repo.config.publishers.contains(&"example.com".to_string()));
        assert!(FileBackend::construct_catalog_path(&repo_path, "example.com").exists());
        assert!(FileBackend::construct_package_dir(&repo_path, "example.com", "").exists());

        let pub_p5i_path = repo_path
            .join("publisher")
            .join("example.com")
            .join("pub.p5i");
        assert!(
            pub_p5i_path.exists(),
            "pub.p5i file should be created for backward compatibility"
        );

        let pub_p5i_content = fs::read_to_string(&pub_p5i_path).unwrap();
        let pub_p5i_json: serde_json::Value = serde_json::from_str(&pub_p5i_content).unwrap();

        assert_eq!(pub_p5i_json["version"], 1);
        assert!(pub_p5i_json["packages"].is_array());
        assert!(pub_p5i_json["publishers"].is_array());
        assert_eq!(pub_p5i_json["publishers"][0]["name"], "example.com");
    }

    #[test]
    fn test_catalog_manager() {
        let test_dir = make_tempdir();
        let publisher_dir = test_dir.path().join("publisher");
        let publisher_name = "test";
        let catalog_dir = publisher_dir.join(publisher_name).join("catalog");

        fs::create_dir_all(&catalog_dir).unwrap();

        let mut catalog_manager = CatalogManager::new(&publisher_dir, publisher_name).unwrap();

        catalog_manager.create_part("test_part");

        let fmri = Fmri::parse("pkg://test/example@1.0.0").unwrap();
        catalog_manager
            .add_package_to_part("test_part", &fmri, None, None)
            .unwrap();

        catalog_manager.save_part("test_part").unwrap();

        assert!(publisher_dir.join("test_part").exists());

        let mut new_catalog_manager = CatalogManager::new(&publisher_dir, publisher_name).unwrap();
        new_catalog_manager.load_part("test_part").unwrap();

        let loaded_part = new_catalog_manager.get_part("test_part").unwrap();
        assert_eq!(loaded_part.packages.len(), 1);
    }

    #[test]
    fn test_publish_files() {
        let test_dir = make_tempdir();
        let (prototype_dir, manifest_dir) = create_fixtures(test_dir.path());
        let repo_path = test_dir.path().join("repo");

        let mut repo = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();

        repo.add_publisher("test").unwrap();

        let manifest_path = manifest_dir.join("example.p5m");
        publish_package(&mut repo, &manifest_path, &prototype_dir, "test").unwrap();

        assert!(
            repo_path
                .join("publisher")
                .join("test")
                .join("file")
                .exists()
        );

        let repo_info = repo.get_info().unwrap();

        assert_eq!(repo_info.publishers.len(), 1);
        let publisher_info = &repo_info.publishers[0];
        assert_eq!(publisher_info.name, "test");
    }

    #[test]
    fn test_list_packages() {
        let test_dir = make_tempdir();
        let (prototype_dir, manifest_dir) = create_fixtures(test_dir.path());
        let repo_path = test_dir.path().join("repo");

        let mut repo = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();

        repo.add_publisher("test").unwrap();

        let manifest_path = manifest_dir.join("example.p5m");
        publish_package(&mut repo, &manifest_path, &prototype_dir, "test").unwrap();

        let packages = repo.list_packages(Some("test"), None).unwrap();

        assert!(!packages.is_empty());

        assert_eq!(packages[0].fmri.name, "example");
    }

    #[test]
    fn test_show_contents() {
        let test_dir = make_tempdir();
        let (prototype_dir, manifest_dir) = create_fixtures(test_dir.path());
        let repo_path = test_dir.path().join("repo");

        let mut repo = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();

        repo.add_publisher("test").unwrap();

        let manifest_path = manifest_dir.join("example.p5m");
        publish_package(&mut repo, &manifest_path, &prototype_dir, "test").unwrap();

        let contents = repo.show_contents(Some("test"), None, None).unwrap();

        assert!(!contents.is_empty());

        let package_contents = &contents[0];
        assert!(package_contents.files.is_some());
        let files = package_contents.files.as_ref().unwrap();

        assert!(files.iter().any(|f| f.contains("usr/bin/hello")));
        assert!(
            files
                .iter()
                .any(|f| f.contains("usr/share/doc/example/README.txt"))
        );
        assert!(files.iter().any(|f| f.contains("etc/config/example.conf")));
    }

    #[test]
    fn test_search() {
        let test_dir = make_tempdir();
        let (prototype_dir, manifest_dir) = create_fixtures(test_dir.path());
        let repo_path = test_dir.path().join("repo");

        let mut repo = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();

        repo.add_publisher("test").unwrap();

        let manifest_path = manifest_dir.join("example.p5m");
        publish_package(&mut repo, &manifest_path, &prototype_dir, "test").unwrap();

        repo.rebuild(Some("test"), false, false).unwrap();

        let results = repo.search("example", Some("test"), None).unwrap();

        assert!(!results.is_empty());

        assert_eq!(results[0].fmri.name, "example");
    }

    #[test]
    fn test_file_structure() {
        let test_dir = make_tempdir();
        let repo_path = test_dir.path().join("repo");

        let mut repo = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();

        repo.add_publisher("test").unwrap();

        let test_file_path = test_dir.path().join("test_file.txt");
        fs::write(&test_file_path, "This is a test file").unwrap();

        let hash = repo.store_file(&test_file_path, "test").unwrap();

        let expected_path =
            FileBackend::construct_file_path_with_publisher(&repo_path, "test", &hash);

        assert!(
            expected_path.exists(),
            "File was not stored at the expected path: {}",
            expected_path.display()
        );

        let old_path = repo_path.join("file").join(&hash);
        assert!(
            !old_path.exists(),
            "File was stored at the old path: {}",
            old_path.display()
        );
    }

    #[test]
    fn test_transaction_pub_p5i_creation() {
        let test_dir = make_tempdir();
        let (_prototype_dir, manifest_dir) = create_fixtures(test_dir.path());
        let repo_path = test_dir.path().join("repo");

        let repo = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();

        let publisher = "transaction_test";

        let mut transaction = repo.begin_transaction().unwrap();

        transaction.set_publisher(publisher);

        let manifest_path = manifest_dir.join("example.p5m");
        let manifest = Manifest::parse_file(&manifest_path).unwrap();
        transaction.update_manifest(manifest);

        transaction.commit().unwrap();

        let pub_p5i_path = repo_path.join("publisher").join(publisher).join("pub.p5i");
        assert!(
            pub_p5i_path.exists(),
            "pub.p5i file should be created for new publisher in transaction"
        );

        let pub_p5i_content = fs::read_to_string(&pub_p5i_path).unwrap();
        let pub_p5i_json: serde_json::Value = serde_json::from_str(&pub_p5i_content).unwrap();

        assert_eq!(pub_p5i_json["version"], 1);
        assert!(pub_p5i_json["packages"].is_array());
        assert!(pub_p5i_json["publishers"].is_array());
        assert_eq!(pub_p5i_json["publishers"][0]["name"], publisher);
    }

    #[test]
    fn test_legacy_pkg5_repository_creation() {
        let test_dir = make_tempdir();
        let repo_path = test_dir.path().join("repo");

        let mut repo = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();

        let publisher = "openindiana.org";
        repo.add_publisher(publisher).unwrap();

        repo.set_default_publisher(publisher).unwrap();

        let pkg5_repo_path = repo_path.join("pkg5.repository");
        assert!(
            pkg5_repo_path.exists(),
            "pkg5.repository file should be created for backward compatibility"
        );

        let pkg5_content = fs::read_to_string(&pkg5_repo_path).unwrap();

        println!("pkg5.repository content:\n{}", pkg5_content);

        assert!(pkg5_content.contains("[publisher]"));
        assert!(pkg5_content.contains("prefix=openindiana.org"));
        assert!(pkg5_content.contains("[repository]"));
        assert!(pkg5_content.contains("version=4"));
        assert!(pkg5_content.contains("trust-anchor-directory=/etc/certs/CA/"));
        assert!(pkg5_content.contains("signature-required-names=[]"));
        assert!(pkg5_content.contains("check-certificate-revocation=False"));
        assert!(pkg5_content.contains("[CONFIGURATION]"));
    }

    #[test]
    fn test_rest_repository_local_functionality() {
        use crate::repository::RestBackend;

        let test_dir = make_tempdir();
        let cache_path = test_dir.path().join("cache");

        println!("Test directory: {}", test_dir.path().display());
        println!("Cache path: {}", cache_path.display());

        let uri = "http://pkg.opensolaris.org/release";
        let mut repo = RestBackend::open(uri).unwrap();

        repo.set_local_cache_path(&cache_path).unwrap();

        println!("Local cache path set to: {:?}", repo.local_cache_path);

        let publisher = "openindiana.org";
        repo.add_publisher(publisher).unwrap();

        println!("Publisher added: {}", publisher);
        println!("Publishers in config: {:?}", repo.config.publishers);

        let publisher_dir = cache_path.join("publisher").join(publisher);
        println!("Publisher directory: {}", publisher_dir.display());
        println!("Publisher directory exists: {}", publisher_dir.exists());

        assert!(
            publisher_dir.exists(),
            "Publisher directory should be created"
        );

        let catalog_dir = publisher_dir.join("catalog");
        println!("Catalog directory: {}", catalog_dir.display());
        println!("Catalog directory exists: {}", catalog_dir.exists());

        assert!(catalog_dir.exists(), "Catalog directory should be created");
    }

    /// A test progress reporter that records all progress events
    #[derive(Debug, Clone)]
    struct TestProgressReporter {
        /// Records of all start events
        start_events: Arc<Mutex<Vec<ProgressInfo>>>,
        /// Records of all update events
        update_events: Arc<Mutex<Vec<ProgressInfo>>>,
        /// Records of all finish events
        finish_events: Arc<Mutex<Vec<ProgressInfo>>>,
    }

    impl TestProgressReporter {
        /// Create a new test progress reporter
        fn new() -> Self {
            TestProgressReporter {
                start_events: Arc::new(Mutex::new(Vec::new())),
                update_events: Arc::new(Mutex::new(Vec::new())),
                finish_events: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Get the number of start events recorded
        fn start_count(&self) -> usize {
            self.start_events.lock().unwrap().len()
        }

        /// Get the number of update events recorded
        fn update_count(&self) -> usize {
            self.update_events.lock().unwrap().len()
        }

        /// Get the number of finish events recorded
        fn finish_count(&self) -> usize {
            self.finish_events.lock().unwrap().len()
        }

        /// Get a clone of all start events
        fn get_start_events(&self) -> Vec<ProgressInfo> {
            self.start_events.lock().unwrap().clone()
        }

        /// Get a clone of all update events
        fn get_update_events(&self) -> Vec<ProgressInfo> {
            self.update_events.lock().unwrap().clone()
        }

        /// Get a clone of all finish events
        fn get_finish_events(&self) -> Vec<ProgressInfo> {
            self.finish_events.lock().unwrap().clone()
        }
    }

    impl ProgressReporter for TestProgressReporter {
        fn start(&self, info: &ProgressInfo) {
            let mut events = self.start_events.lock().unwrap();
            events.push(info.clone());
        }

        fn update(&self, info: &ProgressInfo) {
            let mut events = self.update_events.lock().unwrap();
            events.push(info.clone());
        }

        fn finish(&self, info: &ProgressInfo) {
            let mut events = self.finish_events.lock().unwrap();
            events.push(info.clone());
        }
    }

    #[test]
    fn test_progress_reporter() {
        // Create a test progress reporter
        let reporter = TestProgressReporter::new();

        // Create some progress info
        let info1 = ProgressInfo::new("Test operation 1");
        let info2 = ProgressInfo::new("Test operation 2")
            .with_current(50)
            .with_total(100);

        // Report some progress
        reporter.start(&info1);
        reporter.update(&info2);
        reporter.finish(&info1);

        // Check that the events were recorded
        assert_eq!(reporter.start_count(), 1);
        assert_eq!(reporter.update_count(), 1);
        assert_eq!(reporter.finish_count(), 1);

        // Check the content of the events
        let start_events = reporter.get_start_events();
        let update_events = reporter.get_update_events();
        let finish_events = reporter.get_finish_events();

        assert_eq!(start_events[0].operation, "Test operation 1");
        assert_eq!(update_events[0].operation, "Test operation 2");
        assert_eq!(update_events[0].current, Some(50));
        assert_eq!(update_events[0].total, Some(100));
        assert_eq!(finish_events[0].operation, "Test operation 1");
    }

    #[test]
    fn test_rest_backend_with_progress() {
        // This test is a mock test that doesn't actually connect to a remote server
        // It just verifies that the progress reporting mechanism works correctly

        let test_dir = make_tempdir();
        let cache_path = test_dir.path().join("cache");

        let uri = "http://pkg.opensolaris.org/release";
        let mut repo = RestBackend::create(uri, RepositoryVersion::V4).unwrap();

        repo.set_local_cache_path(&cache_path).unwrap();

        let reporter = TestProgressReporter::new();

        let publisher = "test";
        repo.add_publisher(publisher).unwrap();

        let publisher_dir = cache_path.join("publisher").join(publisher);
        let catalog_dir = publisher_dir.join("catalog");
        fs::create_dir_all(&catalog_dir).unwrap();

        let attrs_content = r#"{
            "created": "20250803T124900Z",
            "last-modified": "20250803T124900Z",
            "package-count": 100,
            "package-version-count": 200,
            "parts": {
                "catalog.base.C": {
                    "last-modified": "20250803T124900Z"
                },
                "catalog.dependency.C": {
                    "last-modified": "20250803T124900Z"
                },
                "catalog.summary.C": {
                    "last-modified": "20250803T124900Z"
                }
            },
            "version": 1
        }"#;

        let attrs_path = catalog_dir.join("catalog.attrs");
        fs::write(&attrs_path, attrs_content).unwrap();

        for part_name in [
            "catalog.base.C",
            "catalog.dependency.C",
            "catalog.summary.C",
        ] {
            let part_path = catalog_dir.join(part_name);
            fs::write(&part_path, "{}").unwrap();
        }

        let progress_info = ProgressInfo::new("Test update")
            .with_current(1)
            .with_total(2);
        reporter.update(&progress_info);

        let _ = repo.download_catalog(publisher, Some(&reporter));

        assert!(reporter.start_count() > 0, "No start events recorded");
        assert!(reporter.update_count() > 0, "No update events recorded");
        assert!(reporter.finish_count() > 0, "No finish events recorded");
    }
}
