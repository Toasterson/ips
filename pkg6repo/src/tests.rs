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

    fn parse(args: &[&str]) -> Result<crate::App, clap::Error> {
        use clap::Parser;
        crate::App::try_parse_from(std::iter::once("pkg6repo").chain(args.iter().copied()))
    }

    // `obsolete-package` moved a manifest out of the active repository and
    // rebuilt the catalog, which is neither pkg5 obsoletion (a published
    // manifest carrying pkg.obsolete=true that stays in the catalog) nor
    // repository retirement. It is not part of the v1 command set.
    #[test]
    fn test_obsolete_package_command_removed() {
        let err = parse(&[
            "obsolete-package",
            "-s",
            "/nonexistent",
            "-p",
            "test",
            "-f",
            "pkg://test/example@1.0",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);

        let help = parse(&["--help"]).unwrap_err().to_string();
        assert!(!help.contains("obsolete-package"), "{help}");
    }

    #[test]
    fn test_obsoleted_store_commands_still_parse() {
        for args in [
            &["list-obsoleted", "-s", "/r", "-p", "test"][..],
            &["show-obsoleted", "-s", "/r", "-p", "test", "-f", "pkg:/a@1"],
            &["search-obsoleted", "-s", "/r", "-p", "test", "-q", "a"],
            &[
                "restore-obsoleted",
                "-s",
                "/r",
                "-p",
                "test",
                "-f",
                "pkg:/a@1",
            ],
            &[
                "export-obsoleted",
                "-s",
                "/r",
                "-p",
                "test",
                "-o",
                "/o.json",
            ],
        ] {
            parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
    }

    // Removed from the v1 command set (#40): cleanup-obsoleted permanently
    // deleted store entries without dependency or snapshot checks;
    // import-obsoleted created new store entries, which hide packages
    // without pkg5 obsoletion semantics; archive/import-archive duplicated
    // pkg6recv with a pkg6-only format and silently empty results.
    #[test]
    fn test_removed_commands_are_rejected() {
        for args in [
            &["cleanup-obsoleted", "-s", "/r", "-p", "test", "--dry-run"][..],
            &["import-obsoleted", "-s", "/r", "-i", "/o.json"],
            &["archive", "-s", "/r", "-o", "/o.p6p"],
            &["import-archive", "-s", "/o.p6p", "-d", "/r"],
        ] {
            let err = parse(args).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::InvalidSubcommand,
                "{args:?}"
            );
        }
        let names: Vec<String> = <crate::App as clap::CommandFactory>::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        for removed in [
            "cleanup-obsoleted",
            "import-obsoleted",
            "import-archive",
            "archive",
        ] {
            assert!(!names.iter().any(|n| n == removed), "{removed}: {names:?}");
        }
    }

    #[test]
    fn test_repository_uris_are_rejected() {
        for args in [
            &["create", "http://example.com/repo"][..],
            &["list", "-s", "https://pkg.example.com/"],
            &["info", "-s", "file:///var/repo"],
            &["import-pkg5", "-s", "http://x/a.p5p", "-d", "/r"],
            &["import-pkg5", "-s", "/a.p5p", "-d", "file:///r"],
        ] {
            let err = parse(args).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ValueValidation,
                "{args:?}"
            );
            assert!(
                err.to_string().contains("local repository paths only"),
                "{err}"
            );
        }
        // A colon alone does not make a URI.
        parse(&["list", "-s", "/var/repo:old"]).unwrap();
    }

    #[test]
    fn test_unsupported_options_are_rejected() {
        use clap::error::ErrorKind;
        for (args, kind) in [
            // No remote repositories, so no client certificates.
            (
                &["get", "-s", "/r", "--key", "/k"][..],
                ErrorKind::UnknownArgument,
            ),
            (
                &["list", "-s", "/r", "--cert", "/c"],
                ErrorKind::UnknownArgument,
            ),
            // pkg5's contents -m prints the manifest; the old flag invented one.
            (&["contents", "-s", "/r", "-m"], ErrorKind::UnknownArgument),
            // Only indexed action types.
            (
                &["contents", "-s", "/r", "-t", "set"],
                ErrorKind::InvalidValue,
            ),
            // --no-catalog skipped the only thing rebuild does.
            (
                &["rebuild", "-s", "/r", "--no-catalog"],
                ErrorKind::UnknownArgument,
            ),
            // Single-publisher operations take -p once.
            (
                &["rebuild", "-s", "/r", "-p", "a", "-p", "b"],
                ErrorKind::ArgumentConflict,
            ),
            (
                &["search", "-s", "/r", "-p", "a", "-p", "b", "q"],
                ErrorKind::ArgumentConflict,
            ),
            // Publisher properties have no effect in v1.
            (&["get", "-s", "/r", "-p", "a"], ErrorKind::UnknownArgument),
            (
                &["set", "-s", "/r", "-p", "a", "x/y=z"],
                ErrorKind::UnknownArgument,
            ),
            (&["list", "-s", "/r", "-F", "xml"], ErrorKind::InvalidValue),
            (
                &["add-publisher", "-s", "/r"],
                ErrorKind::MissingRequiredArgument,
            ),
            (
                &["remove-publisher", "-s", "/r"],
                ErrorKind::MissingRequiredArgument,
            ),
            (&["set", "-s", "/r"], ErrorKind::MissingRequiredArgument),
        ] {
            let err = parse(args).unwrap_err();
            assert_eq!(err.kind(), kind, "{args:?}: {err}");
        }
        // Repeatable filters stay accepted where every value is honoured.
        parse(&["list", "-s", "/r", "-p", "a", "-p", "b", "x", "y"]).unwrap();
        parse(&[
            "contents", "-s", "/r", "-p", "a", "-p", "b", "-t", "file", "-t", "dir",
        ])
        .unwrap();
        parse(&["info", "-s", "/r", "-p", "a", "-p", "b"]).unwrap();
        parse(&["rebuild", "-s", "/r", "--no-index"]).unwrap();
    }

    /// Actions recorded for `stem` in one v1 catalog part of publisher `test`.
    fn catalog_part_actions(repo_path: &std::path::Path, part: &str, stem: &str) -> Vec<String> {
        let path = FileBackend::construct_catalog_path(repo_path, "test").join(part);
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        json["test"][stem]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|entry| entry["actions"].as_array().cloned().unwrap_or_default())
            .map(|a| a.as_str().unwrap().to_string())
            .collect()
    }

    // pkg5 obsoletion is carried by the manifest itself: publishing a
    // pkg.obsolete=true manifest must leave it in the active repository and
    // catalog, not divert it to obsoleted storage. Legacy pkg5 clients read
    // pkg.obsolete/pkg.renamed only from catalog.dependency.C
    // (pkg_solver.__fmri_loadstate, Image package state), so that is where
    // the flag must be published.
    #[test]
    fn test_published_pkg_obsolete_manifest_stays_in_catalog() {
        use libips::actions::{Attr, Manifest};
        use std::collections::HashMap;

        let test_dir = make_tempdir();
        let repo_path = test_dir.path().join("repo");
        let mut repo = FileBackend::create(&repo_path, RepositoryVersion::V4).unwrap();
        repo.add_publisher("test").unwrap();

        let mut tx = repo.begin_transaction().unwrap();
        tx.set_publisher("test");
        let mut manifest = Manifest::new();
        for (key, value) in [
            (
                "pkg.fmri",
                "pkg://test/example@1.0,5.11-0.2024:20240101T000000Z",
            ),
            ("pkg.obsolete", "true"),
            ("pkg.renamed", "false"),
            ("pkg.summary", "gone"),
        ] {
            manifest.attributes.push(Attr {
                key: key.to_string(),
                values: vec![value.to_string()],
                properties: HashMap::new(),
            });
        }
        tx.update_manifest(manifest);
        tx.commit().unwrap();
        repo.rebuild(Some("test"), false, false).unwrap();

        let packages = repo.list_packages(Some("test"), None).unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].fmri.stem(), "example");

        let dependency = catalog_part_actions(&repo_path, "catalog.dependency.C", "example");
        assert!(
            dependency.contains(&"set name=pkg.obsolete value=true".to_string()),
            "{dependency:?}"
        );
        assert!(
            dependency.contains(&"set name=pkg.renamed value=false".to_string()),
            "{dependency:?}"
        );
        let summary = catalog_part_actions(&repo_path, "catalog.summary.C", "example");
        assert_eq!(summary, vec!["set name=pkg.summary value=gone".to_string()]);

        let mut repo = FileBackend::open(&repo_path).unwrap();
        let obsoleted = repo.get_obsoleted_manager().unwrap();
        assert!(
            obsoleted
                .list_obsoleted_packages("test")
                .unwrap()
                .is_empty()
        );
    }
}
