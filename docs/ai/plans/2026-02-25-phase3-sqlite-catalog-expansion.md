# Phase 3: SQLite Catalog Expansion (ADR-003 Implementation)

**Date:** 2026-02-25
**Status:** Active
**Depends on:** Phase 1 (architecture), Phase 2 P2 (query commands)
**Implements:** ADR-003

## Goal

Expand the SQLite catalog shards so that `pkg info`, `pkg contents`, `pkg search -f`, and reverse file lookups work without fetching manifests from the repository. This makes offline queries fast and enables the GUI search bar.

## Step 1: Expand shard build schema

**File:** `libips/src/repository/sqlite_catalog.rs`

### 1.1: Add `package_metadata` table

During `build_shards()`, when parsing catalog parts, extract `set` actions for:
- `pkg.summary` (already extracted for FTS)
- `pkg.description` (already extracted for FTS)
- `info.classification` -> category
- `pkg.human-version` -> human_version
- `info.upstream-url` -> homepage

```sql
CREATE TABLE IF NOT EXISTS package_metadata (
    stem TEXT NOT NULL,
    version TEXT NOT NULL,
    publisher TEXT NOT NULL,
    summary TEXT,
    description TEXT,
    category TEXT,
    human_version TEXT,
    homepage TEXT,
    PRIMARY KEY (stem, version, publisher)
);
```

### 1.2: Add `file_inventory` table

Parse all `file`, `dir`, `link`, `hardlink` actions from manifests during shard build:

```sql
CREATE TABLE IF NOT EXISTS file_inventory (
    stem TEXT NOT NULL,
    version TEXT NOT NULL,
    publisher TEXT NOT NULL,
    action_type TEXT NOT NULL,  -- file, dir, link, hardlink
    path TEXT NOT NULL,
    hash TEXT,                   -- SHA hash for files, NULL for dirs/links
    target TEXT,                 -- link target for links, NULL otherwise
    mode TEXT,
    owner TEXT,
    grp TEXT,
    PRIMARY KEY (stem, version, publisher, path)
);
CREATE INDEX IF NOT EXISTS idx_fi_path ON file_inventory(path);
CREATE INDEX IF NOT EXISTS idx_fi_hash ON file_inventory(hash);
```

### 1.3: Index all dependency types

Currently only `require` is stored. Expand to store all types:

```sql
-- Add column or track in existing dependencies table
-- dep_type already exists, just populate for all types:
-- require, incorporate, optional, conditional, group, require-any, exclude, parent, origin
```

## Step 2: Update shard sync

**File:** `libips/src/repository/shard_sync.rs`

The `catalog.attrs` index already tracks shard hashes. New tables go into `active.db`, so no new shard files are needed. The SHA256 hash of `active.db` changes when content changes, triggering re-download.

Backward compatibility: if the client sees a server without the new tables, queries fall back to manifest fetch. Add version field to `catalog.attrs`:

```json
{
  "version": 3,
  "schema_features": ["package_metadata", "file_inventory"],
  ...
}
```

## Step 3: Update query methods

### 3.1: `pkg info` fast path

```rust
// In libips::api::info
pub fn get_package_info(image: &Image, fmri: &Fmri) -> Result<PackageDetails> {
    // Try catalog metadata first (fast, no manifest parse)
    if let Some(meta) = query_package_metadata(image, fmri)? {
        return Ok(meta);
    }
    // Fall back to manifest fetch (slow, needs network)
    let manifest = image.get_manifest(fmri)?;
    Ok(PackageDetails::from_manifest(&manifest))
}
```

### 3.2: `pkg contents` fast path

```rust
pub fn get_package_contents(image: &Image, fmri: &Fmri) -> Result<Vec<FileEntry>> {
    // Try file_inventory first
    if let Ok(entries) = query_file_inventory(image, fmri)? {
        return Ok(entries);
    }
    // Fall back to manifest parse
    ...
}
```

### 3.3: Reverse file lookup

```rust
/// What package owns this file path?
pub fn search_by_path(image: &Image, path: &str) -> Result<Vec<(Fmri, String)>> {
    // SELECT stem, version, publisher, action_type FROM file_inventory WHERE path = ?1
}
```

### 3.4: FTS expansion

Add file paths to FTS index so `pkg search /usr/bin/vim` works:

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS package_search
    USING fts5(stem, publisher, summary, description, file_paths,
               content='', tokenize='unicode61');
```

Where `file_paths` is a space-separated list of all paths in the package.

## Step 4: Depot server serves new shard format

**File:** `pkg6depotd/src/http/handlers/shard.rs`

No changes needed — shards are served as opaque blobs by SHA256 hash. The depot already serves whatever `catalog.attrs` points to.

## Verification

- Rebuild a test repository: `cargo run -p pkg6repo -- rebuild --publisher test`
- Verify new tables exist: `sqlite3 catalog2/active.db ".tables"`
- Verify `pkg info` returns data without manifest fetch
- Verify `pkg contents` returns file list without manifest fetch
- Verify `pkg search -f /usr/bin/something` returns owning package
- Verify backward compat: old client against new server, new client against old server
