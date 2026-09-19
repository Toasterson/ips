//  This Source Code Form is subject to the terms of
//  the Mozilla Public License, v. 2.0. If a copy of the
//  MPL was not distributed with this file, You can
//  obtain one at https://mozilla.org/MPL/2.0/.

#[cfg(test)]
mod tests {
    use crate::actions::{Attr, File as FileAction, Manifest};
    use crate::fmri::Fmri;
    use crate::repository::{
        ArchiveBackend, ArchiveWriter, FileBackend, ReadableRepository, RepositoryVersion,
        WritableRepository,
    };
    use tempfile::tempdir;

    #[test]
    fn test_archive_metadata_round_trip() {
        use crate::repository::archive_writer::{ArchiveMetadata, ArchivePackageEntry};

        let metadata = ArchiveMetadata {
            version: 1,
            format: "pkg6p".to_string(),
            created: "2026-03-17T00:00:00Z".to_string(),
            publishers: vec!["test".to_string()],
            default_publisher: Some("test".to_string()),
            packages: vec![ArchivePackageEntry {
                fmri: "pkg://test/pkgA@1.0".to_string(),
                publisher: "test".to_string(),
                manifest_path: "publisher/test/pkg/pkgA/1.0".to_string(),
                file_hashes: vec!["abc123".to_string()],
                obsolete: false,
                renamed: false,
                summary: Some("Test package".to_string()),
            }],
            total_packages: 1,
            total_files: 1,
        };

        let json = serde_json::to_string_pretty(&metadata).unwrap();
        let parsed: ArchiveMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.format, "pkg6p");
        assert_eq!(parsed.packages.len(), 1);
        assert_eq!(parsed.packages[0].fmri, "pkg://test/pkgA@1.0");
        assert_eq!(parsed.packages[0].summary, Some("Test package".to_string()));
    }

    #[test]
    fn test_archive_create_and_read() {
        let source_dir = tempdir().unwrap();
        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("test.p6p");

        // Create source repo with a package
        let mut source_repo =
            FileBackend::create(source_dir.path(), RepositoryVersion::V4).unwrap();
        source_repo.add_publisher("test").unwrap();

        let fmri = Fmri::parse("pkg://test/pkgA@1.0").unwrap();
        let mut manifest = Manifest::new();
        manifest.attributes.push(Attr {
            key: "pkg.fmri".to_string(),
            values: vec![fmri.to_string()],
            ..Default::default()
        });
        manifest.attributes.push(Attr {
            key: "pkg.summary".to_string(),
            values: vec!["Test Package A".to_string()],
            ..Default::default()
        });

        let mut txn = source_repo.begin_transaction().unwrap();
        txn.set_publisher("test");
        txn.update_manifest(manifest);
        txn.commit().unwrap();
        source_repo.rebuild(Some("test"), false, false).unwrap();

        // Create archive
        let mut writer = ArchiveWriter::create(&archive_path).unwrap();
        writer
            .add_from_repository(&source_repo, "test", None)
            .unwrap();
        writer.finish().unwrap();

        // Verify the archive is a valid ZIP
        assert!(archive_path.exists());

        // Open archive and verify contents
        let archive_repo = ArchiveBackend::open(&archive_path).unwrap();

        // Check info
        let info = archive_repo.get_info().unwrap();
        assert_eq!(info.publishers.len(), 1);
        assert_eq!(info.publishers[0].name, "test");

        // List packages
        let packages = archive_repo.list_packages(Some("test"), None).unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].fmri.name, "pkgA");

        // Fetch manifest
        let manifest = archive_repo
            .fetch_manifest("test", &Fmri::parse("pkg://test/pkgA@1.0").unwrap())
            .unwrap();
        let summary = manifest
            .attributes
            .iter()
            .find(|a| a.key == "pkg.summary")
            .and_then(|a| a.values.first().cloned());
        assert_eq!(summary, Some("Test Package A".to_string()));
    }

    #[test]
    fn test_archive_with_payloads() {
        let source_dir = tempdir().unwrap();
        let archive_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("payload_test.p6p");

        // Create source repo with a package that has files
        let mut source_repo =
            FileBackend::create(source_dir.path(), RepositoryVersion::V4).unwrap();
        source_repo.add_publisher("test").unwrap();

        // Create a prototype file
        let proto_dir = source_dir.path().join("proto");
        std::fs::create_dir_all(&proto_dir).unwrap();
        let file_path = proto_dir.join("etc/config.txt");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, b"config-content").unwrap();

        let mut fa = FileAction::default();
        fa.path = "etc/config.txt".to_string();
        fa.payload = Some(crate::payload::Payload::compute_payload(&file_path).unwrap());

        let fmri = Fmri::parse("pkg://test/withfiles@1.0").unwrap();
        let mut manifest = Manifest::new();
        manifest.attributes.push(Attr {
            key: "pkg.fmri".to_string(),
            values: vec![fmri.to_string()],
            ..Default::default()
        });

        let mut txn = source_repo.begin_transaction().unwrap();
        txn.set_publisher("test");
        txn.add_file(fa, &file_path).unwrap();
        txn.update_manifest(manifest);
        txn.commit().unwrap();
        source_repo.rebuild(Some("test"), false, false).unwrap();

        // Create archive
        let mut writer = ArchiveWriter::create(&archive_path).unwrap();
        writer
            .add_from_repository(&source_repo, "test", None)
            .unwrap();
        writer.finish().unwrap();

        // Open archive and verify payload can be fetched
        let archive_repo = ArchiveBackend::open(&archive_path).unwrap();
        let manifest = archive_repo
            .fetch_manifest("test", &Fmri::parse("pkg://test/withfiles@1.0").unwrap())
            .unwrap();
        assert_eq!(manifest.files.len(), 1);

        // Verify payload can be extracted
        let hash = &manifest.files[0]
            .payload
            .as_ref()
            .unwrap()
            .primary_identifier
            .hash;
        let payload_dest = dest_dir.path().join("extracted_payload");
        archive_repo
            .fetch_payload("test", hash, &payload_dest)
            .unwrap();
        assert!(payload_dest.exists());
    }

    #[test]
    fn test_archive_round_trip_via_recv() {
        let source_dir = tempdir().unwrap();
        let archive_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("roundtrip.p6p");

        // Create source repo
        let mut source_repo =
            FileBackend::create(source_dir.path(), RepositoryVersion::V4).unwrap();
        source_repo.add_publisher("test").unwrap();

        let fmri = Fmri::parse("pkg://test/pkgB@2.0").unwrap();
        let mut manifest = Manifest::new();
        manifest.attributes.push(Attr {
            key: "pkg.fmri".to_string(),
            values: vec![fmri.to_string()],
            ..Default::default()
        });
        manifest.attributes.push(Attr {
            key: "pkg.summary".to_string(),
            values: vec!["Package B".to_string()],
            ..Default::default()
        });

        let mut txn = source_repo.begin_transaction().unwrap();
        txn.set_publisher("test");
        txn.update_manifest(manifest);
        txn.commit().unwrap();
        source_repo.rebuild(Some("test"), false, false).unwrap();

        // Create archive from source
        let mut writer = ArchiveWriter::create(&archive_path).unwrap();
        writer
            .add_from_repository(&source_repo, "test", None)
            .unwrap();
        writer.finish().unwrap();

        // Import from archive into destination via PackageReceiver
        let archive_repo = ArchiveBackend::open(&archive_path).unwrap();
        let dest_repo = FileBackend::create(dest_dir.path(), RepositoryVersion::V4).unwrap();

        let mut receiver = crate::recv::PackageReceiver::new(&archive_repo, dest_repo);
        let fmris = archive_repo
            .list_packages(Some("test"), None)
            .unwrap()
            .into_iter()
            .map(|p| p.fmri)
            .collect::<Vec<_>>();

        receiver.receive(Some("test"), &fmris, false).unwrap();

        // Verify destination has the package
        let dest_repo = FileBackend::open(dest_dir.path()).unwrap();
        let pkgs = dest_repo.list_packages(Some("test"), Some("pkgB")).unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].fmri.name, "pkgB");
    }

    #[test]
    fn test_archive_search() {
        let source_dir = tempdir().unwrap();
        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("search_test.p6p");

        // Create source repo with multiple packages
        let mut source_repo =
            FileBackend::create(source_dir.path(), RepositoryVersion::V4).unwrap();
        source_repo.add_publisher("test").unwrap();

        for (name, summary) in [
            ("web/nginx", "Nginx Webserver"),
            ("web/apache", "Apache HTTP"),
        ] {
            let fmri = Fmri::parse(&format!("pkg://test/{}@1.0", name)).unwrap();
            let mut manifest = Manifest::new();
            manifest.attributes.push(Attr {
                key: "pkg.fmri".to_string(),
                values: vec![fmri.to_string()],
                ..Default::default()
            });
            manifest.attributes.push(Attr {
                key: "pkg.summary".to_string(),
                values: vec![summary.to_string()],
                ..Default::default()
            });

            let mut txn = source_repo.begin_transaction().unwrap();
            txn.set_publisher("test");
            txn.update_manifest(manifest);
            txn.commit().unwrap();
        }
        source_repo.rebuild(Some("test"), false, false).unwrap();

        // Create archive
        let mut writer = ArchiveWriter::create(&archive_path).unwrap();
        writer
            .add_from_repository(&source_repo, "test", None)
            .unwrap();
        writer.finish().unwrap();

        // Search
        let archive_repo = ArchiveBackend::open(&archive_path).unwrap();
        let results = archive_repo.search("nginx", Some("test"), None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].fmri.name.contains("nginx"));

        let results = archive_repo.search("web", Some("test"), None).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_archive_list_with_pattern() {
        let source_dir = tempdir().unwrap();
        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("pattern_test.p6p");

        let mut source_repo =
            FileBackend::create(source_dir.path(), RepositoryVersion::V4).unwrap();
        source_repo.add_publisher("test").unwrap();

        for name in ["lib/libfoo", "lib/libbar", "app/myapp"] {
            let fmri = Fmri::parse(&format!("pkg://test/{}@1.0", name)).unwrap();
            let mut manifest = Manifest::new();
            manifest.attributes.push(Attr {
                key: "pkg.fmri".to_string(),
                values: vec![fmri.to_string()],
                ..Default::default()
            });

            let mut txn = source_repo.begin_transaction().unwrap();
            txn.set_publisher("test");
            txn.update_manifest(manifest);
            txn.commit().unwrap();
        }
        source_repo.rebuild(Some("test"), false, false).unwrap();

        let mut writer = ArchiveWriter::create(&archive_path).unwrap();
        writer
            .add_from_repository(&source_repo, "test", None)
            .unwrap();
        writer.finish().unwrap();

        let archive_repo = ArchiveBackend::open(&archive_path).unwrap();

        // Pattern matching with wildcard
        let results = archive_repo
            .list_packages(Some("test"), Some("*lib*"))
            .unwrap();
        assert_eq!(results.len(), 2);

        let results = archive_repo
            .list_packages(Some("test"), Some("*myapp*"))
            .unwrap();
        assert_eq!(results.len(), 1);
    }
}
