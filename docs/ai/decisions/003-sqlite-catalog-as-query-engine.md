# ADR-003: SQLite Catalog as Primary Query Engine

**Date:** 2026-02-25
**Status:** Accepted

## Context

The SQLite catalog system (active.db, obsolete.db, fts.db) already stores package metadata, dependencies, and FTS indices. However, many client commands that could use this data are unimplemented, and some operations load everything into memory unnecessarily.

### Current Schema Capabilities (Underutilized)

| Table | Used By | Could Also Serve |
|-------|---------|-----------------|
| `packages` (stem, version, publisher) | Solver init, list | info, search-by-name, update candidates |
| `dependencies` (dep_type, dep_stem, dep_version) | Solver (require only) | reverse-dep queries, optional deps, info --dependencies |
| `incorporate_locks` (stem, release) | Solver | freeze/unfreeze display |
| `package_search` (FTS5: stem, publisher, summary, description) | **Unused** | `pkg search`, GUI search bar |
| `obsolete_packages` | List (partially) | info --obsolete, update path analysis |
| `installed` (fmri, manifest blob) | Install recording | verify, fix, contents, info, uninstall |

### Key Gaps

1. **FTS is built but never queried** — `fts.db` exists, `pkg search` is a stub
2. **No file inventory table** — can't answer "what package owns /usr/bin/vim?" without loading every manifest
3. **No package metadata table** — category, license, homepage not indexed; requires manifest fetch for `pkg info`
4. **Dependencies table only queried for `require` type** — incorporate/optional/conditional ignored

## Decision

Expand the SQLite catalog schema to serve as the primary query engine for all client operations. Avoid loading manifests from repository when the catalog can answer the query.

### Schema Additions

```sql
-- Package metadata (populated during shard build)
CREATE TABLE IF NOT EXISTS package_metadata (
    stem TEXT NOT NULL,
    version TEXT NOT NULL,
    publisher TEXT NOT NULL,
    summary TEXT,
    description TEXT,
    category TEXT,
    license TEXT,
    pkg_size INTEGER,
    PRIMARY KEY (stem, version, publisher)
);

-- File inventory (enables reverse lookups and contents queries)
CREATE TABLE IF NOT EXISTS file_inventory (
    stem TEXT NOT NULL,
    version TEXT NOT NULL,
    publisher TEXT NOT NULL,
    action_type TEXT NOT NULL,
    path TEXT NOT NULL,
    hash TEXT,
    PRIMARY KEY (stem, version, publisher, path)
);
CREATE INDEX IF NOT EXISTS idx_files_path ON file_inventory(path);
CREATE INDEX IF NOT EXISTS idx_files_hash ON file_inventory(hash);
```

### Query Strategy

| Command | Data Source | Avoids |
|---------|-----------|--------|
| `pkg search` | fts.db `package_search MATCH ?` | Loading manifests |
| `pkg info` | package_metadata + dependencies | Loading manifests (for basic info) |
| `pkg contents` | file_inventory | Loading manifests |
| `pkg search -f /usr/bin/vim` | file_inventory WHERE path = ? | Scanning all manifests |
| `pkg verify` | file_inventory + installed.manifest | Only loads installed manifests |
| Solver | packages + dependencies (existing) | No change |

### Installed Actions Table (client-side, installed.db)

The same normalization applies to installed packages. Currently `installed.db` stores a
single JSON blob per package. Cross-package queries (reverse file lookup, bulk verify)
require deserializing every blob — O(n * m).

Add a normalized actions table alongside the existing blob:

```sql
-- Keep existing table for complete manifest reconstruction
-- installed (fmri TEXT PK, manifest BLOB)

-- Normalized action index for fast queries
CREATE TABLE IF NOT EXISTS installed_actions (
    fmri TEXT NOT NULL,
    action_type TEXT NOT NULL,
    path TEXT,
    hash TEXT,
    mode TEXT,
    owner TEXT,
    grp TEXT,
    target TEXT,
    FOREIGN KEY (fmri) REFERENCES installed(fmri) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_ia_path ON installed_actions(path);
CREATE INDEX IF NOT EXISTS idx_ia_hash ON installed_actions(hash);
CREATE INDEX IF NOT EXISTS idx_ia_fmri ON installed_actions(fmri);
```

**Performance comparison:**

| Operation | JSON Blob | Normalized | Speedup |
|-----------|----------|------------|---------|
| "What owns /usr/bin/vim?" | O(n*m) scan + deser | O(log n) index | 1000x+ |
| `pkg verify` (all packages) | Deserialize all blobs | SELECT by fmri | 10-50x |
| `pkg contents pkg-name` | Deserialize 1 blob | SELECT WHERE fmri = ? | ~same |
| `pkg info pkg-name` | Deserialize 1 blob | SELECT WHERE fmri = ? | ~same |
| Install recording | 1 INSERT | N INSERTs (one txn) | ~5ms overhead |
| "All files in /usr/lib/" | Full scan | WHERE path LIKE ? | 100x+ |

Populated during `install_package()` by iterating the manifest's files/dirs/links.
Cleaned up via `ON DELETE CASCADE` during uninstall.

## Consequences

- Shard size increases (file_inventory can be large for repos with many packages)
- Shard build time increases (must parse all manifest actions)
- Client queries become O(1) indexed lookups instead of O(n) manifest scans
- `pkg search`, `pkg info`, `pkg contents` become fast without network access after catalog refresh
- `pkg verify` can check file presence/hash/mode via indexed queries instead of blob deserialization
- "What package owns this file?" becomes instant for both installed and catalog packages
- Backward compatible — old shards without new tables still work, just fall back to manifest fetch
