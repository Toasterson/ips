#[cfg(test)]
mod tests {
    use libips::repository::{
        FileBackend, REPOSITORY_CONFIG_FILENAME, ReadableRepository, RepositoryVersion,
        WritableRepository,
    };
    use tempfile::TempDir;

    // Each test gets its own `TempDir` so tests can run in parallel under
    // `cargo nextest run` without trampling each other's scratch state.
    fn make_tempdir() -> TempDir {
        tempfile::Builder::new()
            .prefix("pkg6repo-test-")
            .tempdir()
            .expect("failed to create temp dir for test")
    }

    #[test]
    fn test_create_repository() {
        let test_dir = make_tempdir();
        let repo_path = test_dir.path().join("repo");

        let _ = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();

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
    }

    #[test]
    fn test_remove_publisher() {
        let test_dir = make_tempdir();
        let repo_path = test_dir.path().join("repo");

        let mut repo = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();

        repo.add_publisher("example.com").unwrap();

        assert!(repo.config.publishers.contains(&"example.com".to_string()));

        let catalog_dir = FileBackend::construct_catalog_path(&repo_path, "example.com");
        let pkg_dir = FileBackend::construct_package_dir(&repo_path, "example.com", "");
        assert!(
            catalog_dir.exists(),
            "Catalog directory should exist after adding publisher"
        );
        assert!(
            pkg_dir.exists(),
            "Package directory should exist after adding publisher"
        );

        repo.remove_publisher("example.com", false).unwrap();

        assert!(!repo.config.publishers.contains(&"example.com".to_string()));

        assert!(
            !catalog_dir.exists(),
            "Catalog directory should not exist after removing publisher"
        );
        assert!(
            !pkg_dir.exists(),
            "Package directory should not exist after removing publisher"
        );
    }

    #[test]
    fn test_set_property() {
        let test_dir = make_tempdir();
        let repo_path = test_dir.path().join("repo");

        let mut repo = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();

        repo.set_property("publisher/prefix", "example.com")
            .unwrap();

        assert_eq!(
            repo.config.properties.get("publisher/prefix").unwrap(),
            "example.com"
        );
    }

    #[test]
    fn test_get_info() {
        let test_dir = make_tempdir();
        let repo_path = test_dir.path().join("repo");

        let mut repo = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();

        repo.add_publisher("example.com").unwrap();

        let repo_info = repo.get_info().unwrap();

        assert_eq!(repo_info.publishers.len(), 1);
        let publisher_info = &repo_info.publishers[0];
        assert_eq!(publisher_info.name, "example.com");
        assert_eq!(publisher_info.package_count, 0);
        assert_eq!(publisher_info.status, "online");
    }
}
