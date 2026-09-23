# Obsoleted Packages in IPS

This document describes the handling of obsoleted packages in the Image Packaging System (IPS).

> **v1 status.** This document covers two different things. Keep them apart.
>
> - **pkg5 obsoletion (protocol-visible).** A package version is obsolete when
>   its published manifest carries `set name=pkg.obsolete value=true`. It
>   stays in the repository and catalog. Following pkg5
>   (`Catalog.add_package` in `src/modules/catalog.py`), the catalog writer
>   puts `pkg.obsolete` and `pkg.renamed` in `catalog.dependency.C`, the only
>   part legacy clients read them from (`pkg_solver.__fmri_loadstate` and
>   image package state). To obsolete a package, publish such a manifest.
> - **The obsoleted store (`<repository>/obsoleted/`).** A pkg6-only side
>   store, described in the rest of this document. Entries in it are *not* in
>   the catalog, so legacy clients never see them, and the store gives them
>   no `pkg.obsolete` semantics. It is not repository retirement either.
> - **Retirement (#39, ADR-010).** `pkg6repo retire` hides superseded versions
>   from catalogs and search while their manifests and payloads stay served
>   for exact requests. It never uses this store, never deletes anything and
>   never retires the newest version of a package, so an obsoletion marker
>   stays visible. See `pkg6repo-retire(8)` and `pkg6-policy(5)`.
>
> The former `pkg6repo obsolete-package` command, which moved a manifest from
> the active repository into this store, has been removed (ADR-004), and so
> have `cleanup-obsoleted` (unchecked permanent deletion) and
> `import-obsoleted` (created new store entries) (ADR-009). The remaining
> `*-obsoleted` commands inspect the store, export it, or publish an entry
> back into the repository. Nothing in v1 adds entries to the store. The pkg5 importer
> (`import-pkg5`) no longer writes to the store: `pkg.obsolete=true` packages
> are published in the main repository and catalog, as in pkg5 (#38, ADR-007).
> Entries earlier importer versions diverted here stay until they are
> re-imported or reviewed under #39/#40.

## Overview

The obsoleted store keeps a record of package versions outside the active
repository: a metadata JSON document per version (status, date, optional
deprecation message and replacements) and optionally the original manifest.
It exists because earlier importer versions placed `pkg.obsolete=true`
packages there and because earlier pkg6 builds provided `obsolete-package`.

## Directory Structure

Obsoleted packages are stored in the following directory structure:

```
<repository>/obsoleted/
    <publisher>/
        <package-stem>/
            <version>.json     # Metadata about the obsoleted package
            <version>.manifest # Original manifest of the obsoleted package
```

For example, an obsoleted package `pkg://openindiana.org/library/perl-5/postgres-dbi-5100@2.19.3,5.11-2014.0.1.1:20250628T100651Z` would be stored as:

```
<repository>/obsoleted/
    openindiana.org/
        library/perl-5/postgres-dbi-5100/
            2.19.3%2C5.11-2014.0.1.1%3A20250628T100651Z.json
            2.19.3%2C5.11-2014.0.1.1%3A20250628T100651Z.manifest
```

## Metadata Format

The metadata for an obsoleted package is stored in a JSON file with the following structure:

```json
{
  "fmri": "pkg://openindiana.org/library/perl-5/postgres-dbi-5100@2.19.3,5.11-2014.0.1.1:20250628T100651Z",
  "status": "obsolete",
  "obsolescence_date": "2025-07-29T12:22:00Z",
  "deprecation_message": "This package is deprecated. Use library/perl-5/postgres-dbi instead.",
  "obsoleted_by": [
    "pkg://openindiana.org/library/perl-5/postgres-dbi@3.0.0"
  ],
  "metadata_version": 1,
  "content_hash": "sha256-abc123def456..."
}
```

The fields in the metadata are:

- `fmri`: The full FMRI (Fault Management Resource Identifier) of the obsoleted package
- `status`: Always "obsolete" for obsoleted packages
- `obsolescence_date`: The date when the package was marked as obsoleted
- `deprecation_message`: Optional message explaining why the package was obsoleted
- `obsoleted_by`: Optional list of FMRIs that replace this package
- `metadata_version`: Version of the metadata schema (currently 1)
- `content_hash`: Hash of the original manifest content for integrity verification, or "null" if the original manifest is not stored

### Null Hash for Obsoleted Packages

For obsoleted packages that don't provide any useful information beyond the fact that they are obsoleted, the system supports a special "null hash" mode. In this mode:

1. The `content_hash` field in the metadata is set to the string "null"
2. The original manifest file is not stored, saving disk space
3. When a client requests the manifest, a minimal manifest with obsoletion attributes is generated on-the-fly

This approach is particularly useful when importing large numbers of obsoleted packages from a pkg5 repository, as it significantly reduces the storage requirements while still providing the necessary information to clients.

The minimal manifest generated for a null hash obsoleted package looks like this:

```json
{
    "attributes": [
        {
            "key": "pkg.fmri",
            "values": [
                "pkg://publisher/package@version"
            ]
        },
        {
            "key": "pkg.obsolete",
            "values": [
                "true"
            ]
        }
    ]
}
```

This contains just enough information for clients to know that the package is obsoleted.

## CLI Commands

The following CLI commands work on the obsoleted package store. The
list, show, search and export commands do not change the repository; on a
repository whose store is empty they report no entries without creating
the store index.

### List Obsoleted Packages

```bash
pkg6repo list-obsoleted -s <repository> -p <publisher> [-F <format>] [-H] [--page <page>] [--page-size <page_size>]
```

This command lists obsoleted packages for a publisher with optional pagination. The output format can be:
- `table` (default): Tabular format with columns for name, version, and publisher
- `json`: JSON format
- `tsv`: Tab-separated values

Pagination parameters:
- `--page`: Page number (1-based, defaults to 1)
- `--page-size`: Number of packages per page (defaults to 100, use 0 for all packages)

The output includes pagination information (current page, total pages, total count) in all formats.

**Example:**
```bash
# List all obsoleted packages for a publisher in JSON format
pkg6repo list-obsoleted -s /path/to/repo -p openindiana.org -F json

# List all obsoleted packages for a publisher in table format without headers
pkg6repo list-obsoleted -s /path/to/repo -p openindiana.org -H

# List obsoleted packages with pagination (page 2, 20 packages per page)
pkg6repo list-obsoleted -s /path/to/repo -p openindiana.org --page 2 --page-size 20

# List all obsoleted packages in a single page
pkg6repo list-obsoleted -s /path/to/repo -p openindiana.org --page-size 0
```

### Search Obsoleted Packages

```bash
pkg6repo search-obsoleted -s <repository> -p <publisher> -q <pattern> [-F <format>] [-H] [-n <limit>]
```

This command searches for obsoleted packages that match a pattern. The
pattern is a regular expression matched against the FMRI and the package
name (a plain substring is a valid expression). A pattern of only digits
and dots matches versions that contain it.

**Example:**
```bash
# Search for obsoleted packages containing "perl" in the name or FMRI
pkg6repo search-obsoleted -s /path/to/repo -p openindiana.org -q "perl"

# Search with a regular expression and limit results to 10
pkg6repo search-obsoleted -s /path/to/repo -p openindiana.org -q "^library/.*" -n 10
```

### Show Obsoleted Package Details

```bash
pkg6repo show-obsoleted -s <repository> -p <publisher> -f <fmri> [-F <format>]
```

This command shows detailed information about an obsoleted package, including:
- FMRI
- Status
- Obsolescence date
- Deprecation message (if any)
- Replacement packages (if any)
- Metadata version
- Content hash

**Example:**
```bash
# Show details of an obsoleted package in JSON format
pkg6repo show-obsoleted -s /path/to/repo -p openindiana.org \
  -f "pkg://openindiana.org/library/perl-5/postgres-dbi-5100@2.19.3" -F json
```

### Restore an Obsoleted Package

```bash
pkg6repo restore-obsoleted -s <repository> -p <publisher> -f <fmri> [--no-rebuild]
```

This command restores an obsoleted package to the main repository:
1. Reads the manifest from the store
2. Publishes the manifest to the main repository in a transaction
3. Removes the store entry, only after the transaction committed
4. Rebuilds the catalog (unless `--no-rebuild` is specified)

A failed publication keeps the store entry. The manifest is published as
stored. For entries imported from pkg5 with a null hash, that is the minimal
manifest described under "Null Hash for Obsoleted Packages", which carries
`pkg.obsolete=true`: restoring such an entry turns it into a proper pkg5
obsolete package that clients see. This is the migration path for entries
earlier importer versions diverted into the store.

Restoring is refused on repositories managed by `import-pkg5`, which
publish only through generations; re-import the source archive there.

**Example:**
```bash
# Restore an obsoleted package to the main repository
pkg6repo restore-obsoleted -s /path/to/repo -p openindiana.org \
  -f "pkg://openindiana.org/library/perl-5/postgres-dbi-5100@2.19.3"
```

### Export Obsoleted Packages

```bash
pkg6repo export-obsoleted -s <repository> -p <publisher> -o <output-file> [-q <pattern>]
```

This command writes the store entries, with their manifests, to a JSON file
for review or backup. It fails, writing nothing, if the output file already
exists or no entry matches. The file is written under a temporary name in the
same directory and linked into place only if the output path is still free,
so an existing file is never replaced, even one created during the export.
There is no import counterpart in v1.

**Example:**
```bash
# Export all obsoleted packages for a publisher
pkg6repo export-obsoleted -s /path/to/repo -p openindiana.org -o /path/to/export.json

# Export only obsoleted packages matching a pattern
pkg6repo export-obsoleted -s /path/to/repo -p openindiana.org -o /path/to/export.json -q "perl"
```

## Importing Obsoleted Packages

`import-pkg5` publishes packages with `pkg.obsolete=true` (and `pkg.renamed=true`)
like any other package: the manifest is stored byte-for-byte in the main
repository and the flag reaches clients through `catalog.dependency.C`, as in
pkg5. Clients need this to remove an obsolete package on update. Earlier
importer versions stored such packages in the obsoleted directory instead;
re-importing the source archive publishes them properly.

## API

The following classes and methods are available for programmatically managing obsoleted packages:

### ObsoletedPackageManager

This class manages obsoleted packages in the repository:

```
pub struct ObsoletedPackageManager {
    base_path: PathBuf,
}

impl ObsoletedPackageManager {
    // Create a new ObsoletedPackageManager
    pub fn new<P: AsRef<Path>>(repo_path: P) -> Self;
    
    // Initialize the obsoleted packages directory structure
    pub fn init(&self) -> Result<()>;
    
    // Store an obsoleted package
    pub fn store_obsoleted_package(
        &self,
        publisher: &str,
        fmri: &Fmri,
        manifest_content: &str,
        obsoleted_by: Option<Vec<String>>,
        deprecation_message: Option<String>,
    ) -> Result<PathBuf>;
    
    // Check if a package is obsoleted
    pub fn is_obsoleted(&self, publisher: &str, fmri: &Fmri) -> bool;
    
    // Get metadata for an obsoleted package
    pub fn get_obsoleted_package_metadata(
        &self,
        publisher: &str,
        fmri: &Fmri,
    ) -> Result<Option<ObsoletedPackageMetadata>>;
    
    // List all obsoleted packages for a publisher
    pub fn list_obsoleted_packages(&self, publisher: &str) -> Result<Vec<Fmri>>;
    
    // Search for obsoleted packages by pattern
    pub fn search_obsoleted_packages(
        &self,
        publisher: &str,
        pattern: &str,
    ) -> Result<Vec<Fmri>>;
    
    // Get and remove an obsoleted package
    pub fn get_and_remove_obsoleted_package(
        &self,
        publisher: &str,
        fmri: &Fmri,
    ) -> Result<String>;
    
    // Export obsoleted packages to a file
    pub fn export_obsoleted_packages(
        &self,
        publisher: &str,
        pattern: Option<&str>,
        output_file: &Path,
    ) -> Result<ObsoletedExport>; // Written(n) | Empty | OutputExists
    
    // Import obsoleted packages from a file
    pub fn import_obsoleted_packages(
        &self,
        input_file: &Path,
        override_publisher: Option<&str>,
    ) -> Result<usize>;
}
```

### ObsoletedPackageMetadata

This struct represents metadata for an obsoleted package:

```
pub struct ObsoletedPackageMetadata {
    pub fmri: String,
    pub status: String,
    pub obsolescence_date: String,
    pub deprecation_message: Option<String>,
    pub obsoleted_by: Option<Vec<String>>,
    pub metadata_version: u32,
    pub content_hash: String,
}
```

## Integration with Repository Operations

- **Package listing and catalog building**: a version present in the store is
  skipped when the repository lists packages from disk with the store loaded.
  Store entries usually have no manifest under `pkg/`, so they are absent
  from the catalog and search.
- **Published `pkg.obsolete=true` manifests** are unaffected by the store.
  They are listed and catalogued, with the flag in `catalog.dependency.C` for
  legacy clients. The client-side shard layout is separate from that protocol.

## Operational Guidance

- **To obsolete a package**, publish a new version whose manifest sets
  `pkg.obsolete=true` (and no other content), as with pkg5. Do not use the
  store for this: legacy clients cannot see store entries.
- **Store entries are not deleted by any v1 command** except
  `restore-obsoleted`, which removes an entry only after publishing it. The
  former `cleanup-obsoleted` TTL purge was removed: it deleted entries
  permanently without dependency or snapshot checks. Purge belongs to
  repository retirement (#39).
- **Rebuild catalogs after bulk operations** (`restore-obsoleted --no-rebuild`)
  to keep them consistent.

## Troubleshooting

Here are solutions to common issues when working with obsoleted packages:

### Package Not Found in Obsoleted Directory

**Issue**: A package that was marked as obsoleted cannot be found in the obsoleted directory.

**Solution**:
1. Check that the FMRI is correct, including the version and timestamp
2. Verify that the publisher name is correct
3. Use the `search-obsoleted` command with a broader pattern to find similar packages
4. Check the repository logs for any errors during the obsolete operation

### Errors During Export

**Issue**: `export-obsoleted` fails.

**Solution**:
1. If the output file already exists, remove it or choose another path
2. If no entries matched, check the publisher and pattern with
   `search-obsoleted`
3. Check that the repository exists and is accessible

### Catalog Issues After Restoring Packages

**Issue**: Catalog issues after restoring obsoleted packages to the main repository.

**Solution**:
1. Rebuild the catalog manually using `pkg6repo rebuild`
2. Check for any errors during the rebuild process
3. Verify that the package was properly restored to the main repository
4. Check for any conflicts with existing packages

### Performance Issues with Large Repositories

**Issue**: Performance issues when working with large repositories with many obsoleted packages.

**Solution**:
1. Use the search command with specific patterns to limit the number of packages processed
2. Use pagination when listing or searching for obsoleted packages
3. Consider using a more powerful machine for repository operations

## Workflow Diagram

```
  earlier pkg6 builds (obsolete-package, pre-#38 import-pkg5)
                          |
                          v
                +-------------------+
                | Obsoleted Storage |  (nothing adds entries in v1)
                +-------------------+
                    |           |
   list-obsoleted   |           |  restore-obsoleted
   show-obsoleted   |           |  (publish, then remove the entry)
   search-obsoleted |           v
   export-obsoleted |   +-----------------------+
   (read-only)      |   | Active repository and |
                    v   | catalog               |
              review /  +-----------------------+
              backup
```
